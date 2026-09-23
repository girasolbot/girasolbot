use borsh::BorshDeserialize;
use log::{info, debug};
use serde_json::{json, Value};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;
use std::sync::Arc;

use crate::settings::Settings;

/// Raydium CPMM program ID
const RAYDIUM_CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Raydium CPMM authority (hardcoded)
const CPMM_AUTHORITY: &str = "GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL";

/// SPL Token program
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// WSOL mint
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// SwapBaseInput discriminator: sha256("global:swap_base_input")[0..8]
const SWAP_BASE_IN_DISCRIMINATOR: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];

/// Pool state size (without 8-byte Anchor discriminator prefix)
const POOL_STATE_SIZE: usize = 629;

/// Raydium CPMM pool state — deserialized from on-chain account data (after 8-byte discriminator)
#[derive(Clone, Debug, Default, BorshDeserialize)]
pub struct CpmmPoolState {
    pub amm_config: Pubkey,
    pub pool_creator: Pubkey,
    pub token0_vault: Pubkey,
    pub token1_vault: Pubkey,
    pub lp_mint: Pubkey,
    pub token0_mint: Pubkey,
    pub token1_mint: Pubkey,
    pub token0_program: Pubkey,
    pub token1_program: Pubkey,
    pub observation_key: Pubkey,
    pub auth_bump: u8,
    pub status: u8,
    pub lp_mint_decimals: u8,
    pub mint0_decimals: u8,
    pub mint1_decimals: u8,
    pub lp_supply: u64,
    pub protocol_fees_token0: u64,
    pub protocol_fees_token1: u64,
    pub fund_fees_token0: u64,
    pub fund_fees_token1: u64,
    pub open_time: u64,
    pub recent_epoch: u64,
    pub padding: [u64; 31],
}

use base64::Engine;

/// Fetch and decode CPMM pool state from on-chain account data
pub async fn fetch_pool_state(
    pool_address: &str,
    _rpc_client: &Arc<solana_client::rpc_client::RpcClient>,
    settings: &Settings,
) -> Result<CpmmPoolState, Box<dyn std::error::Error + Send + Sync>> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pool_address, {"encoding": "base64", "commitment": "processed"}]
    });

    let resp = crate::rpc::SHARED_HTTP_CLIENT
        .post(&settings.solana_rpc_urls[0])
        .json(&body)
        .send()
        .await?;

    let json: Value = resp.json().await?;
    let data_b64 = json
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("data"))
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.as_str())
        .ok_or("Failed to get CPMM pool account data")?;

    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("Base64 decode error: {}", e))?;

    // Skip first 8 bytes (Anchor discriminator)
    if data.len() < 8 + POOL_STATE_SIZE {
        return Err(format!("CPMM pool data too small: {} < {}", data.len(), 8 + POOL_STATE_SIZE).into());
    }

    let pool: CpmmPoolState = borsh::from_slice(&data[8..8 + POOL_STATE_SIZE])
        .map_err(|e| format!("CPMM pool deserialization error: {}", e))?;

    debug!("CPMM pool {}: token0_mint={}, token1_mint={}, token0_vault={}, token1_vault={}",
        pool_address, pool.token0_mint, pool.token1_mint, pool.token0_vault, pool.token1_vault);

    Ok(pool)
}

/// Fetch token balance for a vault account
async fn fetch_token_balance(
    token_account: &Pubkey,
    settings: &Settings,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getTokenAccountBalance",
        "params": [token_account.to_string(), {"commitment": "processed"}]
    });

    let resp = crate::rpc::SHARED_HTTP_CLIENT
        .post(&settings.solana_rpc_urls[0])
        .json(&body)
        .send()
        .await?;

    let json: Value = resp.json().await?;
    let amount_str = json
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("amount"))
        .and_then(|a| a.as_str())
        .ok_or("Failed to get token balance")?;

    Ok(amount_str.parse::<u64>()?)
}

/// Constant-product swap computation (same as Raydium V4 but CPMM has different fee tiers).
/// For simplicity, uses the same 0.25% fee as V4 (actual fee may vary per amm_config).
fn compute_swap_amount(
    reserve_0: u64,
    reserve_1: u64,
    is_base_in: bool,
    amount_in: u64,
    slippage_bps: u64,
) -> (u64, u64) {
    let fee = amount_in * 25 / 10000; // 0.25%
    let amount_in_after_fee = amount_in.saturating_sub(fee);

    let (r_in, r_out) = if is_base_in {
        (reserve_0 as u128, reserve_1 as u128)
    } else {
        (reserve_1 as u128, reserve_0 as u128)
    };

    let amount_out = if r_in + amount_in_after_fee as u128 > 0 {
        (r_out * amount_in_after_fee as u128 / (r_in + amount_in_after_fee as u128)) as u64
    } else {
        0
    };

    let min_amount_out = amount_out * (10000 - slippage_bps) / 10000;
    (amount_out, min_amount_out)
}

/// Build a CPMM SwapBaseInput instruction.
/// 13 accounts in exact order expected by the program.
fn build_swap_instruction(
    pool_address: &Pubkey,
    pool: &CpmmPoolState,
    payer: &Pubkey,
    user_input_ata: &Pubkey,
    user_output_ata: &Pubkey,
    input_vault: &Pubkey,
    output_vault: &Pubkey,
    input_token_program: &Pubkey,
    output_token_program: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    min_amount_out: u64,
) -> Result<Instruction, Box<dyn std::error::Error + Send + Sync>> {
    let program_id = Pubkey::from_str(RAYDIUM_CPMM_PROGRAM)?;
    let authority = Pubkey::from_str(CPMM_AUTHORITY)?;

    let accounts = vec![
        AccountMeta::new(*payer, true),                          // 0: Payer (signer)
        AccountMeta::new_readonly(authority, false),              // 1: Authority
        AccountMeta::new(pool.amm_config, false),                // 2: AMM Config
        AccountMeta::new(*pool_address, false),                  // 3: Pool State
        AccountMeta::new(*user_input_ata, false),                // 4: User Input Token Account
        AccountMeta::new(*user_output_ata, false),               // 5: User Output Token Account
        AccountMeta::new(*input_vault, false),                   // 6: Input Vault
        AccountMeta::new(*output_vault, false),                  // 7: Output Vault
        AccountMeta::new_readonly(*input_token_program, false),  // 8: Input Token Program
        AccountMeta::new_readonly(*output_token_program, false), // 9: Output Token Program
        AccountMeta::new_readonly(*input_mint, false),           // 10: Input Mint
        AccountMeta::new_readonly(*output_mint, false),          // 11: Output Mint
        AccountMeta::new(pool.observation_key, false),           // 12: Observation State
    ];

    let mut data = [0u8; 24];
    data[..8].copy_from_slice(&SWAP_BASE_IN_DISCRIMINATOR);
    data[8..16].copy_from_slice(&amount_in.to_le_bytes());
    data[16..24].copy_from_slice(&min_amount_out.to_le_bytes());

    Ok(Instruction {
        program_id,
        accounts,
        data: data.to_vec(),
    })
}

/// High-level buy: SOL → token via Raydium CPMM.
pub async fn build_buy_instructions(
    pool_address: &str,
    mint: &str,
    sol_amount: f64,
    slippage_bps: u64,
    payer: &Pubkey,
    rpc_client: &Arc<solana_client::rpc_client::RpcClient>,
    settings: &Settings,
) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
    let pool_pk = Pubkey::from_str(pool_address)?;
    let mint_pk = Pubkey::from_str(mint)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;

    let pool = fetch_pool_state(pool_address, rpc_client, settings).await?;

    let reserve_0 = fetch_token_balance(&pool.token0_vault, settings).await?;
    let reserve_1 = fetch_token_balance(&pool.token1_vault, settings).await?;

    info!("CPMM pool {}: reserve_0={}, reserve_1={}, token0={}, token1={}",
        pool_address, reserve_0, reserve_1, pool.token0_mint, pool.token1_mint);

    // Determine direction: SOL (WSOL) is input
    let is_base_in = pool.token0_mint == wsol_mint;
    let amount_in = (sol_amount * 1_000_000_000.0) as u64;

    let (amount_out, min_amount_out) = compute_swap_amount(
        reserve_0, reserve_1, is_base_in, amount_in, slippage_bps,
    );

    info!("CPMM buy swap: {} SOL → {} tokens (min: {})", sol_amount, amount_out, min_amount_out);

    let user_wsol_ata = spl_associated_token_account::get_associated_token_address(payer, &wsol_mint);
    let user_token_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        payer, &mint_pk,
        if is_base_in { &pool.token1_program } else { &pool.token0_program },
    );

    let (input_vault, output_vault) = if is_base_in {
        (pool.token0_vault, pool.token1_vault)
    } else {
        (pool.token1_vault, pool.token0_vault)
    };

    let (input_token_program, output_token_program) = if is_base_in {
        (token_program, pool.token1_program)
    } else {
        (pool.token0_program, token_program) // WSOL always uses SPL Token
    };

    let mut instructions = Vec::new();

    // Create WSOL ATA + wrap SOL
    instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        payer, payer, &wsol_mint, &token_program,
    ));
    instructions.push(solana_sdk::system_instruction::transfer(payer, &user_wsol_ata, amount_in));
    instructions.push(spl_token::instruction::sync_native(&token_program, &user_wsol_ata)?);

    // Create output token ATA
    instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        payer, payer, &mint_pk, &output_token_program,
    ));

    // Swap
    instructions.push(build_swap_instruction(
        &pool_pk, &pool, payer,
        &user_wsol_ata, &user_token_ata,
        &input_vault, &output_vault,
        &input_token_program, &output_token_program,
        &wsol_mint, &mint_pk,
        amount_in, min_amount_out,
    )?);

    // Close WSOL ATA
    instructions.push(spl_token::instruction::close_account(&token_program, &user_wsol_ata, payer, payer, &[])?);

    Ok(instructions)
}

/// High-level sell: token → SOL via Raydium CPMM.
pub async fn build_sell_instructions(
    pool_address: &str,
    mint: &str,
    token_amount: u64,
    slippage_bps: u64,
    payer: &Pubkey,
    rpc_client: &Arc<solana_client::rpc_client::RpcClient>,
    settings: &Settings,
) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
    let pool_pk = Pubkey::from_str(pool_address)?;
    let mint_pk = Pubkey::from_str(mint)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;

    let pool = fetch_pool_state(pool_address, rpc_client, settings).await?;

    let reserve_0 = fetch_token_balance(&pool.token0_vault, settings).await?;
    let reserve_1 = fetch_token_balance(&pool.token1_vault, settings).await?;

    // Token is input, SOL (WSOL) is output
    let is_base_in = pool.token0_mint == mint_pk;

    let (amount_out, min_amount_out) = compute_swap_amount(
        reserve_0, reserve_1, is_base_in, token_amount, slippage_bps,
    );

    info!("CPMM sell swap: {} tokens → ~{} lamports SOL (min: {})", token_amount, amount_out, min_amount_out);

    let mint_token_program = if is_base_in { pool.token0_program } else { pool.token1_program };
    let user_token_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        payer, &mint_pk, &mint_token_program,
    );
    let user_wsol_ata = spl_associated_token_account::get_associated_token_address(payer, &wsol_mint);

    let (input_vault, output_vault) = if is_base_in {
        (pool.token0_vault, pool.token1_vault)
    } else {
        (pool.token1_vault, pool.token0_vault)
    };

    let (input_token_program, output_token_program) = if is_base_in {
        (pool.token0_program, token_program)
    } else {
        (pool.token1_program, token_program)
    };

    let mut instructions = Vec::new();

    // Create WSOL ATA (to receive SOL proceeds)
    instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        payer, payer, &wsol_mint, &token_program,
    ));

    // Swap
    instructions.push(build_swap_instruction(
        &pool_pk, &pool, payer,
        &user_token_ata, &user_wsol_ata,
        &input_vault, &output_vault,
        &input_token_program, &output_token_program,
        &mint_pk, &wsol_mint,
        token_amount, min_amount_out,
    )?);

    // Close WSOL ATA to unwrap SOL
    instructions.push(spl_token::instruction::close_account(&token_program, &user_wsol_ata, payer, payer, &[])?);

    Ok(instructions)
}

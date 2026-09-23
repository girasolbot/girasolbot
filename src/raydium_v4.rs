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

/// Raydium V4 AMM program ID
pub const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// Raydium V4 authority (hardcoded, not a PDA)
const RAYDIUM_AUTHORITY: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";

/// SPL Token program
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// WSOL mint
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// SwapBaseIn instruction discriminator
const SWAP_BASE_IN: u8 = 0x09;

/// Trading fee: 25/10000 = 0.25%
const TRADE_FEE_NUMERATOR: u64 = 25;
const TRADE_FEE_DENOMINATOR: u64 = 10000;

/// AMM state account size (bytes)
const AMM_INFO_SIZE: usize = 752;

/// Raydium V4 AMM state — deserialized from on-chain account data
#[derive(Clone, Debug, Default, BorshDeserialize)]
pub struct AmmInfo {
    pub status: u64,
    pub nonce: u64,
    pub order_num: u64,
    pub depth: u64,
    pub coin_decimals: u64,
    pub pc_decimals: u64,
    pub state: u64,
    pub reset_flag: u64,
    pub min_size: u64,
    pub vol_max_cut_ratio: u64,
    pub amount_wave: u64,
    pub coin_lot_size: u64,
    pub pc_lot_size: u64,
    pub min_price_multiplier: u64,
    pub max_price_multiplier: u64,
    pub sys_decimal_value: u64,
    pub fees: Fees,
    pub out_put: OutPutData,
    pub token_coin: Pubkey,
    pub token_pc: Pubkey,
    pub coin_mint: Pubkey,
    pub pc_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub open_orders: Pubkey,
    pub market: Pubkey,
    pub serum_dex: Pubkey,
    pub target_orders: Pubkey,
    pub withdraw_queue: Pubkey,
    pub token_temp_lp: Pubkey,
    pub amm_owner: Pubkey,
    pub lp_amount: u64,
    pub client_order_id: u64,
    pub padding: [u64; 2],
}

#[derive(Clone, Debug, Default, BorshDeserialize)]
pub struct Fees {
    pub min_separate_numerator: u64,
    pub min_separate_denominator: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub pnl_numerator: u64,
    pub pnl_denominator: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
}

#[derive(Clone, Debug, Default, BorshDeserialize)]
pub struct OutPutData {
    pub need_take_pnl_coin: u64,
    pub need_take_pnl_pc: u64,
    pub total_pnl_pc: u64,
    pub total_pnl_coin: u64,
    pub pool_open_time: u64,
    pub punish_pc_amount: u64,
    pub punish_coin_amount: u64,
    pub orderbook_to_init_time: u64,
    pub swap_coin_in_amount: u128,
    pub swap_pc_out_amount: u128,
    pub swap_take_pc_fee: u64,
    pub swap_pc_in_amount: u128,
    pub swap_coin_out_amount: u128,
    pub swap_take_coin_fee: u64,
}

/// Fetch and decode AMM state from on-chain account data
pub async fn fetch_amm_info(
    amm_address: &str,
    _rpc_client: &Arc<solana_client::rpc_client::RpcClient>,
    settings: &Settings,
) -> Result<AmmInfo, Box<dyn std::error::Error + Send + Sync>> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [amm_address, {"encoding": "base64", "commitment": "processed"}]
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
        .ok_or("Failed to get AMM account data")?;

    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("Base64 decode error: {}", e))?;

    if data.len() < AMM_INFO_SIZE {
        return Err(format!("AMM account data too small: {} < {}", data.len(), AMM_INFO_SIZE).into());
    }

    let amm_info: AmmInfo = borsh::from_slice(&data[..AMM_INFO_SIZE])
        .map_err(|e| format!("AMM deserialization error: {}", e))?;

    debug!("AMM info: coin_mint={}, pc_mint={}, coin_decimals={}, pc_decimals={}",
        amm_info.coin_mint, amm_info.pc_mint, amm_info.coin_decimals, amm_info.pc_decimals);

    Ok(amm_info)
}

use base64::Engine;

/// Fetch token balance for a token account
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

/// Compute swap output for Raydium V4 constant-product AMM.
/// Returns (amount_out, min_amount_out) after fee and slippage.
pub fn compute_swap_amount(
    coin_reserve: u64,
    pc_reserve: u64,
    is_base_in: bool,
    amount_in: u64,
    slippage_bps: u64,
) -> (u64, u64) {
    // Apply trading fee: amount_in * (1 - fee)
    let fee = amount_in * TRADE_FEE_NUMERATOR / TRADE_FEE_DENOMINATOR;
    let amount_in_after_fee = amount_in.saturating_sub(fee);

    // Constant-product: out = (reserve_out * amount_in_after_fee) / (reserve_in + amount_in_after_fee)
    let (reserve_in, reserve_out) = if is_base_in {
        (coin_reserve as u128, pc_reserve as u128)
    } else {
        (pc_reserve as u128, coin_reserve as u128)
    };

    let amount_out = if reserve_in + amount_in_after_fee as u128 > 0 {
        (reserve_out * amount_in_after_fee as u128 / (reserve_in + amount_in_after_fee as u128)) as u64
    } else {
        0
    };

    // Apply slippage
    let min_amount_out = amount_out * (10000 - slippage_bps) / 10000;

    (amount_out, min_amount_out)
}

/// Build a Raydium V4 SwapBaseIn instruction (buy: SOL → token).
///
/// The SDK pattern: pass the AMM address for all Serum-related accounts (indices 3, 6-13).
/// The AMM program resolves real addresses from its own state internally.
pub fn build_swap_base_in_instruction(
    amm: &Pubkey,
    amm_info: &AmmInfo,
    user_source_ata: &Pubkey,  // WSOL ATA (input)
    user_dest_ata: &Pubkey,    // token ATA (output)
    user: &Pubkey,             // signer
    amount_in: u64,
    min_amount_out: u64,
) -> Result<Instruction, Box<dyn std::error::Error + Send + Sync>> {
    let program_id = Pubkey::from_str(RAYDIUM_AMM_V4_PROGRAM)?;
    let authority = Pubkey::from_str(RAYDIUM_AUTHORITY)?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;

    // 17 accounts — AMM address used as placeholder for Serum accounts (SDK pattern)
    let accounts = vec![
        AccountMeta::new_readonly(token_program, false),    // 0: Token Program
        AccountMeta::new(*amm, false),                       // 1: AMM
        AccountMeta::new_readonly(authority, false),          // 2: Authority
        AccountMeta::new(*amm, false),                       // 3: Open Orders (AMM resolves)
        AccountMeta::new(amm_info.token_coin, false),        // 4: Pool Coin Token Account
        AccountMeta::new(amm_info.token_pc, false),          // 5: Pool PC Token Account
        AccountMeta::new(*amm, false),                       // 6: Serum Program (AMM resolves)
        AccountMeta::new(*amm, false),                       // 7: Serum Market (AMM resolves)
        AccountMeta::new(*amm, false),                       // 8: Serum Bids (AMM resolves)
        AccountMeta::new(*amm, false),                       // 9: Serum Asks (AMM resolves)
        AccountMeta::new(*amm, false),                       // 10: Serum Event Queue (AMM resolves)
        AccountMeta::new(*amm, false),                       // 11: Serum Coin Vault (AMM resolves)
        AccountMeta::new(*amm, false),                       // 12: Serum PC Vault (AMM resolves)
        AccountMeta::new(*amm, false),                       // 13: Serum Vault Signer (AMM resolves)
        AccountMeta::new(*user_source_ata, false),           // 14: User Source (WSOL ATA)
        AccountMeta::new(*user_dest_ata, false),             // 15: User Destination (token ATA)
        AccountMeta::new(*user, true),                       // 16: User Owner (signer)
    ];

    // Instruction data: 1 byte discriminator + 8 bytes amount_in + 8 bytes min_amount_out
    let mut data = [0u8; 17];
    data[0] = SWAP_BASE_IN;
    data[1..9].copy_from_slice(&amount_in.to_le_bytes());
    data[9..17].copy_from_slice(&min_amount_out.to_le_bytes());

    Ok(Instruction {
        program_id,
        accounts,
        data: data.to_vec(),
    })
}

/// High-level buy function: SOL → token via Raydium V4.
/// Fetches AMM state, computes swap, builds instruction.
/// Returns the instructions to execute (ATA creation + swap).
pub async fn build_buy_instructions(
    amm_address: &str,
    mint: &str,
    sol_amount: f64,
    slippage_bps: u64,
    payer: &Pubkey,
    rpc_client: &Arc<solana_client::rpc_client::RpcClient>,
    settings: &Settings,
) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
    let amm = Pubkey::from_str(amm_address)?;
    let mint_pk = Pubkey::from_str(mint)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;

    // Fetch AMM state
    let amm_info = fetch_amm_info(amm_address, rpc_client, settings).await?;

    // Fetch current reserves
    let coin_reserve = fetch_token_balance(&amm_info.token_coin, settings).await?;
    let pc_reserve = fetch_token_balance(&amm_info.token_pc, settings).await?;

    info!("Raydium V4 pool {}: coin_reserve={}, pc_reserve={}, coin_mint={}, pc_mint={}",
        amm_address, coin_reserve, pc_reserve, amm_info.coin_mint, amm_info.pc_mint);

    // Determine swap direction: is SOL the "coin" or "pc" side?
    let is_base_in = amm_info.coin_mint == wsol_mint;
    let amount_in = (sol_amount * 1_000_000_000.0) as u64;

    let (amount_out, min_amount_out) = compute_swap_amount(
        coin_reserve, pc_reserve, is_base_in, amount_in, slippage_bps,
    );

    info!("Raydium V4 swap: {} SOL → {} tokens (min: {}, slippage: {} bps)",
        sol_amount, amount_out, min_amount_out, slippage_bps);

    // Build ATAs
    let user_wsol_ata = spl_associated_token_account::get_associated_token_address(payer, &wsol_mint);
    let user_token_ata = spl_associated_token_account::get_associated_token_address(payer, &mint_pk);

    let (user_source, user_dest) = if is_base_in {
        (user_wsol_ata, user_token_ata)
    } else {
        (user_token_ata, user_wsol_ata)
    };

    let mut instructions = Vec::new();

    // Create WSOL ATA + wrap SOL
    instructions.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &wsol_mint, &token_program,
        ),
    );
    // Transfer SOL to WSOL ATA (wrapping)
    instructions.push(
        solana_sdk::system_instruction::transfer(payer, &user_wsol_ata, amount_in),
    );
    // Sync native (finalize WSOL wrap)
    instructions.push(
        spl_token::instruction::sync_native(&token_program, &user_wsol_ata)?,
    );

    // Create token ATA (idempotent)
    instructions.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &mint_pk, &token_program,
        ),
    );

    // Swap instruction
    instructions.push(build_swap_base_in_instruction(
        &amm, &amm_info, &user_source, &user_dest, payer, amount_in, min_amount_out,
    )?);

    // Close WSOL ATA to reclaim rent
    instructions.push(
        spl_token::instruction::close_account(&token_program, &user_wsol_ata, payer, payer, &[])?
    );

    Ok(instructions)
}

/// High-level sell function: token → SOL via Raydium V4.
/// Fetches AMM state, computes swap, builds instruction.
/// Returns the instructions to execute (WSOL ATA create + swap + WSOL close).
pub async fn build_sell_instructions(
    amm_address: &str,
    mint: &str,
    token_amount: u64,
    slippage_bps: u64,
    payer: &Pubkey,
    _rpc_client: &Arc<solana_client::rpc_client::RpcClient>,
    settings: &Settings,
) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
    let amm = Pubkey::from_str(amm_address)?;
    let mint_pk = Pubkey::from_str(mint)?;
    let wsol_mint = Pubkey::from_str(WSOL_MINT)?;
    let token_program = Pubkey::from_str(TOKEN_PROGRAM)?;

    // Fetch AMM state
    let amm_info = fetch_amm_info(amm_address, _rpc_client, settings).await?;

    // Fetch current reserves
    let coin_reserve = fetch_token_balance(&amm_info.token_coin, settings).await?;
    let pc_reserve = fetch_token_balance(&amm_info.token_pc, settings).await?;

    info!("Raydium V4 sell pool {}: coin_reserve={}, pc_reserve={}", amm_address, coin_reserve, pc_reserve);

    // Determine direction: token is input, SOL (WSOL) is output
    // If coin_mint == WSOL → token is on the PC side, so is_base_in = false (PC → coin/WSOL)
    // If pc_mint == WSOL → token is on the coin side, so is_base_in = true (coin → pc/WSOL)
    let is_base_in = amm_info.coin_mint == mint_pk;

    let (amount_out, min_amount_out) = compute_swap_amount(
        coin_reserve, pc_reserve, is_base_in, token_amount, slippage_bps,
    );

    info!("Raydium V4 sell swap: {} tokens → ~{} lamports SOL (min: {}, slippage: {} bps)",
        token_amount, amount_out, min_amount_out, slippage_bps);

    // Build ATAs
    let user_wsol_ata = spl_associated_token_account::get_associated_token_address(payer, &wsol_mint);
    let user_token_ata = spl_associated_token_account::get_associated_token_address(payer, &mint_pk);

    // For sell: source = token ATA, dest = WSOL ATA
    let (user_source, user_dest) = if is_base_in {
        // token is coin side → source=coin ATA, dest=pc ATA (WSOL)
        (user_token_ata, user_wsol_ata)
    } else {
        // token is pc side → source=pc ATA (token), dest=coin ATA (WSOL)
        (user_token_ata, user_wsol_ata)
    };

    let mut instructions = Vec::new();

    // Create WSOL ATA (to receive SOL proceeds)
    instructions.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &wsol_mint, &token_program,
        ),
    );

    // Ensure token ATA exists (should exist from the buy, but be defensive)
    instructions.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &mint_pk, &token_program,
        ),
    );

    // Swap instruction (token → WSOL)
    instructions.push(build_swap_base_in_instruction(
        &amm, &amm_info, &user_source, &user_dest, payer, token_amount, min_amount_out,
    )?);

    // Close WSOL ATA to unwrap SOL proceeds back to wallet
    instructions.push(
        spl_token::instruction::close_account(&token_program, &user_wsol_ata, payer, payer, &[])?
    );

    Ok(instructions)
}

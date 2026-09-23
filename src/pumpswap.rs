use base64::Engine;
use log::{info, debug};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use std::str::FromStr;

/// PumpSwap AMM program ID
pub const PUMPSWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// PumpSwap GlobalConfig PDA (seeds: ["global_config"])
pub const PUMPSWAP_GLOBAL_CONFIG: &str = "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw";

/// Associated Token Program
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// PumpSwap buy discriminator (same as pump.fun bonding curve — both Anchor "buy")
pub const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

/// PumpSwap sell discriminator (same as pump.fun bonding curve — both Anchor "sell")
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Data extracted from a target's PumpSwap transaction for copytrade mirroring.
#[derive(Debug, Clone)]
pub struct PumpSwapAccounts {
    /// All account keys from the target's PumpSwap instruction (17+ accounts)
    pub accounts: Vec<String>,
    /// Pool address (account[0])
    pub pool: String,
    /// Base mint address (account[3]) — the meme token
    pub base_mint: String,
    /// Quote mint address (account[4]) — usually WSOL
    pub quote_mint: String,
    /// Token amount from instruction args
    pub token_amount: u64,
    /// SOL amount from instruction args
    pub sol_amount: u64,
}

/// Build PumpSwap buy instructions for copytrade.
/// Mirrors target's accounts, replacing only user-specific ones with our wallet.
///
/// Target PumpSwap buy account layout (17 accounts):
///   [0]  pool
///   [1]  user (signer) → REPLACE with our wallet
///   [2]  global_config
///   [3]  base_mint
///   [4]  quote_mint (WSOL)
///   [5]  user_base_token_account → REPLACE with our base ATA
///   [6]  user_quote_token_account → REPLACE with our WSOL ATA
///   [7]  pool_base_token_account
///   [8]  pool_quote_token_account
///   [9]  protocol_fee_recipient
///   [10] protocol_fee_recipient_token_account
///   [11] base_token_program
///   [12] quote_token_program
///   [13] system_program
///   [14] associated_token_program
///   [15] event_authority
///   [16] program (self)
pub fn build_buy_instructions(
    target_accounts: &PumpSwapAccounts,
    our_sol_lamports: u64,
    our_token_amount: u64,
    payer: &Pubkey,
    slippage_bps: u64,
) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
    if target_accounts.accounts.len() < 17 {
        return Err(format!(
            "PumpSwap buy needs 17 accounts, got {}",
            target_accounts.accounts.len()
        ).into());
    }

    let base_mint = Pubkey::from_str(&target_accounts.base_mint)?;
    let quote_mint = Pubkey::from_str(&target_accounts.quote_mint)?;

    // Determine token programs from target TX
    let base_token_program = Pubkey::from_str(&target_accounts.accounts[11])?;
    let quote_token_program = Pubkey::from_str(&target_accounts.accounts[12])?;

    // Our ATAs
    let our_base_ata = get_associated_token_address_with_program_id(payer, &base_mint, &base_token_program);
    let our_quote_ata = get_associated_token_address_with_program_id(payer, &quote_mint, &quote_token_program);

    // Slippage: max_quote_amount_in = sol_amount * (1 + slippage_bps/10000)
    let max_quote_in = our_sol_lamports + (our_sol_lamports * slippage_bps / 10000);

    let program_id = Pubkey::from_str(PUMPSWAP_PROGRAM)?;

    // Build account metas — mirror target but replace user accounts
    let mut accounts = vec![
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[0])?, false),  // pool
        AccountMeta::new(*payer, true),                                                       // user (signer)
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[2])?, false),  // global_config
        AccountMeta::new_readonly(base_mint, false),                                          // base_mint
        AccountMeta::new_readonly(quote_mint, false),                                         // quote_mint
        AccountMeta::new(our_base_ata, false),                                                // user_base_token_account
        AccountMeta::new(our_quote_ata, false),                                               // user_quote_token_account
        AccountMeta::new(Pubkey::from_str(&target_accounts.accounts[7])?, false),             // pool_base_token_account
        AccountMeta::new(Pubkey::from_str(&target_accounts.accounts[8])?, false),             // pool_quote_token_account
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[9])?, false),    // protocol_fee_recipient
        AccountMeta::new(Pubkey::from_str(&target_accounts.accounts[10])?, false),            // protocol_fee_recipient_token_account
        AccountMeta::new_readonly(base_token_program, false),                                 // base_token_program
        AccountMeta::new_readonly(quote_token_program, false),                                // quote_token_program
        AccountMeta::new_readonly(solana_program::system_program::id(), false),                // system_program
        AccountMeta::new_readonly(Pubkey::from_str(ATA_PROGRAM)?, false),                     // associated_token_program
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[15])?, false),   // event_authority
        AccountMeta::new_readonly(program_id, false),                                          // program (self)
    ];

    // Any remaining accounts from target (e.g., creator fee sharing)
    for i in 17..target_accounts.accounts.len() {
        if let Ok(pk) = Pubkey::from_str(&target_accounts.accounts[i]) {
            accounts.push(AccountMeta::new_readonly(pk, false));
        }
    }

    // Instruction data: discriminator + base_amount_out (u64) + max_quote_amount_in (u64)
    let mut data = BUY_DISCRIMINATOR.to_vec();
    data.extend(our_token_amount.to_le_bytes());
    data.extend(max_quote_in.to_le_bytes());

    let buy_ix = Instruction {
        program_id,
        accounts,
        data,
    };

    let buy_acct_count = buy_ix.accounts.len();

    // Pre-instructions: create base token ATA + WSOL ATA (idempotent)
    let mut instructions = vec![
        create_associated_token_account_idempotent(payer, payer, &base_mint, &base_token_program),
        create_associated_token_account_idempotent(payer, payer, &quote_mint, &quote_token_program),
    ];
    instructions.push(buy_ix);

    info!(
        "PumpSwap buy: pool={}.. mint={}.. tokens={} max_sol={:.4} SOL ({} accounts)",
        &target_accounts.pool[..target_accounts.pool.len().min(8)],
        &target_accounts.base_mint[..target_accounts.base_mint.len().min(8)],
        our_token_amount,
        max_quote_in as f64 / 1e9,
        buy_acct_count,
    );

    Ok(instructions)
}

/// Build PumpSwap sell instructions for copytrade.
/// Same account layout as buy, but with sell discriminator and different args.
pub fn build_sell_instructions(
    target_accounts: &PumpSwapAccounts,
    token_amount: u64,
    min_sol_out: u64,
    payer: &Pubkey,
) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
    if target_accounts.accounts.len() < 17 {
        return Err(format!(
            "PumpSwap sell needs 17 accounts, got {}",
            target_accounts.accounts.len()
        ).into());
    }

    let base_mint = Pubkey::from_str(&target_accounts.base_mint)?;
    let quote_mint = Pubkey::from_str(&target_accounts.quote_mint)?;
    let base_token_program = Pubkey::from_str(&target_accounts.accounts[11])?;
    let quote_token_program = Pubkey::from_str(&target_accounts.accounts[12])?;

    let our_base_ata = get_associated_token_address_with_program_id(payer, &base_mint, &base_token_program);
    let our_quote_ata = get_associated_token_address_with_program_id(payer, &quote_mint, &quote_token_program);

    let program_id = Pubkey::from_str(PUMPSWAP_PROGRAM)?;

    let mut accounts = vec![
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[0])?, false),  // pool
        AccountMeta::new(*payer, true),                                                       // user (signer)
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[2])?, false),  // global_config
        AccountMeta::new_readonly(base_mint, false),                                          // base_mint
        AccountMeta::new_readonly(quote_mint, false),                                         // quote_mint
        AccountMeta::new(our_base_ata, false),                                                // user_base_token_account
        AccountMeta::new(our_quote_ata, false),                                               // user_quote_token_account
        AccountMeta::new(Pubkey::from_str(&target_accounts.accounts[7])?, false),             // pool_base_token_account
        AccountMeta::new(Pubkey::from_str(&target_accounts.accounts[8])?, false),             // pool_quote_token_account
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[9])?, false),    // protocol_fee_recipient
        AccountMeta::new(Pubkey::from_str(&target_accounts.accounts[10])?, false),            // protocol_fee_recipient_token_account
        AccountMeta::new_readonly(base_token_program, false),                                 // base_token_program
        AccountMeta::new_readonly(quote_token_program, false),                                // quote_token_program
        AccountMeta::new_readonly(solana_program::system_program::id(), false),                // system_program
        AccountMeta::new_readonly(Pubkey::from_str(ATA_PROGRAM)?, false),                     // associated_token_program
        AccountMeta::new_readonly(Pubkey::from_str(&target_accounts.accounts[15])?, false),   // event_authority
        AccountMeta::new_readonly(program_id, false),                                          // program (self)
    ];

    // Extra accounts from target
    for i in 17..target_accounts.accounts.len() {
        if let Ok(pk) = Pubkey::from_str(&target_accounts.accounts[i]) {
            accounts.push(AccountMeta::new_readonly(pk, false));
        }
    }

    // Instruction data: discriminator + base_amount_in (u64) + min_quote_amount_out (u64)
    let mut data = SELL_DISCRIMINATOR.to_vec();
    data.extend(token_amount.to_le_bytes());
    data.extend(min_sol_out.to_le_bytes());

    let sell_ix = Instruction {
        program_id,
        accounts,
        data,
    };

    info!(
        "PumpSwap sell: pool={}.. mint={}.. tokens={} min_sol={:.4} SOL",
        &target_accounts.pool[..target_accounts.pool.len().min(8)],
        &target_accounts.base_mint[..target_accounts.base_mint.len().min(8)],
        token_amount,
        min_sol_out as f64 / 1e9,
    );

    Ok(vec![sell_ix])
}

/// Parse PumpSwap buy/sell instruction from target's transaction.
/// Returns (is_buy, PumpSwapAccounts) if the instruction matches PumpSwap program.
pub fn parse_pumpswap_instruction(
    instruction: &serde_json::Value,
    pumpswap_idx: usize,
    account_keys: &[String],
) -> Option<(bool, PumpSwapAccounts)> {
    let program_id_idx = instruction.get("programIdIndex")?.as_u64()? as usize;
    if program_id_idx != pumpswap_idx {
        return None;
    }

    let data_b64 = instruction.get("data")?.as_str()?;
    let data = base64::engine::general_purpose::STANDARD.decode(data_b64).ok()?;
    if data.len() < 24 {
        return None; // discriminator (8) + amount (8) + amount (8)
    }

    let disc: [u8; 8] = data[..8].try_into().ok()?;
    let is_buy = disc == BUY_DISCRIMINATOR;
    let is_sell = disc == SELL_DISCRIMINATOR;
    if !is_buy && !is_sell {
        return None;
    }

    let amount1 = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let amount2 = u64::from_le_bytes(data[16..24].try_into().ok()?);

    let account_indices: Vec<usize> = instruction.get("accounts")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_u64().map(|i| i as usize))
        .collect();

    if account_indices.len() < 17 {
        debug!("PumpSwap instruction has {} accounts, need 17+", account_indices.len());
        return None;
    }

    let accounts: Vec<String> = account_indices.iter()
        .filter_map(|&idx| account_keys.get(idx).cloned())
        .collect();

    if accounts.len() < 17 {
        return None;
    }

    let (token_amount, sol_amount) = if is_buy {
        (amount1, amount2) // base_amount_out, max_quote_amount_in
    } else {
        (amount1, amount2) // base_amount_in, min_quote_amount_out
    };

    Some((is_buy, PumpSwapAccounts {
        accounts,
        pool: account_keys.get(account_indices[0])?.clone(),
        base_mint: account_keys.get(account_indices[3])?.clone(),
        quote_mint: account_keys.get(account_indices[4])?.clone(),
        token_amount,
        sol_amount,
    }))
}

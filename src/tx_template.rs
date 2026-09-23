use solana_sdk::{
    pubkey::Pubkey,
    hash::Hash,
    instruction::{Instruction, AccountMeta},
    compute_budget::ComputeBudgetInstruction,
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;
use log::{debug, info};

use crate::settings::Settings;
use crate::idl::{load_all_idls, SimpleIdl};
use crate::onchain_idl::{get_instruction_discriminator, compute_anchor_discriminator};
use crate::tx_builder::BuyArgs;

/// Well-known program IDs that never change
pub(crate) const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
pub(crate) const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub(crate) const RENT_SYSVAR: &str = "SysvarRent111111111111111111111111111111111";
pub(crate) const FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
pub(crate) const FEE_CONFIG_SEED: [u8; 32] = [1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170, 81, 137, 203, 151, 245, 210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176];

/// Pre-computed buy transaction template.
/// Everything that does NOT depend on a specific mint is computed once at startup.
/// When a buy is detected, `build_buy_tx` only needs to derive mint-specific PDAs
/// (bonding curve, ATA — pure local computation, ~microseconds).
pub struct BuyTemplate {
    /// Payer public key (from buy keypair)
    pub payer_pubkey: Pubkey,
    /// Pump.fun program ID
    pub pump_program: Pubkey,
    /// Token program (Token-2022 for pump.fun)
    pub token_program: Pubkey,
    /// System program
    pub system_program: Pubkey,
    /// Rent sysvar
    pub rent_sysvar: Pubkey,
    /// Fee program
    pub fee_program: Pubkey,
    /// Fee config PDA (derived from fee program + seed, constant)
    pub fee_config_pda: Pubkey,
    /// Global PDA (derived from pump program, constant)
    pub global_pda: Pubkey,
    /// Event authority PDA (derived from pump program, constant)
    pub event_authority: Pubkey,
    /// Global volume accumulator PDA
    pub global_vol_acc: Pubkey,
    /// User volume accumulator PDA (derived from pump program + payer)
    pub user_vol_acc: Pubkey,
    /// Buy discriminator (8 bytes from IDL or computed)
    pub buy_discriminator: [u8; 8],
    /// Which IDL was used (for logging)
    pub idl_name: String,
    /// The IDL program address to use (may differ from pump_program if IDL overrides it)
    pub idl_program_id: Pubkey,
    /// Pre-cached fee recipients (normal + mayhem)
    pub fee_recipient_normal: Pubkey,
    pub fee_recipient_mayhem: Pubkey,
    /// Default compute unit limit for buy transactions
    pub compute_unit_limit: u32,
    /// Whether IDL-based account building is available
    idl: Option<SimpleIdl>,
}

impl BuyTemplate {
    /// Create a new BuyTemplate by pre-computing all mint-independent values.
    /// Call this once at startup.
    pub fn new(
        payer_pubkey: Pubkey,
        settings: &Settings,
        fee_recipient_normal: Pubkey,
        fee_recipient_mayhem: Pubkey,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pump_program = Pubkey::from_str(&settings.pump_fun_program)?;
        let token_program = Pubkey::from_str(TOKEN_2022_PROGRAM)?;
        let system_program = Pubkey::from_str(SYSTEM_PROGRAM)?;
        let rent_sysvar = Pubkey::from_str(RENT_SYSVAR)?;
        let fee_program = Pubkey::from_str(FEE_PROGRAM)?;

        // Derive constant PDAs
        let (global_pda, _) = Pubkey::find_program_address(&[b"global"], &pump_program);
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &pump_program);
        let (fee_config_pda, _) = Pubkey::find_program_address(&[b"fee_config", &FEE_CONFIG_SEED], &fee_program);
        let (global_vol_acc, _) = Pubkey::find_program_address(&[b"global_volume_accumulator"], &pump_program);
        let (user_vol_acc, _) = Pubkey::find_program_address(&[b"user_volume_accumulator", payer_pubkey.as_ref()], &pump_program);

        // Load IDL and get discriminator
        let idls = load_all_idls();
        let pref = ["pumpfun", "pumpfunamm", "pumpfunfees"];
        let mut best_idl: Option<SimpleIdl> = None;
        let mut idl_name = "fallback".to_string();
        let mut idl_program_id = pump_program;

        for key in pref {
            if let Some(idl) = idls.get(key) {
                best_idl = Some(idl.clone());
                idl_name = key.to_string();
                idl_program_id = idl.address;
                break;
            }
        }

        let buy_discriminator = if let Some(ref idl) = best_idl {
            get_instruction_discriminator(idl, "buy")
                .unwrap_or_else(|_| compute_anchor_discriminator("buy"))
        } else {
            compute_anchor_discriminator("buy")
        };

        info!(
            "BuyTemplate initialized: pump={}, idl={}, discriminator={:?}, fee_config={}",
            pump_program, idl_name, buy_discriminator, fee_config_pda
        );

        Ok(Self {
            payer_pubkey,
            pump_program,
            token_program,
            system_program,
            rent_sysvar,
            fee_program,
            fee_config_pda,
            global_pda,
            event_authority,
            global_vol_acc,
            user_vol_acc,
            buy_discriminator,
            idl_name,
            idl_program_id,
            fee_recipient_normal,
            fee_recipient_mayhem,
            compute_unit_limit: 250_000,
            idl: best_idl,
        })
    }

    /// Build a complete buy transaction for a specific mint.
    /// Only mint-dependent derivations happen here (bonding curve PDA, ATA — all local, ~microseconds).
    ///
    /// Returns a list of instructions ready to be signed and sent.
    /// Does NOT include compute budget or tip instructions — those are added by helius_sender.
    pub fn build_buy_instructions(
        &self,
        mint: &Pubkey,
        token_amount: u64,
        sol_amount: f64,
        slippage_bps: u64,
        is_mayhem: bool,
        creator: Option<Pubkey>,
    ) -> Result<Vec<Instruction>, Box<dyn std::error::Error + Send + Sync>> {
        let base_cost_lamports = (sol_amount * 1_000_000_000.0) as u64;
        let slippage_multiplier = 1.0 + (slippage_bps as f64 / 10000.0);
        let max_sol_cost = (base_cost_lamports as f64 * slippage_multiplier) as u64;

        let fee_recipient = if is_mayhem { self.fee_recipient_mayhem } else { self.fee_recipient_normal };

        // Derive mint-specific PDAs (local, ~microseconds)
        let (bonding_curve, _) = Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &self.pump_program);
        let bonding_curve_ata = get_associated_token_address_with_program_id(&bonding_curve, mint, &self.token_program);
        let user_ata = get_associated_token_address_with_program_id(&self.payer_pubkey, mint, &self.token_program);

        // Build instruction data: discriminator + borsh-serialized args
        let mut data = self.buy_discriminator.to_vec();
        data.extend(borsh::to_vec(&BuyArgs {
            amount: token_amount,
            max_sol_cost: max_sol_cost,
            track_volume: None,
        })?);

        // Build buy instruction — try IDL first, fallback to hardcoded account layout
        let buy_instruction = if let Some(ref idl) = self.idl {
            let mut context = std::collections::HashMap::new();
            context.insert("mint".to_string(), *mint);
            context.insert("user".to_string(), self.payer_pubkey);
            context.insert("fee_recipient".to_string(), fee_recipient);
            context.insert("token_program".to_string(), self.token_program);
            context.insert("bonding_curve".to_string(), bonding_curve);
            if let Some(c) = creator {
                context.insert("bonding_curve.creator".to_string(), c);
                context.insert("bondingCurve.creator".to_string(), c);
                let (creator_vault, _) = Pubkey::find_program_address(&[b"creator-vault", c.as_ref()], &self.pump_program);
                context.insert("creator_vault".to_string(), creator_vault);
            }

            match idl.build_accounts_for("buy", &context) {
                Ok(metas) => {
                    debug!("BuyTemplate: IDL {} built {} accounts for mint {}", self.idl_name, metas.len(), mint);
                    Instruction {
                        program_id: self.idl_program_id,
                        accounts: metas,
                        data,
                    }
                }
                Err(e) => {
                    debug!("BuyTemplate: IDL failed ({}), using fallback for mint {}", e, mint);
                    self.build_fallback_instruction(mint, &bonding_curve, &bonding_curve_ata, &user_ata, &fee_recipient, creator, data)?
                }
            }
        } else {
            self.build_fallback_instruction(mint, &bonding_curve, &bonding_curve_ata, &user_ata, &fee_recipient, creator, data)?
        };

        // ATA creation (idempotent — safe even if exists)
        let create_ata = create_associated_token_account_idempotent(
            &self.payer_pubkey,
            &self.payer_pubkey,
            mint,
            &self.token_program,
        );

        Ok(vec![create_ata, buy_instruction])
    }

    /// Build a ready-to-sign Transaction with compute budget instructions included.
    /// This is the fastest path: call with mint + amount, get back a Transaction to sign.
    pub fn build_buy_tx(
        &self,
        mint: &Pubkey,
        token_amount: u64,
        sol_amount: f64,
        slippage_bps: u64,
        is_mayhem: bool,
        creator: Option<Pubkey>,
        blockhash: Hash,
        priority_fee_microlamports: u64,
    ) -> Result<Transaction, Box<dyn std::error::Error + Send + Sync>> {
        let buy_instructions = self.build_buy_instructions(
            mint, token_amount, sol_amount, slippage_bps, is_mayhem, creator,
        )?;

        let mut all_instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(self.compute_unit_limit),
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports),
        ];
        all_instructions.extend(buy_instructions);

        let tx = Transaction::new_with_payer(&all_instructions, Some(&self.payer_pubkey));
        // Caller must sign with payer keypair and set blockhash
        let mut tx = tx;
        tx.message.recent_blockhash = blockhash;

        Ok(tx)
    }

    /// Fallback account layout when IDL-based building fails.
    /// Uses the hardcoded pump.fun account order.
    fn build_fallback_instruction(
        &self,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        bonding_curve_ata: &Pubkey,
        user_ata: &Pubkey,
        fee_recipient: &Pubkey,
        creator: Option<Pubkey>,
        data: Vec<u8>,
    ) -> Result<Instruction, Box<dyn std::error::Error + Send + Sync>> {
        let creator_pk = creator.ok_or("creator_pubkey is required for buy instruction")?;
        let (creator_vault, _) = Pubkey::find_program_address(&[b"creator-vault", creator_pk.as_ref()], &self.pump_program);

        let accounts = vec![
            AccountMeta::new_readonly(self.global_pda, false),          // 0: global
            AccountMeta::new(*fee_recipient, false),                    // 1: fee_recipient
            AccountMeta::new_readonly(*mint, false),                    // 2: mint
            AccountMeta::new(*bonding_curve, false),                    // 3: bonding_curve
            AccountMeta::new(*bonding_curve_ata, false),                // 4: bonding_curve ATA
            AccountMeta::new(*user_ata, false),                         // 5: user ATA
            AccountMeta::new(self.payer_pubkey, true),                  // 6: user (signer)
            AccountMeta::new_readonly(self.system_program, false),      // 7: system_program
            AccountMeta::new_readonly(self.token_program, false),       // 8: token_program
            AccountMeta::new(creator_vault, false),                     // 9: creator_vault
            AccountMeta::new_readonly(self.event_authority, false),     // 10: event_authority
            AccountMeta::new_readonly(self.pump_program, false),        // 11: program
            AccountMeta::new(self.global_vol_acc, false),               // 12: global_vol_acc
            AccountMeta::new(self.user_vol_acc, false),                 // 13: user_vol_acc
            AccountMeta::new_readonly(self.fee_config_pda, false),      // 14: fee_config
            AccountMeta::new_readonly(self.fee_program, false),         // 15: fee_program
        ];

        Ok(Instruction {
            program_id: self.pump_program,
            accounts,
            data,
        })
    }
}

use serde_json::{json, Value};
use base64::{engine::general_purpose::STANDARD as Base64Engine, Engine};
use crate::{
    models::{Holding, PriceCache},
    settings::Settings,
    rpc::{detect_idl_for_mint, fetch_bonding_curve_creator, build_missing_ata_preinstructions, fetch_with_fallback, detect_token_program_for_mint, fetch_current_price_full},
    tx_builder::{build_buy_instruction},
    idl::load_all_idls,
    onchain_idl::get_instruction_discriminator,
    blockhash_cache::CachedBlockhash,
};
use solana_client::rpc_client::RpcClient;
use std::{sync::Arc, collections::HashMap};
use tokio::sync::Mutex;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::Transaction, pubkey::Pubkey,
};
use log::{info, warn, debug};
use std::str::FromStr;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use chrono::Utc;

// P0 fix: share the canonical fee_config PDA seed with tx_template.rs so the
// fallback BuyerConstants cannot drift from the BuyTemplate fast path.
use crate::tx_template::{FEE_CONFIG_SEED as BUYER_FEE_CONFIG_SEED, FEE_PROGRAM as BUYER_FEE_PROGRAM};

/// Cached fee recipients from Global PDA (normal + mayhem mode).
/// Fetched once at startup, reused for all buys.
pub struct CachedFeeRecipients {
    pub normal: Pubkey,
    pub mayhem: Pubkey,
}

/// Pre-computed constants for buy TX construction (avoid re-deriving every buy).
/// Created once at startup, shared across all buys.
///
/// **DEPRECATED**: Use `BuyTemplate` instead. `BuyerConstants` duplicates pubkeys
/// that `BuyTemplate` already manages correctly (including mayhem routing).
/// Kept only as a fallback path when no BuyTemplate is available.
#[deprecated(note = "Use BuyTemplate instead — handles mayhem routing and IDL accounts correctly")]
pub struct BuyerConstants {
    pub pump_program: Pubkey,
    pub global: Pubkey,
    pub fee_recipient: Pubkey,
    pub event_authority: Pubkey,
    pub fee_config: Pubkey,
    pub fee_program: Pubkey,
    pub global_volume_acc: Pubkey,
    pub user_volume_acc: Pubkey,
    pub token_program: Pubkey,
    pub payer: Pubkey,
    pub buy_discriminator: [u8; 8],
}

impl BuyerConstants {
    pub fn new(payer: &Pubkey, pump_program: &str) -> Self {
        let pump_program_pk = Pubkey::from_str(pump_program).expect("valid pump_program pubkey literal");
        let (user_volume_acc, _) = Pubkey::find_program_address(
            &[b"user_volume_accumulator", payer.as_ref()], &pump_program_pk
        );
        Self {
            pump_program: pump_program_pk,
            global: Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").expect("valid global pubkey literal"),
            fee_recipient: Pubkey::from_str("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV").expect("valid fee_recipient pubkey literal"),
            event_authority: Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1").expect("valid event_authority pubkey literal"),
            fee_config: {
                // P0 fix: derive fee_config from the same seed/program as tx_template.rs
                // instead of hardcoding a duplicate pubkey literal.
                let fee_program_pk = Pubkey::from_str(BUYER_FEE_PROGRAM).expect("valid fee_program literal");
                Pubkey::find_program_address(&[b"fee_config", &BUYER_FEE_CONFIG_SEED], &fee_program_pk).0
            },
            fee_program: Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").expect("valid fee_program pubkey literal"),
            global_volume_acc: Pubkey::from_str("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y").expect("valid global_volume_acc pubkey literal"),
            user_volume_acc,
            // P0 fix: pump.fun uses standard SPL Token program, NOT Token-2022.
            // Previously hardcoded `TokenzQd...` (Token-2022) here, which would derive
            // the wrong ATA and every fallback buy would fail on-chain.
            token_program: Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").expect("valid token_program pubkey literal"),
            payer: *payer,
            buy_discriminator: crate::onchain_idl::compute_anchor_discriminator("buy"),
        }
    }
}

pub async fn buy_token(
    mint: &str,
    sol_amount: f64,
    is_real: bool,
    keypair: Option<&Keypair>,
    simulate_keypair: Option<&Keypair>,
    price_cache: Arc<Mutex<PriceCache>>,
    rpc_client: &Arc<RpcClient>,
    settings: &Arc<Settings>,
    cached_blockhash: Option<CachedBlockhash>,
    cached_fee_recipients: Option<&CachedFeeRecipients>,
    swqos_sender: Option<&Arc<crate::swqos_sender::SwqosSender>>,
    target_sol_spent: Option<f64>,
    target_token_amount: Option<u64>,
    target_pump_accounts: Option<&Vec<String>>,
    buy_template: Option<&Arc<crate::tx_template::BuyTemplate>>,
    cached_leader: Option<crate::leader_cache::CachedLeader>,
) -> Result<Holding, Box<dyn std::error::Error + Send + Sync>> {
    // === FAST PATH: scale tokens from target TX (no RPC calls) ===
    let price_start = std::time::Instant::now();
    let max_sol_cost_lamports: u64 = 100_000_000; // 0.10 SOL
    let decimals: i32 = 6; // All pump.fun tokens are 6 decimals
    // P0 fix: pump.fun mints use standard SPL Token program, NOT Token-2022.
    // Previously hardcoded `TokenzQd...` (Token-2022) here — wrong program → wrong ATA → buy fails.
    let token_program_id = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").expect("valid token_program pubkey literal");

    let (token_amount, buy_price_sol, timing_price_ms) = if let (Some(t_sol), Some(t_tokens)) = (target_sol_spent, target_token_amount) {
        // Scale tokens from target TX: our_tokens = target_tokens × (our_sol / target_sol)
        // No RPC calls needed — 0ms!
        let scaled = ((t_tokens as f64) * (sol_amount / t_sol)) as u64;
        // 10% haircut for safety
        let safe_amount = std::cmp::max((scaled as f64 * 0.90) as u64, 1000);
        let price = if t_tokens > 0 { t_sol / (t_tokens as f64 / 1e6) } else { 0.0 };
        let ms = price_start.elapsed().as_millis();
        info!("Buy {} FAST: scaled={} safe(90%)={} from target {:.6} SOL → {} tokens | 0ms (no RPC)",
            mint, scaled, safe_amount, t_sol, t_tokens);
        (safe_amount, price, ms)
    } else {
        // SLOW PATH fallback: fetch bonding curve from RPC
        warn!("Buy {} SLOW PATH: no target data, fetching bonding curve via RPC", mint);
        let price_fut = fetch_current_price_full(mint, &price_cache, rpc_client, settings);
        let price_data = price_fut.await?;
        let ms = price_start.elapsed().as_millis();
        let sol_lamports = (sol_amount * 1_000_000_000.0) as u128;
        let vsr = price_data.state.virtual_sol_reserves as u128;
        let vtr = price_data.state.virtual_token_reserves as u128;
        let rtr = price_data.state.real_token_reserves as u128;
        let n: u128 = vsr * vtr;
        let i: u128 = vsr + sol_lamports;
        let r: u128 = n / i + 1;
        let s: u128 = vtr.saturating_sub(r);
        let exact_tokens = std::cmp::min(s, rtr) as u64;
        let safe_amount = std::cmp::max((exact_tokens as f64 * 0.90) as u64, 1000);
        info!("Buy {} SLOW: exact={} safe(90%)={} | {}ms RPC", mint, exact_tokens, safe_amount, ms);
        (safe_amount, price_data.price, ms)
    };

    // Fee recipient — use cached fee recipients if available, falling back to
    // the hardcoded standard recipient. The initial value is `normal`; the
    // per-mint mayhem detection below overrides it to `mayhem` when needed.
    let mut fee_recipient = if let Some(cached) = cached_fee_recipients {
        cached.normal
    } else {
        // Hardcoded fallback (standard fee recipient)
        Pubkey::from_str("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV").expect("valid fee_recipient pubkey literal")
    };

    info!(
        "Buy {}: {} tokens for {:.6} SOL (max_sol={:.4}, price: {:.12} SOL/token) | price_ms={}",
        mint, token_amount, sol_amount, max_sol_cost_lamports as f64 / 1e9, buy_price_sol, timing_price_ms
    );

    if is_real {
        // Wrap sync RpcClient call in spawn_blocking to avoid blocking the tokio runtime
        let url = settings.solana_rpc_urls[0].clone();
        let client = Arc::new(tokio::task::spawn_blocking(move || RpcClient::new(url)).await?);
        let payer = keypair.ok_or("Keypair required")?;
        debug!("Preparing buy TX for mint {} amount {} SOL (real)", mint, sol_amount);
        let build_start = std::time::Instant::now();

        // === BLOCKHASH EXPIRY CHECK ===
        // Validate that the cached blockhash hasn't expired before signing.
        // An expired blockhash causes the TX to be rejected = wasted SOL on fees.
        let (resolved_blockhash, was_refreshed) = {
            let client_clone = client.clone();
            let cb = cached_blockhash.clone();
            tokio::task::spawn_blocking(move || -> Result<(solana_sdk::hash::Hash, bool), Box<dyn std::error::Error + Send + Sync>> {
                // resolve_blockhash is sync (uses blocking RPC calls), so run in spawn_blocking
                match cb {
                    Some(cached) => {
                        match client_clone.get_block_height() {
                            Ok(current_height) => {
                                let safety_margin: u64 = 10;
                                if current_height + safety_margin <= cached.last_valid_block_height {
                                    debug!(
                                        "Cached blockhash still valid (height={}, valid_until={}, age={:.0}ms)",
                                        current_height,
                                        cached.last_valid_block_height,
                                        cached.fetched_at.elapsed().as_millis()
                                    );
                                    Ok((cached.blockhash, false))
                                } else {
                                    warn!(
                                        "Cached blockhash EXPIRED (height={}, valid_until={}), fetching fresh blockhash",
                                        current_height, cached.last_valid_block_height
                                    );
                                    match client_clone.get_latest_blockhash() {
                                        Ok(fresh) => Ok((fresh, true)),
                                        Err(e) => {
                                            warn!("Failed to fetch fresh blockhash: {}. Using expired cache as last resort.", e);
                                            Ok((cached.blockhash, false))
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to get block height for expiry check: {}. Using cached blockhash.", e);
                                // If we can't check, assume the cached hash is still valid
                                // (it refreshes every 400ms, so it's very likely fresh)
                                Ok((cached.blockhash, false))
                            }
                        }
                    }
                    None => {
                        warn!("No cached blockhash available, fetching fresh blockhash");
                        match client_clone.get_latest_blockhash() {
                            Ok(fresh) => Ok((fresh, true)),
                            Err(e) => Err(format!("No cached blockhash and failed to fetch fresh: {}", e).into()),
                        }
                    }
                }
            }).await??
        };
        if was_refreshed {
            info!("Buy {}: using freshly fetched blockhash (cached was expired or missing)", mint);
        }
        
        let mint_pk = Pubkey::from_str(mint)?;
        let payer_pubkey = payer.pubkey();
        let pump_program_pk = Pubkey::from_str(&settings.pump_fun_program)?;

        // Resolve the creator pubkey (needed for creator-vault derivation).
        // FAST: target accounts[9] is the creatorVault PDA — derive creator-vault locally
        // from the on-chain creator when target accounts are unavailable.
        let creator_opt: Option<Pubkey> = fetch_bonding_curve_creator(mint, rpc_client, settings).await.ok().flatten();

        // Per-mint mayhem detection: mayhem-mode tokens require the reserved
        // fee_recipient. We detect it from the bonding curve state with retry on
        // RPC failure (G-CONCERN-2 fix: previously defaulted silently to non-mayhem
        // on a single RPC failure, routing fees incorrectly in production).
        let is_mayhem = crate::rpc::detect_mayhem_with_retry(mint, rpc_client, settings).await;

        // P1-2 fix: route fee_recipient based on per-mint mayhem detection.
        // Without this, the mayhem pubkey in CachedFeeRecipients was never used
        // for buys — only `normal` was selected, causing mayhem-mode token buys
        // to fail or route fees to the wrong recipient.
        if is_mayhem {
            fee_recipient = cached_fee_recipients.map(|c| c.mayhem).unwrap_or(fee_recipient);
            info!("Mayhem-mode token detected for {} — using mayhem fee_recipient", mint);
        }

        // PRIMARY PATH: build the buy instructions via BuyTemplate. The template
        // owns the correct fee_config PDA, the mayhem-aware fee_recipient routing
        // (tx_template.rs handles is_mayhem), and the canonical account layout —
        // there are NO duplicate hardcoded constants here.
        let mut all_instrs: Vec<solana_program::instruction::Instruction> = if let Some(template) = buy_template {
            let buy_instrs = template.build_buy_instructions(
                &mint_pk,
                token_amount,
                sol_amount,
                settings.slippage_bps,
                is_mayhem,
                creator_opt,
            )?;
            info!(
                "Buy TX (BuyTemplate): {} instrs, tokens={}, mayhem={}, idl={}",
                buy_instrs.len(), token_amount, is_mayhem, template.idl_name
            );
            buy_instrs
        } else {
            // FALLBACK PATH: no BuyTemplate available. Build from BuyerConstants
            // (which hold the CORRECT pubkeys) — never inline-hardcoded duplicates.
            #[allow(deprecated)] // BuyerConstants is deprecated; kept as fallback only
            let consts = BuyerConstants::new(&payer_pubkey, &settings.pump_fun_program);
            let fee_recipient_fb = if is_mayhem {
                cached_fee_recipients.map(|c| c.mayhem).unwrap_or(fee_recipient)
            } else {
                fee_recipient
            };
            let (bonding_curve, _) = Pubkey::find_program_address(&[b"bonding-curve", mint_pk.as_ref()], &pump_program_pk);
            let assoc_bonding_curve = get_associated_token_address_with_program_id(&bonding_curve, &mint_pk, &token_program_id);
            let assoc_user = get_associated_token_address_with_program_id(&payer_pubkey, &mint_pk, &token_program_id);
            // creatorVault: prefer target accounts[9] (no RPC), else derive from creator
            let creator_vault = if let Some(accts) = target_pump_accounts {
                if accts.len() > 9 {
                    Pubkey::from_str(&accts[9]).map_err(|e| format!("Invalid target creatorVault[9]: {}", e))?
                } else {
                    let creator = creator_opt.ok_or("creator_pubkey required for buy")?;
                    Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &pump_program_pk).0
                }
            } else {
                let creator = creator_opt.ok_or("creator_pubkey required for buy")?;
                Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &pump_program_pk).0
            };

            use solana_program::instruction::AccountMeta;
            let buy_accounts = vec![
                AccountMeta::new_readonly(consts.global, false),               // [0]  global
                AccountMeta::new(fee_recipient_fb, false),                     // [1]  feeRecipient (writable)
                AccountMeta::new_readonly(mint_pk, false),                     // [2]  mint
                AccountMeta::new(bonding_curve, false),                        // [3]  bondingCurve (writable)
                AccountMeta::new(assoc_bonding_curve, false),                  // [4]  assocBondingCurve (writable)
                AccountMeta::new(assoc_user, false),                           // [5]  assocUser (writable)
                AccountMeta::new(payer_pubkey, true),                          // [6]  user/signer (writable)
                AccountMeta::new_readonly(solana_program::system_program::id(), false), // [7]  systemProgram
                AccountMeta::new_readonly(token_program_id, false),            // [8]  tokenProgram (IDL order)
                AccountMeta::new(creator_vault, false),                        // [9]  creatorVault (writable)
                AccountMeta::new_readonly(consts.event_authority, false),      // [10] eventAuthority
                AccountMeta::new_readonly(consts.pump_program, false),         // [11] pumpProgram
                AccountMeta::new(consts.global_volume_acc, false),             // [12] globalVolumeAccumulator (writable)
                AccountMeta::new(consts.user_volume_acc, false),               // [13] userVolumeAccumulator (writable)
                AccountMeta::new_readonly(consts.fee_config, false),           // [14] feeConfig
                AccountMeta::new_readonly(consts.fee_program, false),          // [15] feeProgram
            ];

            let mut instr_data = consts.buy_discriminator.to_vec();
            instr_data.extend(borsh::to_vec(&crate::tx_builder::BuyArgs {
                amount: token_amount,
                max_sol_cost: max_sol_cost_lamports,
                track_volume: None,
            }).map_err(|e| format!("failed to serialize BuyArgs: {e}"))?);

            info!(
                "Buy TX (fallback/BuyerConstants): {} accounts, tokens={}, mayhem={}",
                buy_accounts.len(), token_amount, is_mayhem
            );
            let instruction = solana_program::instruction::Instruction {
                program_id: consts.pump_program,
                accounts: buy_accounts,
                data: instr_data,
            };
            vec![
                create_associated_token_account_idempotent(&payer_pubkey, &payer_pubkey, &mint_pk, &token_program_id),
                instruction,
            ]
        };

        // Append any remaining target accounts (e.g., account [16] = creator PDA)
        // to the buy instruction so on-chain account layout matches the target TX.
        if let Some(target_accts) = target_pump_accounts {
            if target_accts.len() > 16 {
                if let Some(buy_ix) = all_instrs.last_mut() {
                    use solana_program::instruction::AccountMeta;
                    for i in 16..target_accts.len() {
                        if let Ok(pk) = Pubkey::from_str(&target_accts[i]) {
                            buy_ix.accounts.push(AccountMeta::new_readonly(pk, false));
                            info!("Added target account [{}]: {}", i, &target_accts[i]);
                        }
                    }
                }
            }
        }

        // Dev fee removed — not applicable for copytrade sniper
        
        let timing_build_ms = build_start.elapsed().as_millis();
        let send_start = std::time::Instant::now();

        // Record pre-send SOL balance to compute actual cost after the transaction
        // Wrap sync RpcClient call in spawn_blocking to avoid blocking the tokio runtime
        let pre_sol_lamports = {
            let client_clone = client.clone();
            let payer_pubkey_clone = payer_pubkey;
            tokio::task::spawn_blocking(move || client_clone.get_balance(&payer_pubkey_clone))
                .await?
                ?
        };

        // Choose transaction submission method
        let mut final_token_amount_u64: Option<u64> = None;
        let mut buy_tx_signature: Option<String> = None;
        if let Some(swqos) = swqos_sender {
            // SWQoS concurrent send: build final TX via helius_sender (compute budget,
            // tip, signing), then fan out to all providers simultaneously
            info!("Using SWQoS concurrent send for buy of mint {}", mint);
            let tx_base64 = crate::helius_sender::build_signed_transaction(
                all_instrs,
                payer,
                settings,
                &*client,
                Some(resolved_blockhash),
                cached_leader.clone(),
            ).await?;
            let (signature, results) = swqos.send_concurrent(&tx_base64, settings, cached_leader.clone()).await?;
            info!("Buy TX sent via SWQoS ({} providers): {}", results.len(), signature);
            buy_tx_signature = Some(signature);
        } else if settings.helius_sender_enabled {
            info!("Using Helius Sender for buy transaction of mint {}", mint);
            let signature = crate::helius_sender::send_transaction_with_retry(
                all_instrs,
                payer,
                settings,
                &*client,
                3, // max retries
                Some(resolved_blockhash),
            ).await?;
            info!("Buy transaction sent via Helius Sender: {}", signature);
            buy_tx_signature = Some(signature.clone());
            // Poll for signature confirmation instead of blind 2s sleep.
            // Polls every 400ms up to 5s — returns faster when confirmed, doesn't
            // waste time when slow, and catches on-chain failures early.
            let poll_timeout = std::time::Duration::from_secs(5);
            let poll_interval = std::time::Duration::from_millis(400);
            let confirmed = crate::rpc::poll_signature_confirmation(
                &signature,
                rpc_client,
                settings,
                poll_timeout,
                poll_interval,
            ).await.unwrap_or(false);

            if confirmed {
                let owner_str = payer_pubkey.to_string();
                if let Ok(Some(acc)) = crate::rpc::find_token_account_owned_by_owner(mint, &owner_str, rpc_client, settings).await {
                    if let Ok(pk) = Pubkey::from_str(&acc) {
                        // Wrap sync RpcClient call in spawn_blocking to avoid blocking the tokio runtime
                        let client_clone = client.clone();
                        if let Ok(balance) = tokio::task::spawn_blocking(move || client_clone.get_token_account_balance(&pk)).await? {
                            if let Ok(amount_u64) = balance.amount.parse::<u64>() {
                                if amount_u64 > 0 {
                                    final_token_amount_u64 = Some(amount_u64);
                                    info!("Buy confirmed on-chain for {}: {} tokens | sig: {}", mint, amount_u64, signature);
                                }
                            }
                        }
                    }
                }
            }
            if final_token_amount_u64.is_none() {
                info!("Buy {} sent (sig: {}) — returning estimated holding (poll timeout or TX failed)", mint, signature);
            }
        } else {
            let mut tx = Transaction::new_with_payer(&all_instrs, Some(&payer.pubkey()));
            // Blockhash already validated and resolved above (expiry check)
            tx.sign(&[payer], resolved_blockhash);
            // Wrap sync send_and_confirm in spawn_blocking to avoid blocking tokio runtime
            {
                let client_clone = client.clone();
                tokio::task::spawn_blocking(move || client_clone.send_and_confirm_transaction(&tx))
                    .await?
                    ?;
            }
            // Query on-chain token accounts to find exact token balance for payer
            let owner_str = payer_pubkey.to_string();
            if let Ok(Some(acc)) = crate::rpc::find_token_account_owned_by_owner(mint, &owner_str, rpc_client, settings).await {
                if let Ok(pk) = Pubkey::from_str(&acc) {
                    // Wrap sync RpcClient call in spawn_blocking
                    let client_clone = client.clone();
                    if let Ok(balance) = tokio::task::spawn_blocking(move || client_clone.get_token_account_balance(&pk)).await? {
                        if let Ok(amount_u64) = balance.amount.parse::<u64>() {
                            final_token_amount_u64 = Some(amount_u64);
                        }
                    }
                }
            }
        }
        // If we fetched an on-chain amount, use it as the final token amount
        if let Some(exact) = final_token_amount_u64 {
            info!("Buy complete: on-chain token amount for {} = {} (base units)", mint, exact);
            // Compute actual SOL cost from on-chain balance delta
            // Wrap sync RpcClient call in spawn_blocking
            let post_sol_lamports = {
                let client_clone = client.clone();
                let payer_clone = payer_pubkey;
                tokio::task::spawn_blocking(move || client_clone.get_balance(&payer_clone))
                    .await?
                    .unwrap_or(pre_sol_lamports)
            };
            let buy_cost_sol = if pre_sol_lamports > post_sol_lamports {
                (pre_sol_lamports - post_sol_lamports) as f64 / 1_000_000_000.0
            } else {
                warn!("Buy balance unchanged or increased for {} (pre={}, post={}), falling back to intended amount {}",
                      mint, pre_sol_lamports, post_sol_lamports, sol_amount);
                sol_amount // fallback to intended amount
            };
            info!("Buy accounting for {}: pre_sol={} post_sol={} cost={:.9} SOL (intended {:.9} SOL)",
                  mint, pre_sol_lamports, post_sol_lamports, buy_cost_sol, sol_amount);
            // Use this exact amount for returned holding
            let timing_send_ms = send_start.elapsed().as_millis();
            return Ok(Holding {
                amount: exact,
                original_amount: exact,
                buy_price: buy_price_sol,
                buy_time: Utc::now(),
                decimals: decimals as u8,
                buy_cost_sol: Some(buy_cost_sol),
                metadata: None,
                onchain_raw: None,
                onchain: None,
                triggered_tp_levels: vec![],
                triggered_sl_levels: vec![],
                migrated: false,
                pending_sell: false,
                dex: "pumpfun".to_string(),
                amm_pool: None,
                target_buy_tokens: None, // set by caller from DetectedBuy
                extra_pump_account: target_pump_accounts.and_then(|a| a.get(16).cloned()),
                pumpswap_accounts: None,
                buy_signature: buy_tx_signature.clone(),
                timing_price_ms,
                timing_build_ms,
                timing_send_ms,
            });
        } else if is_real {
            // TX was sent but we couldn't verify on-chain.
            // Create holding with estimated values so we can still sell.
            // Better to track a possibly-phantom position than miss sells entirely.
            warn!("Creating estimated holding for {} — TX sent but unverified on-chain. token_amount={}, sol={:.4}",
                mint, token_amount, sol_amount);
            let timing_send_ms = send_start.elapsed().as_millis();
            return Ok(Holding {
                amount: token_amount,
                original_amount: token_amount,
                buy_price: buy_price_sol,
                buy_time: Utc::now(),
                decimals: decimals as u8,
                buy_cost_sol: Some(sol_amount),
                metadata: None,
                onchain_raw: None,
                onchain: None,
                triggered_tp_levels: vec![],
                triggered_sl_levels: vec![],
                migrated: false,
                pending_sell: false,
                dex: "pumpfun".to_string(),
                amm_pool: None,
                target_buy_tokens: None,
                extra_pump_account: target_pump_accounts.and_then(|a| a.get(16).cloned()),
                pumpswap_accounts: None,
                buy_signature: buy_tx_signature.clone(),
                timing_price_ms,
                timing_build_ms,
                timing_send_ms,
            });
        }
    } else {
        // Dry-run simulation: construct same instruction and simulate it using
        // either the provided simulate_keypair or an ephemeral Keypair fallback.
        // Wrap sync RpcClient call in spawn_blocking to avoid blocking the tokio runtime
        let url = settings.solana_rpc_urls[0].clone();
        let client = Arc::new(tokio::task::spawn_blocking(move || RpcClient::new(url)).await?);
        // keep an owned Keypair alive in this scope if we need to create one
        let mut _maybe_owned_sim: Option<Keypair> = None;
        let sim_payer_ref: &Keypair = if let Some(k) = simulate_keypair {
            k
        } else {
            let owned_sim = Keypair::new();
            _maybe_owned_sim = Some(owned_sim);
            _maybe_owned_sim.as_ref().ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("Failed to get sim keypair ref"))?
        };
        debug!("Preparing simulated buy TX for mint {} amount {} SOL (dry run)", mint, sol_amount);
        // Dry-run uses same buy instruction params
        let program_id = Pubkey::from_str(&settings.pump_fun_program)?;
        let creator_opt = fetch_bonding_curve_creator(mint, rpc_client, settings).await.ok().flatten();
        let sim_payer_pubkey = sim_payer_ref.pubkey();
        // Try to build accounts via IDL-aware builder for exactness
        let idls = load_all_idls();
        let mut instruction_opt: Option<solana_program::instruction::Instruction> = None;
        let mut last_err: Option<String> = None;
        let mint_pk = Pubkey::from_str(mint)?;
        if let Some(idl) = idls.get("pumpfun") {
            // prepare context map
            let mut context: HashMap<String, Pubkey> = HashMap::new();
            context.insert("mint".to_string(), mint_pk);
            context.insert("user".to_string(), sim_payer_pubkey);
            if let Some(c) = creator_opt { 
                context.insert("bonding_curve.creator".to_string(), c); 
                context.insert("bondingCurve.creator".to_string(), c); 
            }
            // Add bonding_curve PDA
            let pump_program_pk = Pubkey::from_str(&settings.pump_fun_program)?;
            let (curve_pda, _) = Pubkey::find_program_address(&[b"bonding-curve", mint_pk.as_ref()], &pump_program_pk);
            context.insert("bonding_curve".to_string(), curve_pda);
            // Add creator_vault PDA if creator exists
            if let Some(creator) = context.get("bonding_curve.creator").cloned() {
                let (creator_vault, _) = Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &pump_program_pk);
                context.insert("creator_vault".to_string(), creator_vault);
            }
            // Add fee_recipient - use the already-fetched authorized fee recipient
            context.insert("fee_recipient".to_string(), fee_recipient);
            // Add token program so IDL resolves the correct one (Token-2022 vs SPL Token)
            context.insert("token_program".to_string(), token_program_id);
            // NOTE: fee_program is invoked via CPI, not included in main instruction accounts
            // Do NOT add fee_program to context
            match idl.build_accounts_for("buy", &context) {
                Ok(metas) => {
                    // Filter out feeProgram — CPI only
                    let fee_program_pk = Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").expect("valid fee_program pubkey literal");
                    let metas: Vec<_> = metas.into_iter().filter(|m| m.pubkey != fee_program_pk).collect();
                    debug!("IDL build_accounts_for(buy) using {} accounts (dry-run)", metas.len());
                    for (i, meta) in metas.iter().enumerate() {
                        debug!("  [{}] {} (signer={}, writable={})", i, meta.pubkey, meta.is_signer, meta.is_writable);
                    }
                    let discriminator = get_instruction_discriminator(&idl, "buy")
                        .unwrap_or_else(|_| crate::onchain_idl::compute_anchor_discriminator("buy"));
                    instruction_opt = Some(solana_program::instruction::Instruction { program_id, accounts: metas, data: {
                        let mut d = discriminator.to_vec();
                        d.extend(borsh::to_vec(&crate::tx_builder::BuyArgs {
                            amount: token_amount,
                            max_sol_cost: max_sol_cost_lamports,
                            track_volume: None,
                        }).map_err(|e| format!("failed to serialize BuyArgs: {e}"))?);
                        d
                    }});
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        let instruction = if let Some(instr) = instruction_opt { instr } else {
            if let Some(e) = last_err { debug!("IDL build failed for buy: {}", e); }
            // fallback to legacy builder — market buy
            build_buy_instruction(
                &program_id,
                mint,
                token_amount,
                max_sol_cost_lamports,
                Some(false),
                &sim_payer_pubkey,
                &fee_recipient,
                creator_opt,
                settings,
            )?
        };
    // include any pre_instructions in the simulated tx (e.g., ATA creation)
    let mut tx_instructions = Vec::new();
        // Build pre_instructions for dry-run (ensure ATA exists for sim payer)
        let mut pre_instructions: Vec<solana_program::instruction::Instruction> = Vec::new();
        let mint_pk = Pubkey::from_str(mint)?;
        let ata = get_associated_token_address_with_program_id(&sim_payer_pubkey, &mint_pk, &token_program_id);
        match fetch_with_fallback::<Value>(json!({
            "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
            "params": [ ata.to_string(), { "encoding": "base64", "commitment": "confirmed" } ]
        }), "getAccountInfo", rpc_client, settings).await {
            Ok(info) => {
                if info.result.is_none() {
                    pre_instructions.push(create_associated_token_account_idempotent(&sim_payer_pubkey, &sim_payer_pubkey, &mint_pk, &token_program_id));
                } else if let Some(result_val) = info.result {
                    let val = if let Some(v) = result_val.get("value") { v.clone() } else { result_val.clone() };
                    if val.is_null() {
                        pre_instructions.push(create_associated_token_account_idempotent(&sim_payer_pubkey, &sim_payer_pubkey, &mint_pk, &token_program_id));
                    }
                }
            }
            Err(e) => debug!("Failed to check ATA existence for {}: {}", ata, e),
        }
        // we can't easily create payer-signed ATA in dry-run when using ephemeral keypair,
        // but include the instruction so simulateTransaction can check behavior.
        // convert pre_instructions (created with payer_pubkey) to use sim_payer_pubkey when needed
        for pi in pre_instructions.into_iter() {
            // ensure the payer pubkey in create_associated_token_account matches sim_payer
            // the create_associated_token_account sets accounts; it's safe to just push as-is
            tx_instructions.push(pi);
        }
        tx_instructions.push(instruction.clone());
        
        // Debug: log instruction details before simulation
        debug!("DRY RUN buy simulation for {}: program_id={}", mint, instruction.program_id);
        debug!("  Instruction has {} accounts:", instruction.accounts.len());
        for (i, acc) in instruction.accounts.iter().enumerate() {
            debug!("    [{}] {} (signer={}, writable={})", i, acc.pubkey, acc.is_signer, acc.is_writable);
        }
        debug!("  Instruction data length: {} bytes", instruction.data.len());
        debug!("  Payer (sim wallet): {}", sim_payer_pubkey);
        
        let mut tx = Transaction::new_with_payer(&tx_instructions, Some(&sim_payer_pubkey));
        // Use cached blockhash if available, otherwise fetch from RPC
        // For dry-run, we just need a valid hash — expiry isn't critical for simulation
        let blockhash_result = match cached_blockhash {
            Some(cb) => Ok(cb.blockhash),
            None => {
                let client_clone = client.clone();
                tokio::task::spawn_blocking(move || client_clone.get_latest_blockhash())
                    .await?
            }
        };
        match blockhash_result {
            Ok(blockhash) => {
                // For dry-run simulation with ephemeral keypair:
                // We build the transaction correctly but cannot fully simulate because
                // the ephemeral keypair has no SOL and its ATAs don't exist on-chain.
                // This is expected - the transaction building itself validates the logic.
                tx.message.recent_blockhash = blockhash;
                // Sign the transaction with the simulate payer so remote simulate endpoints
                // which expect a signed transaction will behave more predictably.
                tx.sign(&[sim_payer_ref], blockhash);

                // Serialize and send to Helius simulate endpoint for consistent simulation
                match bincode::serialize(&tx) {
                    Ok(serialized) => {
                        let tx_base64 = Base64Engine.encode(&serialized);
                        match crate::helius_sender::simulate_transaction_via_helius(&tx_base64, &*settings).await {
                            Ok(json) => {
                                // Try to inspect error field inside result if present
                                if let Some(err) = json.get("error") {
                                    warn!("DRY RUN buy simulation error for {}: {}", mint, err);
                                } else {
                                    info!("DRY RUN buy simulation completed for {} (helius)", mint);
                                }
                            }
                            Err(e) => warn!("DRY RUN buy simulation (helius) failed for {}: {}", mint, e),
                        }
                    }
                    Err(e) => warn!("Failed to serialize TX for dry-run simulate: {}", e),
                }
            }
            Err(e) => warn!("DRY RUN cannot get latest blockhash for {}: {}", mint, e),
        }
    }
    // Return simulated holding for dry runs
    Ok(Holding {
        amount: token_amount,
        original_amount: token_amount,
        buy_price: buy_price_sol,
        buy_time: Utc::now(),
        decimals: decimals as u8,
        buy_cost_sol: None,
        metadata: None,
        onchain_raw: None,
        onchain: None,
        triggered_tp_levels: vec![],
        triggered_sl_levels: vec![],
        migrated: false,
        pending_sell: false,
        dex: "pumpfun".to_string(),
        amm_pool: None,
        target_buy_tokens: None, // set by caller from DetectedBuy
        extra_pump_account: target_pump_accounts.and_then(|a| a.get(16).cloned()),
        pumpswap_accounts: None,
        buy_signature: None,
        timing_price_ms,
        timing_build_ms: 0,
        timing_send_ms: 0,
    })
}
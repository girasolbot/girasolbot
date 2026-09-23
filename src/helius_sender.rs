// Allow deprecated system_instruction module until solana_system_interface is available
#![allow(deprecated)]

use solana_sdk::{
    transaction::VersionedTransaction,
    message::{Message, VersionedMessage},
    signature::{Keypair, Signer},
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_instruction,
    compute_budget::ComputeBudgetInstruction,
    commitment_config::CommitmentConfig,
};
use std::str::FromStr;
use solana_client::rpc_client::RpcClient;
use std::error::Error;
use std::sync::Arc;
use crate::settings::Settings;
use log::{debug, info, warn};
use serde_json::{json, Value};
use base64::{engine::general_purpose::STANDARD as Base64Engine, Engine};

/// Reuse shared HTTP client from rpc module for connection pooling
fn shared_client() -> &'static reqwest::Client {
    &*crate::rpc::SHARED_HTTP_CLIENT
}

// 0slot.trade staked_conn recipient accounts (transfer ≥0.001 SOL to any one)
pub const ZEROSLOT_STAKED_ACCOUNTS: &[&str] = &[
    "Eb2KpSC8uMt9GmzyAEm5Eb1AAAgTjRaXWFjKyFXHZxF3",
    "FCjUJZ1qozm1e8romw216qyfQMaaWKxWsuySnumVCCNe",
    "ENxTEjSQ1YabmUpXAdCgevnHQ9MHdLv8tzFiuiYJqa13",
    "6rYLG55Q9RpsPGvqdPNJs4z5WTxJVatMB8zV3WJhs5EK",
    "Cix2bHfqPcKcM233mzxbLk14kSggUUiz2A87fJtGivXr",
];

// Helius Sender tip accounts (mainnet-beta)
pub const TIP_ACCOUNTS: &[&str] = &[
    "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
    "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
    "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    "5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn",
    "2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD",
    "2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ",
    "wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF",
    "3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT",
    "4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey",
    "4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or",
];

/// Jito tip floor API endpoint
const JITO_TIP_FLOOR_API: &str = "https://bundles.jito.wtf/api/v1/bundles/tip_floor";

/// Fetch dynamic tip amount from Jito API (75th percentile)
/// Falls back to minimum based on routing mode if API fails or dynamic tips disabled
pub async fn get_dynamic_tip_amount(settings: &Settings) -> Result<f64, Box<dyn Error + Send + Sync>> {
    // Check if dynamic tips are enabled
    if !settings.helius_use_dynamic_tips {
        let static_tip = settings.get_effective_min_tip_sol();
        debug!("Dynamic tips disabled, using static tip: {:.9} SOL", static_tip);
        return Ok(static_tip);
    }
    
    // Only fetch dynamic tips for dual routing mode
    // SWQOS-only should use minimum to keep costs low
    if settings.helius_use_swqos_only {
        let min_tip = settings.get_effective_min_tip_sol();
        debug!("SWQOS-only mode: using minimum tip {:.9} SOL", min_tip);
        return Ok(min_tip);
    }
    
    // Fetch dynamic tip for dual routing
    match fetch_jito_tip_floor().await {
        Ok(tip_75th) => {
            // Use 75th percentile but enforce minimum based on routing mode
            let min_tip = settings.get_effective_min_tip_sol();
            let effective_tip = tip_75th.max(min_tip);
            
            if effective_tip > min_tip {
                info!("Dynamic tip from Jito API: {:.9} SOL (75th percentile)", tip_75th);
            } else {
                debug!("Jito 75th percentile {:.9} SOL below minimum, using {:.9} SOL", tip_75th, min_tip);
            }
            
            Ok(effective_tip)
        }
        Err(e) => {
            let fallback_tip = settings.get_effective_min_tip_sol();
            warn!("Failed to fetch Jito tip floor ({}), using fallback: {:.9} SOL", e, fallback_tip);
            Ok(fallback_tip)
        }
    }
}

/// Fetch tip floor data from Jito API
async fn fetch_jito_tip_floor() -> Result<f64, Box<dyn Error + Send + Sync>> {
    let response = shared_client()
        .get(JITO_TIP_FLOOR_API)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    let data: Value = response.json().await?;
    
    // Parse response: array with first element containing landed_tips_75th_percentile
    if let Some(array) = data.as_array() {
        if let Some(first) = array.first() {
            if let Some(tip_75th) = first.get("landed_tips_75th_percentile") {
                if let Some(tip_value) = tip_75th.as_f64() {
                    return Ok(tip_value);
                }
            }
        }
    }
    
    Err("Invalid response format from Jito API".into())
}

/// Get a random tip account from the list
pub fn get_random_tip_account() -> Result<Pubkey, Box<dyn Error + Send + Sync>> {
    use rand::seq::SliceRandom;
    let account_str = TIP_ACCOUNTS
        .choose(&mut rand::thread_rng())
        .ok_or("No tip accounts available")?;
    Ok(std::str::FromStr::from_str(account_str)?)
}

/// Fetch priority fee estimate from Helius Priority Fee API
pub async fn get_priority_fee_estimate(
    rpc_url: &str,
    transaction_base64: &str,
    settings: &Settings,
) -> Result<u64, Box<dyn Error + Send + Sync>> {
    let response = shared_client()
        .post(rpc_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "getPriorityFeeEstimate",
            "params": [{
                "transaction": transaction_base64,
                "options": { "recommended": true }
            }]
        }))
        .send()
        .await?;

    let json: Value = response.json().await?;
    
    if let Some(result) = json.get("result") {
        if let Some(fee) = result.get("priorityFeeEstimate") {
            if let Some(fee_f64) = fee.as_f64() {
                let adjusted_fee = (fee_f64 * settings.helius_priority_fee_multiplier).ceil() as u64;
                debug!("Priority fee estimate: {} (adjusted: {})", fee_f64, adjusted_fee);
                return Ok(adjusted_fee);
            }
        }
    }
    
    // Fallback to a safe default
    let fallback_fee = 50_000u64;
    debug!("Failed to get priority fee estimate, using fallback: {}", fallback_fee);
    Ok(fallback_fee)
}

/// Sanitize instructions to prevent PrivilegeEscalation errors.
///
/// The Solana SDK's `Message::new()` merges duplicate accounts across
/// instructions by taking the union of their privileges: if ANY instruction
/// marks an account as `is_signer=true`, the merged account in the message
/// gets `is_signer=true`. This causes `InstructionError(3, PrivilegeEscalation)`
/// when the account is not actually a signer at the transaction level.
///
/// This function strips `is_signer=true` from every `AccountMeta` whose
/// pubkey is NOT the payer, ensuring only the payer is ever marked as a signer.
fn sanitize_instructions(instructions: &[Instruction], payer_pubkey: &Pubkey) -> Vec<Instruction> {
    instructions
        .iter()
        .map(|ix| {
            let sanitized_accounts: Vec<AccountMeta> = ix
                .accounts
                .iter()
                .map(|am| {
                    if am.is_signer && am.pubkey != *payer_pubkey {
                        AccountMeta {
                            pubkey: am.pubkey,
                            is_signer: false,
                            is_writable: am.is_writable,
                        }
                    } else {
                        am.clone()
                    }
                })
                .collect();
            Instruction {
                program_id: ix.program_id,
                accounts: sanitized_accounts,
                data: ix.data.clone(),
            }
        })
        .collect()
}

/// Send a transaction via Helius Sender endpoint
/// 
/// This function:
/// 1. Fetches dynamic tip amount from Jito API (dual routing) or uses minimum (SWQOS)
/// 2. Simulates transaction to determine optimal compute units
/// 3. Fetches dynamic priority fees from Helius API
/// 4. Adds compute budget instructions (compute unit limit and price)
/// 5. Adds a tip instruction (SOL transfer to random tip account)
/// 6. Validates blockhash before sending
/// 7. Sends the transaction to Helius Sender with skipPreflight=true
/// 
/// Note: Dev fee should be added to instructions by the caller before calling this function
/// 
/// Returns the transaction signature on success
pub async fn send_transaction_via_helius(
    instructions: Vec<Instruction>,
    payer: &Keypair,
    settings: &Arc<Settings>,
    rpc_client: &RpcClient,
    cached_blockhash: Option<solana_sdk::hash::Hash>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    if !settings.helius_sender_enabled {
        return Err("Helius Sender is not enabled in settings".into());
    }

    let payer_pubkey = payer.pubkey();
    let skip_sim = settings.skip_simulation && !settings.dry_run;

    // Fetch dynamic tip amount (uses Jito API for dual routing, minimum for SWQOS)
    let tip_amount_sol = get_dynamic_tip_amount(settings).await?;
    let tip_lamports = (tip_amount_sol * 1_000_000_000.0) as u64;

    // Log routing mode and tip
    let routing_mode = if settings.helius_use_swqos_only {
        "SWQOS-only"
    } else {
        "dual routing (validators + Jito)"
    };
    info!("Using {} with tip: {:.9} SOL", routing_mode, tip_amount_sol);

    // Add tip instruction
    let tip_account = get_random_tip_account()?;
    let tip_instruction = system_instruction::transfer(
        &payer_pubkey,
        &tip_account,
        tip_lamports,
    );

    let (compute_units, priority_fee) = if skip_sim {
        debug!("send_transaction_via_helius: SKIP SIMULATION — using defaults");
        (250_000u32, 50_000u64)
    } else {
        // Build a test transaction to get compute units via simulation
        let mut all_test_instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        ];
        all_test_instructions.extend(instructions.clone());
        all_test_instructions.push(tip_instruction.clone());

        let test_message = Message::new(&all_test_instructions, Some(&payer_pubkey));
        let test_tx = VersionedTransaction::try_new(
            VersionedMessage::Legacy(test_message),
            &[payer],
        )?;

        let sim_result = rpc_client.simulate_transaction(&test_tx)?;
        let cu = if let Some(units) = sim_result.value.units_consumed {
            let units_with_margin = (units as f64 * 1.2).ceil() as u32;
            std::cmp::max(units_with_margin, 200_000)
        } else {
            debug!("Simulation did not return compute units, using default");
            200_000u32
        };

        let serialized_test_tx = bincode::serialize(&test_tx)?;
        let test_tx_base64 = Base64Engine.encode(&serialized_test_tx);
        let rpc_url = &settings.solana_rpc_urls[0];
        let pf = get_priority_fee_estimate(rpc_url, &test_tx_base64, settings).await?;
        (cu, pf)
    };

    debug!(
        "Building final transaction with compute_units={}, priority_fee={}, tip={} SOL{}",
        compute_units, priority_fee, settings.helius_min_tip_sol, if skip_sim { " [NO SIM]" } else { "" }
    );

    // Build final transaction with optimized compute budget
    let mut final_instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(compute_units),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
    ];
    final_instructions.extend(instructions);
    final_instructions.push(tip_instruction);

    // 0slot staked_conn: add mandatory transfer (≥0.001 SOL) to random staked account
    if settings.zeroslot_api_key.as_ref().map_or(false, |k| !k.is_empty()) {
        use rand::seq::SliceRandom;
        let staked_lamports = settings.zeroslot_staked_conn_lamports.max(1_000_000); // minimum 0.001 SOL
        if let Some(account_str) = ZEROSLOT_STAKED_ACCOUNTS.choose(&mut rand::thread_rng()) {
            if let Ok(recipient) = std::str::FromStr::from_str(account_str) {
                final_instructions.push(system_instruction::transfer(
                    &payer_pubkey,
                    &recipient,
                    staked_lamports,
                ));
                debug!("Added 0slot staked_conn: {} lamports to {}", staked_lamports, account_str);
            }
        }
    }
    
    // Sanitize: strip is_signer from non-payer accounts to prevent PrivilegeEscalation
    // (REAL-9). Solana's Message::new() merges duplicate accounts and propagates
    // is_signer=true upward, but only the payer actually signs the transaction.
    let final_instructions = sanitize_instructions(&final_instructions, &payer_pubkey);

    // Create final versioned transaction
    let message = Message::new(&final_instructions, Some(&payer_pubkey));
    let mut tx = VersionedTransaction::try_new(
        VersionedMessage::Legacy(message),
        &[payer],
    )?;
    
    // Use cached blockhash if available, otherwise fetch (adds ~200-500ms)
    let final_blockhash = match cached_blockhash {
        Some(bh) => {
            debug!("Using cached blockhash: {}", bh);
            bh
        }
        None => {
            debug!("No cached blockhash, fetching from RPC...");
            rpc_client.get_latest_blockhash()?
        }
    };
    if let VersionedMessage::Legacy(ref mut msg) = tx.message {
        msg.recent_blockhash = final_blockhash;
    }
    
    // Re-sign with updated blockhash
    let signature_bytes = payer.try_sign_message(tx.message.serialize().as_slice())?;
    tx.signatures[0] = signature_bytes;
    
    // Serialize transaction for sending
    let serialized_tx = bincode::serialize(&tx)?;
    let tx_base64 = Base64Engine.encode(&serialized_tx);
    
    // Build Helius Sender endpoint URL with routing mode and optional API key
    let mut endpoint = settings.helius_sender_endpoint.clone();
    let mut params = Vec::new();
    
    // Add SWQOS-only parameter if enabled
    if settings.helius_use_swqos_only {
        params.push("swqos_only=true".to_string());
    }
    
    // Add API key if provided
    if let Some(api_key) = &settings.helius_api_key {
        params.push(format!("api-key={}", api_key));
    }
    
    // Append parameters to endpoint
    if !params.is_empty() {
        let separator = if endpoint.contains('?') { "&" } else { "?" };
        endpoint = format!("{}{}{}", endpoint, separator, params.join("&"));
    }
    
    let routing_mode = if settings.helius_use_swqos_only {
        "SWQOS-only"
    } else {
        "dual routing (validators + Jito)"
    };
    info!("Sending transaction via Helius Sender ({}) to: {}", routing_mode, endpoint);
    
    // === PARALLEL SEND: fire to Jito/Helius + 0slot + Nozomi simultaneously ===
    let send_payload = json!({
        "jsonrpc": "2.0",
        "id": chrono::Utc::now().timestamp_millis().to_string(),
        "method": "sendTransaction",
        "params": [
            tx_base64,
            {
                "encoding": "base64",
                "skipPreflight": true,
                "maxRetries": 0
            }
        ]
    });

    // Spawn parallel senders (fire-and-forget)
    let mut parallel_endpoints: Vec<(String, String)> = Vec::new();

    // 0slot.trade endpoints (Frankfurt, HTTP not HTTPS, 5 TPS limit)
    if let Some(ref api_key) = settings.zeroslot_api_key {
        if !api_key.is_empty() {
            let endpoints = if settings.zeroslot_endpoints.is_empty() {
                vec!["http://de1.0slot.trade".to_string(), "http://de2.0slot.trade".to_string()]
            } else {
                settings.zeroslot_endpoints.clone()
            };
            for (i, ep) in endpoints.iter().enumerate() {
                // 0slot uses sendTransaction JSON-RPC with api-key header
                parallel_endpoints.push((
                    format!("0slot-de{}", i + 1),
                    format!("{}/api/v1/transactions", ep),
                ));
            }
        }
    }

    // Nozomi (temporal.xyz) — Frankfurt JSON-RPC endpoint
    if let Some(ref api_key) = settings.nozomi_api_key {
        if !api_key.is_empty() {
            parallel_endpoints.push((
                "Nozomi-fra".to_string(),
                format!("https://fra2.nozomi.temporal.xyz/?c={}", api_key),
            ));
        }
    }

    for (name, ep_url) in &parallel_endpoints {
        let name = name.to_string();
        let url = ep_url.clone();
        let payload = send_payload.clone();
        // 0slot requires api-key as header, not query param
        let is_zeroslot = name.starts_with("0slot");
        let zeroslot_key = if is_zeroslot {
            settings.zeroslot_api_key.clone().unwrap_or_default()
        } else {
            String::new()
        };
        tokio::spawn(async move {
            let mut req = shared_client().post(&url).json(&payload);
            if is_zeroslot && !zeroslot_key.is_empty() {
                req = req.header("x-api-key", &zeroslot_key);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        info!("TX sent via {} (parallel): OK", name);
                    } else {
                        warn!("TX via {} failed: {} {}", name, status, &body[..body.len().min(200)]);
                    }
                }
                Err(e) => warn!("TX via {} error: {}", name, e),
            }
        });
    }
    if !parallel_endpoints.is_empty() {
        info!("Parallel TX sent to: {}", parallel_endpoints.iter().map(|(n,_)| n.as_str()).collect::<Vec<_>>().join(", "));
    }

    // Primary send via Helius/Jito
    let response = shared_client()
        .post(&endpoint)
        .json(&send_payload)
        .send()
        .await?;
    
    let json: Value = response.json().await?;
    
    if let Some(error) = json.get("error") {
        return Err(format!("Helius Sender error: {}", error).into());
    }
    
    if let Some(result) = json.get("result") {
        if let Some(sig) = result.as_str() {
            info!("Transaction sent via Helius Sender: {} (+ parallel to 0slot/Nozomi)", sig);
            return Ok(sig.to_string());
        }
    }
    
    Err("Invalid response from Helius Sender".into())
}

/// Build a fully signed transaction (with compute budget, priority fee, tip) and return
/// it as a base64-encoded string. Does NOT send it — caller is responsible for submission.
/// This is the "build" half of send_transaction_via_helius, factored out for SWQoS
/// concurrent sending where the same serialized TX goes to multiple endpoints.
pub async fn build_signed_transaction(
    instructions: Vec<Instruction>,
    payer: &Keypair,
    settings: &Arc<Settings>,
    rpc_client: &RpcClient,
    cached_blockhash: Option<solana_sdk::hash::Hash>,
    cached_leader: Option<crate::leader_cache::CachedLeader>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let payer_pubkey = payer.pubkey();
    let skip_sim = settings.skip_simulation && !settings.dry_run;

    // Fetch dynamic tip
    let tip_amount_sol = get_dynamic_tip_amount(settings).await?;
    let tip_lamports = (tip_amount_sol * 1_000_000_000.0) as u64;

    let tip_account = get_random_tip_account()?;
    let tip_instruction = system_instruction::transfer(&payer_pubkey, &tip_account, tip_lamports);

    let (compute_units, priority_fee) = if skip_sim {
        // Skip simulation — use safe defaults (saves ~200ms)
        debug!("build_signed_transaction: SKIP SIMULATION — using defaults cu=250000, priority_fee=50000");
        (250_000u32, 50_000u64)
    } else {
        // Build test transaction for simulation
        let mut all_test_instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        ];
        all_test_instructions.extend(instructions.clone());
        all_test_instructions.push(tip_instruction.clone());

        let test_message = Message::new(&all_test_instructions, Some(&payer_pubkey));
        let test_tx = VersionedTransaction::try_new(VersionedMessage::Legacy(test_message), &[payer])?;

        // Simulate for compute units
        let sim_result = rpc_client.simulate_transaction(&test_tx)?;
        let cu = if let Some(units) = sim_result.value.units_consumed {
            std::cmp::max((units as f64 * 1.2).ceil() as u32, 200_000)
        } else {
            200_000u32
        };

        // Priority fee
        let serialized_test_tx = bincode::serialize(&test_tx)?;
        let test_tx_base64 = Base64Engine.encode(&serialized_test_tx);
        let rpc_url = &settings.solana_rpc_urls[0];
        let pf = get_priority_fee_estimate(rpc_url, &test_tx_base64, settings).await?;
        (cu, pf)
    };

    // Agave #13267 own-leader boost: if the configured Helius/Jito endpoint maps to the
    // current slot leader, optionally raise tip and priority fee.
    let is_own_leader = cached_leader
        .as_ref()
        .and_then(|cl| cl.leader)
        .and_then(|leader| {
            settings
                .leader_mapping
                .helius_sender_endpoint_owner
                .as_ref()
                .and_then(|s| Pubkey::from_str(s).ok())
                .map(|mapped| mapped == leader)
        })
        .unwrap_or(false);
    let (effective_tip_sol, effective_priority_fee) =
        crate::leader_cache::apply_own_leader_multipliers(settings, tip_amount_sol, priority_fee, is_own_leader);
    let effective_tip_lamports = (effective_tip_sol * 1_000_000_000.0) as u64;

    if is_own_leader {
        info!(
            "LEADER_OWN_TIP_BOOST: tip {:.9} -> {:.9} SOL, priority {} -> {} micro-lamports/CU",
            tip_amount_sol, effective_tip_sol, priority_fee, effective_priority_fee
        );
    }

    debug!("build_signed_transaction: cu={}, priority_fee={}, tip={:.9} SOL{}",
        compute_units, effective_priority_fee, effective_tip_sol, if skip_sim { " [NO SIM]" } else { "" });

    // Build final instructions with (possibly boosted) compute budget and tip.
    let mut final_instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(compute_units),
        ComputeBudgetInstruction::set_compute_unit_price(effective_priority_fee),
    ];
    let effective_tip_instruction = system_instruction::transfer(
        &payer_pubkey, &tip_account, effective_tip_lamports,
    );
    final_instructions.extend(instructions);
    final_instructions.push(effective_tip_instruction);

    // 0slot staked_conn: add mandatory transfer (≥0.001 SOL) to random staked account
    if settings.zeroslot_api_key.as_ref().map_or(false, |k| !k.is_empty()) {
        use rand::seq::SliceRandom;
        let staked_lamports = settings.zeroslot_staked_conn_lamports.max(1_000_000);
        if let Some(account_str) = ZEROSLOT_STAKED_ACCOUNTS.choose(&mut rand::thread_rng()) {
            if let Ok(recipient) = std::str::FromStr::from_str(account_str) {
                final_instructions.push(system_instruction::transfer(
                    &payer_pubkey,
                    &recipient,
                    staked_lamports,
                ));
            }
        }
    }

    // Sanitize: strip is_signer from non-payer accounts to prevent PrivilegeEscalation
    // (REAL-9). Solana's Message::new() merges duplicate accounts and propagates
    // is_signer=true upward, but only the payer actually signs the transaction.
    let final_instructions = sanitize_instructions(&final_instructions, &payer_pubkey);

    // Create and sign
    let message = Message::new(&final_instructions, Some(&payer_pubkey));
    let mut tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer])?;

    let final_blockhash = match cached_blockhash {
        Some(bh) => bh,
        None => rpc_client.get_latest_blockhash()?,
    };
    if let VersionedMessage::Legacy(ref mut msg) = tx.message {
        msg.recent_blockhash = final_blockhash;
    }
    let signature_bytes = payer.try_sign_message(tx.message.serialize().as_slice())?;
    tx.signatures[0] = signature_bytes;

    let serialized_tx = bincode::serialize(&tx)?;
    Ok(Base64Engine.encode(&serialized_tx))
}

/// Simulate a base64-encoded serialized transaction via standard RPC endpoint (not Sender).
/// Uses JSON-RPC method `simulateTransaction` against `settings.solana_rpc_urls[0]`.
pub async fn simulate_transaction_via_helius(
    tx_base64: &str,
    settings: &Settings,
) -> Result<Value, Box<dyn Error + Send + Sync>> {
    // Use standard RPC for simulation, as Sender endpoint usually doesn't support simulateTransaction
    let endpoint = if !settings.solana_rpc_urls.is_empty() {
        &settings.solana_rpc_urls[0]
    } else {
        return Err("No standard RPC URL configured for simulation".into());
    };
    
    let payload = json!({
        "jsonrpc": "2.0",
        "id": chrono::Utc::now().timestamp_millis().to_string(),
        "method": "simulateTransaction",
        "params": [ tx_base64, { "encoding": "base64", "sigVerify": false, "commitment": "confirmed" } ]
    });

    let resp = shared_client().post(endpoint).json(&payload).send().await?;
    let json: Value = resp.json().await?;
    if let Some(err) = json.get("error") {
        return Err(format!("Helius simulate error: {}", err).into());
    }
    Ok(json)
}

/// Check if blockhash is still valid
async fn is_blockhash_valid(
    rpc_client: &RpcClient,
    last_valid_block_height: u64,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let current_height = rpc_client.get_block_height()?;
    Ok(current_height <= last_valid_block_height)
}



/// Retry logic for sending transactions via Helius Sender with blockhash validation
/// Attempts up to max_retries times with exponential backoff
pub async fn send_transaction_with_retry(
    instructions: Vec<Instruction>,
    payer: &Keypair,
    settings: &Arc<Settings>,
    rpc_client: &RpcClient,
    max_retries: usize,
    cached_blockhash: Option<solana_sdk::hash::Hash>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let mut last_error: Option<Box<dyn Error + Send + Sync>> = None;

    // Get blockhash info for validity checking.
    // If we have a cached blockhash, use it and fetch only the block height for validity checks.
    // Otherwise fetch both from RPC.
    let last_valid_block_height = if cached_blockhash.is_some() {
        // Cache refreshes every 400ms so the hash is always fresh.
        // Fetch current block height for validity checking (cheap RPC call).
        match rpc_client.get_block_height() {
            Ok(h) => h + 150, // ~1 minute validity window
            Err(_) => u64::MAX, // skip validity check on error
        }
    } else {
        let blockhash_info = rpc_client.get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?;
        blockhash_info.1
    };
    
    debug!("Starting transaction send with blockhash valid until block height: {}", last_valid_block_height);
    
    for attempt in 0..max_retries {
        // Check if blockhash is still valid before attempting
        match is_blockhash_valid(rpc_client, last_valid_block_height).await {
            Ok(true) => {
                debug!("Blockhash still valid, attempting send (attempt {}/{})", attempt + 1, max_retries);
            }
            Ok(false) => {
                let err = "Blockhash expired before send attempt";
                warn!("{}", err);
                return Err(err.into());
            }
            Err(e) => {
                warn!("Failed to check blockhash validity: {}", e);
                // Continue anyway, let the RPC reject if expired
            }
        }
        
        match send_transaction_via_helius(instructions.clone(), payer, settings, rpc_client, cached_blockhash).await {
            Ok(sig) => {
                info!("Transaction sent successfully: {}", sig);
                
                // Optional: Wait for confirmation with timeout
                // Uncomment to enable confirmation checking
                // match confirm_transaction(&sig, rpc_client, 15).await {
                //     Ok(_) => return Ok(sig),
                //     Err(e) => {
                //         warn!("Transaction sent but confirmation failed: {}", e);
                //         return Ok(sig); // Return signature anyway
                //     }
                // }
                
                return Ok(sig);
            }
            Err(e) => {
                warn!("Helius Sender attempt {}/{} failed: {}", attempt + 1, max_retries, e);
                last_error = Some(e);
                
                if attempt < max_retries - 1 {
                    let delay_ms = 1000 * (2_u64.pow(attempt as u32));
                    debug!("Retrying in {} ms...", delay_ms);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }
    
    Err(last_error.unwrap_or_else(|| "All retry attempts failed".into()))
}

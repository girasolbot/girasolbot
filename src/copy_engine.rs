use anyhow::{Context, Result};
use log::{info, warn, debug, error};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::settings::Settings;

/// Retry delays for 429 errors (exponential backoff)
const RETRY_DELAYS_MS: [u64; 3] = [200, 500, 1000];

/// Max concurrent in-flight getTransaction calls.
///
/// P1 fix: replaced the previous serial 100ms gate (MIN_RPC_INTERVAL) that
/// blocked the entire detection loop. A 100ms global interval caps detection
/// at 10 trades/sec and forces every later message in a burst to wait
/// 100ms × queue_position behind the previous one — adding 100ms..N×100ms
/// to detection latency. On a high-frequency target wallet this is the
/// difference between sniping the same block and missing the trade entirely.
///
/// 8 concurrent calls leaves comfortable headroom under ERPC's 50 req/s
/// limit while letting bursts run in parallel. 429 errors are still
/// handled by `fetch_and_parse_with_retry` (exponential backoff).
const MAX_CONCURRENT_RPC: usize = 8;

/// Known DEX program IDs for detecting which exchange the target used
const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMP_FUN_AMM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const RAYDIUM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const RAYDIUM_CPMM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";
const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";

/// WSOL mint — used to identify SOL side of Raydium swaps
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Buy discriminator for pump.fun (from IDL)
const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

/// Sell discriminator for pump.fun (from IDL)
const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Detected buy action from target wallet
#[derive(Debug, Clone)]
pub struct DetectedBuy {
    /// Transaction signature
    pub signature: String,
    /// Token mint address
    pub mint: String,
    /// Which DEX was used
    pub dex: DexType,
    /// SOL amount spent (if parseable)
    pub sol_amount: Option<f64>,
    /// Token amount received by target (base units, from instruction data or balance delta)
    pub token_amount: Option<u64>,
    /// Slot number
    pub slot: Option<u64>,
    /// When the WS notification was received (for end-to-end timing)
    pub ws_received_at: Instant,
    /// AMM pool address (for Raydium V4/CPMM trades)
    pub amm_pool: Option<String>,
    /// Full account list from target's pump instruction (for copying account [16] etc)
    pub target_pump_accounts: Option<Vec<String>>,
}

/// Detected sell action from target wallet
#[derive(Debug, Clone)]
pub struct DetectedSell {
    /// Transaction signature
    pub signature: String,
    /// Token mint address
    pub mint: String,
    /// Which DEX was used
    pub dex: DexType,
    /// SOL amount received (if parseable, from balance delta)
    pub sol_received: Option<f64>,
    /// Token amount sold (from instruction data, in base units)
    pub token_amount: Option<u64>,
    /// Slot number
    pub slot: Option<u64>,
    /// When the WS notification was received (for end-to-end timing)
    pub ws_received_at: Instant,
    /// AMM pool address (for Raydium V4/CPMM trades)
    pub amm_pool: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DexType {
    PumpFun,
    PumpFunAmm,
    RaydiumV4,
    RaydiumCpmm,
    Jupiter,
    Unknown(String),
}

impl DexType {
    /// Returns true if execution (buy/sell) is supported for this DEX
    pub fn is_execution_supported(&self) -> bool {
        matches!(self, DexType::PumpFun | DexType::PumpFunAmm | DexType::RaydiumV4 | DexType::RaydiumCpmm)
    }
}

impl std::fmt::Display for DexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DexType::PumpFun => write!(f, "Pump.fun"),
            DexType::PumpFunAmm => write!(f, "Pump.fun AMM"),
            DexType::RaydiumV4 => write!(f, "Raydium V4"),
            DexType::RaydiumCpmm => write!(f, "Raydium CPMM"),
            DexType::Jupiter => write!(f, "Jupiter"),
            DexType::Unknown(id) => write!(f, "Unknown({})", &id[..id.len().min(8)]),
        }
    }
}

/// Parsed data from a logsSubscribe notification
struct LogsNotification {
    signature: String,
    slot: Option<u64>,
    err: Option<serde_json::Value>,
    logs: Vec<String>,
}

/// Parsed buy data extracted from a full transaction
struct ParsedBuy {
    mint: String,
    sol_amount: Option<f64>,
    /// Token amount received by target (base units)
    token_amount: Option<u64>,
    amm_pool: Option<String>,
    /// Full pump instruction accounts (for copying account [16] etc)
    target_pump_accounts: Option<Vec<String>>,
}

/// Parsed sell data extracted from a full transaction
struct ParsedSell {
    mint: String,
    token_amount: Option<u64>,
    sol_received: Option<f64>,
    amm_pool: Option<String>,
}

/// Combined parse result from a transaction (may contain buy, sell, or both)
struct ParsedTransaction {
    buy: Option<ParsedBuy>,
    sell: Option<ParsedSell>,
}

/// Run the copy engine — listens for logsSubscribe notifications about the target wallet,
/// fetches full transaction data via RPC, and parses pump.fun buy instructions to detect
/// the mint address reliably.
pub async fn run_copy_engine(
    target_wallet: &str,
    mut rx: mpsc::Receiver<String>,
    buy_tx: mpsc::Sender<DetectedBuy>,
    sell_detect_tx: Option<mpsc::Sender<DetectedSell>>,
    settings: Arc<Settings>,
) -> Result<()> {
    info!("Copy engine started — watching wallet {}", target_wallet);

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to create HTTP client")?;

    // Concurrency limiter for in-flight RPC fetches. See MAX_CONCURRENT_RPC.
    let rpc_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_RPC));
    let target_wallet_owned: Arc<str> = Arc::from(target_wallet);

    while let Some(message) = rx.recv().await {
        let ws_received_at = Instant::now();

        // Step 1: Parse the logsSubscribe notification to get signature + logs
        let notif = match parse_logs_notification(&message) {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(e) => {
                warn!("Failed to parse notification: {}", e);
                continue;
            }
        };

        let sig_short_len = notif.signature.len().min(16);

        // Skip errored transactions
        if notif.err.is_some() {
            debug!("Skipping errored tx: {}...", &notif.signature[..sig_short_len]);
            continue;
        }

        // Step 2: Quick filter — detect DEX from log messages
        let dex = detect_dex(&notif.logs);

        // Check if this looks like a buy or sell from logs
        let is_potential_trade = match &dex {
            DexType::PumpFun => notif.logs.iter().any(|l|
                l.contains("Instruction: Buy") || l.contains("Instruction: buy")
                || l.contains("Instruction: Sell") || l.contains("Instruction: sell")
            ),
            DexType::PumpFunAmm => true,
            DexType::RaydiumV4 | DexType::RaydiumCpmm => true, // swaps detected from program invocation
            DexType::Jupiter => true,
            DexType::Unknown(_) => false,
        };

        if !is_potential_trade {
            debug!("Non-trade tx from target: {}...", &notif.signature[..sig_short_len]);
            continue;
        }

        // Jupiter not yet supported
        if dex == DexType::Jupiter {
            debug!("Detected Jupiter tx {}... — not yet supported, skipping", &notif.signature[..sig_short_len]);
            continue;
        }

        info!("Potential {} trade detected, fetching full tx: {}...", dex, &notif.signature[..sig_short_len]);

        let rpc_url = settings.solana_rpc_urls.first()
            .cloned()
            .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());

        let parse_ms = ws_received_at.elapsed().as_millis();

        // P1 fix: spawn the fetch+dispatch into a task so the detection loop
        // never blocks on RPC latency. The Semaphore bounds in-flight calls
        // to MAX_CONCURRENT_RPC instead of serialising them behind a
        // global 100ms gate. A burst of N target trades now runs N fetches
        // in parallel (up to the cap), each one paying its own ~30-80ms
        // RPC cost rather than 30-80ms × queue_position.
        let http_client = http_client.clone();
        let buy_tx = buy_tx.clone();
        let sell_detect_tx = sell_detect_tx.clone();
        let dex = dex.clone();
        let notif = notif;
        let semaphore = rpc_semaphore.clone();
        let target_wallet_task = target_wallet_owned.clone();

        tokio::spawn(async move {
            // Cap concurrent RPC calls. If we're at the cap the task waits
            // here instead of in the receive loop, so detection of later
            // notifications keeps progressing.
            let _permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return, // semaphore closed (shutdown)
            };

            let fetch_start = Instant::now();
            let result = fetch_and_parse_with_retry(
                &http_client, &rpc_url, &notif.signature, &target_wallet_task,
            ).await;
            let fetch_ms = fetch_start.elapsed().as_millis();

            let sig_short_len = notif.signature.len().min(16);

            match result {
                Ok(parsed) => {
                    let found_buy = parsed.buy.is_some();
                    let found_sell = parsed.sell.is_some();
                    let total_ms = ws_received_at.elapsed().as_millis();

                    // Handle buy detection
                    if let Some(buy) = parsed.buy {
                        let detected = DetectedBuy {
                            signature: notif.signature.clone(),
                            mint: buy.mint,
                            dex: dex.clone(),
                            sol_amount: buy.sol_amount,
                            token_amount: buy.token_amount,
                            slot: notif.slot,
                            ws_received_at,
                            amm_pool: buy.amm_pool,
                            target_pump_accounts: buy.target_pump_accounts,
                        };
                        info!(
                            "TARGET BUY DETECTED: {} on {} | mint: {} | sig: {}...",
                            detected.sol_amount.map(|a| format!("{:.4} SOL", a)).unwrap_or_else(|| "? SOL".to_string()),
                            detected.dex,
                            detected.mint,
                            &detected.signature[..sig_short_len],
                        );
                        info!("TIMING detect: {}ms (parse {}ms + fetch {}ms)", total_ms, parse_ms, fetch_ms);
                        if let Err(e) = buy_tx.send(detected).await {
                            error!("Failed to send buy signal: {}", e);
                        }
                    }

                    // Handle sell detection
                    if let Some(sell) = parsed.sell {
                        if let Some(ref tx) = sell_detect_tx {
                            let detected = DetectedSell {
                                signature: notif.signature.clone(),
                                mint: sell.mint,
                                dex: dex.clone(),
                                sol_received: sell.sol_received,
                                token_amount: sell.token_amount,
                                slot: notif.slot,
                                ws_received_at,
                                amm_pool: sell.amm_pool,
                            };
                            info!(
                                "TARGET SELL DETECTED: {} | {} tokens | sig: {}...",
                                detected.mint,
                                detected.token_amount.map(|a| a.to_string()).unwrap_or_else(|| "?".to_string()),
                                &detected.signature[..sig_short_len],
                            );
                            info!("TIMING detect: {}ms (parse {}ms + fetch {}ms)", total_ms, parse_ms, fetch_ms);
                            if let Err(e) = tx.send(detected).await {
                                error!("Failed to send sell signal: {}", e);
                            }
                        }
                    }

                    if !found_buy && !found_sell {
                        debug!("No buy/sell instruction found in {} tx {}...", dex, &notif.signature[..sig_short_len]);
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch/parse tx {}...: {}", &notif.signature[..sig_short_len], e);
                }
            }
        });
    }

    warn!("Copy engine channel closed");
    Ok(())
}

/// Parse a logsSubscribe notification message into structured data
fn parse_logs_notification(message: &str) -> Result<Option<LogsNotification>> {
    let value: serde_json::Value = serde_json::from_str(message)
        .context("Invalid JSON in WebSocket message")?;

    // logsSubscribe notification format:
    // { "params": { "result": { "context": { "slot": N }, "value": { "signature": "...", "err": null, "logs": [...] } } } }
    let notif_value = value
        .get("params")
        .and_then(|p| p.get("result"))
        .and_then(|r| r.get("value"));

    let notif_value = match notif_value {
        Some(v) => v,
        None => return Ok(None),
    };

    let signature = notif_value
        .get("signature")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    if signature.is_empty() {
        return Ok(None);
    }

    // err is null for successful txs, non-null for failed
    let err = notif_value.get("err").cloned();
    let err = if err.as_ref().map_or(true, |v| v.is_null()) { None } else { err };

    let slot = value
        .get("params")
        .and_then(|p| p.get("result"))
        .and_then(|r| r.get("context"))
        .and_then(|c| c.get("slot"))
        .and_then(|s| s.as_u64());

    let logs = notif_value
        .get("logs")
        .and_then(|l| l.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    Ok(Some(LogsNotification { signature, slot, err, logs }))
}

/// Detect which DEX is being used from log messages
fn detect_dex(logs: &[String]) -> DexType {
    for log in logs {
        if log.contains(PUMP_FUN_PROGRAM) {
            if log.contains("Instruction: Buy") || log.contains("Instruction: buy") {
                return DexType::PumpFun;
            }
        }
        if log.contains(PUMP_FUN_AMM) {
            return DexType::PumpFunAmm;
        }
        if log.contains(RAYDIUM_V4) {
            return DexType::RaydiumV4;
        }
        if log.contains(RAYDIUM_CPMM) {
            return DexType::RaydiumCpmm;
        }
        if log.contains(JUPITER_V6) {
            return DexType::Jupiter;
        }
    }

    // Second pass: check for program invocation even without "buy" keyword
    for log in logs {
        if log.contains(PUMP_FUN_PROGRAM) {
            return DexType::PumpFun;
        }
    }

    DexType::Unknown("none".to_string())
}

/// Fetch and parse with retry on 429 errors (up to 3 retries with exponential backoff).
async fn fetch_and_parse_with_retry(
    http_client: &reqwest::Client,
    rpc_url: &str,
    signature: &str,
    target_wallet: &str,
) -> Result<ParsedTransaction> {
    let mut last_err = None;
    for attempt in 0..=RETRY_DELAYS_MS.len() {
        match fetch_and_parse_transaction(http_client, rpc_url, signature, target_wallet).await {
            Ok(parsed) => return Ok(parsed),
            Err(e) => {
                let err_str = format!("{}", e);
                let is_429 = err_str.contains("429") || err_str.contains("Too many requests")
                    || err_str.contains("too many requests") || err_str.contains("rate limit");
                if is_429 && attempt < RETRY_DELAYS_MS.len() {
                    let delay = Duration::from_millis(RETRY_DELAYS_MS[attempt]);
                    warn!(
                        "RPC 429 for tx {}... — retry {}/{} in {}ms",
                        &signature[..signature.len().min(16)],
                        attempt + 1,
                        RETRY_DELAYS_MS.len(),
                        delay.as_millis(),
                    );
                    tokio::time::sleep(delay).await;
                    last_err = Some(e);
                } else {
                    return Err(e);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Retry exhausted")))
}

/// Fetch a transaction via RPC getTransaction and parse pump.fun buy/sell instructions.
async fn fetch_and_parse_transaction(
    http_client: &reqwest::Client,
    rpc_url: &str,
    signature: &str,
    target_wallet: &str,
) -> Result<ParsedTransaction> {
    // "confirmed" commitment required — public RPCs reject "processed" for getTransaction.
    let response = http_client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTransaction",
            "params": [
                signature,
                {
                    "encoding": "json",
                    "maxSupportedTransactionVersion": 0,
                    "commitment": "confirmed"
                }
            ]
        }))
        .send()
        .await
        .context("getTransaction request failed")?;

    // Check HTTP-level 429 before parsing body
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(anyhow::anyhow!("429 Too many requests"));
    }

    let json: serde_json::Value = response.json().await
        .context("Failed to parse getTransaction response")?;

    if let Some(err) = json.get("error") {
        return Err(anyhow::anyhow!("RPC error: {}", err));
    }

    let result = match json.get("result") {
        Some(r) if !r.is_null() => r,
        _ => return Err(anyhow::anyhow!("Transaction not found (null result)")),
    };

    // Extract all account keys (including address table lookups for versioned txs)
    let account_keys = extract_account_keys(result)?;

    // Compute actual SOL change for the target wallet
    let sol_delta = compute_sol_delta(result, &account_keys, target_wallet);

    let mut found_buy: Option<ParsedBuy> = None;
    let mut found_sell: Option<ParsedSell> = None;

    // Collect all instructions to scan (top-level + inner)
    let mut all_instructions: Vec<&serde_json::Value> = Vec::new();

    if let Some(instructions) = result
        .get("transaction")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.get("instructions"))
        .and_then(|i| i.as_array())
    {
        all_instructions.extend(instructions.iter());
    }

    if let Some(inner_groups) = result
        .get("meta")
        .and_then(|m| m.get("innerInstructions"))
        .and_then(|i| i.as_array())
    {
        for group in inner_groups {
            if let Some(instrs) = group.get("instructions").and_then(|i| i.as_array()) {
                all_instructions.extend(instrs.iter());
            }
        }
    }

    // Try pump.fun parsing first
    if let Some(pump_idx) = account_keys.iter().position(|k| k == PUMP_FUN_PROGRAM) {
        for instr in &all_instructions {
            if found_buy.is_none() {
                if let Some(mut parsed) = try_parse_pump_buy_instruction(instr, pump_idx, &account_keys)? {
                    if let Some((_, spent)) = sol_delta {
                        if spent > 0.0 {
                            parsed.sol_amount = Some(spent);
                        }
                    }
                    found_buy = Some(parsed);
                }
            }
            if found_sell.is_none() {
                if let Some(mut parsed) = try_parse_pump_sell_instruction(instr, pump_idx, &account_keys)? {
                    if let Some((received, _)) = sol_delta {
                        if received > 0.0 {
                            parsed.sol_received = Some(received);
                        }
                    }
                    found_sell = Some(parsed);
                }
            }
            if found_buy.is_some() && found_sell.is_some() {
                break;
            }
        }
    }

    // Try Raydium V4 / CPMM if pump.fun didn't match
    if found_buy.is_none() && found_sell.is_none() {
        let raydium_program = if account_keys.iter().any(|k| k == RAYDIUM_V4) {
            Some(RAYDIUM_V4)
        } else if account_keys.iter().any(|k| k == RAYDIUM_CPMM) {
            Some(RAYDIUM_CPMM)
        } else {
            None
        };

        if let Some(program_id) = raydium_program {
            // Extract AMM pool address from the Raydium instruction (account index 1)
            let amm_pool = extract_raydium_amm_pool(&all_instructions, &account_keys, program_id);
            if let Some(parsed) = try_parse_raydium_swap(result, &account_keys, target_wallet, sol_delta, amm_pool.as_deref()) {
                match parsed {
                    RaydiumSwapResult::Buy(b) => found_buy = Some(b),
                    RaydiumSwapResult::Sell(s) => found_sell = Some(s),
                }
            }
        }
    }

    Ok(ParsedTransaction { buy: found_buy, sell: found_sell })
}

/// Extract all account keys from a transaction result, including loaded addresses
/// from address lookup tables (versioned transactions).
fn extract_account_keys(result: &serde_json::Value) -> Result<Vec<String>> {
    let mut keys = Vec::new();

    // Standard accountKeys array
    if let Some(arr) = result
        .get("transaction")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.get("accountKeys"))
        .and_then(|k| k.as_array())
    {
        for key in arr {
            if let Some(s) = key.as_str() {
                keys.push(s.to_string());
            } else if let Some(pk) = key.get("pubkey").and_then(|p| p.as_str()) {
                keys.push(pk.to_string());
            }
        }
    }

    // Address lookup table loaded addresses (versioned transactions v0)
    if let Some(loaded) = result
        .get("meta")
        .and_then(|m| m.get("loadedAddresses"))
    {
        for field in &["writable", "readonly"] {
            if let Some(arr) = loaded.get(field).and_then(|a| a.as_array()) {
                for addr in arr {
                    if let Some(s) = addr.as_str() {
                        keys.push(s.to_string());
                    }
                }
            }
        }
    }

    if keys.is_empty() {
        return Err(anyhow::anyhow!("No account keys found in transaction"));
    }

    Ok(keys)
}

/// Compute SOL balance delta for the target wallet.
/// Returns (received, spent) — received is positive when post > pre (sell),
/// spent is positive when pre > post (buy).
fn compute_sol_delta(
    result: &serde_json::Value,
    account_keys: &[String],
    target_wallet: &str,
) -> Option<(f64, f64)> {
    let target_idx = account_keys.iter().position(|k| k == target_wallet)?;

    let pre_balances = result
        .get("meta")
        .and_then(|m| m.get("preBalances"))
        .and_then(|b| b.as_array())?;
    let post_balances = result
        .get("meta")
        .and_then(|m| m.get("postBalances"))
        .and_then(|b| b.as_array())?;

    let pre = pre_balances.get(target_idx)?.as_u64()?;
    let post = post_balances.get(target_idx)?.as_u64()?;

    if pre > post {
        // Target spent SOL (buy)
        Some((0.0, (pre - post) as f64 / 1_000_000_000.0))
    } else {
        // Target received SOL (sell)
        Some(((post - pre) as f64 / 1_000_000_000.0, 0.0))
    }
}

/// Try to parse a single instruction as a pump.fun buy.
/// Returns the mint and max_sol_cost if the instruction matches the buy discriminator.
///
/// Pump.fun buy instruction layout:
///   - data[0..8]: BUY_DISCRIMINATOR
///   - data[8..16]: token amount (u64 LE)
///   - data[16..24]: max_sol_cost in lamports (u64 LE)
///
/// Account indices in the instruction:
///   [0] = global config
///   [1] = fee recipient
///   [2] = mint
///   [3] = bonding curve
///   [4] = bonding curve ATA
///   [5] = user ATA
///   [6] = user (signer)
///   ...
fn try_parse_pump_buy_instruction(
    instr: &serde_json::Value,
    pump_program_idx: usize,
    account_keys: &[String],
) -> Result<Option<ParsedBuy>> {
    let program_id_index = instr
        .get("programIdIndex")
        .and_then(|p| p.as_u64())
        .unwrap_or(u64::MAX) as usize;

    if program_id_index != pump_program_idx {
        return Ok(None);
    }

    // Decode instruction data (base58 encoded in "json" encoding mode)
    let data_str = match instr.get("data").and_then(|d| d.as_str()) {
        Some(d) => d,
        None => return Ok(None),
    };

    let data_bytes = match bs58::decode(data_str).into_vec() {
        Ok(bytes) => bytes,
        Err(e) => {
            debug!("Failed to decode instruction data: {}", e);
            return Ok(None);
        }
    };

    // Need at least 8 bytes for discriminator
    if data_bytes.len() < 8 {
        return Ok(None);
    }

    // Check if discriminator matches BUY
    if data_bytes[..8] != BUY_DISCRIMINATOR {
        return Ok(None);
    }

    // Get the accounts array for this instruction
    let accounts: Vec<usize> = instr
        .get("accounts")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|i| i as usize)).collect())
        .unwrap_or_default();

    // Account index 2 in the instruction's accounts list is the mint
    if accounts.len() < 3 {
        warn!("Pump.fun buy instruction has fewer than 3 accounts (got {})", accounts.len());
        return Ok(None);
    }

    let mint_key_idx = accounts[2];
    if mint_key_idx >= account_keys.len() {
        warn!("Mint account index {} out of bounds (keys len {})", mint_key_idx, account_keys.len());
        return Ok(None);
    }

    let mint = account_keys[mint_key_idx].clone();

    // Validate the mint looks like a valid pubkey
    if Pubkey::from_str(&mint).is_err() {
        warn!("Extracted mint is not a valid pubkey: {}", mint);
        return Ok(None);
    }

    // Extract max_sol_cost from instruction data as fallback SOL amount
    // data[8..16] = token amount, data[16..24] = max_sol_cost (lamports)
    let sol_amount = if data_bytes.len() >= 24 {
        let max_sol_cost = u64::from_le_bytes(
            data_bytes[16..24].try_into().unwrap_or([0; 8])
        );
        Some(max_sol_cost as f64 / 1_000_000_000.0)
    } else {
        None
    };

    // data[8..16] = token amount (lamports)
    let token_amount = if data_bytes.len() >= 16 {
        Some(u64::from_le_bytes(data_bytes[8..16].try_into().unwrap_or([0; 8])))
    } else {
        None
    };

    // Collect full pump instruction accounts for downstream use (e.g. account [16] for sell)
    let target_pump_accounts: Vec<String> = accounts.iter()
        .filter_map(|&idx| account_keys.get(idx))
        .cloned()
        .collect();
    let target_pump_accounts = if target_pump_accounts.len() > 16 { Some(target_pump_accounts) } else { None };

    info!("Parsed pump.fun buy: mint={} max_sol_cost={} tokens={}{}",
        mint,
        sol_amount.map(|a| format!("{:.4} SOL", a)).unwrap_or_else(|| "?".to_string()),
        token_amount.map(|t| t.to_string()).unwrap_or_else(|| "?".to_string()),
        target_pump_accounts.as_ref().map(|a| format!(" {} accounts", a.len())).unwrap_or_default());

    Ok(Some(ParsedBuy { mint, sol_amount, token_amount, amm_pool: None, target_pump_accounts }))
}

/// Try to parse a single instruction as a pump.fun sell.
///
/// Pump.fun sell instruction layout:
///   - data[0..8]: SELL_DISCRIMINATOR
///   - data[8..16]: token amount (u64 LE)
///   - data[16..24]: min_sol_output in lamports (u64 LE)
///
/// Account layout same as buy: [2] = mint
fn try_parse_pump_sell_instruction(
    instr: &serde_json::Value,
    pump_program_idx: usize,
    account_keys: &[String],
) -> Result<Option<ParsedSell>> {
    let program_id_index = instr
        .get("programIdIndex")
        .and_then(|p| p.as_u64())
        .unwrap_or(u64::MAX) as usize;

    if program_id_index != pump_program_idx {
        return Ok(None);
    }

    let data_str = match instr.get("data").and_then(|d| d.as_str()) {
        Some(d) => d,
        None => return Ok(None),
    };

    let data_bytes = match bs58::decode(data_str).into_vec() {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };

    if data_bytes.len() < 8 || data_bytes[..8] != SELL_DISCRIMINATOR {
        return Ok(None);
    }

    let accounts: Vec<usize> = instr
        .get("accounts")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|i| i as usize)).collect())
        .unwrap_or_default();

    if accounts.len() < 3 {
        return Ok(None);
    }

    let mint_key_idx = accounts[2];
    if mint_key_idx >= account_keys.len() {
        return Ok(None);
    }

    let mint = account_keys[mint_key_idx].clone();
    if Pubkey::from_str(&mint).is_err() {
        return Ok(None);
    }

    // Extract token amount from data[8..16]
    let token_amount = if data_bytes.len() >= 16 {
        Some(u64::from_le_bytes(data_bytes[8..16].try_into().unwrap_or([0; 8])))
    } else {
        None
    };

    info!("Parsed pump.fun sell: mint={} tokens={}", mint,
        token_amount.map(|a| a.to_string()).unwrap_or_else(|| "?".to_string()));

    Ok(Some(ParsedSell { mint, token_amount, sol_received: None, amm_pool: None }))
}

/// Extract the AMM pool address from a Raydium V4/CPMM instruction.
/// In Raydium V4, account index 1 of the swap instruction is the AMM pool.
fn extract_raydium_amm_pool(
    instructions: &[&serde_json::Value],
    account_keys: &[String],
    raydium_program: &str,
) -> Option<String> {
    let program_idx = account_keys.iter().position(|k| k == raydium_program)?;
    for instr in instructions {
        let pid = instr.get("programIdIndex").and_then(|p| p.as_u64())? as usize;
        if pid != program_idx { continue; }
        let accounts = instr.get("accounts").and_then(|a| a.as_array())?;
        // Account index 1 in the instruction is the AMM pool
        if accounts.len() > 1 {
            let amm_idx = accounts[1].as_u64()? as usize;
            if amm_idx < account_keys.len() {
                return Some(account_keys[amm_idx].clone());
            }
        }
    }
    None
}

/// Result of parsing a Raydium swap — either a buy or sell
enum RaydiumSwapResult {
    Buy(ParsedBuy),
    Sell(ParsedSell),
}

/// Parse a Raydium V4/CPMM swap from token balance changes.
///
/// Raydium swaps don't have a simple discriminator-based layout we can rely on.
/// Instead we detect direction from the pre/post token balance changes in tx meta:
/// - If the target wallet GAINED a non-WSOL token and LOST SOL → buy
/// - If the target wallet LOST a non-WSOL token and GAINED SOL → sell
///
/// We extract the mint from whichever non-WSOL token changed for the target.
fn try_parse_raydium_swap(
    result: &serde_json::Value,
    account_keys: &[String],
    target_wallet: &str,
    sol_delta: Option<(f64, f64)>,
    amm_pool: Option<&str>,
) -> Option<RaydiumSwapResult> {
    // Verify target wallet is in the transaction
    let _target_idx = account_keys.iter().position(|k| k == target_wallet)?;

    // Scan preTokenBalances / postTokenBalances for the target wallet's token changes
    let pre_tokens = result.get("meta")?.get("preTokenBalances")?.as_array()?;
    let post_tokens = result.get("meta")?.get("postTokenBalances")?.as_array()?;

    // Build map: mint → (pre_amount, post_amount) for the target wallet
    // Token balance entries have: { accountIndex, mint, uiTokenAmount: { amount: "..." } }
    let mut pre_map: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut post_map: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    for entry in pre_tokens {
        let owner = entry.get("owner").and_then(|o| o.as_str()).unwrap_or("");
        if owner != target_wallet {
            // Also check by accountIndex → account_keys[idx] matching target wallet
            let idx = entry.get("accountIndex").and_then(|i| i.as_u64()).unwrap_or(u64::MAX) as usize;
            if idx >= account_keys.len() || account_keys[idx] != target_wallet {
                continue;
            }
        }
        if let Some(mint) = entry.get("mint").and_then(|m| m.as_str()) {
            if mint == WSOL_MINT { continue; }
            let amount = entry.get("uiTokenAmount")
                .and_then(|u| u.get("amount"))
                .and_then(|a| a.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            pre_map.insert(mint.to_string(), amount);
        }
    }

    for entry in post_tokens {
        let owner = entry.get("owner").and_then(|o| o.as_str()).unwrap_or("");
        if owner != target_wallet {
            let idx = entry.get("accountIndex").and_then(|i| i.as_u64()).unwrap_or(u64::MAX) as usize;
            if idx >= account_keys.len() || account_keys[idx] != target_wallet {
                continue;
            }
        }
        if let Some(mint) = entry.get("mint").and_then(|m| m.as_str()) {
            if mint == WSOL_MINT { continue; }
            let amount = entry.get("uiTokenAmount")
                .and_then(|u| u.get("amount"))
                .and_then(|a| a.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            post_map.insert(mint.to_string(), amount);
        }
    }

    // Find a non-WSOL mint where token balance changed
    // Collect all mints from both pre and post
    let all_mints: std::collections::HashSet<&String> = pre_map.keys().chain(post_map.keys()).collect();

    for mint in all_mints {
        let pre_amt = pre_map.get(mint).copied().unwrap_or(0);
        let post_amt = post_map.get(mint).copied().unwrap_or(0);

        if post_amt > pre_amt {
            // Token balance increased → target BOUGHT tokens
            let sol_spent = sol_delta.map(|(_, spent)| spent).filter(|s| *s > 0.0);
            info!("Parsed Raydium buy: mint={} tokens_gained={} sol_spent={:.4} pool={}",
                mint, post_amt - pre_amt, sol_spent.unwrap_or(0.0), amm_pool.unwrap_or("?"));
            return Some(RaydiumSwapResult::Buy(ParsedBuy {
                mint: mint.clone(),
                sol_amount: sol_spent,
                token_amount: Some(post_amt - pre_amt),
                amm_pool: amm_pool.map(|s| s.to_string()),
                target_pump_accounts: None, // Raydium swaps don't use pump accounts
            }));
        } else if pre_amt > post_amt {
            // Token balance decreased → target SOLD tokens
            let token_sold = pre_amt - post_amt;
            let sol_received = sol_delta.map(|(received, _)| received).filter(|r| *r > 0.0);
            info!("Parsed Raydium sell: mint={} tokens_sold={} sol_received={:.4} pool={}",
                mint, token_sold, sol_received.unwrap_or(0.0), amm_pool.unwrap_or("?"));
            return Some(RaydiumSwapResult::Sell(ParsedSell {
                mint: mint.clone(),
                token_amount: Some(token_sold),
                sol_received,
                amm_pool: amm_pool.map(|s| s.to_string()),
            }));
        }
    }

    debug!("Raydium tx detected but no non-WSOL token balance change found for target wallet");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_dex_pumpfun() {
        let logs: Vec<String> = vec![
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]".into(),
            "Program log: Instruction: Buy".into(),
        ];
        assert_eq!(detect_dex(&logs), DexType::PumpFun);
    }

    #[test]
    fn test_detect_dex_raydium_v4() {
        let logs: Vec<String> = vec![
            "Program 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 invoke [1]".into(),
        ];
        assert_eq!(detect_dex(&logs), DexType::RaydiumV4);
    }

    #[test]
    fn test_detect_dex_raydium_cpmm() {
        let logs: Vec<String> = vec![
            "Program CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C invoke [1]".into(),
        ];
        assert_eq!(detect_dex(&logs), DexType::RaydiumCpmm);
    }

    #[test]
    fn test_detect_dex_unknown() {
        let logs: Vec<String> = vec![
            "Program 11111111111111111111111111111111 invoke [1]".into(),
        ];
        assert_eq!(detect_dex(&logs), DexType::Unknown("none".to_string()));
    }

    #[test]
    fn test_parse_logs_notification_success() {
        let msg = r#"{
            "jsonrpc": "2.0",
            "method": "logsNotification",
            "params": {
                "result": {
                    "context": { "slot": 12345 },
                    "value": {
                        "signature": "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQU",
                        "err": null,
                        "logs": [
                            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]",
                            "Program log: Instruction: Buy"
                        ]
                    }
                },
                "subscription": 1
            }
        }"#;

        let notif = parse_logs_notification(msg).unwrap().unwrap();
        assert_eq!(notif.signature, "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQU");
        assert_eq!(notif.slot, Some(12345));
        assert!(notif.err.is_none());
        assert_eq!(notif.logs.len(), 2);
    }

    #[test]
    fn test_parse_logs_notification_with_error() {
        let msg = r#"{
            "jsonrpc": "2.0",
            "method": "logsNotification",
            "params": {
                "result": {
                    "context": { "slot": 12345 },
                    "value": {
                        "signature": "abc123",
                        "err": { "InstructionError": [0, "Custom"] },
                        "logs": []
                    }
                },
                "subscription": 1
            }
        }"#;

        let notif = parse_logs_notification(msg).unwrap().unwrap();
        assert!(notif.err.is_some());
    }

    #[test]
    fn test_extract_account_keys_simple() {
        let result = serde_json::json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        "11111111111111111111111111111111",
                        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                    ]
                }
            }
        });
        let keys = extract_account_keys(&result).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "11111111111111111111111111111111");
    }

    #[test]
    fn test_extract_account_keys_with_loaded() {
        let result = serde_json::json!({
            "transaction": {
                "message": {
                    "accountKeys": ["key0", "key1"]
                }
            },
            "meta": {
                "loadedAddresses": {
                    "writable": ["key2"],
                    "readonly": ["key3", "key4"]
                }
            }
        });
        let keys = extract_account_keys(&result).unwrap();
        assert_eq!(keys.len(), 5);
    }

    #[test]
    fn test_try_parse_pump_buy_instruction() {
        // Build a fake instruction with BUY_DISCRIMINATOR
        let mut data = BUY_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&1000000u64.to_le_bytes()); // token amount
        data.extend_from_slice(&500000000u64.to_le_bytes()); // max_sol_cost = 0.5 SOL
        let data_b58 = bs58::encode(&data).into_string();

        // Use a valid base58 pubkey for the mint (32 bytes of 1s = So11...2)
        let fake_mint = "4uQeVj5tqViQh7yWWGStvkEG1Zmhx6uasJtWCJziofM"; // valid pubkey
        let account_keys = vec![
            "11111111111111111111111111111111".to_string(),
            "11111111111111111111111111111111".to_string(),
            fake_mint.to_string(), // index 2 = mint
            "11111111111111111111111111111111".to_string(),
        ];

        let mut keys = account_keys;
        keys.push(PUMP_FUN_PROGRAM.to_string()); // index 4

        let instr = serde_json::json!({
            "programIdIndex": 4,
            "accounts": [0, 1, 2, 3, 0, 0],
            "data": data_b58,
        });

        let result = try_parse_pump_buy_instruction(&instr, 4, &keys).unwrap();
        assert!(result.is_some());
        let parsed = result.unwrap();
        assert_eq!(parsed.mint, fake_mint);
        assert!((parsed.sol_amount.unwrap() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_try_parse_wrong_discriminator() {
        let data = vec![0u8; 24]; // wrong discriminator
        let data_b58 = bs58::encode(&data).into_string();

        let keys = vec![PUMP_FUN_PROGRAM.to_string(), "mint".to_string(), "other".to_string()];
        let instr = serde_json::json!({
            "programIdIndex": 0,
            "accounts": [0, 1, 2],
            "data": data_b58,
        });

        let result = try_parse_pump_buy_instruction(&instr, 0, &keys).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_compute_sol_delta_buy() {
        let result = serde_json::json!({
            "meta": {
                "preBalances": [5000000000u64, 100000000u64],
                "postBalances": [4500000000u64, 100000000u64]
            }
        });
        let keys = vec!["target_wallet".to_string(), "other".to_string()];

        let delta = compute_sol_delta(&result, &keys, "target_wallet");
        assert!(delta.is_some());
        let (received, spent) = delta.unwrap();
        assert!((spent - 0.5).abs() < 0.001);
        assert!(received == 0.0);
    }

    #[test]
    fn test_compute_sol_delta_sell() {
        let result = serde_json::json!({
            "meta": {
                "preBalances": [4500000000u64, 100000000u64],
                "postBalances": [5000000000u64, 100000000u64]
            }
        });
        let keys = vec!["target_wallet".to_string(), "other".to_string()];

        let delta = compute_sol_delta(&result, &keys, "target_wallet");
        assert!(delta.is_some());
        let (received, spent) = delta.unwrap();
        assert!((received - 0.5).abs() < 0.001);
        assert!(spent == 0.0);
    }

    #[test]
    fn test_compute_sol_delta_not_found() {
        let result = serde_json::json!({
            "meta": {
                "preBalances": [5000000000u64],
                "postBalances": [4500000000u64]
            }
        });
        let keys = vec!["some_wallet".to_string()];

        let delta = compute_sol_delta(&result, &keys, "missing_wallet");
        assert!(delta.is_none());
    }
}

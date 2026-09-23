use anyhow::{Context, Result};
use log::{info, warn, error, debug};
use prost::Message;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tokio::sync::mpsc;

use crate::copy_engine::{DetectedBuy, DetectedSell, DexType};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);

// ─── Program IDs (base58, decoded to bytes at startup) ───
const PUMP_FUN_PROGRAM_B58: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMP_FUN_AMM_B58: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const RAYDIUM_V4_B58: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const RAYDIUM_CPMM_B58: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Pre-decoded program ID bytes for fast comparison in hot loop
struct ProgramIds {
    pump_fun: Vec<u8>,
    pump_fun_amm: Vec<u8>,
    raydium_v4: Vec<u8>,
    raydium_cpmm: Vec<u8>,
}

impl ProgramIds {
    fn new() -> Result<Self> {
        Ok(Self {
            pump_fun: bs58::decode(PUMP_FUN_PROGRAM_B58).into_vec().context("bad pump_fun b58")?,
            pump_fun_amm: bs58::decode(PUMP_FUN_AMM_B58).into_vec().context("bad pump_fun_amm b58")?,
            raydium_v4: bs58::decode(RAYDIUM_V4_B58).into_vec().context("bad raydium_v4 b58")?,
            raydium_cpmm: bs58::decode(RAYDIUM_CPMM_B58).into_vec().context("bad raydium_cpmm b58")?,
        })
    }

    fn detect_dex(&self, keys: &[Vec<u8>]) -> DexType {
        for key in keys {
            if key.as_slice() == self.pump_fun.as_slice() { return DexType::PumpFun; }
            if key.as_slice() == self.pump_fun_amm.as_slice() { return DexType::PumpFunAmm; }
            if key.as_slice() == self.raydium_v4.as_slice() { return DexType::RaydiumV4; }
            if key.as_slice() == self.raydium_cpmm.as_slice() { return DexType::RaydiumCpmm; }
        }
        DexType::Unknown("none".to_string())
    }

    fn find_program_index(&self, keys: &[Vec<u8>], dex: &DexType) -> Option<usize> {
        let target = match dex {
            DexType::PumpFun => &self.pump_fun,
            DexType::PumpFunAmm => &self.pump_fun_amm,
            DexType::RaydiumV4 => &self.raydium_v4,
            DexType::RaydiumCpmm => &self.raydium_cpmm,
            _ => return None,
        };
        keys.iter().position(|k| k.as_slice() == target.as_slice())
    }
}

/// Buy discriminator for pump.fun
const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// Sell discriminator for pump.fun
const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

// ─── Hand-written prost structs matching yellowstone geyser.proto ───

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum CommitmentLevel {
    Processed = 0,
    Confirmed = 1,
    Finalized = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequest {
    #[prost(map = "string, message", tag = "1")]
    pub accounts: HashMap<String, SubscribeRequestFilterAccounts>,
    #[prost(map = "string, message", tag = "2")]
    pub slots: HashMap<String, SubscribeRequestFilterSlots>,
    #[prost(map = "string, message", tag = "3")]
    pub transactions: HashMap<String, SubscribeRequestFilterTransactions>,
    #[prost(map = "string, message", tag = "5")]
    pub blocks_meta: HashMap<String, SubscribeRequestFilterBlocksMeta>,
    #[prost(map = "string, message", tag = "10")]
    pub entries: HashMap<String, SubscribeRequestFilterEntry>,
    #[prost(enumeration = "i32", optional, tag = "4")]
    pub commitment: Option<i32>,
    #[prost(message, repeated, tag = "6")]
    pub accounts_data_slice: Vec<SubscribeRequestAccountsDataSlice>,
    #[prost(message, optional, tag = "7")]
    pub ping: Option<SubscribeRequestPing>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestFilterAccounts {
    #[prost(string, repeated, tag = "2")]
    pub account: Vec<String>,
    #[prost(string, repeated, tag = "3")]
    pub owner: Vec<String>,
    #[prost(bool, repeated, tag = "4")]
    pub filters: Vec<bool>,
    #[prost(bool, optional, tag = "5")]
    pub nonempty_txn_signature: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestFilterSlots {
    #[prost(bool, optional, tag = "1")]
    pub filter_by_commitment: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestFilterTransactions {
    #[prost(bool, optional, tag = "1")]
    pub vote: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub failed: Option<bool>,
    #[prost(string, repeated, tag = "3")]
    pub account_include: Vec<String>,
    #[prost(string, repeated, tag = "4")]
    pub account_exclude: Vec<String>,
    #[prost(string, optional, tag = "5")]
    pub signature: Option<String>,
    #[prost(string, repeated, tag = "6")]
    pub account_required: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestFilterBlocksMeta {}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestFilterEntry {}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestAccountsDataSlice {
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    #[prost(uint64, tag = "2")]
    pub length: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeRequestPing {
    #[prost(int32, tag = "1")]
    pub id: i32,
}

// ─── Subscribe Update (response) types ───

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdate {
    #[prost(string, repeated, tag = "1")]
    pub filters: Vec<String>,
    #[prost(message, optional, tag = "2")]
    pub account: Option<SubscribeUpdateAccount>,
    #[prost(message, optional, tag = "3")]
    pub slot: Option<SubscribeUpdateSlot>,
    #[prost(message, optional, tag = "4")]
    pub transaction: Option<SubscribeUpdateTransaction>,
    #[prost(message, optional, tag = "10")]
    pub pong: Option<SubscribeUpdatePong>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdateAccount {
    #[prost(message, optional, tag = "1")]
    pub account: Option<SubscribeUpdateAccountInfo>,
    #[prost(uint64, tag = "2")]
    pub slot: u64,
    #[prost(bool, tag = "3")]
    pub is_startup: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdateAccountInfo {
    #[prost(bytes = "vec", tag = "1")]
    pub pubkey: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub lamports: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub owner: Vec<u8>,
    #[prost(bool, tag = "4")]
    pub executable: bool,
    #[prost(uint64, tag = "5")]
    pub rent_epoch: u64,
    #[prost(bytes = "vec", tag = "6")]
    pub data: Vec<u8>,
    #[prost(uint64, tag = "7")]
    pub write_version: u64,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub txn_signature: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdateSlot {
    #[prost(uint64, tag = "1")]
    pub slot: u64,
    #[prost(uint64, optional, tag = "2")]
    pub parent: Option<u64>,
    #[prost(int32, tag = "3")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdateTransaction {
    #[prost(message, optional, tag = "1")]
    pub transaction: Option<SubscribeUpdateTransactionInfo>,
    #[prost(uint64, tag = "2")]
    pub slot: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdateTransactionInfo {
    #[prost(bytes = "vec", tag = "1")]
    pub signature: Vec<u8>,
    #[prost(bool, tag = "2")]
    pub is_vote: bool,
    #[prost(message, optional, tag = "3")]
    pub transaction: Option<Transaction>,
    #[prost(message, optional, tag = "4")]
    pub meta: Option<TransactionStatusMeta>,
    #[prost(uint64, tag = "5")]
    pub index: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Transaction {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub signatures: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "2")]
    pub message: Option<TransactionMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TransactionMessage {
    #[prost(message, optional, tag = "1")]
    pub header: Option<MessageHeader>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub account_keys: Vec<Vec<u8>>,
    #[prost(bytes = "vec", tag = "3")]
    pub recent_blockhash: Vec<u8>,
    #[prost(message, repeated, tag = "4")]
    pub instructions: Vec<CompiledInstruction>,
    #[prost(bool, tag = "5")]
    pub versioned: bool,
    #[prost(message, repeated, tag = "6")]
    pub address_table_lookups: Vec<MessageAddressTableLookup>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MessageHeader {
    #[prost(uint32, tag = "1")]
    pub num_required_signatures: u32,
    #[prost(uint32, tag = "2")]
    pub num_readonly_signed_accounts: u32,
    #[prost(uint32, tag = "3")]
    pub num_readonly_unsigned_accounts: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct CompiledInstruction {
    #[prost(uint32, tag = "1")]
    pub program_id_index: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub accounts: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MessageAddressTableLookup {
    #[prost(bytes = "vec", tag = "1")]
    pub account_key: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub writable_indexes: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub readonly_indexes: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TransactionStatusMeta {
    #[prost(message, optional, tag = "1")]
    pub err: Option<TransactionError>,
    #[prost(uint64, tag = "2")]
    pub fee: u64,
    #[prost(uint64, repeated, tag = "3")]
    pub pre_balances: Vec<u64>,
    #[prost(uint64, repeated, tag = "4")]
    pub post_balances: Vec<u64>,
    #[prost(message, repeated, tag = "5")]
    pub inner_instructions: Vec<InnerInstructions>,
    #[prost(string, repeated, tag = "6")]
    pub log_messages: Vec<String>,
    #[prost(message, repeated, tag = "7")]
    pub pre_token_balances: Vec<TokenBalance>,
    #[prost(message, repeated, tag = "8")]
    pub post_token_balances: Vec<TokenBalance>,
    // tag 9 = rewards (skipped)
    #[prost(bool, tag = "10")]
    pub inner_instructions_none: bool,
    #[prost(bool, tag = "11")]
    pub log_messages_none: bool,
    #[prost(bytes = "vec", repeated, tag = "12")]
    pub loaded_writable_addresses: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "13")]
    pub loaded_readonly_addresses: Vec<Vec<u8>>,
    // tag 14 = return_data (message, skipped)
    #[prost(bool, tag = "15")]
    pub return_data_none: bool,
    #[prost(uint64, optional, tag = "16")]
    pub compute_units_consumed: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TransactionError {
    #[prost(bytes = "vec", tag = "1")]
    pub err: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InnerInstructions {
    #[prost(uint32, tag = "1")]
    pub index: u32,
    #[prost(message, repeated, tag = "2")]
    pub instructions: Vec<InnerInstruction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InnerInstruction {
    #[prost(uint32, tag = "1")]
    pub program_id_index: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub accounts: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
    #[prost(uint32, optional, tag = "4")]
    pub stack_height: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TokenBalance {
    #[prost(uint32, tag = "1")]
    pub account_index: u32,
    #[prost(string, tag = "2")]
    pub mint: String,
    #[prost(message, optional, tag = "3")]
    pub ui_token_amount: Option<UiTokenAmount>,
    #[prost(string, tag = "4")]
    pub owner: String,
    #[prost(string, tag = "5")]
    pub program_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct UiTokenAmount {
    #[prost(double, tag = "1")]
    pub ui_amount: f64,
    #[prost(uint32, tag = "2")]
    pub decimals: u32,
    #[prost(string, tag = "3")]
    pub amount: String,
    #[prost(string, tag = "4")]
    pub ui_amount_string: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeUpdatePong {
    #[prost(int32, tag = "1")]
    pub id: i32,
}

// ─── Geyser listener — direct parsing mode ───

/// Run Geyser listener that parses transactions directly from proto data.
/// No RPC getTransaction calls needed — all data comes from Geyser stream.
pub async fn run_geyser_listener(
    geyser_url: &str,
    target_wallet: &str,
    buy_tx: mpsc::Sender<DetectedBuy>,
    sell_tx: Option<mpsc::Sender<DetectedSell>>,
) -> Result<()> {
    info!("Geyser gRPC listener starting (direct parse mode)");
    info!("Endpoint: {}", geyser_url);
    info!("Target wallet: {}", target_wallet);

    // Pre-decode target wallet and program IDs to bytes for comparison
    let target_bytes = bs58::decode(target_wallet).into_vec()
        .context("Invalid target wallet base58")?;
    let program_ids = ProgramIds::new().context("Failed to decode program IDs")?;

    loop {
        info!("Connecting to Geyser gRPC: {}", geyser_url);
        match connect_and_stream(geyser_url, target_wallet, &target_bytes, &program_ids, &buy_tx, &sell_tx).await {
            Ok(()) => {
                warn!("Geyser connection closed, reconnecting in {}s...", RECONNECT_DELAY.as_secs());
            }
            Err(e) => {
                error!("Geyser error: {:?}, reconnecting in {}s...", e, RECONNECT_DELAY.as_secs());
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn connect_and_stream(
    geyser_url: &str,
    target_wallet: &str,
    target_bytes: &[u8],
    program_ids: &ProgramIds,
    buy_tx: &mpsc::Sender<DetectedBuy>,
    sell_tx: &Option<mpsc::Sender<DetectedSell>>,
) -> Result<()> {
    let connect_start = Instant::now();

    let channel = if geyser_url.starts_with("https") {
        tonic::transport::Channel::from_shared(geyser_url.to_string())
            .context("Invalid Geyser URL")?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())
            .context("TLS config error")?
            .connect()
            .await
            .context("Failed to connect to Geyser gRPC (TLS)")?
    } else {
        tonic::transport::Channel::from_shared(geyser_url.to_string())
            .context("Invalid Geyser URL")?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .connect()
            .await
            .context("Failed to connect to Geyser gRPC")?
    };

    info!("Geyser gRPC connected in {}ms", connect_start.elapsed().as_millis());

    let request = build_geyser_subscribe_request(target_wallet);
    info!("Subscribing to Geyser transactions for wallet {}...", target_wallet);

    let mut client = tonic::client::Grpc::new(channel);
    client = client.max_decoding_message_size(64 * 1024 * 1024);
    client.ready().await.context("gRPC client not ready")?;

    let path: tonic::codegen::http::uri::PathAndQuery =
        "/geyser.Geyser/Subscribe".parse().context("Invalid gRPC path")?;
    let codec: tonic::codec::ProstCodec<SubscribeRequest, SubscribeUpdate> =
        tonic::codec::ProstCodec::default();

    let response = client
        .server_streaming(tonic::Request::new(request), path, codec)
        .await
        .context("Geyser Subscribe RPC failed")?;

    let mut stream = response.into_inner();
    let mut total_updates: u64 = 0;
    let mut tx_updates: u64 = 0;

    info!("Geyser stream established, waiting for target wallet activity...");

    let mut first_slot_logged = false;

    while let Some(update) = stream.message().await.context("Geyser stream error")? {
        total_updates += 1;

        if update.pong.is_some() {
            continue;
        }

        if let Some(slot) = &update.slot {
            if !first_slot_logged {
                info!("Geyser: first slot update: {} (stream alive)", slot.slot);
                first_slot_logged = true;
            }
            continue;
        }

        if let Some(tx_update) = &update.transaction {
            tx_updates += 1;
            let received_at = Instant::now();
            let slot = tx_update.slot;

            if let Some(tx_info) = &tx_update.transaction {
                let sig = if !tx_info.signature.is_empty() {
                    bs58::encode(&tx_info.signature).into_string()
                } else {
                    "unknown".to_string()
                };
                let sig_short = &sig[..sig.len().min(16)];

                // Skip errored transactions
                if tx_info.meta.as_ref().map_or(false, |m| m.err.is_some()) {
                    continue;
                }

                // Build full account keys list (static + loaded addresses)
                let all_keys = match build_account_keys(tx_info) {
                    Some(keys) => keys,
                    None => {
                        debug!("Geyser: no account keys in tx {}...", sig_short);
                        continue;
                    }
                };

                info!("Geyser: target TX {}... (slot {}, {} keys, {} instructions)",
                    sig_short, slot, all_keys.len(),
                    tx_info.transaction.as_ref()
                        .and_then(|t| t.message.as_ref())
                        .map_or(0, |m| m.instructions.len()));

                // Detect DEX from account keys
                let dex = program_ids.detect_dex(&all_keys);
                if matches!(dex, DexType::Unknown(_)) {
                    debug!("Geyser: unknown DEX in tx {}...", sig_short);
                    continue;
                }

                // Also check logs for Pump.fun buy vs sell keyword
                let logs = tx_info.meta.as_ref()
                    .map(|m| &m.log_messages)
                    .cloned()
                    .unwrap_or_default();

                let detect_ms = received_at.elapsed().as_millis();

                // Parse buy/sell directly from proto data
                match &dex {
                    DexType::PumpFun => {
                        parse_pump_from_proto(
                            tx_info, &all_keys, target_bytes, program_ids, &sig, slot,
                            received_at, detect_ms, buy_tx, sell_tx, &logs,
                        ).await;
                    }
                    DexType::RaydiumV4 | DexType::RaydiumCpmm => {
                        parse_raydium_from_proto(
                            tx_info, &all_keys, target_bytes, program_ids, &sig, slot,
                            received_at, detect_ms, &dex, buy_tx, sell_tx,
                        ).await;
                    }
                    DexType::PumpFunAmm => {
                        parse_pumpswap_from_proto(
                            tx_info, &all_keys, target_bytes, program_ids, &sig, slot,
                            received_at, detect_ms, buy_tx, sell_tx,
                        ).await;
                    }
                    DexType::Jupiter => {
                        debug!("Geyser: Jupiter tx {}... — not yet supported", sig_short);
                    }
                    _ => {}
                }
            }
        }

        if tx_updates > 0 && tx_updates % 50000 == 0 {
            info!("Geyser stats: {} total updates, {} transactions scanned", total_updates, tx_updates);
        }
    }

    Ok(())
}

/// Build complete account keys list: static keys + loaded writable/readonly from ATL
fn build_account_keys(tx_info: &SubscribeUpdateTransactionInfo) -> Option<Vec<Vec<u8>>> {
    let msg = tx_info.transaction.as_ref()?.message.as_ref()?;
    let meta = tx_info.meta.as_ref()?;

    let mut keys: Vec<Vec<u8>> = msg.account_keys.clone();
    // Append loaded addresses from address table lookups
    for addr in &meta.loaded_writable_addresses {
        keys.push(addr.clone());
    }
    for addr in &meta.loaded_readonly_addresses {
        keys.push(addr.clone());
    }
    Some(keys)
}

/// Find target wallet's index in account keys
fn find_target_index(keys: &[Vec<u8>], target: &[u8]) -> Option<usize> {
    keys.iter().position(|k| k.as_slice() == target)
}

/// Compute SOL delta for target wallet from pre/post balances.
/// Returns (sol_received, sol_spent)
fn compute_sol_delta(meta: &TransactionStatusMeta, target_idx: usize) -> (f64, f64) {
    let pre = meta.pre_balances.get(target_idx).copied().unwrap_or(0);
    let post = meta.post_balances.get(target_idx).copied().unwrap_or(0);
    if pre > post {
        (0.0, (pre - post) as f64 / 1_000_000_000.0)
    } else {
        ((post - pre) as f64 / 1_000_000_000.0, 0.0)
    }
}

/// Parse Pump.fun buy/sell from proto instructions
async fn parse_pump_from_proto(
    tx_info: &SubscribeUpdateTransactionInfo,
    all_keys: &[Vec<u8>],
    target_bytes: &[u8],
    program_ids: &ProgramIds,
    sig: &str,
    slot: u64,
    received_at: Instant,
    detect_ms: u128,
    buy_tx: &mpsc::Sender<DetectedBuy>,
    sell_tx: &Option<mpsc::Sender<DetectedSell>>,
    logs: &[String],
) {
    let sig_short = &sig[..sig.len().min(16)];

    // Find pump program index
    let pump_idx = match program_ids.find_program_index(all_keys, &DexType::PumpFun) {
        Some(idx) => idx,
        None => return,
    };

    let target_idx = find_target_index(all_keys, target_bytes);
    let sol_delta = target_idx.and_then(|idx| tx_info.meta.as_ref().map(|m| compute_sol_delta(m, idx)));

    // Collect ALL instructions: top-level + inner
    let msg = match tx_info.transaction.as_ref().and_then(|t| t.message.as_ref()) {
        Some(m) => m,
        None => return,
    };

    let mut found_buy = false;
    let mut found_sell = false;

    // Helper to check an instruction
    let check_instruction = |program_id_index: u32, data: &[u8], accounts: &[u8]|
        -> (Option<(String, Option<u64>, Option<f64>, Vec<String>)>, Option<(String, Option<u64>, Option<f64>)>)
    {
        if program_id_index as usize != pump_idx || data.len() < 8 {
            return (None, None);
        }

        let mut buy_result = None;
        let mut sell_result = None;

        if data[..8] == BUY_DISCRIMINATOR && accounts.len() >= 3 {
            let mint_idx = accounts[2] as usize;
            if mint_idx < all_keys.len() {
                let mint = bs58::encode(&all_keys[mint_idx]).into_string();
                let token_amount = if data.len() >= 16 {
                    Some(u64::from_le_bytes(data[8..16].try_into().unwrap_or([0; 8])))
                } else { None };
                let sol_amount = sol_delta.map(|(_, spent)| spent).filter(|s| *s > 0.0)
                    .or_else(|| {
                        if data.len() >= 24 {
                            Some(u64::from_le_bytes(data[16..24].try_into().unwrap_or([0; 8])) as f64 / 1e9)
                        } else { None }
                    });
                // Collect all account pubkeys from this instruction
                let pump_accounts: Vec<String> = accounts.iter()
                    .filter_map(|&idx| all_keys.get(idx as usize))
                    .map(|k| bs58::encode(k).into_string())
                    .collect();
                buy_result = Some((mint, token_amount, sol_amount, pump_accounts));
            }
        } else if data[..8] == SELL_DISCRIMINATOR && accounts.len() >= 3 {
            let mint_idx = accounts[2] as usize;
            if mint_idx < all_keys.len() {
                let mint = bs58::encode(&all_keys[mint_idx]).into_string();
                let token_amount = if data.len() >= 16 {
                    Some(u64::from_le_bytes(data[8..16].try_into().unwrap_or([0; 8])))
                } else { None };
                let sol_received = sol_delta.map(|(recv, _)| recv).filter(|r| *r > 0.0);
                sell_result = Some((mint, token_amount, sol_received));
            }
        }

        (buy_result, sell_result)
    };

    // Check top-level instructions
    for ix in &msg.instructions {
        let (buy, sell) = check_instruction(ix.program_id_index, &ix.data, &ix.accounts);
        if let Some((mint, token_amount, sol_amount, pump_accounts)) = buy {
            if !found_buy {
                found_buy = true;
                let total_ms = received_at.elapsed().as_millis();
                info!(
                    "TARGET BUY DETECTED [geyser direct]: {} on Pump.fun | mint: {} | tokens: {} | sig: {}... | slot: {} | TIMING: {}ms total (detect {}ms) | {} pump accounts",
                    sol_amount.map(|a| format!("{:.4} SOL", a)).unwrap_or("? SOL".to_string()),
                    mint, token_amount.map(|t| t.to_string()).unwrap_or("?".to_string()),
                    sig_short, slot, total_ms, detect_ms, pump_accounts.len(),
                );
                let detected = DetectedBuy {
                    signature: sig.to_string(),
                    mint,
                    dex: DexType::PumpFun,
                    sol_amount,
                    token_amount,
                    slot: Some(slot),
                    ws_received_at: received_at,
                    amm_pool: None,
                    target_pump_accounts: if pump_accounts.len() > 16 { Some(pump_accounts) } else { None },
                };
                if let Err(e) = buy_tx.send(detected).await {
                    error!("Failed to send buy signal: {}", e);
                }
            }
        }
        if let Some((mint, token_amount, sol_received)) = sell {
            if !found_sell {
                found_sell = true;
                let total_ms = received_at.elapsed().as_millis();
                info!(
                    "TARGET SELL DETECTED [geyser direct]: {} | {} tokens on Pump.fun | sig: {}... | slot: {} | TIMING: {}ms",
                    mint, token_amount.map(|t| t.to_string()).unwrap_or("?".to_string()),
                    sig_short, slot, total_ms,
                );
                if let Some(stx) = sell_tx {
                    let detected = DetectedSell {
                        signature: sig.to_string(),
                        mint,
                        dex: DexType::PumpFun,
                        sol_received,
                        token_amount,
                        slot: Some(slot),
                        ws_received_at: received_at,
                        amm_pool: None,
                    };
                    if let Err(e) = stx.send(detected).await {
                        error!("Failed to send sell signal: {}", e);
                    }
                }
            }
        }
    }

    // Check inner instructions
    if let Some(meta) = &tx_info.meta {
        for inner_group in &meta.inner_instructions {
            for ix in &inner_group.instructions {
                let (buy, sell) = check_instruction(ix.program_id_index, &ix.data, &ix.accounts);
                if let Some((mint, token_amount, sol_amount, pump_accounts)) = buy {
                    if !found_buy {
                        found_buy = true;
                        let total_ms = received_at.elapsed().as_millis();
                        info!(
                            "TARGET BUY DETECTED [geyser inner]: {} on Pump.fun | mint: {} | sig: {}... | TIMING: {}ms",
                            sol_amount.map(|a| format!("{:.4} SOL", a)).unwrap_or("? SOL".to_string()),
                            mint, sig_short, total_ms,
                        );
                        let detected = DetectedBuy {
                            signature: sig.to_string(),
                            mint,
                            dex: DexType::PumpFun,
                            sol_amount,
                            token_amount,
                            slot: Some(slot),
                            ws_received_at: received_at,
                            amm_pool: None,
                            target_pump_accounts: if pump_accounts.len() > 16 { Some(pump_accounts) } else { None },
                        };
                        if let Err(e) = buy_tx.send(detected).await {
                            error!("Failed to send buy signal: {}", e);
                        }
                    }
                }
                if let Some((mint, token_amount, sol_received)) = sell {
                    if !found_sell {
                        found_sell = true;
                        let total_ms = received_at.elapsed().as_millis();
                        info!(
                            "TARGET SELL DETECTED [geyser inner]: {} | {} tokens on Pump.fun | sig: {}... | TIMING: {}ms",
                            mint, token_amount.map(|t| t.to_string()).unwrap_or("?".to_string()),
                            sig_short, total_ms,
                        );
                        if let Some(stx) = sell_tx {
                            let detected = DetectedSell {
                                signature: sig.to_string(),
                                mint,
                                dex: DexType::PumpFun,
                                sol_received,
                                token_amount,
                                slot: Some(slot),
                                ws_received_at: received_at,
                                amm_pool: None,
                            };
                            if let Err(e) = stx.send(detected).await {
                                error!("Failed to send sell signal: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    // If we found nothing via instructions, check logs as fallback indicator
    if !found_buy && !found_sell {
        let has_buy_log = logs.iter().any(|l| l.contains("Instruction: Buy") || l.contains("Instruction: buy"));
        let has_sell_log = logs.iter().any(|l| l.contains("Instruction: Sell") || l.contains("Instruction: sell"));
        if has_buy_log || has_sell_log {
            debug!("Geyser: Pump.fun tx {}... had buy/sell logs but instruction parse failed", sig_short);
        }
    }
}

/// Parse PumpSwap AMM buy/sell from proto instructions.
///
/// PumpSwap account layout (17 accounts):
///   [0]  pool
///   [1]  user (signer)
///   [2]  global_config
///   [3]  base_mint (the meme token)
///   [4]  quote_mint (WSOL)
///   [5]  user_base_token_account
///   [6]  user_quote_token_account
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
///
/// Uses same BUY/SELL discriminators as pump.fun bonding curve.
/// Instruction data: discriminator(8) + base_amount(8) + quote_amount(8)
async fn parse_pumpswap_from_proto(
    tx_info: &SubscribeUpdateTransactionInfo,
    all_keys: &[Vec<u8>],
    target_bytes: &[u8],
    program_ids: &ProgramIds,
    sig: &str,
    slot: u64,
    received_at: Instant,
    detect_ms: u128,
    buy_tx: &mpsc::Sender<DetectedBuy>,
    sell_tx: &Option<mpsc::Sender<DetectedSell>>,
) {
    let sig_short = &sig[..sig.len().min(16)];

    let amm_idx = match program_ids.find_program_index(all_keys, &DexType::PumpFunAmm) {
        Some(idx) => idx,
        None => return,
    };

    let target_idx = find_target_index(all_keys, target_bytes);
    let sol_delta = target_idx.and_then(|idx| tx_info.meta.as_ref().map(|m| compute_sol_delta(m, idx)));

    let msg = match tx_info.transaction.as_ref().and_then(|t| t.message.as_ref()) {
        Some(m) => m,
        None => return,
    };

    let mut found_buy = false;
    let mut found_sell = false;

    /// Check a single instruction for PumpSwap buy/sell.
    /// Returns (buy_info, sell_info) where buy carries pumpswap account strings.
    let check_pumpswap_ix = |program_id_index: u32, data: &[u8], accounts: &[u8]|
        -> (Option<(String, Option<u64>, Option<f64>, String, Vec<String>)>,
            Option<(String, Option<u64>, Option<f64>)>)
    {
        if program_id_index as usize != amm_idx || data.len() < 24 {
            return (None, None);
        }

        // PumpSwap needs at least 17 accounts
        if accounts.len() < 17 {
            return (None, None);
        }

        let disc: [u8; 8] = match data[..8].try_into() {
            Ok(d) => d,
            Err(_) => return (None, None),
        };

        let is_buy = disc == BUY_DISCRIMINATOR;
        let is_sell = disc == SELL_DISCRIMINATOR;
        if !is_buy && !is_sell {
            return (None, None);
        }

        // base_mint is at account index [3]
        let mint_idx = accounts[3] as usize;
        if mint_idx >= all_keys.len() {
            return (None, None);
        }
        let mint = bs58::encode(&all_keys[mint_idx]).into_string();

        // pool is at account index [0]
        let pool_idx = accounts[0] as usize;
        let pool = if pool_idx < all_keys.len() {
            bs58::encode(&all_keys[pool_idx]).into_string()
        } else {
            String::new()
        };

        let amount1 = u64::from_le_bytes(data[8..16].try_into().unwrap_or([0; 8]));
        let amount2 = u64::from_le_bytes(data[16..24].try_into().unwrap_or([0; 8]));

        if is_buy {
            // Buy: base_amount_out(tokens), max_quote_amount_in(SOL lamports)
            let token_amount = Some(amount1);
            let sol_amount = sol_delta.map(|(_, spent)| spent).filter(|s| *s > 0.0)
                .or(Some(amount2 as f64 / 1e9));
            // Collect all account pubkeys for copytrade execution
            let pumpswap_accounts: Vec<String> = accounts.iter()
                .filter_map(|&idx| all_keys.get(idx as usize))
                .map(|k| bs58::encode(k).into_string())
                .collect();
            (Some((mint, token_amount, sol_amount, pool, pumpswap_accounts)), None)
        } else {
            // Sell: base_amount_in(tokens), min_quote_amount_out(SOL lamports)
            let token_amount = Some(amount1);
            let sol_received = sol_delta.map(|(recv, _)| recv).filter(|r| *r > 0.0)
                .or(Some(amount2 as f64 / 1e9));
            (None, Some((mint, token_amount, sol_received)))
        }
    };

    // Check top-level instructions
    for ix in &msg.instructions {
        let (buy, sell) = check_pumpswap_ix(ix.program_id_index, &ix.data, &ix.accounts);
        if let Some((mint, token_amount, sol_amount, pool, pumpswap_accounts)) = buy {
            if !found_buy {
                found_buy = true;
                let total_ms = received_at.elapsed().as_millis();
                info!(
                    "TARGET BUY DETECTED [geyser pumpswap]: {} on PumpSwap AMM | mint: {} | pool: {}.. | tokens: {} | sig: {}... | slot: {} | TIMING: {}ms total (detect {}ms) | {} accounts",
                    sol_amount.map(|a| format!("{:.4} SOL", a)).unwrap_or("? SOL".to_string()),
                    mint, &pool[..pool.len().min(8)],
                    token_amount.map(|t| t.to_string()).unwrap_or("?".to_string()),
                    sig_short, slot, total_ms, detect_ms, pumpswap_accounts.len(),
                );
                let detected = DetectedBuy {
                    signature: sig.to_string(),
                    mint,
                    dex: DexType::PumpFunAmm,
                    sol_amount,
                    token_amount,
                    slot: Some(slot),
                    ws_received_at: received_at,
                    amm_pool: Some(pool),
                    target_pump_accounts: if pumpswap_accounts.len() >= 17 { Some(pumpswap_accounts) } else { None },
                };
                if let Err(e) = buy_tx.send(detected).await {
                    error!("Failed to send PumpSwap buy signal: {}", e);
                }
            }
        }
        if let Some((mint, token_amount, sol_received)) = sell {
            if !found_sell {
                found_sell = true;
                let total_ms = received_at.elapsed().as_millis();
                info!(
                    "TARGET SELL DETECTED [geyser pumpswap]: {} | {} tokens on PumpSwap AMM | sig: {}... | slot: {} | TIMING: {}ms",
                    mint, token_amount.map(|t| t.to_string()).unwrap_or("?".to_string()),
                    sig_short, slot, total_ms,
                );
                if let Some(stx) = sell_tx {
                    let detected = DetectedSell {
                        signature: sig.to_string(),
                        mint,
                        dex: DexType::PumpFunAmm,
                        sol_received,
                        token_amount,
                        slot: Some(slot),
                        ws_received_at: received_at,
                        amm_pool: None,
                    };
                    if let Err(e) = stx.send(detected).await {
                        error!("Failed to send PumpSwap sell signal: {}", e);
                    }
                }
            }
        }
    }

    // Check inner instructions (PumpSwap may be invoked via CPI)
    if let Some(meta) = &tx_info.meta {
        for inner_group in &meta.inner_instructions {
            for ix in &inner_group.instructions {
                let (buy, sell) = check_pumpswap_ix(ix.program_id_index, &ix.data, &ix.accounts);
                if let Some((mint, token_amount, sol_amount, pool, pumpswap_accounts)) = buy {
                    if !found_buy {
                        found_buy = true;
                        let total_ms = received_at.elapsed().as_millis();
                        info!(
                            "TARGET BUY DETECTED [geyser pumpswap inner]: {} on PumpSwap AMM | mint: {} | sig: {}... | TIMING: {}ms",
                            sol_amount.map(|a| format!("{:.4} SOL", a)).unwrap_or("? SOL".to_string()),
                            mint, sig_short, total_ms,
                        );
                        let detected = DetectedBuy {
                            signature: sig.to_string(),
                            mint,
                            dex: DexType::PumpFunAmm,
                            sol_amount,
                            token_amount,
                            slot: Some(slot),
                            ws_received_at: received_at,
                            amm_pool: Some(pool),
                            target_pump_accounts: if pumpswap_accounts.len() >= 17 { Some(pumpswap_accounts) } else { None },
                        };
                        if let Err(e) = buy_tx.send(detected).await {
                            error!("Failed to send PumpSwap buy signal: {}", e);
                        }
                    }
                }
                if let Some((mint, token_amount, sol_received)) = sell {
                    if !found_sell {
                        found_sell = true;
                        let total_ms = received_at.elapsed().as_millis();
                        info!(
                            "TARGET SELL DETECTED [geyser pumpswap inner]: {} | {} tokens on PumpSwap AMM | sig: {}... | TIMING: {}ms",
                            mint, token_amount.map(|t| t.to_string()).unwrap_or("?".to_string()),
                            sig_short, total_ms,
                        );
                        if let Some(stx) = sell_tx {
                            let detected = DetectedSell {
                                signature: sig.to_string(),
                                mint,
                                dex: DexType::PumpFunAmm,
                                sol_received,
                                token_amount,
                                slot: Some(slot),
                                ws_received_at: received_at,
                                amm_pool: None,
                            };
                            if let Err(e) = stx.send(detected).await {
                                error!("Failed to send PumpSwap sell signal: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    if !found_buy && !found_sell {
        debug!("Geyser: PumpSwap AMM tx {}... — matched program but no buy/sell discriminator", sig_short);
    }
}

/// Parse Raydium V4/CPMM swap from proto token balance changes
async fn parse_raydium_from_proto(
    tx_info: &SubscribeUpdateTransactionInfo,
    all_keys: &[Vec<u8>],
    target_bytes: &[u8],
    program_ids: &ProgramIds,
    sig: &str,
    slot: u64,
    received_at: Instant,
    detect_ms: u128,
    dex: &DexType,
    buy_tx: &mpsc::Sender<DetectedBuy>,
    sell_tx: &Option<mpsc::Sender<DetectedSell>>,
) {
    let sig_short = &sig[..sig.len().min(16)];
    let meta = match &tx_info.meta {
        Some(m) => m,
        None => return,
    };

    let target_wallet_b58 = bs58::encode(target_bytes).into_string();
    let target_idx = match find_target_index(all_keys, target_bytes) {
        Some(idx) => idx,
        None => return,
    };

    let (sol_received, sol_spent) = compute_sol_delta(meta, target_idx);

    // Find Raydium program ID for AMM pool extraction
    let raydium_program_bytes: &[u8] = match dex {
        DexType::RaydiumV4 => &program_ids.raydium_v4,
        DexType::RaydiumCpmm => &program_ids.raydium_cpmm,
        _ => return,
    };

    // Extract AMM pool from instruction accounts
    let amm_pool = extract_amm_pool_from_proto(tx_info, all_keys, raydium_program_bytes);

    // Scan pre/post token balances for target's non-WSOL token changes
    let mut pre_map: HashMap<String, u64> = HashMap::new();
    let mut post_map: HashMap<String, u64> = HashMap::new();

    for entry in &meta.pre_token_balances {
        if entry.owner != target_wallet_b58 { continue; }
        if is_wsol_mint_str(&entry.mint) { continue; }
        let amount = entry.ui_token_amount.as_ref()
            .and_then(|u| u.amount.parse::<u64>().ok())
            .unwrap_or(0);
        pre_map.insert(entry.mint.clone(), amount);
    }

    for entry in &meta.post_token_balances {
        if entry.owner != target_wallet_b58 { continue; }
        if is_wsol_mint_str(&entry.mint) { continue; }
        let amount = entry.ui_token_amount.as_ref()
            .and_then(|u| u.amount.parse::<u64>().ok())
            .unwrap_or(0);
        post_map.insert(entry.mint.clone(), amount);
    }

    let all_mints: std::collections::HashSet<&String> = pre_map.keys().chain(post_map.keys()).collect();

    for mint in all_mints {
        let pre_amt = pre_map.get(mint).copied().unwrap_or(0);
        let post_amt = post_map.get(mint).copied().unwrap_or(0);

        if post_amt > pre_amt && sol_spent > 0.0 {
            // BUY: gained tokens, lost SOL
            let total_ms = received_at.elapsed().as_millis();
            let tokens_gained = post_amt - pre_amt;
            info!(
                "TARGET BUY DETECTED [geyser direct]: {:.4} SOL on {} | mint: {} | tokens: {} | sig: {}... | slot: {} | TIMING: {}ms (detect {}ms)",
                sol_spent, dex, mint, tokens_gained, sig_short, slot, total_ms, detect_ms,
            );
            let detected = DetectedBuy {
                signature: sig.to_string(),
                mint: mint.clone(),
                dex: dex.clone(),
                sol_amount: Some(sol_spent),
                token_amount: Some(tokens_gained),
                slot: Some(slot),
                ws_received_at: received_at,
                amm_pool: amm_pool.clone(),
                target_pump_accounts: None,
            };
            if let Err(e) = buy_tx.send(detected).await {
                error!("Failed to send buy signal: {}", e);
            }
            return;
        } else if pre_amt > post_amt && sol_received > 0.0 {
            // SELL: lost tokens, gained SOL
            let total_ms = received_at.elapsed().as_millis();
            let tokens_sold = pre_amt - post_amt;
            info!(
                "TARGET SELL DETECTED [geyser direct]: {} | {} tokens on {} | {:.4} SOL received | sig: {}... | slot: {} | TIMING: {}ms",
                mint, tokens_sold, dex, sol_received, sig_short, slot, total_ms,
            );
            if let Some(stx) = sell_tx {
                let detected = DetectedSell {
                    signature: sig.to_string(),
                    mint: mint.clone(),
                    dex: dex.clone(),
                    sol_received: Some(sol_received),
                    token_amount: Some(tokens_sold),
                    slot: Some(slot),
                    ws_received_at: received_at,
                    amm_pool: amm_pool.clone(),
                };
                if let Err(e) = stx.send(detected).await {
                    error!("Failed to send sell signal: {}", e);
                }
            }
            return;
        }
    }

    debug!("Geyser: {} tx {}... — no token balance change detected for target", dex, sig_short);
}

/// Extract AMM pool address from Raydium instruction (account index 1)
fn extract_amm_pool_from_proto(
    tx_info: &SubscribeUpdateTransactionInfo,
    all_keys: &[Vec<u8>],
    raydium_program: &[u8],
) -> Option<String> {
    let msg = tx_info.transaction.as_ref()?.message.as_ref()?;
    let program_idx = all_keys.iter().position(|k| k.as_slice() == raydium_program)?;

    for ix in &msg.instructions {
        if ix.program_id_index as usize == program_idx && ix.accounts.len() > 1 {
            let amm_idx = ix.accounts[1] as usize;
            if amm_idx < all_keys.len() {
                return Some(bs58::encode(&all_keys[amm_idx]).into_string());
            }
        }
    }
    None
}

fn is_wsol_mint_str(mint: &str) -> bool {
    mint == "So11111111111111111111111111111111111111112"
}

fn build_geyser_subscribe_request(target_wallet: &str) -> SubscribeRequest {
    let mut transactions = HashMap::new();
    transactions.insert(
        "target".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: vec![target_wallet.to_string()],
            account_exclude: vec![],
            signature: None,
            account_required: vec![],
        },
    );

    // Slots subscription required for Geyser stream to deliver transactions
    let mut slots = HashMap::new();
    slots.insert(
        "".to_string(),
        SubscribeRequestFilterSlots {
            filter_by_commitment: Some(false),
        },
    );

    SubscribeRequest {
        accounts: HashMap::new(),
        slots,
        transactions,
        blocks_meta: HashMap::new(),
        entries: HashMap::new(),
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![],
        ping: None,
    }
}

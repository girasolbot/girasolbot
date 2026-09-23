use anyhow::{Context, Result};
use log::{info, warn, error, debug};
use lru::LruCache;
use prost::Message;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;

use solana_client::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::state::AddressLookupTable;
use solana_sdk::message::{v0::MessageAddressTableLookup, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;
use crate::copy_engine::{DetectedBuy, DetectedSell, DexType};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
const SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// Capacity of the resolved ALT cache. Target wallet typically uses a small
/// rotating set of ALT addresses (often 1-5), so 128 entries is generous.
const ALT_CACHE_SIZE: usize = 128;

/// Resolved address lookup table: the full list of addresses stored in this ALT
/// account. ALTs are append-only on-chain, so once we've read N addresses they
/// stay at indices 0..N forever (extensions append at N+).
#[derive(Clone)]
struct ResolvedAlt {
    addresses: Arc<Vec<Pubkey>>,
}

/// LRU-backed cache of resolved address lookup tables. Cache misses spawn a
/// background RPC fetch and SKIP the current TX rather than blocking the hot
/// loop — the next TX referencing the same ALT will hit the cache.
#[derive(Clone)]
struct AltCache {
    cache: Arc<Mutex<LruCache<Pubkey, ResolvedAlt>>>,
    /// Pubkeys with an in-flight fetch — prevents thundering-herd duplicate fetches
    /// when a burst of TXs all reference the same uncached ALT.
    in_flight: Arc<Mutex<std::collections::HashSet<Pubkey>>>,
    rpc_client: Arc<RpcClient>,
}

impl AltCache {
    fn new(rpc_client: Arc<RpcClient>) -> Self {
        Self {
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(ALT_CACHE_SIZE).expect("ALT_CACHE_SIZE != 0"),
            ))),
            in_flight: Arc::new(Mutex::new(std::collections::HashSet::new())),
            rpc_client,
        }
    }

    /// Return resolved ALT if cached, else None. Never blocks on RPC.
    async fn get(&self, alt: &Pubkey) -> Option<ResolvedAlt> {
        let mut cache = self.cache.lock().await;
        cache.get(alt).cloned()
    }

    /// Spawn a background fetch for `alt` if not already in-flight and not cached.
    /// Designed to be fire-and-forget from the hot loop.
    fn prefetch(&self, alt: Pubkey) {
        let cache = self.cache.clone();
        let in_flight = self.in_flight.clone();
        let rpc_client = self.rpc_client.clone();
        tokio::spawn(async move {
            // De-duplicate: skip if another task is already fetching this ALT
            {
                let mut guard = in_flight.lock().await;
                if guard.contains(&alt) {
                    return;
                }
                // Double-check the cache too — may have been filled while we waited
                if cache.lock().await.contains(&alt) {
                    return;
                }
                guard.insert(alt);
            }

            // Fetch via spawn_blocking — RpcClient::get_account is sync
            let alt_for_task = alt;
            let result = tokio::task::spawn_blocking(move || {
                rpc_client.get_account(&alt_for_task)
            }).await;

            match result {
                Ok(Ok(account)) => match AddressLookupTable::deserialize(&account.data) {
                    Ok(table) => {
                        let addresses: Vec<Pubkey> = table.addresses.iter().copied().collect();
                        let len = addresses.len();
                        let resolved = ResolvedAlt { addresses: Arc::new(addresses) };
                        cache.lock().await.put(alt, resolved);
                        debug!("ALT cache: resolved {} ({} addresses)", alt, len);
                    }
                    Err(e) => warn!("ALT cache: deserialize failed for {}: {:?}", alt, e),
                },
                Ok(Err(e)) => debug!("ALT cache: get_account failed for {}: {}", alt, e),
                Err(e) => warn!("ALT cache: spawn_blocking panic for {}: {}", alt, e),
            }

            in_flight.lock().await.remove(&alt);
        });
    }
}

/// Decoded view of a TX's account keys with the dynamic ALT portion separated.
/// `static_keys` are always available; `writable_alt_keys` and `readonly_alt_keys`
/// are populated only when every referenced ALT is already cached.
struct ResolvedKeys<'a> {
    static_keys: &'a [Pubkey],
    /// Combined list ordered as: static_keys, then ALT writable keys (in lookup order),
    /// then ALT readonly keys (in lookup order). Matches the on-chain index mapping
    /// for v0 transactions when all ALTs are resolved. None when resolution failed.
    full_keys: Option<Vec<Pubkey>>,
}

impl<'a> ResolvedKeys<'a> {
    /// Get the account key at instruction-account-index `idx`. Returns None when
    /// the index falls into the ALT range but no resolution was available.
    fn get(&self, idx: usize) -> Option<Pubkey> {
        if idx < self.static_keys.len() {
            return Some(self.static_keys[idx]);
        }
        self.full_keys.as_ref().and_then(|v| v.get(idx).copied())
    }

    /// True when ALT-range indices can be resolved.
    fn alt_resolved(&self) -> bool {
        self.full_keys.is_some()
    }
}

/// Try to assemble static keys + ALT-resolved keys for this vtx. On any miss,
/// returns ResolvedKeys with `full_keys = None` and spawns background prefetches
/// so the next TX referencing those ALTs will hit the cache.
async fn resolve_keys<'a>(
    vtx: &'a VersionedTransaction,
    static_keys: &'a [Pubkey],
    alt_cache: &AltCache,
) -> ResolvedKeys<'a> {
    let lookups: &[MessageAddressTableLookup] = match &vtx.message {
        VersionedMessage::Legacy(_) => return ResolvedKeys { static_keys, full_keys: None },
        VersionedMessage::V0(m) => &m.address_table_lookups,
    };

    if lookups.is_empty() {
        // v0 message with no ATL — static keys are everything
        return ResolvedKeys { static_keys, full_keys: Some(static_keys.to_vec()) };
    }

    // Resolve every referenced ALT; on the first miss bail and prefetch the rest
    let mut resolved_tables: Vec<(ResolvedAlt, &MessageAddressTableLookup)> =
        Vec::with_capacity(lookups.len());
    let mut all_cached = true;
    for lookup in lookups {
        match alt_cache.get(&lookup.account_key).await {
            Some(r) => resolved_tables.push((r, lookup)),
            None => {
                all_cached = false;
                alt_cache.prefetch(lookup.account_key);
            }
        }
    }

    if !all_cached {
        return ResolvedKeys { static_keys, full_keys: None };
    }

    // Build the full key list in Solana's canonical order:
    //   static_keys + (writable from each ALT) + (readonly from each ALT)
    let mut full: Vec<Pubkey> = Vec::with_capacity(
        static_keys.len()
            + resolved_tables.iter().map(|(_, l)| l.writable_indexes.len() + l.readonly_indexes.len()).sum::<usize>(),
    );
    full.extend_from_slice(static_keys);

    for (table, lookup) in &resolved_tables {
        for &idx in &lookup.writable_indexes {
            match table.addresses.get(idx as usize) {
                Some(pk) => full.push(*pk),
                None => {
                    // ALT was extended after we cached it AND lookup references a new
                    // index — invalidate this entry so the next prefetch re-reads it.
                    alt_cache.cache.lock().await.pop(&lookup.account_key);
                    alt_cache.prefetch(lookup.account_key);
                    return ResolvedKeys { static_keys, full_keys: None };
                }
            }
        }
    }
    for (table, lookup) in &resolved_tables {
        for &idx in &lookup.readonly_indexes {
            match table.addresses.get(idx as usize) {
                Some(pk) => full.push(*pk),
                None => {
                    alt_cache.cache.lock().await.pop(&lookup.account_key);
                    alt_cache.prefetch(lookup.account_key);
                    return ResolvedKeys { static_keys, full_keys: None };
                }
            }
        }
    }

    ResolvedKeys { static_keys, full_keys: Some(full) }
}

// ─── Hand-written prost structs matching shredstream.proto ───

/// shredstream.Entry
#[derive(Clone, PartialEq, Message)]
pub struct ProtoEntry {
    #[prost(uint64, tag = "1")]
    pub slot: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub entries: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProtoFilterAccounts {
    #[prost(string, repeated, tag = "2")]
    pub account: Vec<String>,
    #[prost(string, repeated, tag = "3")]
    pub owner: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProtoFilterTransactions {
    #[prost(string, repeated, tag = "3")]
    pub account_include: Vec<String>,
    #[prost(string, repeated, tag = "4")]
    pub account_exclude: Vec<String>,
    #[prost(string, repeated, tag = "6")]
    pub account_required: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProtoFilterSlots {
    #[prost(bool, optional, tag = "1")]
    pub filter_by_commitment: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub interslot_updates: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscribeEntriesRequest {
    #[prost(map = "string, message", tag = "1")]
    pub accounts: HashMap<String, ProtoFilterAccounts>,
    #[prost(map = "string, message", tag = "2")]
    pub slots: HashMap<String, ProtoFilterSlots>,
    #[prost(map = "string, message", tag = "3")]
    pub transactions: HashMap<String, ProtoFilterTransactions>,
    #[prost(int32, optional, tag = "6")]
    pub commitment: Option<i32>,
}

#[derive(serde::Deserialize)]
struct SolanaEntry {
    #[allow(dead_code)]
    num_hashes: u64,
    #[allow(dead_code)]
    hash: solana_sdk::hash::Hash,
    transactions: Vec<VersionedTransaction>,
}

/// Run ShredStream listener in parallel with Geyser.
/// Sends DetectedBuy/DetectedSell on same channels — first-detect-wins.
///
/// `rpc_client` is used to resolve Address Lookup Tables (ALTs) for v0
/// transactions. ALTs are cached after the first hit; misses spawn a
/// background prefetch and skip the current TX so the hot loop never
/// blocks on RPC.
pub async fn run_shreds_listener(
    shreds_url: &str,
    target_wallet: &str,
    pump_program: &str,
    rpc_client: Arc<RpcClient>,
    buy_tx: mpsc::Sender<DetectedBuy>,
    sell_tx: mpsc::Sender<DetectedSell>,
) -> Result<()> {
    info!("ShredStream listener starting | endpoint: {} | target: {}", shreds_url, target_wallet);

    // ALT cache survives reconnect cycles — re-resolving hot ALTs on every WS
    // reconnect would defeat the cache's purpose.
    let alt_cache = AltCache::new(rpc_client);

    loop {
        info!("Connecting to ShredStream: {}", shreds_url);
        match connect_and_stream(shreds_url, target_wallet, pump_program, &alt_cache, &buy_tx, &sell_tx).await {
            Ok(()) => warn!("ShredStream closed, reconnecting in {}s...", RECONNECT_DELAY.as_secs()),
            Err(e) => error!("ShredStream error: {}, reconnecting in {}s...", e, RECONNECT_DELAY.as_secs()),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn connect_and_stream(
    shreds_url: &str,
    target_wallet: &str,
    pump_program: &str,
    alt_cache: &AltCache,
    buy_tx: &mpsc::Sender<DetectedBuy>,
    sell_tx: &mpsc::Sender<DetectedSell>,
) -> Result<()> {
    // P1 fix: pre-parse target_wallet + pump_program into Pubkey once per
    // connect-cycle. The hot loop previously did `k.to_string() == target_wallet`
    // for EVERY account key in EVERY TX (~20-40 String allocations per TX before
    // we even know if it's relevant), which is pure overhead because Pubkey
    // already implements PartialEq. Same pattern for pump_program in
    // `parse_pump_from_vtx`. Pubkey equality is a 32-byte memcmp.
    let target_pubkey = Pubkey::from_str(target_wallet)
        .context("Invalid target_wallet pubkey")?;
    let pump_pubkey = Pubkey::from_str(pump_program)
        .context("Invalid pump_program pubkey")?;

    let channel = tonic::transport::Channel::from_shared(shreds_url.to_string())
        .context("Invalid ShredStream URL")?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .connect()
        .await
        .context("Failed to connect to ShredStream")?;

    let request = build_subscribe_request(target_wallet);
    info!("ShredStream: subscribing for wallet {}...", target_wallet);

    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await.context("gRPC not ready")?;

    let path: tonic::codegen::http::uri::PathAndQuery =
        "/shredstream.ShredstreamProxy/SubscribeEntries".parse()
        .context("Invalid gRPC path")?;

    let codec: tonic::codec::ProstCodec<SubscribeEntriesRequest, ProtoEntry> =
        tonic::codec::ProstCodec::default();

    let response = client
        .server_streaming(tonic::Request::new(request), path, codec)
        .await
        .context("SubscribeEntries failed")?;

    let mut stream = response.into_inner();
    let mut stats = (0u64, 0u64, 0u64); // entries, txs, matched

    while let Some(entry) = stream.message().await.context("Stream error")? {
        let received_at = Instant::now();
        stats.0 += 1;

        let solana_entries: Vec<SolanaEntry> = match bincode::deserialize(&entry.entries) {
            Ok(e) => e,
            Err(e) => {
                debug!("ShredStream: deserialize failed slot {}: {}", entry.slot, e);
                continue;
            }
        };

        for sol_entry in &solana_entries {
            for vtx in &sol_entry.transactions {
                stats.1 += 1;
                let static_keys = vtx.message.static_account_keys();

                // Fast pre-filter: target wallet must be a STATIC signer/writable for
                // any meaningful TX it sends (it's always among static_keys when it's
                // the payer or a signer). This dodges the cost of ALT resolution for
                // the 99% of TXs that don't involve target.
                let target_found = static_keys.iter().any(|k| k == &target_pubkey);
                if !target_found { continue; }

                stats.2 += 1;
                let sig = vtx.signatures.first()
                    .map(|s| s.to_string())
                    .unwrap_or_default();

                // For target-matching TXs, resolve ALT entries so accounts[i] that
                // index into the dynamic range (mint, pool, etc.) work correctly.
                // On cache miss this returns full_keys=None and prefetches in
                // background — the next TX hits cache.
                let resolved = resolve_keys(vtx, static_keys, alt_cache).await;

                // Parse pump.fun instructions
                parse_pump_from_vtx(
                    vtx, &resolved, &pump_pubkey, &sig, entry.slot,
                    received_at, buy_tx, sell_tx,
                ).await;
            }
        }

        if stats.0 % 5000 == 0 {
            info!("ShredStream stats: {} entries, {} txs, {} matched", stats.0, stats.1, stats.2);
        }
    }

    Ok(())
}

/// Parse pump.fun buy/sell from a VersionedTransaction
async fn parse_pump_from_vtx(
    vtx: &VersionedTransaction,
    resolved: &ResolvedKeys<'_>,
    pump_program: &Pubkey,
    sig: &str,
    slot: u64,
    received_at: Instant,
    buy_tx: &mpsc::Sender<DetectedBuy>,
    sell_tx: &mpsc::Sender<DetectedSell>,
) {
    // Pump program is virtually always in static keys (it's referenced as a
    // program ID), so we only need to scan the static range.
    let pump_idx = match resolved.static_keys.iter().position(|k| k == pump_program) {
        Some(idx) => idx,
        None => return, // Not a pump TX
    };

    for ix in vtx.message.instructions() {
        if ix.program_id_index as usize != pump_idx || ix.data.len() < 8 {
            continue;
        }

        let accounts: Vec<usize> = ix.accounts.iter().map(|&a| a as usize).collect();
        let detect_ms = received_at.elapsed().as_millis();

        // Helper for the common "look up an account index" pattern. Returns
        // None when the index falls in the ALT range and resolution failed
        // (cache miss). The caller should skip in that case — the next TX
        // referencing the same ALT will hit cache.
        let get_key = |idx: usize| -> Option<Pubkey> { resolved.get(idx) };

        if ix.data[..8] == BUY_DISCRIMINATOR && accounts.len() >= 3 {
            // P1 fix: accounts[2] (mint) may live in an Address Lookup Table
            // for v0 transactions. The old code did `if mint_idx >= keys.len()
            // { continue; }`, which silently dropped every ALT-using buy.
            let mint = match get_key(accounts[2]) {
                Some(pk) => pk.to_string(),
                None => {
                    debug!(
                        "ShredStream: skipping BUY (mint in unresolved ALT, prefetch queued) | sig: {}...",
                        &sig[..sig.len().min(16)]
                    );
                    return;
                }
            };

            let token_amount = if ix.data.len() >= 16 {
                Some(u64::from_le_bytes(ix.data[8..16].try_into().unwrap_or([0; 8])))
            } else { None };

            let sol_amount = if ix.data.len() >= 24 {
                let lamports = u64::from_le_bytes(ix.data[16..24].try_into().unwrap_or([0; 8]));
                Some(lamports as f64 / 1e9)
            } else { None };

            // Collect all pump accounts. When ALT keys aren't resolved we
            // include only the resolvable ones (the buyer's pre-built template
            // can fill the rest from chain-derived PDAs).
            let pump_accounts: Vec<String> = accounts.iter()
                .filter_map(|&idx| get_key(idx))
                .map(|k| k.to_string())
                .collect();

            let sig_short = &sig[..sig.len().min(16)];
            let alt_note = if resolved.alt_resolved() { "" } else { " [partial-alt]" };
            info!(
                "TARGET BUY DETECTED [shreds]{}: {:.4} SOL on Pump.fun | mint: {} | tokens: {} | sig: {}... | slot: {} | detect: {}ms | {} accounts",
                alt_note,
                sol_amount.unwrap_or(0.0), mint,
                token_amount.map(|t| t.to_string()).unwrap_or("?".into()),
                sig_short, slot, detect_ms, pump_accounts.len(),
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
                error!("ShredStream: failed to send buy: {}", e);
            }
            return;
        }

        if ix.data[..8] == SELL_DISCRIMINATOR && accounts.len() >= 3 {
            let mint = match get_key(accounts[2]) {
                Some(pk) => pk.to_string(),
                None => {
                    debug!(
                        "ShredStream: skipping SELL (mint in unresolved ALT, prefetch queued) | sig: {}...",
                        &sig[..sig.len().min(16)]
                    );
                    return;
                }
            };

            let token_amount = if ix.data.len() >= 16 {
                Some(u64::from_le_bytes(ix.data[8..16].try_into().unwrap_or([0; 8])))
            } else { None };

            let sig_short = &sig[..sig.len().min(16)];
            info!(
                "TARGET SELL DETECTED [shreds]: {} | {} tokens on Pump.fun | sig: {}... | slot: {} | detect: {}ms",
                mint, token_amount.map(|t| t.to_string()).unwrap_or("?".into()),
                sig_short, slot, detect_ms,
            );

            let detected = DetectedSell {
                signature: sig.to_string(),
                mint,
                dex: DexType::PumpFun,
                token_amount,
                sol_received: None,
                slot: Some(slot),
                ws_received_at: received_at,
                amm_pool: None,
            };
            if let Err(e) = sell_tx.send(detected).await {
                error!("ShredStream: failed to send sell: {}", e);
            }
            return;
        }
    }
}

fn build_subscribe_request(target_wallet: &str) -> SubscribeEntriesRequest {
    let mut transactions = HashMap::new();
    transactions.insert(
        "target".to_string(),
        ProtoFilterTransactions {
            account_include: vec![target_wallet.to_string()],
            account_exclude: vec![],
            account_required: vec![],
        },
    );

    let mut slots = HashMap::new();
    slots.insert(
        "".to_string(),
        ProtoFilterSlots {
            filter_by_commitment: Some(false), // Get PROCESSED (earliest)
            interslot_updates: Some(true),
        },
    );

    SubscribeEntriesRequest {
        accounts: HashMap::new(),
        transactions,
        slots,
        commitment: Some(0), // PROCESSED
    }
}

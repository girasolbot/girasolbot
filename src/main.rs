mod blockhash_cache;
mod api_key_vault;
mod key_panel;
mod copy_engine;
mod logging;
mod position_monitor;
mod geyser_listener;
mod shreds_listener;
mod ws_listener;
mod error;
mod models;
mod settings;
mod idl;
mod onchain_idl;
mod tx_builder;
mod helius_sender;
mod leader_cache;
mod buyer;
mod raydium_v4;
mod raydium_cpmm;
mod tx_template;
mod swqos_sender;
mod quic_sender;
mod pumpswap;
mod rpc;

use anyhow::Result;
use arc_swap::ArcSwap;
use log::{info, warn, error, debug};
use tokio::sync::mpsc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::num::NonZeroUsize;
use scopeguard;
use std::time::Instant;

use crate::copy_engine::{DetectedBuy, DetectedSell};
use crate::models::PriceCache;
use crate::position_monitor::{PositionMonitor, SellOrder, SellReason};
use crate::leader_cache::LeaderCache;
use crate::settings::Settings;

use solana_client::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signer};
use tokio::sync::Mutex;

/// Trade statistics tracked across the session
pub struct Stats {
    pub total_detections: AtomicU64,
    pub total_buys: AtomicU64,
    pub total_skipped: AtomicU64,
    pub total_sells: AtomicU64,
    pub total_buy_failures: AtomicU64,
    pub total_sell_failures: AtomicU64,
    /// Total realized PnL in SOL (stored as i64 nanolamps for atomic ops: value * 1e9)
    total_pnl_nano: AtomicI64,
}

use std::sync::atomic::AtomicI64;

impl Stats {
    fn new() -> Self {
        Self {
            total_detections: AtomicU64::new(0),
            total_buys: AtomicU64::new(0),
            total_skipped: AtomicU64::new(0),
            total_sells: AtomicU64::new(0),
            total_buy_failures: AtomicU64::new(0),
            total_sell_failures: AtomicU64::new(0),
            total_pnl_nano: AtomicI64::new(0),
        }
    }

    /// Add realized SOL PnL (positive or negative)
    /// Uses round() to avoid f64→i64 truncation bias (raw `as i64` always rounds toward zero)
    pub fn add_pnl_sol(&self, sol: f64) {
        let nanos = (sol * 1e9).round() as i64;
        self.total_pnl_nano.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Get total realized PnL in SOL
    pub fn pnl_sol(&self) -> f64 {
        self.total_pnl_nano.load(Ordering::Relaxed) as f64 / 1e9
    }

    /// Print a summary table to the log
    pub fn log_summary(&self) {
        let detections = self.total_detections.load(Ordering::Relaxed);
        let buys = self.total_buys.load(Ordering::Relaxed);
        let skipped = self.total_skipped.load(Ordering::Relaxed);
        let sells = self.total_sells.load(Ordering::Relaxed);
        let buy_fails = self.total_buy_failures.load(Ordering::Relaxed);
        let sell_fails = self.total_sell_failures.load(Ordering::Relaxed);
        let pnl = self.pnl_sol();

        info!("========== SESSION STATISTICS ==========");
        info!("  Detections:      {}", detections);
        info!("  Buys executed:   {}", buys);
        info!("  Buys skipped:    {} (duplicate)", skipped);
        info!("  Buy failures:    {}", buy_fails);
        info!("  Sells executed:  {}", sells);
        info!("  Sell failures:   {}", sell_fails);
        info!("  Realized PnL:    {:+.9} SOL", pnl);
        info!("========================================");
    }
}

/// Wallet whose transactions are copied. Configured in config.toml
/// (`target_wallet`) — this const is only the built-in fallback.
const TARGET_WALLET: &str = "TargetWalletPlaceholder11111111111111111111111";

/// Load a keypair from settings (tries wallet_private_key_string, wallet_keypair_json, wallet_keypair_path)
fn load_keypair_from_settings(settings: &Settings) -> Option<Keypair> {
    if let Some(ref pk_str) = settings.wallet_private_key_string {
        match settings::parse_private_key_string(pk_str) {
            Ok(bytes) => match Keypair::try_from(bytes.as_slice()) {
                Ok(kp) => return Some(kp),
                Err(e) => warn!("wallet_private_key_string has valid format but invalid key: {}", e),
            },
            Err(e) => warn!("Failed to parse wallet_private_key_string: {}", e),
        }
    }

    if let Some(ref json_str) = settings.wallet_keypair_json {
        if let Ok(bytes) = serde_json::from_str::<Vec<u8>>(json_str) {
            match Keypair::try_from(bytes.as_slice()) {
                Ok(kp) => return Some(kp),
                Err(e) => warn!("wallet_keypair_json invalid: {}", e),
            }
        }
    }

    if let Some(ref path) = settings.wallet_keypair_path {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(bytes) = serde_json::from_str::<Vec<u8>>(&content) {
                match Keypair::try_from(bytes.as_slice()) {
                    Ok(kp) => return Some(kp),
                    Err(e) => warn!("wallet_keypair_path file invalid: {}", e),
                }
            }
        }
    }

    None
}

/// Load simulate keypair from settings
fn load_simulate_keypair(settings: &Settings) -> Option<Keypair> {
    if let Some(ref pk_str) = settings.simulate_wallet_private_key_string {
        match settings::parse_private_key_string(pk_str) {
            Ok(bytes) => match Keypair::try_from(bytes.as_slice()) {
                Ok(kp) => return Some(kp),
                Err(e) => warn!("simulate_wallet_private_key_string invalid key: {}", e),
            },
            Err(e) => warn!("Failed to parse simulate_wallet_private_key_string: {}", e),
        }
    }

    if let Some(ref json_str) = settings.simulate_wallet_keypair_json {
        if let Ok(bytes) = serde_json::from_str::<Vec<u8>>(json_str) {
            match Keypair::try_from(bytes.as_slice()) {
                Ok(kp) => return Some(kp),
                Err(e) => warn!("simulate_wallet_keypair_json invalid: {}", e),
            }
        }
    }

    None
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    logging::init();
    info!("=== SAME BLOCK SNIPER v{} ===", env!("CARGO_PKG_VERSION"));
    // Load config
    let config_path = std::env::var("SNIPER_CONFIG")
        .unwrap_or_else(|_| "config.toml".to_string());
    let settings = Settings::from_file(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    settings.validate()
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;
    info!("Config loaded and validated from {}", config_path);

    // Target wallet: config value wins (env SNIPER_TARGET_WALLET overrides),
    // built-in const is only a placeholder. Shared as Arc because listener
    // spawn tasks each need a clone across reconnect loops.
    let target_wallet = std::env::var("SNIPER_TARGET_WALLET").ok()
        .or_else(|| settings.target_wallet.clone())
        .unwrap_or_else(|| TARGET_WALLET.to_string());
    let target_wallet = Arc::new(target_wallet);
    info!("Target wallet: {}", target_wallet);

    // Determine mode: config dry_run=false OR --real CLI flag → real mode
    let cli_real = std::env::args().any(|a| a == "--real");
    let is_real = !settings.dry_run || cli_real;
    if is_real {
        warn!("REAL MODE ACTIVE - transactions will be submitted to the network");
        if cli_real && settings.dry_run {
            info!("(dry_run=true in config, overridden by --real CLI flag)");
        }
        if settings.skip_simulation {
            warn!("SKIP SIMULATION mode — TX sent without simulate (saves ~200ms, uses default CU/priority)");
        }
    } else {
        info!("DRY RUN mode - no real transactions will be sent");
    }

    // Startup config summary
    log_config_summary(&settings, is_real);

    // Wrap settings in ArcSwap for hot-reload support
    let shared_settings: Arc<ArcSwap<Settings>> = Arc::new(ArcSwap::from_pointee(settings));

    // Take a snapshot for initial setup (keypairs, RPC URLs — not hot-reloadable)
    let settings = shared_settings.load_full();

    // Load keypairs — two copies needed (buy executor + sell executor, Keypair is not Clone)
    // Buy keypairs are wrapped in Arc so the per-detection buy task (spawned to keep the
    // select! loop non-blocking) can cheaply clone a shared handle instead of borrowing.
    let buy_keypair: Arc<Option<Keypair>> = Arc::new(load_keypair_from_settings(&settings));
    let buy_simulate_keypair: Arc<Option<Keypair>> = Arc::new(load_simulate_keypair(&settings));
    let sell_keypair = load_keypair_from_settings(&settings);
    let sell_simulate_keypair = load_simulate_keypair(&settings);

    if is_real && buy_keypair.is_none() {
        return Err(anyhow::anyhow!(
            "REAL mode requires a wallet keypair. Set wallet_private_key_string, wallet_keypair_json, or wallet_keypair_path in config."
        ));
    }

    if let Some(kp) = buy_keypair.as_ref().as_ref() {
        info!("Wallet loaded: {}", kp.pubkey());
    } else {
        info!("No wallet keypair loaded (dry-run will use ephemeral keypair)");
    }

    // API-key management panel (encrypted vault + web UI). Disabled unless
    // PANEL_AUTH_TOKEN (min 8 chars) is set; binds to SNIPER_PANEL_BIND
    // (default 127.0.0.1:8078, loopback only).
    {
        let panel_bind = std::env::var("SNIPER_PANEL_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8078".to_string());
        let data_dir = std::env::var("SNIPER_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("data"));
        let panel_result = key_panel::start_panel(&panel_bind, data_dir).await;
        if let Err(e) = panel_result {
            warn!("Key panel disabled: {}", e);
        }
    }

    // Create RPC client
    let rpc_url = settings.solana_rpc_urls.first()
        .cloned()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    let rpc_client = Arc::new(RpcClient::new(&rpc_url));

    // Create price cache
    let cache_size = NonZeroUsize::new(settings.cache_capacity)
        .unwrap_or_else(|| NonZeroUsize::new(128).expect("128 is non-zero"));
    let price_cache: Arc<Mutex<PriceCache>> = Arc::new(Mutex::new(PriceCache::new(cache_size)));

    // Start background blockhash cache (refreshes every 400ms) and wait for
    // initial population. Without this wait, detections arriving in the first
    // ~400ms get None from get() and fall back to a synchronous RPC fetch in
    // the buyer hot path — exactly when we want speed. 5s timeout is generous;
    // a healthy ERPC endpoint returns the first blockhash in <100ms.
    let blockhash_cache = blockhash_cache::BlockhashCache::start_and_wait(
        rpc_client.clone(),
        std::time::Duration::from_secs(5),
    ).await;
    info!("Blockhash cache started (400ms refresh)");

    // Start background slot-leader cache (refreshes every 400ms). It is used by
    // Agave #13267 own-leader strategy to boost tips/priority when the submission
    // endpoint is the current leader, and to optionally skip providers that are
    // the current leader. Disabled by default — when the strategy is off the cache
    // still runs but no multipliers are applied.
    let leader_cache = leader_cache::LeaderCache::start_and_wait(
        rpc_client.clone(),
        std::time::Duration::from_secs(5),
    ).await;
    info!("Leader cache started (400ms refresh)");

    // Pre-fetch fee recipients from Global PDA (saves ~1300ms per buy)
    let cached_fee_recipients = {
        let normal_fut = rpc::fetch_fee_recipient_for_mint(false, &rpc_client, &settings);
        let mayhem_fut = rpc::fetch_fee_recipient_for_mint(true, &rpc_client, &settings);
        let (normal_res, mayhem_res) = tokio::join!(normal_fut, mayhem_fut);
        match (normal_res, mayhem_res) {
            (Ok(normal), Ok(mayhem)) => {
                info!("Fee recipients cached: normal={}, mayhem={}", normal, mayhem);
                Some(Arc::new(buyer::CachedFeeRecipients { normal, mayhem }))
            }
            (Ok(normal), Err(e)) => {
                warn!("Failed to cache mayhem fee_recipient: {}, using normal for both", e);
                Some(Arc::new(buyer::CachedFeeRecipients { normal, mayhem: normal }))
            }
            _ => {
                warn!("Failed to cache fee_recipients, will fetch per-buy");
                None
            }
        }
    };

    // Pre-build buy transaction template (all mint-independent values computed once)
    let buy_template = if let Some(ref fee_cache) = cached_fee_recipients {
        match tx_template::BuyTemplate::new(
            buy_keypair.as_ref().as_ref().map(|k| k.pubkey()).unwrap_or_else(|| {
                buy_simulate_keypair.as_ref().as_ref().map(|k| k.pubkey()).unwrap_or_default()
            }),
            &settings,
            fee_cache.normal,
            fee_cache.mayhem,
        ) {
            Ok(t) => {
                info!("BuyTemplate ready (IDL: {}, discriminator: {:?})", t.idl_name, t.buy_discriminator);
                Some(Arc::new(t))
            }
            Err(e) => {
                warn!("Failed to create BuyTemplate: {} — will use legacy path", e);
                None
            }
        }
    } else {
        warn!("No cached fee recipients — BuyTemplate disabled, using legacy path");
        None
    };

    // Initialize SWQoS concurrent sender (if configured)
    let swqos_sender = swqos_sender::SwqosSender::from_settings(&settings)
        .map(Arc::new);
    if swqos_sender.is_some() {
        info!("SWQoS concurrent send: ON (providers: {})", settings.swqos_provider_names());
    } else {
        info!("SWQoS concurrent send: OFF (using legacy helius_sender)");
    }

    // Pre-warm HTTP connection pool (establishes TCP+TLS before first trade)
    {
        let http = &*crate::rpc::SHARED_HTTP_CLIENT;
        let mut endpoints: Vec<String> = Vec::new();
        for url in &settings.solana_rpc_urls {
            endpoints.push(url.clone());
        }
        if settings.helius_sender_enabled {
            endpoints.push(settings.helius_sender_endpoint.clone());
        }
        if settings.swqos_concurrent_send {
            endpoints.push(settings.swqos_jito_url.clone());
        }
        let n = endpoints.len();
        let mut ok = 0usize;
        for url in &endpoints {
            let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getHealth"});
            match http.post(url).json(&body).timeout(std::time::Duration::from_secs(5)).send().await {
                Ok(_) => { ok += 1; }
                Err(e) => { debug!("Warm-up failed for {}: {}", url, e); }
            }
        }
        info!("Connection pool warmed: {}/{} endpoints", ok, n);
    }

    // Channel: WS listener → Copy Engine
    let (ws_tx, ws_rx) = mpsc::channel::<String>(1000);

    // Channel: Copy Engine → Buy Executor
    let (buy_tx, mut buy_rx) = mpsc::channel::<DetectedBuy>(100);

    // Channel: Copy Engine → Mirror Sell Handler (only if mirror_sells enabled)
    let (mirror_sell_tx, mut mirror_sell_rx) = mpsc::channel::<DetectedSell>(100);
    let mirror_sell_sender = if settings.mirror_sells {
        Some(mirror_sell_tx)
    } else {
        None
    };

    // Channel: Position Monitor → Sell Executor (also used for mirror sells)
    let (sell_tx, mut sell_rx) = mpsc::channel::<SellOrder>(100);
    let mirror_sell_order_tx = sell_tx.clone(); // clone before moving into monitor

    // Trade statistics
    let stats = Arc::new(Stats::new());

    // Start position monitor (checks TP/SL every 5s, uses ArcSwap for hot-reload)
    let position_monitor = PositionMonitor::start(
        sell_tx,
        price_cache.clone(),
        rpc_client.clone(),
        shared_settings.clone(),
        buy_keypair.as_ref().as_ref().map(|k| k.pubkey()),
    );
    info!("Position monitor started (5s interval)");

    // Spawn listener based on configured mode
    let listener_mode = settings.listener_mode.clone();
    let listener_handle = match listener_mode.as_str() {
        "shreds" => {
            let shreds_url = settings.shreds_url.clone();
            info!("Listener mode: SHREDS (ShredStream gRPC) -> {}", shreds_url);
            let shreds_buy_tx = buy_tx.clone();
            // Shreds always needs a sell channel. If mirror_sells is off,
            // create a drain channel that silently discards sells (no error spam).
            let shreds_sell_tx: mpsc::Sender<DetectedSell> = if settings.mirror_sells {
                mirror_sell_sender.clone().expect("mirror_sell_sender required for shreds mode")
            } else {
                // P2-1 fix: keep the receiver alive in a background drain task
                // so sends succeed silently instead of failing on a closed channel.
                let (drain_tx, mut drain_rx) = mpsc::channel::<DetectedSell>(100);
                tokio::spawn(async move { while drain_rx.recv().await.is_some() {} });
                info!("Shreds mode: mirror_sells=OFF, sell detections silently drained");
                drain_tx
            };
            let pump_program = settings.pump_fun_program.clone();
            let shreds_rpc_client = rpc_client.clone();
            let target_wallet = Arc::clone(&target_wallet);
            tokio::spawn(async move {
                drop(ws_tx);
                // P1 fix: outer restart loop — see geyser spawn below.
                let mut backoff_secs: u64 = 5;
                loop {
                    match shreds_listener::run_shreds_listener(&shreds_url, &target_wallet, &pump_program, shreds_rpc_client.clone(), shreds_buy_tx.clone(), shreds_sell_tx.clone()).await {
                        Ok(()) => log::warn!("Shreds listener exited cleanly — restarting in {}s", backoff_secs),
                        Err(e) => log::error!("Shreds listener fatal error: {} — restarting in {}s", e, backoff_secs),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            })
        }
        "geyser" => {
            let geyser_url = settings.geyser_url.clone();
            let target_wallet = Arc::clone(&target_wallet);
            info!("Listener mode: GEYSER (Yellowstone gRPC, direct parse) -> {}", geyser_url);
            // Geyser mode: parse transactions directly from proto data, no RPC getTransaction needed
            let geyser_buy_tx = buy_tx.clone();
            let geyser_sell_tx = mirror_sell_sender.clone();
            // Also spawn ShredStream in parallel if URL is configured (first-detect-wins)
            if !settings.shreds_url.is_empty() {
                let shreds_url = settings.shreds_url.clone();
                let shreds_buy_tx = buy_tx.clone();
                let shreds_sell_tx: mpsc::Sender<DetectedSell> = if settings.mirror_sells {
                    mirror_sell_sender.clone().expect("mirror_sell_sender required")
                } else {
                    // P2-1 fix: drain channel with alive receiver (closed channel causes error spam)
                    let (drain_tx, mut drain_rx) = mpsc::channel::<DetectedSell>(100);
                    tokio::spawn(async move { while drain_rx.recv().await.is_some() {} });
                    info!("Parallel Shreds: mirror_sells=OFF, sell detections silently drained");
                    drain_tx
                };
                let pump_program = settings.pump_fun_program.clone();
                let shreds_rpc_client = rpc_client.clone();
                let target_wallet = Arc::clone(&target_wallet);
                info!("Also spawning ShredStream listener: {} (parallel, first-detect-wins)", shreds_url);
                tokio::spawn(async move {
                    // P1 fix: outer restart loop — see geyser spawn below.
                    let mut backoff_secs: u64 = 5;
                    loop {
                        match shreds_listener::run_shreds_listener(&shreds_url, &target_wallet, &pump_program, shreds_rpc_client.clone(), shreds_buy_tx.clone(), shreds_sell_tx.clone()).await {
                            Ok(()) => log::warn!("ShredStream listener exited cleanly — restarting in {}s", backoff_secs),
                            Err(e) => log::error!("ShredStream listener error: {} — restarting in {}s", e, backoff_secs),
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(60);
                    }
                });
            }
            let target_wallet = Arc::clone(&target_wallet);
            tokio::spawn(async move {
                // Drop ws_tx since geyser bypasses copy_engine
                drop(ws_tx);
                // P1 fix: outer restart loop. run_geyser_listener already
                // reconnects on stream errors INSIDE its inner loop, but
                // any error that escapes that loop (URL parse, ProgramIds
                // decode, initial connect failure that bubbles up) returns
                // Err from the spawned task — and without this outer loop
                // the task dies and the bot is silently blind for the rest
                // of its lifetime. We retry forever with a backoff so the
                // bot self-heals from transient startup failures.
                let mut backoff_secs: u64 = 5;
                loop {
                    match geyser_listener::run_geyser_listener(&geyser_url, &target_wallet, geyser_buy_tx.clone(), geyser_sell_tx.clone()).await {
                        Ok(()) => {
                            log::warn!(
                                "Geyser listener exited cleanly (should not happen) — restarting in {}s",
                                backoff_secs
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "Geyser listener fatal error: {} — restarting in {}s",
                                e, backoff_secs
                            );
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    // Cap exponential backoff at 60s so we don't sleep for
                    // minutes after extended outages.
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            })
        }
        _ => {
            let wss_url = settings.solana_ws_urls.first()
                .cloned()
                .unwrap_or_else(|| "wss://api.mainnet-beta.solana.com/".to_string());
            info!("Listener mode: WEBSOCKET -> {}", wss_url);
            let target_wallet = Arc::clone(&target_wallet);
            tokio::spawn(async move {
                // P1 fix: outer restart loop — see geyser spawn above.
                let mut backoff_secs: u64 = 5;
                loop {
                    match ws_listener::run_ws_listener(&wss_url, &target_wallet, ws_tx.clone()).await {
                        Ok(()) => log::warn!("WS listener exited cleanly — restarting in {}s", backoff_secs),
                        Err(e) => log::error!("WS listener fatal error: {} — restarting in {}s", e, backoff_secs),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            })
        }
    };

    // Spawn Copy Engine (uses snapshot — RPC URLs don't hot-reload)
    let engine_settings = shared_settings.load_full();
    let engine_target_wallet = Arc::clone(&target_wallet);
    let engine_handle = tokio::spawn(async move {
        if let Err(e) = copy_engine::run_copy_engine(&engine_target_wallet, ws_rx, buy_tx, mirror_sell_sender, engine_settings).await {
            log::error!("Copy engine fatal error: {}", e);
        }
    });

    // Spawn Sell Executor (uses ArcSwap for hot-reload of sell-related settings)
    let sell_rpc = rpc_client.clone();
    let sell_shared_settings = shared_settings.clone();
    let sell_monitor = position_monitor.clone();
    let sell_stats = stats.clone();
    let sell_cached_fee = cached_fee_recipients.clone();
    let sell_swqos = swqos_sender.clone();
    let sell_leader_cache = leader_cache.clone();
    let sell_handle = tokio::spawn(async move {
        run_sell_executor(
            &mut sell_rx,
            is_real,
            sell_keypair.as_ref(),
            sell_simulate_keypair.as_ref(),
            &sell_rpc,
            &sell_shared_settings,
            &sell_monitor,
            &sell_stats,
            sell_cached_fee.as_ref().map(|arc| arc.as_ref()),
            sell_swqos.as_ref(),
            &sell_leader_cache,
        ).await;
    });

    // Spawn config file watcher for hot-reload
    let watcher_settings = shared_settings.clone();
    let watcher_config_path = config_path.clone();
    tokio::spawn(async move {
        run_config_watcher(watcher_config_path, watcher_settings).await;
    });

    // Buy executor (main thread) with Ctrl+C shutdown
    // Settings are loaded from ArcSwap each iteration for hot-reload
    {
        let settings_snapshot = shared_settings.load_full();
        if settings_snapshot.mirror_percent > 0.0 {
            info!("Sniper ready. Mirror mode: {:.1}% of target | Waiting for target wallet activity...", settings_snapshot.mirror_percent);
        } else {
            info!("Sniper ready. Buy amount: {:.4} SOL | Waiting for target wallet activity...", settings_snapshot.buy_amount);
        }
    }

    let shutdown_stats = stats.clone();
    // Dedup: track recently seen signatures to avoid processing same TX from both geyser + shreds.
    // P1-3 fix: time-based eviction. The previous HashSet::drain().take(200) evicted
    // signatures in arbitrary hash order, so recently-seen sigs (which matter most for
    // dedup) could be dropped while old ones were kept — re-admitting a duplicate buy.
    // We now key each signature by the Instant it was first seen and evict only entries
    // older than SEEN_SIG_TTL, which deterministically keeps the most recent signatures.
    const SEEN_SIG_TTL: std::time::Duration = std::time::Duration::from_secs(120);
    let mut seen_sigs: std::collections::HashMap<String, Instant> = std::collections::HashMap::new();
    let mut seen_sigs_cleanup_counter: u32 = 0;
    loop {
        tokio::select! {
            detected = buy_rx.recv() => {
                let detected = match detected {
                    Some(d) => d,
                    None => {
                        warn!("Buy channel closed, shutting down");
                        break;
                    }
                };

                // Dedup: skip if we already processed this signature.
                // insert() returns Some(prev) if the key already existed → duplicate.
                if seen_sigs.insert(detected.signature.clone(), Instant::now()).is_some() {
                    debug!("DEDUP: skipping already-processed sig {}...", &detected.signature[..16.min(detected.signature.len())]);
                    continue;
                }
                // Periodic cleanup: evict signatures older than SEEN_SIG_TTL.
                // P1-3 fix: time-based eviction keeps the most recent signatures
                // deterministically (unlike HashSet::drain which evicted arbitrarily).
                seen_sigs_cleanup_counter += 1;
                if seen_sigs_cleanup_counter > 100 {
                    let now = Instant::now();
                    seen_sigs.retain(|_sig, seen_at| now.duration_since(*seen_at) < SEEN_SIG_TTL);
                    seen_sigs_cleanup_counter = 0;
                }

                stats.total_detections.fetch_add(1, Ordering::Relaxed);

                // P1-1 fix: process the buy in a spawned task so the select! loop is
                // never blocked by the buy's .await chain (RPC, sender, signature polling
                // up to 5s). The select! arm now only receives + dedups + dispatches.
                // Slow buys no longer serialize behind one another, so concurrent
                // same-slot detections are all picked up immediately. Arc clones are
                // cheap (atomic refcount bump); Keypairs are shared via Arc<Option<_>>.
                let shared_settings = shared_settings.clone();
                let position_monitor = position_monitor.clone();
                let stats = stats.clone();
                let price_cache = price_cache.clone();
                let rpc_client = rpc_client.clone();
                let blockhash_cache = blockhash_cache.clone();
                let leader_cache = leader_cache.clone();
                let swqos_sender = swqos_sender.clone();
                let cached_fee_recipients = cached_fee_recipients.clone();
                let buy_template = buy_template.clone();
                let buy_keypair = buy_keypair.clone();
                let buy_simulate_keypair = buy_simulate_keypair.clone();
                tokio::spawn(async move {
                // Load latest settings (hot-reloadable)
                if let Err(e) = async {
                let current_settings = shared_settings.load_full();

                let mint = detected.mint.clone();

                // G-CONCERN-1 fix: atomic acquire to prevent TOCTOU race.
                // Two concurrent detections of the same mint could both pass
                // the old `has_position` check before either calls `add_position`,
                // resulting in duplicate buys and a lost position on overwrite.
                // `try_acquire_buy` atomically checks the position map AND
                // reserves a pending-buy slot under one Mutex, so only one
                // task wins the race for a given mint.
                let acquire = position_monitor.try_acquire_buy(
                    &mint,
                    current_settings.allow_accumulate,
                ).await;

                match acquire {
                    position_monitor::AcquireResult::AlreadyHolding => {
                        info!("SKIP: already holding {}", mint);
                        stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                        return Ok::<(), anyhow::Error>(());
                    }
                    position_monitor::AcquireResult::Busy => {
                        info!("SKIP: another buy task is already processing {}", mint);
                        stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                        return Ok::<(), anyhow::Error>(());
                    }
                    position_monitor::AcquireResult::Acquired => {
                        // We won the race — this mint is ours to buy.
                    }
                }

                // Defer release of pending-buy slot regardless of exit path.
                // Only fires when we actually Acquired, not on AlreadyHolding/Busy.
                // Uses tokio::spawn to fire-and-forget the async release so we
                // don't have to sprinkle `.await` before every early return.
                let mint_for_guard = mint.clone();
                let _pending_guard = scopeguard::guard((), |_| {
                    let pm = position_monitor.clone();
                    tokio::spawn(async move {
                        pm.release_buy(&mint_for_guard).await;
                    });
                });

                let already_holding = position_monitor.has_position(&mint).await;

                // Max positions enforcement
                if !already_holding {
                    let count = position_monitor.position_count().await;
                    if count >= current_settings.max_holded_coins {
                        info!("SKIP: max positions reached ({}/{})", count, current_settings.max_holded_coins);
                        stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                        return Ok::<(), anyhow::Error>(());
                    }
                }

                // Fixed buy amount from config (not mirror_percent)
                let sol_to_spend = current_settings.buy_amount;
                if let Some(target_sol) = detected.sol_amount {
                    info!("Target spent {:.4} SOL, we buy fixed {:.4} SOL", target_sol, sol_to_spend);
                }

                info!(
                    "BUY ORDER: {:.4} SOL on {} | mint: {} | sig: {}...{}",
                    sol_to_spend,
                    detected.dex,
                    detected.mint,
                    &detected.signature[..std::cmp::min(16, detected.signature.len())],
                    if already_holding { " (accumulate)" } else { "" },
                );

                // Check if execution is supported for this DEX
                if !detected.dex.is_execution_supported() {
                    warn!("{} trade detected but execution not yet supported — skipping buy for {}", detected.dex, mint);
                    stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                    return Ok::<(), anyhow::Error>(());
                }

                // Get cached blockhash (instant, no RPC call) — pass full CachedBlockhash
                // so buyer.rs can validate expiry before signing
                let cached_bh = blockhash_cache.get().await;

                let buy_start = Instant::now();
                let ws_received_at = detected.ws_received_at;

                // Route to DEX-specific buyer
                let is_raydium = matches!(detected.dex, copy_engine::DexType::RaydiumV4 | copy_engine::DexType::RaydiumCpmm);
                let is_pumpswap = matches!(detected.dex, copy_engine::DexType::PumpFunAmm);
                let buy_result = if is_raydium {
                    // Raydium V4/CPMM: build swap instructions + send via SWQoS/helius
                    let amm_pool = match detected.amm_pool.as_deref() {
                        Some(p) => p,
                        None => {
                            error!("Raydium V4 buy detected but no AMM pool address — skipping");
                            stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                            return Ok::<(), anyhow::Error>(());
                        }
                    };
                    let payer = match buy_keypair.as_ref() {
                        Some(kp) => kp,
                        None => {
                            warn!("Raydium V4 buy requires keypair (dry-run not supported for Raydium)");
                            stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                            return Ok::<(), anyhow::Error>(());
                        }
                    };
                    let raydium_build_result = if detected.dex == copy_engine::DexType::RaydiumCpmm {
                        raydium_cpmm::build_buy_instructions(
                            amm_pool, &mint, sol_to_spend, current_settings.slippage_bps,
                            &payer.pubkey(), &rpc_client, &current_settings,
                        ).await
                    } else {
                        raydium_v4::build_buy_instructions(
                            amm_pool, &mint, sol_to_spend, current_settings.slippage_bps,
                            &payer.pubkey(), &rpc_client, &current_settings,
                        ).await
                    };
                    let dex_name = if detected.dex == copy_engine::DexType::RaydiumCpmm { "CPMM" } else { "V4" };
                    match raydium_build_result {
                        Ok(instructions) => {
                            if !is_real {
                                info!("DRY RUN: Raydium {} buy instructions built for {} ({} instructions)", dex_name, mint, instructions.len());
                                Ok(crate::models::Holding {
                                    amount: 0,
                                    original_amount: 0,
                                    buy_price: 0.0,
                                    buy_time: chrono::Utc::now(),
                                    decimals: 9,
                                    buy_cost_sol: None,
                                    metadata: None,
                                    onchain_raw: None,
                                    onchain: None,
                                    triggered_tp_levels: vec![],
                                    triggered_sl_levels: vec![],
                                    migrated: false,
                                    pending_sell: false,
                                    dex: if detected.dex == copy_engine::DexType::RaydiumCpmm { "raydium_cpmm" } else { "raydium_v4" }.to_string(),
                                    amm_pool: Some(amm_pool.to_string()),
                                    target_buy_tokens: detected.token_amount,
                                    extra_pump_account: None,
                                    pumpswap_accounts: None,
                                    buy_signature: None,
                                    timing_price_ms: buy_start.elapsed().as_millis(),
                                    timing_build_ms: 0,
                                    timing_send_ms: 0,
                                })
                            } else {
                                // Real mode: send via SWQoS or helius
                                let cached_leader = leader_cache.get().await;
                                let client = solana_client::rpc_client::RpcClient::new(&current_settings.solana_rpc_urls[0]);
                                let tx_base64 = crate::helius_sender::build_signed_transaction(
                                    instructions,
                                    payer,
                                    &current_settings,
                                    &client,
                                    cached_bh.as_ref().map(|cb| cb.blockhash),
                                    cached_leader.clone(),
                                ).await.map_err(|e| anyhow::anyhow!("{}", e))?;
                                let signature = if let Some(ref swqos) = swqos_sender {
                                    let (sig, _) = swqos.send_concurrent(&tx_base64, &current_settings, cached_leader).await
                                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                                    sig
                                } else {
                                    // fallback: send via helius endpoint directly
                                    let body = serde_json::json!({
                                        "jsonrpc": "2.0", "id": "1",
                                        "method": "sendTransaction",
                                        "params": [tx_base64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]
                                    });
                                    let resp = crate::rpc::SHARED_HTTP_CLIENT
                                        .post(&current_settings.helius_sender_endpoint)
                                        .json(&body).send().await?;
                                    let json: serde_json::Value = resp.json().await?;
                                    json.get("result").and_then(|r| r.as_str()).unwrap_or("unknown").to_string()
                                };
                                info!("Raydium {} buy TX sent: {}", dex_name, signature);
                                Ok(crate::models::Holding {
                                    amount: 0,
                                    original_amount: 0,
                                    buy_price: 0.0,
                                    buy_time: chrono::Utc::now(),
                                    decimals: 9,
                                    buy_cost_sol: Some(sol_to_spend),
                                    metadata: None,
                                    onchain_raw: None,
                                    onchain: None,
                                    triggered_tp_levels: vec![],
                                    triggered_sl_levels: vec![],
                                    migrated: false,
                                    pending_sell: false,
                                    dex: if detected.dex == copy_engine::DexType::RaydiumCpmm { "raydium_cpmm" } else { "raydium_v4" }.to_string(),
                                    amm_pool: Some(amm_pool.to_string()),
                                    target_buy_tokens: detected.token_amount,
                                    extra_pump_account: None,
                                    pumpswap_accounts: None,
                                    buy_signature: None,
                                    timing_price_ms: 0,
                                    timing_build_ms: buy_start.elapsed().as_millis(),
                                    timing_send_ms: 0,
                                })
                            }
                        }
                        Err(e) => Err(anyhow::anyhow!("{}", e)),
                    }
                } else if is_pumpswap {
                    // PumpSwap AMM: build buy instructions from target's accounts
                    let target_accts = match detected.target_pump_accounts.as_ref() {
                        Some(accts) if accts.len() >= 17 => accts,
                        _ => {
                            error!("PumpSwap buy detected but no target accounts (need 17+) — skipping {}", mint);
                            stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                            return Ok::<(), anyhow::Error>(());
                        }
                    };
                    let payer = match buy_keypair.as_ref() {
                        Some(kp) => kp,
                        None => {
                            warn!("PumpSwap buy requires keypair — skipping {}", mint);
                            stats.total_skipped.fetch_add(1, Ordering::Relaxed);
                            return Ok::<(), anyhow::Error>(());
                        }
                    };
                    let pool = detected.amm_pool.as_deref().unwrap_or(&target_accts[0]);
                    let base_mint_str = &target_accts[3];
                    let quote_mint_str = &target_accts[4];

                    // Estimate token amount: scale proportionally from target's trade
                    let our_sol_lamports = (sol_to_spend * 1e9) as u64;
                    let our_token_amount = if let (Some(target_sol), Some(target_tokens)) = (detected.sol_amount, detected.token_amount) {
                        if target_sol > 0.0 && target_tokens > 0 {
                            let ratio = sol_to_spend / target_sol;
                            let scaled = (target_tokens as f64 * ratio * 0.90) as u64; // 10% safety haircut
                            scaled.max(1)
                        } else {
                            1 // minimum: accept any amount, rely on slippage via max_sol
                        }
                    } else {
                        1 // no target data: accept any amount
                    };

                    let pumpswap_accts = pumpswap::PumpSwapAccounts {
                        accounts: target_accts.clone(),
                        pool: pool.to_string(),
                        base_mint: base_mint_str.clone(),
                        quote_mint: quote_mint_str.clone(),
                        token_amount: our_token_amount,
                        sol_amount: our_sol_lamports,
                    };

                    match pumpswap::build_buy_instructions(
                        &pumpswap_accts, our_sol_lamports, our_token_amount,
                        &payer.pubkey(), current_settings.slippage_bps,
                    ) {
                        Ok(instructions) => {
                            if !is_real {
                                info!("DRY RUN: PumpSwap buy instructions built for {} ({} instructions)", mint, instructions.len());
                                Ok(crate::models::Holding {
                                    amount: 0,
                                    original_amount: 0,
                                    buy_price: 0.0,
                                    buy_time: chrono::Utc::now(),
                                    decimals: 6,
                                    buy_cost_sol: None,
                                    metadata: None,
                                    onchain_raw: None,
                                    onchain: None,
                                    triggered_tp_levels: vec![],
                                    triggered_sl_levels: vec![],
                                    migrated: false,
                                    pending_sell: false,
                                    dex: "pumpswap".to_string(),
                                    amm_pool: Some(pool.to_string()),
                                    target_buy_tokens: detected.token_amount,
                                    extra_pump_account: None,
                                    pumpswap_accounts: Some(target_accts.clone()),
                                    buy_signature: None,
                                    timing_price_ms: buy_start.elapsed().as_millis(),
                                    timing_build_ms: 0,
                                    timing_send_ms: 0,
                                })
                            } else {
                                // Real mode: build+sign+send via SWQoS/helius
                                let cached_leader = leader_cache.get().await;
                                let client = solana_client::rpc_client::RpcClient::new(&current_settings.solana_rpc_urls[0]);
                                let tx_base64 = crate::helius_sender::build_signed_transaction(
                                    instructions,
                                    payer,
                                    &current_settings,
                                    &client,
                                    cached_bh.as_ref().map(|cb| cb.blockhash),
                                    cached_leader.clone(),
                                ).await.map_err(|e| anyhow::anyhow!("{}", e))?;
                                let signature = if let Some(ref swqos) = swqos_sender {
                                    let (sig, _) = swqos.send_concurrent(&tx_base64, &current_settings, cached_leader).await
                                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                                    sig
                                } else {
                                    let body = serde_json::json!({
                                        "jsonrpc": "2.0", "id": "1",
                                        "method": "sendTransaction",
                                        "params": [tx_base64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]
                                    });
                                    let resp = crate::rpc::SHARED_HTTP_CLIENT
                                        .post(&current_settings.helius_sender_endpoint)
                                        .json(&body).send().await?;
                                    let json: serde_json::Value = resp.json().await?;
                                    json.get("result").and_then(|r| r.as_str()).unwrap_or("unknown").to_string()
                                };
                                info!("PumpSwap buy TX sent: {}", signature);
                                Ok(crate::models::Holding {
                                    amount: 0,
                                    original_amount: 0,
                                    buy_price: 0.0,
                                    buy_time: chrono::Utc::now(),
                                    decimals: 6,
                                    buy_cost_sol: Some(sol_to_spend),
                                    metadata: None,
                                    onchain_raw: None,
                                    onchain: None,
                                    triggered_tp_levels: vec![],
                                    triggered_sl_levels: vec![],
                                    migrated: false,
                                    pending_sell: false,
                                    dex: "pumpswap".to_string(),
                                    amm_pool: Some(pool.to_string()),
                                    target_buy_tokens: detected.token_amount,
                                    extra_pump_account: None,
                                    pumpswap_accounts: Some(target_accts.clone()),
                                    buy_signature: Some(signature),
                                    timing_price_ms: 0,
                                    timing_build_ms: buy_start.elapsed().as_millis(),
                                    timing_send_ms: 0,
                                })
                            }
                        }
                        Err(e) => Err(anyhow::anyhow!("{}", e)),
                    }
                } else {
                    // PumpFun bonding curve: existing buyer
                    buyer::buy_token(
                        &mint,
                        sol_to_spend,
                        is_real,
                        buy_keypair.as_ref().as_ref(),
                        buy_simulate_keypair.as_ref().as_ref(),
                        price_cache.clone(),
                        &rpc_client,
                        &current_settings,
                        cached_bh,
                        cached_fee_recipients.as_ref().map(|arc| arc.as_ref()),
                        swqos_sender.as_ref(),
                        detected.sol_amount,
                        detected.token_amount,
                        detected.target_pump_accounts.as_ref(),
                        buy_template.as_ref(),
                        None,
                    ).await.map_err(|e| anyhow::anyhow!("{}", e))
                };

                match buy_result {
                    Ok(mut holding) => {
                        // Store target's buy token amount for proportional mirror sells
                        if holding.target_buy_tokens.is_none() {
                            holding.target_buy_tokens = detected.token_amount;
                        }
                        let buy_ms = buy_start.elapsed().as_millis();
                        let pipeline_ms = ws_received_at.elapsed().as_millis();
                        stats.total_buys.fetch_add(1, Ordering::Relaxed);
                        let target_slot = detected.slot.unwrap_or(0);
                        // Query our TX slot asynchronously (best effort)
                        let our_slot_str = if let Some(ref sig) = holding.buy_signature {
                            match rpc::get_tx_slot(sig, &rpc_client, &current_settings).await {
                                Ok(s) => {
                                    let diff = s as i64 - target_slot as i64;
                                    format!("our_slot={} slot_diff={}", s, diff)
                                },
                                Err(_) => "our_slot=? slot_diff=?".to_string(),
                            }
                        } else {
                            "our_slot=? slot_diff=?".to_string()
                        };
                        info!(
                            "BUY OK: {} | {} tokens | price {:.12} | target_slot={} {} | pipeline={}ms",
                            mint, holding.amount, holding.buy_price,
                            target_slot, our_slot_str, pipeline_ms,
                        );
                        info!(
                            "TIMING: total={}ms price={}ms build={}ms send={}ms | target_tokens={}",
                            buy_ms, holding.timing_price_ms, holding.timing_build_ms, holding.timing_send_ms,
                            holding.target_buy_tokens.map(|t| t.to_string()).unwrap_or_else(|| "?".to_string()),
                        );
                        if let Some(cost) = holding.buy_cost_sol {
                            info!("Actual cost: {:.9} SOL", cost);
                        }
                        if already_holding {
                            // Accumulate into existing position
                            position_monitor.accumulate_position(
                                &mint,
                                holding.amount,
                                holding.buy_price,
                                holding.buy_cost_sol,
                            ).await;
                        } else {
                            // Register new position for TP/SL monitoring
                            position_monitor.add_position(mint.clone(), holding).await;
                        }
                    }
                    Err(e) => {
                        stats.total_buy_failures.fetch_add(1, Ordering::Relaxed);
                        error!("Buy failed for {}: {}", mint, e);
                    }
                }
                    Ok::<(), anyhow::Error>(())
                }.await {
                    error!("Buy task failed: {e}");
                }
                }); // end tokio::spawn
            }

            // Mirror sell: target sold, we sell our entire position of that mint
            detected_sell = mirror_sell_rx.recv(), if current_settings_mirror_sells(&shared_settings) => {
                if let Some(sell) = detected_sell {
                    if position_monitor.has_position(&sell.mint).await {
                        let positions = position_monitor.get_positions().await;
                        if let Some(holding) = positions.get(&sell.mint) {
                            // Calculate proportional sell amount based on target's sell ratio
                            let (sell_amount, sell_ratio) = if let (Some(target_sold), Some(target_bought)) =
                                (sell.token_amount, holding.target_buy_tokens)
                            {
                                if target_bought > 0 {
                                    // Proportional: if target sold 50% of their tokens, we sell 50% of ours
                                    let ratio = (target_sold as f64) / (target_bought as f64);
                                    let ratio = ratio.min(1.0); // cap at 100%
                                    let amount = (holding.amount as f64 * ratio) as u64;
                                    let amount = amount.max(1).min(holding.amount); // at least 1, at most all
                                    (amount, ratio)
                                } else {
                                    (holding.amount, 1.0) // fallback: sell all
                                }
                            } else {
                                (holding.amount, 1.0) // no data: sell all
                            };

                            let is_final = sell_amount >= holding.amount;

                            info!(
                                "MIRROR SELL: target sold {} | ratio {:.1}% | our sell: {} / {} tokens | sig: {}...{}",
                                sell.mint,
                                sell_ratio * 100.0,
                                sell_amount,
                                holding.amount,
                                &sell.signature[..std::cmp::min(16, sell.signature.len())],
                                if is_final { " (FINAL)" } else { "" },
                            );

                            let order = SellOrder {
                                mint: sell.mint.clone(),
                                amount: sell_amount,
                                current_price: holding.buy_price,
                                decimals: holding.decimals,
                                reason: SellReason::MirrorSell,
                                level_index: 0,
                                is_final,
                                dex: holding.dex.clone(),
                                amm_pool: holding.amm_pool.clone(),
                                extra_pump_account: holding.extra_pump_account.clone(),
                                pumpswap_accounts: holding.pumpswap_accounts.clone(),
                            };
                            if let Err(e) = mirror_sell_order_tx.send(order).await {
                                error!("Failed to queue mirror sell for {}: {}", sell.mint, e);
                            }
                        }
                    } else {
                        debug!("SKIP mirror sell: no longer holding {}", sell.mint);
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down...");
                break;
            }
        }
    }

    // Print session statistics on shutdown
    shutdown_stats.log_summary();

    // Wait briefly for background tasks to notice channels are closed
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        async { let _ = tokio::join!(listener_handle, engine_handle, sell_handle); },
    ).await;

    info!("Shutdown complete.");
    Ok(())
}

fn log_config_summary(settings: &Settings, is_real: bool) {
    info!("--- Configuration Summary ---");
    info!("  Mode:            {}", if is_real { "REAL" } else { "DRY RUN" });
    info!("  Target wallet:   {}", settings.target_wallet.as_deref().unwrap_or("(built-in fallback)"));
    info!("  Buy amount:      {:.4} SOL", settings.buy_amount);
    info!("  Slippage:        {} bps ({:.1}%)", settings.slippage_bps, settings.slippage_bps as f64 / 100.0);
    info!("  Max positions:   {}", settings.max_holded_coins);

    // TP levels
    for (i, tp) in settings.tp_levels.iter().enumerate() {
        info!("  TP level {}:      {:+.1}% -> sell {:.0}%", i + 1, tp.trigger_percent, tp.sell_percent);
    }

    // SL levels
    for (i, sl) in settings.sl_levels.iter().enumerate() {
        info!("  SL level {}:      {:+.1}% -> sell {:.0}%", i + 1, sl.trigger_percent, sl.sell_percent);
    }

    // Safety
    if settings.enable_safer_sniping {
        info!("  Safer sniping:   ON (min tokens: {}, max price: {:.12} SOL, liquidity: {:.1}-{:.1} SOL)",
              settings.min_tokens_threshold, settings.max_sol_per_token,
              settings.min_liquidity_sol, settings.max_liquidity_sol);
    } else {
        info!("  Safer sniping:   OFF");
    }

    // RPC
    for (i, url) in settings.solana_rpc_urls.iter().enumerate() {
        info!("  RPC [{}]:         {}", i, url);
    }
    for (i, url) in settings.solana_ws_urls.iter().enumerate() {
        info!("  WSS [{}]:         {}", i, url);
    }

    // Helius / Jito
    info!("  Helius sender:   {}", if settings.helius_sender_enabled { "ON" } else { "OFF" });
    if settings.helius_sender_enabled {
        info!("  Helius endpoint: {}", settings.helius_sender_endpoint);
        info!("  Routing:         {}", if settings.helius_use_swqos_only { "SWQOS-only" } else { "dual (validators + Jito)" });
        info!("  Min tip:         {:.9} SOL", settings.helius_min_tip_sol);
        info!("  Dynamic tips:    {}", if settings.helius_use_dynamic_tips { "ON" } else { "OFF" });
        info!("  Priority fee x:  {:.1}", settings.helius_priority_fee_multiplier);
    }

    // Mirror trading
    if settings.mirror_percent > 0.0 {
        info!("  Mirror mode:     {:.1}% of target", settings.mirror_percent);
    } else {
        info!("  Mirror mode:     OFF (fixed buy_amount)");
    }
    info!("  Accumulate:      {}", if settings.allow_accumulate { "ON" } else { "OFF" });
    info!("  Mirror sells:    {}", if settings.mirror_sells { "ON" } else { "OFF" });
    if settings.max_position_sol > 0.0 {
        info!("  Max position:    {:.4} SOL", settings.max_position_sol);
    }

    // Listener
    info!("  Listener mode:   {}", settings.listener_mode);
    if settings.listener_mode == "shreds" {
        info!("  Shreds URL:      {}", settings.shreds_url);
    }
    if settings.listener_mode == "geyser" {
        info!("  Geyser URL:      {}", settings.geyser_url);
    }

    // Pump.fun
    info!("  Pump program:    {}", settings.pump_fun_program);
    info!("  Token decimals:  {} (default)", settings.default_token_decimals);
    info!("-----------------------------");
}

/// Helper to check mirror_sells from ArcSwap without binding a local variable in select!
fn current_settings_mirror_sells(shared: &Arc<ArcSwap<Settings>>) -> bool {
    shared.load().mirror_sells
}

/// Sell executor loop — receives SellOrders from position monitor and executes sells
async fn run_sell_executor(
    sell_rx: &mut mpsc::Receiver<SellOrder>,
    is_real: bool,
    keypair: Option<&Keypair>,
    simulate_keypair: Option<&Keypair>,
    rpc_client: &Arc<RpcClient>,
    shared_settings: &Arc<ArcSwap<Settings>>,
    monitor: &PositionMonitor,
    stats: &Stats,
    cached_fee_recipients: Option<&buyer::CachedFeeRecipients>,
    swqos_sender: Option<&Arc<swqos_sender::SwqosSender>>,
    leader_cache: &LeaderCache,
) {
    while let Some(order) = sell_rx.recv().await {
        // Skip if position was already fully exited by a prior sell (prevents double-sells)
        if !monitor.has_position(&order.mint).await {
            debug!("SKIP {} sell: no longer holding {}", order.reason, order.mint);
            continue;
        }

        info!(
            "SELL ORDER: {} | {} tokens | {} | price {:.12} SOL/token",
            order.mint, order.amount, order.reason, order.current_price
        );

        // Load latest settings snapshot for this sell
        let settings = shared_settings.load_full();

        let sell_start = Instant::now();

        // Route sell by DEX
        let is_raydium_sell = order.dex == "raydium_v4" || order.dex == "raydium_cpmm";
        let sell_result = if is_raydium_sell {
            // Raydium V4/CPMM sell
            let amm_pool = match order.amm_pool.as_deref() {
                Some(p) => p,
                None => {
                    error!("Raydium V4 sell for {} but no amm_pool stored — skipping", order.mint);
                    stats.total_sell_failures.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let payer = match keypair {
                Some(kp) => kp,
                None => {
                    warn!("Raydium V4 sell requires keypair — skipping {}", order.mint);
                    stats.total_sell_failures.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let sell_build = if order.dex == "raydium_cpmm" {
                raydium_cpmm::build_sell_instructions(
                    amm_pool, &order.mint, order.amount, settings.slippage_bps,
                    &payer.pubkey(), rpc_client, &settings,
                ).await
            } else {
                raydium_v4::build_sell_instructions(
                    amm_pool, &order.mint, order.amount, settings.slippage_bps,
                    &payer.pubkey(), rpc_client, &settings,
                ).await
            };
            let dex_label = if order.dex == "raydium_cpmm" { "CPMM" } else { "V4" };
            match sell_build {
                Ok(instructions) => {
                    if !is_real {
                        info!("DRY RUN: Raydium {} sell instructions built for {} ({} instructions)", dex_label, order.mint, instructions.len());
                        Ok(rpc::SellResult { sol_balance_change: None, tx_fee_sol: None })
                    } else {
                        let cached_leader = leader_cache.get().await;
                        let client = RpcClient::new(&settings.solana_rpc_urls[0]);
                        match crate::helius_sender::build_signed_transaction(
                            instructions, payer, &settings, &client, None, cached_leader.clone(),
                        ).await {
                            Ok(tx_base64) => {
                                let sig = if let Some(ref swqos) = swqos_sender {
                                    match swqos.send_concurrent(&tx_base64, &settings, cached_leader).await {
                                        Ok((s, _)) => s,
                                        Err(e) => { error!("Raydium {} sell send failed for {}: {}", dex_label, order.mint, e); continue; }
                                    }
                                } else {
                                    let body = serde_json::json!({
                                        "jsonrpc": "2.0", "id": "1",
                                        "method": "sendTransaction",
                                        "params": [tx_base64, {"encoding": "base64", "skipPreflight": true}]
                                    });
                                    let resp = match crate::rpc::SHARED_HTTP_CLIENT
                                        .post(&settings.helius_sender_endpoint)
                                        .json(&body).send().await {
                                        Ok(r) => r,
                                        Err(e) => { error!("Raydium V4 sell HTTP failed: {}", e); continue; }
                                    };
                                    let json: serde_json::Value = match resp.json().await {
                                        Ok(j) => j,
                                        Err(e) => { error!("Raydium V4 sell parse failed: {}", e); continue; }
                                    };
                                    json.get("result").and_then(|r| r.as_str()).unwrap_or("unknown").to_string()
                                };
                                info!("Raydium {} sell TX sent for {}: {}", dex_label, order.mint, sig);
                                Ok(rpc::SellResult { sol_balance_change: None, tx_fee_sol: None })
                            }
                            Err(e) => Err(format!("Failed to build Raydium V4 sell TX: {}", e)),
                        }
                    }
                }
                Err(e) => Err(format!("Raydium V4 sell instruction build failed: {}", e)),
            }
        } else if order.dex == "pumpswap" {
            // PumpSwap AMM sell: reconstruct PumpSwapAccounts from stored accounts
            let stored_accts = match order.pumpswap_accounts.as_ref() {
                Some(accts) if accts.len() >= 17 => accts,
                _ => {
                    error!("PumpSwap sell for {} but no stored accounts (need 17+) — skipping", order.mint);
                    stats.total_sell_failures.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let payer = match keypair {
                Some(kp) => kp,
                None => {
                    warn!("PumpSwap sell requires keypair — skipping {}", order.mint);
                    stats.total_sell_failures.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            let pumpswap_accts = pumpswap::PumpSwapAccounts {
                accounts: stored_accts.clone(),
                pool: stored_accts[0].clone(),
                base_mint: stored_accts[3].clone(),
                quote_mint: stored_accts[4].clone(),
                token_amount: order.amount,
                sol_amount: 0,
            };

            // min_sol_out with slippage: use current_price * amount * (1 - slippage)
            let min_sol_out = {
                let gross = order.current_price * (order.amount as f64);
                let after_slippage = gross * (1.0 - settings.slippage_bps as f64 / 10000.0);
                (after_slippage * 1e9) as u64
            };

            match pumpswap::build_sell_instructions(
                &pumpswap_accts, order.amount, min_sol_out, &payer.pubkey(),
            ) {
                Ok(instructions) => {
                    if !is_real {
                        info!("DRY RUN: PumpSwap sell instructions built for {} ({} instructions)", order.mint, instructions.len());
                        Ok(rpc::SellResult { sol_balance_change: None, tx_fee_sol: None })
                    } else {
                        let cached_leader = leader_cache.get().await;
                        let client = RpcClient::new(&settings.solana_rpc_urls[0]);
                        match crate::helius_sender::build_signed_transaction(
                            instructions, payer, &settings, &client, None, cached_leader.clone(),
                        ).await {
                            Ok(tx_base64) => {
                                let sig = if let Some(ref swqos) = swqos_sender {
                                    match swqos.send_concurrent(&tx_base64, &settings, cached_leader).await {
                                        Ok((s, _)) => s,
                                        Err(e) => { error!("PumpSwap sell send failed for {}: {}", order.mint, e); continue; }
                                    }
                                } else {
                                    let body = serde_json::json!({
                                        "jsonrpc": "2.0", "id": "1",
                                        "method": "sendTransaction",
                                        "params": [tx_base64, {"encoding": "base64", "skipPreflight": true}]
                                    });
                                    let resp = match crate::rpc::SHARED_HTTP_CLIENT
                                        .post(&settings.helius_sender_endpoint)
                                        .json(&body).send().await {
                                        Ok(r) => r,
                                        Err(e) => { error!("PumpSwap sell HTTP failed for {}: {}", order.mint, e); continue; }
                                    };
                                    let json: serde_json::Value = match resp.json().await {
                                        Ok(j) => j,
                                        Err(e) => { error!("PumpSwap sell parse failed for {}: {}", order.mint, e); continue; }
                                    };
                                    json.get("result").and_then(|r| r.as_str()).unwrap_or("unknown").to_string()
                                };
                                info!("PumpSwap sell TX sent for {}: {}", order.mint, sig);
                                Ok(rpc::SellResult { sol_balance_change: None, tx_fee_sol: None })
                            }
                            Err(e) => Err(format!("Failed to build PumpSwap sell TX: {}", e)),
                        }
                    }
                }
                Err(e) => Err(format!("PumpSwap sell instruction build failed for {}: {}", order.mint, e)),
            }
        } else {
            // PumpFun bonding curve sell (existing path)
            rpc::sell_token(
                &order.mint,
                order.amount,
                order.current_price,
                order.decimals,
                is_real,
                keypair,
                simulate_keypair,
                rpc_client,
                &settings,
                order.is_final,
                cached_fee_recipients,
                swqos_sender,
                order.extra_pump_account.as_deref(),
            ).await.map_err(|e| format!("{}", e))
        };

        match sell_result {
            Ok(result) => {
                let sell_ms = sell_start.elapsed().as_millis();
                stats.total_sells.fetch_add(1, Ordering::Relaxed);
                if let Some(sol_change) = result.sol_balance_change {
                    stats.add_pnl_sol(sol_change);
                    info!("Sell executed for {}: received {:.9} SOL | TIMING sell: {}ms", order.mint, sol_change, sell_ms);
                } else {
                    info!("Sell executed for {} (dry-run) | TIMING sell: {}ms", order.mint, sell_ms);
                }
                // Update position in monitor
                monitor.update_after_sell(
                    &order.mint,
                    order.amount,
                    &order.reason,
                    order.level_index,
                ).await;
            }
            Err(e) => {
                stats.total_sell_failures.fetch_add(1, Ordering::Relaxed);
                error!("Sell failed for {}: {}", order.mint, e);
                // Clear pending_sell so monitor can retry on next tick
                monitor.clear_pending_sell(&order.mint).await;
            }
        }
    }
    warn!("Sell executor channel closed");
}

/// Watch config.toml for changes and hot-reload settings via ArcSwap
async fn run_config_watcher(config_path: String, shared_settings: Arc<ArcSwap<Settings>>) {
    use notify::{Watcher, RecursiveMode, Event, EventKind};

    let (notify_tx, mut notify_rx) = mpsc::channel::<()>(10);

    // Debounce: only reload after writes settle
    let mut watcher = match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                let _ = notify_tx.blocking_send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            error!("Failed to create config watcher: {}", e);
            return;
        }
    };

    let watch_path = std::path::Path::new(&config_path);
    if let Err(e) = watcher.watch(watch_path, RecursiveMode::NonRecursive) {
        error!("Failed to watch {}: {}", config_path, e);
        return;
    }
    info!("Config watcher started for {}", config_path);

    // Debounce: wait 500ms after last event before reloading
    loop {
        if notify_rx.recv().await.is_none() {
            break;
        }
        // Drain any rapid-fire events and wait for them to settle
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        while notify_rx.try_recv().is_ok() {}

        // Reload settings
        match Settings::from_file(&config_path) {
            Ok(new_settings) => {
                if let Err(e) = new_settings.validate() {
                    warn!("Config reload validation failed (keeping old settings): {}", e);
                    continue;
                }
                let old = shared_settings.load();
                log_settings_diff(&old, &new_settings);
                shared_settings.store(Arc::new(new_settings));
                info!("Config hot-reloaded from {}", config_path);
            }
            Err(e) => {
                warn!("Config reload failed (keeping old settings): {}", e);
            }
        }
    }
}

/// Log which settings changed between old and new
fn log_settings_diff(old: &Settings, new: &Settings) {
    if (old.buy_amount - new.buy_amount).abs() > 1e-12 {
        info!("  Config changed: buy_amount {:.4} -> {:.4}", old.buy_amount, new.buy_amount);
    }
    if (old.mirror_percent - new.mirror_percent).abs() > 1e-12 {
        info!("  Config changed: mirror_percent {:.1} -> {:.1}", old.mirror_percent, new.mirror_percent);
    }
    if old.allow_accumulate != new.allow_accumulate {
        info!("  Config changed: allow_accumulate {} -> {}", old.allow_accumulate, new.allow_accumulate);
    }
    if old.mirror_sells != new.mirror_sells {
        info!("  Config changed: mirror_sells {} -> {}", old.mirror_sells, new.mirror_sells);
    }
    if old.max_holded_coins != new.max_holded_coins {
        info!("  Config changed: max_holded_coins {} -> {}", old.max_holded_coins, new.max_holded_coins);
    }
    if old.slippage_bps != new.slippage_bps {
        info!("  Config changed: slippage_bps {} -> {}", old.slippage_bps, new.slippage_bps);
    }
    if old.tp_levels != new.tp_levels {
        info!("  Config changed: tp_levels {:?} -> {:?}", old.tp_levels, new.tp_levels);
    }
    if old.sl_levels != new.sl_levels {
        info!("  Config changed: sl_levels {:?} -> {:?}", old.sl_levels, new.sl_levels);
    }
    if old.listener_mode != new.listener_mode {
        info!("  Config changed: listener_mode {} -> {} (requires restart)", old.listener_mode, new.listener_mode);
    }
    if old.shreds_url != new.shreds_url {
        info!("  Config changed: shreds_url {} -> {} (requires restart)", old.shreds_url, new.shreds_url);
    }
}

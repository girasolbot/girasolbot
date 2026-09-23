use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use arc_swap::ArcSwap;
use log::{info, warn, debug, error};
use tokio::sync::{mpsc, RwLock};
use solana_client::rpc_client::RpcClient;
use tokio::sync::Mutex;

use crate::models::{Holding, PriceCache};
use crate::settings::Settings;

const POSITIONS_FILE: &str = "positions.json";

/// Result of `try_acquire_buy` — atomic race-condition guard for concurrent buys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireResult {
    /// Mint already has an open position (skip unless allow_accumulate)
    AlreadyHolding,
    /// Mint atomically reserved — caller may proceed with buy
    Acquired,
    /// Another task is already buying this mint — skip
    Busy,
}

/// A sell order queued by the position monitor when TP/SL triggers
#[derive(Debug, Clone)]
pub struct SellOrder {
    pub mint: String,
    /// Token amount to sell (in base units)
    pub amount: u64,
    /// Current price at trigger time (SOL per token)
    pub current_price: f64,
    /// Token decimals
    pub decimals: u8,
    /// Reason for the sell
    pub reason: SellReason,
    /// Index of the TP/SL level that triggered
    pub level_index: usize,
    /// True if this sell should close the ATA (position fully exited)
    pub is_final: bool,
    /// Which DEX this position was bought on ("pumpfun", "raydium_v4")
    pub dex: String,
    /// AMM pool address (for Raydium V4 sells)
    pub amm_pool: Option<String>,
    /// Extra pump account (account [16] from buy, used as [14] in sell)
    pub extra_pump_account: Option<String>,
    /// PumpSwap AMM account keys for sell (17+ accounts from target's buy TX)
    pub pumpswap_accounts: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub enum SellReason {
    TakeProfit { pnl_percent: f64 },
    StopLoss { pnl_percent: f64 },
    MirrorSell,
}

impl std::fmt::Display for SellReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SellReason::TakeProfit { pnl_percent } => write!(f, "TP ({:+.1}%)", pnl_percent),
            SellReason::StopLoss { pnl_percent } => write!(f, "SL ({:+.1}%)", pnl_percent),
            SellReason::MirrorSell => write!(f, "MIRROR (target sold)"),
        }
    }
}

/// Monitors open positions and triggers TP/SL sells
#[derive(Clone)]
pub struct PositionMonitor {
    positions: Arc<RwLock<HashMap<String, Holding>>>,
    /// Mints currently being bought (race condition guard).
    /// When a buy task starts, it atomically inserts its mint here.
    /// If the insert returns false (already present), another task is already buying → skip.
    /// On buy completion (success or failure), the mint is removed.
    pending_buys: Arc<Mutex<HashSet<String>>>,
    persist_path: PathBuf,
}

impl PositionMonitor {
    /// Create a new monitor and spawn the background check loop.
    /// Loads any previously persisted positions from disk before starting.
    /// Also performs startup validation: removes positions with 0 on-chain tokens.
    pub fn start(
        sell_tx: mpsc::Sender<SellOrder>,
        price_cache: Arc<Mutex<PriceCache>>,
        rpc_client: Arc<RpcClient>,
        settings: Arc<ArcSwap<Settings>>,
        payer_pubkey: Option<solana_sdk::pubkey::Pubkey>,
    ) -> Self {
        let loaded = load_positions_from_file(POSITIONS_FILE);
        let total_loaded = loaded.len();
        if !loaded.is_empty() {
            info!("Restored {} position(s) from {}", loaded.len(), POSITIONS_FILE);
            for (mint, h) in &loaded {
                info!(
                    "  Resumed: {} | {} tokens | buy price {:.12} SOL/token | TP triggered {:?} | SL triggered {:?}",
                    mint, h.amount, h.buy_price, h.triggered_tp_levels, h.triggered_sl_levels
                );
            }
        }

        let positions: Arc<RwLock<HashMap<String, Holding>>> =
            Arc::new(RwLock::new(loaded));
        let persist_path = PathBuf::from(POSITIONS_FILE);
        let monitor = PositionMonitor {
            positions: positions.clone(),
            pending_buys: Arc::new(Mutex::new(HashSet::new())),
            persist_path: persist_path.clone(),
        };

        // Startup validation: remove positions with 0 on-chain tokens
        if let Some(payer) = payer_pubkey {
            let positions_clone = positions.clone();
            let rpc_clone = rpc_client.clone();
            let settings_clone = settings.clone();
            let persist_for_validation = persist_path.clone();
            tokio::spawn(async move {
                validate_positions_on_startup(positions_clone, rpc_clone, settings_clone, payer, persist_for_validation).await;
            });
        }

        tokio::spawn(async move {
            monitor_loop(positions, sell_tx, price_cache, rpc_client, settings, payer_pubkey, persist_path).await;
        });

        monitor
    }

    /// Add a new position after a successful buy
    pub async fn add_position(&self, mint: String, holding: Holding) {
        info!(
            "Position opened: {} | {} tokens | buy price {:.12} SOL/token",
            mint, holding.amount, holding.buy_price
        );
        self.positions.write().await.insert(mint, holding);
        self.persist().await;
    }

    /// Remove a position (fully exited)
    pub async fn remove_position(&self, mint: &str) {
        if self.positions.write().await.remove(mint).is_some() {
            info!("Position closed: {}", mint);
            self.persist().await;
        }
    }

    /// Clear the pending_sell flag after a sell completes or fails.
    /// This allows the monitor to queue new sell orders for this position.
    pub async fn clear_pending_sell(&self, mint: &str) {
        let mut positions = self.positions.write().await;
        if let Some(holding) = positions.get_mut(mint) {
            holding.pending_sell = false;
        }
    }

    /// Update a position after a partial sell (reduce amount, mark level triggered)
    pub async fn update_after_sell(&self, mint: &str, sold_amount: u64, reason: &SellReason, level_index: usize) {
        let mut positions = self.positions.write().await;
        if let Some(holding) = positions.get_mut(mint) {
            holding.pending_sell = false; // sell completed, allow new orders
            holding.amount = holding.amount.saturating_sub(sold_amount);
            match reason {
                SellReason::TakeProfit { .. } => {
                    if !holding.triggered_tp_levels.contains(&level_index) {
                        holding.triggered_tp_levels.push(level_index);
                    }
                }
                SellReason::StopLoss { .. } => {
                    if !holding.triggered_sl_levels.contains(&level_index) {
                        holding.triggered_sl_levels.push(level_index);
                    }
                }
                SellReason::MirrorSell => {
                    // Full exit, no level tracking needed
                }
            }
            if holding.amount == 0 {
                info!("Position fully exited: {}", mint);
                positions.remove(mint);
            } else {
                info!("Position reduced: {} | {} tokens remaining", mint, holding.amount);
            }
        }
        // Persist while still holding the lock to avoid races
        save_positions_to_file(&positions, &self.persist_path);
    }

    /// Check if a position already exists for the given mint
    pub async fn has_position(&self, mint: &str) -> bool {
        self.positions.read().await.contains_key(mint)
    }

    /// Accumulate into an existing position (add tokens from another buy)
    pub async fn accumulate_position(&self, mint: &str, additional_amount: u64, new_buy_price: f64, additional_cost: Option<f64>) {
        let mut positions = self.positions.write().await;
        if let Some(holding) = positions.get_mut(mint) {
            // Weighted average buy price
            let old_total = holding.amount as f64 * holding.buy_price;
            let new_total = additional_amount as f64 * new_buy_price;
            let combined_amount = holding.amount + additional_amount;
            if combined_amount > 0 {
                holding.buy_price = (old_total + new_total) / combined_amount as f64;
            }
            holding.amount = combined_amount;
            holding.original_amount = combined_amount;
            // Accumulate cost
            if let Some(cost) = additional_cost {
                holding.buy_cost_sol = Some(holding.buy_cost_sol.unwrap_or(0.0) + cost);
            }
            // Note: do NOT clear triggered_tp_levels / triggered_sl_levels on accumulation.
            // The position size changed but already-triggered levels should remain marked
            // to prevent re-triggering. Only new levels that weren't triggered before
            // should be eligible to trigger.
            info!(
                "Position accumulated: {} | {} tokens | avg price {:.12} SOL/token",
                mint, holding.amount, holding.buy_price
            );
        }
        save_positions_to_file(&positions, &self.persist_path);
    }

    /// Get a snapshot of all current positions
    pub async fn get_positions(&self) -> HashMap<String, Holding> {
        self.positions.read().await.clone()
    }

    /// Get current number of open positions
    pub async fn position_count(&self) -> usize {
        self.positions.read().await.len()
    }

    /// Attempt to reserve a mint for buying. Returns `AcquireResult`.
    ///
    /// This is the **race-condition guard** for the TOCTOU bug between
    /// `has_position()` and `add_position()`. Without it, two concurrent
    /// buy tasks for the same mint can both see `already_holding = false`,
    /// both execute the buy, and the second `add_position` overwrites the first.
    ///
    /// The method is atomic: it acquires `pending_buys` (Mutex) and checks
    /// `positions` (RwLock read) under the same await suspension, so only one
    /// task can win the race for a given mint.
    ///
    /// - `AlreadyHolding` → mint already has a position (skip unless allow_accumulate)
    /// - `Acquired`       → mint atomically reserved, caller may proceed with buy
    /// - `Busy`           → another task is already buying this mint, skip
    pub async fn try_acquire_buy(&self, mint: &str, allow_accumulate: bool) -> AcquireResult {
        // Acquire the pending_buys mutex FIRST — it serializes all racing
        // try_acquire_buy callers for the same mint. Any positions check has
        // to happen WHILE holding this mutex, otherwise another task can
        // call add_position in the window between our read and our insert.
        //
        // P1 TOCTOU fix: previously this method did
        //   read positions → drop read lock → take pending mutex → insert
        // Two concurrent callers could both observe `already_holding=false`,
        // both pass the early return, then both insert into pending_buys
        // (different mints would race fine, but for the *same* mint the
        // second call's `insert` returns false → AcquireResult::Busy is
        // returned even though the FIRST caller had already moved past the
        // positions check on stale data). More importantly, between the
        // read-lock drop and the pending insert, add_position can land for
        // this very mint from a previous concurrent acquire, and we'd
        // happily acquire again instead of returning AlreadyHolding.
        let mut pending = self.pending_buys.lock().await;

        // Re-check positions under the mutex so the AlreadyHolding decision
        // is atomic with the Busy/Acquired decision.
        let already_holding = self.positions.read().await.contains_key(mint);
        if already_holding && !allow_accumulate {
            return AcquireResult::AlreadyHolding;
        }

        if !pending.insert(mint.to_string()) {
            // insert returned false → another task already holds the pending slot
            return AcquireResult::Busy;
        }
        AcquireResult::Acquired
    }

    /// Release a mint from the pending-buys set after the buy completes
    /// (success or failure). Must be called for every `try_acquire_buy`
    /// that returned `AcquireResult::Acquired`, otherwise the mint stays
    /// blocked forever.
    pub async fn release_buy(&self, mint: &str) {
        let mut pending = self.pending_buys.lock().await;
        pending.remove(mint);
    }

    /// Save current positions to disk
    async fn persist(&self) {
        let positions = self.positions.read().await;
        save_positions_to_file(&positions, &self.persist_path);
    }
}

/// Load positions from a JSON file. Returns empty map on any error.
fn load_positions_from_file(path: &str) -> HashMap<String, Holding> {
    let path = Path::new(path);
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return HashMap::new();
            }
            match serde_json::from_str::<HashMap<String, Holding>>(&content) {
                Ok(map) => map,
                Err(e) => {
                    warn!("Failed to parse {}: {} — starting with empty positions", path.display(), e);
                    HashMap::new()
                }
            }
        }
        Err(e) => {
            warn!("Failed to read {}: {}", path.display(), e);
            HashMap::new()
        }
    }
}

/// Atomically save positions to a JSON file (write to .tmp then rename).
fn save_positions_to_file(positions: &HashMap<String, Holding>, path: &Path) {
    let tmp_path = path.with_extension("json.tmp");
    match serde_json::to_string_pretty(positions) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&tmp_path, &json) {
                error!("Failed to write {}: {}", tmp_path.display(), e);
                return;
            }
            if let Err(e) = std::fs::rename(&tmp_path, path) {
                error!("Failed to rename {} -> {}: {}", tmp_path.display(), path.display(), e);
            } else {
                debug!("Positions saved to {} ({} position(s))", path.display(), positions.len());
            }
        }
        Err(e) => {
            error!("Failed to serialize positions: {}", e);
        }
    }
}

async fn monitor_loop(
    positions: Arc<RwLock<HashMap<String, Holding>>>,
    sell_tx: mpsc::Sender<SellOrder>,
    price_cache: Arc<Mutex<PriceCache>>,
    rpc_client: Arc<RpcClient>,
    shared_settings: Arc<ArcSwap<Settings>>,
    payer_pubkey: Option<solana_sdk::pubkey::Pubkey>,
    persist_path: PathBuf,
) {
    let mut tp_sl_interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
    // Revalidation interval: default 300s (5 min), 0 = disabled
    // Start with a reasonable default; will be adjusted on each tick based on settings
    let mut revalidate_interval = tokio::time::interval(tokio::time::Duration::from_secs(300));

    loop {
        tokio::select! {
            _ = tp_sl_interval.tick() => {
                // Load latest settings each tick (supports hot-reload)
                let settings = shared_settings.load_full();

                // Snapshot current positions
                let snapshot: Vec<(String, Holding)> = {
                    let pos = positions.read().await;
                    if pos.is_empty() {
                        continue;
                    }
                    pos.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                };

                debug!("Monitoring {} open position(s)", snapshot.len());

                for (mint, holding) in &snapshot {
                    // Skip migrated tokens (bonding curve complete, no longer priceable via curve)
                    if holding.migrated {
                        continue;
                    }
        
                    // Skip positions with a sell already in-flight (prevents duplicate sell spam)
                    if holding.pending_sell {
                        debug!("Skipping {} — sell already in-flight", &mint[..mint.len().min(12)]);
                        continue;
                    }
        
                    // Fetch current price
                    let current_price = match crate::rpc::fetch_current_price(
                        mint,
                        &price_cache,
                        &rpc_client,
                        &settings,
                    ).await {
                        Ok(p) => {
                            if p <= 0.0 {
                                // Zero/negative price likely means bonding curve drained or migrated
                                mark_migrated(&positions, mint).await;
                                continue;
                            }
                            p
                        }
                        Err(e) => {
                            let err_str = format!("{}", e);
                            // Detect migration signals: complete=true, reserves=0, account not found
                            if err_str.contains("complete")
                                || err_str.contains("migrated")
                                || err_str.contains("reserves")
                                || err_str.contains("not found")
                                || err_str.contains("no data")
                            {
                                mark_migrated(&positions, mint).await;
                            } else {
                                debug!("Failed to fetch price for {}: {}", mint, e);
                            }
                            continue;
                        }
                    };
        
                    // Compute PnL
                    if holding.buy_price <= 0.0 {
                        continue;
                    }
                    let pnl_percent = (current_price - holding.buy_price) / holding.buy_price * 100.0;
        
                    debug!(
                        "Position {}: price {:.12} -> {:.12} SOL/token | PnL: {:+.2}%",
                        &mint[..mint.len().min(12)],
                        holding.buy_price,
                        current_price,
                        pnl_percent
                    );
        
                    // Check take-profit levels
                    for (i, tp) in settings.tp_levels.iter().enumerate() {
                        if holding.triggered_tp_levels.contains(&i) {
                            continue;
                        }
                        if pnl_percent >= tp.trigger_percent {
                            let sell_amount = ((tp.sell_percent / 100.0) * holding.original_amount as f64) as u64;
                            if sell_amount == 0 {
                                continue;
                            }
                            // Clamp to remaining amount
                            let sell_amount = sell_amount.min(holding.amount);
                            let remaining_after = holding.amount.saturating_sub(sell_amount);
                            let is_final = remaining_after == 0;
        
                            info!(
                                "TP TRIGGERED [level {}]: {} | PnL {:+.2}% >= {:+.1}% | selling {} tokens ({:.0}% of original)",
                                i, mint, pnl_percent, tp.trigger_percent, sell_amount, tp.sell_percent
                            );
        
                            let order = SellOrder {
                                mint: mint.clone(),
                                amount: sell_amount,
                                current_price,
                                decimals: holding.decimals,
                                reason: SellReason::TakeProfit { pnl_percent },
                                level_index: i,
                                is_final,
                                dex: holding.dex.clone(),
                                amm_pool: holding.amm_pool.clone(),
                                extra_pump_account: holding.extra_pump_account.clone(),
                                pumpswap_accounts: holding.pumpswap_accounts.clone(),
                            };
                            if let Err(e) = sell_tx.send(order).await {
                                error!("Failed to queue TP sell for {}: {}", mint, e);
                            } else {
                                // Mark as pending to prevent duplicate sells on next tick
                                set_pending_sell(&positions, mint).await;
                            }
                            // Only trigger one level per tick to avoid race conditions
                            break;
                        }
                    }
        
                    // Check stop-loss levels
                    for (i, sl) in settings.sl_levels.iter().enumerate() {
                        if holding.triggered_sl_levels.contains(&i) {
                            continue;
                        }
                        // sl.trigger_percent is negative (e.g. -20.0)
                        if pnl_percent <= sl.trigger_percent {
                            let sell_amount = ((sl.sell_percent / 100.0) * holding.original_amount as f64) as u64;
                            if sell_amount == 0 {
                                continue;
                            }
                            let sell_amount = sell_amount.min(holding.amount);
                            let remaining_after = holding.amount.saturating_sub(sell_amount);
                            let is_final = remaining_after == 0;
        
                            info!(
                                "SL TRIGGERED [level {}]: {} | PnL {:+.2}% <= {:+.1}% | selling {} tokens ({:.0}% of original)",
                                i, mint, pnl_percent, sl.trigger_percent, sell_amount, sl.sell_percent
                            );
        
                            let order = SellOrder {
                                mint: mint.clone(),
                                amount: sell_amount,
                                current_price,
                                decimals: holding.decimals,
                                reason: SellReason::StopLoss { pnl_percent },
                                level_index: i,
                                is_final,
                                dex: holding.dex.clone(),
                                amm_pool: holding.amm_pool.clone(),
                                extra_pump_account: holding.extra_pump_account.clone(),
                                pumpswap_accounts: holding.pumpswap_accounts.clone(),
                            };
                            if let Err(e) = sell_tx.send(order).await {
                                error!("Failed to queue SL sell for {}: {}", mint, e);
                            } else {
                                set_pending_sell(&positions, mint).await;
                            }
                            break;
                        }
                    }
                }
            }

            // Periodic revalidation: verify all positions' on-chain token balances
            _ = revalidate_interval.tick() => {
                let settings = shared_settings.load_full();
                // Revalidation interval: 300s (5 min) by default; set to 0 to disable.
                // This was previously `settings.position_revalidate_interval_secs` which doesn't
                // exist in Settings — hardcode to sane default.
                let interval_secs: u64 = 300;

                // Hot-reload: if interval changed, reset the timer
                if interval_secs == 0 {
                    // Disabled — skip this tick and wait a while before checking again
                    revalidate_interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
                    continue;
                }
                {
                    let expected = tokio::time::Duration::from_secs(interval_secs);
                    let current = revalidate_interval.period();
                    if current != expected {
                        revalidate_interval = tokio::time::interval(expected);
                    }
                }

                if let Some(payer) = payer_pubkey {
                    let snapshot: Vec<(String, Holding)> = {
                        let pos = positions.read().await;
                        pos.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    };

                    if snapshot.is_empty() {
                        continue;
                    }

                    let owner_str = payer.to_string();
                    let mut removed = 0usize;

                    for (mint, holding) in &snapshot {
                        // Skip positions with a sell already in-flight
                        if holding.pending_sell {
                            debug!("Revalidation: skipping {} — sell already in-flight", mint);
                            continue;
                        }

                        let settings_ref = settings.clone(); // Already Arc<Settings> from shared_settings.load_full()
                        match crate::rpc::find_token_account_owned_by_owner(mint, &owner_str, &rpc_client, &settings_ref).await {
                            Ok(Some(ata)) => {
                                if let Ok(pk) = solana_sdk::pubkey::Pubkey::from_str(&ata) {
                                    let rpc_clone = rpc_client.clone();
                                    match tokio::task::spawn_blocking(move || rpc_clone.get_token_account_balance(&pk)).await {
                                        Ok(Ok(balance)) => {
                                            if let Ok(amount) = balance.amount.parse::<u64>() {
                                                if amount == 0 {
                                                    info!(
                                                        "Revalidation cleanup: removed stale position {} (0 tokens on-chain)",
                                                        mint
                                                    );
                                                    positions.write().await.remove(mint);
                                                    removed += 1;
                                                }
                                            }
                                        }
                                        Ok(Err(e)) => {
                                            warn!("Revalidation: balance check failed for {} — keeping position ({})", mint, e);
                                        }
                                        Err(e) => {
                                            warn!("Revalidation: spawn_blocking failed for {} — keeping position ({})", mint, e);
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                info!(
                                    "Revalidation cleanup: removed stale position {} (no ATA found on-chain)",
                                    mint
                                );
                                positions.write().await.remove(mint);
                                removed += 1;
                            }
                            Err(e) => {
                                warn!("Revalidation: RPC error checking {} — keeping position ({})", mint, e);
                            }
                        }
                        // Rate-limit: 200ms between position RPC calls
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }

                    // Persist cleaned positions to disk
                    {
                        let pos = positions.read().await;
                        save_positions_to_file(&pos, &persist_path);
                    }

                    if removed > 0 {
                        info!(
                            "Revalidation: {}/{} positions removed (0 tokens on-chain)",
                            removed, snapshot.len()
                        );
                    } else {
                        debug!("Revalidation: all {} position(s) still valid on-chain", snapshot.len());
                    }
                }
            }
        }
    }
}

/// Mark a position as having a pending sell in-flight. Prevents duplicate sell orders.
async fn set_pending_sell(positions: &Arc<RwLock<HashMap<String, Holding>>>, mint: &str) {
    let mut pos = positions.write().await;
    if let Some(holding) = pos.get_mut(mint) {
        holding.pending_sell = true;
    }
}

/// Mark a position as migrated (bonding curve complete). Logs once and stops polling.
async fn mark_migrated(positions: &Arc<RwLock<HashMap<String, Holding>>>, mint: &str) {
    let mut pos = positions.write().await;
    if let Some(holding) = pos.get_mut(mint) {
        if !holding.migrated {
            holding.migrated = true;
            info!("Token {} migrated to AMM, pausing price monitoring", mint);
        }
    }
}

/// On startup, validate loaded positions against on-chain state.
/// Removes positions where the wallet holds 0 tokens for that mint (stale positions).
/// Rate-limits RPC calls to 200ms between each position check.
async fn validate_positions_on_startup(
    positions: Arc<RwLock<HashMap<String, Holding>>>,
    rpc_client: Arc<RpcClient>,
    settings: Arc<ArcSwap<Settings>>,
    payer_pubkey: solana_sdk::pubkey::Pubkey,
    persist_path: PathBuf,
) {
    let snapshot: Vec<String> = {
        let pos = positions.read().await;
        pos.keys().cloned().collect()
    };
    let total = snapshot.len();
    if total == 0 {
        info!("Startup validation: no positions to validate");
        return;
    }
    info!("Startup validation: checking {} position(s) against on-chain state...", total);

    let owner_str = payer_pubkey.to_string();
    let mut removed = 0usize;

    for mint in &snapshot {
        let settings_ref = settings.load_full();
        match crate::rpc::find_token_account_owned_by_owner(mint, &owner_str, &rpc_client, &settings_ref).await {
            Ok(Some(ata)) => {
                // ATA found — check balance
                if let Ok(pk) = solana_sdk::pubkey::Pubkey::from_str(&ata) {
                    let rpc_clone = rpc_client.clone();
                    match tokio::task::spawn_blocking(move || rpc_clone.get_token_account_balance(&pk)).await {
                        Ok(Ok(balance)) => {
                            if let Ok(amount) = balance.amount.parse::<u64>() {
                                if amount == 0 {
                                    info!(
                                        "Startup cleanup: removing stale position {} (0 tokens on-chain, ATA exists but empty)",
                                        mint
                                    );
                                    positions.write().await.remove(mint);
                                    removed += 1;
                                } else {
                                    debug!("Startup validation: {} has {} tokens on-chain — OK", mint, amount);
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            // Balance query failed — conservatively keep the position
                            warn!("Startup validation: balance check failed for {} — keeping position ({})", mint, e);
                        }
                        Err(e) => {
                            warn!("Startup validation: spawn_blocking failed for {} — keeping position ({})", mint, e);
                        }
                    }
                }
            }
            Ok(None) => {
                // No ATA found for this mint — we don't hold it
                info!(
                    "Startup cleanup: removing stale position {} (no ATA found on-chain)",
                    mint
                );
                positions.write().await.remove(mint);
                removed += 1;
            }
            Err(e) => {
                // RPC error — conservatively keep the position
                warn!("Startup validation: RPC error checking {} — keeping position ({})", mint, e);
            }
        }
        // Rate-limit: 200ms between position checks
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Persist cleaned positions to disk
    {
        let pos = positions.read().await;
        save_positions_to_file(&pos, &persist_path);
    }

    info!(
        "Startup validation: {}/{} positions removed (0 tokens on-chain)",
        removed, total
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_holding(amount: u64, buy_price: f64) -> Holding {
        Holding {
            amount,
            original_amount: amount,
            buy_price,
            buy_time: Utc::now(),
            decimals: 6,
            buy_cost_sol: None,
            triggered_tp_levels: vec![],
            triggered_sl_levels: vec![],
            migrated: false,
            pending_sell: false,
            dex: "pumpfun".to_string(),
            amm_pool: None,
            target_buy_tokens: None,
            extra_pump_account: None,
            pumpswap_accounts: None,
            buy_signature: None,
            metadata: None,
            onchain_raw: None,
            onchain: None,
            timing_price_ms: 0,
            timing_build_ms: 0,
            timing_send_ms: 0,
        }
    }

    #[test]
    fn test_pnl_calculation() {
        let buy_price: f64 = 0.000001;
        let current_price: f64 = 0.0000013;
        let pnl = (current_price - buy_price) / buy_price * 100.0;
        assert!((pnl - 30.0).abs() < 0.01);
    }

    #[test]
    fn test_sell_amount_from_percent() {
        let holding = make_holding(1_000_000_000, 0.000001);
        let sell_percent = 50.0;
        let sell_amount = ((sell_percent / 100.0) * holding.original_amount as f64) as u64;
        assert_eq!(sell_amount, 500_000_000);
    }

    #[test]
    fn test_sell_reason_display() {
        let tp = SellReason::TakeProfit { pnl_percent: 35.5 };
        assert_eq!(format!("{}", tp), "TP (+35.5%)");
        let sl = SellReason::StopLoss { pnl_percent: -22.3 };
        assert_eq!(format!("{}", sl), "SL (-22.3%)");
    }

    #[test]
    fn test_positions_persistence_roundtrip() {
        let mut positions = HashMap::new();
        positions.insert("MintAAA111".to_string(), make_holding(1_000_000, 0.00001));
        positions.insert("MintBBB222".to_string(), make_holding(500_000, 0.00005));

        let tmp_dir = std::env::temp_dir();
        let path = tmp_dir.join("test_positions_roundtrip.json");
        save_positions_to_file(&positions, &path);

        let loaded = load_positions_from_file(path.to_str().unwrap());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["MintAAA111"].amount, 1_000_000);
        assert!((loaded["MintBBB222"].buy_price - 0.00005).abs() < 1e-15);

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_missing_file_returns_empty() {
        let loaded = load_positions_from_file("/nonexistent/path/positions.json");
        assert!(loaded.is_empty());
    }
}

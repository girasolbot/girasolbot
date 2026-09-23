use log::{debug, info, warn};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::commitment_config::CommitmentConfig;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};

use crate::settings::Settings;

/// Cached slot leader with validity metadata.
#[derive(Debug, Clone)]
pub struct CachedLeader {
    pub slot: u64,
    pub leader: Option<Pubkey>,
    pub fetched_at: Instant,
}

/// Background slot-leader cache that refreshes every 400ms.
/// Call `get()` to retrieve the latest cached leader instantly without an RPC call.
#[derive(Clone)]
pub struct LeaderCache {
    inner: Arc<RwLock<Option<CachedLeader>>>,
    /// Notified the first time the cache is populated.
    ready: Arc<Notify>,
}

impl LeaderCache {
    /// Create a new empty cache and spawn the background refresh task.
    /// The task polls `getSlotLeader` every 400ms using the provided RPC client.
    pub fn start(rpc_client: Arc<RpcClient>) -> Self {
        let inner: Arc<RwLock<Option<CachedLeader>>> = Arc::new(RwLock::new(None));
        let ready = Arc::new(Notify::new());
        let cache = LeaderCache {
            inner: inner.clone(),
            ready: ready.clone(),
        };

        tokio::spawn(async move {
            refresh_loop(inner, ready, rpc_client).await;
        });

        cache
    }

    /// Start the cache AND block until the first leader is fetched (or timeout elapses).
    pub async fn start_and_wait(rpc_client: Arc<RpcClient>, timeout: Duration) -> Self {
        let cache = Self::start(rpc_client);
        match cache.wait_ready(timeout).await {
            Ok(()) => info!("LeaderCache ready (initial population complete)"),
            Err(()) => warn!(
                "LeaderCache: initial fetch did not complete within {:?} — cache will populate on next successful refresh",
                timeout
            ),
        }
        cache
    }

    /// Block until the cache has at least one entry, or `timeout` elapses.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<(), ()> {
        if self.inner.read().await.is_some() {
            return Ok(());
        }
        match tokio::time::timeout(timeout, self.ready.notified()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(()),
        }
    }

    /// Get the latest cached leader. Returns None if no leader has been fetched yet.
    pub async fn get(&self) -> Option<CachedLeader> {
        self.inner.read().await.clone()
    }

    /// Static helper: determine if `endpoint_owner` is the current slot leader.
    /// Returns false if either side is missing, if the strategy is disabled, or
    /// if the cached leader is older than `max_age`.
    pub fn is_own_leader(
        cached: &Option<CachedLeader>,
        endpoint_owner: &Option<Pubkey>,
        max_age: Duration,
    ) -> bool {
        let leader = match cached.as_ref().and_then(|c| c.leader) {
            Some(l) => l,
            None => return false,
        };
        if cached.as_ref().unwrap().fetched_at.elapsed() > max_age {
            return false;
        }
        match endpoint_owner {
            Some(owner) => leader == *owner,
            None => false,
        }
    }
}

/// Apply own-leader multipliers to tip (SOL) and priority fee (micro-lamports/CU).
/// Floors: tip at the effective minimum tip, priority fee at 1.
pub fn apply_own_leader_multipliers(
    settings: &Settings,
    base_tip_sol: f64,
    base_priority_fee: u64,
    is_own_leader: bool,
) -> (f64, u64) {
    if !settings.own_leader_strategy_enabled || !is_own_leader {
        return (base_tip_sol, base_priority_fee);
    }
    let min_tip = settings.get_effective_min_tip_sol();
    let multiplied_tip = base_tip_sol * settings.own_leader_tip_multiplier;
    let effective_tip = multiplied_tip.max(min_tip);
    let multiplied_priority = (base_priority_fee as f64 * settings.own_leader_priority_multiplier)
        .ceil()
        .max(1.0) as u64;
    (effective_tip, multiplied_priority.max(1))
}

async fn refresh_loop(
    cache: Arc<RwLock<Option<CachedLeader>>>,
    ready: Arc<Notify>,
    rpc_client: Arc<RpcClient>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(400));
    let mut consecutive_errors = 0u32;
    let mut first_success = true;

    loop {
        interval.tick().await;

        let client = rpc_client.clone();
        let slot_result = tokio::task::spawn_blocking(move || {
            client.get_slot_with_commitment(CommitmentConfig::confirmed())
        })
        .await;

        let slot = match slot_result {
            Ok(Ok(slot)) => slot,
            Ok(Err(e)) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                    warn!("LeaderCache slot refresh failed (x{}): {}", consecutive_errors, e);
                }
                continue;
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                    warn!("LeaderCache slot refresh task panicked (x{}): {}", consecutive_errors, e);
                }
                continue;
            }
        };

        let client = rpc_client.clone();
        let leader_result = tokio::task::spawn_blocking(move || {
            // solana-client 2.3 exposes get_slot_leaders, not get_slot_leader.
            // Query the current slot's single leader and flatten the option.
            (*client).get_slot_leaders(slot, 1).map(|mut v| v.pop())
        })
        .await;

        match leader_result {
            Ok(Ok(leader)) => {
                consecutive_errors = 0;
                let entry = CachedLeader {
                    slot,
                    leader,
                    fetched_at: Instant::now(),
                };
                *cache.write().await = Some(entry);
                debug!("Leader refreshed: slot={} leader={:?}", slot, leader);

                if first_success {
                    first_success = false;
                    ready.notify_waiters();
                }
            }
            Ok(Err(e)) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                    warn!("LeaderCache leader refresh failed (x{}): {}", consecutive_errors, e);
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                    warn!("LeaderCache leader refresh task panicked (x{}): {}", consecutive_errors, e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_cached_leader_parses_pubkey() {
        let pk = Pubkey::from_str("4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE").unwrap();
        let cached = CachedLeader {
            slot: 123,
            leader: Some(pk),
            fetched_at: Instant::now(),
        };
        assert_eq!(cached.slot, 123);
        assert_eq!(cached.leader, Some(pk));
    }

    #[test]
    fn test_is_own_leader_matches() {
        let pk = Pubkey::new_unique();
        let cached = Some(CachedLeader {
            slot: 1,
            leader: Some(pk),
            fetched_at: Instant::now(),
        });
        assert!(LeaderCache::is_own_leader(&cached, &Some(pk), Duration::from_secs(1)));
    }

    #[test]
    fn test_is_own_leader_mismatch() {
        let leader = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let cached = Some(CachedLeader {
            slot: 1,
            leader: Some(leader),
            fetched_at: Instant::now(),
        });
        assert!(!LeaderCache::is_own_leader(&cached, &Some(owner), Duration::from_secs(1)));
    }

    #[test]
    fn test_is_own_leader_missing_owner() {
        let leader = Pubkey::new_unique();
        let cached = Some(CachedLeader {
            slot: 1,
            leader: Some(leader),
            fetched_at: Instant::now(),
        });
        assert!(!LeaderCache::is_own_leader(&cached, &None, Duration::from_secs(1)));
    }

    #[test]
    fn test_is_own_leader_stale_cache() {
        let pk = Pubkey::new_unique();
        let cached = Some(CachedLeader {
            slot: 1,
            leader: Some(pk),
            fetched_at: Instant::now() - Duration::from_secs(10),
        });
        assert!(!LeaderCache::is_own_leader(&cached, &Some(pk), Duration::from_secs(1)));
    }

    #[test]
    fn test_apply_multipliers_disabled() {
        let mut settings = Settings::default();
        settings.own_leader_strategy_enabled = false;
        settings.own_leader_tip_multiplier = 2.0;
        let (tip, priority) = apply_own_leader_multipliers(&settings, 0.001, 100, true);
        assert_eq!(tip, 0.001);
        assert_eq!(priority, 100);
    }

    #[test]
    fn test_apply_multipliers_floor() {
        let mut settings = Settings::default();
        settings.own_leader_strategy_enabled = true;
        settings.own_leader_tip_multiplier = 2.0;
        settings.own_leader_priority_multiplier = 1.5;
        settings.helius_min_tip_sol = 0.001;
        let (tip, priority) = apply_own_leader_multipliers(&settings, 0.001, 100, true);
        assert!(tip >= 0.001);
        assert_eq!(priority, 150);
    }
}

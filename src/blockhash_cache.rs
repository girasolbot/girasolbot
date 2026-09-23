use log::{debug, info, warn};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, hash::Hash};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};

/// Cached blockhash with validity metadata
#[derive(Debug, Clone)]
pub struct CachedBlockhash {
    pub blockhash: Hash,
    pub last_valid_block_height: u64,
    pub fetched_at: Instant,
}

/// Background blockhash cache that refreshes every 400ms.
/// Call `get()` to retrieve the latest cached blockhash instantly without an RPC call.
#[derive(Clone)]
pub struct BlockhashCache {
    inner: Arc<RwLock<Option<CachedBlockhash>>>,
    /// Notified the first time the cache is populated. Allows `wait_ready()`
    /// callers to block on cold-start without polling.
    ready: Arc<Notify>,
}

impl BlockhashCache {
    /// Create a new empty cache and spawn the background refresh task.
    /// The task polls `getLatestBlockhash` every 400ms using the provided RPC client.
    ///
    /// NOTE: this returns immediately even though the cache is still empty.
    /// Callers that need a guaranteed-populated cache before serving traffic
    /// must call [`start_and_wait`] or [`wait_ready`] before the first
    /// `get().await`.
    pub fn start(rpc_client: Arc<RpcClient>) -> Self {
        let inner: Arc<RwLock<Option<CachedBlockhash>>> = Arc::new(RwLock::new(None));
        let ready = Arc::new(Notify::new());
        let cache = BlockhashCache { inner: inner.clone(), ready: ready.clone() };

        tokio::spawn(async move {
            refresh_loop(inner, ready, rpc_client).await;
        });

        cache
    }

    /// Start the cache AND block until the first blockhash is fetched (or
    /// `timeout` elapses). Use this in `main` before starting listeners so
    /// detections that arrive in the first ~400ms don't fall through to the
    /// slow synchronous RPC path.
    ///
    /// On timeout, the cache is returned but still empty — callers will then
    /// hit the slow path until the background refresh succeeds. The cache is
    /// only ever empty BEFORE the first successful fetch; once populated, the
    /// background task keeps it warm.
    pub async fn start_and_wait(
        rpc_client: Arc<RpcClient>,
        timeout: Duration,
    ) -> Self {
        let cache = Self::start(rpc_client);
        match cache.wait_ready(timeout).await {
            Ok(()) => info!("BlockhashCache ready (initial population complete)"),
            Err(()) => warn!(
                "BlockhashCache: initial fetch did not complete within {:?} — \
                 cache will populate on next successful refresh; first buys may \
                 fall back to synchronous RPC",
                timeout
            ),
        }
        cache
    }

    /// Block until the cache has at least one entry, or `timeout` elapses.
    /// Returns Ok(()) if populated, Err(()) on timeout.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<(), ()> {
        // Fast path: already populated
        if self.inner.read().await.is_some() {
            return Ok(());
        }
        // Wait on the notify with a deadline
        match tokio::time::timeout(timeout, self.ready.notified()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(()),
        }
    }

    /// Get the latest cached blockhash. Returns None if no blockhash has been fetched yet.
    pub async fn get(&self) -> Option<CachedBlockhash> {
        self.inner.read().await.clone()
    }
}

async fn refresh_loop(
    cache: Arc<RwLock<Option<CachedBlockhash>>>,
    ready: Arc<Notify>,
    rpc_client: Arc<RpcClient>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(400));
    let mut consecutive_errors = 0u32;
    let mut first_success = true;

    loop {
        interval.tick().await;

        // Use spawn_blocking since RpcClient::get_latest_blockhash_with_commitment is sync
        let client = rpc_client.clone();
        let result = tokio::task::spawn_blocking(move || {
            client.get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
        }).await;

        match result {
            Ok(Ok((blockhash, last_valid_block_height))) => {
                consecutive_errors = 0;
                let entry = CachedBlockhash {
                    blockhash,
                    last_valid_block_height,
                    fetched_at: Instant::now(),
                };
                *cache.write().await = Some(entry);
                debug!("Blockhash refreshed: {} (valid until height {})", blockhash, last_valid_block_height);

                // Wake up any wait_ready() callers on the FIRST successful fetch.
                // notify_waiters() wakes all currently-waiting tasks but does NOT
                // store the notification — that's fine because subsequent calls
                // to wait_ready() take the fast path through is_some().
                if first_success {
                    first_success = false;
                    ready.notify_waiters();
                }
            }
            Ok(Err(e)) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                    warn!("Blockhash refresh failed (x{}): {}", consecutive_errors, e);
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                    warn!("Blockhash refresh task panicked (x{}): {}", consecutive_errors, e);
                }
            }
        }
    }
}

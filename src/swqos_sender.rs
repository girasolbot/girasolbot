use base64::Engine;
use log::{info, warn, debug};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Semaphore};
use std::sync::RwLock;

use crate::quic_sender::{BloxrouteQuicSender, ClientIdentity, NextBlockQuicSender};
use crate::settings::{LeaderMapping, Settings};
use crate::leader_cache::{CachedLeader, LeaderCache};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::pubkey;
use std::str::FromStr;

// ZeroSlot and Temporal tip accounts (mirroring LulzFi constants)
pub const ZEROSLOT_TIP_ACCOUNTS: &[Pubkey] = &[
    pubkey!("Eb2KpSC8uMt9GmzyAEm5Eb1AAAgTjRaXWFjKyFXHZxF3"),
    pubkey!("FCjUJZ1qozm1e8romw216qyfQMaaWKxWsuySnumVCCNe"),
    pubkey!("ENxTEjSQ1YabmUpXAdCgevnHQ9MHdLv8tzFiuiYJqa13"),
    pubkey!("6rYLG55Q9RpsPGvqdPNJs4z5WTxJVatMB8zV3WJhs5EK"),
    pubkey!("Cix2bHfqPcKcM233mzxbLk14kSggUUiz2A87fJtGivXr"),
];

pub const TEMPORAL_TIP_ACCOUNTS: &[Pubkey] = &[
    pubkey!("TEMPaMeCRFAS9EKF53Jd6KpHxgL47uWLcpFArU1Fanq"),
    pubkey!("noz3jAjPiHuBPqiSPkkugaJDkJscPuRhYnSpbi8UvC4"),
    pubkey!("noz3str9KXfpKknefHji8L1mPgimezaiUyCHYMDv1GE"),
    pubkey!("noz6uoYCDijhu1V7cutCpwxNiSovEwLdRHPwmgCGDNo"),
    pubkey!("noz9EPNcT7WH6Sou3sr3GGjHQYVkN3DNirpbvDkv9YJ"),
];

// ─── Circuit breaker ────────────────────────────────────────────────
//
// Lightweight per-provider circuit breaker: tracks consecutive failures,
// opens the circuit after `failure_threshold` consecutive failures,
// and allows a single probe (half-open) after `cooldown_secs`.
// On success, the circuit closes immediately.

/// Per-provider circuit-breaker state.
#[derive(Debug)]
struct ProviderCircuit {
    /// Number of consecutive failures (reset to 0 on success).
    consecutive_failures: usize,
    /// Instant when the circuit opened (None = closed).
    opened_at: Option<Instant>,
}

impl ProviderCircuit {
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
            opened_at: None,
        }
    }
}

/// Circuit state for a single provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitState {
    /// Provider is healthy — send traffic normally.
    Closed,
    /// Provider has been failing; allow one probe attempt.
    HalfOpen,
    /// Provider is down — skip sending entirely.
    Open,
}

/// Health tracker for all SWQoS providers. Tracks consecutive failures
/// per provider and opens/closes circuits to prevent sending into a
/// dead endpoint on every transaction.
struct ProviderHealthTracker {
    /// Per-provider circuits, keyed by provider name (e.g. "bloxroute").
    circuits: RwLock<HashMap<String, ProviderCircuit>>,
    /// How many consecutive failures before opening the circuit.
    failure_threshold: usize,
    /// Seconds to wait in Open state before transitioning to HalfOpen.
    cooldown_secs: u64,
}

impl ProviderHealthTracker {
    fn new(failure_threshold: usize, cooldown_secs: u64) -> Self {
        Self {
            circuits: RwLock::new(HashMap::new()),
            failure_threshold,
            cooldown_secs,
        }
    }

    /// Check the current circuit state for a provider. If the circuit is Open
    /// but cooldown has elapsed, transitions to HalfOpen.
    fn state(&self, provider: &str) -> CircuitState {
        let circuits = self.circuits.read().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = circuits.get(provider) {
            if c.consecutive_failures < self.failure_threshold {
                CircuitState::Closed
            } else if let Some(opened) = c.opened_at {
                if opened.elapsed().as_secs() >= self.cooldown_secs {
                    CircuitState::HalfOpen
                } else {
                    CircuitState::Open
                }
            } else {
                // At threshold but no opened_at — treat as open
                CircuitState::Open
            }
        } else {
            CircuitState::Closed
        }
    }

    /// Record a successful send. Resets consecutive failures to 0 and closes circuit.
    fn record_success(&self, provider: &str) {
        let mut circuits = self.circuits.write().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = circuits.get_mut(provider) {
            c.consecutive_failures = 0;
            c.opened_at = None;
        }
    }

    /// Record a failed send. Increments consecutive failures; opens circuit at threshold.
    fn record_failure(&self, provider: &str) {
        let mut circuits = self.circuits.write().unwrap_or_else(|e| e.into_inner());
        let c = circuits.entry(provider.to_string()).or_insert_with(ProviderCircuit::new);
        c.consecutive_failures += 1;
        if c.consecutive_failures >= self.failure_threshold && c.opened_at.is_none() {
            c.opened_at = Some(Instant::now());
            warn!(
                "SWQOS circuit OPEN for '{}' after {} consecutive failures (cooldown: {}s)",
                provider, c.consecutive_failures, self.cooldown_secs
            );
        }
    }
}

/// Reuse shared HTTP client from rpc module for connection pooling
fn shared_client() -> &'static reqwest::Client {
    &*crate::rpc::SHARED_HTTP_CLIENT
}

/// Result of a single provider send attempt
#[derive(Debug, Clone)]
pub struct ProviderResult {
    pub provider: String,
    pub signature: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// Configured SWQoS provider
#[derive(Debug, Clone)]
pub enum SwqosProvider {
    /// Standard Solana RPC sendTransaction
    DefaultRpc { url: String },
    /// Jito block engine (Frankfurt)
    Jito { url: String },
    /// Helius Sender (with optional API key and SWQOS-only mode)
    Helius { url: String, api_key: Option<String>, swqos_only: bool },
    /// bloXroute submit-snipe (HTTP + QUIC dual path)
    Bloxroute {
        http_url: String,
        quic_endpoints: Vec<String>,
        api_key: String,
    },
    /// NextBlock QUIC (persistent connection, fire-and-forget)
    NextBlock {
        endpoint: String,
        api_key: String,
    },
    /// ZeroSlot HTTP JSON-RPC (query-string auth)
    ZeroSlot {
        endpoint: String,
        api_key: String,
    },
    /// Temporal HTTP JSON-RPC (query-string auth)
    Temporal {
        endpoint: String,
        api_key: String,
    },
    /// Test-only mock provider: `fast_sig=Some` returns instantly (winner);
    /// `fast_sig=None` sleeps a long time then increments `counter` (loser).
    /// Used to verify that `send_concurrent` aborts losing tasks via
    /// `AbortHandle::abort()` before they reach the increment.
    #[cfg(test)]
    Mock {
        fast_sig: Option<String>,
        counter: Arc<std::sync::atomic::AtomicUsize>,
    },
}

impl SwqosProvider {
    fn name(&self) -> &str {
        match self {
            SwqosProvider::DefaultRpc { .. } => "default_rpc",
            SwqosProvider::Jito { .. } => "jito",
            SwqosProvider::Helius { .. } => "helius",
            SwqosProvider::Bloxroute { .. } => "bloxroute",
            SwqosProvider::NextBlock { .. } => "nextblock",
            SwqosProvider::ZeroSlot { .. } => "zeroslot",
            SwqosProvider::Temporal { .. } => "temporal",
            #[cfg(test)]
            SwqosProvider::Mock { .. } => "mock",
        }
    }

    /// Resolve the validator identity pubkey for this provider from the leader mapping.
    fn identity_pubkey(&self, mapping: &LeaderMapping) -> Option<Pubkey> {
        let maybe_str: Option<&String> = match self {
            SwqosProvider::DefaultRpc { .. } => mapping.swqos_default_rpc_endpoint_owner.as_ref(),
            SwqosProvider::Jito { .. } => mapping.swqos_jito_endpoint_owner.as_ref(),
            SwqosProvider::Helius { .. } => mapping.swqos_helius_endpoint_owner.as_ref(),
            SwqosProvider::Bloxroute { .. } => mapping.swqos_bloxroute_endpoint_owner.as_ref(),
            SwqosProvider::NextBlock { .. } => mapping.swqos_nextblock_endpoint_owner.as_ref(),
            SwqosProvider::ZeroSlot { .. } => mapping.swqos_zeroslot_endpoint_owner.as_ref(),
            SwqosProvider::Temporal { .. } => mapping.swqos_temporal_endpoint_owner.as_ref(),
            #[cfg(test)]
            SwqosProvider::Mock { .. } => None,
        };
        maybe_str.and_then(|s| Pubkey::from_str(s).ok())
    }
}

/// Concurrent SWQoS sender — sends the same signed transaction to multiple
/// providers simultaneously and returns on first success.
///
/// Includes a bounded Semaphore to cap concurrent in-flight sends (prevents
/// unlimited task spawn under network problems) and per-provider circuit
/// breakers to skip known-down endpoints.
pub struct SwqosSender {
    providers: Vec<SwqosProvider>,
    /// Persistent bloXroute QUIC sender (mTLS, multi-region).
    /// Created once at startup; reused across all sends.
    /// `None` if no bloXroute provider configured OR mTLS PEMs missing/invalid
    /// (in which case bloXroute falls back to HTTP-only).
    bloxroute_quic: Option<Arc<BloxrouteQuicSender>>,
    /// Persistent NextBlock QUIC sender.
    /// Created once at startup; reused across all sends.
    /// `None` if no NextBlock provider configured OR `nextblock_quic_endpoint` not set
    /// (in which case NextBlock falls back to HTTP-only).
    nextblock_quic: Option<Arc<NextBlockQuicSender>>,
    /// Bounded semaphore limiting concurrent in-flight sends across all providers.
    /// Prevents unbounded task spawn when network problems stall sends.
    send_semaphore: Arc<Semaphore>,
    /// Per-provider circuit breaker: tracks consecutive failures and skips
    /// providers that are in Open state (cooldown elapsed → HalfOpen probe).
    health: Arc<ProviderHealthTracker>,
    /// Validator identity mapping for Agave #13267 own-leader strategy.
    leader_mapping: LeaderMapping,
}

impl SwqosSender {
    /// Build providers from settings. If `swqos_concurrent_send` is false or no
    /// providers configured, returns None (caller should fall back to legacy path).
    pub fn from_settings(settings: &Settings) -> Option<Self> {
        if !settings.swqos_concurrent_send {
            return None;
        }

        let provider_names = &settings.swqos_providers;
        if provider_names.is_empty() {
            return None;
        }

        let mut providers = Vec::new();
        for name in provider_names {
            match name.as_str() {
                "default_rpc" => {
                    if let Some(url) = settings.solana_rpc_urls.first() {
                        providers.push(SwqosProvider::DefaultRpc { url: url.clone() });
                    }
                }
                "jito" => {
                    providers.push(SwqosProvider::Jito {
                        url: settings.swqos_jito_url.clone(),
                    });
                }
                "helius" => {
                    providers.push(SwqosProvider::Helius {
                        url: settings.helius_sender_endpoint.clone(),
                        api_key: settings.helius_api_key.clone(),
                        swqos_only: settings.helius_use_swqos_only,
                    });
                }
                "bloxroute" => {
                    if let Some(ref api_key) = settings.bloxroute_api_key {
                        providers.push(SwqosProvider::Bloxroute {
                            http_url: settings.bloxroute_http_url.clone(),
                            quic_endpoints: settings.bloxroute_quic_endpoints.clone(),
                            api_key: api_key.clone(),
                        });
                    } else {
                        warn!("bloXroute provider requested but no API key configured, skipping");
                    }
                }
                "nextblock" => {
                    if let Some(ref api_key) = settings.nextblock_api_key {
                        providers.push(SwqosProvider::NextBlock {
                            endpoint: settings.nextblock_endpoint.clone(),
                            api_key: api_key.clone(),
                        });
                    } else {
                        warn!("NextBlock provider requested but no API key configured, skipping");
                    }
                }
                "zeroslot" => {
                    if let Some(ref api_key) = settings.zeroslot_api_key2 {
                        providers.push(SwqosProvider::ZeroSlot {
                            endpoint: settings.zeroslot_endpoint.clone(),
                            api_key: api_key.clone(),
                        });
                    } else {
                        warn!("ZeroSlot provider requested but no API key configured, skipping");
                    }
                }
                "temporal" => {
                    if let Some(ref api_key) = settings.temporal_api_key2 {
                        providers.push(SwqosProvider::Temporal {
                            endpoint: settings.temporal_endpoint.clone(),
                            api_key: api_key.clone(),
                        });
                    } else {
                        warn!("Temporal provider requested but no API key configured, skipping");
                    }
                }
                other => {
                    warn!("Unknown SWQoS provider '{}', skipping", other);
                }
            }
        }

        if providers.is_empty() {
            return None;
        }

        // Build persistent QUIC senders for bloXroute and NextBlock.
        // These connections are created once and reused across all sends.
        let bloxroute_quic = if providers.iter().any(|p| matches!(p, SwqosProvider::Bloxroute { .. })) {
            build_bloxroute_quic(settings)
        } else {
            None
        };

        let nextblock_quic = providers.iter().find_map(|p| {
            if let SwqosProvider::NextBlock { api_key, .. } = p {
                build_nextblock_quic(settings, api_key)
            } else {
                None
            }
        });

        info!("SWQoS concurrent sender initialized with {} providers: {} (bloXroute QUIC: {}, NextBlock QUIC: {}), max_concurrent={}, circuit_breaker_failures={}",
            providers.len(),
            providers.iter().map(|p| p.name()).collect::<Vec<_>>().join(", "),
            if bloxroute_quic.is_some() { "ON" } else { "off" },
            if nextblock_quic.is_some() { "ON" } else { "off" },
            settings.swqos_max_concurrent_sends,
            settings.swqos_circuit_breaker_failures,
        );

        let send_semaphore = Arc::new(Semaphore::new(settings.swqos_max_concurrent_sends));
        let health = Arc::new(ProviderHealthTracker::new(
            settings.swqos_circuit_breaker_failures,
            settings.swqos_circuit_breaker_cooldown_secs,
        ));

        Some(Self {
            providers,
            bloxroute_quic,
            nextblock_quic,
            send_semaphore,
            health,
            leader_mapping: settings.leader_mapping.clone(),
        })
    }

    /// Send a base64-encoded signed transaction to all configured providers
    /// simultaneously. Returns the signature from the first provider to succeed.
    ///
    /// All providers receive the exact same serialized transaction — the signature
    /// is deterministic (same bytes, same result). Whichever endpoint lands the
    /// transaction first wins; the rest are harmlessly redundant.
    ///
    /// **Circuit breaker**: Providers in Open state (consecutive failures ≥ threshold
    /// and cooldown not elapsed) are skipped entirely. Providers in HalfOpen state
    /// (cooldown elapsed) get one probe attempt — success closes the circuit, failure
    /// re-opens it.
    ///
    /// **Semaphore**: A bounded semaphore limits how many provider-sending tasks can be
    /// in-flight at once across all concurrent `send_concurrent` calls. This prevents
    /// unbounded task spawn when network problems cause every send to stall.
    pub async fn send_concurrent(
        &self,
        tx_base64: &str,
        settings: &Settings,
        leader_cache: Option<CachedLeader>,
    ) -> Result<(String, Vec<ProviderResult>), Box<dyn std::error::Error + Send + Sync>> {
        let overall_start = Instant::now();

        // Pre-filter providers by circuit state. Skip Open providers entirely.
        let mut active_providers: Vec<(SwqosProvider, CircuitState)> = Vec::new();
        let mut skipped_open: Vec<&str> = Vec::new();

        for provider in &self.providers {
            let state = self.health.state(provider.name());
            match state {
                CircuitState::Open => {
                    skipped_open.push(provider.name());
                }
                CircuitState::HalfOpen | CircuitState::Closed => {
                    active_providers.push((provider.clone(), state));
                }
            }
        }

        // Agave #13267 own-leader provider skip
        let skip_leader_provider = settings.own_leader_strategy_enabled && settings.swqos_skip_leader_provider;
        let current_leader = leader_cache.as_ref().and_then(|c| c.leader);
        let mut skipped_leader: Vec<String> = Vec::new();
        if skip_leader_provider {
            active_providers.retain(|(provider, _)| {
                let skip = current_leader
                    .and_then(|leader| {
                        provider.identity_pubkey(&settings.leader_mapping)
                            .map(|owner| owner == leader)
                    })
                    .unwrap_or(false);
                if skip {
                    skipped_leader.push(provider.name().to_string());
                    false
                } else {
                    true
                }
            });
        }

        if !skipped_leader.is_empty() {
            warn!(
                "SWQOS own-leader skip: skipping {} provider(s) that are current leader: [{}]",
                skipped_leader.len(),
                skipped_leader.join(", ")
            );
        }

        if !skipped_open.is_empty() {
            warn!("SWQOS circuit breaker: skipping {} OPEN provider(s): [{}]",
                skipped_open.len(), skipped_open.join(", "));
        }

        if active_providers.is_empty() {
            if settings.own_leader_strategy_enabled && settings.own_leader_fallback_direct_rpc {
                warn!("All SWQoS providers skipped due to own-leader rule; falling back to direct RPC sendTransaction");
                if let Some(url) = settings.solana_rpc_urls.first() {
                    match send_rpc_transaction(url, tx_base64).await {
                        Ok(sig) => {
                            let total_ms = overall_start.elapsed().as_millis();
                            info!("TIMING swqos-direct-fallback: {}ms total | sig {}", total_ms, sig);
                            let results = vec![ProviderResult {
                                provider: "direct_rpc_fallback".to_string(),
                                signature: Some(sig.clone()),
                                error: None,
                                elapsed_ms: total_ms,
                            }];
                            return Ok((sig, results));
                        }
                        Err(e) => {
                            return Err(format!("Direct RPC fallback failed: {}", e).into());
                        }
                    }
                }
            }
            let total_ms = overall_start.elapsed().as_millis();
            return Err(format!(
                "All {} SWQoS providers are in OPEN circuit state ({}ms) — skipping send entirely",
                self.providers.len(), total_ms
            ).into());
        }

        let n = active_providers.len();
        let (result_tx, mut result_rx) = mpsc::channel::<ProviderResult>(n);

        // Spawn one task per active provider — semaphore bounds concurrency.
        // Keep AbortHandles so we can cancel stragglers once a winner lands.
        let mut abort_handles: Vec<tokio::task::AbortHandle> = Vec::with_capacity(n);
        for (provider, circuit_state) in active_providers {
            let tx_b64 = tx_base64.to_string();
            let tx_chan = result_tx.clone();
            let bxr_quic = self.bloxroute_quic.clone();
            let nb_quic = self.nextblock_quic.clone();
            let semaphore = self.send_semaphore.clone();
            let health = self.health.clone();

            let handle = tokio::spawn(async move {
                // Acquire semaphore permit — bounds concurrent in-flight sends
                let _permit = match semaphore.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        // Semaphore closed; should not happen in normal operation
                        let _ = tx_chan.send(ProviderResult {
                            provider: provider.name().to_string(),
                            signature: None,
                            error: Some("semaphore closed".to_string()),
                            elapsed_ms: 0,
                        }).await;
                        return;
                    }
                };

                let start = Instant::now();
                let result = send_to_provider(&provider, &tx_b64, bxr_quic.as_deref(), nb_quic.as_deref()).await;
                let elapsed_ms = start.elapsed().as_millis();

                // Update circuit breaker
                match result {
                    Ok(ref sig) => {
                        health.record_success(provider.name());
                        let pr = ProviderResult {
                            provider: provider.name().to_string(),
                            signature: Some(sig.clone()),
                            error: None,
                            elapsed_ms,
                        };
                        let _ = tx_chan.send(pr).await;
                    }
                    Err(ref e) => {
                        // Record failure regardless of circuit state.
                        // If HalfOpen → this re-opens the circuit.
                        // If Closed → increments toward threshold.
                        health.record_failure(provider.name());
                        let state_label = match circuit_state {
                            CircuitState::HalfOpen => "HALF-OPEN→FAIL",
                            CircuitState::Closed => "FAIL",
                            CircuitState::Open => "OPEN", // shouldn't reach here, but be safe
                        };
                        let pr = ProviderResult {
                            provider: provider.name().to_string(),
                            signature: None,
                            error: Some(format!("[{}] {}", state_label, e)),
                            elapsed_ms,
                        };
                        let _ = tx_chan.send(pr).await;
                    }
                }
            });
            abort_handles.push(handle.abort_handle());
        }

        // Drop our copy so the channel closes when all tasks finish
        drop(result_tx);

        // Collect results, return on first success
        let mut all_results: Vec<ProviderResult> = Vec::with_capacity(n);
        let mut first_sig: Option<String> = None;

        while let Some(pr) = result_rx.recv().await {
            if let Some(ref sig) = pr.signature {
                info!("SWQOS {}: OK in {}ms (sig: {}...)", pr.provider, pr.elapsed_ms,
                    &sig[..sig.len().min(16)]);
                if first_sig.is_none() {
                    first_sig = Some(sig.clone());
                }
            } else if let Some(ref err) = pr.error {
                warn!("SWQOS {}: {}", pr.provider, err);
            }
            all_results.push(pr);

            // Once we have first success, don't block waiting for stragglers —
            // cancel remaining in-flight provider tasks and drain any results
            // that already finished (won't be cancelled).
            if first_sig.is_some() {
                for h in &abort_handles {
                    h.abort();
                }
                // Drain any immediately-available results without blocking
                while let Ok(pr) = result_rx.try_recv() {
                    if pr.signature.is_some() {
                        info!("SWQOS {}: OK in {}ms (redundant)", pr.provider, pr.elapsed_ms);
                    } else if let Some(ref err) = pr.error {
                        debug!("SWQOS {}: {}", pr.provider, err);
                    }
                    all_results.push(pr);
                }
                break;
            }
        }

        let total_ms = overall_start.elapsed().as_millis();

        match first_sig {
            Some(sig) => {
                let successes = all_results.iter().filter(|r| r.signature.is_some()).count();
                let failures = all_results.iter().filter(|r| r.error.is_some()).count();
                info!("TIMING swqos: {}ms total | {}/{} succeeded, {}/{} failed ({} circuits open)",
                    total_ms, successes, n, failures, n, skipped_open.len());
                Ok((sig, all_results))
            }
            None => {
                // Include skipped (open-circuit) providers in the error count
                let total_attempted = n;
                let total_skipped = skipped_open.len();
                let errors: Vec<String> = all_results.iter()
                    .filter_map(|r| r.error.as_ref().map(|e| format!("{}: {}", r.provider, e)))
                    .collect();
                Err(format!("All {} active SWQoS providers failed in {}ms ({} circuits open, skipped); [{}]",
                    total_attempted, total_ms, total_skipped, errors.join("; ")).into())
            }
        }
    }
}

/// Send a base64-encoded transaction to a single provider.
///
/// For bloXroute and NextBlock: tries QUIC first (if persistent sender is available),
/// falls back to HTTP on any QUIC failure. QUIC saves ~10-30ms per send by
/// eliminating TCP/TLS handshake.
async fn send_to_provider(
    provider: &SwqosProvider,
    tx_base64: &str,
    bloxroute_quic: Option<&BloxrouteQuicSender>,
    nextblock_quic: Option<&NextBlockQuicSender>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match provider {
        SwqosProvider::DefaultRpc { url } => {
            send_rpc_transaction(url, tx_base64).await
        }
        SwqosProvider::Jito { url } => {
            send_rpc_transaction(url, tx_base64).await
        }
        SwqosProvider::Helius { url, api_key, swqos_only } => {
            let mut endpoint = url.clone();
            let mut params = Vec::new();
            if *swqos_only {
                params.push("swqos_only=true".to_string());
            }
            if let Some(key) = api_key {
                params.push(format!("api-key={}", key));
            }
            if !params.is_empty() {
                let sep = if endpoint.contains('?') { "&" } else { "?" };
                endpoint = format!("{}{}{}", endpoint, sep, params.join("&"));
            }
            send_rpc_transaction(&endpoint, tx_base64).await
        }
        SwqosProvider::Bloxroute { http_url, api_key, .. } => {
            // QUIC primary (mTLS, multi-region) → HTTP fallback
            if let Some(quic) = bloxroute_quic {
                match decode_b64_then_quic_bloxroute(quic, tx_base64).await {
                    Ok(sig) => return Ok(sig),
                    Err(e) => {
                        warn!("bloXroute QUIC failed, falling back to HTTP: {}", e);
                    }
                }
            }
            send_bloxroute_http(http_url, api_key, tx_base64).await
        }
        SwqosProvider::NextBlock { endpoint, api_key } => {
            // QUIC primary (persistent connection) → HTTP fallback
            if let Some(quic) = nextblock_quic {
                match decode_b64_then_quic_nextblock(quic, tx_base64).await {
                    Ok(sig) => return Ok(sig),
                    Err(e) => {
                        warn!("NextBlock QUIC failed, falling back to HTTP: {}", e);
                    }
                }
            }
            send_nextblock_http(endpoint, api_key, tx_base64).await
        }
        SwqosProvider::ZeroSlot { endpoint, api_key } => {
            // ZeroSlot: append ?api-key= to base URL, then JSON-RPC sendTransaction
            let url = format!("{}?api-key={}", endpoint, api_key);
            send_rpc_transaction(&url, tx_base64).await
        }
        SwqosProvider::Temporal { endpoint, api_key } => {
            // Temporal: append ?c= channel param, then JSON-RPC sendTransaction
            let url = format!("{}?c={}", endpoint, api_key);
            send_rpc_transaction(&url, tx_base64).await
        }
        #[cfg(test)]
        SwqosProvider::Mock { fast_sig, counter } => {
            if let Some(sig) = fast_sig {
                Ok(sig.clone())
            } else {
                // Simulate a slow provider that would eventually "succeed".
                // If the task is NOT aborted, this sleep completes and the
                // counter is incremented — the test asserts that never happens.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok("slowsig".to_string())
            }
        }
    }
}

/// Build a persistent BloxrouteQuicSender from settings.
/// Returns `None` if mTLS PEM paths are missing or fail to load — in which case
/// bloXroute falls back to HTTP-only mode (still functional, just slower).
fn build_bloxroute_quic(settings: &Settings) -> Option<Arc<BloxrouteQuicSender>> {
    let identity = match (&settings.bloxroute_client_cert_pem, &settings.bloxroute_client_key_pem) {
        (Some(cert_path), Some(key_path)) => {
            match ClientIdentity::from_pem_files(cert_path, key_path) {
                Ok(id) => Some(id),
                Err(e) => {
                    warn!(
                        "bloXroute mTLS PEM load failed (cert='{}', key='{}'): {} — QUIC disabled, HTTP-only",
                        cert_path, key_path, e
                    );
                    return None;
                }
            }
        }
        _ => {
            warn!("bloXroute mTLS PEMs not configured (bloxroute_client_cert_pem/key_pem) — QUIC disabled, HTTP-only");
            return None;
        }
    };

    if settings.bloxroute_quic_endpoints.is_empty() {
        warn!("bloXroute QUIC endpoints empty — QUIC disabled, HTTP-only");
        return None;
    }

    info!(
        "bloXroute QUIC sender initialized: {} endpoint(s), mTLS active",
        settings.bloxroute_quic_endpoints.len()
    );
    Some(Arc::new(BloxrouteQuicSender::new(
        settings.bloxroute_quic_endpoints.clone(),
        identity,
    )))
}

/// Build a persistent NextBlockQuicSender from settings.
/// Returns `None` if `nextblock_quic_endpoint` is not configured — in which case
/// NextBlock falls back to HTTP-only mode.
fn build_nextblock_quic(settings: &Settings, api_key: &str) -> Option<Arc<NextBlockQuicSender>> {
    let endpoint = settings.nextblock_quic_endpoint.as_ref()?.clone();
    if endpoint.is_empty() {
        return None;
    }
    info!("NextBlock QUIC sender initialized: endpoint='{}'", endpoint);
    Some(Arc::new(NextBlockQuicSender::new(endpoint, api_key.to_string())))
}

/// Decode base64 → raw bytes → bloXroute QUIC send (multi-region concurrent).
async fn decode_b64_then_quic_bloxroute(
    quic: &BloxrouteQuicSender,
    tx_base64: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let tx_bytes = base64::engine::general_purpose::STANDARD
        .decode(tx_base64)
        .map_err(|e| format!("base64 decode failed: {}", e))?;
    quic.send_concurrent(&tx_bytes)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
}

/// Decode base64 → raw bytes → NextBlock QUIC send (persistent connection).
async fn decode_b64_then_quic_nextblock(
    quic: &NextBlockQuicSender,
    tx_base64: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let tx_bytes = base64::engine::general_purpose::STANDARD
        .decode(tx_base64)
        .map_err(|e| format!("base64 decode failed: {}", e))?;
    quic.send_tx(&tx_bytes)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
}

/// Generic JSON-RPC sendTransaction call (works for standard RPC, Jito, Helius).
async fn send_rpc_transaction(
    endpoint: &str,
    tx_base64: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let payload = json!({
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

    let response = shared_client()
        .post(endpoint)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    let json: Value = response.json().await?;

    if let Some(error) = json.get("error") {
        return Err(format!("{}", error).into());
    }

    if let Some(result) = json.get("result") {
        if let Some(sig) = result.as_str() {
            return Ok(sig.to_string());
        }
    }

    Err("Invalid response: no result or error field".into())
}

/// bloXroute submit-snipe HTTP endpoint.
/// POST /api/v2/submit-snipe with Authorization header.
/// Supports dual Jito bundle path and staked path.
async fn send_bloxroute_http(
    endpoint: &str,
    api_key: &str,
    tx_base64: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let payload = json!({
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

    let response = shared_client()
        .post(endpoint)
        .header("Authorization", api_key)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    let json: Value = response.json().await?;

    if let Some(error) = json.get("error") {
        return Err(format!("bloXroute error: {}", error).into());
    }

    if let Some(result) = json.get("result") {
        if let Some(sig) = result.as_str() {
            return Ok(sig.to_string());
        }
    }

    // bloXroute may return { "signature": "..." } directly
    if let Some(sig) = json.get("signature").and_then(|s| s.as_str()) {
        return Ok(sig.to_string());
    }

    Err("bloXroute: invalid response format".into())
}

/// NextBlock HTTP endpoint.
/// POST with `authorization` header (lowercase, NO "Bearer" prefix).
async fn send_nextblock_http(
    endpoint: &str,
    api_key: &str,
    tx_base64: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let payload = json!({
        "transaction": {
            "content": tx_base64
        }
    });

    let response = shared_client()
        .post(endpoint)
        .header("authorization", api_key)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    let json: Value = response.json().await?;

    if let Some(error) = json.get("error") {
        return Err(format!("NextBlock error: {}", error).into());
    }

    if let Some(result) = json.get("result") {
        if let Some(sig) = result.as_str() {
            return Ok(sig.to_string());
        }
    }

    // NextBlock may return { "signature": "..." }
    if let Some(sig) = json.get("signature").and_then(|s| s.as_str()) {
        return Ok(sig.to_string());
    }

    Err("NextBlock: invalid response format".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Verify the REAL cancellation logic in `send_concurrent`: once the first
    /// provider returns a signature, all remaining in-flight tasks must be
    /// aborted via `AbortHandle::abort()` before they complete their work.
    ///
    /// We use the `SwqosProvider::Mock` variant: a fast winner returns a
    /// signature instantly, while a slow loser sleeps for 60s and only then
    /// increments an `Arc<AtomicUsize>` counter. If `send_concurrent` correctly
    /// aborts the loser's task, the sleep is cancelled and the counter stays 0.
    #[tokio::test]
    async fn test_losing_tasks_aborted_after_first_success() {
        let counter = Arc::new(AtomicUsize::new(0));

        // Winner: returns a signature immediately.
        let winner = SwqosProvider::Mock {
            fast_sig: Some("fastsig123".to_string()),
            counter: counter.clone(),
        };
        // Loser: sleeps 60s, then increments the counter (must be aborted first).
        let loser = SwqosProvider::Mock {
            fast_sig: None,
            counter: counter.clone(),
        };

        let sender = SwqosSender {
            providers: vec![winner, loser],
            bloxroute_quic: None,
            nextblock_quic: None,
            send_semaphore: Arc::new(Semaphore::new(8)),
            health: Arc::new(ProviderHealthTracker::new(3, 30)),
            leader_mapping: LeaderMapping::default(),
        };

        let settings = Settings::default();

        // Drive the REAL send_concurrent path — no manual task spawning.
        let (sig, _results) = sender
            .send_concurrent("dGVzdA==", &settings, None)
            .await
            .expect("send_concurrent should succeed with the fast winner");

        assert_eq!(sig, "fastsig123", "winner signature should be returned first");

        // Give the runtime a moment to process the abort of the loser task.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "loser should have been aborted via AbortHandle::abort() before incrementing the counter"
        );
    }
}

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use base64::engine::general_purpose::STANDARD as Base64Engine;
use base64::Engine;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::keypair::Keypair;
use std::env;
use std::str::FromStr;
use url::Url;

/// A single take-profit level: when profit reaches `trigger_percent`, sell `sell_percent`% of the original position.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct TpLevel {
    pub trigger_percent: f64,
    pub sell_percent: f64,
}

/// A single stop-loss level: when loss reaches `trigger_percent` (negative), sell `sell_percent`% of the original position.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct SlLevel {
    pub trigger_percent: f64,
    pub sell_percent: f64,
}

fn default_tp_levels() -> Vec<TpLevel> {
    vec![TpLevel { trigger_percent: 30.0, sell_percent: 100.0 }]
}

fn default_sl_levels() -> Vec<SlLevel> {
    vec![SlLevel { trigger_percent: -20.0, sell_percent: 100.0 }]
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Settings {
    /// Dry-run mode: if true, no real transactions are sent (simulation only).
    /// Set to false to enable real trading. Can be overridden by --real CLI flag.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
    pub solana_ws_urls: Vec<String>,
    pub solana_rpc_urls: Vec<String>,
    pub pump_fun_program: String,
    pub metadata_program: String,
    /// Wallet whose transactions are copied (base58). Falls back to the
    /// built-in TARGET_WALLET const in main.rs when unset.
    #[serde(default)]
    pub target_wallet: Option<String>,
    #[serde(default)]
    pub wallet_keypair_path: Option<String>,
    #[serde(default)]
    pub wallet_keypair_json: Option<String>,
    #[serde(default)]
    pub wallet_private_key_string: Option<String>,
    #[serde(default)]
    pub simulate_wallet_private_key_string: Option<String>,
    /// Multi-level take-profit configuration (1-4 levels). Sum of sell_percent must be <= 100.
    #[serde(default = "default_tp_levels")]
    pub tp_levels: Vec<TpLevel>,
    /// Multi-level stop-loss configuration (1-4 levels). Sum of sell_percent must be <= 100.
    #[serde(default = "default_sl_levels")]
    pub sl_levels: Vec<SlLevel>,
    pub timeout_secs: i64,
    pub cache_capacity: usize,
    pub price_cache_ttl_secs: u64,
    #[serde(default = "default_buy_amount")]
    pub buy_amount: f64,
    #[serde(default = "default_price_source")]
    pub price_source: String,
    #[serde(default = "default_rotate_rpc")]
    pub rotate_rpc: bool,
    #[serde(default = "default_rpc_rotate_interval_secs")]
    pub rpc_rotate_interval_secs: u64,
    #[serde(default = "default_max_holded_coins")]
    pub max_holded_coins: usize,
    #[serde(default = "default_max_subs_per_wss")]
    pub max_subs_per_wss: usize,
    #[serde(default = "default_sub_ttl_secs")]
    pub sub_ttl_secs: u64,
    #[serde(default = "default_wss_subscribe_timeout_secs")]
    pub wss_subscribe_timeout_secs: u64,
    #[serde(default = "default_max_create_to_buy_secs")]
    pub max_create_to_buy_secs: u64,
    #[serde(default = "default_bonding_curve_strict")]
    pub bonding_curve_strict: bool,
    #[serde(default = "default_bonding_curve_log_debounce_secs")]
    pub bonding_curve_log_debounce_secs: u64,
    #[serde(default)]
    pub simulate_wallet_keypair_json: Option<String>,
    #[serde(default = "default_min_tokens_threshold")]
    pub min_tokens_threshold: u64,
    #[serde(default = "default_max_sol_per_token")]
    pub max_sol_per_token: f64,
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u64,
    #[serde(default = "default_enable_safer_sniping")]
    pub enable_safer_sniping: bool,
    #[serde(default = "default_min_liquidity_sol")]
    pub min_liquidity_sol: f64,
    #[serde(default = "default_max_liquidity_sol")]
    pub max_liquidity_sol: f64,
    // Helius Sender configuration
    #[serde(default)]
    pub helius_sender_enabled: bool,
    #[serde(default)]
    pub helius_api_key: Option<String>,
    #[serde(default = "default_helius_sender_endpoint")]
    pub helius_sender_endpoint: String,
    #[serde(default = "default_helius_min_tip_sol")]
    pub helius_min_tip_sol: f64,
    #[serde(default = "default_helius_priority_fee_multiplier")]
    pub helius_priority_fee_multiplier: f64,
    #[serde(default)]
    pub helius_use_swqos_only: bool,
    #[serde(default = "default_helius_use_dynamic_tips")]
    pub helius_use_dynamic_tips: bool,
    #[serde(default = "default_helius_confirm_timeout_secs")]
    pub helius_confirm_timeout_secs: u64,
    /// 0slot.trade API key for parallel TX sending (Frankfurt: de.0slot.trade)
    #[serde(default)]
    pub zeroslot_api_key: Option<String>,
    /// 0slot.trade Frankfurt endpoints (HTTP, not HTTPS!)
    #[serde(default)]
    pub zeroslot_endpoints: Vec<String>,
    /// 0slot staked_conn: recipient pubkey for mandatory transfer in each TX
    #[serde(default)]
    pub zeroslot_staked_conn_recipient: Option<String>,
    /// 0slot staked_conn: lamports to transfer (default 1_000_000 = 0.001 SOL)
    #[serde(default = "default_zeroslot_staked_conn_lamports")]
    pub zeroslot_staked_conn_lamports: u64,
    /// Nozomi (temporal.xyz) API key for parallel TX sending
    #[serde(default)]
    pub nozomi_api_key: Option<String>,
    /// Nozomi (temporal.xyz) HTTP endpoints (default Frankfurt)
    #[serde(default)]
    pub nozomi_endpoints: Vec<String>,
    #[serde(default = "default_pumpportal_enabled")]
    pub pumpportal_enabled: bool,
    #[serde(default = "default_pumpportal_wss")]
    pub pumpportal_wss: Vec<String>,
    #[serde(default = "default_detected_coins_max")]
    pub detected_coins_max: usize,
    #[serde(default = "default_token_decimals")]
    pub default_token_decimals: u8,
    /// Dev fee configuration: controls whether 1% developer fee is applied to transactions
    #[serde(default = "default_dev_fee_enabled")]
    pub dev_fee_enabled: bool,
    /// Enable fetching IDLs from on-chain (Anchor-style IDL accounts)
    #[serde(default = "default_use_onchain_idl")]
    pub use_onchain_idl: bool,
    /// Optional: Override IDL account pubkeys for specific programs (program_id -> idl_account)
    #[serde(default)]
    pub idl_account_overrides: std::collections::HashMap<String, String>,

    // === Listener Mode ===
    /// Listener mode: "shreds" (default, Jito ShredStream), "websocket", or "geyser" (Yellowstone gRPC)
    #[serde(default = "default_listener_mode")]
    pub listener_mode: String,
    /// Shredstream proxy gRPC URL (used when listener_mode = "shreds")
    #[serde(default = "default_shreds_url")]
    pub shreds_url: String,
    /// Yellowstone Geyser gRPC URL (used when listener_mode = "geyser")
    #[serde(default = "default_geyser_url")]
    pub geyser_url: String,

    // === Mirror Trading ===
    /// If > 0, buy mirror_percent% of what the target spent (overrides buy_amount).
    /// E.g. mirror_percent = 10.0 means spend 10% of target's SOL amount.
    #[serde(default)]
    pub mirror_percent: f64,
    /// If true, allow accumulating into an existing position (buy more of the same mint).
    /// If false, skip duplicate buys.
    #[serde(default)]
    pub allow_accumulate: bool,
    /// If true, mirror the target's sell transactions too (sell when they sell).
    #[serde(default)]
    pub mirror_sells: bool,
    /// Maximum SOL to spend on a single position entry. Caps both fixed buy_amount and
    /// mirror_percent calculations. 0 = no limit.
    #[serde(default)]
    pub max_position_sol: f64,

    // === SWQoS Concurrent Sending ===
    /// Skip simulateTransaction before sending (saves ~200ms per trade).
    /// Only effective when dry_run=false. Skips CU estimation and uses defaults.
    #[serde(default = "default_skip_simulation")]
    pub skip_simulation: bool,

    /// If true, send the same signed TX to multiple providers simultaneously.
    /// First to land wins; others are redundant.
    #[serde(default)]
    pub swqos_concurrent_send: bool,
    /// List of provider names: "default_rpc", "jito", "helius", "bloxroute", "nextblock", "zeroslot", "temporal"
    #[serde(default)]
    pub swqos_providers: Vec<String>,
    /// Jito block engine sendTransaction endpoint (Frankfurt default)
    #[serde(default = "default_swqos_jito_url")]
    pub swqos_jito_url: String,
    /// Maximum number of concurrent in-flight SWQoS sends across all providers.
    /// Bounds task spawn when network problems stall sends. Default: 8.
    #[serde(default = "default_swqos_max_concurrent_sends")]
    pub swqos_max_concurrent_sends: usize,
    /// Number of consecutive failures per provider before opening its circuit.
    /// Open providers are skipped entirely until cooldown elapses. Default: 3.
    #[serde(default = "default_swqos_circuit_breaker_failures")]
    pub swqos_circuit_breaker_failures: usize,
    /// Seconds a provider stays in Open state before transitioning to HalfOpen
    /// (a single probe attempt). Default: 30.
    #[serde(default = "default_swqos_circuit_breaker_cooldown_secs")]
    pub swqos_circuit_breaker_cooldown_secs: u64,

    // === bloXroute Submit-Snipe ===
    /// bloXroute API key (required to activate bloXroute provider)
    #[serde(default)]
    pub bloxroute_api_key: Option<String>,
    /// bloXroute HTTP submit-snipe endpoint
    #[serde(default = "default_bloxroute_http_url")]
    pub bloxroute_http_url: String,
    /// bloXroute QUIC endpoints (multi-region for geographic redundancy)
    #[serde(default = "default_bloxroute_quic_endpoints")]
    pub bloxroute_quic_endpoints: Vec<String>,
    /// bloXroute mTLS client certificate PEM path (QUIC requires client cert)
    #[serde(default)]
    pub bloxroute_client_cert_pem: Option<String>,
    /// bloXroute mTLS client key PEM path
    #[serde(default)]
    pub bloxroute_client_key_pem: Option<String>,

    // === NextBlock QUIC ===
    /// NextBlock API key (required to activate NextBlock provider)
    #[serde(default)]
    pub nextblock_api_key: Option<String>,
    /// NextBlock HTTP endpoint
    #[serde(default = "default_nextblock_endpoint")]
    pub nextblock_endpoint: String,
    /// NextBlock QUIC host (e.g. "mainnet.quic.nextblock.io" or "host:port").
    /// When set, NextBlock provider uses QUIC as primary path, HTTP as fallback.
    #[serde(default)]
    pub nextblock_quic_endpoint: Option<String>,

    // === ZeroSlot provider ===
    /// ZeroSlot API key (required to activate zeroslot provider)
    #[serde(default)]
    pub zeroslot_api_key2: Option<String>,
    /// ZeroSlot HTTP endpoint base URL. Transaction path and api-key appended at runtime.
    #[serde(default = "default_zeroslot_endpoint")]
    pub zeroslot_endpoint: String,

    // === Temporal provider ===
    /// Temporal API key / channel parameter (required to activate temporal provider)
    #[serde(default)]
    pub temporal_api_key2: Option<String>,
    /// Temporal HTTP endpoint base URL. Channel parameter appended at runtime.
    #[serde(default = "default_temporal_endpoint")]
    pub temporal_endpoint: String,

    // === Agave #13267 own-leader strategy ===
    /// Master toggle for leader-aware bundle submission. When false (default),
    /// all behavior is identical to pre-#13267.
    #[serde(default = "default_own_leader_strategy_enabled")]
    pub own_leader_strategy_enabled: bool,
    /// Multiplier applied to the base Jito tip when the submission endpoint
    /// is the current slot leader.
    #[serde(default = "default_own_leader_tip_multiplier")]
    pub own_leader_tip_multiplier: f64,
    /// Multiplier applied to the priority fee when the submission endpoint
    /// is the current slot leader.
    #[serde(default = "default_own_leader_priority_multiplier")]
    pub own_leader_priority_multiplier: f64,
    /// If true, SWQoS will skip providers whose mapped validator identity
    /// matches the current slot leader.
    #[serde(default = "default_swqos_skip_leader_provider")]
    pub swqos_skip_leader_provider: bool,
    /// If true and SWQoS skips all providers because they are the leader,
    /// fall back to direct RPC sendTransaction.
    #[serde(default = "default_own_leader_fallback_direct_rpc")]
    pub own_leader_fallback_direct_rpc: bool,
    /// Mapping of submission endpoints to validator identity pubkeys.
    /// Required for leader-aware tip boost / provider skip to be effective.
    #[serde(default)]
    pub leader_mapping: LeaderMapping,
}

fn default_own_leader_strategy_enabled() -> bool { false }
fn default_own_leader_tip_multiplier() -> f64 { 2.0 }
fn default_own_leader_priority_multiplier() -> f64 { 1.5 }
fn default_swqos_skip_leader_provider() -> bool { false }
fn default_own_leader_fallback_direct_rpc() -> bool { false }

/// Optional mapping of submission endpoints to validator identity pubkeys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LeaderMapping {
    /// Validator identity for the primary Helius Sender endpoint.
    #[serde(default)]
    pub helius_sender_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS Jito block-engine endpoint.
    #[serde(default)]
    pub swqos_jito_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS Helius Sender endpoint.
    #[serde(default)]
    pub swqos_helius_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS default RPC endpoint.
    #[serde(default)]
    pub swqos_default_rpc_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS bloXroute endpoint.
    #[serde(default)]
    pub swqos_bloxroute_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS NextBlock endpoint.
    #[serde(default)]
    pub swqos_nextblock_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS ZeroSlot endpoint.
    #[serde(default)]
    pub swqos_zeroslot_endpoint_owner: Option<String>,
    /// Validator identity for the SWQoS Temporal endpoint.
    #[serde(default)]
    pub swqos_temporal_endpoint_owner: Option<String>,
}

fn default_zeroslot_staked_conn_lamports() -> u64 { 1_000_000 }
fn default_token_decimals() -> u8 { 6 }
fn default_dev_fee_enabled() -> bool { true }
fn default_use_onchain_idl() -> bool { true }

impl Settings {
    pub fn from_file(path: &str) -> Result<Self, AppError> {
        let builder = config::Config::builder()
            .add_source(config::File::with_name(path));
        let cfg = builder.build()?;
        Ok(cfg.try_deserialize()?)
    }

    pub fn save_to_file(&self, path: &str) -> Result<(), AppError> {
        let toml_string = toml::to_string(self)?;
        std::fs::write(path, toml_string)?;
        Ok(())
    }

    /// Merge another Settings struct, only updating fields that differ
    /// This is used for partial updates from API requests
    pub fn merge(&mut self, other: &Settings) {
        // Only update non-default values; use serde_json to detect changes
        if other.solana_rpc_urls != self.solana_rpc_urls {
            self.solana_rpc_urls = other.solana_rpc_urls.clone();
        }
        if other.solana_ws_urls != self.solana_ws_urls {
            self.solana_ws_urls = other.solana_ws_urls.clone();
        }
        if other.pump_fun_program != self.pump_fun_program {
            self.pump_fun_program = other.pump_fun_program.clone();
        }
        if other.metadata_program != self.metadata_program {
            self.metadata_program = other.metadata_program.clone();
        }
        if other.buy_amount != self.buy_amount {
            self.buy_amount = other.buy_amount;
        }
        if other.tp_levels != self.tp_levels {
            self.tp_levels = other.tp_levels.clone();
        }
        if other.sl_levels != self.sl_levels {
            self.sl_levels = other.sl_levels.clone();
        }
        if other.timeout_secs != self.timeout_secs {
            self.timeout_secs = other.timeout_secs;
        }
        if other.price_cache_ttl_secs != self.price_cache_ttl_secs {
            self.price_cache_ttl_secs = other.price_cache_ttl_secs;
        }
        if other.cache_capacity != self.cache_capacity {
            self.cache_capacity = other.cache_capacity;
        }
        if other.max_holded_coins != self.max_holded_coins {
            self.max_holded_coins = other.max_holded_coins;
        }
        if other.price_source != self.price_source {
            self.price_source = other.price_source.clone();
        }
        if other.rpc_rotate_interval_secs != self.rpc_rotate_interval_secs {
            self.rpc_rotate_interval_secs = other.rpc_rotate_interval_secs;
        }
        if other.helius_sender_enabled != self.helius_sender_enabled {
            self.helius_sender_enabled = other.helius_sender_enabled;
        }
        if other.helius_api_key != self.helius_api_key {
            self.helius_api_key = other.helius_api_key.clone();
        }
        if other.helius_sender_endpoint != self.helius_sender_endpoint {
            self.helius_sender_endpoint = other.helius_sender_endpoint.clone();
        }
        if other.helius_use_swqos_only != self.helius_use_swqos_only {
            self.helius_use_swqos_only = other.helius_use_swqos_only;
        }
        if other.helius_use_dynamic_tips != self.helius_use_dynamic_tips {
            self.helius_use_dynamic_tips = other.helius_use_dynamic_tips;
        }
        if other.pumpportal_enabled != self.pumpportal_enabled {
            self.pumpportal_enabled = other.pumpportal_enabled;
        }
        if other.pumpportal_wss != self.pumpportal_wss {
            self.pumpportal_wss = other.pumpportal_wss.clone();
        }
        if other.detected_coins_max != self.detected_coins_max {
            self.detected_coins_max = other.detected_coins_max;
        }
        if other.default_token_decimals != self.default_token_decimals {
            self.default_token_decimals = other.default_token_decimals;
        }
        if other.helius_min_tip_sol != self.helius_min_tip_sol {
            self.helius_min_tip_sol = other.helius_min_tip_sol;
        }
        if other.helius_priority_fee_multiplier != self.helius_priority_fee_multiplier {
            self.helius_priority_fee_multiplier = other.helius_priority_fee_multiplier;
        }

        if other.enable_safer_sniping != self.enable_safer_sniping {
            self.enable_safer_sniping = other.enable_safer_sniping;
        }
        if other.min_tokens_threshold != self.min_tokens_threshold {
            self.min_tokens_threshold = other.min_tokens_threshold;
        }
        if other.max_sol_per_token != self.max_sol_per_token {
            self.max_sol_per_token = other.max_sol_per_token;
        }
        if other.min_liquidity_sol != self.min_liquidity_sol {
            self.min_liquidity_sol = other.min_liquidity_sol;
        }
        if other.max_liquidity_sol != self.max_liquidity_sol {
            self.max_liquidity_sol = other.max_liquidity_sol;
        }
        if other.bonding_curve_strict != self.bonding_curve_strict {
            self.bonding_curve_strict = other.bonding_curve_strict;
        }
        if other.bonding_curve_log_debounce_secs != self.bonding_curve_log_debounce_secs {
            self.bonding_curve_log_debounce_secs = other.bonding_curve_log_debounce_secs;
        }
        if other.slippage_bps != self.slippage_bps {
            self.slippage_bps = other.slippage_bps;
        }
        if other.wallet_keypair_path != self.wallet_keypair_path {
            self.wallet_keypair_path = other.wallet_keypair_path.clone();
        }
        if other.wallet_keypair_json != self.wallet_keypair_json {
            self.wallet_keypair_json = other.wallet_keypair_json.clone();
        }
        if other.wallet_private_key_string != self.wallet_private_key_string {
            self.wallet_private_key_string = other.wallet_private_key_string.clone();
        }
        if other.simulate_wallet_keypair_json != self.simulate_wallet_keypair_json {
            self.simulate_wallet_keypair_json = other.simulate_wallet_keypair_json.clone();
        }
        if other.simulate_wallet_private_key_string != self.simulate_wallet_private_key_string {
            self.simulate_wallet_private_key_string = other.simulate_wallet_private_key_string.clone();
        }
        if other.dev_fee_enabled != self.dev_fee_enabled {
            self.dev_fee_enabled = other.dev_fee_enabled;
        }
        if other.use_onchain_idl != self.use_onchain_idl {
            self.use_onchain_idl = other.use_onchain_idl;
        }
        if other.idl_account_overrides != self.idl_account_overrides {
            self.idl_account_overrides = other.idl_account_overrides.clone();
        }
        if other.listener_mode != self.listener_mode {
            self.listener_mode = other.listener_mode.clone();
        }
        if other.shreds_url != self.shreds_url {
            self.shreds_url = other.shreds_url.clone();
        }
        if other.geyser_url != self.geyser_url {
            self.geyser_url = other.geyser_url.clone();
        }
        if other.mirror_percent != self.mirror_percent {
            self.mirror_percent = other.mirror_percent;
        }
        if other.allow_accumulate != self.allow_accumulate {
            self.allow_accumulate = other.allow_accumulate;
        }
        if other.mirror_sells != self.mirror_sells {
            self.mirror_sells = other.mirror_sells;
        }
        if other.own_leader_strategy_enabled != self.own_leader_strategy_enabled {
            self.own_leader_strategy_enabled = other.own_leader_strategy_enabled;
        }
        if other.own_leader_tip_multiplier != self.own_leader_tip_multiplier {
            self.own_leader_tip_multiplier = other.own_leader_tip_multiplier;
        }
        if other.own_leader_priority_multiplier != self.own_leader_priority_multiplier {
            self.own_leader_priority_multiplier = other.own_leader_priority_multiplier;
        }
        if other.swqos_skip_leader_provider != self.swqos_skip_leader_provider {
            self.swqos_skip_leader_provider = other.swqos_skip_leader_provider;
        }
        if other.own_leader_fallback_direct_rpc != self.own_leader_fallback_direct_rpc {
            self.own_leader_fallback_direct_rpc = other.own_leader_fallback_direct_rpc;
        }
        if other.leader_mapping != self.leader_mapping {
            self.leader_mapping = other.leader_mapping.clone();
        }
        if other.max_position_sol != self.max_position_sol {
            self.max_position_sol = other.max_position_sol;
        }
    }

    /// Validate settings ranges and constraints
    pub fn validate(&self) -> Result<(), AppError> {
        // Validate TP levels
        if self.tp_levels.is_empty() {
            return Err(AppError::Validation("At least one TP level is required".to_string()));
        }
        if self.tp_levels.len() > 4 {
            return Err(AppError::Validation("Maximum 4 TP levels allowed".to_string()));
        }
        let mut tp_sell_sum = 0.0;
        for (i, level) in self.tp_levels.iter().enumerate() {
            if level.trigger_percent <= 0.0 {
                return Err(AppError::Validation(format!("TP level {} trigger_percent must be > 0", i + 1)));
            }
            if level.sell_percent <= 0.0 || level.sell_percent > 100.0 {
                return Err(AppError::Validation(format!("TP level {} sell_percent must be between 0 and 100", i + 1)));
            }
            tp_sell_sum += level.sell_percent;
        }
        if tp_sell_sum > 100.0 + f64::EPSILON {
            return Err(AppError::Validation(format!("TP levels sell_percent sum ({:.1}%) must be <= 100%", tp_sell_sum)));
        }

        // Validate SL levels
        if self.sl_levels.is_empty() {
            return Err(AppError::Validation("At least one SL level is required".to_string()));
        }
        if self.sl_levels.len() > 4 {
            return Err(AppError::Validation("Maximum 4 SL levels allowed".to_string()));
        }
        let mut sl_sell_sum = 0.0;
        for (i, level) in self.sl_levels.iter().enumerate() {
            if level.trigger_percent >= 0.0 {
                return Err(AppError::Validation(format!("SL level {} trigger_percent must be < 0", i + 1)));
            }
            if level.sell_percent <= 0.0 || level.sell_percent > 100.0 {
                return Err(AppError::Validation(format!("SL level {} sell_percent must be between 0 and 100", i + 1)));
            }
            sl_sell_sum += level.sell_percent;
        }
        if sl_sell_sum > 100.0 + f64::EPSILON {
            return Err(AppError::Validation(format!("SL levels sell_percent sum ({:.1}%) must be <= 100%", sl_sell_sum)));
        }

        if self.buy_amount <= 0.0 {
            return Err(AppError::Validation("buy_amount must be > 0".to_string()));
        }
        if self.timeout_secs <= 0 {
            return Err(AppError::Validation("timeout_secs must be > 0".to_string()));
        }
        if self.cache_capacity == 0 {
            return Err(AppError::Validation("cache_capacity must be > 0".to_string()));
        }
        if self.max_holded_coins == 0 {
            return Err(AppError::Validation("max_holded_coins must be > 0".to_string()));
        }
        if self.max_liquidity_sol < self.min_liquidity_sol {
            return Err(AppError::Validation("max_liquidity_sol must be >= min_liquidity_sol".to_string()));
        }

        Pubkey::from_str(&self.pump_fun_program)
            .map_err(|e| AppError::Validation(format!("invalid pump_fun_program: {}", e)))?;

        // Validate keypairs eagerly — catch typos, corruption, or wrong
        // formats at startup instead of failing silently at transaction time.
        self.validate_keypairs()?;

        // Validate URL fields — catch empty, malformed, or scheme-less URLs
        // at startup instead of failing at connection time.
        self.validate_urls()?;

        Ok(())
    }

    /// Validate that required URL fields are present and well-formed.
    ///
    /// Checks each URL for:
    ///   - Not empty (for required fields) or blank
    ///   - Valid `url::Url` parse
    ///   - Has a scheme (`http`, `https`, `wss`, `ws`, or custom like `grpc`)
    ///   - Has a host (domain or IP)
    ///
    /// Optional URL fields (Option<String>) are only validated when present.
    fn validate_urls(&self) -> Result<(), AppError> {
        // Required HTTP/HTTPS RPC URLs
        for (i, url) in self.solana_rpc_urls.iter().enumerate() {
            validate_http_url(url, &format!("solana_rpc_urls[{}]", i))?;
        }
        if self.solana_rpc_urls.is_empty() {
            return Err(AppError::Validation(
                "solana_rpc_urls must contain at least one RPC URL".to_string(),
            ));
        }

        // Required WSS URLs
        for (i, url) in self.solana_ws_urls.iter().enumerate() {
            validate_ws_url(url, &format!("solana_ws_urls[{}]", i))?;
        }
        if self.solana_ws_urls.is_empty() {
            return Err(AppError::Validation(
                "solana_ws_urls must contain at least one WebSocket URL".to_string(),
            ));
        }

        // Required single-URL string fields (HTTP/HTTPS)
        validate_http_url(&self.helius_sender_endpoint, "helius_sender_endpoint")?;
        validate_http_url(&self.swqos_jito_url, "swqos_jito_url")?;
        validate_http_url(&self.bloxroute_http_url, "bloxroute_http_url")?;
        validate_http_url(&self.nextblock_endpoint, "nextblock_endpoint")?;
        validate_non_empty_url(&self.shreds_url, "shreds_url")?;
        validate_non_empty_url(&self.geyser_url, "geyser_url")?;

        // Optional single-URL fields — validate only when present
        if let Some(ref url) = self.nextblock_quic_endpoint {
            validate_non_empty_url(url, "nextblock_quic_endpoint")?;
        }

        // PumpPortal WSS URLs
        if self.pumpportal_enabled {
            for (i, url) in self.pumpportal_wss.iter().enumerate() {
                validate_ws_url(url, &format!("pumpportal_wss[{}]", i))?;
            }
            if self.pumpportal_wss.is_empty() {
                return Err(AppError::Validation(
                    "pumpportal_enabled is true but pumpportal_wss is empty".to_string(),
                ));
            }
        }

        // 0slot endpoints (HTTP)
        for (i, url) in self.zeroslot_endpoints.iter().enumerate() {
            validate_http_url(url, &format!("zeroslot_endpoints[{}]", i))?;
        }

        Ok(())
    }

    /// Validate all private key / keypair fields at load time.
    ///
    /// Each key is tested via the same parse path used at runtime:
    ///   - `wallet_private_key_string`  → parse_private_key_string → Keypair::try_from
    ///   - `wallet_keypair_json`        → serde_json parse         → Keypair::try_from
    ///   - `wallet_keypair_path`        → read file, serde_json    → Keypair::try_from
    ///   - `simulate_wallet_private_key_string` → parse_private_key_string → Keypair::try_from
    ///   - `simulate_wallet_keypair_json`       → serde_json parse         → Keypair::try_from
    ///
    /// Missing / None fields are skipped (they are optional). Invalid keys
    /// that are present produce an AppError::InvalidKeypair immediately.
    fn validate_keypairs(&self) -> Result<(), AppError> {
        if let Some(ref pk_str) = self.wallet_private_key_string {
            let bytes = parse_private_key_string(pk_str)
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_private_key_string: parse failed — {}", e)
                ))?;
            Keypair::try_from(bytes.as_slice())
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_private_key_string: invalid keypair — {}", e)
                ))?;
        }

        if let Some(ref json_str) = self.wallet_keypair_json {
            let bytes: Vec<u8> = serde_json::from_str(json_str)
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_keypair_json: JSON parse failed — {}", e)
                ))?;
            Keypair::try_from(bytes.as_slice())
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_keypair_json: invalid keypair — {}", e)
                ))?;
        }

        if let Some(ref path) = self.wallet_keypair_path {
            let content = std::fs::read_to_string(path)
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_keypair_path: cannot read file '{}' — {}", path, e)
                ))?;
            let bytes: Vec<u8> = serde_json::from_str(&content)
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_keypair_path: JSON parse failed for '{}' — {}", path, e)
                ))?;
            Keypair::try_from(bytes.as_slice())
                .map_err(|e| AppError::InvalidKeypair(
                    format!("wallet_keypair_path: invalid keypair in '{}' — {}", path, e)
                ))?;
        }

        if let Some(ref pk_str) = self.simulate_wallet_private_key_string {
            let bytes = parse_private_key_string(pk_str)
                .map_err(|e| AppError::InvalidKeypair(
                    format!("simulate_wallet_private_key_string: parse failed — {}", e)
                ))?;
            Keypair::try_from(bytes.as_slice())
                .map_err(|e| AppError::InvalidKeypair(
                    format!("simulate_wallet_private_key_string: invalid keypair — {}", e)
                ))?;
        }

        if let Some(ref json_str) = self.simulate_wallet_keypair_json {
            let bytes: Vec<u8> = serde_json::from_str(json_str)
                .map_err(|e| AppError::InvalidKeypair(
                    format!("simulate_wallet_keypair_json: JSON parse failed — {}", e)
                ))?;
            Keypair::try_from(bytes.as_slice())
                .map_err(|e| AppError::InvalidKeypair(
                    format!("simulate_wallet_keypair_json: invalid keypair — {}", e)
                ))?;
        }

        Ok(())
    }
}

/// Try to read a base64-encoded keypair from the given env var. Returns
/// the raw decoded bytes if present and valid, otherwise None.
pub fn load_keypair_from_env_var(var: &str) -> Option<Vec<u8>> {
    if let Ok(s) = env::var(var) {
        match Base64Engine.decode(&s) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                eprintln!("Failed to decode {}: {}", var, e);
                None
            }
        }
    } else {
        None
    }
}

/// Parse a private key string in various formats:
/// - Base58 (standard Solana format, 88 chars)
/// - JSON array string like "[1,2,3,...]" 
/// - Comma-separated bytes like "1,2,3,..."
pub fn parse_private_key_string(s: &str) -> Result<Vec<u8>, String> {
    let trimmed = s.trim();
    
    // Try base58 first (most common format)
    if trimmed.len() >= 80 && !trimmed.starts_with('[') && !trimmed.contains(',') {
        return bs58::decode(trimmed)
            .into_vec()
            .map_err(|e| format!("Base58 decode failed: {}", e));
    }
    
    // Try JSON array format: [1,2,3,...]
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<u8>>(trimmed)
            .map_err(|e| format!("JSON parse failed: {}", e));
    }
    
    // Try comma-separated format: 1,2,3,...
    if trimmed.contains(',') {
        let parts: Result<Vec<u8>, _> = trimmed
            .split(',')
            .map(|s| s.trim().parse::<u8>())
            .collect();
        return parts.map_err(|e| format!("CSV parse failed: {}", e));
    }
    
    Err("Unrecognized private key format. Expected: base58, JSON array, or comma-separated bytes".to_string())
}

fn default_bonding_curve_strict() -> bool { false }
fn default_bonding_curve_log_debounce_secs() -> u64 { 300 }
fn default_buy_amount() -> f64 { 0.1 }
fn default_price_source() -> String { "wss".to_string() }
fn default_rotate_rpc() -> bool { true }
fn default_rpc_rotate_interval_secs() -> u64 { 60 }
fn default_max_holded_coins() -> usize { 100 }
fn default_max_subs_per_wss() -> usize { 4 }
fn default_sub_ttl_secs() -> u64 { 900 }
fn default_wss_subscribe_timeout_secs() -> u64 { 6 }
fn default_max_create_to_buy_secs() -> u64 { 6 }
fn default_min_tokens_threshold() -> u64 { 1_000_000 }
fn default_max_sol_per_token() -> f64 { 0.0001 }
fn default_slippage_bps() -> u64 { 500 }
fn default_enable_safer_sniping() -> bool { false }
fn default_min_liquidity_sol() -> f64 { 0.0 }
fn default_max_liquidity_sol() -> f64 { 100.0 }
fn default_helius_sender_endpoint() -> String { "https://sender.helius-rpc.com/fast".to_string() }
fn default_helius_min_tip_sol() -> f64 { 0.001 }
fn default_helius_priority_fee_multiplier() -> f64 { 1.2 }
fn default_helius_use_dynamic_tips() -> bool { true }
fn default_helius_confirm_timeout_secs() -> u64 { 15 }
fn default_skip_simulation() -> bool { true }
fn default_dry_run() -> bool { true }
fn default_swqos_jito_url() -> String { "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions".to_string() }
fn default_swqos_max_concurrent_sends() -> usize { 8 }
fn default_swqos_circuit_breaker_failures() -> usize { 3 }
fn default_swqos_circuit_breaker_cooldown_secs() -> u64 { 30 }
fn default_bloxroute_http_url() -> String { "https://uk.solana.dex.blxrbdn.com/api/v2/submit-snipe".to_string() }
fn default_bloxroute_quic_endpoints() -> Vec<String> {
    // bloXroute QUIC endpoints across regions (TLD: blxrbdn.com, NOT blxrdev.network)
    vec![
        "uk.solana.dex.blxrbdn.com".to_string(),         // UK (London)
        "ny.solana.dex.blxrbdn.com".to_string(),         // New York
        "la.solana.dex.blxrbdn.com".to_string(),         // Los Angeles
        "germany.solana.dex.blxrbdn.com".to_string(),   // Germany (Frankfurt)
        "amsterdam.solana.dex.blxrbdn.com".to_string(),  // Amsterdam
        "tokyo.solana.dex.blxrbdn.com".to_string(),      // Tokyo
    ]
}
fn default_nextblock_endpoint() -> String { "https://frankfurt.nextblock.io/api/v2/submit".to_string() }
fn default_zeroslot_endpoint() -> String { "https://de.0slot.trade/api".to_string() }
fn default_temporal_endpoint() -> String { "http://fra2.nozomi.temporal.xyz".to_string() }
fn default_pumpportal_enabled() -> bool { false }
fn default_pumpportal_wss() -> Vec<String> { vec!["wss://pumpportal.fun/api/data".to_string()] }

fn default_detected_coins_max() -> usize { 300 }
fn default_listener_mode() -> String { "shreds".to_string() }
// Defaults are placeholders; set real ShredStream/Geyser endpoints in config.toml
// (or via the API-key panel). See INSTALL.md for provider options.
fn default_shreds_url() -> String { "http://placeholder.example.com:9999".to_string() }
fn default_geyser_url() -> String { "http://placeholder.example.com:10000".to_string() }

impl Settings {
    /// Get the effective minimum tip amount based on routing mode
    /// - Default dual routing: uses configured helius_min_tip_sol (default 0.001 SOL)
    /// - SWQOS-only: uses minimum 0.000005 SOL unless helius_min_tip_sol is higher
    pub fn swqos_provider_names(&self) -> String {
        if self.swqos_providers.is_empty() { "none".to_string() }
        else { self.swqos_providers.join(", ") }
    }

    pub fn get_effective_min_tip_sol(&self) -> f64 {
        if self.helius_use_swqos_only {
            // SWQOS-only minimum is 0.000005 SOL, but respect user's higher setting
            self.helius_min_tip_sol.max(0.000005)
        } else {
            // Default dual routing minimum is 0.001 SOL
            self.helius_min_tip_sol.max(0.001)
        }
    }
}

/// Validate that a URL string is non-empty, parses as a valid URL, has a scheme,
/// and has a host. Accepts any scheme (http, https, ws, wss, grpc, etc.).
fn validate_non_empty_url(raw: &str, field: &str) -> Result<(), AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation(format!(
            "{} must not be empty", field
        )));
    }
    let parsed = Url::parse(trimmed).map_err(|e| AppError::Validation(format!(
        "{}: invalid URL '{}': {}", field, trimmed, e
    )))?;
    if parsed.scheme().is_empty() {
        return Err(AppError::Validation(format!(
            "{}: URL '{}' is missing a scheme (expected http://, https://, wss://, etc.)", field, trimmed
        )));
    }
    if parsed.host_str().is_none() || parsed.host_str().unwrap_or("").is_empty() {
        return Err(AppError::Validation(format!(
            "{}: URL '{}' is missing a host", field, trimmed
        )));
    }
    Ok(())
}

/// Validate that a URL string is a well-formed HTTP/HTTPS URL.
/// Requires scheme to be "http" or "https" and host to be present.
fn validate_http_url(raw: &str, field: &str) -> Result<(), AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation(format!(
            "{} must not be empty", field
        )));
    }
    let parsed = Url::parse(trimmed).map_err(|e| AppError::Validation(format!(
        "{}: invalid URL '{}': {}", field, trimmed, e
    )))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(AppError::Validation(format!(
            "{}: URL '{}' must use http:// or https:// scheme (got '{}')", field, trimmed, scheme
        )));
    }
    if parsed.host_str().is_none() || parsed.host_str().unwrap_or("").is_empty() {
        return Err(AppError::Validation(format!(
            "{}: URL '{}' is missing a host", field, trimmed
        )));
    }
    Ok(())
}

/// Validate that a URL string is a well-formed WebSocket URL.
/// Requires scheme to be "ws" or "wss" and host to be present.
fn validate_ws_url(raw: &str, field: &str) -> Result<(), AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation(format!(
            "{} must not be empty", field
        )));
    }
    let parsed = Url::parse(trimmed).map_err(|e| AppError::Validation(format!(
        "{}: invalid URL '{}': {}", field, trimmed, e
    )))?;
    let scheme = parsed.scheme();
    if scheme != "ws" && scheme != "wss" {
        return Err(AppError::Validation(format!(
            "{}: URL '{}' must use ws:// or wss:// scheme (got '{}')", field, trimmed, scheme
        )));
    }
    if parsed.host_str().is_none() || parsed.host_str().unwrap_or("").is_empty() {
        return Err(AppError::Validation(format!(
            "{}: URL '{}' is missing a host", field, trimmed
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_example_config() {
        // This test validates that `Settings::from_file` can load the example
        // config without panicking and that a couple of fields match expected
        // placeholder values from `config.example.toml`.
        let s = Settings::from_file("config.example.toml").unwrap();
        assert_eq!(s.tp_levels.len(), 2);
        assert_eq!(s.tp_levels[0].trigger_percent, 30.0);
        assert_eq!(s.tp_levels[0].sell_percent, 50.0);
        assert_eq!(s.tp_levels[1].trigger_percent, 100.0);
        assert_eq!(s.tp_levels[1].sell_percent, 50.0);
        assert_eq!(s.sl_levels.len(), 1);
        assert_eq!(s.sl_levels[0].trigger_percent, -20.0);
        assert_eq!(s.cache_capacity, 1024);
    }

    // ── URL validation unit tests ──

    #[test]
    fn test_validate_http_url_accepts_valid() {
        assert!(validate_http_url("https://api.example.com/rpc", "test_field").is_ok());
        assert!(validate_http_url("http://localhost:8899", "test_field").is_ok());
        assert!(validate_http_url("https://rpc.example.com?api-key=abc", "test_field").is_ok());
    }

    #[test]
    fn test_validate_http_url_rejects_empty() {
        let err = validate_http_url("", "rpc_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("rpc_url") && msg.contains("empty")),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_http_url_rejects_wrong_scheme() {
        let err = validate_http_url("wss://example.com", "rpc_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("http") || msg.contains("https"), "msg: {}", msg),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_http_url_rejects_no_scheme() {
        let err = validate_http_url("example.com/rpc", "rpc_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("invalid URL") || msg.contains("scheme"), "msg: {}", msg),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_http_url_rejects_no_host() {
        let err = validate_http_url("https://", "rpc_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("host"), "msg: {}", msg),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_ws_url_accepts_valid() {
        assert!(validate_ws_url("wss://example.com/api/data", "test_field").is_ok());
        assert!(validate_ws_url("ws://localhost:8080", "test_field").is_ok());
        assert!(validate_ws_url("wss://rpc.example.com/?api-key=abc", "test_field").is_ok());
    }

    #[test]
    fn test_validate_ws_url_rejects_http_scheme() {
        let err = validate_ws_url("https://example.com", "wss_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("ws") || msg.contains("wss"), "msg: {}", msg),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_ws_url_rejects_empty() {
        let err = validate_ws_url("", "wss_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("empty")),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_non_empty_url_accepts_various_schemes() {
        // Shreds URL uses http
        assert!(validate_non_empty_url("http://shreds.example.com", "shreds_url").is_ok());
        // gRPC URL uses http (custom gRPC over HTTP/2)
        assert!(validate_non_empty_url("http://grpc.example.com", "geyser_url").is_ok());
        // HTTPS also accepted
        assert!(validate_non_empty_url("https://example.com", "test").is_ok());
        // wss also accepted
        assert!(validate_non_empty_url("wss://example.com", "test").is_ok());
    }

    #[test]
    fn test_validate_non_empty_url_rejects_empty() {
        let err = validate_non_empty_url("", "shreds_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("empty")),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_non_empty_url_rejects_no_host() {
        let err = validate_non_empty_url("https://", "shreds_url").unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(msg.contains("host")),
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_non_empty_url_rejects_no_scheme() {
        let err = validate_non_empty_url("just-a-hostname", "shreds_url").unwrap_err();
        match err {
            AppError::Validation(msg) => {
                // url crate may parse "just-a-hostname" as scheme=no_scheme, host=None
                // Either way it should be rejected
                assert!(msg.contains("scheme") || msg.contains("host") || msg.contains("invalid"), "msg: {}", msg);
            }
            other => panic!("expected Validation error, got {:?}", other),
        }
    }

    #[test]
    fn test_url_validation_wss_with_slash_and_query() {
        // Regression: WSS URL with slash before query string?api-key=... must be valid
        assert!(validate_ws_url("wss://rpc.example.com/?api-key=secret", "solana_ws_urls[0]").is_ok());
    }

    #[test]
    fn test_url_validation_rpc_with_query_string() {
        // Regression: HTTP URL with query string (no trailing slash before ?)
        assert!(validate_http_url("https://rpc.example.com?api-key=secret", "solana_rpc_urls[0]").is_ok());
    }

    // --- Keypair validation tests ---

    /// Helper: build a minimal Settings with all required fields for testing.
    fn default_test_settings() -> Settings {
        Settings::from_file("config.example.toml").expect("config.example.toml must load for tests")
    }

    #[test]
    fn test_validate_keypairs_skips_none_fields() {
        let s = default_test_settings();
        // All keypair fields are None by default — validation should succeed (nothing to check)
        assert!(s.validate_keypairs().is_ok());
    }

    #[test]
    fn test_validate_keypairs_rejects_invalid_base58() {
        let mut s = default_test_settings();
        s.wallet_private_key_string = Some("not_a_valid_base58_key_at_all".to_string());
        let err = s.validate_keypairs().unwrap_err();
        match err {
            AppError::InvalidKeypair(msg) => assert!(msg.contains("wallet_private_key_string"), "Error should mention the field: {}", msg),
            other => panic!("expected InvalidKeypair, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_keypairs_rejects_invalid_json() {
        let mut s = default_test_settings();
        s.wallet_keypair_json = Some("not json at all".to_string());
        let err = s.validate_keypairs().unwrap_err();
        match err {
            AppError::InvalidKeypair(msg) => assert!(msg.contains("JSON parse failed"), "msg: {}", msg),
            other => panic!("expected InvalidKeypair, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_keypairs_rejects_wrong_length_json() {
        // Valid JSON array but wrong number of bytes (not 64 for a keypair)
        let mut s = default_test_settings();
        s.wallet_keypair_json = Some("[1, 2, 3]".to_string());
        let err = s.validate_keypairs().unwrap_err();
        match err {
            AppError::InvalidKeypair(msg) => assert!(msg.contains("invalid keypair"), "msg: {}", msg),
            other => panic!("expected InvalidKeypair, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_keypairs_simulate_wallet_rejects_invalid() {
        let mut s = default_test_settings();
        s.simulate_wallet_private_key_string = Some("garbage_key".to_string());
        let err = s.validate_keypairs().unwrap_err();
        match err {
            AppError::InvalidKeypair(msg) => assert!(msg.contains("simulate_wallet_private_key_string"), "msg: {}", msg),
            other => panic!("expected InvalidKeypair, got {:?}", other),
        }
    }
}
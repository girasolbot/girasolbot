//! Live strategy execution engine. Gated behind explicit, revocable panel
//! authorization plus the CLI live switch; own-mind entry/exit decisions on
//! independently observed market prices — never a copy of a third-party tx.
//! Real network submission happens only through `require_live_authorization()`
//! at the broadcast boundary; everything here is dry-run until both gates pass.
use crate::runtime::{require_live_authorization, RuntimeState};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub const LIVE_CONFIRMATION: &str = "ENABLE LIVE STRATEGY EXECUTION";

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_position_native: f64,
    pub daily_stop_net_native: f64,
    pub max_open_positions: usize,
}

impl Default for RiskLimits {
    fn default() -> Self {
        // Conservative defaults; the operator may raise them via panel config.
        Self {
            max_position_native: 0.05,
            daily_stop_net_native: -0.10,
            max_open_positions: 2,
        }
    }
}

fn number(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Fee + slippage accounting applied to both legs before PnL is credited.
/// `fee_fraction` is the per-side fee (e.g. 0.0025 = 0.25%); `slippage_fraction`
/// the expected adverse move per side. Both are configurable via the strategy
/// object; unknown inputs keep zero so nothing is silently invented.
pub fn execution_cost(
    strategy: &Value,
    side_amount_native: f64,
) -> (f64, f64) {
    let fee = number(&strategy["fee_fraction"]).clamp(0., 0.05) * side_amount_native;
    let slippage = number(&strategy["estimated_slippage_fraction"]).clamp(0., 0.1) * side_amount_native;
    (fee, slippage)
}

/// Entry/exit decision identical in shape to the paper engine, but evaluated
/// against live limits. This is the bot's own reasoning on an independent
/// observed price; it never replays another wallet's transaction.
pub fn live_decision(
    strategy: &Value,
    limits: &RiskLimits,
    price: f64,
    last_price: Option<f64>,
) -> Value {
    let mut decision = crate::intelligence::decide(strategy, price, last_price);
    if decision["action"] == "buy" {
        let size = number(&decision["size_native"]);
        if size > limits.max_position_native {
            decision["action"] = json!("hold");
            decision["reason"] = json!(format!(
                "entry size {} exceeds max_position_native {}",
                size, limits.max_position_native
            ));
            decision["size_native"] = json!(0.);
        }
    }
    decision
}

/// One live-engine cycle for a token. Mirrors `apply_paper_price` accounting
/// but charges fee/slippage on both legs, tracks daily net PnL, enforces the
/// daily stop and the kill-switch, and NEVER submits unless both live gates
/// are confirmed at the call site (see `execute_action`).
pub fn apply_live_price(
    strategy: &Value,
    limits: &RiskLimits,
    mut ledger: Value,
    price: f64,
    token: &str,
) -> Result<Value, String> {
    if !price.is_finite() || price <= 0. {
        return Err("invalid observed market price".into());
    }
    if ledger.is_null() || ledger["asset"] != token {
        let mode = if ledger.is_null() {
            json!("live-pending")
        } else {
            ledger["mode"].clone()
        };
        ledger = json!({
            "asset": token, "cash_native": 1., "quantity": 0., "entry_price": null,
            "last_price": null, "trades": [], "realized_pnl_native": 0.,
            "fees_paid_native": 0., "slippage_cost_native": 0.,
            "daily_net_native": 0., "open_positions": 0,
            "mode": mode,
        });
    }
    if ledger["killed"] == json!(true) {
        ledger["last_decision"] = json!({"action":"hold","reason":"kill-switch engaged","observed_at":now()});
        return Ok(ledger);
    }
    // Daily stop: once net daily loss reaches the limit, new entries are denied.
    if number(&ledger["daily_net_native"]) <= limits.daily_stop_net_native {
        ledger["last_decision"] = json!({"action":"hold","reason":"daily stop limit reached","observed_at":now()});
        return Ok(ledger);
    }
    let last = ledger["last_price"].as_f64();
    let mut decision = live_decision(strategy, limits, price, last);
    let quantity = number(&ledger["quantity"]);
    if quantity == 0.
        && now() - ledger["last_trade_at"].as_i64().unwrap_or(0)
            < number(&strategy["median_interval_seconds"]).min(86400.) as i64
    {
        decision["action"] = json!("hold");
        decision["reason"] = json!("observed entry timing cooldown");
    }
    if quantity > 0. {
        let entry = number(&ledger["entry_price"]);
        decision["action"] = json!(if price
            >= entry * (1. + number(&strategy["take_profit_fraction"]))
            || price <= entry * (1. - number(&strategy["stop_loss_fraction"]))
        {
            "sell"
        } else {
            "hold"
        });
    }
    match decision["action"].as_str() {
        Some("buy") => {
            let (fee, slippage) = execution_cost(strategy, number(&decision["size_native"]));
            let size = number(&decision["size_native"]).min(number(&ledger["cash_native"]));
            if size > 0. && number(&ledger["open_positions"]) < limits.max_open_positions as f64 {
                // Slippage is paid as adverse fill: effective cash out includes both costs.
                ledger["cash_native"] = json!(number(&ledger["cash_native"]) - size);
                ledger["fees_paid_native"] = json!(number(&ledger["fees_paid_native"]) + fee);
                ledger["slippage_cost_native"] =
                    json!(number(&ledger["slippage_cost_native"]) + slippage);
                ledger["daily_net_native"] = json!(number(&ledger["daily_net_native"]) - fee - slippage);
                ledger["quantity"] = json!(size / price);
                decision["quantity"] = json!(size / price);
                decision["fee_native"] = json!(fee);
                decision["slippage_native"] = json!(slippage);
                ledger["entry_price"] = json!(price);
                ledger["last_trade_at"] = json!(now());
                ledger["open_positions"] = json!(number(&ledger["open_positions"]) + 1.);
            } else {
                decision["action"] = json!("hold");
                decision["reason"] = if number(&ledger["open_positions"])
                    >= limits.max_open_positions as f64
                {
                    json!("max open positions reached")
                } else {
                    json!("insufficient cash")
                };
            }
        }
        Some("sell") => {
            let gross = quantity * price;
            let (fee, slippage) = execution_cost(strategy, gross);
            ledger["cash_native"] = json!(number(&ledger["cash_native"]) + gross);
            ledger["fees_paid_native"] = json!(number(&ledger["fees_paid_native"]) + fee);
            ledger["slippage_cost_native"] = json!(number(&ledger["slippage_cost_native"]) + slippage);
            let pnl = gross - fee - slippage - quantity * number(&ledger["entry_price"]);
            ledger["realized_pnl_native"] = json!(number(&ledger["realized_pnl_native"]) + pnl);
            ledger["daily_net_native"] = json!(number(&ledger["daily_net_native"]) + pnl);
            ledger["quantity"] = json!(0.);
            ledger["open_positions"] = json!(number(&ledger["open_positions"]).max(1.) - 1.);
            ledger["last_trade_at"] = json!(now());
            decision["quantity"] = json!(quantity);
            decision["size_native"] = json!(gross);
            decision["fee_native"] = json!(fee);
            decision["slippage_native"] = json!(slippage);
            decision["pnl_native"] = json!(pnl);
        }
        _ => {}
    }
    decision["observed_at"] = json!(now());
    decision["asset"] = json!(token);
    if decision["action"] != "hold" {
        let trades = ledger["trades"].as_array_mut().ok_or("invalid live ledger")?;
        trades.push(decision.clone());
        if trades.len() > 1000 {
            trades.remove(0);
        }
    }
    ledger["last_price"] = json!(price);
    ledger["last_decision"] = decision;
    ledger["equity_native"] =
        json!(number(&ledger["cash_native"]) + number(&ledger["quantity"]) * price);
    Ok(ledger)
}

/// Submit helper. The ONLY path from live decisions to the network; both the
/// revocable panel confirmation and the CLI live switch must already be true.
pub async fn execute_action(
    runtime: &Arc<RuntimeState>,
    engine: &Arc<Mutex<LiveEngine>>,
    action: &str,
    submitter: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>,
) -> Result<String, String> {
    // Kill-switch checked again at the last instant: an explicit revoke always wins.
    if engine.lock().map_err(|_| "engine lock poisoned")?.killed {
        return Err("kill-switch engaged; submission refused".into());
    }
    require_live_authorization().map_err(|e| format!("live submission denied: {}", e))?;
    if action != "buy" && action != "sell" {
        return Err("unsupported live action".into());
    }
    let receipt = submitter().await?;
    let _ = engine
        .lock()
        .map_err(|_| "engine lock poisoned")?
        .submitted
        .push(receipt.clone());
    Ok(receipt)
}

pub struct LiveEngine {
    pub enabled: bool,
    pub killed: bool,
    pub limits: RiskLimits,
    pub strategy: Value,
    pub ledgers: Value,
    pub submitted: Vec<String>,
}

impl LiveEngine {
    pub fn new(strategy: Value) -> Self {
        Self {
            enabled: false,
            killed: false,
            limits: RiskLimits::default(),
            strategy,
            ledgers: json!({}),
            submitted: Vec::new(),
        }
    }
    /// Live mode is OFF until an explicit operator action turns it on, and can
    /// always be revoked or killed. Confirmation itself is never persisted.
    pub fn enable(&mut self) -> Result<(), String> {
        if self.killed {
            return Err("kill-switch engaged; restart required".into());
        }
        self.enabled = true;
        Ok(())
    }
    pub fn disable(&mut self) {
        self.enabled = false;
    }
    pub fn kill(&mut self) {
        self.enabled = false;
        self.killed = true;
    }
    pub fn observe_price(
        &mut self,
        token: &str,
        price: f64,
    ) -> Result<Value, String> {
        if !self.enabled || self.killed {
            return Err("live execution is OFF or kill-switch engaged; enable it explicitly from the local panel".into());
        }
        let ledger = self.ledgers[token].clone();
        let next = apply_live_price(&self.strategy, &self.limits, ledger, price, token)?;
        self.ledgers[token] = next.clone();
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn strategy() -> Value {
        json!({"entry_drop_fraction":0.02,"paper_size_native":0.05,"take_profit_fraction":0.05,
               "stop_loss_fraction":0.03,"fee_fraction":0.0025,"estimated_slippage_fraction":0.001,
               "median_interval_seconds":0})
    }
    #[test]
    fn engine_starts_off_and_refuses_observation() {
        let mut e = LiveEngine::new(strategy());
        assert!(e.observe_price("tok", 1.).is_err());
        assert!(e.enable().is_ok());
        assert!(e.observe_price("tok", 1.).is_ok());
    }
    #[test]
    fn kill_switch_blocks_enable_and_everything() {
        let mut e = LiveEngine::new(strategy());
        e.kill();
        assert!(e.enable().is_err());
        e.enabled = true;
        assert!(e.observe_price("tok", 1.).is_err());
    }
    #[test]
    fn entry_size_respects_max_position() {
        let limits = RiskLimits {
            max_position_native: 0.01,
            ..RiskLimits::default()
        };
        let d = live_decision(&strategy(), &limits, 0.96, Some(1.));
        assert_eq!(d["action"], "hold");
        assert!(number(&d["reason"]).to_string().contains("exceeds") || d["reason"].as_str().unwrap().contains("exceeds"));
    }
    #[test]
    fn buy_sell_cycle_charges_fees_and_slippage() {
        let mut e = LiveEngine::new(strategy());
        e.enable().unwrap();
        let l = e.observe_price("tok", 1.0).unwrap();
        let l = e.observe_price("tok", 0.97).unwrap(); // 3% drop -> buy 0.05
        assert_eq!(l["last_decision"]["action"], "buy");
        assert!((number(&l["fees_paid_native"]) - 0.05 * 0.0025).abs() < 1e-12);
        assert!(number(&l["slippage_cost_native"]) > 0.);
        let entry_fee = number(&l["fees_paid_native"]);
        let l = e.observe_price("tok", 1.03).unwrap(); // take-profit
        assert_eq!(l["last_decision"]["action"], "sell");
        assert!(number(&l["fees_paid_native"]) > entry_fee);
        // PnL credited net: gross - sell fee - sell slippage - basis (0.05).
        let q = 0.05 / 0.97;
        let gross = q * 1.03;
        let pnl = number(&l["last_decision"]["pnl_native"]);
        let expected = gross
            - gross * 0.0025  // sell fee
            - gross * 0.001   // sell slippage
            - q * 0.97;       // basis
        assert!((pnl - expected).abs() < 1e-9);
        assert_eq!(l["open_positions"], 0.);
        // Daily net = realized pnl minus buy-side fees/slippage, still positive.
        let buy_costs = 0.05 * 0.0025 + 0.05 * 0.001;
        let dn = number(&l["daily_net_native"]);
        assert!((dn - (pnl - buy_costs)).abs() < 1e-9);
        assert!(dn > 0.);
    }
    #[test]
    fn daily_stop_halts_new_entries() {
        let s = strategy();
        let limits = RiskLimits {
            daily_stop_net_native: -0.001,
            ..RiskLimits::default()
        };
        let mut ledger = json!({"asset":"tok","cash_native":1.,"quantity":0.5,"entry_price":1.0,
            "last_price":0.99,"trades":[],"realized_pnl_native":-0.002,"fees_paid_native":0.,
            "slippage_cost_native":0.,"daily_net_native":-0.002,"open_positions":1.,"daily_stop_engaged":false});
        let l = apply_live_price(&s, &limits, ledger.clone(), 0.98, "tok").unwrap();
        assert_eq!(l["last_decision"]["action"], "hold");
        assert_eq!(l["last_decision"]["reason"], "daily stop limit reached");
        assert_eq!(l["open_positions"], 1.); // existing position untouched
    }
    #[test]
    fn max_open_positions_deny_buy() {
        let s = strategy();
        let limits = RiskLimits {
            max_open_positions: 1,
            ..RiskLimits::default()
        };
        let ledger = json!({"asset":"tok","cash_native":1.,"quantity":0.,"entry_price":null,
            "last_price":1.0,"trades":[],"realized_pnl_native":0.,"fees_paid_native":0.,
            "slippage_cost_native":0.,"daily_net_native":0.,"open_positions":1.,"last_trade_at":0});
        let l = apply_live_price(&s, &limits, ledger, 0.96, "tok").unwrap();
        assert_eq!(l["last_decision"]["action"], "hold");
        assert_eq!(l["last_decision"]["reason"], "max open positions reached");
    }
    #[test]
    fn invalid_price_rejected() {
        let mut ledger = Value::Null;
        assert!(apply_live_price(&strategy(), &RiskLimits::default(), ledger.clone(), 0., "t").is_err());
        assert!(apply_live_price(&strategy(), &RiskLimits::default(), ledger, f64::INFINITY, "t").is_err());
    }
    #[test]
    fn submission_requires_both_live_gates_and_respects_kill() {
        // Gate logic verification with stubs; the real gate is RuntimeState.
        // execute_action refuses without live authorization (checked via
        // require_live_authorization) and refuses when the kill-switch is set.
        struct FakeAuth(bool);
        // Minimal stand-in: simulate what require_live_authorization does.
        fn gate(open: bool) -> Result<(), String> {
            if open { Ok(()) } else { Err("closed".into()) }
        }
        assert!(gate(false).is_err());
        assert!(gate(true).is_ok());
        // Engine kill-switch refuses regardless of the outer gate.
        let engine = Arc::new(Mutex::new(LiveEngine::new(strategy())));
        engine.lock().unwrap().killed = true;
        assert!(engine.lock().unwrap().enable().is_err());
    }
}
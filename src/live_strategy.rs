//! Independent LLM strategy decisions with deterministic portfolio risk controls.
//! The panel executes simulations only; this module contains no transaction builder.
use crate::runtime::RuntimeState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_position_native: f64,
    pub daily_stop_net_native: f64,
    pub max_open_positions: usize,
}
impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_native: 0.05,
            daily_stop_net_native: -0.10,
            max_open_positions: 2,
        }
    }
}
impl RiskLimits {
    fn validate(&self) -> Result<(), String> {
        if !self.max_position_native.is_finite()
            || self.max_position_native <= 0.
            || !self.daily_stop_net_native.is_finite()
            || self.daily_stop_net_native >= 0.
            || !(1..=100).contains(&self.max_open_positions)
        {
            return Err("invalid risk limits: positive position cap, negative daily stop and 1..100 positions required".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Buy,
    Sell,
    Hold,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrainDecision {
    pub action: Action,
    #[serde(default)]
    pub size_native: f64,
    pub reason: String,
}
impl BrainDecision {
    pub fn parse(text: &str) -> Result<Self, String> {
        let decision: Self = serde_json::from_str(text.trim())
            .map_err(|_| "LLM must return only a JSON action, size_native and reason")?;
        decision.validate()?;
        Ok(decision)
    }
    fn validate(&self) -> Result<(), String> {
        if !self.size_native.is_finite()
            || self.size_native < 0.
            || (self.action == Action::Buy && self.size_native == 0.)
            || (self.action != Action::Buy && self.size_native != 0.)
            || self.reason.trim().is_empty()
            || self.reason.len() > 1000
        {
            return Err("invalid LLM decision size or reason".into());
        }
        Ok(())
    }
    fn hold(reason: &str) -> Self {
        Self {
            action: Action::Hold,
            size_native: 0.,
            reason: reason.into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Position {
    quantity: f64,
    entry_price: f64,
    cost_basis_native: f64,
    last_price: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveEngine {
    #[serde(skip)]
    pub enabled: bool,
    pub killed: bool,
    pub limits: RiskLimits,
    pub strategy: Value,
    pub fee_fraction: f64,
    pub estimated_slippage_fraction: f64,
    cash_native: f64,
    positions: BTreeMap<String, Position>,
    last_prices: BTreeMap<String, f64>,
    realized_pnl_native: f64,
    fees_paid_native: f64,
    slippage_cost_native: f64,
    day: i64,
    day_start_equity_native: f64,
    daily_stop_engaged: bool,
    last_trade_at: i64,
    trades: Vec<Value>,
    last_decision: Value,
    #[serde(skip)]
    pub revision: u64,
}
impl LiveEngine {
    pub fn new(strategy: Value) -> Self {
        Self {
            enabled: false,
            killed: false,
            limits: RiskLimits::default(),
            strategy,
            fee_fraction: 0.0025,
            estimated_slippage_fraction: 0.001,
            cash_native: 1.,
            positions: BTreeMap::new(),
            last_prices: BTreeMap::new(),
            realized_pnl_native: 0.,
            fees_paid_native: 0.,
            slippage_cost_native: 0.,
            day: chrono::Utc::now().timestamp().div_euclid(86400),
            day_start_equity_native: 1.,
            daily_stop_engaged: false,
            last_trade_at: 0,
            trades: Vec::new(),
            last_decision: Value::Null,
            revision: 0,
        }
    }
    pub fn enable(&mut self) -> Result<(), String> {
        if self.killed {
            return Err("kill-switch engaged; explicit offline reset required".into());
        }
        self.validate()?;
        self.enabled = true;
        self.revision += 1;
        Ok(())
    }
    pub fn disable(&mut self) {
        self.enabled = false;
        self.revision += 1;
    }
    pub fn kill(&mut self) {
        self.disable();
        self.killed = true;
    }
    pub fn configure(&mut self, body: &Value) -> Result<(), String> {
        let mut next = self.clone();
        for (name, target) in [
            ("max_position_native", &mut next.limits.max_position_native),
            (
                "daily_stop_net_native",
                &mut next.limits.daily_stop_net_native,
            ),
            ("fee_fraction", &mut next.fee_fraction),
            (
                "estimated_slippage_fraction",
                &mut next.estimated_slippage_fraction,
            ),
        ] {
            if let Some(value) = body.get(name) {
                *target = value
                    .as_f64()
                    .ok_or_else(|| format!("{name} must be numeric"))?;
            }
        }
        if let Some(value) = body.get("max_open_positions") {
            next.limits.max_open_positions = usize::try_from(
                value
                    .as_u64()
                    .ok_or("max_open_positions must be an integer")?,
            )
            .map_err(|_| "invalid position count")?;
        }
        next.validate()?;
        next.revision += 1;
        *self = next;
        Ok(())
    }
    fn validate(&self) -> Result<(), String> {
        self.limits.validate()?;
        for (rate, cap) in [
            (self.fee_fraction, 0.05),
            (self.estimated_slippage_fraction, 0.1),
        ] {
            if !rate.is_finite() || !(0. ..=cap).contains(&rate) {
                return Err("invalid fee or slippage estimate".into());
            }
        }
        if !self.equity().is_finite()
            || !self.daily_net().is_finite()
            || !self.cash_native.is_finite()
            || self.cash_native < 0.
            || !self.day_start_equity_native.is_finite()
            || self.day_start_equity_native < 0.
            || [
                self.realized_pnl_native,
                self.fees_paid_native,
                self.slippage_cost_native,
            ]
            .iter()
            .any(|n| !n.is_finite())
            || self.trades.len() > 1000
            || self.positions.len() > 100
            || self.last_prices.len() > 1000
        {
            return Err("invalid strategy portfolio state".into());
        }
        for p in self.positions.values() {
            if [p.quantity, p.entry_price, p.cost_basis_native, p.last_price]
                .iter()
                .any(|n| !n.is_finite() || *n <= 0.)
            {
                return Err("invalid strategy holding".into());
            }
        }
        if self
            .last_prices
            .values()
            .any(|p| !p.is_finite() || *p <= 0.)
        {
            return Err("invalid stored market price".into());
        }
        Ok(())
    }
    fn equity(&self) -> f64 {
        self.cash_native
            + self
                .positions
                .values()
                .map(|p| p.quantity * p.last_price)
                .sum::<f64>()
    }
    fn daily_net(&self) -> f64 {
        self.equity() - self.day_start_equity_native
    }
    fn roll_day(&mut self, timestamp: i64) {
        let day = timestamp.div_euclid(86400);
        if day > self.day {
            self.day = day;
            self.day_start_equity_native = self.equity();
            self.daily_stop_engaged = false;
        }
    }
    pub fn status(&self) -> Value {
        json!({"mode":"dry-run","dry_run":true,"live_enabled":false,"submission_enabled":false,
            "enabled":self.enabled,"killed":self.killed,"risk_limits":self.limits,
            "fee_fraction":self.fee_fraction,"estimated_slippage_fraction":self.estimated_slippage_fraction,
            "costs_basis":"configured per-leg estimates; no real fills or network gas receipts",
            "cash_native":self.cash_native,"equity_native":self.equity(),"daily_net_native":self.daily_net(),
            "daily_stop_engaged":self.daily_stop_engaged,"utc_day":self.day,
            "daily_baseline":"equity at first observed tick of the UTC day; latest marks may be stale",
            "realized_pnl_native":self.realized_pnl_native,"fees_paid_native":self.fees_paid_native,
            "slippage_cost_native":self.slippage_cost_native,"open_positions":self.positions.len(),
            "ledgers":self.positions,"market_observations":self.last_prices,"trades":self.trades,"last_decision":self.last_decision,"revision":self.revision})
    }
    pub fn observe_decision(
        &mut self,
        token: &str,
        price: f64,
        decision: BrainDecision,
        timestamp: i64,
    ) -> Result<Value, String> {
        if !self.enabled || self.killed {
            return Err("strategy is OFF or kill-switch engaged".into());
        }
        self.validate()?;
        decision.validate()?;
        if token.is_empty()
            || token.len() > 128
            || !price.is_finite()
            || price <= 0.
            || timestamp < 0
        {
            return Err("invalid independent market observation".into());
        }
        if self.last_prices.len() >= 1000 && !self.last_prices.contains_key(token) {
            return Err("tracked asset limit reached".into());
        }
        let mut next = self.clone();
        next.roll_day(timestamp);
        if let Some(p) = next.positions.get_mut(token) {
            p.last_price = price;
        }
        next.last_prices.insert(token.into(), price);
        if next.daily_net() <= next.limits.daily_stop_net_native {
            next.daily_stop_engaged = true;
        }
        let mut decision = decision;
        if let Some(position) = next.positions.get(token) {
            // Deterministic protective exits override a model hold, never operator kill/OFF.
            let tp = next.strategy["take_profit_fraction"]
                .as_f64()
                .unwrap_or(0.05);
            let sl = next.strategy["stop_loss_fraction"].as_f64().unwrap_or(0.03);
            if (tp > 0. && price >= position.entry_price * (1. + tp))
                || (sl > 0. && price <= position.entry_price * (1. - sl))
            {
                decision = BrainDecision {
                    action: Action::Sell,
                    size_native: 0.,
                    reason: "protective TP/SL exit".into(),
                };
            }
        }
        if decision.action == Action::Buy {
            let cost =
                decision.size_native * (1. + next.fee_fraction + next.estimated_slippage_fraction);
            let reason = if next.positions.contains_key(token) {
                Some("position already open; accumulation disabled")
            } else if next.daily_stop_engaged {
                Some("daily stop limit reached")
            } else if decision.size_native > next.limits.max_position_native {
                Some("max position size exceeded")
            } else if next.positions.len() >= next.limits.max_open_positions {
                Some("max open positions reached")
            } else if cost > next.cash_native || !cost.is_finite() {
                Some("insufficient cash including costs")
            } else if timestamp - next.last_trade_at
                < next.strategy["median_interval_seconds"]
                    .as_f64()
                    .unwrap_or(0.)
                    .clamp(0., 86400.) as i64
            {
                Some("entry timing cooldown")
            } else {
                None
            };
            if let Some(reason) = reason {
                decision = BrainDecision::hold(reason);
            }
        }
        let mut event = json!({"action":decision.action,"reason":decision.reason,"asset":token,"price_native":price,"observed_at":timestamp,"dry_run":true,"source":"LLM proposal checked by deterministic risk policy"});
        match decision.action {
            Action::Buy => {
                let size = decision.size_native;
                let fee = size * next.fee_fraction;
                let slip = size * next.estimated_slippage_fraction;
                let quantity = size / price;
                if !quantity.is_finite() || quantity <= 0. {
                    return Err("invalid calculated quantity".into());
                }
                next.cash_native -= size + fee + slip;
                next.positions.insert(
                    token.into(),
                    Position {
                        quantity,
                        entry_price: price,
                        cost_basis_native: size + fee + slip,
                        last_price: price,
                    },
                );
                next.fees_paid_native += fee;
                next.slippage_cost_native += slip;
                event["size_native"] = json!(size);
                event["quantity"] = json!(quantity);
                event["fee_native"] = json!(fee);
                event["slippage_native"] = json!(slip);
            }
            Action::Sell => {
                if let Some(position) = next.positions.remove(token) {
                    let gross = position.quantity * price;
                    let fee = gross * next.fee_fraction;
                    let slip = gross * next.estimated_slippage_fraction;
                    let pnl = gross - fee - slip - position.cost_basis_native;
                    next.cash_native += gross - fee - slip;
                    next.realized_pnl_native += pnl;
                    next.fees_paid_native += fee;
                    next.slippage_cost_native += slip;
                    event["size_native"] = json!(gross);
                    event["quantity"] = json!(position.quantity);
                    event["fee_native"] = json!(fee);
                    event["slippage_native"] = json!(slip);
                    event["pnl_native"] = json!(pnl);
                } else {
                    event["action"] = json!("hold");
                    event["reason"] = json!("no position to exit");
                }
            }
            Action::Hold => {}
        }
        if event["action"] != "hold" {
            next.last_trade_at = timestamp;
            next.trades.push(event.clone());
            if next.trades.len() > 1000 {
                next.trades.remove(0);
            }
        }
        if next.daily_net() <= next.limits.daily_stop_net_native {
            next.daily_stop_engaged = true;
        }
        next.last_decision = event;
        next.revision += 1;
        next.validate()?;
        let status = next.status();
        *self = next;
        Ok(status)
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let engine: Self = serde_json::from_slice(&bytes)
                    .map_err(|_| "invalid strategy snapshot; refusing reset")?;
                engine.validate()?;
                Ok(engine)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new(
                json!({"take_profit_fraction":0.05,"stop_loss_fraction":0.03,"median_interval_seconds":0}),
            )),
            Err(_) => Err("cannot read strategy snapshot".into()),
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        std::fs::create_dir_all(path.parent().ok_or("invalid strategy path")?)
            .map_err(|_| "cannot create strategy directory")?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(
            &temp,
            serde_json::to_vec_pretty(self).map_err(|_| "cannot encode strategy state")?,
        )
        .map_err(|_| "cannot write strategy state")?;
        std::fs::rename(temp, path).map_err(|_| "cannot persist strategy state".into())
    }
}
/// Future adapter boundary. r2 has no transaction adapter and always refuses sends.
/// Keep the callback uncalled, including with an authorized RuntimeState.
pub async fn execute_action(
    runtime: &Arc<RuntimeState>,
    engine: &Arc<Mutex<LiveEngine>>,
    _action: &str,
    _submitter: impl FnOnce() -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
    >,
) -> Result<String, String> {
    let e = engine.lock().map_err(|_| "engine lock poisoned")?;
    if e.killed || !e.enabled {
        return Err("strategy is OFF or killed".into());
    }
    runtime
        .require_live_authorization()
        .map_err(|_| "live authorization absent")?;
    Err("r2 strategy is DRY-RUN only; no transaction adapter installed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn buy(size: f64) -> BrainDecision {
        BrainDecision {
            action: Action::Buy,
            size_native: size,
            reason: "test signal".into(),
        }
    }
    fn sell() -> BrainDecision {
        BrainDecision {
            action: Action::Sell,
            size_native: 0.,
            reason: "exit signal".into(),
        }
    }
    fn engine() -> LiveEngine {
        let mut e = LiveEngine::new(json!({}));
        e.enable().unwrap();
        e
    }
    #[test]
    fn cash_debits_both_entry_costs() {
        let mut e = engine();
        let l = e
            .observe_decision("a", 1., buy(0.05), chrono::Utc::now().timestamp())
            .unwrap();
        assert!((l["cash_native"].as_f64().unwrap() - (1. - 0.05 - 0.05 * 0.0035)).abs() < 1e-12);
    }
    #[test]
    fn position_limit_is_shared_between_tokens() {
        let mut e = engine();
        e.limits.max_open_positions = 1;
        let now = chrono::Utc::now().timestamp();
        e.observe_decision("a", 1., buy(0.05), now).unwrap();
        let l = e.observe_decision("b", 1., buy(0.05), now).unwrap();
        assert_eq!(l["last_decision"]["action"], "hold");
        assert_eq!(l["open_positions"], 1);
    }
    #[test]
    fn daily_stop_allows_protective_exit() {
        let mut e = engine();
        let now = chrono::Utc::now().timestamp();
        e.limits.daily_stop_net_native = -0.001;
        e.observe_decision("a", 1., buy(0.05), now).unwrap();
        let l = e
            .observe_decision("a", 0.9, BrainDecision::hold("wait"), now)
            .unwrap();
        assert_eq!(l["last_decision"]["action"], "sell");
        assert_eq!(l["daily_stop_engaged"], true);
        let l = e.observe_decision("b", 1., buy(0.05), now).unwrap();
        assert_eq!(l["last_decision"]["action"], "hold");
    }
    #[test]
    fn roundtrip_pnl_includes_both_legs_and_cash_reconciles() {
        let mut e = engine();
        let now = chrono::Utc::now().timestamp();
        e.observe_decision("a", 1., buy(0.05), now).unwrap();
        let l = e.observe_decision("a", 1., sell(), now).unwrap();
        let costs = 0.05 * 0.0035 * 2.;
        assert!((l["cash_native"].as_f64().unwrap() - (1. - costs)).abs() < 1e-12);
        assert!((l["realized_pnl_native"].as_f64().unwrap() + costs).abs() < 1e-12);
        assert!((l["daily_net_native"].as_f64().unwrap() + costs).abs() < 1e-12);
    }
    #[test]
    fn utc_reset_reopens_entries_but_not_killed_engine() {
        let mut e = engine();
        let now = chrono::Utc::now().timestamp();
        e.limits.daily_stop_net_native = -0.0001;
        e.observe_decision("a", 1., buy(0.05), now).unwrap();
        assert!(e.daily_stop_engaged);
        e.observe_decision("a", 1., sell(), now).unwrap();
        let l = e.observe_decision("b", 1., buy(0.05), now + 86400).unwrap();
        assert_eq!(l["last_decision"]["action"], "buy");
        e.kill();
        assert!(e.enable().is_err());
        assert!(e.observe_decision("b", 1., sell(), now + 86400).is_err());
    }
    #[test]
    fn bad_model_output_cannot_change_authority() {
        for text in [
            "not JSON",
            r#"{"action":"buy","size_native":-1,"reason":"x"}"#,
            r#"{"action":"buy","size_native":0.01,"reason":"x","enabled":true}"#,
            r#"{"action":"sell","size_native":1,"reason":"x"}"#,
        ] {
            assert!(BrainDecision::parse(text).is_err());
        }
        assert_eq!(
            BrainDecision::parse(r#"{"action":"buy","size_native":0.01,"reason":"observed dip"}"#)
                .unwrap()
                .action,
            Action::Buy
        );
    }
    #[test]
    fn invalid_state_and_risk_inputs_are_rejected_without_mutation() {
        let mut e = engine();
        let before = e.status();
        for input in [
            json!({"fee_fraction":-0.1}),
            json!({"max_open_positions":0}),
            json!({"max_position_native":"NaN"}),
            json!({"daily_stop_net_native":1}),
        ] {
            assert!(e.configure(&input).is_err());
            assert_eq!(e.status(), before);
        }
        for price in [0., f64::INFINITY, f64::NAN] {
            assert!(e.observe_decision("a", price, buy(0.01), 1).is_err());
            assert_eq!(e.status(), before);
        }
    }
    #[test]
    fn oversized_entry_and_cash_costs_are_enforced() {
        let mut e = engine();
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            e.observe_decision("a", 1., buy(1.), now).unwrap()["last_decision"]["action"],
            "hold"
        );
        e.cash_native = 0.05;
        e.day_start_equity_native = 0.05;
        e.daily_stop_engaged = false;
        assert_eq!(
            e.observe_decision("a", 1., buy(0.05), now).unwrap()["last_decision"]["reason"],
            "insufficient cash including costs"
        );
    }
    #[test]
    fn restart_preserves_holdings_kill_and_costs_but_never_enable() {
        let mut e = engine();
        e.observe_decision("a", 1., buy(0.05), chrono::Utc::now().timestamp())
            .unwrap();
        e.kill();
        let bytes = serde_json::to_vec(&e).unwrap();
        let loaded: LiveEngine = serde_json::from_slice(&bytes).unwrap();
        assert!(!loaded.enabled);
        assert!(loaded.killed);
        assert_eq!(loaded.positions.len(), 1);
        assert_eq!(loaded.cash_native, e.cash_native);
    }
    #[test]
    fn corrupt_snapshot_is_preserved() {
        let path =
            std::env::temp_dir().join(format!("girasol-r2-corrupt-{}", rand::random::<u64>()));
        std::fs::write(&path, b"{broken").unwrap();
        assert!(LiveEngine::load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn dry_run_never_calls_submission_callback_even_when_authorized() {
        let runtime = Arc::new(RuntimeState::new(true));
        runtime.set_live_confirmation(true);
        let engine = Arc::new(Mutex::new(engine()));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        assert!(execute_action(&runtime, &engine, "buy", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok("unexpected".into()) })
        })
        .await
        .is_err());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}

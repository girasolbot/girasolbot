//! Shared execution authorization; confirmations are deliberately never persisted.
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

pub struct RuntimeState {
    cli_live: bool,
    confirmed: AtomicBool,
    journal: Mutex<()>,
    data_dir: PathBuf,
}
impl RuntimeState {
    pub fn new(cli_live: bool) -> Self {
        Self {
            cli_live,
            confirmed: AtomicBool::new(false),
            journal: Mutex::new(()),
            data_dir: std::env::var("SNIPER_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("data")),
        }
    }
    pub fn set_live_confirmation(&self, confirmed: bool) {
        self.confirmed.store(confirmed, Ordering::SeqCst);
    }
    pub fn live_enabled(&self) -> bool {
        self.cli_live && self.confirmed.load(Ordering::SeqCst)
    }
    pub fn require_live_authorization(&self) -> std::io::Result<()> {
        if self.live_enabled() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "live authorization absent or revoked",
            ))
        }
    }
    pub fn cli_live_requested(&self) -> bool {
        self.cli_live
    }
    pub fn record_trade(&self, mut event: Value) -> Result<(), String> {
        if !event.is_object() {
            return Err("trade event must be an object".into());
        }
        let _lock = self
            .journal
            .lock()
            .map_err(|_| "trade journal lock poisoned")?;
        std::fs::create_dir_all(&self.data_dir).map_err(|e| e.to_string())?;
        let path = self.data_dir.join("trades.json");
        let mut events: Vec<Value> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "trade journal corrupt; refusing to overwrite")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        event["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        events.push(event);
        if events.len() > 10000 {
            events.drain(..events.len() - 10000);
        }
        let temporary = self.data_dir.join("trades.json.tmp");
        std::fs::write(
            &temporary,
            serde_json::to_vec(&events).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(temporary, path).map_err(|e| e.to_string())
    }
    pub fn status(&self) -> Value {
        json!({"cli_live":self.cli_live,"live_enabled":self.live_enabled(),"execution_mode":if self.live_enabled(){"live"}else if self.cli_live{"paused"}else{"paper"}})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_requires_both_factors_and_can_be_revoked() {
        let paper = RuntimeState::new(false);
        paper.set_live_confirmation(true);
        assert!(!paper.live_enabled());
        let live = RuntimeState::new(true);
        assert!(!live.live_enabled());
        live.set_live_confirmation(true);
        assert!(live.live_enabled());
        assert!(live.require_live_authorization().is_ok());
        live.set_live_confirmation(false);
        assert!(!live.live_enabled());
        assert!(live.require_live_authorization().is_err());
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    fn isolated() -> RuntimeState {
        let mut r = RuntimeState::new(false);
        r.data_dir = std::env::temp_dir().join(format!(
            "girasol-journal-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        r
    }
    #[test]
    fn malformed_event_is_rejected_without_panicking() {
        assert!(isolated().record_trade(json!("invalid")).is_err());
    }
    #[test]
    fn corrupt_journal_is_preserved() {
        let r = isolated();
        std::fs::create_dir_all(&r.data_dir).unwrap();
        let path = r.data_dir.join("trades.json");
        std::fs::write(&path, "corrupt").unwrap();
        assert!(r.record_trade(json!({"side":"buy"})).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "corrupt");
    }
    #[test]
    fn records_real_event_with_timestamp() {
        let r = isolated();
        r.record_trade(json!({"network":"solana","side":"buy","dry_run":true}))
            .unwrap();
        let v: Value =
            serde_json::from_slice(&std::fs::read(r.data_dir.join("trades.json")).unwrap())
                .unwrap();
        assert_eq!(v[0]["side"], "buy");
        assert!(v[0]["timestamp"].as_str().unwrap().contains('T'));
    }
}

/// Net sell receipts less proportional acquisition cost; unknown basis stays unknown.
pub fn realized_pnl(proceeds: f64, cost: Option<f64>, original: u64, sold: u64) -> Option<f64> {
    let cost = cost?;
    if original == 0 || sold > original || !cost.is_finite() || cost < 0. || !proceeds.is_finite() {
        return None;
    }
    Some(proceeds - cost * sold as f64 / original as f64)
}
#[cfg(test)]
mod pnl_tests {
    use super::*;
    #[test]
    fn sale_proceeds_are_not_profit() {
        assert!((realized_pnl(0.8, Some(1.0), 100, 100).unwrap() + 0.2).abs() < 1e-12);
    }
    #[test]
    fn missing_basis_stays_unknown() {
        assert!(realized_pnl(1.0, None, 100, 100).is_none());
    }
    #[test]
    fn partial_exit_deducts_proportional_basis() {
        assert!((realized_pnl(0.6, Some(1.0), 100, 50).unwrap() - 0.1).abs() < 1e-12);
    }
}

static AUTHORIZATION: OnceLock<Arc<RuntimeState>> = OnceLock::new();
pub fn install_authorization(state: Arc<RuntimeState>) -> std::io::Result<()> {
    AUTHORIZATION.set(state).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "execution authorization already installed",
        )
    })
}
/// Check at the network boundary, after asynchronous preparation, for every broadcast.
pub fn require_live_authorization() -> std::io::Result<()> {
    AUTHORIZATION
        .get()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "execution authorization not installed",
            )
        })?
        .require_live_authorization()
}

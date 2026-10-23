//! EVM target observation and exact calldata replay. No sends in dry-run.
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
pub struct EvmConfig {
    pub chain: String,
    pub chain_id: u64,
    #[serde(default)]
    pub allow_new_chain: bool,
    pub rpc_url: String,
    #[serde(default)]
    pub execution_rpc_url: String,
    #[serde(default)]
    pub ws_url: String,
    #[serde(default)]
    pub sequencer_feed: bool,
    #[serde(default = "yes")]
    pub dry_run: bool,
    #[serde(default)]
    pub targets: Vec<String>,
    pub copy_address: String,
    #[serde(default = "value_limit")]
    pub max_value_wei: String,
    #[serde(default = "gas_limit")]
    pub max_gas: u64,
    #[serde(default = "fee_limit")]
    pub max_fee_per_gas_wei: String,
    #[serde(default)]
    pub allow_unmodified_calldata: bool,
    #[serde(default = "poll_ms")]
    pub poll_interval_ms: u64,
}
fn yes() -> bool {
    true
}
fn value_limit() -> String {
    "0".into()
}
fn gas_limit() -> u64 {
    500_000
}
fn fee_limit() -> String {
    "10000000000".into()
}
fn poll_ms() -> u64 {
    1000
}

impl EvmConfig {
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let cfg: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        cfg.validate()?;
        Ok(cfg)
    }
    pub async fn load_with_vault(
        path: impl AsRef<std::path::Path>,
        vault: &ApiKeyVault,
    ) -> Result<Self> {
        let mut cfg: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        anyhow::ensure!(
            matches!(cfg.chain.as_str(), "elysium" | "robinhood"),
            "invalid chain metadata"
        );
        if let Some(endpoint) = vault.get_api_key(&format!("{}-rpc", cfg.chain)).await {
            cfg.rpc_url = endpoint;
        }
        if let Some(endpoint) = vault.get_api_key(&format!("{}-tip-rpc", cfg.chain)).await {
            cfg.execution_rpc_url = endpoint;
        }
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.chain_id > 0 && matches!(self.chain.as_str(), "elysium" | "robinhood"),
            "invalid chain metadata"
        );
        anyhow::ensure!(
            self.allow_new_chain
                || matches!(
                    (self.chain.as_str(), self.chain_id),
                    ("elysium", 99801) | ("robinhood", 4663 | 46630)
                ),
            "new chain metadata requires explicit allow_new_chain acknowledgement"
        );
        let rpc = url::Url::parse(&self.rpc_url)?;
        anyhow::ensure!(
            matches!(rpc.scheme(), "http" | "https"),
            "RPC requires HTTP(S)"
        );
        if !self.execution_rpc_url.is_empty() {
            let execution = url::Url::parse(&self.execution_rpc_url)?;
            anyhow::ensure!(
                matches!(execution.scheme(), "http" | "https"),
                "execution RPC requires HTTP(S)"
            );
        }
        if !self.ws_url.is_empty() {
            let ws = url::Url::parse(&self.ws_url)?;
            anyhow::ensure!(
                matches!(ws.scheme(), "ws" | "wss"),
                "listener requires WS(S)"
            );
        }
        validate_address(&self.copy_address)?;
        anyhow::ensure!(
            self.copy_address[2..].bytes().any(|x| x != b'0'),
            "copy wallet cannot be zero"
        );
        for target in &self.targets {
            validate_address(target)?;
        }
        self.max_value_wei.parse::<u128>()?;
        anyhow::ensure!(
            self.max_fee_per_gas_wei.parse::<u128>()? > 0,
            "fee cap must be positive"
        );
        anyhow::ensure!(
            self.max_gas >= 21000 && self.poll_interval_ms >= 100,
            "invalid gas cap or polling interval"
        );
        anyhow::ensure!(
            self.dry_run || self.allow_unmodified_calldata,
            "live replay requires acknowledging sender-bound recipients/calldata"
        );
        Ok(())
    }
}

fn validate_address(address: &str) -> Result<()> {
    anyhow::ensure!(
        address.starts_with("0x")
            && address.len() == 42
            && address[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid EVM address"
    );
    Ok(())
}
#[path = "evm_tx.rs"]
mod tx;
use crate::api_key_vault::ApiKeyVault;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tx::ObservedTx;

#[derive(Clone)]
struct Rpc {
    url: String,
    http: reqwest::Client,
}
impl Rpc {
    fn new(url: &str) -> Result<Self> {
        Ok(Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut response = self
            .http
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("EVM RPC transport failure"))?
            .error_for_status()
            .map_err(|_| anyhow::anyhow!("EVM RPC HTTP failure"))?;
        anyhow::ensure!(
            response.content_length().unwrap_or(0) <= 8 * 1024 * 1024,
            "oversized RPC response"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 8 * 1024 * 1024,
                "oversized RPC response"
            );
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes)?;
        if let Some(error) = body.get("error") {
            anyhow::bail!("{method} rejected: {error}");
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{method} missing result"))
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct EvmEvent {
    pub network: String,
    pub chain_id: u64,
    pub mode: String,
    pub status: String,
    pub source_hash: String,
    pub transaction_hash: Option<String>,
    pub source: String,
    pub from: String,
    pub to: Option<String>,
    pub value_wei: String,
    pub calldata: String,
    pub gas: Option<u64>,
    pub latency_ms: u64,
    pub error: Option<String>,
}
fn quantity(value: &Value) -> Result<u128> {
    let s = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("RPC quantity is not a string"))?;
    anyhow::ensure!(
        s.starts_with("0x") && s.len() > 2 && s.len() <= 34,
        "invalid RPC quantity"
    );
    Ok(u128::from_str_radix(&s[2..], 16)?)
}
fn event(cfg: &EvmConfig, tx: &ObservedTx, source: &str, status: &str) -> EvmEvent {
    EvmEvent {
        network: cfg.chain.clone(),
        chain_id: cfg.chain_id,
        mode: if cfg.dry_run { "paper" } else { "live" }.into(),
        status: status.into(),
        source_hash: tx.hash.clone(),
        transaction_hash: None,
        source: source.into(),
        from: tx.from.clone(),
        to: tx.to.clone(),
        value_wei: tx.value.to_string(),
        calldata: format!("0x{}", hex::encode(&tx.input)),
        gas: None,
        latency_ms: 0,
        error: None,
    }
}
async fn assert_chain(cfg: &EvmConfig, rpc: &Rpc) -> Result<()> {
    anyhow::ensure!(
        quantity(&rpc.call("eth_chainId", json!([])).await?)? == cfg.chain_id as u128,
        "RPC chain ID mismatch"
    );
    Ok(())
}
async fn process(
    cfg: &EvmConfig,
    rpc: &Rpc,
    tx: &ObservedTx,
    source: &str,
    gate: &(dyn Fn() -> bool + Send + Sync),
    vault: &ApiKeyVault,
) -> Result<EvmEvent> {
    let start = Instant::now();
    anyhow::ensure!(tx.chain_id == cfg.chain_id, "source chain mismatch");
    anyhow::ensure!(
        tx.value <= cfg.max_value_wei.parse::<u128>()?,
        "native value exceeds configured budget"
    );
    if !cfg.dry_run && !gate() {
        return Ok(event(cfg, tx, source, "awaiting_confirmation"));
    }
    assert_chain(cfg, rpc).await?;
    let mut request = json!({"from":cfg.copy_address,"value":format!("0x{:x}",tx.value),"data":format!("0x{}",hex::encode(&tx.input)),"gas":format!("0x{:x}",cfg.max_gas)});
    if let Some(to) = &tx.to {
        request["to"] = json!(to);
    }
    rpc.call("eth_call", json!([request, "pending"])).await?;
    let gas = u64::try_from(quantity(
        &rpc.call("eth_estimateGas", json!([request, "pending"]))
            .await?,
    )?)?;
    anyhow::ensure!(
        gas >= 21000 && gas <= cfg.max_gas,
        "estimated gas exceeds configured budget"
    );
    let mut report = event(cfg, tx, source, "simulated");
    report.gas = Some(gas);
    if !cfg.dry_run {
        anyhow::ensure!(
            cfg.allow_unmodified_calldata,
            "live calldata replay not acknowledged"
        );
        anyhow::ensure!(gate(), "live confirmation revoked");
        let secret = vault
            .get_vault_key(&format!("evm_{}_private_key", cfg.chain))
            .await
            .ok_or_else(|| anyhow::anyhow!("EVM signing key is not present in encrypted vault"))?;
        let execution = if cfg.execution_rpc_url.is_empty() {
            rpc.clone()
        } else {
            Rpc::new(&cfg.execution_rpc_url)?
        };
        assert_chain(cfg, &execution).await?;
        let nonce = u64::try_from(quantity(
            &execution
                .call(
                    "eth_getTransactionCount",
                    json!([cfg.copy_address, "pending"]),
                )
                .await?,
        )?)?;
        let fee = quantity(&rpc.call("eth_gasPrice", json!([])).await?)?;
        anyhow::ensure!(
            fee > 0 && fee <= cfg.max_fee_per_gas_wei.parse::<u128>()?,
            "RPC gas price exceeds configured fee cap"
        );
        // Gas headroom is bounded by the user's absolute cap; fees are not bumped.
        let gas = gas.saturating_add(gas / 5).min(cfg.max_gas);
        let (raw, signer) = tx::sign_replay(
            cfg.chain_id,
            nonce,
            gas,
            fee,
            tx.to.as_deref(),
            tx.value,
            &tx.input,
            &secret,
        )?;
        anyhow::ensure!(
            signer.eq_ignore_ascii_case(&cfg.copy_address),
            "vault key does not match copy_address"
        );
        assert_chain(cfg, &execution).await?;
        anyhow::ensure!(gate(), "live confirmation revoked before broadcast");
        let result = execution
            .call(
                "eth_sendRawTransaction",
                json!([format!("0x{}", hex::encode(raw))]),
            )
            .await?;
        let hash = result
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("RPC returned invalid transaction hash"))?;
        anyhow::ensure!(
            hash.len() == 66
                && hash.starts_with("0x")
                && hash[2..].bytes().all(|b| b.is_ascii_hexdigit()),
            "RPC returned invalid transaction hash"
        );
        report.status = "submitted".into();
        report.transaction_hash = Some(hash.into());
        report.gas = Some(gas);
    }
    report.latency_ms = start.elapsed().as_millis() as u64;
    Ok(report)
}

fn parse_rpc_tx(value: &Value, chain: u64) -> Result<ObservedTx> {
    let from = value["from"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("transaction missing sender"))?;
    validate_address(from)?;
    let hash = value["hash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("transaction missing hash"))?;
    anyhow::ensure!(
        hash.starts_with("0x")
            && hash.len() == 66
            && hash[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid transaction hash"
    );
    if let Some(id) = value.get("chainId").filter(|v| !v.is_null()) {
        anyhow::ensure!(
            quantity(id)? == chain as u128,
            "RPC transaction chain mismatch"
        );
    }
    if let Some(kind) = value.get("type").filter(|v| !v.is_null()) {
        anyhow::ensure!(
            quantity(kind)? <= 2,
            "unsupported system/blob/delegation transaction"
        );
    }
    let to = if value["to"].is_null() {
        None
    } else {
        let a = value["to"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid transaction destination"))?;
        validate_address(a)?;
        Some(a.to_lowercase())
    };
    let input = value["input"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing transaction input"))?;
    anyhow::ensure!(
        input.starts_with("0x") && input.len() <= 2 + 256 * 1024 * 2,
        "invalid calldata size"
    );
    Ok(ObservedTx {
        hash: hash.to_lowercase(),
        from: from.to_lowercase(),
        nonce: u64::try_from(quantity(&value["nonce"])?)?,
        to,
        value: quantity(&value["value"])?,
        input: hex::decode(&input[2..])?,
        chain_id: chain,
    })
}
fn parse_feed(value: &Value, chain: u64) -> Result<Vec<ObservedTx>> {
    use base64::Engine;
    let mut txs = Vec::new();
    if let Some(messages) = value["messages"].as_array() {
        anyhow::ensure!(messages.len() <= 8192, "too many feed messages");
        for message in messages {
            let body = &message["message"]["message"];
            if body["header"]["kind"].as_u64() != Some(3) {
                continue;
            }
            let encoded = body["l2Msg"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing Nitro payload"))?;
            anyhow::ensure!(encoded.len() <= 350000, "oversized feed payload");
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            txs.extend(tx::decode_l2(&decoded, chain)?);
        }
    }
    Ok(txs)
}

struct Incoming {
    tx: ObservedTx,
    source: &'static str,
    received: Instant,
}
fn is_target(cfg: &EvmConfig, from: &str) -> bool {
    cfg.targets
        .iter()
        .any(|target| target.eq_ignore_ascii_case(from))
}

async fn poll_blocks(
    cfg: EvmConfig,
    rpc: Rpc,
    out: tokio::sync::mpsc::Sender<Incoming>,
) -> Result<()> {
    let mut next = None;
    loop {
        if out.is_closed() {
            return Ok(());
        }
        let result: Result<()> = async {
            assert_chain(&cfg, &rpc).await?;
            let head = u64::try_from(quantity(&rpc.call("eth_blockNumber", json!([])).await?)?)?;
            // Start observing at startup, never replay historical blocks implicitly.
            let cursor = next.get_or_insert(head.saturating_add(1));
            for _ in 0..100 {
                if *cursor > head {
                    break;
                }
                let block = rpc
                    .call(
                        "eth_getBlockByNumber",
                        json!([format!("0x{:x}", *cursor), true]),
                    )
                    .await?;
                let transactions = block["transactions"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("RPC block unavailable"))?;
                for value in transactions {
                    if value["from"].as_str().is_some_and(|a| is_target(&cfg, a)) {
                        match parse_rpc_tx(value, cfg.chain_id) {
                            Ok(tx) => {
                                out.send(Incoming {
                                    tx,
                                    source: "rpc_confirmed",
                                    received: Instant::now(),
                                })
                                .await?
                            }
                            Err(error) => log::warn!("EVM target envelope skipped: {error}"),
                        }
                    }
                }
                *cursor = cursor.saturating_add(1);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            log::warn!("EVM confirmed listener retry: {error}");
        }
        tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)).await;
    }
}
async fn websocket_once(
    cfg: &EvmConfig,
    rpc: &Rpc,
    out: &tokio::sync::mpsc::Sender<Incoming>,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
    let mut limits = WebSocketConfig::default();
    limits.max_message_size = Some(8 * 1024 * 1024);
    limits.max_frame_size = Some(8 * 1024 * 1024);
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async_with_config(&cfg.ws_url, Some(limits), false),
    )
    .await??;
    if !cfg.sequencer_feed {
        ws.send(Message::Text(json!({"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newPendingTransactions"]}).to_string())).await?;
    }
    loop {
        if out.is_closed() {
            return Ok(());
        }
        let frame = tokio::time::timeout(Duration::from_secs(45), ws.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("WebSocket ended"))??;
        let received = Instant::now();
        let bytes = match frame {
            Message::Text(text) => text.into_bytes(),
            Message::Binary(bytes) => bytes,
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await?;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => return Ok(()),
            Message::Frame(_) => continue,
        };
        let body: Value = serde_json::from_slice(&bytes)?;
        if body.get("error").is_some() {
            anyhow::bail!("pending subscription rejected");
        }
        let candidates = if cfg.sequencer_feed {
            parse_feed(&body, cfg.chain_id)
        } else {
            if body["method"] != "eth_subscription" {
                continue;
            }
            let result = &body["params"]["result"];
            let value = if let Some(hash) = result.as_str() {
                rpc.call("eth_getTransactionByHash", json!([hash])).await?
            } else {
                result.clone()
            };
            if value.is_null() || !value["from"].as_str().is_some_and(|a| is_target(cfg, a)) {
                continue;
            }
            parse_rpc_tx(&value, cfg.chain_id).map(|tx| vec![tx])
        };
        match candidates {
            Ok(txs) => {
                for tx in txs {
                    if is_target(cfg, &tx.from) {
                        out.send(Incoming {
                            tx,
                            source: if cfg.sequencer_feed {
                                "sequencer_feed"
                            } else {
                                "rpc_pending"
                            },
                            received,
                        })
                        .await?;
                    }
                }
            }
            Err(error) => log::warn!("EVM feed envelope skipped: {error}"),
        }
    }
}
async fn websocket_loop(
    cfg: EvmConfig,
    rpc: Rpc,
    out: tokio::sync::mpsc::Sender<Incoming>,
) -> Result<()> {
    let mut backoff = 1;
    loop {
        if out.is_closed() {
            return Ok(());
        }
        // URL may contain RPC credentials; transport errors are deliberately not logged.
        if websocket_once(&cfg, &rpc, &out).await.is_err() {
            log::warn!("EVM WebSocket disconnected; confirmed RPC fallback remains active");
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// Observe every supported user transaction from targets, independent of DEX route.
/// Exact calldata may bind recipients/allowances to the target: simulation is not a safety proof.
/// Receipt confirmation and local revm execution are not implemented.
pub async fn run(
    cfg: EvmConfig,
    out: tokio::sync::mpsc::Sender<EvmEvent>,
    gate: Arc<dyn Fn() -> bool + Send + Sync>,
    vault: Arc<ApiKeyVault>,
) -> Result<()> {
    cfg.validate()?;
    anyhow::ensure!(!cfg.targets.is_empty(), "configure at least one EVM target");
    anyhow::ensure!(
        !is_target(&cfg, &cfg.copy_address),
        "copy wallet cannot track itself"
    );
    let rpc = Rpc::new(&cfg.rpc_url)?;
    assert_chain(&cfg, &rpc).await?;
    let (send, mut receive) = tokio::sync::mpsc::channel::<Incoming>(1024);
    // JoinSet aborts listeners when this future is dropped or the event consumer closes.
    let mut listeners = tokio::task::JoinSet::new();
    listeners.spawn(poll_blocks(cfg.clone(), rpc.clone(), send.clone()));
    if !cfg.ws_url.is_empty() {
        listeners.spawn(websocket_loop(cfg.clone(), rpc.clone(), send.clone()));
    }
    drop(send);
    let mut seen = lru::LruCache::new(std::num::NonZeroUsize::new(8192).unwrap());
    loop {
        tokio::select! {
            _=out.closed()=>return Ok(()),
            incoming=receive.recv()=>{
                let Some(incoming)=incoming else {anyhow::bail!("all EVM listeners stopped");};
                if seen.put((incoming.tx.from.to_ascii_lowercase(),incoming.tx.nonce),()).is_some(){continue;}
                // Source replacements share a nonce: copy only the first observation.
                // Mark before submission: ambiguous RPC failures never trigger automatic resends.
                let mut report=match process(&cfg,&rpc,&incoming.tx,incoming.source,gate.as_ref(),&vault).await {
                    Ok(report)=>report,
                    Err(error)=>{let mut report=event(&cfg,&incoming.tx,incoming.source,"failed_or_unknown");report.error=Some(error.to_string());report},
                };
                report.latency_ms=incoming.received.elapsed().as_millis() as u64;
                if out.send(report).await.is_err(){return Ok(());}
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn config(extra: &str) -> EvmConfig {
        toml::from_str(&format!(
            r#"
chain = "elysium"
chain_id = 99801
rpc_url = "https://rpc.example.test"
copy_address = "0x1111111111111111111111111111111111111111"
{extra}
"#
        ))
        .unwrap()
    }
    #[test]
    fn defaults_never_send_and_limit_native_value() {
        let cfg = config("");
        assert!(cfg.dry_run);
        assert_eq!(cfg.max_value_wei, "0");
        cfg.validate().unwrap();
    }
    #[test]
    fn rejects_unknown_mainnet_id() {
        let mut cfg = config("");
        cfg.chain_id = 0;
        assert!(cfg.validate().is_err());
    }
    #[test]
    fn rejects_malformed_targets_and_zero_copy_wallet() {
        let mut cfg = config("");
        cfg.targets = vec!["0x123".into()];
        assert!(cfg.validate().is_err());
        cfg.targets.clear();
        cfg.copy_address = format!("0x{}", "00".repeat(20));
        assert!(cfg.validate().is_err());
    }
    #[test]
    fn live_calldata_replay_requires_explicit_acknowledgement() {
        let cfg = config("dry_run = false");
        assert!(cfg.validate().is_err());
    }
    async fn mock_rpc(
        fail_call: bool,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let task = tokio::spawn(async move {
            let mut blocks = 0;
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                let body = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(p) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..p]);
                        let length: usize = header
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= p + 4 + length {
                            break request[p + 4..p + 4 + length].to_vec();
                        }
                    }
                };
                let body: Value = serde_json::from_slice(&body).unwrap();
                let method = body["method"].as_str().unwrap();
                seen.lock().unwrap().push(method.to_string());
                let result = match method {
                    "eth_chainId" => json!("0x185d9"),
                    "eth_call" => {
                        assert_eq!(
                            body["params"][0]["from"],
                            "0x1111111111111111111111111111111111111111"
                        );
                        assert_eq!(body["params"][0]["data"], "0x01020304");
                        json!("0x")
                    }
                    "eth_blockNumber" => {
                        blocks += 1;
                        if blocks == 1 {
                            json!("0x1")
                        } else {
                            json!("0x2")
                        }
                    }
                    "eth_getBlockByNumber" => {
                        let tx = transfer();
                        json!({"transactions":[{"hash":tx.hash,"from":tx.from,"to":tx.to,"value":"0x0","input":"0x01020304","chainId":"0x185d9","nonce":"0x9","type":"0x2"}]})
                    }
                    "eth_estimateGas" => json!("0x5208"),
                    "eth_gasPrice" => json!("0x1"),
                    _ => panic!("unexpected method {method}"),
                };
                let response = if fail_call && method=="eth_call" {json!({"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}})}else{json!({"jsonrpc":"2.0","id":1,"result":result})}.to_string();
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response.len(),
                            response
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        (endpoint, calls, task)
    }
    fn transfer() -> ObservedTx {
        ObservedTx {
            hash: format!("0x{}", "12".repeat(32)),
            from: format!("0x{}", "22".repeat(20)),
            nonce: 9,
            to: Some(format!("0x{}", "33".repeat(20))),
            value: 0,
            input: vec![1, 2, 3, 4],
            chain_id: 99801,
        }
    }
    #[tokio::test]
    async fn dry_run_uses_real_simulation_without_signing_or_sending() {
        let (url, calls, task) = mock_rpc(false).await;
        let mut cfg = config("");
        cfg.rpc_url = url;
        let vault = ApiKeyVault::new(std::path::PathBuf::from("/tmp/unused-evm-test-vault"));
        let result = process(
            &cfg,
            &Rpc::new(&cfg.rpc_url).unwrap(),
            &transfer(),
            "test",
            &|| true,
            &vault,
        )
        .await
        .unwrap();
        assert_eq!(result.status, "simulated");
        assert_eq!(result.gas, Some(21000));
        assert!(result.transaction_hash.is_none());
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_estimateGas"]
        );
        task.abort();
    }
    #[tokio::test]
    async fn paused_live_never_simulates_or_reads_secrets() {
        let (url, calls, task) = mock_rpc(false).await;
        let mut cfg = config("dry_run=false\nallow_unmodified_calldata=true");
        cfg.rpc_url = url;
        let vault = ApiKeyVault::new(std::path::PathBuf::from("/tmp/unused-evm-test-vault"));
        let result = process(
            &cfg,
            &Rpc::new(&cfg.rpc_url).unwrap(),
            &transfer(),
            "test",
            &|| false,
            &vault,
        )
        .await
        .unwrap();
        assert_eq!(result.status, "awaiting_confirmation");
        assert!(calls.lock().unwrap().is_empty());
        task.abort();
    }
    #[tokio::test]
    async fn source_chain_mismatch_and_native_budget_reject_before_rpc() {
        let (url, calls, task) = mock_rpc(false).await;
        let mut cfg = config("");
        cfg.rpc_url = url;
        let vault = ApiKeyVault::new(std::path::PathBuf::from("/tmp/unused-evm-test-vault"));
        let mut tx = transfer();
        tx.chain_id = 1;
        assert!(process(
            &cfg,
            &Rpc::new(&cfg.rpc_url).unwrap(),
            &tx,
            "test",
            &|| true,
            &vault
        )
        .await
        .is_err());
        tx.chain_id = 99801;
        tx.value = 1;
        assert!(process(
            &cfg,
            &Rpc::new(&cfg.rpc_url).unwrap(),
            &tx,
            "test",
            &|| true,
            &vault
        )
        .await
        .is_err());
        assert!(calls.lock().unwrap().is_empty());
        task.abort();
    }

    #[test]
    fn confirmed_rpc_parsing_preserves_creation_and_rejects_foreign_chain() {
        let value = json!({"hash":format!("0x{}","ab".repeat(32)),"from":format!("0x{}","11".repeat(20)),"to":null,"value":"0x12","input":"0xdeadbeef","chainId":"0x185d9","nonce":"0x9","type":"0x2"});
        let tx = parse_rpc_tx(&value, 99801).unwrap();
        assert!(tx.to.is_none());
        assert_eq!(tx.value, 18);
        assert_eq!(tx.nonce, 9);
        assert_eq!(tx.input, vec![0xde, 0xad, 0xbe, 0xef]);
        assert!(parse_rpc_tx(&value, 46630).is_err());
    }
    #[test]
    fn feed_json_decodes_signed_envelope_and_ignores_system_messages() {
        use base64::Engine;
        let (raw, _) =
            tx::sign_replay(46630, 0, 21000, 1, None, 0, &[], &format!("{:064x}", 1)).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode([&[4][..], &raw].concat());
        let feed = json!({"version":1,"messages":[{"message":{"message":{"header":{"kind":3},"l2Msg":encoded}}},{"message":{"message":{"header":{"kind":6},"l2Msg":""}}}]});
        assert_eq!(parse_feed(&feed, 46630).unwrap().len(), 1);
        assert!(parse_feed(&feed, 4663).is_err());
    }
    #[test]
    fn future_mainnet_metadata_requires_explicit_opt_in() {
        let mut cfg = config("allow_new_chain=true");
        cfg.chain_id = 12345;
        cfg.validate().unwrap();
        cfg.chain_id = 0;
        assert!(cfg.validate().is_err());
    }
    #[tokio::test]
    async fn reverted_simulation_never_reaches_submission() {
        let (url, calls, server) = mock_rpc(true).await;
        let mut cfg = config("");
        cfg.rpc_url = url;
        let vault = ApiKeyVault::new(std::path::PathBuf::from("/tmp/unused-evm-test-vault"));
        let error = process(
            &cfg,
            &Rpc::new(&cfg.rpc_url).unwrap(),
            &transfer(),
            "test",
            &|| true,
            &vault,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("execution reverted"));
        assert_eq!(*calls.lock().unwrap(), vec!["eth_chainId", "eth_call"]);
        server.abort();
    }
    #[tokio::test]
    async fn confirmation_revoked_during_simulation_prevents_signing() {
        let (url, calls, server) = mock_rpc(false).await;
        let mut cfg = config("dry_run=false\nallow_unmodified_calldata=true");
        cfg.rpc_url = url;
        let vault = ApiKeyVault::new(std::path::PathBuf::from("/tmp/unused-evm-test-vault"));
        let confirmations = std::sync::atomic::AtomicUsize::new(0);
        let gate = || confirmations.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let error = process(
            &cfg,
            &Rpc::new(&cfg.rpc_url).unwrap(),
            &transfer(),
            "test",
            &gate,
            &vault,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("revoked"));
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_estimateGas"]
        );
        server.abort();
    }
    #[tokio::test]
    async fn confirmed_block_listener_emits_simulated_target_event() {
        let (url, calls, server) = mock_rpc(false).await;
        let mut cfg = config("");
        cfg.rpc_url = url;
        cfg.targets = vec![transfer().from];
        cfg.poll_interval_ms = 100;
        let vault = Arc::new(ApiKeyVault::new(std::path::PathBuf::from(
            "/tmp/unused-evm-test-vault",
        )));
        let (out, mut events) = tokio::sync::mpsc::channel(4);
        let worker = tokio::spawn(run(cfg, out, Arc::new(|| false), vault));
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.status, "simulated");
        assert_eq!(event.source, "rpc_confirmed");
        assert_eq!(event.source_hash, transfer().hash);
        assert!(!calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_sendRawTransaction"));
        eprintln!("local EVM pipeline latency: {} ms", event.latency_ms);
        drop(events);
        worker.await.unwrap().unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn sequencer_websocket_decodes_target_and_answers_ping() {
        use base64::Engine;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config("");
        cfg.chain = "robinhood".into();
        cfg.chain_id = 46630;
        cfg.sequencer_feed = true;
        cfg.ws_url = format!("ws://{}", listener.local_addr().unwrap());
        let (raw, signer) =
            tx::sign_replay(46630, 0, 21000, 1, None, 0, &[], &format!("{:064x}", 1)).unwrap();
        cfg.targets = vec![signer.clone()];
        let data = base64::engine::general_purpose::STANDARD.encode([&[4][..], &raw].concat());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
            ws.send(Message::Text(json!({"version":1,"messages":[{"message":{"message":{"header":{"kind":3},"l2Msg":data}}}]}).to_string())).await.unwrap();
            assert!(
                matches!(ws.next().await.unwrap().unwrap(),Message::Pong(data) if data==vec![1,2,3])
            );
            ws.close(None).await.unwrap();
        });
        let (out, mut incoming) = tokio::sync::mpsc::channel(4);
        let rpc = Rpc::new(&cfg.rpc_url).unwrap();
        let worker = tokio::spawn(async move { websocket_once(&cfg, &rpc, &out).await });
        let observed = tokio::time::timeout(Duration::from_secs(10), incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.tx.from, signer);
        assert_eq!(observed.source, "sequencer_feed");
        server.await.unwrap();
        worker.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn panel_rpc_overrides_blank_template_before_validation() {
        let dir = std::env::temp_dir().join(format!(
            "girasol-evm-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path,"chain='elysium'\nchain_id=99801\nrpc_url=''\ncopy_address='0x1111111111111111111111111111111111111111'").unwrap();
        let vault = ApiKeyVault::new(dir.join("vault"));
        vault
            .set_api_key("elysium-rpc", "http://127.0.0.1:8545")
            .await
            .unwrap();
        let cfg = EvmConfig::load_with_vault(&path, &vault).await.unwrap();
        assert_eq!(cfg.rpc_url, "http://127.0.0.1:8545");
        assert!(cfg.dry_run);
    }
    async fn assert_replacement_is_not_replayed(sequencer_feed: bool) {
        use base64::Engine;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let (rpc_url, _, rpc_server) = mock_rpc(false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config("");
        cfg.rpc_url = rpc_url;
        cfg.ws_url = format!("ws://{}", listener.local_addr().unwrap());
        cfg.sequencer_feed = sequencer_feed;
        cfg.poll_interval_ms = 100;
        let mut frames = Vec::new();
        let mut hashes = Vec::new();
        for (index, nonce) in [9, 9, 10].into_iter().enumerate() {
            let (hash, from, frame) = if sequencer_feed {
                let (raw, signer) = tx::sign_replay(
                    99801,
                    nonce,
                    21000,
                    index as u128 + 1,
                    transfer().to.as_deref(),
                    0,
                    &[1, 2, 3, 4],
                    &format!("{:064x}", 1),
                )
                .unwrap();
                let hash = tx::decode_signed(&raw).unwrap().hash;
                let payload =
                    base64::engine::general_purpose::STANDARD.encode([&[4][..], &raw].concat());
                (
                    hash,
                    signer,
                    json!({"version":1,"messages":[{"message":{"message":{"header":{"kind":3},"l2Msg":payload}}}]}),
                )
            } else {
                let hash = format!("0x{}", format!("{:02x}", 0x12 + index).repeat(32));
                let from = transfer().from;
                let frame = json!({"method":"eth_subscription","params":{"result":{"hash":hash,"from":from,"to":transfer().to,"value":"0x0","input":"0x01020304","chainId":"0x185d9","nonce":format!("0x{nonce:x}"),"type":"0x2"}}});
                (hash, from, frame)
            };
            cfg.targets = vec![from];
            hashes.push(hash);
            frames.push(frame);
        }
        let ws_server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            if !sequencer_feed {
                ws.next().await.unwrap().unwrap();
            }
            for frame in frames {
                ws.send(Message::Text(frame.to_string())).await.unwrap();
            }
            std::future::pending::<()>().await;
        });
        let vault = Arc::new(ApiKeyVault::new(std::path::PathBuf::from(
            "/tmp/unused-evm-test-vault",
        )));
        let (out, mut events) = tokio::sync::mpsc::channel(4);
        let worker = tokio::spawn(run(cfg, out, Arc::new(|| false), vault));
        let first = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        let next = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        drop(events);
        worker.await.unwrap().unwrap();
        ws_server.abort();
        rpc_server.abort();
        assert_eq!(first.source_hash, hashes[0]);
        assert_eq!(
            next.source_hash, hashes[2],
            "replacement with the same sender/nonce must not create another replay"
        );
    }
    #[tokio::test]
    async fn pending_rpc_replacement_is_replayed_once_but_next_nonce_is_allowed() {
        assert_replacement_is_not_replayed(false).await;
    }
    #[tokio::test]
    async fn sequencer_replacement_is_replayed_once_but_next_nonce_is_allowed() {
        assert_replacement_is_not_replayed(true).await;
    }
}

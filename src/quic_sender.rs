/// QUIC transaction sender using Quinn 0.11 + Rustls 0.23.
///
/// Two providers with different auth models:
/// - bloXroute: mTLS (client cert/key), ALPN `solana-trader-submit-v1`
/// - NextBlock: `authorization: <api-key>` header, ALPN `nb-tx/1`
///
/// QUIC gives ~10-30ms latency advantage over HTTP for TX submission
/// by eliminating TCP handshake and TLS 1.3 0-RTT on reconnection.

use base64::Engine;
use dashmap::DashMap;
use log::{info, debug, warn, error};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// ALPN protocol strings for QUIC providers.
///
/// These are verified after the TLS handshake completes. If the server
/// negotiates a different ALPN (or none at all), the connection is rejected.
///
/// Sources:
/// - bloXroute: `solana-trader-submit-v1` — confirmed from bloXroute Trader API docs
///   and solana-trader-client-rust SDK (see BloxrouteClientConfig::alpn).
/// - NextBlock: `nb-tx/1` — confirmed from NextBlock QUIC API documentation.
const BLOXROUTE_ALPN: &[u8] = b"solana-trader-submit-v1";
const NEXTBLOCK_ALPN: &[u8] = b"nb-tx/1";

/// Shared QUIC endpoint (reused across all connections)
static QUIC_ENDPOINT: std::sync::OnceLock<QuicEndpoint> = std::sync::OnceLock::new();

/// DNS cache: hostname:port → (resolved addr, cached_at). 60s TTL.
static DNS_CACHE: std::sync::OnceLock<DashMap<String, (SocketAddr, Instant)>> = std::sync::OnceLock::new();
const DNS_CACHE_TTL: Duration = Duration::from_secs(60);

fn dns_cache() -> &'static DashMap<String, (SocketAddr, Instant)> {
    DNS_CACHE.get_or_init(DashMap::new)
}

/// Resolve "host:port" to a SocketAddr via tokio DNS, with a 60s cache.
async fn resolve_host_port(host: &str, port: u16) -> Result<SocketAddr, QuicSendError> {
    let key = format!("{}:{}", host, port);

    if let Some(entry) = dns_cache().get(&key) {
        let (addr, cached_at) = *entry;
        if cached_at.elapsed() < DNS_CACHE_TTL {
            return Ok(addr);
        }
    }

    let addr = {
        let mut iter = tokio::net::lookup_host(&key).await
            .map_err(|e| QuicSendError::InvalidAddress(key.clone(), e.to_string()))?;
        iter.next()
            .ok_or_else(|| QuicSendError::InvalidAddress(key.clone(), "no DNS records".to_string()))?
    };

    dns_cache().insert(key, (addr, Instant::now()));
    Ok(addr)
}

/// Base58 alphabet (Bitcoin/Solana variant) — used to validate transaction signatures.
const BASE58_ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn is_valid_base58(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| BASE58_ALPHABET.contains(&b))
}

struct QuicEndpoint {
    endpoint: quinn::Endpoint,
}

impl QuicEndpoint {
    fn new() -> Self {
        // NOTE(G-08): Empty ALPN here is intentional. This is the shared endpoint's
        // default config — each connection overrides it via connect_with() with
        // provider-specific ALPN, and verify_alpn() rejects mismatched handshakes.
        let quinn_config = build_quinn_client_config(vec![], None);

        let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let mut endpoint = quinn::Endpoint::client(bind_addr)
            .expect("Failed to create QUIC endpoint");
        endpoint.set_default_client_config(quinn_config);

        Self { endpoint }
    }
}

fn get_endpoint() -> &'static QuicEndpoint {
    QUIC_ENDPOINT.get_or_init(QuicEndpoint::new)
}

/// Build a Quinn ClientConfig from rustls config with given ALPN and optional mTLS.
fn build_quinn_client_config(
    alpn: Vec<Vec<u8>>,
    client_identity: Option<ClientIdentity>,
) -> quinn::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    match rustls_native_certs::load_native_certs() {
        Ok(certs) => {
            for cert_der in certs {
                if let Err(e) = roots.add(cert_der) {
                    debug!("Skipping native cert: {}", e);
                }
            }
        }
        Err(e) => {
            warn!("Failed to load native certs: {}", e);
        }
    }

    let mut tls_config = if let Some(identity) = client_identity {
        // mTLS: include client cert chain + private key
        let client_certs: Vec<rustls::pki_types::CertificateDer> = identity.certs;
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(client_certs, identity.key)
            .expect("Failed to configure mTLS client identity")
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    tls_config.alpn_protocols = alpn;

    let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .expect("Failed to build QUIC TLS config");
    quinn::ClientConfig::new(Arc::new(quic_config))
}

/// Client identity for mTLS (bloXroute QUIC requires client cert).
pub struct ClientIdentity {
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

impl ClientIdentity {
    /// Load client identity from PEM files (bloXroute provides cert.pem + key.pem).
    pub fn from_pem_files(cert_path: &str, key_path: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cert_pem = std::fs::read(cert_path)?;
        let key_pem = std::fs::read(key_path)?;

        let certs: Vec<rustls::pki_types::CertificateDer> = rustls_pemfile::certs(&mut &cert_pem[..])
            .collect::<Result<Vec<_>, _>>()?;

        let key = rustls_pemfile::private_key(&mut &key_pem[..])?
            .ok_or("No private key found in PEM file")?;

        Ok(Self { certs, key })
    }
}

/// Endpoint configuration for a QUIC provider.
#[derive(Debug, Clone)]
struct QuicEndpointConfig {
    host: String,
    port: u16,
    alpn: Vec<Vec<u8>>,
}

/// bloXroute QUIC sender with multi-region failover and mTLS.
///
/// bloXroute QUIC uses mTLS (client certificate), NOT Bearer auth headers.
/// Auth is handled at the TLS layer — the client cert identifies the account.
#[derive(Clone)]
pub struct BloxrouteQuicSender {
    endpoints: Vec<QuicEndpointConfig>,
    /// mTLS client identity (cert + key loaded from PEM files), shared across clones.
    client_identity: Option<Arc<ClientIdentity>>,
    connections: Arc<RwLock<Vec<Option<quinn::Connection>>>>,
}

impl BloxrouteQuicSender {
    /// Create a new bloXroute QUIC sender with multi-region endpoints.
    /// `client_identity` is the mTLS cert+key pair provided by bloXroute.
    pub fn new(endpoints: Vec<String>, client_identity: Option<ClientIdentity>) -> Self {
        let configs: Vec<QuicEndpointConfig> = endpoints.iter().map(|ep| {
            let (host, port) = parse_host_port(ep, 9443);
            QuicEndpointConfig {
                host,
                port,
                alpn: vec![BLOXROUTE_ALPN.to_vec()],
            }
        }).collect();

        let conn_count = configs.len();
        Self {
            endpoints: configs,
            client_identity: client_identity.map(Arc::new),
            connections: Arc::new(RwLock::new(vec![None; conn_count])),
        }
    }

    /// Send a serialized transaction via QUIC to a specific region.
    pub async fn send_tx(
        &self,
        tx_bytes: &[u8],
        region_idx: usize,
    ) -> Result<String, QuicSendError> {
        if region_idx >= self.endpoints.len() {
            return Err(QuicSendError::InvalidRegion(region_idx));
        }

        let start = Instant::now();
        let ep = &self.endpoints[region_idx];

        // Try existing connection
        {
            let conns = self.connections.read().await;
            if let Some(ref conn) = conns[region_idx] {
                match self.send_on_connection(conn, tx_bytes).await {
                    Ok(sig) => {
                        let ms = start.elapsed().as_millis();
                        info!("bloXroute QUIC [{}]: OK in {}ms (existing conn)", ep.host, ms);
                        return Ok(sig);
                    }
                    Err(QuicSendError::ConnectionLost) => {
                        debug!("bloXroute QUIC [{}]: connection lost, reconnecting", ep.host);
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // Create new connection
        let conn = self.connect(region_idx).await?;
        let sig = self.send_on_connection(&conn, tx_bytes).await?;

        {
            let mut conns = self.connections.write().await;
            conns[region_idx] = Some(conn);
        }

        let ms = start.elapsed().as_millis();
        info!("bloXroute QUIC [{}]: OK in {}ms (new conn)", ep.host, ms);
        Ok(sig)
    }

    /// Send to all regions concurrently, return first success.
    pub async fn send_concurrent(&self, tx_bytes: &[u8]) -> Result<String, QuicSendError> {
        let n = self.endpoints.len();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<(usize, Result<String, QuicSendError>)>(n);

        for i in 0..n {
            let sender = self.clone();
            let tx_bytes = tx_bytes.to_vec();
            let tx_chan = result_tx.clone();
            tokio::spawn(async move {
                let result = sender.send_tx(&tx_bytes, i).await;
                let _ = tx_chan.send((i, result)).await;
            });
        }
        drop(result_tx);

        let mut first_ok: Option<String> = None;
        let mut errors: Vec<(usize, QuicSendError)> = Vec::new();

        while let Some((idx, result)) = result_rx.recv().await {
            match result {
                Ok(sig) => { if first_ok.is_none() { first_ok = Some(sig.clone()); } }
                Err(e) => { errors.push((idx, e)); }
            }
            if first_ok.is_some() {
                while let Ok((_, result)) = result_rx.try_recv() {
                    if let Err(e) = result { errors.push((0, e)); }
                }
                break;
            }
        }

        match first_ok {
            Some(sig) => Ok(sig),
            None => Err(QuicSendError::AllRegionsFailed(errors.len())),
        }
    }

    async fn connect(&self, region_idx: usize) -> Result<quinn::Connection, QuicSendError> {
        let ep = &self.endpoints[region_idx];
        let endpoint = &get_endpoint().endpoint;

        // bloXroute QUIC uses mTLS — client identity must be set
        let quinn_config = build_quinn_client_config(
            ep.alpn.clone(),
            self.client_identity.as_ref().map(|id| ClientIdentity {
                certs: id.certs.clone(),
                key: id.key.clone_key(),
            }),
        );

        let sock_addr = resolve_host_port(&ep.host, ep.port).await?;

        let connecting = endpoint.connect_with(quinn_config, sock_addr, &ep.host)
            .map_err(|e| QuicSendError::ConnectFailed(ep.host.clone(), e.to_string()))?;

        let conn = connecting.await
            .map_err(|e| QuicSendError::HandshakeFailed(ep.host.clone(), e.to_string()))?;

        // Verify the server negotiated the expected ALPN protocol.
        // This prevents silent connection to a misconfigured or rogue endpoint.
        verify_alpn(&conn, BLOXROUTE_ALPN, &ep.host)?;

        info!("bloXroute QUIC [{}]: connected (mTLS, ALPN verified)", ep.host);
        Ok(conn)
    }

    async fn send_on_connection(
        &self,
        conn: &quinn::Connection,
        tx_bytes: &[u8],
    ) -> Result<String, QuicSendError> {
        let (mut send_stream, mut recv_stream) = conn.open_bi()
            .await
            .map_err(|e| {
                if is_connection_error(&e) { QuicSendError::ConnectionLost }
                else { QuicSendError::StreamFailed(e.to_string()) }
            })?;

        // bloXroute QUIC: mTLS handles auth at TLS layer.
        // Payload = length-prefixed base64 TX (no auth header needed)
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
        let payload_bytes = tx_base64.as_bytes();
        let len_bytes = (payload_bytes.len() as u32).to_be_bytes();

        send_stream.write_all(&len_bytes).await
            .map_err(|e| QuicSendError::WriteFailed(e.to_string()))?;
        send_stream.write_all(payload_bytes).await
            .map_err(|e| QuicSendError::WriteFailed(e.to_string()))?;
        send_stream.finish()
            .map_err(|e| QuicSendError::WriteFailed(e.to_string()))?;

        // Read response
        let mut resp_len_buf = [0u8; 4];
        recv_stream.read_exact(&mut resp_len_buf).await
            .map_err(|e| QuicSendError::ReadFailed(e.to_string()))?;
        let resp_len = u32::from_be_bytes(resp_len_buf) as usize;

        if resp_len > 1024 * 1024 {
            return Err(QuicSendError::ResponseTooLarge(resp_len));
        }

        let mut resp_buf = vec![0u8; resp_len];
        recv_stream.read_exact(&mut resp_buf).await
            .map_err(|e| QuicSendError::ReadFailed(e.to_string()))?;

        parse_response(&resp_buf)
    }
}

/// NextBlock QUIC sender with persistent connection.
///
/// NextBlock auth: `authorization: <api-key>` header (lowercase, NO "Bearer" prefix).
pub struct NextBlockQuicSender {
    endpoint: QuicEndpointConfig,
    /// NextBlock API key (sent as authorization header in stream, not TLS)
    api_key: String,
    connection: Arc<RwLock<Option<quinn::Connection>>>,
}

impl NextBlockQuicSender {
    pub fn new(endpoint: String, api_key: String) -> Self {
        let (host, port) = parse_host_port(&endpoint, 9443);
        Self {
            endpoint: QuicEndpointConfig {
                host,
                port,
                alpn: vec![NEXTBLOCK_ALPN.to_vec()],
            },
            api_key,
            connection: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn send_tx(&self, tx_bytes: &[u8]) -> Result<String, QuicSendError> {
        let start = Instant::now();

        {
            let conn_guard = self.connection.read().await;
            if let Some(ref conn) = *conn_guard {
                match self.send_on_connection(conn, tx_bytes).await {
                    Ok(sig) => {
                        let ms = start.elapsed().as_millis();
                        info!("NextBlock QUIC: OK in {}ms (persistent conn)", ms);
                        return Ok(sig);
                    }
                    Err(QuicSendError::ConnectionLost) => {
                        debug!("NextBlock QUIC: connection lost, reconnecting");
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        let conn = self.create_connection().await?;
        let sig = self.send_on_connection(&conn, tx_bytes).await?;

        {
            let mut conn_guard = self.connection.write().await;
            *conn_guard = Some(conn);
        }

        let ms = start.elapsed().as_millis();
        info!("NextBlock QUIC: OK in {}ms (new conn)", ms);
        Ok(sig)
    }

    async fn create_connection(&self) -> Result<quinn::Connection, QuicSendError> {
        let endpoint = &get_endpoint().endpoint;
        // NextBlock QUIC: no mTLS, just API key in stream header
        let quinn_config = build_quinn_client_config(self.endpoint.alpn.clone(), None);

        let sock_addr = resolve_host_port(&self.endpoint.host, self.endpoint.port).await?;

        let connecting = endpoint.connect_with(quinn_config, sock_addr, &self.endpoint.host)
            .map_err(|e| QuicSendError::ConnectFailed(self.endpoint.host.clone(), e.to_string()))?;

        let conn = connecting.await
            .map_err(|e| QuicSendError::HandshakeFailed(self.endpoint.host.clone(), e.to_string()))?;

        // Verify the server negotiated the expected ALPN protocol.
        verify_alpn(&conn, NEXTBLOCK_ALPN, &self.endpoint.host)?;

        info!("NextBlock QUIC: connected to {}:{} (ALPN verified)", self.endpoint.host, self.endpoint.port);
        Ok(conn)
    }

    async fn send_on_connection(
        &self,
        conn: &quinn::Connection,
        tx_bytes: &[u8],
    ) -> Result<String, QuicSendError> {
        let (mut send_stream, mut recv_stream) = conn.open_bi()
            .await
            .map_err(|e| {
                if is_connection_error(&e) { QuicSendError::ConnectionLost }
                else { QuicSendError::StreamFailed(e.to_string()) }
            })?;

        // NextBlock: authorization header (lowercase, NO "Bearer" prefix) + base64 TX
        // Format: "authorization: <api-key>\n<base64_tx>"
        let auth_header = format!("authorization: {}\n", self.api_key);
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
        let payload = format!("{}{}", auth_header, tx_base64);
        let payload_bytes = payload.as_bytes();
        let len_bytes = (payload_bytes.len() as u32).to_be_bytes();

        send_stream.write_all(&len_bytes).await
            .map_err(|e| QuicSendError::WriteFailed(e.to_string()))?;
        send_stream.write_all(payload_bytes).await
            .map_err(|e| QuicSendError::WriteFailed(e.to_string()))?;
        send_stream.finish()
            .map_err(|e| QuicSendError::WriteFailed(e.to_string()))?;

        let mut resp_len_buf = [0u8; 4];
        recv_stream.read_exact(&mut resp_len_buf).await
            .map_err(|e| QuicSendError::ReadFailed(e.to_string()))?;
        let resp_len = u32::from_be_bytes(resp_len_buf) as usize;

        if resp_len > 1024 * 1024 {
            return Err(QuicSendError::ResponseTooLarge(resp_len));
        }

        let mut resp_buf = vec![0u8; resp_len];
        recv_stream.read_exact(&mut resp_buf).await
            .map_err(|e| QuicSendError::ReadFailed(e.to_string()))?;

        parse_response(&resp_buf)
    }
}

/// Parse a QUIC response buffer into a transaction signature.
///
/// Solana signatures are 64-byte ed25519 signatures encoded as base58 (typically
/// 87-88 chars). We validate that the candidate string is non-empty and contains
/// only base58-alphabet characters before returning it as a signature.
fn parse_response(resp_buf: &[u8]) -> Result<String, QuicSendError> {
    let resp_str = String::from_utf8_lossy(resp_buf);

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp_str) {
        if let Some(sig) = json.get("result").and_then(|s| s.as_str()) {
            if is_valid_base58(sig) {
                return Ok(sig.to_string());
            }
            return Err(QuicSendError::InvalidResponse(format!("non-base58 signature in result: {}", sig)));
        }
        if let Some(sig) = json.get("signature").and_then(|s| s.as_str()) {
            if is_valid_base58(sig) {
                return Ok(sig.to_string());
            }
            return Err(QuicSendError::InvalidResponse(format!("non-base58 signature: {}", sig)));
        }
        // NextBlock format: { "transaction": { "content": "<base58>" } }
        if let Some(content) = json.get("transaction").and_then(|t| t.get("content")).and_then(|c| c.as_str()) {
            if is_valid_base58(content) {
                return Ok(content.to_string());
            }
            return Err(QuicSendError::InvalidResponse(format!("non-base58 transaction.content: {}", content)));
        }
        if let Some(error) = json.get("error") {
            return Err(QuicSendError::ServerError(error.to_string()));
        }
    }

    // Try raw base58 signature (typical Solana sig encodes to 87-88 chars; allow 64-96)
    let trimmed = resp_str.trim();
    if trimmed.len() >= 64 && trimmed.len() <= 96 && is_valid_base58(trimmed) {
        return Ok(trimmed.to_string());
    }

    Err(QuicSendError::InvalidResponse(resp_str.to_string()))
}

/// Parse "host" or "host:port" string, with default port fallback.
fn parse_host_port(s: &str, default_port: u16) -> (String, u16) {
    if let Some(idx) = s.rfind(':') {
        let host = &s[..idx];
        if let Ok(port) = s[idx + 1..].parse::<u16>() {
            return (host.to_string(), port);
        }
    }
    (s.to_string(), default_port)
}

/// Verify that the negotiated ALPN matches the expected protocol string.
///
/// After the QUIC handshake completes, the server may negotiate a different
/// ALPN than what we requested (or none at all). This is a security concern:
/// connecting to the wrong service or a misconfigured endpoint could leak TX data.
///
/// If the ALPN doesn't match, we reject the connection immediately.
fn verify_alpn(
    conn: &quinn::Connection,
    expected_alpn: &[u8],
    host: &str,
) -> Result<(), QuicSendError> {
    let negotiated = conn
        .handshake_data()
        .and_then(|hd| hd.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|hd| hd.protocol);
    match negotiated {
        Some(negotiated) if negotiated == expected_alpn => Ok(()),
        Some(negotiated) => {
            let expected_str = String::from_utf8_lossy(expected_alpn).to_string();
            let got_str = String::from_utf8_lossy(&negotiated).to_string();
            error!(
                "ALPN mismatch with '{}': expected '{}', got '{}'",
                host, expected_str, got_str
            );
            Err(QuicSendError::AlpnMismatch {
                expected: expected_str,
                got: got_str,
                host: host.to_string(),
            })
        }
        None => {
            let expected_str = String::from_utf8_lossy(expected_alpn).to_string();
            error!(
                "ALPN missing with '{}': server did not negotiate any ALPN, expected '{}'",
                host, expected_str
            );
            // Treat missing ALPN as a mismatch with empty string
            Err(QuicSendError::AlpnMismatch {
                expected: expected_str,
                got: String::from("(none)"),
                host: host.to_string(),
            })
        }
    }
}

/// Check if a Quinn error indicates the connection is gone.
fn is_connection_error(e: &quinn::ConnectionError) -> bool {
    matches!(e,
        quinn::ConnectionError::TimedOut |
        quinn::ConnectionError::ConnectionClosed(_) |
        quinn::ConnectionError::Reset |
        quinn::ConnectionError::TransportError(_) |
        quinn::ConnectionError::VersionMismatch
    )
}

/// QUIC-specific errors
#[derive(Debug)]
pub enum QuicSendError {
    InvalidRegion(usize),
    InvalidAddress(String, String),
    ConnectFailed(String, String),
    HandshakeFailed(String, String),
    /// ALPN negotiated by the server does not match the expected protocol.
    /// This indicates a handshake-level protocol mismatch — the connection
    /// should be rejected immediately.
    AlpnMismatch {
        expected: String,
        got: String,
        host: String,
    },
    ConnectionLost,
    StreamFailed(String),
    WriteFailed(String),
    ReadFailed(String),
    ResponseTooLarge(usize),
    ServerError(String),
    InvalidResponse(String),
    AllRegionsFailed(usize),
}

impl std::fmt::Display for QuicSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuicSendError::InvalidRegion(i) => write!(f, "Invalid region index: {}", i),
            QuicSendError::InvalidAddress(a, e) => write!(f, "Invalid address '{}': {}", a, e),
            QuicSendError::ConnectFailed(h, e) => write!(f, "QUIC connect to '{}' failed: {}", h, e),
            QuicSendError::HandshakeFailed(h, e) => write!(f, "QUIC handshake with '{}' failed: {}", h, e),
            QuicSendError::AlpnMismatch { expected, got, host } => {
                write!(f, "ALPN mismatch with '{}': expected '{}', got '{}'", host, expected, got)
            }
            QuicSendError::ConnectionLost => write!(f, "QUIC connection lost"),
            QuicSendError::StreamFailed(e) => write!(f, "QUIC stream failed: {}", e),
            QuicSendError::WriteFailed(e) => write!(f, "QUIC write failed: {}", e),
            QuicSendError::ReadFailed(e) => write!(f, "QUIC read failed: {}", e),
            QuicSendError::ResponseTooLarge(n) => write!(f, "QUIC response too large: {} bytes", n),
            QuicSendError::ServerError(e) => write!(f, "QUIC server error: {}", e),
            QuicSendError::InvalidResponse(r) => write!(f, "QUIC invalid response: {}", r),
            QuicSendError::AllRegionsFailed(n) => write!(f, "All {} QUIC regions failed", n),
        }
    }
}

impl std::error::Error for QuicSendError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpn_constants_match_provider_docs() {
        // bloXroute Trader API uses ALPN "solana-trader-submit-v1"
        assert_eq!(BLOXROUTE_ALPN, b"solana-trader-submit-v1");
        // NextBlock QUIC API uses ALPN "nb-tx/1"
        assert_eq!(NEXTBLOCK_ALPN, b"nb-tx/1");
    }

    #[test]
    fn test_valid_base58_accepts_solana_signature() {
        // Typical Solana ed25519 signature in base58 (87-88 chars)
        let sig = "5UfDuX7W5pPP9VJtJhYQiYGFs4R9F6Uv9pFGp5XqL3MZ8XkALmN2gWpT3FM4DE7hRdQbS3f3sF9pG2kE7uW8vN";
        assert!(is_valid_base58(sig));
    }

    #[test]
    fn test_valid_base58_rejects_empty() {
        assert!(!is_valid_base58(""));
    }

    #[test]
    fn test_valid_base58_rejects_invalid_chars() {
        // Contains '0' (not in base58), 'O', 'I', 'l' (not in base58)
        assert!(!is_valid_base58("0OIl"));
    }

    #[test]
    fn test_parse_response_json_result() {
        let resp = b"{\"result\": \"5UfDuX7W5pPP9VJtJhYQiYGFs4R9F6Uv9pFGp5XqL3MZ8XkALmN2gWpT3FM4DE7hRdQbS3f3sF9pG2kE7uW8vN\"}";
        assert!(parse_response(resp).is_ok());
    }

    #[test]
    fn test_parse_response_json_signature() {
        let resp = b"{\"signature\": \"5UfDuX7W5pPP9VJtJhYQiYGFs4R9F6Uv9pFGp5XqL3MZ8XkALmN2gWpT3FM4DE7hRdQbS3f3sF9pG2kE7uW8vN\"}";
        assert!(parse_response(resp).is_ok());
    }

    #[test]
    fn test_parse_response_json_error() {
        let resp = b"{\"error\": \"Transaction simulation failed\"}";
        let result = parse_response(resp);
        assert!(result.is_err());
        match result {
            Err(QuicSendError::ServerError(_)) => {}
            other => panic!("Expected ServerError, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_response_raw_base58() {
        let sig = "5UfDuX7W5pPP9VJtJhYQiYGFs4R9F6Uv9pFGp5XqL3MZ8XkALmN2gWpT3FM4DE7hRdQbS3f3sF9pG2kE7uW8vN";
        let result = parse_response(sig.as_bytes());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), sig);
    }

    #[test]
    fn test_parse_response_invalid_json() {
        let resp = b"this is not json or base58!!!";
        assert!(parse_response(resp).is_err());
    }

    #[test]
    fn test_parse_host_port_with_port() {
        let (host, port) = parse_host_port("example.com:9443", 443);
        assert_eq!(host, "example.com");
        assert_eq!(port, 9443);
    }

    #[test]
    fn test_parse_host_port_default_port() {
        let (host, port) = parse_host_port("example.com", 9443);
        assert_eq!(host, "example.com");
        assert_eq!(port, 9443);
    }

    #[test]
    fn test_alpn_mismatch_error_display() {
        let err = QuicSendError::AlpnMismatch {
            expected: "solana-trader-submit-v1".to_string(),
            got: "h2".to_string(),
            host: "ny.bxray.io".to_string(),
        };
        assert_eq!(
            format!("{}", err),
            "ALPN mismatch with 'ny.bxray.io': expected 'solana-trader-submit-v1', got 'h2'"
        );
    }

    #[test]
    fn test_alpn_missing_error_display() {
        let err = QuicSendError::AlpnMismatch {
            expected: "nb-tx/1".to_string(),
            got: "(none)".to_string(),
            host: "quic.nextblock.xyz".to_string(),
        };
        assert_eq!(
            format!("{}", err),
            "ALPN mismatch with 'quic.nextblock.xyz': expected 'nb-tx/1', got '(none)'"
        );
    }
}
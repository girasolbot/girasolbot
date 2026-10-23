//! Local dashboard. HTML is public; every API request requires bearer auth.
//! Token is generated once in DATA_DIR/panel-auth-token (0600), never in HTML.
use crate::api_key_vault::{ApiKeyVault, API_KEY_SLOTS};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const LIVE_CONFIRMATION: &str = "I UNDERSTAND REAL FUNDS ARE AT RISK";

pub struct KeyPanel {
    vault: Arc<ApiKeyVault>,
    auth_token: String,
    data_dir: PathBuf,
    runtime: Arc<crate::runtime::RuntimeState>,
}

fn local_token(data_dir: &Path) -> Result<String, String> {
    use rand::RngCore;
    let path = data_dir.join("panel-auth-token");
    if let Ok(token) = std::env::var("PANEL_AUTH_TOKEN") {
        return if token.trim().len() >= 16 {
            Ok(token.trim().to_owned())
        } else {
            Err("PANEL_AUTH_TOKEN must contain at least 16 characters".into())
        };
    }
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.is_file() {
            return Err("panel-auth-token must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        let token = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if token.trim().len() < 16 {
            return Err("stored panel token is invalid".into());
        }
        return Ok(token.trim().into());
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options
        .open(&path)
        .and_then(|mut f| {
            f.write_all(token.as_bytes())?;
            f.sync_all()
        })
        .map_err(|e| e.to_string())?;
    log::info!("Panel bearer token saved to {}", path.display());
    Ok(token)
}

pub async fn start_panel(
    bind_addr: &str,
    data_dir: PathBuf,
    runtime: Arc<crate::runtime::RuntimeState>,
    vault: Arc<ApiKeyVault>,
) -> Result<(), String> {
    let auth_token = local_token(&data_dir)?;
    let panel = Arc::new(KeyPanel {
        vault,
        data_dir,
        auth_token,
        runtime,
    });
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|e| format!("panel bind failed: {}", e))?;
    let slots = Arc::new(tokio::sync::Semaphore::new(32));
    log::info!("Girasol dashboard: http://{}", bind_addr);
    loop {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let permit = match slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                drop(stream);
                continue;
            }
        };
        let panel = panel.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle_connection(stream, panel).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Result<Request, u16> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            if pos > MAX_HEADERS {
                return Err(413);
            }
            break pos;
        }
        if buf.len() > MAX_HEADERS {
            return Err(413);
        }
    };
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let parts: Vec<_> = lines.next().ok_or(400u16)?.split_whitespace().collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err(400);
    }
    let method = parts[0].to_owned();
    let path = parts[1].to_owned();
    let mut auth = None;
    let mut length = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        match name.to_ascii_lowercase().as_str() {
            "authorization" => {
                if auth.is_some() {
                    return Err(400);
                }
                auth = Some(value.trim().to_owned());
            }
            "content-length" => {
                if length.is_some() {
                    return Err(400);
                }
                let v = value.trim();
                if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(400);
                }
                length = Some(v.parse::<usize>().map_err(|_| 413u16)?);
            }
            "transfer-encoding" => return Err(400),
            _ => {}
        }
    }
    let length = length.unwrap_or(0);
    if length > MAX_BODY {
        return Err(413);
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let remaining = (length - body.len()).min(chunk.len());
        let n = stream
            .read(&mut chunk[..remaining])
            .await
            .map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok(Request {
        method,
        path,
        auth,
        body,
    })
}
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}
fn json_response(status: u16, value: Value) -> (u16, String, Vec<u8>) {
    (
        status,
        "application/json".into(),
        serde_json::to_vec(&value).unwrap_or_default(),
    )
}
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    panel: Arc<KeyPanel>,
) -> std::io::Result<()> {
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(req)) => {
            match tokio::time::timeout(Duration::from_secs(120), panel.route(&req)).await {
                Ok(response) => response,
                Err(_) => json_response(504, json!({"error":"operation timed out"})),
            }
        }
        Ok(Err(status)) => json_response(status, json!({"error":"invalid HTTP request"})),
        Err(_) => json_response(408, json!({"error":"request timed out"})),
    };
    let _ = tokio::time::timeout(
        REQUEST_TIMEOUT,
        write_response(&mut stream, response.0, &response.1, &response.2),
    )
    .await;
    Ok(())
}
impl KeyPanel {
    fn authorize(&self, req: &Request) -> bool {
        req.auth
            .as_deref()
            .and_then(|a| a.strip_prefix("Bearer "))
            .is_some_and(|token| constant_time_eq(token, &self.auth_token))
    }
    async fn route(&self, req: &Request) -> (u16, String, Vec<u8>) {
        let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        if req.method == "GET" && ["/", "/index.html"].contains(&path) {
            return (
                200,
                "text/html; charset=utf-8".into(),
                render_index().into_bytes(),
            );
        }
        if !self.authorize(req) {
            return json_response(401, json!({"error":"unauthorized"}));
        }
        let network = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("network="))
            .unwrap_or("solana");
        if !["solana", "robinhood", "elysium"].contains(&network) {
            return json_response(400, json!({"error":"unknown network"}));
        }
        let body = if req.method == "POST" {
            match serde_json::from_slice::<Value>(&req.body) {
                Ok(v) if v.is_object() => v,
                _ => return json_response(400, json!({"error":"expected a JSON object"})),
            }
        } else {
            Value::Null
        };
        let result = match (req.method.as_str(), path) {
            ("GET", "/api/panel/keys") => json!({"keys":self.vault.statuses().await}),
            ("GET", "/api/panel/health") => self.vault.vault_health(),
            ("GET", "/api/panel/slots") => {
                json!({"slots":API_KEY_SLOTS.iter().map(|s| json!({"id":s.id,"label":s.label,"pricing":s.pricing.as_str()})).collect::<Vec<_>>()})
            }
            ("GET", "/api/panel/status") => self.runtime.status(),
            ("GET", "/api/panel/positions") => {
                dataset(&self.data_dir.join("panel-positions.json"), network).await
            }
            ("GET", "/api/panel/history") => {
                dataset(&self.data_dir.join("trades.json"), network).await
            }
            ("GET", "/api/panel/pnl") => {
                json!({"realized":null,"unrealized":null,"available":false,"reason":"A complete cost basis, execution prices and fees are required. Trade history is available separately."})
            }
            ("GET", "/api/panel/logs") => read_logs().await,
            ("POST", "/api/panel/live") => {
                let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
                    return json_response(400, json!({"error":"enabled must be boolean"}));
                };
                if enabled && body["confirmation"].as_str() != Some(LIVE_CONFIRMATION) {
                    return json_response(
                        400,
                        json!({"error":"exact live confirmation phrase required"}),
                    );
                }
                self.runtime.set_live_confirmation(enabled);
                if enabled && !self.runtime.live_enabled() {
                    return json_response(
                        403,
                        json!({"error":"Live trading was not authorized at startup. Restart with --real."}),
                    );
                }
                self.runtime.status()
            }
            ("POST", p) if p.starts_with("/api/panel/keys/") => {
                let slot = &p["/api/panel/keys/".len()..];
                let Some(value) = body["value"].as_str() else {
                    return json_response(400, json!({"error":"value must be a string"}));
                };
                if value.trim().is_empty() {
                    return json_response(400, json!({"error":"use DELETE to remove a key"}));
                }
                match self.vault.set_api_key(slot, value).await {
                    Ok(()) => json!({"ok":true}),
                    Err(e) => return json_response(400, json!({"error":e})),
                }
            }
            ("DELETE", p) if p.starts_with("/api/panel/keys/") => {
                match self
                    .vault
                    .set_api_key(&p["/api/panel/keys/".len()..], "")
                    .await
                {
                    Ok(()) => json!({"ok":true}),
                    Err(e) => return json_response(400, json!({"error":e})),
                }
            }
            ("GET", "/api/intelligence/status")
            | (
                "POST",
                "/api/intelligence/profile"
                | "/api/intelligence/strategy"
                | "/api/intelligence/replan"
                | "/api/intelligence/underworld"
                | "/api/intelligence/paper"
                | "/api/ai/chat",
            ) => {
                match crate::intelligence::handle_api(
                    path,
                    body,
                    network,
                    &self.data_dir,
                    &self.vault,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => return json_response(400, json!({"error":e})),
                }
            }
            _ => return json_response(404, json!({"error":"not found"})),
        };
        json_response(200, result)
    }
}
async fn dataset(path: &Path, network: &str) -> Value {
    let bytes = match tokio::fs::File::open(path).await {
        Ok(file) => {
            let mut bytes = Vec::new();
            if file
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await
                .is_err()
                || bytes.len() > 4 * 1024 * 1024
            {
                return json!({"available":false,"error":"snapshot exceeds read limit"});
            }
            bytes
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({"available":false,"items":[],"reason":"No runtime observations yet"})
        }
        Err(_) => return json!({"available":false,"error":"snapshot unavailable"}),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => {
            let items: Vec<Value> = match value {
                Value::Array(a) => a,
                Value::Object(o) => o
                    .into_iter()
                    .map(|(id, mut v)| {
                        if let Some(o) = v.as_object_mut() {
                            o.entry("asset").or_insert(Value::String(id));
                        }
                        v
                    })
                    .collect(),
                _ => return json!({"available":false,"error":"invalid snapshot format"}),
            };
            let filtered: Vec<_> = items
                .into_iter()
                .filter(|v| v["network"].as_str().unwrap_or("solana") == network)
                .rev()
                .take(200)
                .collect();
            json!({"available":true,"items":filtered,"limit":200})
        }
        Err(_) => json!({"available":false,"error":"invalid snapshot JSON"}),
    }
}
async fn read_logs() -> Value {
    let path = Path::new("logs/sniper.log");
    match tokio::fs::File::open(path).await {
        Ok(mut file) => {
            let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
            let _ = file
                .seek(std::io::SeekFrom::Start(len.saturating_sub(64 * 1024)))
                .await;
            let mut bytes = Vec::new();
            let _ = file.take(64 * 1024).read_to_end(&mut bytes).await;
            let text = String::from_utf8_lossy(&bytes);
            // Existing logs may include transport URLs with credentials: redact entire sensitive lines.
            let lines: Vec<String> = text
                .lines()
                .rev()
                .take(100)
                .map(|line| {
                    let lower = line.to_ascii_lowercase();
                    if [
                        "http://",
                        "https://",
                        "wss://",
                        "token",
                        "secret",
                        "private",
                        "api_key",
                        "api-key",
                        "authorization",
                        "password",
                    ]
                    .iter()
                    .any(|s| lower.contains(s))
                    {
                        "[sensitive transport/configuration line redacted]".into()
                    } else {
                        line.chars().take(1000).collect()
                    }
                })
                .collect();
            json!({"available":true,"lines":lines.into_iter().rev().collect::<Vec<_>>()})
        }
        Err(_) => json!({"available":false,"lines":[],"reason":"Log file has not been created"}),
    }
}
fn constant_time_eq(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(a.as_bytes());
    let b = Sha256::digest(b.as_bytes());
    a.iter().zip(b.iter()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}
fn render_index() -> String {
    include_str!("panel.html").to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_json_preserves_key() {
        let _env = crate::api_key_vault::tests::ENV_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!("panel-malformed-{}", std::process::id()));
        let panel = KeyPanel {
            vault: Arc::new(ApiKeyVault::new(dir)),
            auth_token: "secret-token-1".into(),
            data_dir: std::env::temp_dir(),
            runtime: Arc::new(crate::runtime::RuntimeState::new(false)),
        };
        panel
            .vault
            .set_api_key("jito", "keep-this-secret")
            .await
            .unwrap();
        for body in ["{", "{}", "{\"value\":null}", "{\"value\":42}"] {
            let req = Request {
                method: "POST".into(),
                path: "/api/panel/keys/jito".into(),
                auth: Some("Bearer secret-token-1".into()),
                body: body.as_bytes().to_vec(),
            };
            assert_eq!(panel.route(&req).await.0, 400);
            assert_eq!(
                panel.vault.get_api_key("jito").await.as_deref(),
                Some("keep-this-secret")
            );
        }
    }

    fn test_panel(tag: &str, allow_live: bool) -> Arc<KeyPanel> {
        let dir =
            std::env::temp_dir().join(format!("girasol-panel-{}-{}", tag, rand::random::<u64>()));
        Arc::new(KeyPanel {
            vault: Arc::new(ApiKeyVault::new(dir.clone())),
            auth_token: "test-bearer-token".into(),
            data_dir: dir,
            runtime: Arc::new(crate::runtime::RuntimeState::new(allow_live)),
        })
    }
    async fn http(panel: Arc<KeyPanel>, raw: &[u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, panel).await.unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(raw).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(8), client.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        String::from_utf8(bytes).unwrap()
    }
    #[tokio::test]
    async fn http_auth_html_and_body_cap_smoke() {
        let panel = test_panel("http", false);
        assert!(
            http(panel.clone(), b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .starts_with("HTTP/1.1 200")
        );
        for path in ["status", "keys", "positions", "history", "pnl", "logs"] {
            let request = format!("GET /api/panel/{path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert!(http(panel.clone(), request.as_bytes())
                .await
                .starts_with("HTTP/1.1 401"));
        }
        assert!(http(
            panel.clone(),
            b"GET /api/panel/status HTTP/1.1\r\nAuthorization: Bearer test-bearer-token\r\n\r\n"
        )
        .await
        .starts_with("HTTP/1.1 200"));
        assert!(http(
            panel.clone(),
            b"POST /api/panel/keys/jito HTTP/1.1\r\nContent-Length: 65537\r\n\r\n"
        )
        .await
        .starts_with("HTTP/1.1 413"));
        assert!(http(
            panel.clone(),
            b"POST /api/panel/keys/jito HTTP/1.1\r\nContent-Length: rubbish\r\n\r\n"
        )
        .await
        .starts_with("HTTP/1.1 400"));
        let mut oversized = b"GET / HTTP/1.1\r\nX-Test: ".to_vec();
        oversized.extend(vec![b'a'; MAX_HEADERS]);
        oversized.extend_from_slice(b"\r\n\r\n");
        assert!(http(panel, &oversized).await.starts_with("HTTP/1.1 413"));
    }
    #[tokio::test]
    async fn http_partial_request_times_out() {
        assert!(http(test_panel("timeout", false), b"GET / HTTP/1.1\r\n")
            .await
            .starts_with("HTTP/1.1 408"));
    }
    #[tokio::test]
    async fn live_needs_phrase_and_cli_authorization() {
        for allow in [false, true] {
            let panel = test_panel("live", allow);
            let mut req = Request {
                method: "POST".into(),
                path: "/api/panel/live".into(),
                auth: Some("Bearer test-bearer-token".into()),
                body: serde_json::to_vec(&json!({"enabled":true,"confirmation":"yes"})).unwrap(),
            };
            assert_eq!(panel.route(&req).await.0, 400);
            assert!(!panel.runtime.live_enabled());
            req.body =
                serde_json::to_vec(&json!({"enabled":true,"confirmation":LIVE_CONFIRMATION}))
                    .unwrap();
            assert_eq!(panel.route(&req).await.0, if allow { 200 } else { 403 });
            assert_eq!(panel.runtime.live_enabled(), allow);
            req.body = serde_json::to_vec(&json!({"enabled":false})).unwrap();
            assert_eq!(panel.route(&req).await.0, 200);
            assert!(!panel.runtime.live_enabled());
        }
    }
    #[tokio::test]
    async fn dataset_reads_real_values_and_filters_network() {
        let panel = test_panel("data", false);
        tokio::fs::create_dir_all(&panel.data_dir).await.unwrap();
        let path = panel.data_dir.join("trades.json");
        tokio::fs::write(
            &path,
            serde_json::to_vec(
                &json!([{"network":"robinhood","asset":"real-asset"},{"asset":"solana-asset"}]),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            dataset(&path, "robinhood").await["items"][0]["asset"],
            "real-asset"
        );
        assert_eq!(
            dataset(&path, "solana").await["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            dataset(&path, "elysium").await["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }
    #[test]
    fn generated_token_is_persistent_and_private() {
        if std::env::var_os("PANEL_AUTH_TOKEN").is_some() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("girasol-panel-token-{}", rand::random::<u64>()));
        let token = local_token(&dir).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(local_token(&dir).unwrap(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("panel-auth-token"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!render_index().contains(&token));
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn dashboard_contains_networks_and_no_external_assets() {
        let html = render_index();
        for value in [
            "solana",
            "robinhood",
            "elysium",
            "Positions",
            "Telegram",
            "Bearer ",
            "Underworld",
        ] {
            assert!(html.contains(value), "missing {}", value);
        }
        assert!(!html.contains("<script src="));
    }

    #[test]
    fn test_find_subsequence() {
        assert_eq!(
            find_subsequence(b"hello\r\n\r\nworld", b"\r\n\r\n"),
            Some(5)
        );
        assert_eq!(find_subsequence(b"abc", b"zz"), None);
    }

    #[tokio::test]
    async fn test_route_unauthorized() {
        let dir = std::env::temp_dir().join(format!("girasol-panel-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let panel = KeyPanel {
            vault: Arc::new(ApiKeyVault::new(dir)),
            auth_token: "secret-token-1".into(),
            data_dir: std::env::temp_dir(),
            runtime: Arc::new(crate::runtime::RuntimeState::new(false)),
        };
        let req = Request {
            method: "GET".into(),
            path: "/api/panel/keys".into(),
            auth: None,
            body: vec![],
        };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 401);
        let req = Request {
            method: "GET".into(),
            path: "/api/panel/keys".into(),
            auth: Some("Bearer wrong".into()),
            body: vec![],
        };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 401);
        let req = Request {
            method: "GET".into(),
            path: "/api/panel/keys".into(),
            auth: Some("Bearer secret-token-1".into()),
            body: vec![],
        };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 200);
        let req = Request {
            method: "GET".into(),
            path: "/".into(),
            auth: None,
            body: vec![],
        };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 200);
    }
}

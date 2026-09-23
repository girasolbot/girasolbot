//! API-key management panel — minimal HTTP server (no framework) exposing the
//! encrypted vault behind existing-style bearer auth.
//!
//! Endpoints (all require `Authorization: Bearer <PANEL_AUTH_TOKEN>`):
//!   GET  /api/panel/keys            → masked status for every slot
//!   POST /api/panel/keys/{slot}     → body: {"value": "<key>"} (empty deletes)
//!   DELETE /api/panel/keys/{slot}   → delete stored key
//!   GET  /api/panel/health          → vault file health
//!   GET  /                          → minimal web UI (single page)
//!
//! Security notes:
//!   - Bind address defaults to 127.0.0.1 (loopback only). Use the
//!     `SNIPER_PANEL_BIND` env var to change (e.g. a tailscale/wg address).
//!   - PANEL_AUTH_TOKEN env var must be set or the panel refuses to start.
//!   - Keys are never returned in plaintext over the API — masked only.
//!   - tips/free-friendly providers are flagged in the UI and sorted first.

use crate::api_key_vault::{ApiKeyVault, API_KEY_SLOTS};
use std::sync::Arc;

/// Panel server. Owns the vault and serves the REST API + minimal UI.
pub struct KeyPanel {
    vault: Arc<ApiKeyVault>,
    auth_token: String,
}

/// Start the panel on the given addr. Returns a JoinHandle for the listener.
pub async fn start_panel(bind_addr: &str, data_dir: std::path::PathBuf) -> Result<(), String> {
    let auth_token = match std::env::var("PANEL_AUTH_TOKEN") {
        Ok(t) if t.trim().len() >= 8 => t.trim().to_string(),
        Ok(_) => {
            return Err("PANEL_AUTH_TOKEN is set but too short (min 8 chars)".to_string());
        }
        Err(_) => {
            log::info!("key panel: PANEL_AUTH_TOKEN not set — panel disabled");
            return Ok(());
        }
    };

    let vault = Arc::new(ApiKeyVault::new(data_dir));
    let panel = Arc::new(KeyPanel { vault, auth_token });
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|e| format!("panel bind {} failed: {}", bind_addr, e))?;
    log::info!("Key panel listening on http://{} (auth: bearer token)", bind_addr);

    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let panel = Arc::clone(&panel);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, panel).await;
                });
            }
            Err(e) => {
                log::warn!("panel accept error: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: Vec<u8>,
}

/// Read one HTTP request (headers + body). Supports Content-Length only.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Request> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    // Read until \r\n\r\n (end of headers)
    let header_end;
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            header_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            return None; // header too large
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let raw_path = parts.next()?.to_string();
    let path = raw_path.split('?').next().unwrap_or("").to_string();
    let mut auth = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "authorization" {
                auth = Some(v);
            } else if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
        }
    }
    if content_length > 256 * 1024 {
        return None; // body too large
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Some(Request { method, path, auth, body })
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Write a minimal HTTP response and shutdown.
async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        status, reason, content_type, body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

fn json_response(status: u16, value: serde_json::Value) -> (u16, String, Vec<u8>) {
    (status, "application/json".to_string(), serde_json::to_vec(&value).unwrap_or_default())
}

async fn handle_connection(mut stream: tokio::net::TcpStream, panel: Arc<KeyPanel>) -> std::io::Result<()> {
    let req = match read_request(&mut stream).await {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, ctype, body) = panel.route(&req).await;
    write_response(&mut stream, status, &ctype, &body).await?;
    Ok(())
}

impl KeyPanel {
    fn authorize(&self, req: &Request) -> bool {
        match &req.auth {
            Some(a) => match a.strip_prefix("Bearer ") {
                Some(token) => constant_time_eq(token.trim(), &self.auth_token),
                None => false,
            },
            None => false,
        }
    }

    async fn route(&self, req: &Request) -> (u16, String, Vec<u8>) {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                (200, "text/html; charset=utf-8".to_string(), render_index().into_bytes())
            }
            ("GET", "/api/panel/keys") => {
                if !self.authorize(req) {
                    return json_response(401, serde_json::json!({"error": "unauthorized"}));
                }
                let statuses = self.vault.statuses().await;
                // tips-friendly first, then free, then freemium, then subscription
                let rank = |p: &str| match p {
                    "tips" => 0,
                    "free" => 1,
                    "freemium" => 2,
                    _ => 3,
                };
                let mut sorted = statuses;
                sorted.sort_by_key(|s| rank(&s.pricing));
                json_response(200, serde_json::json!({ "keys": sorted }))
            }
            ("GET", "/api/panel/health") => {
                if !self.authorize(req) {
                    return json_response(401, serde_json::json!({"error": "unauthorized"}));
                }
                json_response(200, self.vault.vault_health())
            }
            ("POST", p) if p.starts_with("/api/panel/keys/") => {
                if !self.authorize(req) {
                    return json_response(401, serde_json::json!({"error": "unauthorized"}));
                }
                let slot_id = p.trim_start_matches("/api/panel/keys/");
                let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&req.body);
                let value = match parsed {
                    Ok(v) => v.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    Err(_) => String::new(),
                };
                match self.vault.set_api_key(slot_id, &value).await {
                    Ok(()) => json_response(200, serde_json::json!({"ok": true, "slot": slot_id})),
                    Err(e) => json_response(400, serde_json::json!({"error": e})),
                }
            }
            ("DELETE", p) if p.starts_with("/api/panel/keys/") => {
                if !self.authorize(req) {
                    return json_response(401, serde_json::json!({"error": "unauthorized"}));
                }
                let slot_id = p.trim_start_matches("/api/panel/keys/");
                match self.vault.set_api_key(slot_id, "").await {
                    Ok(()) => json_response(200, serde_json::json!({"ok": true, "deleted": slot_id})),
                    Err(e) => json_response(400, serde_json::json!({"error": e})),
                }
            }
            ("GET", "/api/panel/slots") => {
                if !self.authorize(req) {
                    return json_response(401, serde_json::json!({"error": "unauthorized"}));
                }
                let slots: Vec<serde_json::Value> = API_KEY_SLOTS
                    .iter()
                    .map(|s| serde_json::json!({"id": s.id, "label": s.label, "pricing": s.pricing.as_str()}))
                    .collect();
                json_response(200, serde_json::json!({ "slots": slots }))
            }
            (m, _) if m == "POST" || m == "DELETE" => {
                json_response(404, serde_json::json!({"error": "not found"}))
            }
            (_, _) => json_response(404, serde_json::json!({"error": "not found"})),
        }
    }
}

/// Constant-time string comparison for bearer tokens.
fn constant_time_eq(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= ha[i] ^ hb[i];
    }
    diff == 0
}

/// Minimal single-page web UI. Vanilla JS, no external assets.
fn render_index() -> String {
    let rows = API_KEY_SLOTS
        .iter()
        .map(|s| {
            let tips_flag = if s.pricing.as_str() == "tips" {
                r#"<span class="flag tips">tips-friendly</span>"#.to_string()
            } else if s.pricing.as_str() == "free" {
                r#"<span class="flag free">free</span>"#.to_string()
            } else {
                String::new()
            };
            format!(
                r#"<tr data-slot="{id}" data-pricing="{pricing}"><td><b>{label}</b> {flag}<div class="purpose">{purpose}</div><div class="meta">env: {env} · <a href="{signup}" target="_blank" rel="noopener">get key</a></div></td>
<td><div class="masked" id="masked-{id}">—</div></td>
<td><input type="password" id="input-{id}" placeholder="{placeholder}" autocomplete="off"><button onclick="saveKey('{id}')">Save</button><button class="del" onclick="deleteKey('{id}')">Delete</button></td></tr>"#,
                id = s.id, label = s.label, pricing = s.pricing.as_str(), flag = tips_flag,
                purpose = s.purpose, env = s.env_var, signup = s.signup_url, placeholder = s.placeholder,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Girasol API-key panel</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 1080px; margin: 2rem auto; padding: 0 1rem; color: #1a1a2e; }}
h1 {{ font-size: 1.4rem; }} h1 span {{ color: #e6a817; }}
table {{ border-collapse: collapse; width: 100%; }}
td {{ border-bottom: 1px solid #e5e5ea; padding: .7rem .5rem; vertical-align: top; }}
.purpose {{ color: #666; font-size: .85rem; margin-top: .25rem; }}
.meta {{ color: #999; font-size: .75rem; margin-top: .25rem; }}
.masked {{ font-family: monospace; }}
input {{ width: 240px; padding: .3rem; }}
button {{ margin-left: .4rem; padding: .3rem .7rem; cursor: pointer; }}
button.del {{ color: #b00020; }}
.flag {{ font-size: .7rem; padding: .1rem .4rem; border-radius: 3px; vertical-align: middle; }}
.flag.tips {{ background: #fff3d6; color: #8a6100; }}
.flag.free {{ background: #e2f5e9; color: #1c6b38; }}
#auth {{ margin: 1rem 0; }}
#status {{ color: #666; font-size: .85rem; margin-left: 1rem; }}
</style></head><body>
<h1>🌻 Girasol <span>API-key panel</span></h1>
<p>Keys are stored encrypted (AES-256-GCM) in <code>data/apikeys.enc.json</code> and are never returned in plaintext. tips/free-friendly providers are listed first.</p>
<div id="auth"><label>Panel token: <input type="password" id="token" autocomplete="off" style="width:280px"></label><button onclick="loadKeys()">Load keys</button><span id="status"></span></div>
<table id="tbl"><thead><tr><th>Provider</th><th>Stored</th><th>Update</th></tr></thead><tbody>{rows}</tbody></table>
<script>
function headers() {{ return {{ 'Authorization': 'Bearer ' + document.getElementById('token').value, 'Content-Type': 'application/json' }}; }}
async function loadKeys() {{
  const r = await fetch('/api/panel/keys', {{ headers: headers() }});
  const el = document.getElementById('status');
  if (r.status === 401) {{ el.textContent = 'unauthorized'; return; }}
  const data = await r.json();
  for (const k of data.keys) {{
    const m = document.getElementById('masked-' + k.id);
    if (m) m.textContent = k.configured ? (k.masked + ' (' + k.source + ')') : '—';
  }}
  el.textContent = 'loaded';
}}
async function saveKey(id) {{
  const value = document.getElementById('input-' + id).value;
  const r = await fetch('/api/panel/keys/' + id, {{ method: 'POST', headers: headers(), body: JSON.stringify({{ value }}) }});
  document.getElementById('status').textContent = r.ok ? 'saved ' + id : 'error ' + r.status;
  loadKeys();
}}
async function deleteKey(id) {{
  const r = await fetch('/api/panel/keys/' + id, {{ method: 'DELETE', headers: headers() }});
  document.getElementById('status').textContent = r.ok ? 'deleted ' + id : 'error ' + r.status;
  loadKeys();
}}
</script></body></html>"#,
        rows = rows
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn test_render_index_contains_slots() {
        let html = render_index();
        for s in API_KEY_SLOTS {
            assert!(html.contains(s.id), "index missing slot {}", s.id);
            assert!(html.contains(s.label));
        }
        assert!(html.contains("tips-friendly"));
        assert!(html.contains("Bearer "));
    }

    #[test]
    fn test_find_subsequence() {
        assert_eq!(find_subsequence(b"hello\r\n\r\nworld", b"\r\n\r\n"), Some(5));
        assert_eq!(find_subsequence(b"abc", b"zz"), None);
    }

    #[tokio::test]
    async fn test_route_unauthorized() {
        let dir = std::env::temp_dir().join(format!("girasol-panel-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let panel = KeyPanel { vault: Arc::new(ApiKeyVault::new(dir)), auth_token: "secret-token-1".into() };
        let req = Request { method: "GET".into(), path: "/api/panel/keys".into(), auth: None, body: vec![] };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 401);
        let req = Request { method: "GET".into(), path: "/api/panel/keys".into(), auth: Some("Bearer wrong".into()), body: vec![] };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 401);
        let req = Request { method: "GET".into(), path: "/api/panel/keys".into(), auth: Some("Bearer secret-token-1".into()), body: vec![] };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 200);
        let req = Request { method: "GET".into(), path: "/".into(), auth: None, body: vec![] };
        let (status, _, _) = panel.route(&req).await;
        assert_eq!(status, 200);
    }
}
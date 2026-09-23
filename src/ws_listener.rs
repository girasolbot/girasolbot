use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{info, warn, error, debug};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// Subscribe to all transactions involving the target wallet via WebSocket
/// and forward raw messages to the copy engine
pub async fn run_ws_listener(
    wss_url: &str,
    target_wallet: &str,
    tx: mpsc::Sender<String>,
) -> Result<()> {
    loop {
        info!("Connecting to WSS: {}", wss_url);
        match connect_and_subscribe(wss_url, target_wallet, &tx).await {
            Ok(()) => {
                warn!("WebSocket connection closed cleanly, reconnecting in 5s...");
            }
            Err(e) => {
                error!("WebSocket error: {:?}, reconnecting in 5s...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_subscribe(
    wss_url: &str,
    target_wallet: &str,
    tx: &mpsc::Sender<String>,
) -> Result<()> {
    // Fix: ensure path separator "/" exists before query string.
    // url::Url::parse normalizes "wss://host/?key=val" correctly,
    // but tungstenite's IntoClientRequest for &str can lose the "/"
    // when host has no explicit path. We parse and rebuild to guarantee it.
    let mut parsed_url = Url::parse(wss_url)
        .context("Invalid WSS URL")?;
    if parsed_url.path().is_empty() {
        parsed_url.set_path("/");
    }
    let fixed_url = parsed_url.as_str();
    debug!("WSS URL after fix: {}", fixed_url);

    let (ws_stream, _response) = connect_async(fixed_url)
        .await
        .context(format!("Failed to connect to WebSocket: {}", fixed_url))?;

    info!("Connected to WSS: {}", fixed_url);
    let (mut write, mut read) = ws_stream.split();

    // Subscribe to logs mentioning the target wallet
    // This catches all transactions where the target wallet is involved
    let subscribe_msg = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "logsSubscribe",
        "params": [
            {
                "mentions": [target_wallet]
            },
            {
                "commitment": "confirmed"
            }
        ]
    });

    write
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .context("Failed to send subscribe message")?;

    info!("Subscribed to logsSubscribe for target wallet: {}", target_wallet);

    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // consume immediate first tick
    let mut waiting_for_pong = false;
    let mut pong_deadline: Option<tokio::time::Instant> = None;

    loop {
        // Build an optional pong timeout future
        let pong_sleep = match pong_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline),
            None => tokio::time::sleep(Duration::from_secs(86400)), // effectively never
        };

        tokio::select! {
            // Branch 1: incoming WebSocket message
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Subscription confirmation
                        if text.contains("\"result\"") && !text.contains("\"params\"") {
                            debug!("Subscription confirmed: {}", &text[..text.len().min(100)]);
                            continue;
                        }

                        // Extract signature for logging before forwarding
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(sig) = val
                                .get("params")
                                .and_then(|p| p.get("result"))
                                .and_then(|r| r.get("value"))
                                .and_then(|v| v.get("signature"))
                                .and_then(|s| s.as_str())
                            {
                                let sig_short = &sig[..sig.len().min(16)];
                                let has_err = val
                                    .get("params")
                                    .and_then(|p| p.get("result"))
                                    .and_then(|r| r.get("value"))
                                    .and_then(|v| v.get("err"))
                                    .map_or(false, |e| !e.is_null());
                                if has_err {
                                    debug!("WS: target tx {}... (errored, forwarding)", sig_short);
                                } else {
                                    info!("WS: target tx {}... (forwarding to copy engine)", sig_short);
                                }
                            }
                        }

                        // Forward raw message to copy engine
                        if let Err(e) = tx.send(text).await {
                            error!("Failed to forward message to copy engine: {}", e);
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        debug!("Pong received");
                        waiting_for_pong = false;
                        pong_deadline = None;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if let Err(e) = write.send(Message::Pong(data)).await {
                            warn!("Failed to send pong: {}", e);
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("WebSocket closed by server");
                        break;
                    }
                    Some(Err(e)) => {
                        error!("WebSocket read error: {}", e);
                        break;
                    }
                    None => {
                        warn!("WebSocket stream ended");
                        break;
                    }
                    _ => {}
                }
            }

            // Branch 2: time to send a ping
            _ = ping_interval.tick() => {
                if waiting_for_pong {
                    // Previous ping still unanswered — don't stack pings
                    debug!("Skipping ping, still waiting for pong");
                    continue;
                }
                debug!("Sending WebSocket ping");
                if let Err(e) = write.send(Message::Ping(vec![])).await {
                    error!("Failed to send ping: {}", e);
                    break;
                }
                waiting_for_pong = true;
                pong_deadline = Some(tokio::time::Instant::now() + PONG_TIMEOUT);
            }

            // Branch 3: pong timeout expired
            _ = pong_sleep, if waiting_for_pong => {
                error!("Pong timeout ({}s) — forcing reconnect", PONG_TIMEOUT.as_secs());
                break;
            }
        }
    }

    Ok(())
}

//! Allowlisted Telegram research assistant. Deliberately has no live execution API.
use crate::{api_key_vault::ApiKeyVault, intelligence};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
fn allowed(ids: &str, id: i64) -> bool {
    ids.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|s| s.parse::<i64>().ok())
        .any(|i| i == id)
}
fn permitted(text: &str) -> bool {
    matches!(
        text.split_whitespace()
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or(""),
        "/start"
            | "/help"
            | "/status"
            | "/profile"
            | "/strategy"
            | "/paper"
            | "/underworld"
            | "/ask"
    )
}
async fn command(
    text: &str,
    vault: &ApiKeyVault,
    data_dir: &std::path::Path,
    status: &(dyn Fn() -> Value + Send + Sync),
) -> Result<Value, String> {
    if text.starts_with('/') && !permitted(text) {
        return Err("Supported: /status, /profile NETWORK ADDRESS [DAYS], /strategy NETWORK ADDRESS, /paper NETWORK ADDRESS, /underworld NETWORK ADDRESS, /ask MESSAGE. Live activation is available only in the local panel.".into());
    }
    let mut words = text.split_whitespace();
    let action = words.next().unwrap_or("").split('@').next().unwrap_or("");
    match action {
        "/start" | "/help" => Ok(
            json!({"help":"/status; /profile NETWORK ADDRESS [DAYS]; /strategy NETWORK ADDRESS; /paper NETWORK ADDRESS; /underworld NETWORK ADDRESS; /ask MESSAGE. NETWORK: solana, robinhood, elysium. Research and paper trading only."}),
        ),
        "/status" => Ok(status()),
        "/profile" | "/strategy" | "/paper" | "/underworld" => {
            let network = words.next().ok_or("network required")?;
            let address = words.next().ok_or("wallet address required")?;
            let days = words
                .next()
                .map(str::parse::<u64>)
                .transpose()
                .map_err(|_| "invalid day count")?
                .unwrap_or(7);
            intelligence::panel_api(
                &format!("/api/intelligence/{}", action.trim_start_matches('/')),
                json!({"address":address,"days":days}),
                network,
                data_dir,
                vault,
            )
            .await
        }
        _ => {
            let message = if action == "/ask" {
                text.split_once(char::is_whitespace)
                    .map(|(_, s)| s)
                    .unwrap_or("")
            } else {
                text
            };
            let mut provider = "local".to_owned();
            for name in ["gemini", "deepseek", "openai", "anthropic", "glm", "qwen"] {
                if vault.get_api_key(&format!("llm-{name}")).await.is_some() {
                    provider = name.to_owned();
                    break;
                }
            }
            intelligence::panel_api(
                "/api/ai/chat",
                json!({"message":message,"provider":provider}),
                "solana",
                data_dir,
                vault,
            )
            .await
        }
    }
}
async fn api(
    http: &reqwest::Client,
    token: &str,
    method: &str,
    body: Value,
) -> Result<Value, String> {
    // Errors intentionally omit request URLs: Telegram embeds the bot token in the path.
    let response: Value = http
        .post(format!("https://api.telegram.org/bot{token}/{method}"))
        .json(&body)
        .send()
        .await
        .map_err(|_| "Telegram transport failed")?
        .error_for_status()
        .map_err(|_| "Telegram HTTP error")?
        .json()
        .await
        .map_err(|_| "invalid Telegram JSON")?;
    if response["ok"] != true {
        return Err("Telegram rejected API request".into());
    }
    Ok(response["result"].clone())
}
pub fn spawn(
    vault: Arc<ApiKeyVault>,
    data_dir: PathBuf,
    status: Arc<dyn Fn() -> Value + Send + Sync>,
) {
    tokio::spawn(async move {
        let http = match reqwest::Client::builder()
            .timeout(Duration::from_secs(40))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut offset = 0i64;
        let mut active_token = String::new();
        loop {
            let token = vault
                .get_api_key("telegram-bot-token")
                .await
                .unwrap_or_default();
            let ids = vault
                .get_api_key("telegram-chat-id")
                .await
                .unwrap_or_default();
            if token.is_empty() || ids.trim().is_empty() {
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
            if token != active_token {
                match api(
                    &http,
                    &token,
                    "getUpdates",
                    json!({"offset":-1,"timeout":0,"allowed_updates":["message"]}),
                )
                .await
                {
                    Ok(updates) => {
                        offset = updates
                            .as_array()
                            .and_then(|a| a.last())
                            .and_then(|u| u["update_id"].as_i64())
                            .map(|n| n + 1)
                            .unwrap_or(0);
                        active_token = token.clone();
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_secs(15)).await;
                        continue;
                    }
                }
            }
            let updates = match api(
                &http,
                &token,
                "getUpdates",
                json!({"offset":offset,"timeout":25,"allowed_updates":["message"]}),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            if let Some(list) = updates.as_array() {
                for update in list {
                    if let Some(id) = update["update_id"].as_i64() {
                        offset = offset.max(id + 1)
                    }
                    let message = &update["message"];
                    let Some(chat_id) = message["chat"]["id"].as_i64() else {
                        continue;
                    };
                    if !allowed(&ids, chat_id) {
                        continue;
                    }
                    let Some(text) = message["text"].as_str() else {
                        continue;
                    };
                    let answer = match command(text, &vault, &data_dir, status.as_ref()).await {
                        Ok(v) => v["answer"].as_str().map(str::to_owned).unwrap_or_else(|| {
                            serde_json::to_string_pretty(&v).unwrap_or_default()
                        }),
                        Err(e) => e,
                    };
                    let chars: Vec<char> = answer.chars().take(14000).collect();
                    for chunk in chars.chunks(3500) {
                        let text: String = chunk.iter().collect();
                        let _=api(&http,&token,"sendMessage",json!({"chat_id":chat_id,"text":text,"link_preview_options":{"is_disabled":true}})).await;
                    }
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlist_requires_exact_chat_id() {
        assert!(allowed("123, -456", -456));
        assert!(!allowed("123", 12));
        assert!(!allowed("", 0));
    }
    #[test]
    fn commands_do_not_allow_live_activation() {
        assert!(permitted("/status"));
        assert!(permitted("/profile solana wallet 7"));
        assert!(!permitted("/live"));
        assert!(!permitted("/buy"));
    }
}

//! Minimal Telegram Bot API client: just sendMessage with retry on 429.
use anyhow::{anyhow, Result};
use serde_json::json;

pub struct Client {
    http: reqwest::Client,
    token: String,
}

impl Client {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            http: crate::util::http()
                .timeout(std::time::Duration::from_secs(60))
                .build()?,
            token: crate::config::req("BOT_TOKEN")?,
        })
    }

    pub async fn send_html(&self, chat_id: i64, thread_id: Option<i64>, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        if let Some(t) = thread_id {
            body["message_thread_id"] = json!(t);
        }

        for attempt in 0..3 {
            let res = match self.http.post(&url).json(&body).send().await {
                Ok(res) => res,
                // DNS or TLS not ready yet — a second attempt usually works, and the caller
                // (connection watchdog) still gets the final error if it does not.
                Err(e) if attempt < 2 => {
                    crate::logx::log_to("telegram", &format!("send attempt {} failed, retrying: {e:#}", attempt + 1));
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("Telegram unreachable after 3 attempts: {e:#}")),
            };
            let status = res.status();
            if status.is_success() {
                return Ok(());
            }
            let payload: serde_json::Value = match res.json().await {
                Ok(v) => v,
                Err(e) => return Err(anyhow!("Telegram API error: {status} - body unreadable: {e:#}")),
            };
            let retry_after = payload["parameters"]["retry_after"].as_u64().unwrap_or(5);
            if status.as_u16() == 429 && attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }
            return Err(anyhow!("Telegram API error: {status} - {payload}"));
        }
        Err(anyhow!("Telegram rate limited after 3 attempts"))
    }
}

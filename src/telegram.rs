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
            let res = self.http.post(&url).json(&body).send().await?;
            let status = res.status();
            let payload: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
            if status.is_success() {
                return Ok(());
            }
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

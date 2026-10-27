use serde_json::Value;

/// Thin wrapper: raw Bot API calls over reqwest instead of a framework,
/// so behaviour matches the original Telegraf bot 1:1 (same JSON, same retries).
#[derive(Clone)]
pub struct Tg {
    http: reqwest::Client,
    token: String,
}

pub const DEFAULT_RETRIES: usize = 2;
pub const DEFAULT_RETRY_DELAY_MS: u64 = 2000;

impl Tg {
    pub fn new(token: impl Into<String>, http: reqwest::Client) -> Self {
        Self { http, token: token.into() }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{method}", self.token)
    }

    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let res = self
            .http
            .post(self.url(method))
            .json(&params)
            .timeout(std::time::Duration::from_secs(45))
            .send()
            .await?;
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if body["ok"].as_bool() == Some(true) {
            Ok(body["result"].clone())
        } else {
            anyhow::bail!(
                "telegram {method}: {}",
                body["description"].as_str().unwrap_or("unknown error")
            )
        }
    }

    async fn call_multipart(&self, method: &str, form: reqwest::multipart::Form) -> anyhow::Result<Value> {
        let res = self
            .http
            .post(self.url(method))
            .multipart(form)
            .timeout(std::time::Duration::from_secs(300))
            .send()
            .await?;
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if body["ok"].as_bool() == Some(true) {
            Ok(body["result"].clone())
        } else {
            anyhow::bail!(
                "telegram {method}: {}",
                body["description"].as_str().unwrap_or("unknown error")
            )
        }
    }

    pub async fn send_html(&self, chat: i64, thread: Option<i64>, text: &str) -> anyhow::Result<i64> {
        self.send_message(chat, thread, text, Some("HTML"), None, None).await
    }

    pub async fn send_message(
        &self,
        chat: i64,
        thread: Option<i64>,
        text: &str,
        parse_mode: Option<&str>,
        keyboard: Option<Value>,
        reply_to: Option<i64>,
    ) -> anyhow::Result<i64> {
        for attempt in 0..=DEFAULT_RETRIES {
            let mut p = serde_json::json!({ "chat_id": chat, "text": text });
            if let Some(m) = parse_mode {
                p["parse_mode"] = m.into();
            }
            if let Some(t) = thread {
                p["message_thread_id"] = t.into();
            }
            if let Some(k) = &keyboard {
                p["reply_markup"] = k.clone();
            }
            if let Some(r) = reply_to {
                p["reply_parameters"] = serde_json::json!({ "message_id": r, "allow_sending_without_reply": true });
            }
            match self.call("sendMessage", p).await {
                Ok(v) => return Ok(v["message_id"].as_i64().unwrap_or(0)),
                Err(e) => {
                    crate::util::log_to("telegram.log", &format!("safeSend error (chat {chat}): {e}"));
                    if attempt < DEFAULT_RETRIES {
                        tokio::time::sleep(std::time::Duration::from_millis(DEFAULT_RETRY_DELAY_MS)).await;
                    }
                }
            }
        }
        anyhow::bail!("sendMessage failed after retries")
    }

    pub async fn edit_html(&self, chat: i64, message_id: i64, text: &str, keyboard: Option<Value>) -> anyhow::Result<()> {
        let mut p = serde_json::json!({ "chat_id": chat, "message_id": message_id, "text": text, "parse_mode": "HTML" });
        if let Some(k) = keyboard {
            p["reply_markup"] = k;
        }
        match self.call("editMessageText", p).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if e.to_string().contains("message is not modified") {
                    Ok(())
                } else {
                    crate::util::log_to("telegram.log", &format!("safeEdit error (chat {chat}): {e}"));
                    Err(e)
                }
            }
        }
    }

    pub async fn chat_action(&self, chat: i64, thread: Option<i64>, action: &str) {
        let mut p = serde_json::json!({ "chat_id": chat, "action": action });
        if let Some(t) = thread {
            p["message_thread_id"] = t.into();
        }
        let _ = self.call("sendChatAction", p).await;
    }

    pub async fn answer_callback(&self, id: &str, text: &str, alert: bool) {
        let _ = self
            .call(
                "answerCallbackQuery",
                serde_json::json!({ "callback_query_id": id, "text": text, "show_alert": alert }),
            )
            .await;
    }

    pub async fn get_updates(&self, offset: i64, timeout: u32) -> anyhow::Result<Vec<Value>> {
        let res = self
            .http
            .post(self.url("getUpdates"))
            .json(&serde_json::json!({
                "offset": offset,
                "timeout": timeout,
                "allowed_updates": ["message", "callback_query"],
            }))
            .timeout(std::time::Duration::from_secs(timeout as u64 + 20))
            .send()
            .await?;
        let body: Value = res.json().await?;
        if body["ok"].as_bool() == Some(true) {
            Ok(body["result"].as_array().cloned().unwrap_or_default())
        } else {
            anyhow::bail!("getUpdates: {}", body["description"].as_str().unwrap_or("unknown error"))
        }
    }

    pub async fn set_my_commands(&self) -> anyhow::Result<()> {
        let commands = serde_json::json!([
            {"command": "p", "description": "Check product price"},
            {"command": "help", "description": "Show command list"},
            {"command": "speed", "description": "Run internet speed test"},
            {"command": "battery", "description": "Show battery status"},
            {"command": "usage", "description": "Show data usage"},
            {"command": "playback", "description": "Control media playback"},
            {"command": "approve", "description": "Approve request"},
            {"command": "tt", "description": "Download TikTok/Douyin video"},
            {"command": "music", "description": "Download music (server 1)"},
            {"command": "music2", "description": "Download music (server 2)"},
            {"command": "reboot", "description": "Reboot device"}
        ]);
        self.call("setMyCommands", serde_json::json!({ "commands": commands })).await?;
        Ok(())
    }

    pub async fn admins(&self, chat: i64) -> Vec<i64> {
        match self.call("getChatAdministrators", serde_json::json!({ "chat_id": chat })).await {
            Ok(v) => v
                .as_array()
                .map(|a| a.iter().filter_map(|x| x["user"]["id"].as_i64()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Mute a non-admin for `until_ts` (unix seconds).
    pub async fn mute_until(&self, chat: i64, user: i64, until_ts: i64) {
        let p = serde_json::json!({
            "chat_id": chat,
            "user_id": user,
            "permissions": {"can_send_messages": false},
            "until_date": until_ts,
        });
        let _ = self.call("restrictChatMember", p).await;
    }

    pub async fn download_file(&self, file_id: &str) -> anyhow::Result<Vec<u8>> {
        let info = self.call("getFile", serde_json::json!({ "file_id": file_id })).await?;
        let path = info["file_path"].as_str().unwrap_or_default();
        let url = format!("https://api.telegram.org/file/bot{}/{path}", self.token);
        let res = self.http.get(url).timeout(std::time::Duration::from_secs(120)).send().await?;
        Ok(res.bytes().await?.to_vec())
    }

    /// Upload a local audio file with an optional local thumbnail.
    pub async fn upload_audio(
        &self,
        chat: i64,
        thread: Option<i64>,
        path: &std::path::Path,
        title: &str,
        performer: &str,
        duration: Option<f64>,
        thumb: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        let bytes = tokio::fs::read(path).await?;
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat.to_string())
            .text("title", title.to_string())
            .text("performer", performer.to_string())
            .part(
                "audio",
                reqwest::multipart::Part::bytes(bytes).file_name(name),
            );
        if let Some(t) = thread {
            form = form.text("message_thread_id", t.to_string());
        }
        if let Some(d) = duration {
            form = form.text("duration", format!("{}", d.round() as i64));
        }
        if let Some(tp) = thumb {
            if let Ok(tb) = tokio::fs::read(tp).await {
                form = form.part(
                    "thumbnail",
                    reqwest::multipart::Part::bytes(tb).file_name("thumb.jpg"),
                );
            }
        }
        self.call_multipart("sendAudio", form).await?;
        Ok(())
    }

    pub async fn upload_video(&self, chat: i64, thread: Option<i64>, path: &std::path::Path, caption: &str) -> anyhow::Result<()> {
        let bytes = tokio::fs::read(path).await?;
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat.to_string())
            .text("caption", caption.to_string())
            .text("parse_mode", "HTML")
            .part("video", reqwest::multipart::Part::bytes(bytes).file_name(name));
        if let Some(t) = thread {
            form = form.text("message_thread_id", t.to_string());
        }
        self.call_multipart("sendVideo", form).await?;
        Ok(())
    }

    pub async fn send_photo_bytes(&self, chat: i64, thread: Option<i64>, bytes: Vec<u8>, caption: &str) -> anyhow::Result<()> {
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat.to_string())
            .text("caption", caption.to_string())
            .text("parse_mode", "HTML")
            .part("photo", reqwest::multipart::Part::bytes(bytes).file_name("photo.jpg"));
        if let Some(t) = thread {
            form = form.text("message_thread_id", t.to_string());
        }
        self.call_multipart("sendPhoto", form).await?;
        Ok(())
    }

    /// Send several local photos as one album (attach:// refs).
    pub async fn send_media_group(&self, chat: i64, thread: Option<i64>, photos: Vec<Vec<u8>>, caption: &str) -> anyhow::Result<()> {
        let mut form = reqwest::multipart::Form::new().text("chat_id", chat.to_string());
        if let Some(t) = thread {
            form = form.text("message_thread_id", t.to_string());
        }
        let mut media = Vec::new();
        for (i, bytes) in photos.into_iter().enumerate() {
            let key = format!("file{i}");
            form = form.part(
                key.clone(),
                reqwest::multipart::Part::bytes(bytes).file_name(format!("{key}.jpg")),
            );
            let mut item = serde_json::json!({ "type": "photo", "media": format!("attach://{key}") });
            if i == 0 && !caption.is_empty() {
                item["caption"] = caption.into();
                item["parse_mode"] = "HTML".into();
            }
            media.push(item);
        }
        form = form.text("media", serde_json::to_string(&media)?);
        self.call_multipart("sendMediaGroup", form).await?;
        Ok(())
    }
}

/// Minimal message view used by every handler.
#[derive(Debug, Clone)]
pub struct Msg {
    pub chat_id: i64,
    pub thread: Option<i64>,
    pub id: i64,
    pub user_id: i64,
    pub username: String,
    pub text: String,
    pub caption: String,
    pub photo: Vec<Value>,
    pub document: Option<Value>,
    pub video: Option<Value>,
    pub raw: Value,
}

impl Msg {
    pub fn parse(update: &Value) -> Option<Self> {
        let m = update.get("message")?;
        if m.is_null() {
            return None;
        }
        Some(Self {
            chat_id: m["chat"]["id"].as_i64()?,
            thread: m["message_thread_id"].as_i64(),
            id: m["message_id"].as_i64()?,
            user_id: m["from"]["id"].as_i64().unwrap_or(0),
            username: m["from"]["username"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| m["from"]["id"].as_i64().unwrap_or(0).to_string()),
            text: m["text"].as_str().unwrap_or_default().to_string(),
            caption: m["caption"].as_str().unwrap_or_default().to_string(),
            photo: m["photo"].as_array().cloned().unwrap_or_default(),
            document: m.get("document").filter(|v| !v.is_null()).cloned(),
            video: m.get("video").filter(|v| !v.is_null()).cloned(),
            raw: m.clone(),
        })
    }

    /// Big text the bot logs for every update (mirrors the Telegraf middleware).
    pub fn log_summary(&self) -> String {
        if !self.text.is_empty() {
            self.text.clone()
        } else if !self.caption.is_empty() {
            let kind = if !self.photo.is_empty() {
                "photo"
            } else if self.video.is_some() {
                "video"
            } else {
                "media"
            };
            format!("[{kind}+caption] {}", self.caption)
        } else {
            let kind = if !self.photo.is_empty() {
                "photo"
            } else if self.video.is_some() {
                "video"
            } else if self.document.is_some() {
                "document"
            } else {
                "non-text"
            };
            format!("[{kind}]")
        }
    }
}

/// Minimal callback-query view.
#[derive(Debug, Clone)]
pub struct Cb {
    pub id: String,
    pub data: String,
    pub chat_id: i64,
    pub message_id: Option<i64>,
    pub thread: Option<i64>,
    pub user_id: i64,
}

impl Cb {
    pub fn parse(update: &Value) -> Option<Self> {
        let cq = update.get("callback_query")?;
        let msg = cq.get("message").cloned().unwrap_or(Value::Null);
        Some(Self {
            id: cq["id"].as_str()?.to_string(),
            data: cq["data"].as_str().unwrap_or_default().to_string(),
            chat_id: msg["chat"]["id"].as_i64().or_else(|| cq["message"]["chat"]["id"].as_i64())?,
            message_id: msg["message_id"].as_i64(),
            thread: msg["message_thread_id"].as_i64(),
            user_id: cq["from"]["id"].as_i64().unwrap_or(0),
        })
    }
}

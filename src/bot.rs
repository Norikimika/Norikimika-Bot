//! Telegram command bot (port of index.js).
//! Hand-rolled long polling on top of reqwest — no teloxide, same handlers and texts as the JS.
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use crate::util::{escape_html, rupiah};

const MOD: &str = "telegram";
const DEFAULT_RETRIES: usize = 2;
const DEFAULT_RETRY_DELAY_MS: u64 = 2000;
const ADMIN_CACHE_TTL_SECS: i64 = 5 * 60;
/// One command may not hold a handler slot forever (downloads legitimately take minutes).
const HANDLER_TIMEOUT: Duration = Duration::from_secs(300);
const SH_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_ATTEMPTS: u32 = 2;
const AUDIO_EXTS: &[&str] = &["webm", "m4a", "mp3", "opus", "ogg", "aac", "flac", "wav"];

fn dir() -> PathBuf {
    let d = crate::config::get("DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::state_dir().join("media"));
    let _ = std::fs::create_dir_all(&d);
    d
}

fn topic(key: &str) -> Option<i64> {
    crate::config::topic(key)
}

struct Msg {
    chat_id: i64,
    thread_id: Option<i64>,
    user_id: i64,
    message_id: i64,
    text: String,
    reply_text: String,
    photo_file_id: Option<String>,
}

struct Callback {
    chat_id: i64,
    message_id: i64,
    data: String,
    is_admin: bool,
}

/// One file upload: the arguments of `send_file`, which clippy rightly called too many.
struct Upload<'a> {
    kind: &'a str,
    chat_id: i64,
    thread: Option<i64>,
    field: &'a str,
    bytes: Vec<u8>,
    filename: &'a str,
    extra: Value,
    /// `thumbnail` part: sendAudio shows it as the cover, and without it Telegram falls back to a
    /// grey placeholder — the JS bot attached one, the port dropped it.
    thumb: Option<(Vec<u8>, String)>,
}

/// (chat_id, user_id) -> (is_admin, checked_at)
type AdminCache = std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<(i64, i64), (bool, i64)>>>;

#[derive(Clone)]
pub struct Bot {
    http: reqwest::Client,
    upload: reqwest::Client,
    token: String,
    topic_id: Option<i64>,
    admins: AdminCache,
}

impl Bot {
    fn new() -> Result<Self> {
        Ok(Self {
            http: crate::util::http()
                .timeout(Duration::from_secs(120))
                .build()?,
            // a 5 MB track on a mobile uplink took longer than the 120 s API timeout, so uploads
            // get their own client
            upload: crate::util::http()
                .timeout(Duration::from_secs(600))
                .build()?,
            token: crate::config::req("BOT_TOKEN")?,
            topic_id: topic("TOPIC_ID"),
            admins: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    async fn api(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("https://api.telegram.org/bot{}/{method}", self.token);
        let mut last = String::new();
        for attempt in 0..=DEFAULT_RETRIES {
            match self.http.post(&url).json(&body).send().await {
                Ok(res) => {
                    let status = res.status();
                    let payload: Value = res.json().await.unwrap_or(Value::Null);
                    if status.is_success() && payload["ok"].as_bool().unwrap_or(false) {
                        return Ok(payload["result"].clone());
                    }
                    last = payload.to_string();
                    if let Some(desc) = payload["description"].as_str() {
                        if desc.contains("message is not modified") {
                            return Ok(Value::Null);
                        }
                    }
                }
                Err(e) => last = e.to_string(),
            }
            if attempt < DEFAULT_RETRIES {
                tokio::time::sleep(Duration::from_millis(DEFAULT_RETRY_DELAY_MS)).await;
            }
        }
        Err(anyhow!("telegram {method} failed: {last}"))
    }

    async fn send(&self, chat_id: i64, thread: Option<i64>, html: &str, reply_to: Option<i64>, keyboard: Option<Value>) -> Result<i64> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": html,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        if let Some(t) = thread {
            body["message_thread_id"] = json!(t);
        }
        if let Some(r) = reply_to {
            body["reply_to_message_id"] = json!(r);
        }
        if let Some(k) = keyboard {
            body["reply_markup"] = json!({ "inline_keyboard": k });
        }
        let res = self.api("sendMessage", body).await?;
        Ok(res["message_id"].as_i64().unwrap_or(0))
    }

    async fn edit(&self, chat_id: i64, message_id: i64, html: &str, keyboard: Option<Value>) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": html,
            "parse_mode": "HTML",
        });
        if let Some(k) = keyboard {
            body["reply_markup"] = json!({ "inline_keyboard": k });
        }
        self.api("editMessageText", body).await.map(|_| ())
    }

    async fn answer_callback(&self, id: &str, text: &str, alert: bool) -> Result<()> {
        let body = json!({ "callback_query_id": id, "text": text, "show_alert": alert });
        match self.api("answerCallbackQuery", body).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // A button pressed after a restart, or one whose handler outlived Telegram's
                // ~15s window, can no longer be answered. Nothing is broken, so this must not be
                // reported as a handler failure — but it is still written down.
                if is_expired_query(&format!("{e:#}")) {
                    logln!(MOD, "[callback] answer skipped, query already expired");
                    return Ok(());
                }
                Err(e)
            }
        }
    }

    async fn is_admin(&self, chat_id: i64, user_id: i64) -> bool {
        // explicit operator allow-list wins: chat-admin lookup alone made the gate depend on
        // whoever happens to be admin in the group.
        if let Some(list) = crate::config::get("ADMIN_IDS") {
            if list
                .split(',')
                .filter_map(|id| id.trim().parse::<i64>().ok())
                .any(|id| id == user_id)
            {
                return true;
            }
        }
        let key = (chat_id, user_id);
        let now = chrono::Utc::now().timestamp();
        {
            let cache = self.admins.lock().await;
            if let Some((is_admin, ts)) = cache.get(&key) {
                if now - ts < ADMIN_CACHE_TTL_SECS {
                    return *is_admin;
                }
            }
        }
        let admins = self.api("getChatAdministrators", json!({ "chat_id": chat_id })).await;
        let is_admin = admins
            .map(|list| {
                list.as_array()
                    .map(|arr| arr.iter().any(|a| a["user"]["id"].as_i64() == Some(user_id)))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        self.admins.lock().await.insert(key, (is_admin, now));
        is_admin
    }

    /// Gate for admin-only commands: deny + reply when the sender is not an admin.
    ///
    /// The bot no longer calls restrictChatMember: muting people is not its job, and doing it
    /// from a command handler meant any stranger could make the bot punish group members.
    async fn require_admin(&self, msg: &Msg) -> bool {
        if self.is_admin(msg.chat_id, msg.user_id).await {
            return true;
        }
        if let Err(e) = self
            .send(
                msg.chat_id,
                msg.thread_id,
                "⛔ Not authorized.",
                Some(msg.message_id),
                None,
            )
            .await
        {
            logln!(MOD, "could not warn a non-admin: {e}");
        }
        false
    }

    async fn typing_loop(&self, chat_id: i64, thread: Option<i64>, action: &str) -> tokio::task::JoinHandle<()> {
        let token = self.token.clone();
        let http = self.http.clone();
        let action = action.to_string();
        tokio::spawn(async move {
            loop {
                let mut body = json!({ "chat_id": chat_id, "action": action });
                if let Some(t) = thread {
                    body["message_thread_id"] = json!(t);
                }
                // typing indicator only: logging every failure would drown the log
                let _ = http
                    .post(format!("https://api.telegram.org/bot{token}/sendChatAction"))
                    .json(&body)
                    .send()
                    .await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        })
    }

    async fn file_path(&self, file_id: &str) -> Result<String> {
        let res = self.api("getFile", json!({ "file_id": file_id })).await?;
        res["file_path"].as_str().map(str::to_string).ok_or_else(|| anyhow!("no file_path"))
    }

    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        let path = self.file_path(file_id).await?;
        let url = format!("https://api.telegram.org/file/bot{}/{}", self.token, path);
        Ok(self.http.get(url).send().await?.bytes().await?.to_vec())
    }

    /// Upload one file with `kind` = sendPhoto/sendVideo/sendAudio/sendDocument.
    ///
    /// Retried only when the request itself failed: a slow uplink that times out is worth another
    /// try, while a 4xx would fail the same way twice.
    async fn send_file(&self, up: Upload<'_>) -> Result<()> {
        let kind = up.kind;
        let url = format!("https://api.telegram.org/bot{}/{kind}", self.token);
        let mut last = String::new();
        for attempt in 1..=UPLOAD_ATTEMPTS {
            let form = upload_form(&up)?;
            match self.upload.post(&url).multipart(form).send().await {
                Ok(res) => {
                    if !res.status().is_success() {
                        let body = res.text().await.unwrap_or_default();
                        return Err(anyhow!(
                            "{kind} failed: {}",
                            crate::util::redact_token(&body)
                        ));
                    }
                    if kind == "sendAudio" {
                        // Telegram answers with what it actually stored. A cover that was sent but
                        // dropped leaves no trace in the message, so ask the response instead of
                        // the screenshot.
                        let body: Value = res.json().await.unwrap_or(Value::Null);
                        let stored = body["result"]["audio"]["thumb"].get("file_id").is_some();
                        let container = up
                            .filename
                            .rsplit('.')
                            .next()
                            .unwrap_or("?")
                            .to_lowercase();
                        logln!(
                            MOD,
                            "[sendAudio] telegram {} the cover ({container})",
                            if stored { "kept" } else { "dropped" }
                        );
                    }
                    return Ok(());
                }
                Err(e) => {
                    last = crate::util::redact_token(&format!("{e}"));
                    logln!(MOD, "[upload] {kind} attempt {attempt}/{UPLOAD_ATTEMPTS}: {last}");
                    if attempt < UPLOAD_ATTEMPTS {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
        Err(anyhow!(
            "{kind} upload failed after {UPLOAD_ATTEMPTS} attempts: {last}"
        ))
    }

    /// sendMediaGroup with local files (attach:// references).
    async fn send_media_group(&self, chat_id: i64, thread: Option<i64>, items: Vec<(String, Vec<u8>, String)>) -> Result<()> {
        let mut form = reqwest::multipart::Form::new().text("chat_id", chat_id.to_string());
        if let Some(t) = thread {
            form = form.text("message_thread_id", t.to_string());
        }
        let mut media = Vec::new();
        for (i, (kind, bytes, filename)) in items.into_iter().enumerate() {
            let field = format!("file{i}");
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name(filename)
                .mime_str("application/octet-stream")?;
            form = form.part(field.clone(), part);
            media.push(json!({ "type": kind, "media": format!("attach://{field}") }));
        }
        form = form.text("media", serde_json::to_string(&media)?);
        let url = format!("https://api.telegram.org/bot{}/sendMediaGroup", self.token);
        let res = self.http.post(&url).multipart(form).send().await?;
        if !res.status().is_success() {
            return Err(anyhow!("sendMediaGroup failed: {}", res.text().await.unwrap_or_default()));
        }
        Ok(())
    }
}

/// The multipart body for an upload. Rebuilt per attempt because a `Form` cannot be cloned.
fn upload_form(up: &Upload<'_>) -> Result<reqwest::multipart::Form> {
    let part = reqwest::multipart::Part::bytes(up.bytes.clone())
        .file_name(up.filename.to_string())
        .mime_str("application/octet-stream")?;
    let mut form = reqwest::multipart::Form::new()
        .text("chat_id", up.chat_id.to_string())
        .part(up.field.to_string(), part);
    if let Some((bytes, name)) = &up.thumb {
        let cover = reqwest::multipart::Part::bytes(bytes.clone())
            .file_name(name.clone())
            .mime_str("image/jpeg")?;
        form = form.part("thumbnail", cover);
    }
    if let Some(t) = up.thread {
        form = form.text("message_thread_id", t.to_string());
    }
    if let Some(obj) = up.extra.as_object() {
        for (k, v) in obj {
            // `duration` is a number; as_str() turned it into an empty string and Telegram got
            // `duration=`
            let text = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            form = form.text(k.clone(), text);
        }
    }
    Ok(form)
}

// ================== shared helpers ==================

/// Termux helpers can block forever (no TTS engine, waiting for the API app), and every call here
/// used to hold the whole polling loop hostage.
async fn sh(cmd: &str, args: &[&str]) -> (bool, String) {
    let fut = tokio::process::Command::new(cmd).args(args).kill_on_drop(true).output();
    match tokio::time::timeout(SH_TIMEOUT, fut).await {
        Ok(Ok(out)) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(e)) => (false, format!("{cmd}: {e}")),
        Err(_) => (false, format!("{cmd} timed out after {}s", SH_TIMEOUT.as_secs())),
    }
}

async fn sh_root(cmd: &str) -> (bool, String) {
    // `su` blocks forever when the root prompt is never answered, which used to hang the
    // whole command handler; time it out instead.
    let fut = tokio::process::Command::new(crate::util::su())
        .arg("-c")
        .arg(cmd)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(20), fut).await {
        Ok(Ok(out)) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(e)) => (false, format!("su: {e}")),
        Err(_) => (false, "su timed out: grant root access for Termux".into()),
    }
}

async fn sh_shell(cmd: &str) -> (bool, String) {
    let fut = tokio::process::Command::new("sh").arg("-c").arg(cmd).kill_on_drop(true).output();
    match tokio::time::timeout(SH_TIMEOUT, fut).await {
        Ok(Ok(out)) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(_)) => (false, String::new()),
        Err(_) => (false, format!("`{cmd}` timed out after {}s", SH_TIMEOUT.as_secs())),
    }
}

fn format_track_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "??:??".into();
    }
    format!("{}:{:02}", (seconds / 60.0).floor() as i64, (seconds % 60.0).floor() as i64)
}

/// Download one media part; on failure say why instead of dropping it silently.
async fn download_logged(url: &str, what: &str) -> Option<Vec<u8>> {
    match download(url).await {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            crate::util::log_once(MOD, what, &format!("[TT] {what} download failed: {e:#}"));
            None
        }
    }
}

fn find_downloaded_audio(file_id: &str) -> Option<PathBuf> {
    let dir = dir();
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(file_id) {
            return None;
        }
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
        AUDIO_EXTS.contains(&ext.as_str()).then(|| entry.path())
    })
}

fn active_iface() -> Option<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("ip route show default 2>/dev/null")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    out.split_whitespace().skip_while(|t| *t != "dev").nth(1).map(str::to_string)
}

fn format_kb(kb: f64) -> String {
    const M: f64 = 1024.0 * 1024.0;
    if kb < M {
        format!("{kb:.2} KB")
    } else if kb < M * 1024.0 {
        format!("{:.2} MB", kb / M)
    } else if kb < M * 1024.0 * 1024.0 {
        format!("{:.2} GB", kb / M / 1024.0)
    } else {
        format!("{:.2} TB", kb / M / 1024.0 / 1024.0)
    }
}

struct Traffic {
    up: String,
    down: String,
    total: String,
}

fn traffic_of(obj: Option<&Value>) -> Option<Traffic> {
    let obj = obj?;
    let up = obj["tx"].as_f64().unwrap_or(0.0);
    let down = obj["rx"].as_f64().unwrap_or(0.0);
    Some(Traffic {
        up: format_kb(up),
        down: format_kb(down),
        total: format_kb(up + down),
    })
}

// ================== receipt formatting ==================

fn truncate_middle(text: &str, max_len: usize) -> String {
    let s = if text.is_empty() { "-" } else { text };
    s.chars().take(max_len).collect()
}

fn pad_c(text: &str, width: usize) -> String {
    let s = truncate_middle(text, width);
    let len = s.chars().count();
    if len >= width {
        return s;
    }
    format!("{}{}", " ".repeat((width - len) / 2), s)
}

fn pad_r(label: &str, value: &str, width: usize) -> String {
    let used = label.chars().count() + value.chars().count();
    if used >= width {
        return format!("{label}{value}");
    }
    format!("{label}{}{value}", " ".repeat(width - used))
}

fn parse_idr(text: &str) -> i64 {
    text.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)
}

fn calc_admin(nominal: &str) -> (i64, String) {
    let num = parse_idr(nominal);
    if num <= 500_000 {
        return (5000, "5.000".into());
    }
    let steps = ((num - 500_000) as f64 / 1_000_000.0).ceil() as i64;
    let admin = 5000 + steps * 5000;
    (admin, rupiah(admin as f64))
}

fn round_token_nominal(nominal: &str) -> String {
    let num = parse_idr(nominal);
    if num <= 0 {
        return "0".into();
    }
    let remainder = num % 10_000;
    if remainder > 0 && remainder <= 5000 {
        return rupiah((num - remainder) as f64);
    }
    rupiah(num as f64)
}

fn adjust_to_wita(time_str: &str) -> String {
    if !time_str.contains(':') {
        return "-".into();
    }
    let parts: Vec<&str> = time_str.trim().split(':').collect();
    let Ok(hours) = parts[0].parse::<i64>() else {
        return time_str.to_string();
    };
    let minutes = parts.get(1).copied().unwrap_or("00");
    let seconds = parts.get(2).copied().unwrap_or("00");
    format!("{:02}:{minutes}:{seconds}", (hours + 1) % 24)
}

struct Struk {
    kind: String,
    tanggal: String,
    waktu: String,
    nominal: String,
    id_pel: String,
    no_meter: String,
    nama: String,
    tarif: String,
    jml_kwh: String,
    token: String,
    bank_tujuan: String,
    no_rekening: String,
    penerima: String,
    provider_tujuan: String,
    no_provider: String,
}

impl Struk {
    fn from_json(v: &Value) -> Self {
        let get = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Self {
            kind: get("type"),
            tanggal: get("tanggal"),
            waktu: get("waktu"),
            nominal: get("nominal"),
            id_pel: get("idPel"),
            no_meter: get("noMeter"),
            nama: get("nama"),
            tarif: get("tarif"),
            jml_kwh: get("jmlKwh"),
            token: get("token"),
            bank_tujuan: get("bankTujuan"),
            no_rekening: get("noRekening"),
            penerima: get("penerima"),
            provider_tujuan: get("providerTujuan"),
            no_provider: get("noProvider"),
        }
    }
}

fn build_struk(data: &Struk) -> String {
    const W: usize = 32;
    let eq = "=".repeat(W);
    let da = "-".repeat(W);
    let store_name = crate::config::get("STORE_NAME").unwrap_or_default();
    let store_address = crate::config::get("STORE_ADDRESS").unwrap_or_default();
    let tanggal = if data.tanggal.is_empty() { "-".to_string() } else { data.tanggal.clone() };
    let wita = adjust_to_wita(&data.waktu);

    let mut lines: Vec<String> = vec![
        pad_c(&store_name, W),
        pad_c(&store_address, W),
        eq.clone(),
    ];

    match data.kind.as_str() {
        "transfer" => {
            let (admin, admin_str) = calc_admin(&data.nominal);
            let total = rupiah((parse_idr(&data.nominal) + admin) as f64);
            lines.extend([
                pad_r("TANGGAL", &tanggal, W),
                pad_r("WAKTU", &wita, W),
                da.clone(),
                pad_c("DATA PENERIMA", W),
                format!("TUJUAN   : {}", truncate_middle(&data.bank_tujuan, 10)),
                format!("NOMOR    : {}", truncate_middle(&data.no_rekening, 15)),
                format!("PENERIMA : {}", truncate_middle(&data.penerima, 21)),
                da.clone(),
                pad_r("NOMINAL", &format!("Rp {}", data.nominal), W),
                pad_r("ADMIN", &format!("Rp {admin_str}"), W),
                eq.clone(),
                pad_r("TOTAL", &format!("Rp {total}"), W),
            ]);
        }
        "pulsa" => {
            let rounded = round_token_nominal(&data.nominal);
            let admin = 3000;
            let total = rupiah((parse_idr(&rounded) + admin) as f64);
            lines.extend([
                pad_r("TANGGAL", &tanggal, W),
                pad_r("WAKTU", &wita, W),
                da.clone(),
                pad_c("DATA PENERIMA", W),
                format!("TUJUAN   : {}", truncate_middle(&data.provider_tujuan, 18)),
                format!("NOMOR    : {}", truncate_middle(&data.no_provider, 18)),
                da.clone(),
                pad_r("NOMINAL", &format!("Rp {rounded}"), W),
                pad_r("ADMIN", "Rp 3.000", W),
                eq.clone(),
                pad_r("TOTAL", &format!("Rp {total}"), W),
            ]);
        }
        _ => {
            let rounded = round_token_nominal(&data.nominal);
            let (admin, admin_str) = calc_admin(&rounded);
            let total = rupiah((parse_idr(&rounded) + admin) as f64);
            let digits: String = data.token.chars().filter(|c| c.is_ascii_digit()).collect();
            let groups: Vec<String> = digits
                .chars()
                .collect::<Vec<_>>()
                .chunks(4)
                .map(|c| c.iter().collect())
                .collect();
            let tl1: String = groups.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
            let tl2: String = groups.iter().skip(3).cloned().collect::<Vec<_>>().join(" ");
            lines.extend([
                pad_r("TANGGAL", &tanggal, W),
                pad_r("WAKTU", &wita, W),
                da.clone(),
                format!("ID PEL     : {}", truncate_middle(&data.id_pel, 18)),
                format!("NO METER   : {}", truncate_middle(&data.no_meter, 18)),
                format!("NAMA       : {}", truncate_middle(&data.nama, 18)),
                format!("TARIF/DAYA : {}", truncate_middle(&data.tarif, 18)),
                format!("JML KWH    : {}", truncate_middle(&data.jml_kwh, 18)),
                da.clone(),
                pad_c("TOKEN", W),
                pad_c(&tl1, W),
            ]);
            if !tl2.is_empty() {
                lines.push(pad_c(&tl2, W));
            }
            lines.extend([
                da.clone(),
                pad_r("NOMINAL", &format!("Rp {rounded}"), W),
                pad_r("ADMIN", &format!("Rp {admin_str}"), W),
                eq.clone(),
                pad_r("TOTAL", &format!("Rp {total}"), W),
            ]);
        }
    }

    lines.push(eq);
    lines.push(String::new());
    lines.push(pad_c("** TRANSAKSI BERHASIL **", W));
    lines.push(String::new());
    lines.push(pad_c("TERIMA KASIH", W));
    lines.join("\n")
}

/// Render the text receipt to a PNG, matching the JS ImageMagick layout:
/// store name at `size+8`, address at `max(9, size-4)`, body at `size`, blocks centred,
/// canvas width driven by the widest body line, 25x20 white border.
fn render_struk_image(struk: &str) -> Result<Vec<u8>> {
    use crate::render::{Canvas, Face, Fonts};

    let normal = 20.0_f32;
    let small = (normal - 4.0).max(9.0);
    let large = normal + 8.0;

    let mut rows = struk.lines();
    let name = rows.next().unwrap_or("").trim().to_string();
    let addr = rows.next().unwrap_or("").trim().to_string();
    let body: Vec<&str> = rows.collect();

    let fonts = Fonts::load()?;
    let line_h = |size: f32| (size * 1.2).ceil();
    let body_w = body.iter().map(|l| fonts.measure(Face::Mono, normal, l)).fold(0.0_f32, f32::max);
    let name_w = fonts.measure(Face::Mono, large, &name);
    let inner_w = body_w.max(name_w + 20.0).max(200.0);

    const PAD_X: u32 = 25;
    const PAD_Y: u32 = 20;
    let width = inner_w.ceil() as u32 + PAD_X * 2;
    let name_h = line_h(large);
    let addr_h = line_h(small);
    let body_h = line_h(normal) * body.len() as f32;
    let height = PAD_Y * 2 + (name_h + addr_h + body_h) as u32;

    let blank = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 255, 255, 255]));
    let mut canvas = Canvas::from_image(blank, fonts);
    let black = [0u8, 0, 0, 255];
    let centred = |text_w: f32| ((width as f32 - text_w) / 2.0).max(PAD_X as f32) as i32;

    let mut y = PAD_Y as f32;
    canvas.text(centred(name_w), (y + large * 0.8) as i32, large, Face::Mono, black, &name);
    y += name_h;
    canvas.text(centred(canvas.fonts.measure(Face::Mono, small, &addr)), (y + small * 0.8) as i32, small, Face::Mono, black, &addr);
    y += addr_h;

    let body_x = (PAD_X as f32 + (inner_w - body_w) / 2.0).max(PAD_X as f32) as i32;
    for line in &body {
        canvas.text(body_x, (y + normal * 0.8) as i32, normal, Face::Mono, black, line);
        y += line_h(normal);
    }
    canvas.png()
}

// ================== command handlers ==================

async fn cmd_speed(bot: &Bot, msg: &Msg) -> Result<()> {
    let typing = bot.typing_loop(msg.chat_id, msg.thread_id, "typing").await;
    let (ok, out) = sh("speedtest-go", &["--json"]).await;
    typing.abort();
    if !ok {
        logln!(MOD, "Speedtest failed");
        bot.send(msg.chat_id, msg.thread_id, "❌ Failed to run speedtest", Some(msg.message_id), None).await?;
        return Ok(());
    }
    let result: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "Speedtest JSON parse failed: {e}");
            bot.send(msg.chat_id, msg.thread_id, "⚠️ Failed to parse speedtest result.", Some(msg.message_id), None).await?;
            return Ok(());
        }
    };
    let isp = result["user_info"]["Isp"].as_str().unwrap_or("Unknown");
    let server = result["servers"][0]["name"].as_str().unwrap_or("Unknown");
    let country = result["servers"][0]["country"].as_str().unwrap_or("-");
    let sponsor = result["servers"][0]["sponsor"].as_str().unwrap_or("Unknown");
    let latency = result["servers"][0]["latency"].as_f64().unwrap_or(0.0) / 1e6;
    let download = result["servers"][0]["dl_speed"].as_f64().unwrap_or(0.0) / 125000.0;
    let upload = result["servers"][0]["ul_speed"].as_f64().unwrap_or(0.0) / 125000.0;
    let summary = format!(
        "<b>⚡ Speed Test Result</b>\n<blockquote>ISP : {}\nServer : {}\nCountry : {} {}\nPing : {latency:.2} ms\nDownload : {download:.2} Mbps\nUpload : {upload:.2} Mbps</blockquote>",
        escape_html(isp),
        escape_html(sponsor),
        escape_html(server),
        escape_html(country)
    );
    bot.send(msg.chat_id, msg.thread_id, summary.trim(), Some(msg.message_id), None).await?;
    Ok(())
}

async fn cmd_reboot(bot: &Bot, msg: &Msg) -> Result<()> {
    bot.send(msg.chat_id, msg.thread_id, "♻️ Rebooting system...", Some(msg.message_id), None).await?;
    let (rebooted, out) = sh_root("reboot").await;
    if !rebooted {
        logln!(MOD, "Reboot failed: {out}");
        bot.send(msg.chat_id, msg.thread_id, "❌ Failed to reboot", Some(msg.message_id), None).await?;
    }
    Ok(())
}

async fn cmd_battery(bot: &Bot, msg: &Msg) -> Result<()> {
    let (ok, out) = sh("termux-battery-status", &[]).await;
    if !ok {
        logln!(MOD, "Battery failed");
        bot.send(msg.chat_id, msg.thread_id, "❌ Failed to get battery info", Some(msg.message_id), None).await?;
        return Ok(());
    }
    let data: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "Battery parse error: {e}");
            bot.send(msg.chat_id, msg.thread_id, "❌ Failed to parse battery info", Some(msg.message_id), None).await?;
            return Ok(());
        }
    };
    let text = format!(
        "<b>🔋 Battery Status</b>\n<pre>Percentage : {}%\nStatus : {}\nTemperature : {}°C</pre>",
        data["percentage"].as_i64().unwrap_or(0),
        data["status"].as_str().unwrap_or("UNKNOWN"),
        data["temperature"].as_f64().unwrap_or(0.0).round() as i64
    );
    bot.send(msg.chat_id, msg.thread_id, &text, Some(msg.message_id), None).await?;
    Ok(())
}

async fn cmd_usage(bot: &Bot, msg: &Msg) -> Result<()> {
    let typing = bot.typing_loop(msg.chat_id, msg.thread_id, "typing").await;
    let out = crate::util::capture("vnstat", &["--json"]).await;
    typing.abort();
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            logln!(MOD, "vnstat failed: {e}");
            bot.send(msg.chat_id, msg.thread_id, &crate::util::vnstat_hint(&e), Some(msg.message_id), None).await?;
            return Ok(());
        }
    };
    let data: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "vnstat parse failed: {e}");
            bot.send(msg.chat_id, msg.thread_id, "❌ Failed to parse usage", Some(msg.message_id), None).await?;
            return Ok(());
        }
    };
    let empty = Vec::new();
    let ifaces = data["interfaces"].as_array().unwrap_or(&empty);
    let active = active_iface();
    let chosen = ifaces
        .iter()
        .find(|i| active.as_deref() == i["name"].as_str())
        .or_else(|| {
            let mut others: Vec<&Value> = ifaces.iter().filter(|i| i["name"].as_str() != Some("lo")).collect();
            others.sort_by(|a, b| {
                let total = |i: &Value| i["traffic"]["total"]["rx"].as_f64().unwrap_or(0.0) + i["traffic"]["total"]["tx"].as_f64().unwrap_or(0.0);
                total(b).partial_cmp(&total(a)).unwrap_or(std::cmp::Ordering::Equal)
            });
            others.first().copied()
        })
        .or_else(|| ifaces.first());
    let Some(chosen) = chosen else {
        bot.send(msg.chat_id, msg.thread_id, "❌ Failed to parse usage", Some(msg.message_id), None).await?;
        return Ok(());
    };
    let iface = chosen["name"].as_str().unwrap_or("unknown");
    let traffic = &chosen["traffic"];

    let today = traffic_of(traffic["day"].as_array().and_then(|d| d.last()));
    let week = if let Some(week) = traffic["week"].as_array().filter(|w| !w.is_empty()) {
        traffic_of(week.last())
    } else if let Some(days) = traffic["day"].as_array() {
        let last7 = &days[days.len().saturating_sub(7)..];
        let sum: (f64, f64) = last7.iter().fold((0.0, 0.0), |(tx, rx), d| {
            (tx + d["tx"].as_f64().unwrap_or(0.0), rx + d["rx"].as_f64().unwrap_or(0.0))
        });
        Some(Traffic {
            up: format_kb(sum.0),
            down: format_kb(sum.1),
            total: format_kb(sum.0 + sum.1),
        })
    } else {
        None
    };
    let month = traffic_of(traffic["month"].as_array().and_then(|m| m.last()));

    let mut report = format!("<b>📊 Data Usage ({iface})</b>\n<blockquote>");
    report += &match &today {
        Some(t) => format!("📅 <b>Today</b>\nUpload : {}\nDownload : {}\nTotal : {}\n", t.up, t.down, t.total),
        None => "No data\n".into(),
    };
    report += &match &week {
        Some(w) => format!("\n📆 <b>This Week</b>\nUpload : {}\nDownload : {}\nTotal : {}\n", w.up, w.down, w.total),
        None => "No data\n".into(),
    };
    if let Some(m) = &month {
        report += &format!("\n🗓️ <b>This Month</b>\nUpload : {}\nDownload : {}\nTotal : {}", m.up, m.down, m.total);
    }
    report += "</blockquote>";
    bot.send(msg.chat_id, msg.thread_id, report.trim(), Some(msg.message_id), None).await?;
    Ok(())
}

/// Telegram's answer for a callback that waited too long or belongs to a previous run.
fn is_expired_query(message: &str) -> bool {
    message.contains("query is too old") || message.contains("query ID is invalid")
}

fn music_info_text(title: &str, performer: &str, duration: &str, status: &str) -> String {
    format!(
        "<b>🎵 Music Info</b>\n<blockquote><b>📝 Title:</b>\n-> <code>{}</code>\n<b>👤 Artist:</b>\n-> <code>{}</code>\n<b>⏳ Duration:</b>\n-> <code>{duration}</code>\n<b>📦 Status:</b>\n-> <code>{status}</code></blockquote>",
        escape_html(title),
        escape_html(performer)
    )
}

async fn handle_music(bot: &Bot, msg: &Msg, target_topic: Option<i64>) -> Result<()> {
    let query = msg
        .text
        .trim()
        .trim_start_matches('/')
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default();
    if query.is_empty() {
        bot.send(msg.chat_id, msg.thread_id, "❌ Please provide a song title or link.", Some(msg.message_id), None).await?;
        return Ok(());
    }

    let search_arg = if query.starts_with("http") {
        query.clone()
    } else if query.to_lowercase().starts_with("sc:") {
        format!("scsearch1:{}", query[3..].trim())
    } else {
        // YouTube Music first: plain ytsearch ranks slowed, reverb and tiktok edits of a song
        // above the song itself
        match crate::ytmusic::search(&query, 1).await {
            Ok(tracks) if !tracks.is_empty() => {
                let track = &tracks[0];
                logln!(MOD, "[music] YouTube Music picked {} - {}", track.title, track.artist);
                format!("https://www.youtube.com/watch?v={}", track.id)
            }
            Ok(_) => {
                logln!(MOD, "[music] YouTube Music had no match for {query:?}, falling back to ytsearch");
                format!("ytsearch1:{query}")
            }
            Err(e) => {
                logln!(MOD, "[music] YouTube Music search failed: {e}");
                format!("ytsearch1:{query}")
            }
        }
    };

    let typing = bot.typing_loop(msg.chat_id, msg.thread_id, "typing").await;
    let info_args = vec![
        "-f".to_string(),
        crate::ytdlp::AUDIO_FORMAT_CHAIN.to_string(),
        "--no-playlist".to_string(),
        "-j".to_string(),
    ];
    // the shared runner walks to the next client on a bot wall, and says why it gave up
    let stdout = match crate::ytdlp::run(info_args, search_arg.clone(), Duration::from_secs(60), MOD).await {
        Ok(out) => out,
        Err(e) => {
            typing.abort();
            logln!(MOD, "[FETCH ERROR] yt-dlp -j failed for {search_arg:?}: {e:#}");
            bot.send(msg.chat_id, msg.thread_id, "❌ Failed to fetch music.", Some(msg.message_id), None).await?;
            return Ok(());
        }
    };
    let info: Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            typing.abort();
            logln!(
                MOD,
                "[FETCH ERROR] yt-dlp printed no JSON for {search_arg:?}: {e}: {}",
                crate::util::truncate(stdout.trim(), 300)
            );
            bot.send(msg.chat_id, msg.thread_id, "❌ Failed to fetch music.", Some(msg.message_id), None).await?;
            return Ok(());
        }
    };

    let title = info["track"].as_str().or(info["title"].as_str()).unwrap_or("Unknown Title").to_string();
    let performer = info["artist"]
        .as_str()
        .or(info["uploader"].as_str())
        .or(info["uploader_id"].as_str())
        .unwrap_or("Unknown Artist")
        .to_string();
    let duration = format_track_duration(info["duration"].as_f64().unwrap_or(0.0));
    let media_url = info["webpage_url"]
        .as_str()
        .map(str::to_string)
        .or_else(|| if query.starts_with("http") { Some(query.clone()) } else { info["url"].as_str().map(str::to_string) })
        .unwrap_or_default();

    let file_id = format!("{}_{}", chrono::Utc::now().timestamp_millis(), info["id"].as_str().unwrap_or("x"));
    let audio_out = dir().join(format!("{file_id}.%(ext)s"));
    let thumb_base = dir().join(&file_id);

    let info_message = bot
        .send(
            msg.chat_id,
            msg.thread_id,
            &music_info_text(&title, &performer, &duration, "Downloading"),
            Some(msg.message_id),
            None,
        )
        .await?;

    let dl_args = vec![
        "-f".to_string(),
        crate::ytdlp::AUDIO_FORMAT_CHAIN.to_string(),
        "--no-playlist".to_string(),
        "-o".to_string(), audio_out.display().to_string(),
        "--write-thumbnail".to_string(),
        "--convert-thumbnails".to_string(), "jpg".to_string(),
        "-o".to_string(), format!("thumbnail:{}", thumb_base.display()),
    ];
    let dl_ok = match crate::ytdlp::run(dl_args, media_url.clone(), Duration::from_secs(180), MOD).await {
        Ok(_) => true,
        Err(e) => {
            logln!(MOD, "[DOWNLOAD ERROR] {media_url}: {e:#}");
            false
        }
    };

    let mut status = "Failed";
    let mut audio_file = None;
    if dl_ok {
        if let Some(path) = find_downloaded_audio(&file_id) {
            let too_large = std::fs::metadata(&path).map(|m| m.len() > 49 * 1024 * 1024).unwrap_or(true);
            if too_large {
                status = "Too Large";
            } else {
                status = "Sending";
                audio_file = Some(path);
            }
        }
    }

    bot.edit(msg.chat_id, info_message, &music_info_text(&title, &performer, &duration, status), None).await?;

    if let Some(path) = audio_file {
        let bytes = std::fs::read(&path)?;
        let filename = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "audio".into());
        let mut extra = json!({
            "title": title,
            "performer": performer,
            "duration": info["duration"].as_i64().unwrap_or(0),
        });
        let sent = bot
            .send_file(Upload {
                kind: "sendAudio",
                chat_id: msg.chat_id,
                thread: target_topic.or(msg.thread_id),
                field: "audio",
                bytes,
                filename: &filename,
                extra: extra.take(),
                thumb: audio_thumb(&thumb_base, &info)
                    .await
                    .map(|(bytes, _)| (bytes, format!("{file_id}.jpg"))),
            })
            .await;
        if let Err(e) = &sent {
            logln!(MOD, "[SEND ERROR] audio for {file_id}: {e:#}");
        }
        status = if sent.is_ok() { "Success" } else { "Failed" };
        bot.edit(msg.chat_id, info_message, &music_info_text(&title, &performer, &duration, status), None).await?;
    }

    typing.abort();
    // best-effort cleanup of downloaded audio + thumbnails
    let _ = std::fs::remove_file(thumb_base.with_extension("jpg"));
    let _ = std::fs::remove_file(thumb_base.with_extension("webp"));
    let _ = std::fs::remove_file(thumb_base.with_extension("png"));
    if let Some(path) = find_downloaded_audio(&file_id) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Telegram accepts an audio cover only as a small jpeg. yt-dlp converts it when ffmpeg is on
/// PATH; when it is not, the file left behind is the original webp, which this build cannot decode
/// — so the jpg URL from the info JSON is fetched instead. i.ytimg.com offers both, and only the
/// jpg one is usable here, which is why the `.webp` entry must not be picked.
async fn audio_thumb(base: &std::path::Path, info: &Value) -> Option<(Vec<u8>, String)> {
    let mut raw = ["jpg", "jpeg", "png"]
        .iter()
        .find_map(|ext| std::fs::read(base.with_extension(ext)).ok());
    if raw.is_none() {
        match jpg_thumbnail_url(info) {
            Some(url) => match reqwest::get(url).await {
                Ok(res) => raw = res.bytes().await.ok().map(|b| b.to_vec()),
                Err(e) => logln!(MOD, "[music] thumbnail fetch failed: {e}"),
            },
            None => logln!(MOD, "[music] no jpg thumbnail in the info JSON"),
        }
    }
    let Some(raw) = raw else {
        logln!(MOD, "[music] no cover file and no jpg url, sending without artwork");
        return None;
    };
    match image::load_from_memory(&raw) {
        Ok(img) => {
            let small = img.thumbnail(320, 320);
            let (w, h) = (small.width(), small.height());
            let mut out = std::io::Cursor::new(Vec::new());
            match small.write_to(&mut out, image::ImageFormat::Jpeg) {
                Ok(()) => {
                    let bytes = out.into_inner();
                    logln!(MOD, "[music] cover {w}x{h} {}KB", bytes.len() / 1024);
                    Some((bytes, "thumb.jpg".to_string()))
                }
                Err(e) => {
                    logln!(MOD, "[music] thumbnail encode failed: {e}");
                    None
                }
            }
        }
        Err(e) => {
            logln!(MOD, "[music] thumbnail decode failed: {e}");
            None
        }
    }
}

/// The largest jpg yt-dlp listed. `info["thumbnail"]` is usually the webp variant, so the
/// `thumbnails` array is searched instead of trusting that field.
fn jpg_thumbnail_url(info: &Value) -> Option<&str> {
    fn jpg(entry: &Value) -> Option<&str> {
        entry["url"]
            .as_str()
            .filter(|u| u.ends_with(".jpg") || u.ends_with(".jpeg"))
    }
    info["thumbnails"]
        .as_array()
        .and_then(|list| list.iter().rev().find_map(jpg))
        .or_else(|| jpg(&info["thumbnail"]))
}

fn volume_home_text(volumes: &[Value]) -> String {
    let mut text = String::from("<b>🔊 Playback Control</b>\n<blockquote>");
    for v in volumes {
        text += &format!("<b>Stream</b>\n-> {}\n<b>Current</b>\n-> {}\n<b>Maximum</b>\n-> {}\n\n", v["stream"].as_str().unwrap_or(""), v["volume"].as_i64().unwrap_or(0), v["max_volume"].as_i64().unwrap_or(0));
    }
    text + "</blockquote>"
}

fn volume_keyboard(volumes: &[Value]) -> Value {
    json!(volumes
        .iter()
        .map(|v| {
            let stream = v["stream"].as_str().unwrap_or("");
            json!([{ "text": stream, "callback_data": format!("volume_select_{stream}") }])
        })
        .collect::<Vec<_>>())
}

fn build_volume_text(target: &Value) -> String {
    format!(
        "<b>🔊 Playback Control</b>\n<blockquote><b>Stream</b>\n-> {}\n<b>Current</b>\n-> {}\n<b>Maximum</b>\n-> {}</blockquote>",
        target["stream"].as_str().unwrap_or(""),
        target["volume"].as_i64().unwrap_or(0),
        target["max_volume"].as_i64().unwrap_or(0)
    )
}

fn build_volume_keyboard(stream: &str, target: &Value) -> Value {
    let volume = target["volume"].as_i64().unwrap_or(0);
    let max = target["max_volume"].as_i64().unwrap_or(15);
    json!([
        [
            { "text": "➖5", "callback_data": format!("volume_set_{stream}_{}", (volume - 5).max(0)) },
            { "text": "➖1", "callback_data": format!("volume_set_{stream}_{}", (volume - 1).max(0)) },
            { "text": "➕1", "callback_data": format!("volume_set_{stream}_{}", (volume + 1).min(max)) },
            { "text": "➕5", "callback_data": format!("volume_set_{stream}_{}", (volume + 5).min(max)) }
        ],
        [
            { "text": "🔇 Mute", "callback_data": format!("volume_set_{stream}_0") },
            { "text": "🔊 Max", "callback_data": format!("volume_set_{stream}_{max}") }
        ],
        [
            { "text": "⏮️", "callback_data": "media_previous" },
            { "text": "▶️", "callback_data": "media_play" },
            { "text": "⏹️", "callback_data": "media_stop" },
            { "text": "⏭️", "callback_data": "media_next" }
        ],
        [{ "text": "⬅️ Back", "callback_data": "volume_home" }]
    ])
}

async fn volumes() -> Option<Vec<Value>> {
    let (ok, out) = sh("termux-volume", &[]).await;
    if !ok {
        return None;
    }
    serde_json::from_str::<Vec<Value>>(&out).ok()
}

async fn cmd_playback(bot: &Bot, msg: &Msg) -> Result<()> {
    let Some(list) = volumes().await else {
        bot.send(msg.chat_id, msg.thread_id, "❌ Failed to get volume information", Some(msg.message_id), None).await?;
        return Ok(());
    };
    bot.send(
        msg.chat_id,
        msg.thread_id,
        &volume_home_text(&list),
        Some(msg.message_id),
        Some(volume_keyboard(&list)),
    )
    .await?;
    Ok(())
}

async fn handle_callback(bot: &Bot, cb: &Callback, callback_id: &str) -> Result<()> {
    if !cb.is_admin {
        bot.answer_callback(callback_id, "⛔ Not authorized.", true).await?;
        return Ok(());
    }

    if let Some(stream) = cb.data.strip_prefix("volume_select_") {
        let Some(list) = volumes().await else {
            bot.answer_callback(callback_id, "Failed to get volume information", true).await?;
            return Ok(());
        };
        let Some(target) = list.iter().find(|v| v["stream"].as_str().unwrap_or("").eq_ignore_ascii_case(stream)) else {
            bot.answer_callback(callback_id, "Invalid stream", true).await?;
            return Ok(());
        };
        bot.edit(cb.chat_id, cb.message_id, &build_volume_text(target), Some(build_volume_keyboard(stream, target))).await?;
        bot.answer_callback(callback_id, "", false).await?;
        return Ok(());
    }

    if let Some(rest) = cb.data.strip_prefix("volume_set_") {
        let (stream, value) = rest.rsplit_once('_').unwrap_or((rest, "0"));
        let (ok, _) = sh("termux-volume", &[stream, value]).await;
        if !ok {
            logln!(MOD, "Volume set failed");
            bot.answer_callback(callback_id, "Failed to set volume", true).await?;
            return Ok(());
        }
        let Some(list) = volumes().await else {
            bot.answer_callback(callback_id, "Failed to refresh volume", true).await?;
            return Ok(());
        };
        let Some(target) = list.iter().find(|v| v["stream"].as_str().unwrap_or("").eq_ignore_ascii_case(stream)) else {
            bot.answer_callback(callback_id, "Invalid stream", true).await?;
            return Ok(());
        };
        bot.edit(cb.chat_id, cb.message_id, &build_volume_text(target), Some(build_volume_keyboard(stream, target))).await?;
        bot.answer_callback(callback_id, &format!("{}: {}/{}", stream, target["volume"].as_i64().unwrap_or(0), target["max_volume"].as_i64().unwrap_or(0)), false).await?;
        return Ok(());
    }

    if cb.data == "volume_home" {
        let Some(list) = volumes().await else {
            bot.answer_callback(callback_id, "Failed", true).await?;
            return Ok(());
        };
        bot.edit(cb.chat_id, cb.message_id, &volume_home_text(&list), Some(volume_keyboard(&list))).await?;
        bot.answer_callback(callback_id, "", false).await?;
        return Ok(());
    }

    if let Some(action) = cb.data.strip_prefix("media_") {
        let (ok, _) = sh("cmd", &["media_session", "dispatch", action]).await;
        if ok {
            bot.answer_callback(callback_id, &format!("{action} executed"), false).await?;
        } else {
            bot.answer_callback(callback_id, &format!("Failed: {action}"), true).await?;
        }
    }
    Ok(())
}

async fn fetch_price_rows() -> Result<Vec<Vec<String>>> {
    let sheets_id = crate::config::req("SHEETS_ID")?;
    let gid = crate::config::get("SHEETS_GID").unwrap_or_else(|| "0".into());
    let url = format!("https://docs.google.com/spreadsheets/d/{sheets_id}/export?format=csv&gid={gid}");
    // `reqwest::get` builds a default client whose TLS verifier is rustls-platform-verifier on
    // Android, which panics ("Expect rustls-platform-verifier to be initialized"). Use util::client().
    let csv = crate::util::client().get(&url).send().await?.text().await?;

    let mut rows = Vec::new();
    for line in csv.split('\n') {
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut in_quotes = false;
        for ch in line.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => {
                    fields.push(field.trim().to_string());
                    field.clear();
                }
                _ => field.push(ch),
            }
        }
        fields.push(field.trim().to_string());
        if fields.iter().any(|c| !c.replace('\r', "").is_empty()) {
            rows.push(fields);
        }
    }
    Ok(rows)
}

async fn cmd_price(bot: &Bot, msg: &Msg, keyword: &str) -> Result<()> {
    let keyword = keyword.trim().to_lowercase();
    if keyword.is_empty() {
        bot.send(msg.chat_id, msg.thread_id, "❌ Please provide an item name.", Some(msg.message_id), None).await?;
        return Ok(());
    }
    let rows = match fetch_price_rows().await {
        Ok(r) => r,
        Err(e) => {
            logln!(MOD, "Price fetch error: {e}");
            bot.send(msg.chat_id, msg.thread_id, "❌ Failed to fetch price list.", Some(msg.message_id), None).await?;
            return Ok(());
        }
    };
    let data_rows: Vec<&Vec<String>> = rows.iter().skip(1).filter(|r| r.first().map(|c| !c.replace('\r', "").is_empty()).unwrap_or(false)).collect();
    if data_rows.is_empty() {
        bot.send(msg.chat_id, msg.thread_id, "❌ Price list is empty.", Some(msg.message_id), None).await?;
        return Ok(());
    }
    let filtered: Vec<&&Vec<String>> = data_rows
        .iter()
        .filter(|r| {
            r.first().map(|c| c.to_lowercase().contains(&keyword)).unwrap_or(false)
                || r.get(1).map(|c| c.to_lowercase().contains(&keyword)).unwrap_or(false)
        })
        .collect();
    if filtered.is_empty() {
        bot.send(
            msg.chat_id,
            msg.thread_id,
            &format!("❌ No items found matching \"<b>{}</b>\".", escape_html(&keyword)),
            Some(msg.message_id),
            None,
        )
        .await?;
        return Ok(());
    }

    let cell = |row: &Vec<String>, i: usize| row.get(i).map(|c| c.replace('\r', "")).unwrap_or_else(|| "-".into());
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::from("<b>💰 Norikimika Price</b>\n");
    for row in filtered {
        let name = escape_html(&cell(row, 0));
        let contents = escape_html(&cell(row, 1));
        let price = escape_html(&cell(row, 2));
        let capital_raw = cell(row, 5).trim().to_string();
        let capital = escape_html(if capital_raw.to_uppercase() == "#VALUE!" { "-" } else { &capital_raw });
        let profit_raw = cell(row, 6).trim().to_string();
        let profit = escape_html(if profit_raw.to_uppercase() == "#VALUE!" { "-" } else { &profit_raw });
        let item = format!(
            "<blockquote>Name : {name}\nContents : {contents}\nPrice : {price}\nCapital : <tg-spoiler>{capital}</tg-spoiler>\nProfit : <tg-spoiler>{profit}</tg-spoiler></blockquote>\n"
        );
        if current.chars().count() + item.chars().count() > 3800 {
            chunks.push(current.trim().to_string());
            current = item;
        } else {
            current += &item;
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    for chunk in chunks {
        bot.send(msg.chat_id, msg.thread_id, &chunk, Some(msg.message_id), None).await?;
    }
    Ok(())
}

async fn cmd_approve(bot: &Bot, msg: &Msg) -> Result<()> {
    let (ok, _) = sh_root("input tap 150 1260 && sleep 2 && input tap 880 1435").await;
    if ok {
        bot.send(msg.chat_id, msg.thread_id, "✅ Approve executed", Some(msg.message_id), None).await?;
    } else {
        logln!(MOD, "Approve failed");
        bot.send(msg.chat_id, msg.thread_id, "❌ Failed to run approve", Some(msg.message_id), None).await?;
    }
    Ok(())
}

fn tt_status_text(platform: &str, uploader: &str, title: &str, status: &str) -> String {
    format!(
        "<b>🎬 {platform} Info</b>\n<blockquote><b>👤 Author:</b>\n-> <code>{}</code>\n<b>📝 Title:</b>\n-> <code>{}</code>\n<b>📦 Status:</b>\n-> <code>{status}</code></blockquote>",
        escape_html(uploader),
        escape_html(title)
    )
}

async fn download(url: &str) -> Result<Vec<u8>> {
    Ok(crate::util::client().get(url).send().await?.bytes().await?.to_vec())
}

async fn cmd_tt(bot: &Bot, msg: &Msg) -> Result<()> {
    let target_topic = topic("TOPIC_ID_TIKTOK").or(msg.thread_id);
    let combined = format!("{} {}", msg.text.trim(), msg.reply_text.trim());
    let target_url = combined
        .split_whitespace()
        .find(|t| t.starts_with("http"))
        .map(str::to_string);

    let Some(target_url) = target_url else {
        bot.send(
            msg.chat_id,
            msg.thread_id,
            "❌ Please provide a TikTok/Douyin URL or reply to a message containing one.",
            Some(msg.message_id),
            None,
        )
        .await?;
        return Ok(());
    };

    let is_douyin = target_url.contains("douyin.com") || target_url.contains("iesdouyin.com");
    let platform = if is_douyin { "Douyin" } else { "TikTok" };
    let typing = bot.typing_loop(msg.chat_id, target_topic, "upload_document").await;
    let info_message = bot
        .send(
            msg.chat_id,
            msg.thread_id,
            &tt_status_text(platform, "Fetching...", "Fetching...", "Downloading"),
            Some(msg.message_id),
            None,
        )
        .await?;

    let result = fetch_tt_media(&target_url, is_douyin).await;
    match result {
        Err(e) => {
            logln!(MOD, "[TT ERROR] {e}");
            bot.edit(msg.chat_id, info_message, &tt_status_text(platform, "Unknown", "Error", "Failed"), None).await?;
        }
        Ok(media) => {
            let title: String = media.title.chars().take(80).collect();
            bot.edit(msg.chat_id, info_message, &tt_status_text(platform, &media.uploader, &title, "Sending"), None).await?;

            let sent = if !media.slides.is_empty() {
                send_slides(bot, msg.chat_id, target_topic, &media.slides).await
            } else if !media.images.is_empty() {
                send_slides(bot, msg.chat_id, target_topic, &media.images).await
            } else if let Some(video) = &media.video {
                bot.send_file(Upload {
                    kind: "sendVideo",
                    chat_id: msg.chat_id,
                    thread: target_topic,
                    field: "video",
                    bytes: video.clone(),
                    filename: "video.mp4",
                    extra: json!({}),
                    thumb: None,
                })
                .await
            } else {
                Err(anyhow!("No media downloaded from {platform} response"))
            };

            if let Err(e) = sent {
                logln!(MOD, "[TT ERROR] {e}");
                bot.edit(msg.chat_id, info_message, &tt_status_text(platform, &media.uploader, &title, "Failed"), None).await?;
            } else {
                if let Some(audio) = &media.audio {
                    if let Err(e) = bot
                        .send_file(Upload {
                            kind: "sendAudio",
                            chat_id: msg.chat_id,
                            thread: target_topic,
                            field: "audio",
                            bytes: audio.clone(),
                            filename: "audio.mp3",
                            extra: json!({ "title": title, "performer": media.uploader }),
                            thumb: None,
                        })
                        .await
                    {
                        logln!(MOD, "[TT] audio upload failed: {e}");
                    }
                }
                bot.edit(msg.chat_id, info_message, &tt_status_text(platform, &media.uploader, &title, "Success"), None).await?;
            }
        }
    }
    typing.abort();
    Ok(())
}

struct TtMedia {
    title: String,
    uploader: String,
    video: Option<Vec<u8>>,
    images: Vec<(String, Vec<u8>, String)>,
    slides: Vec<(String, Vec<u8>, String)>,
    audio: Option<Vec<u8>>,
}

async fn send_slides(bot: &Bot, chat_id: i64, thread: Option<i64>, items: &[(String, Vec<u8>, String)]) -> Result<()> {
    if items.len() == 1 {
        let (kind, bytes, name) = &items[0];
        let method = if kind == "photo" { "sendPhoto" } else { "sendVideo" };
        return bot
            .send_file(Upload {
                kind: method,
                chat_id,
                thread,
                field: "photo",
                bytes: bytes.clone(),
                filename: name,
                extra: json!({}),
                thumb: None,
            })
            .await;
    }
    for chunk in items.chunks(10) {
        bot.send_media_group(chat_id, thread, chunk.to_vec()).await?;
    }
    Ok(())
}

async fn fetch_tt_media(url: &str, is_douyin: bool) -> Result<TtMedia> {
    if is_douyin {
        let api = format!("https://douyin.wtf/api/hybrid/video_data?url={}&minimal=true", urlencoding(url));
        let json: Value = crate::util::client().get(&api).send().await?.json().await?;
        if json["code"].as_i64() != Some(200) {
            return Err(anyhow!(json["message"].as_str().unwrap_or("Failed to fetch media from Douyin API").to_string()));
        }
        let data = &json["data"];
        let title = data["desc"].as_str().unwrap_or("Douyin Post").to_string();
        let uploader = data["author"]["nickname"].as_str().or(data["author"]["unique_id"].as_str()).unwrap_or("Unknown").to_string();
        let vd = &data["video_data"];
        let video_url = ["nwm_video_url", "nwm_video_url_HQ", "wm_video_url", "wm_video_url_HQ"]
            .iter()
            .find_map(|k| vd[k].as_str());
        let image_urls: Vec<String> = data["image_data"]["no_watermark_image_list"]
            .as_array()
            .or(data["image_data"]["watermark_image_list"].as_array())
            .or(data["images"].as_array())
            .map(|arr| arr.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let audio_url = data["music"]["play_url"]["url_list"][0].as_str().map(str::to_string);

        let mut images = Vec::new();
        for (i, image) in image_urls.iter().enumerate() {
            if let Some(bytes) = download_logged(image, "image").await {
                images.push(("photo".to_string(), bytes, format!("photo_{:02}.jpg", i + 1)));
            }
        }
        let video = match (video_url, images.is_empty()) {
            (Some(v), true) => Some(download(v).await?),
            _ => None,
        };
        let audio = match audio_url {
            Some(a) => download_logged(&a, "audio").await,
            None => None,
        };
        Ok(TtMedia { title, uploader, video, images, slides: Vec::new(), audio })
    } else {
        let api = format!("https://www.tikwm.com/api/?url={}&hd=1", urlencoding(url));
        let json: Value = crate::util::client().get(&api).send().await?.json().await?;
        if json["code"].as_i64() != Some(0) {
            return Err(anyhow!(json["msg"].as_str().unwrap_or("Failed to fetch media from TikTok API").to_string()));
        }
        let data = &json["data"];
        let title = data["title"].as_str().unwrap_or("TikTok Post").to_string();
        let uploader = data["author"]["nickname"].as_str().or(data["author"]["unique_id"].as_str()).unwrap_or("Unknown").to_string();
        let video_url = data["hdplay"].as_str().or(data["play"].as_str()).map(str::to_string);
        let image_urls: Vec<String> = data["images"].as_array().map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let live_urls: Vec<String> = data["live_images"].as_array().map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let audio_url = data["music_info"]["play"].as_str().or(data["music"].as_str()).map(str::to_string);

        let mut slides = Vec::new();
        if !live_urls.is_empty() {
            let n = image_urls.len().max(live_urls.len());
            for i in 0..n {
                let tag = format!("{:02}", i + 1);
                if let Some(live) = live_urls.get(i) {
                    if let Some(bytes) = download_logged(live, "slide video").await {
                        slides.push(("video".to_string(), bytes, format!("slide_{tag}.mp4")));
                    }
                } else if let Some(img) = image_urls.get(i) {
                    if let Some(bytes) = download_logged(img, "slide image").await {
                        slides.push(("photo".to_string(), bytes, format!("slide_{tag}.jpg")));
                    }
                }
            }
        }

        let mut images = Vec::new();
        if slides.is_empty() {
            for (i, image) in image_urls.iter().enumerate() {
                if let Some(bytes) = download_logged(image, "image").await {
                    images.push(("photo".to_string(), bytes, format!("photo_{:02}.jpg", i + 1)));
                }
            }
        }
        let video = match (video_url, slides.is_empty() && images.is_empty()) {
            (Some(v), true) => Some(download(&v).await?),
            _ => None,
        };
        let audio = match audio_url {
            Some(a) => download_logged(&a, "audio").await,
            None => None,
        };
        Ok(TtMedia { title, uploader, video, images, slides, audio })
    }
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

const RECEIPT_PROMPT: &str = "Identify the type of this receipt and extract its data.\nRespond with a single JSON object and nothing else — no preamble, no safety note, no markdown, no explanation. Your entire response must start with { and end with }.\n\nIf it's a Money TRANSFER receipt (DANA, OVO, GoPay, BCA, BRI, etc):\n{\n  \"type\": \"transfer\",\n  \"tanggal\": \"YYYY-MM-DD\",\n  \"waktu\": \"HH:MM:SS\",\n  \"bankTujuan\": \"destination bank/wallet name e.g. DANA\",\n  \"noRekening\": \"destination account/phone number\",\n  \"penerima\": \"recipient name\",\n  \"nominal\": \"amount transferred excluding admin fee\"\n}\n\nIf it's a PLN ELECTRICITY TOKEN receipt:\n{\n  \"type\": \"pln\",\n  \"tanggal\": \"YYYY-MM-DD\",\n  \"waktu\": \"HH:MM:SS\",\n  \"idPel\": \"customer ID number\",\n  \"noMeter\": \"meter number\",\n  \"nama\": \"customer name\",\n  \"tarif\": \"tariff/power e.g. R2/3500VA\",\n  \"jmlKwh\": \"kWh total amount\",\n  \"token\": \"20 digits token numbers only without space or hyphen\",\n  \"nominal\": \"total payment paid by customer with thousands separator\"\n}\n\nIf it's a Pulsa receipt (Telkomsel, Tri, XL, simPATI, Indosat, etc):\n{\n  \"type\": \"pulsa\",\n  \"tanggal\": \"YYYY-MM-DD\",\n  \"waktu\": \"HH:MM:SS\",\n  \"providerTujuan\": \"destination provider/sim name e.g. SIMPATI\",\n  \"noProvider\": \"destination account/phone number\",\n  \"nominal\": \"amount transferred excluding admin fee\"\n}";

/// Extract the first balanced JSON object that carries a "type" field.
fn extract_receipt_json(text: &str) -> Option<Value> {
    let bytes: Vec<char> = text.chars().collect();
    for start in 0..bytes.len() {
        if bytes[start] != '{' {
            continue;
        }
        let mut depth = 0i64;
        let mut in_str = false;
        let mut esc = false;
        for i in start..bytes.len() {
            let c = bytes[i];
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate: String = bytes[start..=i].iter().collect();
                        if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
                            if v["type"].is_string() {
                                return Some(v);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

async fn analyze_receipt(base64_image: &str) -> Result<Value> {
    let raw_url = crate::config::req("NINE_ROUTER_URL")?;
    let api_key = crate::config::req("NINE_ROUTER_API_KEY")?;
    let model = crate::config::req("NINE_ROUTER_VISION_MODEL")?;

    let base = raw_url.trim_end_matches('/');
    let endpoint = if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    };

    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{base64_image}") } },
                { "type": "text", "text": RECEIPT_PROMPT }
            ]
        }],
        "stream": false,
        "response_format": { "type": "json_object" }
    });

    let res = crate::util::client()
        .post(&endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {api_key}"))
        .timeout(Duration::from_secs(60))
        .json(&body)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        return Err(anyhow!("9Router API HTTP {status}: {text}"));
    }
    let raw = res.text().await?;

    let mut content = String::new();
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => content = v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string(),
        Err(_) => {
            for line in raw.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("data: ") {
                    if rest == "[DONE]" {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(rest) {
                        if let Some(part) = v["choices"][0]["delta"]["content"].as_str() {
                            content.push_str(part);
                        } else if let Some(part) = v["choices"][0]["message"]["content"].as_str() {
                            content.push_str(part);
                        }
                    }
                }
            }
        }
    }
    if content.is_empty() {
        return Err(anyhow!("9Router returned empty response for vision analysis"));
    }
    logln!(MOD, "Vision: 9Router OK");

    let cleaned: String = content.replace("```json", "").replace("```", "");
    extract_receipt_json(cleaned.trim()).ok_or_else(|| anyhow!("no JSON object with \"type\" field"))
}

async fn cmd_receipt(bot: &Bot, msg: &Msg, file_id: &str) -> Result<()> {
    let intake = topic("TOPIC_ID_INTAKE").unwrap_or(416182);
    if msg.thread_id != Some(intake) {
        return Ok(());
    }
    let target_thread = topic("TOPIC_ID_PREVIEW").or(msg.thread_id);

    let typing = bot.typing_loop(msg.chat_id, target_thread, "upload_photo").await;
    let result: Result<()> = async {
        let bytes = bot.download_file(file_id).await?;
        let b64 = base64_encode(&bytes);
        let parsed = analyze_receipt(&b64).await?;

        let token_digits = parsed["token"].as_str().map(|t| t.chars().filter(|c| c.is_ascii_digit()).count()).unwrap_or(0);
        let kind = parsed["type"].as_str().unwrap_or("");
        let valid = (kind == "pln" && token_digits >= 10)
            || (kind == "transfer" && (parsed["noRekening"].is_string() || parsed["bankTujuan"].is_string()))
            || (kind == "pulsa" && (parsed["providerTujuan"].is_string() || parsed["noProvider"].is_string()));
        if !valid {
            logln!(MOD, "Struk validation failed: Image is not recognized as a valid receipt.");
            return Ok(());
        }

        let mut data = Struk::from_json(&parsed);
        if !data.penerima.is_empty() {
            const NOISE: &[&str] = &["Dana", "Dnid", "Dnidl", "Dnidd", "Dnidx", "Dnvid"];
            for word in NOISE {
                data.penerima = data.penerima.replace(word, "");
            }
            data.penerima = data.penerima.split_whitespace().collect::<Vec<_>>().join(" ");
        }
        if !data.nominal.is_empty() {
            let digits: String = data.nominal.chars().filter(|c| c.is_ascii_digit()).collect();
            data.nominal = rupiah(digits.parse::<f64>().unwrap_or(0.0));
        }

        let struk = build_struk(&data);
        let png = render_struk_image(&struk)?;
        bot.send_file(Upload {
            kind: "sendPhoto",
            chat_id: msg.chat_id,
            thread: target_thread,
            field: "photo",
            bytes: png.clone(),
            filename: "receipt.png",
            extra: json!({}),
            thumb: None,
        })
        .await?;

        match crate::config::get("PRINT_CMD") {
            Some(cmd) if !cmd.is_empty() => {
                let path = dir().join(format!("struk_{}.png", chrono::Utc::now().timestamp_millis()));
                std::fs::write(&path, &png)?;
                let full = format!("{cmd} \"{}\"", path.display());
                let (ok, _) = sh_shell(&full).await;
                if !ok {
                    logln!(MOD, "Print execution error");
                }
            }
            _ => logln!(MOD, "Print error: PRINT_CMD is not configured in .env."),
        }
        Ok(())
    }
    .await;

    typing.abort();
    if let Err(e) = result {
        logln!(MOD, "Receipt process error: {e}");
    }
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

const HELP_TEXT: &str = "<b>🤖 Public Commands</b>\n<blockquote><b>✹ Price List</b>\n-> <code>/p [name]</code></blockquote>\n<b>⚠️ Admin Commands</b>\n<blockquote><b>✹ Approve</b>\n-> /approve</blockquote>\n<blockquote><b>✹ Battery Status</b>\n-> /battery</blockquote>\n<blockquote><b>✹ Download Music</b>\n-> <code>/music [title]</code>\n-> <code>/music2 [title]</code></blockquote>\n<blockquote><b>✹ Download TikTok/Douyin</b>\n-> <code>/tt [link]</code></blockquote>\n<blockquote><b>✹ Data Usage</b>\n-> /usage</blockquote>\n<blockquote><b>✹ Playback Controller</b>\n-> /playback</blockquote>\n<blockquote><b>✹ Reboot Device</b>\n-> /reboot</blockquote>\n<blockquote><b>✹ Speed Test</b>\n-> /speed</blockquote>";

fn command_of(text: &str) -> (String, String) {
    let trimmed = text.trim();
    let without_slash = trimmed.trim_start_matches('/');
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").split('@').next().unwrap_or("").to_lowercase();
    let args = parts.next().unwrap_or("").trim().to_string();
    (cmd, args)
}

// ================== update dispatch ==================

async fn handle_message(bot: &Bot, msg: Msg) -> Result<()> {
    let in_topic = bot.topic_id.is_some() && msg.thread_id == bot.topic_id;

    if let Some(file_id) = &msg.photo_file_id {
        if bot.require_admin(&msg).await {
            // a failing receipt used to vanish: no reply, no log, user waiting
            if let Err(e) = cmd_receipt(bot, &msg, file_id).await {
                logln!(MOD, "[Receipt] {e:#}");
                let text = format!("❌ Receipt failed: {}", escape_html(&format!("{e:#}")));
                if let Err(send_err) = bot.send(msg.chat_id, msg.thread_id, &text, Some(msg.message_id), None).await {
                    logln!(MOD, "[Receipt] could not report the failure: {send_err}");
                }
            }
        }
        return Ok(());
    }

    let text = msg.text.clone();
    if text.trim().is_empty() {
        return Ok(());
    }

    // Non-command chatter in the main topic from non-admins: warn, never mute (see require_admin).
    if in_topic && !text.trim_start().starts_with('/') && !bot.is_admin(msg.chat_id, msg.user_id).await {
        bot.send(
            msg.chat_id,
            msg.thread_id,
            "⛔ Commands only!",
            Some(msg.message_id),
            None,
        )
        .await?;
        return Ok(());
    }

    if !text.trim_start().starts_with('/') {
        return Ok(());
    }

    let (cmd, args) = command_of(&text);
    match cmd.as_str() {
        "p" => cmd_price(bot, &msg, &args).await,
        _ if !in_topic => Ok(()),
        "speed" if bot.require_admin(&msg).await => cmd_speed(bot, &msg).await,
        "reboot" if bot.require_admin(&msg).await => cmd_reboot(bot, &msg).await,
        "battery" if bot.require_admin(&msg).await => cmd_battery(bot, &msg).await,
        "usage" if bot.require_admin(&msg).await => cmd_usage(bot, &msg).await,
        "music" if bot.require_admin(&msg).await => handle_music(bot, &msg, topic("TOPIC_ID_MUSIC")).await,
        "music2" if bot.require_admin(&msg).await => handle_music(bot, &msg, topic("TOPIC_ID_MUSIC2")).await,
        "playback" if bot.require_admin(&msg).await => cmd_playback(bot, &msg).await,
        "approve" if bot.require_admin(&msg).await => cmd_approve(bot, &msg).await,
        "tt" if bot.require_admin(&msg).await => cmd_tt(bot, &msg).await,
        _ => {
            bot.send(msg.chat_id, msg.thread_id, HELP_TEXT, Some(msg.message_id), None).await?;
            Ok(())
        }
    }
}

fn parse_message(v: &Value) -> Option<Msg> {
    let m = v.get("message")?;
    let text = m["text"].as_str().unwrap_or("").to_string();
    let photo = m["photo"].as_array().and_then(|arr| arr.last()).and_then(|p| p["file_id"].as_str()).map(str::to_string);
    Some(Msg {
        chat_id: m["chat"]["id"].as_i64()?,
        thread_id: m["message_thread_id"].as_i64(),
        user_id: m["from"]["id"].as_i64().unwrap_or(0),
        message_id: m["message_id"].as_i64().unwrap_or(0),
        text,
        reply_text: m["reply_to_message"]["text"].as_str().unwrap_or("").to_string(),
        photo_file_id: photo,
    })
}

/// Telegram's update offset has to survive restarts. It used to start at 0 every boot, so the
/// whole 24h backlog was replayed — one `/reboot` therefore rebooted the device on every start.
fn offset_path() -> PathBuf {
    crate::config::state_file("telegram.json")
}

/// The offset used to live in `telegram-offset.json`. It is still read once so an upgrade keeps
/// its position instead of falling back to "newest update" and losing what arrived while down.
fn legacy_offset_path() -> PathBuf {
    crate::config::state_file("telegram-offset.json")
}

fn read_offset(path: &std::path::Path) -> Option<i64> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v["offset"].as_str()?.parse().ok()
}

fn write_offset(path: &std::path::Path, offset: i64) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json!({ "offset": offset.to_string() }).to_string())?;
    std::fs::rename(&tmp, path)
}

fn load_offset() -> Option<i64> {
    load_offset_from(&offset_path(), &legacy_offset_path())
}

fn load_offset_from(path: &std::path::Path, legacy: &std::path::Path) -> Option<i64> {
    if let Some(offset) = read_offset(path) {
        return Some(offset);
    }
    let offset = read_offset(legacy)?;
    if let Err(e) = write_offset(path, offset) {
        logln!(MOD, "offset migrate failed: {e}");
        return Some(offset);
    }
    if let Err(e) = std::fs::remove_file(legacy) {
        logln!(MOD, "old offset file not removed: {e}");
    }
    Some(offset)
}

fn save_offset(offset: i64) {
    if let Err(e) = write_offset(&offset_path(), offset) {
        logln!(MOD, "offset save failed: {e}");
    }
}

/// With no saved offset, ask for the newest update only instead of replaying the backlog.
async fn newest_offset(bot: &Bot) -> i64 {
    let url = format!("https://api.telegram.org/bot{}/getUpdates", bot.token);
    let res = bot.http.post(&url).json(&json!({"offset": -1, "timeout": 0, "limit": 1})).send().await;
    match res {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => v["result"][0]["update_id"].as_i64().unwrap_or(-1) + 1,
            Err(e) => {
                logln!(MOD, "offset bootstrap parse failed: {e}");
                0
            }
        },
        Err(e) => {
            logln!(MOD, "offset bootstrap failed: {e}");
            0
        }
    }
}

/// One Telegram update. Kept separate so the poll loop can run it off-thread and time it out.
async fn handle_update(bot: &Bot, update: Value) -> Result<()> {
    if let Some(cb) = update.get("callback_query") {
        let data = cb["data"].as_str().unwrap_or("").to_string();
        let chat_id = cb["message"]["chat"]["id"].as_i64().unwrap_or(0);
        let user_id = cb["from"]["id"].as_i64().unwrap_or(0);
        let is_admin = bot.is_admin(chat_id, user_id).await;
        let callback = Callback {
            chat_id,
            message_id: cb["message"]["message_id"].as_i64().unwrap_or(0),
            data,
            is_admin,
        };
        let callback_id = cb["id"].as_str().unwrap_or("").to_string();
        return handle_callback(bot, &callback, &callback_id).await;
    }
    if let Some(msg) = parse_message(&update) {
        return handle_message(bot, msg).await;
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    let bot = Bot::new()?;
    logln!(MOD, "Bot launched");
    let (announced, out) = sh("termux-tts-speak", &["Bot launched"]).await;
    if !announced {
        logln!(MOD, "startup announcement failed: {}", crate::util::truncate(out.trim(), 120));
    }

    let mut offset: i64 = match load_offset() {
        Some(saved) => saved,
        None => newest_offset(&bot).await,
    };
    logln!(MOD, "Offset {offset}");
    let mut failures = 0u32;
    let mut idle_polls = 0u32;
    loop {
        let url = format!("https://api.telegram.org/bot{}/getUpdates", bot.token);
        let res = bot
            .http
            .post(&url)
            .json(&json!({ "offset": offset, "timeout": 50, "allowed_updates": ["message", "callback_query"] }))
            .timeout(Duration::from_secs(70))
            .send()
            .await;

        let payload: Value = match res {
            Ok(r) => r.json().await.unwrap_or(Value::Null),
            Err(e) => {
                failures += 1;
                // reqwest's Display stops at "error sending request"; the cause (timeout, DNS,
                // connection refused) is one level down, so the whole chain is printed.
                logln!(MOD, "getUpdates failed ({failures}): {:#}", anyhow::Error::new(e));
                // during an outage this used to hammer Telegram every 5 s and bury the log
                let backoff = 5 * u64::from(failures.min(6));
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                continue;
            }
        };
        if !payload["ok"].as_bool().unwrap_or(false) {
            failures += 1;
            logln!(MOD, "getUpdates not ok ({failures}): {payload}");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        if failures > 0 {
            logln!(MOD, "getUpdates recovered after {failures} failure(s)");
            failures = 0;
        }
        // ~50s per long poll: this heartbeat says "alive but quiet", which a silent bot never did
        if payload["result"].as_array().map(|r| r.is_empty()).unwrap_or(true) {
            idle_polls += 1;
            if idle_polls % 20 == 0 {
                logln!(MOD, "poll idle, no updates for {idle_polls} polls");
            }
        } else {
            idle_polls = 0;
        }

        let empty = Vec::new();
        for update in payload["result"].as_array().unwrap_or(&empty) {
            offset = update["update_id"].as_i64().unwrap_or(offset) + 1;
            // persist before handling: a reboot in the middle of a command must not replay it
            save_offset(offset);

            // Handlers used to run inline, so one slow command (a download, a `termux-tts-speak`
            // with no engine, a video render) stopped the bot from reading the next one until it
            // finished — commands looked dead while the process was fine.
            let bot = bot.clone();
            let update = update.clone();
            tokio::spawn(async move {
                let task = tokio::spawn(async move { handle_update(&bot, update).await });
                match tokio::time::timeout(HANDLER_TIMEOUT, task).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(e))) => logln!(MOD, "handler error: {e:#}"),
                    Ok(Err(e)) if e.is_panic() => logln!(MOD, "handler panicked: {e}"),
                    Ok(Err(e)) => logln!(MOD, "handler aborted: {e}"),
                    Err(_) => logln!(MOD, "handler timed out after {}s", HANDLER_TIMEOUT.as_secs()),
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rupiah_and_admin_fee() {
        assert_eq!(rupiah(1_234_567.0), "1.234.567");
        assert_eq!(calc_admin("400.000"), (5000, "5.000".to_string()));
        assert_eq!(calc_admin("1.500.000"), (10_000, "10.000".to_string()));
        assert_eq!(round_token_nominal("103.200"), "100.000");
        assert_eq!(round_token_nominal("107.600"), "107.600");
    }

    #[test]
    fn pads_receipt_columns() {
        assert_eq!(pad_r("NOMINAL", "Rp 100.000", 32).chars().count(), 32);
        // pad_c only left-pads (same as the JS): floor((32-5)/2) spaces + TOKEN
        assert_eq!(pad_c("TOKEN", 32), format!("{}TOKEN", " ".repeat(13)));
        assert_eq!(truncate_middle("abcdefghij", 4), "abcd");
        assert_eq!(adjust_to_wita("23:59:59"), "00:59:59");
        assert_eq!(adjust_to_wita("-"), "-");
    }

    #[test]
    fn builds_pln_receipt() {
        let data = Struk {
            kind: "pln".into(),
            tanggal: "2026-10-27".into(),
            waktu: "10:00:00".into(),
            nominal: "103.200".into(),
            id_pel: "12345".into(),
            no_meter: "999".into(),
            nama: "BUDI".into(),
            tarif: "R1/1300VA".into(),
            jml_kwh: "70.5".into(),
            token: "1234 5678 9012 3456 7890".into(),
            bank_tujuan: String::new(),
            no_rekening: String::new(),
            penerima: String::new(),
            provider_tujuan: String::new(),
            no_provider: String::new(),
        };
        let struk = build_struk(&data);
        assert!(!struk.contains("STRUK PEMBELIAN TOKEN LISTRIK"));
        assert!(struk.contains("TANGGAL"));
        assert!(struk.contains("1234 5678 9012"));
        assert!(struk.contains("TOTAL"));
        assert!(struk.contains("TERIMA KASIH"));
    }

    #[test]
    fn renders_struk_png() {
        std::env::set_var("STORE_NAME", "NORIKIMIKA STORE");
        std::env::set_var("STORE_ADDRESS", "Jl. Contoh No. 404, Jakarta");
        for kind in ["transfer", "pln", "pulsa"] {
            let struk = build_struk(&sample_struk(kind));
            let png = render_struk_image(&struk).expect("render struk");
            assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "{kind}: not a PNG");
            assert!(png.len() > 2000, "{kind}: PNG suspiciously small");
            let path = std::env::temp_dir().join(format!("struk-{kind}.png"));
            std::fs::write(&path, &png).expect("write png");
            println!("{kind} -> {} ({} bytes)", path.display(), png.len());
        }
    }

    fn sample_struk(kind: &str) -> Struk {
        let (bank, rek, penerima, provider, no_provider) = if kind == "transfer" {
            ("DANA".to_string(), "081234567890".to_string(), "BUDI SANTOSO".to_string(), String::new(), String::new())
        } else if kind == "pulsa" {
            (String::new(), String::new(), String::new(), "SIMPATI".to_string(), "081298765432".to_string())
        } else {
            (String::new(), String::new(), String::new(), String::new(), String::new())
        };
        Struk {
            kind: kind.into(),
            tanggal: "2026-10-27".into(),
            waktu: "10:15:30".into(),
            nominal: "103.200".into(),
            id_pel: "12345678901".into(),
            no_meter: "99887766".into(),
            nama: "BUDI SANTOSO".into(),
            tarif: "R1/1300VA".into(),
            jml_kwh: "70.5".into(),
            token: "1234 5678 9012 3456 7890".into(),
            bank_tujuan: bank,
            no_rekening: rek,
            penerima,
            provider_tujuan: provider,
            no_provider,
        }
    }

    #[test]
    fn extracts_receipt_json_from_noise() {
        let noisy = "User Safety: safe\n{\"type\":\"transfer\",\"nominal\":\"50.000\"}<|im_end|>";
        let parsed = extract_receipt_json(noisy).expect("json extracted");
        assert_eq!(parsed["type"], "transfer");
        assert_eq!(parsed["nominal"], "50.000");
        assert!(extract_receipt_json("no json here").is_none());
    }

    #[test]
    fn parses_commands() {
        assert_eq!(command_of("/music@bot lofi beats"), ("music".to_string(), "lofi beats".to_string()));
        assert_eq!(command_of("/p kopi"), ("p".to_string(), "kopi".to_string()));
        assert_eq!(command_of("/speed"), ("speed".to_string(), String::new()));
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }
}

#[cfg(test)]
mod offset_tests {
    use super::*;

    /// A reboot in the middle of a command must not replay that command once the bot is back.
    #[test]
    fn offset_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("norikimika-offset-{}", std::process::id()));
        let path = dir.join("telegram.json");
        assert_eq!(read_offset(&path), None, "no state file yet");
        write_offset(&path, 4242).expect("write offset");
        assert_eq!(read_offset(&path), Some(4242), "first write must be readable");
        write_offset(&path, 4243).expect("rewrite offset");
        assert_eq!(read_offset(&path), Some(4243), "restart resumes at the newest offset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Renaming the file must not throw away the saved position.
    #[test]
    fn old_offset_file_is_carried_over() {
        let dir = std::env::temp_dir().join(format!("norikimika-offset-migrate-{}", std::process::id()));
        let new_path = dir.join("telegram.json");
        let legacy = dir.join("telegram-offset.json");
        write_offset(&legacy, 777).expect("write legacy offset");
        assert_eq!(load_offset_from(&new_path, &legacy), Some(777));
        assert_eq!(read_offset(&new_path), Some(777), "offset must land in the new file");
        assert!(!legacy.exists(), "old file is gone once carried over");
        assert_eq!(load_offset_from(&new_path, &legacy), Some(777), "second boot reads the new file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod mime_tests {
    use super::*;

    /// A stale button press is not a handler failure.
    #[test]
    fn an_expired_callback_is_recognised() {
        assert!(is_expired_query(
            "telegram answerCallbackQuery failed: {\"description\":\"Bad Request: query is too old \
and response timeout expired or query ID is invalid\"}"
        ));
        assert!(!is_expired_query("telegram sendMessage failed: chat not found"));
    }

    /// yt-dlp lists the webp variant last, and `thumbnail` points at it: picking it gives a file
    /// this build cannot decode, which is how the cover disappeared.
    #[test]
    fn the_webp_thumbnail_url_is_never_chosen() {
        let info = json!({
            "thumbnail": "https://i.ytimg.com/vi_webp/abc/maxresdefault.webp",
            "thumbnails": [
                { "url": "https://i.ytimg.com/vi/abc/hqdefault.jpg" },
                { "url": "https://i.ytimg.com/vi/abc/maxresdefault.jpg" },
                { "url": "https://i.ytimg.com/vi_webp/abc/maxresdefault.webp" }
            ]
        });
        assert_eq!(
            jpg_thumbnail_url(&info),
            Some("https://i.ytimg.com/vi/abc/maxresdefault.jpg"),
            "the largest jpg wins"
        );
        assert_eq!(jpg_thumbnail_url(&json!({ "thumbnail": "https://x/y.webp" })), None);
    }


}

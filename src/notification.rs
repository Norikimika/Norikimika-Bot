//! Port of notification.js: watches dumpsys notification for a few packages,
//! forwards them to Telegram topics, announces GoPay ones with TTS and clears
//! the shade when it fills up.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

use crate::config;
use crate::state::State;

const MOD: &str = "notification";

const TTS_MAX_ATTEMPTS: u32 = 3;
const TTS_RETRY_BASE_MS: u64 = 800;
const TTS_TIMEOUT_MS: u64 = 25_000;
const TTS_DEFAULT_MODEL: &str = "edge-tts/id-ID-GadisNeural";

const TELEGRAM_RETRY: u32 = 3;
const TELEGRAM_RETRY_BACKOFF: u32 = 2;

// ====================== TTS endpoint ======================

/// Accepts `http://host:port`, `.../v1` or a full endpoint.
fn build_speech_endpoint(raw: &str) -> String {
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    if base.ends_with("/audio/speech") {
        return base.to_string();
    }
    if base.ends_with("/v1") {
        format!("{base}/audio/speech")
    } else {
        format!("{base}/v1/audio/speech")
    }
}

#[derive(Debug)]
enum TtsError {
    /// HTTP status, kept so the retry layer can tell 429/5xx from 400/401.
    Http(u16, String),
    Other(String),
}

impl std::fmt::Display for TtsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TtsError::Http(code, detail) => {
                if detail.is_empty() {
                    write!(f, "Speech request failed (HTTP {code})")
                } else {
                    write!(f, "Speech request failed (HTTP {code}): {detail}")
                }
            }
            TtsError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

fn is_retryable_tts_error(e: &TtsError) -> bool {
    match e {
        TtsError::Http(code, _) => *code == 429 || *code >= 500,
        TtsError::Other(msg) => {
            let m = msg.to_lowercase();
            [
                "econnreset",
                "econnrefused",
                "epipe",
                "socket hang up",
                "eai_again",
                "enetunreach",
                "etimedout",
                "fetch failed",
                "network",
            ]
            .iter()
            .any(|needle| m.contains(needle))
        }
    }
}

async fn tts_to_file(text: &str, audio_path: &Path, timeout_ms: u64) -> Result<(), TtsError> {
    let endpoint = build_speech_endpoint(&config::get("NINE_ROUTER_URL").unwrap_or_default());
    if endpoint.is_empty() {
        return Err(TtsError::Other(
            "NINE_ROUTER_URL is missing; cannot synthesize speech".into(),
        ));
    }
    let api_key = config::get("NINE_ROUTER_API_KEY")
        .ok_or_else(|| TtsError::Other("NINE_ROUTER_API_KEY is missing; cannot synthesize speech".into()))?;
    let model = config::get("NINE_ROUTER_TTS_MODEL").unwrap_or_else(|| TTS_DEFAULT_MODEL.to_string());

    let client = crate::util::http()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| TtsError::Other(e.to_string()))?;

    let res = client
        .post(&endpoint)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({ "model": model, "input": text }))
        .send()
        .await
        .map_err(|e| TtsError::Other(e.to_string()))?;

    let status = res.status();
    if !status.is_success() {
        let detail = res.text().await.unwrap_or_default();
        let detail: String = detail.trim().chars().take(200).collect();
        return Err(TtsError::Http(status.as_u16(), detail));
    }

    let audio = res
        .bytes()
        .await
        .map_err(|e| TtsError::Other(e.to_string()))?;
    if audio.is_empty() {
        return Err(TtsError::Other("Speech endpoint returned an empty body".into()));
    }
    std::fs::write(audio_path, &audio).map_err(|e| TtsError::Other(e.to_string()))
}

/// Exponential backoff with jitter: throttles arrive in bursts.
fn jittered_backoff(base_ms: u64, attempt: u32) -> u64 {
    let backoff = base_ms.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    let frac = (now_ms() as u64 % 1_000) as f64 / 1_000.0;
    (backoff as f64 * (0.5 + frac)).round() as u64
}

async fn tts_to_file_with_retry(text: &str, audio_path: &Path, timeout_ms: u64) -> Result<(), TtsError> {
    let mut attempt = 1;
    loop {
        let err = match tts_to_file(text, audio_path, timeout_ms).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if attempt >= TTS_MAX_ATTEMPTS || !is_retryable_tts_error(&err) {
            return Err(err);
        }
        let delay = jittered_backoff(TTS_RETRY_BASE_MS, attempt);
        logln!(
            MOD,
            "TTS attempt {attempt}/{TTS_MAX_ATTEMPTS} failed ({err}); retrying in {delay}ms"
        );
        tokio::time::sleep(Duration::from_millis(delay)).await;
        attempt += 1;
    }
}

// ====================== config ======================

struct App {
    pkg: &'static str,
    name: &'static str,
    topic: i64,
    keywords: Vec<&'static str>,
    tts: bool,
}

struct Config {
    token: String,
    chat_id: i64,
    apps: Vec<App>,
    check_interval: u64,
    max_notifications: i64,
    auto_clear: bool,
    db_cleanup_cycles: i64,
    db_retention: i64,
    dedupe_window: i64,
    pkill_throttle: i64,
}

fn app(pkg: &'static str, name: &'static str, topic_key: &str, default_topic: i64, keywords: Vec<&'static str>, tts: bool) -> App {
    App {
        pkg,
        name,
        topic: config::topic(topic_key).unwrap_or(default_topic),
        keywords,
        tts,
    }
}

fn load_config() -> Result<Config> {
    let token = config::req("BOT_TOKEN")?;
    let chat_id: i64 = config::req("CHAT_ID")?
        .parse()
        .map_err(|_| anyhow!("CHAT_ID is not a valid integer"))?;
    if token.len() < 10 {
        return Err(anyhow!("Telegram bot token looks invalid"));
    }

    let auto_clear = matches!(
        config::get("AUTO_CLEAR_ENABLED").unwrap_or_else(|| "true".into()).to_lowercase().as_str(),
        "1" | "true" | "yes"
    );

    Ok(Config {
        token,
        chat_id,
        apps: vec![
            app("com.macrovideo.v380pro", "V380 Pro", "TOPIC_ID_V380", 15208, vec![], false),
            app("com.tuya.smart", "Tuya", "TOPIC_ID_TUYA", 15208, vec![], false),
            app("com.andi.alquran.id", "Al-Qur'an Indonesia", "TOPIC_ID_AL_QURAN", 34084, vec![], false),
            app("com.gojek.gopay", "GoPay", "TOPIC_ID_GOPAY", 15210, vec!["Norikimika"], true),
        ],
        check_interval: config::int("CHECK_INTERVAL", 2).max(1) as u64,
        max_notifications: config::int("MAX_NOTIFICATIONS", 49),
        auto_clear,
        db_cleanup_cycles: config::int("DB_CLEANUP_CYCLES", 180).max(1),
        db_retention: config::int("DB_RETENTION_SECONDS", 24 * 3600),
        dedupe_window: config::int("DEDUPE_WINDOW_SECONDS", 300),
        pkill_throttle: config::int("PKILL_THROTTLE_SECONDS", 300),
    })
}

// ====================== storage ======================

#[derive(Serialize, Deserialize, Clone, Default)]
struct Entry {
    package_name: String,
    title: String,
    message_text: String,
    notification_time: i64,
    sent_time: i64,
}

fn state_path() -> PathBuf {
    config::state_file("notification.json")
}

fn load_entries() -> Vec<Entry> {
    let st = State::load("notification.json");
    st.map
        .get("entries")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn save_entries(entries: &[Entry]) -> Result<()> {
    let mut st = State::load("notification.json");
    st.map.insert("entries".to_string(), serde_json::to_value(entries)?);
    st.save()
}

fn same_notification(n: &Entry, pkg: &str, title: &str, text: &str, notif_time: i64, window_ms: i64) -> bool {
    if n.package_name != pkg || n.title != title || n.message_text != text {
        return false;
    }
    if n.notification_time == notif_time {
        return true;
    }
    (n.notification_time - notif_time).abs() <= window_ms
}

// ====================== dumpsys parsing ======================

/// One dumpsys scan can list the same notification twice (group + child).
fn split_blocks(out: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in out.lines() {
        if line.contains("NotificationRecord(") {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
            }
            current = vec![line];
        } else if !current.is_empty() {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    blocks
}

/// First occurrence of `marker`, value runs to the end of that line.
fn field_rest(block: &str, marker: &str) -> Option<String> {
    block
        .lines()
        .find_map(|line| line.find(marker).map(|i| line[i + marker.len()..].to_string()))
}

/// `pkg=` value stops at whitespace or `]`.
fn field_token(block: &str, marker: &str) -> Option<String> {
    field_rest(block, marker).map(|rest| {
        rest.chars()
            .take_while(|c| !c.is_whitespace() && *c != ']')
            .collect()
    })
}

fn digits_after(block: &str, marker: &str) -> Option<i64> {
    let rest = field_rest(block, marker)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn strip_entities(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '&' {
            if let Some(off) = (i + 1..c.len()).find(|&j| c[j] == ';') {
                if off > i + 1 {
                    i = off + 1;
                    continue;
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

fn unwrap_string_calls(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        if c[i..].starts_with(&['S', 't', 'r', 'i', 'n', 'g']) {
            let mut j = i + 6;
            while j < c.len() && c[j].is_whitespace() {
                j += 1;
            }
            if j < c.len() && c[j] == '(' {
                if let Some(k) = (j + 1..c.len()).find(|&x| c[x] == ')') {
                    if k > j + 1 {
                        out.extend(&c[j + 1..k]);
                        i = k + 1;
                        continue;
                    }
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

fn clean_text(text: &str) -> String {
    let s = strip_entities(text);
    let s = s.replace("canonical=1", "");
    unwrap_string_calls(&s).trim().to_string()
}

fn time_str(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| chrono::Local.timestamp_millis_opt(0).unwrap())
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn format_message(n: &Found, app_name: &str) -> String {
    // title/text come from arbitrary app notifications and are sent with parse_mode=HTML:
    // unescaped "&" or "<" broke the whole message (and let an app inject markup).
    format!(
        "<b>🔔 New {} Notification</b>\n\
         <blockquote>⏰ Time: {}\n\
         📝 Title: {}\n\
         💬 Text: {}</blockquote>",
        crate::util::escape_html(app_name),
        n.time_str,
        crate::util::escape_html(&n.title),
        crate::util::escape_html(&n.text)
    )
}

#[derive(Debug, Clone)]
struct Found {
    pkg: String,
    title: String,
    text: String,
    timestamp: i64,
    time_str: String,
}

// ====================== process helpers ======================

async fn exec(program: &str, args: &[&str], timeout: Duration) -> (bool, String, String) {
    let fut = Command::new(program).args(args).kill_on_drop(true).output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(out)) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Ok(Err(e)) => (false, String::new(), e.to_string()),
        Err(_) => (false, String::new(), "timed out".to_string()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_secs_f64() -> f64 {
    now_ms() as f64 / 1000.0
}

// ====================== monitor ======================

struct Monitor {
    cfg: Config,
    su: bool,
    last_clear_kill: f64,
    last_clear_time: f64,
    http: reqwest::Client,
}

struct Silence {
    checked: bool,
    players: Vec<String>,
}

struct MediaReport {
    volume: Option<i64>,
}

impl Monitor {
    async fn new(cfg: Config) -> Self {
        let http = crate::util::client();
        Self {
            cfg,
            su: false,
            last_clear_kill: 0.0,
            last_clear_time: 0.0,
            http,
        }
    }

    fn setup_storage(&self) {
        let path = state_path();
        if !path.exists() {
            if let Err(e) = save_entries(&[]) {
                logln!(MOD, "could not create {path:?}: {e}");
            }
            logln!(MOD, "JSON storage initialized (new file)");
            return;
        }
        let st = State::load("notification.json");
        let valid = st.map.get("entries").map(|v| v.is_array()).unwrap_or(false);
        if !valid {
            logln!(MOD, "WARNING JSON file invalid (expected a list), resetting.");
            if let Err(e) = save_entries(&[]) {
                logln!(MOD, "reset failed: {e}");
            }
            return;
        }
        logln!(MOD, "JSON storage loaded ({} entries)", load_entries().len());
    }

    fn is_sent(&self, pkg: &str, title: &str, text: &str, ts: i64) -> bool {
        let window = self.cfg.dedupe_window * 1000;
        load_entries()
            .iter()
            .any(|n| same_notification(n, pkg, title, text, ts, window))
    }

    fn mark_sent(&self, pkg: &str, title: &str, text: &str, ts: i64) {
        let window = self.cfg.dedupe_window * 1000;
        let mut data = load_entries();
        if data.iter().any(|n| same_notification(n, pkg, title, text, ts, window)) {
            return;
        }
        data.push(Entry {
            package_name: pkg.to_string(),
            title: title.to_string(),
            message_text: text.to_string(),
            notification_time: ts,
            sent_time: now_secs_f64() as i64,
        });
        if let Err(e) = save_entries(&data) {
            logln!(MOD, "ERROR mark_notification_sent error: {e}");
        }
    }

    fn cleanup_old(&self) {
        let cutoff = now_secs_f64() as i64 - self.cfg.db_retention;
        let data = load_entries();
        let before = data.len();
        let filtered: Vec<Entry> = data.into_iter().filter(|n| n.sent_time >= cutoff).collect();
        let deleted = before - filtered.len();
        if deleted > 0 {
            if let Err(e) = save_entries(&filtered) {
                logln!(MOD, "ERROR cleanup_old_notifications error: {e}");
                return;
            }
            logln!(MOD, "Cleaned up {deleted} old notifications");
        }
    }

    async fn check_su(&mut self) -> bool {
        let (ok, out, _) = exec("su", &["-c", "id"], Duration::from_secs(5)).await;
        self.su = ok && out.to_lowercase().contains("uid");
        if !self.su {
            logln!(MOD, "WARNING Root (su) not available or not permitted. Some operations may fail.");
        }
        self.su
    }

    async fn execute_root_command(&self, command: &str, timeout: Duration) -> String {
        let (ok, out, err) = if self.su {
            exec("su", &["-c", command], timeout).await
        } else {
            exec("sh", &["-c", command], timeout).await
        };
        if !ok {
            let err = err.trim();
            if err == "timed out" {
                logln!(MOD, "ERROR Command timed out: {command}");
            } else if !err.is_empty() {
                logln!(MOD, "DEBUG Cmd stderr: {err}");
            }
        }
        out
    }

    async fn notification_count(&self) -> i64 {
        let out = self
            .execute_root_command("dumpsys notification 2>/dev/null | grep -c 'NotificationRecord'", Duration::from_secs(10))
            .await;
        let trimmed = out.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
            return trimmed.parse().unwrap_or(0);
        }
        let out2 = self
            .execute_root_command("dumpsys notification 2>/dev/null", Duration::from_secs(10))
            .await;
        out2.matches("NotificationRecord").count() as i64
    }

    async fn clear_all(&mut self) -> bool {
        logln!(MOD, "Attempting to clear notifications (safe mode)");
        let before = self.notification_count().await;

        self.execute_root_command("cmd notification remove_all", Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        if self.notification_count().await < before {
            logln!(MOD, "Cleared notifications via 'cmd notification remove_all'");
            return true;
        }

        self.execute_root_command("service call notification 1", Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        if self.notification_count().await < before {
            logln!(MOD, "Cleared notifications via 'service call notification 1'");
            return true;
        }

        let now = now_secs_f64();
        if now - self.last_clear_kill < self.cfg.pkill_throttle as f64 {
            logln!(MOD, "WARNING Skipping pkill (throttled). Will not kill SystemUI frequently.");
            return false;
        }

        logln!(MOD, "WARNING As a last resort: killing SystemUI (pkill). This may cause UI restart.");
        self.last_clear_kill = now;
        self.execute_root_command("pkill -f com.android.systemui", Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        if self.notification_count().await < before {
            logln!(MOD, "Cleared notifications via pkill SystemUI");
            return true;
        }

        logln!(MOD, "ERROR Failed to clear notifications by available methods");
        false
    }

    async fn check_and_clear(&mut self) -> i64 {
        if !self.cfg.auto_clear {
            return self.notification_count().await;
        }
        let current = self.notification_count().await;
        if current >= self.cfg.max_notifications {
            logln!(
                MOD,
                "WARNING Threshold reached ({current} >= {}). Auto-clearing...",
                self.cfg.max_notifications
            );
            self.clear_all().await;
            return self.notification_count().await;
        }
        current
    }

    async fn scan(&self) -> Vec<Found> {
        let out = self
            .execute_root_command("dumpsys notification --noredact 2>/dev/null", Duration::from_secs(10))
            .await;
        if out.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for blk in split_blocks(&out) {
            let Some(pkg) = field_token(&blk, "pkg=") else { continue };
            let Some(app) = self.cfg.apps.iter().find(|a| a.pkg == pkg) else { continue };

            let Some(title_raw) = field_rest(&blk, "title=") else { continue };
            let title = clean_text(title_raw.trim());
            let text = field_rest(&blk, "text=")
                .map(|t| clean_text(t.trim()))
                .unwrap_or_default();

            if !app.keywords.is_empty() {
                let combined = format!("{title} {text}").to_lowercase();
                if !app
                    .keywords
                    .iter()
                    .any(|kw| combined.contains(&kw.to_lowercase()))
                {
                    continue;
                }
            }

            let timestamp = digits_after(&blk, "when=").unwrap_or(0);

            let batch_key = format!("{pkg}\u{0}{title}\u{0}{text}");
            if seen.contains(&batch_key) {
                continue;
            }
            if self.is_sent(&pkg, &title, &text, timestamp) {
                continue;
            }
            seen.push(batch_key);

            results.push(Found {
                pkg,
                title,
                text,
                timestamp,
                time_str: time_str(timestamp),
            });
        }
        results
    }

    async fn send_telegram(&self, message: &str, topic: Option<i64>) -> bool {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.cfg.token);
        let mut payload = json!({
            "chat_id": self.cfg.chat_id,
            "text": message,
            "parse_mode": "HTML",
        });
        if let Some(t) = topic {
            if t != 0 {
                payload["message_thread_id"] = json!(t);
            }
        }

        let mut attempt = 0;
        while attempt < TELEGRAM_RETRY {
            attempt += 1;
            match self
                .http
                .post(&url)
                .json(&payload)
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(res) => {
                    let status = res.status().as_u16();
                    if status == 200 {
                        return true;
                    }
                    if status == 429 {
                        let retry_after = res
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(5);
                        logln!(MOD, "WARNING Telegram rate limited. Retry after {retry_after}s");
                        tokio::time::sleep(Duration::from_secs(retry_after)).await;
                    } else {
                        let body = res.text().await.unwrap_or_default();
                        logln!(MOD, "WARNING Telegram send failed (status {status}): {body}");
                        let secs = TELEGRAM_RETRY_BACKOFF.pow(attempt) as u64;
                        tokio::time::sleep(Duration::from_secs(secs)).await;
                    }
                }
                Err(e) => {
                    logln!(MOD, "WARNING Telegram request error: {e}. Backing off.");
                    let secs = TELEGRAM_RETRY_BACKOFF.pow(attempt) as u64;
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
            }
        }
        logln!(MOD, "ERROR Telegram send failed after retries");
        false
    }

    /// Volume first, then the pause: setting the volume afterwards leaves the
    /// announcement playing for a second at the old level.
    async fn stop_media_playback(&self) -> MediaReport {
        let mut report = MediaReport { volume: None };

        let (ok, out, err) = exec("termux-volume", &[], Duration::from_secs(5)).await;
        if !ok {
            logln!(MOD, "ERROR Failed to set music volume: {}", err.trim());
        } else if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(&out) {
            let music = items.iter().find(|v| {
                v.get("stream")
                    .and_then(|s| s.as_str())
                    .map(|s| s.eq_ignore_ascii_case("music"))
                    .unwrap_or(false)
            });
            if let Some(max) = music
                .and_then(|m| m.get("max_volume"))
                .and_then(|v| v.as_i64())
                .filter(|m| *m > 0)
            {
                let max_s = max.to_string();
                let (ok, _, err) = exec("termux-volume", &["music", &max_s], Duration::from_secs(5)).await;
                if ok {
                    report.volume = Some(max);
                    logln!(MOD, "Music volume set to max ({max})");
                } else {
                    logln!(MOD, "ERROR Failed to set music volume: {}", err.trim());
                }
            }
        }

        let (ok, _, err) = exec("cmd", &["media_session", "dispatch", "pause"], Duration::from_secs(10)).await;
        if ok {
            logln!(MOD, "Media playback paused");
        } else {
            logln!(MOD, "ERROR Failed to stop media playback: {}", err.trim());
        }

        report
    }

    /// Poll until the music stream has no started player, or the deadline passes.
    async fn wait_for_silence(&self, timeout_ms: u64) -> Silence {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            let (ok, out, err) = exec("su", &["-c", "dumpsys audio"], Duration::from_secs(5)).await;
            if !ok {
                logln!(MOD, "WARNING Audio state unavailable: {}", err.trim());
                return Silence { checked: false, players: Vec::new() };
            }
            last = out
                .lines()
                .filter(|l| l.contains("state:started") && l.contains("STREAM_MUSIC"))
                .map(|l| l.trim().chars().take(120).collect())
                .collect();
            if last.is_empty() {
                return Silence { checked: true, players: Vec::new() };
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Silence { checked: true, players: last }
    }

    async fn speak(&self, title: &str, text: &str) {
        let speech = format!("{title}. {text}");
        logln!(MOD, "TTS: {speech}");

        let audio = std::env::temp_dir().join(format!("tts_{}.mp3", now_ms()));
        let gen = {
            let speech = speech.clone();
            let audio = audio.clone();
            tokio::spawn(async move { tts_to_file_with_retry(&speech, &audio, TTS_TIMEOUT_MS).await })
        };

        let report = self.stop_media_playback().await;
        let silence = self.wait_for_silence(4000).await;
        if !silence.checked {
            logln!(MOD, "WARNING Audio state unavailable; speaking without silence check");
        } else if !silence.players.is_empty() {
            logln!(
                MOD,
                "WARNING Music still playing after pause ({}); announcement may be masked",
                silence.players.len()
            );
            for p in &silence.players {
                logln!(MOD, "  still playing: {p}");
            }
        } else {
            logln!(MOD, "Music stream idle before speaking");
        }

        let t0 = Instant::now();
        let audio_ready = match gen.await {
            Ok(Ok(())) if audio.exists() => true,
            Ok(Err(e)) => {
                logln!(MOD, "WARNING TTS prefetch failed: {e}; fallback termux-tts");
                let _ = std::fs::remove_file(&audio);
                false
            }
            _ => false,
        };

        let mut spoke = false;
        if audio_ready {
            spoke = play_file(&audio).await;
        }
        if !spoke {
            spoke = fallback_tts(&speech).await;
        }
        logln!(
            MOD,
            "TTS: {} after {}ms (volume {})",
            if spoke { "ok" } else { "failed" },
            t0.elapsed().as_millis(),
            report.volume.map(|v| v.to_string()).unwrap_or_else(|| "unset".into())
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    async fn monitor(&mut self) -> Result<()> {
        logln!(MOD, "Notification Monitor Started");
        logln!(
            MOD,
            "Auto-clear: {} (threshold {})",
            self.cfg.auto_clear,
            self.cfg.max_notifications
        );
        for a in &self.cfg.apps {
            logln!(MOD, "{} ({}) - topic {}", a.name, a.pkg, a.topic);
        }
        self.check_su().await;

        let mut cycle: u64 = 0;
        loop {
            cycle += 1;
            if cycle % self.cfg.db_cleanup_cycles as u64 == 0 {
                self.cleanup_old();
            }

            let found = self.scan().await;
            if !found.is_empty() {
                logln!(MOD, "Found {} new notification(s)", found.len());
                for n in &found {
                    let Some(app) = self.cfg.apps.iter().find(|a| a.pkg == n.pkg) else { continue };
                    logln!(MOD, "Sending: {} - {}", app.name, n.title);
                    let message = format_message(n, app.name);

                    if self.send_telegram(&message, Some(app.topic)).await {
                        self.mark_sent(&n.pkg, &n.title, &n.text, n.timestamp);
                        logln!(MOD, "Sent & marked");
                    } else {
                        logln!(MOD, "WARNING Failed to send to Telegram");
                    }

                    if app.tts {
                        self.speak(&n.title, &n.text).await;
                    }
                }
            }

            let now = now_secs_f64();
            if cycle % 10 == 0 || (self.cfg.auto_clear && now - self.last_clear_time > 30.0) {
                let _count = self.check_and_clear().await;
                self.last_clear_time = now;
            }

            tokio::time::sleep(Duration::from_secs(self.cfg.check_interval)).await;
        }
    }
}

async fn play_file(audio: &Path) -> bool {
    let path = audio.to_string_lossy().to_string();
    let (ok, _, err) = exec("termux-media-player", &["play", &path], Duration::from_secs(20)).await;
    let _ = std::fs::remove_file(audio);
    if !ok {
        logln!(MOD, "WARNING TTS play failed: {}", err.trim());
    }
    ok
}

async fn fallback_tts(speech: &str) -> bool {
    // -s music matches this device's usage=USAGE_MEDIA (STREAM_MUSIC=3).
    let (ok, _, err) = exec(
        "termux-tts-speak",
        &["-l", "id", "-s", "music", speech],
        Duration::from_secs(20),
    )
    .await;
    if !ok {
        logln!(MOD, "WARNING TTS fallback failed: {}", err.trim());
    }
    ok
}

pub async fn run() -> Result<()> {
    logln!(MOD, "Initializing Notification Monitor Service...");
    logln!(MOD, "Validating configuration");
    let cfg = load_config()?;
    logln!(MOD, "Configuration OK");
    let mut monitor = Monitor::new(cfg).await;
    monitor.setup_storage();
    monitor.monitor().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_html_is_escaped() {
        // notification text is arbitrary app output and goes out with parse_mode=HTML
        let n = Found {
            pkg: "com.example".into(),
            title: "A & B <b>".into(),
            text: "1 < 2 & 3 > 2".into(),
            timestamp: 0,
            time_str: "2026-10-29 00:00:00".into(),
        };
        let msg = format_message(&n, "GoPay");
        assert!(msg.contains("A &amp; B &lt;b&gt;"), "{msg}");
        assert!(msg.contains("1 &lt; 2 &amp; 3 &gt; 2"), "{msg}");
    }

    #[test]
    fn speech_endpoint_shapes() {
        assert_eq!(build_speech_endpoint("http://127.0.0.1:20128"), "http://127.0.0.1:20128/v1/audio/speech");
        assert_eq!(build_speech_endpoint("http://h:1/v1/"), "http://h:1/v1/audio/speech");
        assert_eq!(
            build_speech_endpoint("http://h:1/v1/audio/speech"),
            "http://h:1/v1/audio/speech"
        );
        assert_eq!(build_speech_endpoint("   "), "");
    }

    #[test]
    fn cleans_dumpsys_text() {
        assert_eq!(clean_text("String (Hello &amp; bye)  "), "Hello  bye");
        assert_eq!(clean_text("total=3 canonical=1"), "total=3");
        assert_eq!(clean_text("String(42)"), "42");
        assert_eq!(clean_text("plain"), "plain");
    }

    #[test]
    fn splits_notification_blocks() {
        let out = "header\nNotificationRecord(1)\n  pkg=a title=x\nNotificationRecord(2)\n  pkg=b\n";
        let blocks = split_blocks(out);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("pkg=a"));
        assert!(blocks[1].contains("pkg=b"));
    }

    #[test]
    fn extracts_fields() {
        let blk = "NotificationRecord(1)\n  pkg=com.gojek.gopay user=0\n  when=1750000000123\n  android.title=String (QRIS)\n  android.text=String (Pay now)";
        assert_eq!(field_token(blk, "pkg=").as_deref(), Some("com.gojek.gopay"));
        assert_eq!(field_rest(blk, "title=").as_deref(), Some("String (QRIS)"));
        assert_eq!(field_rest(blk, "text=").as_deref(), Some("String (Pay now)"));
        assert_eq!(digits_after(blk, "when="), Some(1750000000123));
        assert_eq!(field_token("nope", "pkg="), None);
    }

    #[test]
    fn dedupe_window() {
        let e = Entry {
            package_name: "p".into(),
            title: "t".into(),
            message_text: "m".into(),
            notification_time: 1_000_000,
            sent_time: 0,
        };
        assert!(same_notification(&e, "p", "t", "m", 1_000_000, 300_000));
        assert!(same_notification(&e, "p", "t", "m", 1_100_000, 300_000));
        assert!(!same_notification(&e, "p", "t", "m", 1_400_001, 300_000));
        assert!(!same_notification(&e, "p", "t", "other", 1_000_000, 300_000));
    }

    #[test]
    fn formats_message() {
        let n = Found {
            pkg: "p".into(),
            title: "GoPay".into(),
            text: "Rp 10.000".into(),
            timestamp: 0,
            time_str: "2026-10-27 10:00:00".into(),
        };
        assert_eq!(
            format_message(&n, "GoPay"),
            "<b>🔔 New GoPay Notification</b>\n<blockquote>⏰ Time: 2026-10-27 10:00:00\n📝 Title: GoPay\n💬 Text: Rp 10.000</blockquote>"
        );
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_tts_error(&TtsError::Http(429, String::new())));
        assert!(is_retryable_tts_error(&TtsError::Http(503, String::new())));
        assert!(!is_retryable_tts_error(&TtsError::Http(401, String::new())));
        assert!(is_retryable_tts_error(&TtsError::Other("fetch failed".into())));
        assert!(!is_retryable_tts_error(&TtsError::Other("bad model".into())));
    }
}

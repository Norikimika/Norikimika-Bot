//! Android notification monitor ported from notification.js.
//!
//! Polls `dumpsys notification --noredact`, forwards new notifications of the
//! watched packages to their Telegram topics, optionally announces GoPay ones
//! through 9router TTS (termux-tts-speak fallback), and trims the on-disk
//! dedupe DB.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::app::App;
use crate::config::Config;
use crate::util::{backup_dir, log_to, now_ms, run_timeout};

const TTS_MAX_ATTEMPTS: usize = 3;
const TTS_RETRY_BASE_MS: f64 = 800.0;
const TTS_TIMEOUT_SECS: u64 = 25;

const PAT_PKG: &str = r"pkg=([^\s\]]+)";
const PAT_TITLE: &str = r"title=([^\n\r]+)";
const PAT_TEXT: &str = r"text=([^\n\r]+)";
const PAT_WHEN: &str = r"when=(\d+)";
const PAT_ENTITY: &str = r"&[^;]+;";
const PAT_STRING_WRAP: &str = r"String\s*\(([^)]+)\)";

fn logl(level: &str, msg: impl AsRef<str>) {
    log_to("notification.log", &format!("[{level}] {}", msg.as_ref()));
}

/// config.rs defaults are 0/false; notification.js used its own fallbacks.
fn or(v: i64, default: i64) -> i64 {
    if v > 0 {
        v
    } else {
        default
    }
}

fn re<'a>(cell: &'a OnceLock<regex::Regex>, pattern: &str) -> &'a regex::Regex {
    cell.get_or_init(|| regex::Regex::new(pattern).expect("static regex"))
}

// ====================== TTS ======================

fn build_speech_endpoint(raw_url: &str) -> String {
    let base = raw_url.trim().trim_end_matches('/');
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
struct TtsError {
    status: Option<u16>,
    msg: String,
}

impl TtsError {
    fn new(status: Option<u16>, msg: impl Into<String>) -> Self {
        Self { status, msg: msg.into() }
    }

    /// 429/5xx and dropped sockets are worth another try; 400/401 are not.
    fn retryable(&self) -> bool {
        match self.status {
            Some(s) => s == 429 || s >= 500,
            None => {
                let m = self.msg.to_lowercase();
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
                    "timed out",
                    "error sending request",
                    "connection",
                ]
                .iter()
                .any(|k| m.contains(k))
            }
        }
    }
}

async fn tts_to_file(app: &Arc<App>, text: &str, audio_path: &Path) -> Result<(), TtsError> {
    let endpoint = build_speech_endpoint(app.cfg.nine_router_url.as_deref().unwrap_or(""));
    if endpoint.is_empty() {
        return Err(TtsError::new(None, "NINE_ROUTER_URL is missing; cannot synthesize speech"));
    }
    let key = app
        .cfg
        .nine_router_key
        .as_deref()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| TtsError::new(None, "NINE_ROUTER_API_KEY is missing; cannot synthesize speech"))?;

    let res = app
        .http
        .post(&endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({ "model": app.cfg.nine_router_tts_model, "input": text }))
        .timeout(Duration::from_secs(TTS_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| TtsError::new(None, format!("speech request failed: {e}")))?;

    if !res.status().is_success() {
        let status = res.status().as_u16();
        let detail: String = res.text().await.unwrap_or_default().chars().take(200).collect();
        return Err(TtsError::new(Some(status), format!("HTTP {status}: {}", detail.trim())));
    }

    let audio = res
        .bytes()
        .await
        .map_err(|e| TtsError::new(None, format!("speech body failed: {e}")))?;
    if audio.is_empty() {
        return Err(TtsError::new(None, "Speech endpoint returned an empty body"));
    }
    std::fs::write(audio_path, &audio)
        .map_err(|e| TtsError::new(None, format!("write {} failed: {e}", audio_path.display())))
}

/// Bounded retry with jittered exponential backoff (throttles arrive in bursts).
async fn tts_to_file_with_retry(app: &Arc<App>, text: &str, audio_path: &Path) -> Result<(), TtsError> {
    let mut last: Option<TtsError> = None;
    for attempt in 1..=TTS_MAX_ATTEMPTS {
        match tts_to_file(app, text, audio_path).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retry = attempt < TTS_MAX_ATTEMPTS && e.retryable();
                if retry {
                    let backoff = TTS_RETRY_BASE_MS * 2f64.powi(attempt as i32 - 1);
                    let delay = (backoff * (0.5 + rand::random::<f64>())).round() as u64;
                    logl(
                        "WARNING",
                        format!(
                            "TTS attempt {attempt}/{TTS_MAX_ATTEMPTS} failed ({}); retrying in {delay}ms",
                            e.msg
                        ),
                    );
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| TtsError::new(None, "TTS failed")))
}

// ====================== STORAGE ======================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    package_name: String,
    title: String,
    message_text: String,
    #[serde(default)]
    notification_time: i64,
    #[serde(default)]
    sent_time: i64,
}

#[derive(Debug, Clone)]
struct Found {
    pkg: String,
    title: String,
    text: String,
    timestamp: i64,
    time_str: String,
}

struct Monitored {
    pkg: &'static str,
    name: &'static str,
    topic: Option<i64>,
    keywords: &'static [&'static str],
    tts: bool,
}

fn monitored_packages(cfg: &Config) -> Vec<Monitored> {
    vec![
        Monitored {
            pkg: "com.macrovideo.v380pro",
            name: "V380 Pro",
            topic: Some(cfg.topic_v380.unwrap_or(15208)),
            keywords: &[],
            tts: false,
        },
        Monitored {
            pkg: "com.tuya.smart",
            name: "Tuya",
            topic: Some(cfg.topic_tuya.unwrap_or(15208)),
            keywords: &[],
            tts: false,
        },
        Monitored {
            pkg: "com.andi.alquran.id",
            name: "Al-Qur'an Indonesia",
            topic: Some(cfg.topic_al_quran.unwrap_or(34084)),
            keywords: &[],
            tts: false,
        },
        Monitored {
            pkg: "com.gojek.gopay",
            name: "GoPay",
            topic: Some(cfg.topic_gopay.unwrap_or(15210)),
            keywords: &["Norikimika"],
            tts: true,
        },
    ]
}

enum Silence {
    Unchecked,
    Idle,
    Busy(Vec<String>),
}

struct MediaReport {
    volume: Option<i64>,
}

// ====================== MONITOR ======================

struct Monitor {
    app: Arc<App>,
    json: PathBuf,
    dedupe_window_seconds: i64,
    retention_seconds: i64,
    cleanup_cycles: i64,
    interval_seconds: i64,
    max_notifications: i64,
    auto_clear: bool,
    pkill_throttle_seconds: f64,
    last_clear_kill: f64,
    last_clear_time: f64,
    su_available: Option<bool>,
    tts_queue: Mutex<()>,
}

impl Monitor {
    fn new(app: Arc<App>) -> Self {
        let cfg = &app.cfg;
        Self {
            dedupe_window_seconds: or(cfg.dedupe_window_seconds, 300),
            retention_seconds: or(cfg.db_retention_seconds, 24 * 3600),
            cleanup_cycles: or(cfg.db_cleanup_cycles, 180),
            interval_seconds: or(cfg.check_interval, 2),
            max_notifications: or(cfg.max_notifications, 49),
            auto_clear: cfg.auto_clear_enabled,
            pkill_throttle_seconds: or(cfg.pkill_throttle_seconds, 300) as f64,
            json: backup_dir().join("notification.json"),
            last_clear_kill: 0.0,
            last_clear_time: 0.0,
            su_available: None,
            tts_queue: Mutex::new(()),
            app,
        }
    }

    // ---------- json db ----------

    fn read_json(&self) -> Option<Vec<Stored>> {
        let raw = std::fs::read_to_string(&self.json).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn write_json(&self, data: &[Stored]) {
        let dir = self.json.parent().unwrap_or_else(|| Path::new("."));
        let tmp = dir.join(format!("notif_{}_{}.tmp", now_ms(), rand::random::<u32>()));
        match serde_json::to_string_pretty(data) {
            Ok(body) => {
                if let Err(e) = std::fs::write(&tmp, body) {
                    logl("ERROR", format!("write {} failed: {e}", tmp.display()));
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, &self.json) {
                    logl("ERROR", format!("rename {} failed: {e}", self.json.display()));
                }
            }
            Err(e) => logl("ERROR", format!("serialize failed: {e}")),
        }
    }

    fn setup_storage(&self) {
        if let Some(dir) = self.json.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if !self.json.exists() {
            self.write_json(&[]);
            logl("INFO", "JSON storage initialized (new file)");
            return;
        }
        match self.read_json() {
            Some(v) => logl("INFO", format!("JSON storage loaded ({} entries)", v.len())),
            None => {
                logl("WARNING", "JSON file invalid, resetting.");
                self.write_json(&[]);
            }
        }
    }

    /// Apps repost with a fresh `when`, so same app + title + text inside the
    /// window counts as the same notification.
    fn same_notification(&self, n: &Stored, pkg: &str, title: &str, text: &str, notif_time: i64) -> bool {
        if n.package_name != pkg || n.title != title || n.message_text != text {
            return false;
        }
        if n.notification_time == notif_time {
            return true;
        }
        let window_ms = self.dedupe_window_seconds * 1000;
        (n.notification_time - notif_time).abs() <= window_ms
    }

    fn is_sent(&self, pkg: &str, title: &str, text: &str, notif_time: i64) -> bool {
        match self.read_json() {
            Some(data) => data.iter().any(|n| self.same_notification(n, pkg, title, text, notif_time)),
            None => false,
        }
    }

    fn mark_sent(&self, pkg: &str, title: &str, text: &str, notif_time: i64) {
        let Some(mut data) = self.read_json() else {
            logl("ERROR", "mark_notification_sent: unreadable db");
            return;
        };
        if data.iter().any(|n| self.same_notification(n, pkg, title, text, notif_time)) {
            return;
        }
        data.push(Stored {
            package_name: pkg.to_string(),
            title: title.to_string(),
            message_text: text.to_string(),
            notification_time: notif_time,
            sent_time: chrono::Utc::now().timestamp(),
        });
        self.write_json(&data);
    }

    fn cleanup_old_notifications(&self) {
        let cutoff = chrono::Utc::now().timestamp() - self.retention_seconds;
        let Some(data) = self.read_json() else { return };
        let before = data.len();
        let kept: Vec<Stored> = data.into_iter().filter(|n| n.sent_time >= cutoff).collect();
        let deleted = before - kept.len();
        if deleted > 0 {
            self.write_json(&kept);
            logl("INFO", format!("Cleaned up {deleted} old notifications"));
        }
    }

    // ---------- shell ----------

    async fn check_su(&mut self) -> bool {
        if let Some(v) = self.su_available {
            return v;
        }
        let available = match run_timeout("su", &["-c", "id"], 5).await {
            Ok(o) => o.stdout.to_lowercase().contains("uid"),
            Err(_) => false,
        };
        self.su_available = Some(available);
        if !available {
            logl("WARNING", "Root (su) not available or not permitted. Some operations may fail.");
        }
        available
    }

    async fn root_command(&mut self, command: &str, timeout_secs: u64) -> String {
        let out = if self.check_su().await {
            run_timeout("su", &["-c", command], timeout_secs).await
        } else {
            run_timeout("sh", &["-c", command], timeout_secs).await
        };
        match out {
            Ok(o) => o.stdout,
            Err(e) => {
                logl("DEBUG", format!("Cmd failed: {e}"));
                String::new()
            }
        }
    }

    // ---------- notification tray ----------

    async fn notification_count(&mut self) -> i64 {
        let out = self
            .root_command("dumpsys notification 2>/dev/null | grep -c 'NotificationRecord'", 10)
            .await;
        let trimmed = out.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = trimmed.parse::<i64>() {
                return n;
            }
        }
        let fallback = self.root_command("dumpsys notification 2>/dev/null", 10).await;
        fallback.matches("NotificationRecord").count() as i64
    }

    async fn clear_all_notifications(&mut self) -> bool {
        logl("INFO", "Attempting to clear notifications (safe mode)");
        let before = self.notification_count().await;

        self.root_command("cmd notification remove_all", 10).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        if self.notification_count().await < before {
            logl("INFO", "Cleared notifications via 'cmd notification remove_all'");
            return true;
        }

        self.root_command("service call notification 1", 10).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        if self.notification_count().await < before {
            logl("INFO", "Cleared notifications via 'service call notification 1'");
            return true;
        }

        let now = now_ms() as f64 / 1000.0;
        if now - self.last_clear_kill < self.pkill_throttle_seconds {
            logl("WARNING", "Skipping pkill (throttled). Will not kill SystemUI frequently.");
            return false;
        }

        logl("WARNING", "As a last resort: killing SystemUI (pkill). This may cause UI restart.");
        self.last_clear_kill = now;
        self.root_command("pkill -f com.android.systemui", 10).await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        if self.notification_count().await < before {
            logl("INFO", "Cleared notifications via pkill SystemUI");
            return true;
        }

        logl("ERROR", "Failed to clear notifications by available methods");
        false
    }

    async fn check_and_clear_notifications(&mut self) -> i64 {
        if !self.auto_clear {
            return self.notification_count().await;
        }
        let current = self.notification_count().await;
        if current >= self.max_notifications {
            logl(
                "WARNING",
                format!(
                    "Threshold reached ({current} >= {}). Auto-clearing...",
                    self.max_notifications
                ),
            );
            self.clear_all_notifications().await;
            return self.notification_count().await;
        }
        current
    }

    async fn get_new_notifications(&mut self, packages: &[Monitored]) -> Vec<Found> {
        let out = self.root_command("dumpsys notification --noredact 2>/dev/null", 15).await;
        if out.is_empty() {
            return Vec::new();
        }

        let mut blocks: Vec<String> = Vec::new();
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

        let pkg_re = re(&static_regex::PKG, PAT_PKG);
        let title_re = re(&static_regex::TITLE, PAT_TITLE);
        let text_re = re(&static_regex::TEXT, PAT_TEXT);
        let when_re = re(&static_regex::WHEN, PAT_WHEN);

        let mut results: Vec<Found> = Vec::new();
        let mut seen: HashSet<(String, String, String)> = HashSet::new();

        for blk in &blocks {
            let Some(pkg_cap) = pkg_re.captures(blk) else { continue };
            let pkg = pkg_cap[1].to_string();
            let Some(entry) = packages.iter().find(|p| p.pkg == pkg) else { continue };

            let Some(title_cap) = title_re.captures(blk) else { continue };
            let title = clean_text(title_cap[1].trim());
            let text = text_re
                .captures(blk)
                .map(|c| clean_text(c[1].trim()))
                .unwrap_or_default();

            if !entry.keywords.is_empty() {
                let combined = format!("{title} {text}").to_lowercase();
                if !entry
                    .keywords
                    .iter()
                    .any(|kw| combined.contains(&kw.to_lowercase()))
                {
                    continue;
                }
            }

            let timestamp = when_re
                .captures(blk)
                .and_then(|c| c[1].parse::<i64>().ok())
                .unwrap_or(0);

            let batch_key = (pkg.clone(), title.clone(), text.clone());
            if seen.contains(&batch_key) || self.is_sent(&pkg, &title, &text, timestamp) {
                continue;
            }
            seen.insert(batch_key);

            results.push(Found {
                pkg,
                title,
                text,
                timestamp,
                time_str: fmt_ts(timestamp),
            });
        }
        results
    }

    // ---------- telegram ----------

    async fn send_telegram_message(&self, message: &str, topic_id: Option<i64>) -> bool {
        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.app.cfg.bot_token
        );
        let mut payload = serde_json::json!({
            "chat_id": self.app.cfg.chat_id,
            "text": message,
            "parse_mode": "HTML",
        });
        if let Some(t) = topic_id {
            payload["message_thread_id"] = t.into();
        }

        let mut attempt = 0;
        while attempt < 3 {
            attempt += 1;
            let backoff = 2u64.pow(attempt as u32);
            match self
                .app
                .http
                .post(&url)
                .json(&payload)
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(res) if res.status().as_u16() == 200 => return true,
                Ok(res) if res.status().as_u16() == 429 => {
                    let wait = res
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .unwrap_or(5);
                    logl("WARNING", format!("Telegram rate limited. Retry after {wait}s"));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                Ok(res) => {
                    let status = res.status();
                    let body: String = res.text().await.unwrap_or_default().chars().take(500).collect();
                    logl("WARNING", format!("Telegram send failed (status {status}): {body}"));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
                Err(e) => {
                    logl("WARNING", format!("Telegram request error: {e}. Backing off."));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
        logl("ERROR", "Telegram send failed after retries");
        false
    }

    fn format_message(n: &Found, app_name: &str) -> String {
        format!(
            "<b>🔔 New {app_name} Notification</b>\n<blockquote>⏰ Time: {}\n📝 Title: {}\n💬 Text: {}</blockquote>",
            n.time_str, n.title, n.text
        )
    }

    // ---------- tts pipeline ----------

    async fn speak_notification(&self, title: &str, text: &str) {
        let _guard = self.tts_queue.lock().await;
        let speech = format!("{title}. {text}");
        logl("INFO", format!("TTS: {speech}"));

        // Generate audio in parallel with the silence check.
        let audio_file = std::env::temp_dir().join(format!("tts_{}.mp3", now_ms()));
        let app = self.app.clone();
        let speech_for_gen = speech.clone();
        let path_for_gen = audio_file.clone();
        let gen = tokio::spawn(async move {
            match tts_to_file_with_retry(&app, &speech_for_gen, &path_for_gen).await {
                Ok(()) if path_for_gen.exists() => Some(path_for_gen),
                Ok(()) => None,
                Err(e) => {
                    logl(
                        "WARNING",
                        format!("TTS prefetch failed: {}; fallback termux-tts", e.msg),
                    );
                    let _ = std::fs::remove_file(&path_for_gen);
                    None
                }
            }
        });

        let report = self.stop_media_playback().await;
        match self.wait_for_silence(4000).await {
            Silence::Unchecked => logl("WARNING", "Audio state unavailable; speaking without silence check"),
            Silence::Busy(players) => {
                logl(
                    "WARNING",
                    format!(
                        "Music still playing after pause ({}); announcement may be masked",
                        players.len()
                    ),
                );
                for p in players {
                    logl("INFO", format!("  still playing: {p}"));
                }
            }
            Silence::Idle => logl("INFO", "Music stream idle before speaking"),
        }

        let t0 = now_ms();
        let mut spoke = false;
        if let Some(file) = gen.await.ok().flatten() {
            spoke = play_file(&file).await;
        }
        if !spoke {
            spoke = fallback_tts(&speech).await;
        }
        logl(
            "INFO",
            format!(
                "TTS: {} after {}ms (volume {})",
                if spoke { "ok" } else { "failed" },
                now_ms() - t0,
                report.volume.map(|v| v.to_string()).unwrap_or_else(|| "unset".to_string())
            ),
        );
        // Both players resolve when audio ends, so the queue is already serial.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Volume first, then pause: raising it after the pause left the start of
    /// the announcement audible at the old level.
    async fn stop_media_playback(&self) -> MediaReport {
        let mut report = MediaReport { volume: None };

        match run_timeout("termux-volume", &[], 5).await {
            Ok(o) if o.ok() => match serde_json::from_str::<serde_json::Value>(&o.stdout) {
                Ok(serde_json::Value::Array(items)) => {
                    let music = items.iter().find(|v| {
                        v["stream"]
                            .as_str()
                            .map(|s| s.to_lowercase() == "music")
                            .unwrap_or(false)
                    });
                    if let Some(m) = music {
                        let max = m["max_volume"].as_i64().unwrap_or(0);
                        if max > 0 {
                            let max_str = max.to_string();
                            match run_timeout("termux-volume", &["music", &max_str], 5).await {
                                Ok(_) => {
                                    report.volume = Some(max);
                                    logl("INFO", format!("Music volume set to max ({max})"));
                                }
                                Err(e) => logl("ERROR", format!("Failed to set music volume: {e}")),
                            }
                        }
                    }
                }
                _ => logl("ERROR", "Failed to set music volume: bad termux-volume output"),
            },
            Ok(o) => logl(
                "ERROR",
                format!("Failed to set music volume: termux-volume code {}", o.code),
            ),
            Err(e) => logl("ERROR", format!("Failed to set music volume: {e}")),
        }

        match run_timeout("cmd", &["media_session", "dispatch", "pause"], 10).await {
            Ok(o) if o.ok() => logl("INFO", "Media playback paused"),
            Ok(o) => logl(
                "ERROR",
                format!("Failed to stop media playback: code {} {}", o.code, o.stderr.trim()),
            ),
            Err(e) => logl("ERROR", format!("Failed to stop media playback: {e}")),
        }

        report
    }

    /// Poll dumpsys audio until no started player holds STREAM_MUSIC.
    async fn wait_for_silence(&self, timeout_ms: u64) -> Silence {
        let deadline = now_ms() + timeout_ms as i64;
        let mut last: Vec<String> = Vec::new();
        while now_ms() < deadline {
            let out = match run_timeout("su", &["-c", "dumpsys audio"], 5).await {
                Ok(o) => o.stdout,
                Err(e) => {
                    logl("WARNING", format!("Audio state unavailable: {e}"));
                    return Silence::Unchecked;
                }
            };
            last = out
                .lines()
                .filter(|l| l.contains("state:started") && l.contains("STREAM_MUSIC"))
                .map(|l| l.trim().chars().take(120).collect())
                .collect();
            if last.is_empty() {
                return Silence::Idle;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Silence::Busy(last)
    }

    // ---------- main loop ----------

    async fn run(&mut self) {
        let packages = monitored_packages(&self.app.cfg);
        logl("INFO", "Notification Monitor Started");
        logl(
            "INFO",
            format!(
                "Auto-clear: {} (threshold {})",
                self.auto_clear, self.max_notifications
            ),
        );
        for p in &packages {
            logl(
                "INFO",
                format!(
                    "{} ({}) - topic {}",
                    p.name,
                    p.pkg,
                    p.topic.map(|t| t.to_string()).unwrap_or_default()
                ),
            );
        }
        self.setup_storage();
        self.check_su().await;

        let mut cycle: i64 = 0;
        loop {
            cycle += 1;

            if cycle % self.cleanup_cycles.max(1) == 0 {
                self.cleanup_old_notifications();
            }

            let new_notifs = self.get_new_notifications(&packages).await;
            if !new_notifs.is_empty() {
                logl("INFO", format!("Found {} new notification(s)", new_notifs.len()));
                for n in new_notifs {
                    let entry = packages.iter().find(|p| p.pkg == n.pkg);
                    let (app_name, topic, tts) = match entry {
                        Some(e) => (e.name, e.topic, e.tts),
                        None => continue,
                    };

                    logl("INFO", format!("Sending: {app_name} - {}", n.title));
                    let message = Self::format_message(&n, app_name);

                    if self.send_telegram_message(&message, topic).await {
                        self.mark_sent(&n.pkg, &n.title, &n.text, n.timestamp);
                        logl("INFO", "Sent & marked");
                    } else {
                        logl("WARNING", "Failed to send to Telegram");
                    }

                    // The JS queue awaited the announcement before the next
                    // notification too, so one inline call is equivalent.
                    if tts {
                        self.speak_notification(&n.title, &n.text).await;
                    }
                }
            }

            let now = now_ms() as f64 / 1000.0;
            if cycle % 10 == 0 || (self.auto_clear && (now - self.last_clear_time > 30.0)) {
                self.check_and_clear_notifications().await;
                self.last_clear_time = now;
            }

            tokio::time::sleep(Duration::from_secs(self.interval_seconds.max(1) as u64)).await;
        }
    }
}

async fn play_file(audio_file: &Path) -> bool {
    let path = audio_file.to_string_lossy().to_string();
    let out = run_timeout("termux-media-player", &["play", &path], 20).await;
    let _ = std::fs::remove_file(audio_file);
    match out {
        Ok(o) if o.ok() => true,
        Ok(o) => {
            logl("WARNING", format!("TTS play failed: code {}", o.code));
            false
        }
        Err(e) => {
            logl("WARNING", format!("TTS play failed: {e}"));
            false
        }
    }
}

/// `-s music` matches this device: dumpsys audio shows TTS on USAGE_MEDIA.
async fn fallback_tts(speech: &str) -> bool {
    match run_timeout("termux-tts-speak", &["-l", "id", "-s", "music", speech], 20).await {
        Ok(o) if o.ok() => true,
        Ok(o) => {
            logl("WARNING", format!("TTS fallback failed: code {}", o.code));
            false
        }
        Err(e) => {
            logl("WARNING", format!("TTS fallback failed: {e}"));
            false
        }
    }
}

fn fmt_ts(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn clean_text(text: &str) -> String {
    let entities = re(&static_regex::ENTITY, PAT_ENTITY);
    let wrapped = re(&static_regex::STRING_WRAP, PAT_STRING_WRAP);
    let without_entities = entities.replace_all(text, "");
    let without_canonical = without_entities.replace("canonical=1", "");
    wrapped
        .replace_all(&without_canonical, "$1")
        .trim()
        .to_string()
}

mod static_regex {
    use std::sync::OnceLock;

    use regex::Regex;

    pub static PKG: OnceLock<Regex> = OnceLock::new();
    pub static TITLE: OnceLock<Regex> = OnceLock::new();
    pub static TEXT: OnceLock<Regex> = OnceLock::new();
    pub static WHEN: OnceLock<Regex> = OnceLock::new();
    pub static ENTITY: OnceLock<Regex> = OnceLock::new();
    pub static STRING_WRAP: OnceLock<Regex> = OnceLock::new();
}

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        if app.cfg.bot_token.len() < 10 {
            logl("ERROR", "Telegram bot token looks invalid");
            return;
        }
        if app.cfg.chat_id == 0 {
            logl("ERROR", "Telegram chat id missing");
            return;
        }
        let mut monitor = Monitor::new(app);
        monitor.run().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_endpoint_shapes() {
        assert_eq!(build_speech_endpoint(""), "");
        assert_eq!(build_speech_endpoint("http://h:20128/"), "http://h:20128/v1/audio/speech");
        assert_eq!(build_speech_endpoint("http://h:20128/v1"), "http://h:20128/v1/audio/speech");
        assert_eq!(
            build_speech_endpoint("http://h:20128/v1/audio/speech"),
            "http://h:20128/v1/audio/speech"
        );
    }

    #[test]
    fn clean_text_strips_dumpsys_noise() {
        assert_eq!(clean_text(" &amp; hello "), "hello");
        assert_eq!(clean_text("String (abc) x"), "abc x");
        assert_eq!(clean_text("pay?canonical=1"), "pay?");
    }

    #[test]
    fn dumpsys_block_regexes() {
        let blk = "NotificationRecord(0x1: pkg=com.gojek.gopay user=0)\n  title=Payment received\n  text=You got Rp 10.000\n  when=1712345678901";
        assert_eq!(re(&static_regex::PKG, PAT_PKG).captures(blk).unwrap()[1].to_string(), "com.gojek.gopay");
        assert_eq!(re(&static_regex::TITLE, PAT_TITLE).captures(blk).unwrap()[1].trim(), "Payment received");
        assert_eq!(
            re(&static_regex::TEXT, PAT_TEXT).captures(blk).unwrap()[1].trim(),
            "You got Rp 10.000"
        );
        assert_eq!(re(&static_regex::WHEN, PAT_WHEN).captures(blk).unwrap()[1], *"1712345678901");
    }

    #[test]
    fn dedupe_window_matches_republishes() {
        let app = Arc::new(App::new(
            Arc::new(Config::load()),
            reqwest::Client::new(),
        ));
        let mut m = Monitor::new(app);
        m.dedupe_window_seconds = 300;
        let n = Stored {
            package_name: "com.gojek.gopay".into(),
            title: "T".into(),
            message_text: "x".into(),
            notification_time: 1_000_000,
            sent_time: 0,
        };
        assert!(m.same_notification(&n, "com.gojek.gopay", "T", "x", 1_000_000));
        assert!(m.same_notification(&n, "com.gojek.gopay", "T", "x", 1_200_000));
        assert!(!m.same_notification(&n, "com.gojek.gopay", "T", "x", 2_000_000));
        assert!(!m.same_notification(&n, "com.tuya.smart", "T", "x", 1_000_000));
    }
}

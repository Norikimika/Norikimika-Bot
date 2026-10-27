//! Device + service monitors (port of monitor.js).
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::render::{hex, Canvas, Face};
use chrono::Datelike;
use crate::util::{escape_html, format_duration, truncate};

const MOD: &str = "monitor";
const INPUT_SUSPEND: &str = "/sys/class/power_supply/battery/input_suspend";
const CONN_URL: &str = "https://www.gstatic.com/generate_204";
const CONN_HOST: &str = "www.gstatic.com";
const CONN_FAIL_LIMIT: u32 = 7;
const CONN_INTERVAL_SECS: u64 = 10;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MonitorState {
    #[serde(rename = "battery80Sent")]
    pub battery_full_sent: bool,
    #[serde(rename = "battery20Sent")]
    pub battery_low_sent: bool,
    #[serde(rename = "lastStatus")]
    pub last_status: String,
    #[serde(rename = "chargeStartTime")]
    pub charge_start_ms: i64,
    #[serde(rename = "useStartTime")]
    pub use_start_ms: i64,
    #[serde(rename = "connUpEpoch")]
    pub conn_up_ms: i64,
    #[serde(rename = "connDownEpoch")]
    pub conn_down_ms: Option<i64>,
    #[serde(rename = "lastOpenRouterNotified")]
    pub last_openrouter_notified: i64,
    #[serde(rename = "notifiedOpenRouterModelIds")]
    pub notified_ids: Vec<String>,
}

impl Default for MonitorState {
    fn default() -> Self {
        let now = now_ms();
        Self {
            battery_full_sent: false,
            battery_low_sent: false,
            last_status: "UNKNOWN".into(),
            charge_start_ms: now,
            use_start_ms: now,
            conn_up_ms: now,
            conn_down_ms: None,
            last_openrouter_notified: 0,
            notified_ids: Vec::new(),
        }
    }
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

fn state_path() -> std::path::PathBuf {
    crate::config::state_file("monitor.json")
}

fn load_state() -> MonitorState {
    match std::fs::read_to_string(state_path()) {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw).unwrap_or_else(|e| {
            logln!(MOD, "⚠️ monitor.json corrupted, resetting. {e}");
            MonitorState::default()
        }),
        _ => MonitorState::default(),
    }
}

fn save_state(state: &MonitorState) {
    let path = state_path();
    let tmp = path.with_extension("tmp");
    match serde_json::to_string_pretty(state)
        .map_err(anyhow::Error::from)
        .and_then(|json| std::fs::write(&tmp, json).map_err(anyhow::Error::from))
        .and_then(|_| std::fs::rename(&tmp, &path).map_err(anyhow::Error::from))
    {
        Ok(()) => {}
        Err(e) => logln!(MOD, "Failed to save monitor state: {e}"),
    }
}

/// Run a command, returning (success, stdout).
async fn sh(cmd: &str, args: &[&str]) -> (bool, String) {
    match tokio::process::Command::new(cmd).args(args).output().await {
        Ok(out) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Err(_) => (false, String::new()),
    }
}

async fn sh_shell(cmd: &str) -> (bool, String) {
    match tokio::process::Command::new("sh").arg("-c").arg(cmd).output().await {
        Ok(out) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Err(_) => (false, String::new()),
    }
}

struct Tg {
    client: crate::telegram::Client,
    http: reqwest::Client,
    token: String,
    chat_id: i64,
}

impl Tg {
    fn from_env() -> Result<Self> {
        Ok(Self {
            client: crate::telegram::Client::from_env()?,
            http: crate::util::client(),
            token: crate::config::req("BOT_TOKEN")?,
            chat_id: crate::config::chat_id().unwrap_or(0),
        })
    }

    async fn send(&self, topic: Option<i64>, html: &str) {
        if let Err(e) = self.client.send_html(self.chat_id, topic, html).await {
            logln!(MOD, "❌ Telegram failed: {e}");
        }
    }

    async fn send_photo(&self, topic: Option<i64>, png: Vec<u8>, caption: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendPhoto", self.token);
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("image.png")
            .mime_str("image/png")
            .expect("static mime");
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", self.chat_id.to_string())
            .text("caption", caption.to_string())
            .text("parse_mode", "HTML")
            .part("photo", part);
        if let Some(t) = topic {
            form = form.text("message_thread_id", t.to_string());
        }
        match self.http.post(&url).multipart(form).send().await {
            Ok(r) if r.status().is_success() => logln!(MOD, "photo sent"),
            Ok(r) => logln!(MOD, "photo send failed: {}", r.status()),
            Err(e) => logln!(MOD, "photo send error: {e}"),
        }
    }
}

// ================== USAGE ==================

async fn active_iface() -> Option<String> {
    let (_, out) = sh_shell("ip route show default 2>/dev/null").await;
    out.split_whitespace()
        .skip_while(|t| *t != "dev")
        .nth(1)
        .map(str::to_string)
}

fn iface_total(iface: &Value) -> f64 {
    let total = &iface["traffic"]["total"];
    total["rx"].as_f64().unwrap_or(0.0) + total["tx"].as_f64().unwrap_or(0.0)
}

fn format_bytes(kb: f64) -> String {
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

async fn send_usage_report(tg: &Tg) {
    logln!(MOD, "Running scheduled daily usage...");
    let iface = active_iface().await;
    let (ok, out) = sh("vnstat", &["--json"]).await;
    if !ok {
        logln!(MOD, "Daily usage vnstat failed");
        tg.send(crate::config::topic("TOPIC_ID_USAGE"), "❌ Failed to get daily usage").await;
        return;
    }
    let data: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "Daily usage parse failed: {e}");
            return;
        }
    };
    let empty = Vec::new();
    let ifaces = data["interfaces"].as_array().unwrap_or(&empty);
    let chosen = ifaces
        .iter()
        .find(|i| iface.as_deref() == i["name"].as_str())
        .or_else(|| {
            let mut others: Vec<&Value> = ifaces.iter().filter(|i| i["name"].as_str() != Some("lo")).collect();
            others.sort_by(|a, b| iface_total(b).partial_cmp(&iface_total(a)).unwrap_or(std::cmp::Ordering::Equal));
            others.first().copied()
        })
        .or_else(|| ifaces.first());
    let Some(chosen) = chosen else {
        logln!(MOD, "vnstat reported no interfaces");
        return;
    };
    let name = chosen["name"].as_str().unwrap_or("?");
    let days = chosen["traffic"]["day"].as_array();
    let today = days.and_then(|d| d.last());
    let body = match today {
        Some(t) => {
            let up = format_bytes(t["tx"].as_f64().unwrap_or(0.0));
            let down = format_bytes(t["rx"].as_f64().unwrap_or(0.0));
            let total = format_bytes(t["tx"].as_f64().unwrap_or(0.0) + t["rx"].as_f64().unwrap_or(0.0));
            format!("📅 <b>Today</b>\nUpload : {up}\nDownload : {down}\nTotal : {total}")
        }
        None => "No data".to_string(),
    };
    tg.send(
        crate::config::topic("TOPIC_ID_USAGE"),
        &format!("<b>📊 Daily Usage Report ({name})</b>\n<blockquote>{body}</blockquote>"),
    )
    .await;
}

// ================== BATTERY ==================

async fn set_input_suspend(value: u8) -> bool {
    let cmd = format!(
        "v=$(cat {INPUT_SUSPEND} 2>/dev/null); [ \"$v\" = \"{value}\" ] || {{ echo {value} > {INPUT_SUSPEND} && echo CHANGED; }}"
    );
    let (ok, out) = sh("su", &["-c", &cmd]).await;
    if !ok {
        logln!(MOD, "input_suspend={value} failed");
    } else if out.contains("CHANGED") {
        logln!(MOD, "input_suspend set to {value}");
    }
    ok
}

async fn check_battery(tg: &Tg, state: &mut MonitorState) {
    let (ok, out) = sh("termux-battery-status", &[]).await;
    if !ok {
        logln!(MOD, "Battery monitor failed");
        return;
    }
    let data: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "Battery monitor parse error: {e}");
            return;
        }
    };
    let level = data["percentage"].as_i64().unwrap_or(-1);
    let status = data["status"].as_str().unwrap_or("UNKNOWN").to_uppercase();
    let temperature = data["temperature"].as_f64().unwrap_or(0.0).round() as i64;

    let charging = status == "CHARGING" || status == "FULL";
    let discharging = status == "DISCHARGING" || status == "NOT_CHARGING";
    if charging && (state.last_status == "DISCHARGING" || state.last_status == "NOT_CHARGING") {
        state.charge_start_ms = now_ms();
        save_state(state);
    } else if discharging && (state.last_status == "CHARGING" || state.last_status == "FULL") {
        state.use_start_ms = now_ms();
        save_state(state);
    }
    if status != "UNKNOWN" {
        state.last_status = status.clone();
        save_state(state);
    }
    if level < 100 && state.battery_full_sent {
        state.battery_full_sent = false;
        save_state(state);
    }
    if level > 20 && state.battery_low_sent {
        state.battery_low_sent = false;
        save_state(state);
    }

    if level >= 100 {
        set_input_suspend(1).await;
    } else if level <= 20 {
        set_input_suspend(0).await;
    }

    let topic = crate::config::topic("TOPIC_ID_STATUS");
    if level >= 100 && !state.battery_full_sent {
        state.battery_full_sent = true;
        save_state(state);
        let dur = format_duration(((now_ms() - state.charge_start_ms) / 1000).max(0) as u64);
        tg.send(
            topic,
            &format!(
                "<b>🔋 Battery Fully Charged</b>\n<blockquote>Percentage : {level}%\nStatus : {status}\nTemperature : {temperature}°C\nDuration : {dur}</blockquote>"
            ),
        )
        .await;
    }
    if level <= 20 && !state.battery_low_sent {
        state.battery_low_sent = true;
        save_state(state);
        let dur = format_duration(((now_ms() - state.use_start_ms) / 1000).max(0) as u64);
        tg.send(
            topic,
            &format!(
                "<b>🪫 Low Battery Warning</b>\n<blockquote>Percentage : {level}%\nStatus : {status}\nTemperature : {temperature}°C\nDuration : {dur}</blockquote>"
            ),
        )
        .await;
    }
}

// ================== SPEEDTEST ==================

async fn run_auto_speedtest(tg: &Tg) {
    logln!(MOD, "Running scheduled speedtest...");
    let (ok, out) = sh("speedtest-go", &["--json"]).await;
    if !ok {
        logln!(MOD, "Scheduled speedtest failed");
        return;
    }
    let result: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "Scheduled speedtest JSON parse failed: {e}");
            return;
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
    tg.send(crate::config::topic("TOPIC_ID_SPEED"), summary.trim()).await;
}

// ================== OPENROUTER NEW MODELS ==================

struct ModelRow {
    name: String,
    id: String,
    date: String,
    desc: String,
    context: f64,
    price_in: Option<f64>,
    price_out: Option<f64>,
}

fn format_context(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "Unknown".into();
    }
    if value >= 1e6 {
        let v = value / 1e6;
        return if value % 1e6 != 0.0 { format!("{v:.1}M") } else { format!("{:.0}M", v) };
    }
    if value >= 1e3 {
        return format!("{}K", (value / 1e3).round());
    }
    format!("{}", value.round())
}

fn format_price(value: Option<f64>) -> String {
    let Some(n) = value else { return "N/A".into() };
    if !n.is_finite() || n < 0.0 {
        return "N/A".into();
    }
    if n == 0.0 {
        return "Free".into();
    }
    let per_million = n * 1e6;
    if per_million < 0.01 {
        format!("${per_million:.4}")
    } else {
        format!("${per_million:.2}")
    }
}

fn strip_leading_model_name(name: &str, description: &str) -> String {
    let original = description.trim();
    if original.is_empty() {
        return "No description".into();
    }
    let short = name.split_once(':').map(|(_, rest)| rest.trim()).unwrap_or(name.trim());
    for candidate in [name.trim(), short] {
        if candidate.is_empty() {
            continue;
        }
        let lower_orig = original.to_lowercase();
        let lower_cand = candidate.to_lowercase();
        if !lower_orig.starts_with(&lower_cand) {
            continue;
        }
        let mut rest = original[candidate.len()..].trim_start();
        rest = rest.trim_start_matches([':', ',', '-']).trim_start();
        for prefix in ["is ", "are ", "Is ", "Are ", "IS ", "ARE "] {
            if let Some(stripped) = rest.strip_prefix(prefix) {
                rest = stripped.trim_start();
                break;
            }
        }
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    original.to_string()
}

fn render_new_models(list: &[ModelRow], total: usize, extra: &str) -> Result<Vec<u8>> {
    let (w, pad, header_h, row_h, gap) = (1200u32, 48u32, 174u32, 152u32, 8u32);
    let content_w = w - pad * 2;
    let h = header_h + list.len() as u32 * (row_h + gap) - gap + 58;
    let colors = |k: &str| match k {
        "bg" => hex("#050706"),
        "panel" => hex("#0B0E0C"),
        "row" => hex("#090C0A"),
        "line" => hex("#202621"),
        "white" => hex("#F7F9F7"),
        "muted" => hex("#A4ADA6"),
        "green" => hex("#76B900"),
        "bright" => hex("#B7F34A"),
        "leader" => hex("#10180C"),
        _ => hex("#000000"),
    };
    let mut c = Canvas::new(w, h, colors("bg"))?;
    c.fill(0, 0, w, 8, colors("green"));

    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    let date_line = now.format("%d %B %Y").to_string().to_uppercase();

    c.text(pad as i32, 73, 48.0, Face::Bold, colors("white"), "New OpenRouter Models");
    c.text(pad as i32, 111, 22.0, Face::Mono, colors("bright"), "NEWEST RELEASES FIRST");
    c.text_right((w - pad) as i32, 72, 18.0, Face::Mono, colors("muted"), &format!("{total} new models"));
    c.text_right((w - pad) as i32, 104, 16.0, Face::Body, colors("muted"), &date_line);

    c.fill(pad as i32, 130, content_w, 34, colors("panel"));
    c.text((pad + 18) as i32, 153, 14.0, Face::MonoBold, colors("muted"), "#");
    c.text(146, 153, 14.0, Face::MonoBold, colors("muted"), "MODEL / PROVIDER / RELEASED");
    c.text_right(700, 153, 14.0, Face::MonoBold, colors("muted"), "CONTEXT");
    c.text_right(835, 153, 14.0, Face::MonoBold, colors("muted"), "INPUT / M");
    c.text_right(1030, 153, 14.0, Face::MonoBold, colors("muted"), "OUTPUT / M");

    for (i, m) in list.iter().enumerate() {
        let y = header_h + i as u32 * (row_h + gap);
        let bg = if i == 0 {
            colors("leader")
        } else if i % 2 == 1 {
            colors("row")
        } else {
            colors("bg")
        };
        c.fill(pad as i32, y as i32, content_w, row_h, bg);
        if i == 0 {
            c.fill(pad as i32, y as i32, 5, row_h, colors("green"));
        }
        let rank_color = if i == 0 { colors("bright") } else { colors("muted") };
        c.text((pad + 18) as i32, (y + 48) as i32, 24.0, Face::MonoBold, rank_color, &format!("{:02}", i + 1));
        if i == 0 {
            c.text((pad + 18) as i32, (y + 72) as i32, 11.0, Face::Mono, colors("green"), "LATEST");
        }

        let name = c.fit(Face::Bold, 30.0, &m.name, 470.0);
        c.text(146, (y + 39) as i32, 30.0, Face::Bold, colors("white"), &name);
        let sub = c.fit(Face::Body, 16.0, &format!("{} · {}", m.id, m.date), 520.0);
        c.text(146, (y + 65) as i32, 16.0, Face::Body, colors("muted"), &sub);

        c.text_right(700, (y + 43) as i32, 20.0, Face::Mono, colors("white"), &format_context(m.context));
        c.text_right(835, (y + 43) as i32, 20.0, Face::Mono, colors("white"), &format_price(m.price_in));
        c.text_right(1030, (y + 43) as i32, 20.0, Face::Mono, colors("white"), &format_price(m.price_out));

        let desc_color = if i == 0 { hex("#DDE8DD") } else { hex("#C7D0C9") };
        let mut lines = c.wrap(Face::Body, 20.0, &m.desc, 1000.0);
        if lines.len() > 2 {
            let second = lines[1].trim_end_matches(['.', ',', ';', ':', ' ']).to_string();
            lines = vec![lines[0].clone(), format!("{second}…")];
        }
        if let Some(first) = lines.first() {
            c.text(146, (y + 96) as i32, 20.0, Face::Body, desc_color, first);
        } else {
            c.text(146, (y + 96) as i32, 20.0, Face::Body, desc_color, "No description");
        }
        if let Some(second) = lines.get(1) {
            c.text(146, (y + 122) as i32, 20.0, Face::Body, desc_color, second);
        }

        c.fill(pad as i32, (y + row_h) as i32, content_w, 1, colors("line"));
    }

    if !extra.is_empty() {
        c.text(pad as i32, (h - 25) as i32, 15.0, Face::Body, colors("muted"), extra);
    }
    c.png()
}

async fn check_openrouter_models(tg: &Tg, http: &reqwest::Client, state: &mut MonitorState) {
    logln!(MOD, "Checking OpenRouter models...");
    let res = match http
        .get("https://openrouter.ai/api/v1/models?sort=newest&output_modalities=all&limit=1000")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            logln!(MOD, "OpenRouter models fetch failed: {}", r.status());
            return;
        }
        Err(e) => {
            logln!(MOD, "OpenRouter models fetch failed: {e}");
            return;
        }
    };
    let data: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => {
            logln!(MOD, "OpenRouter models parse failed: {e}");
            return;
        }
    };
    let cutoff = chrono::Utc::now().timestamp() - 48 * 3600;
    let last_notified = state.last_openrouter_notified;
    let notified: std::collections::HashSet<&str> = state.notified_ids.iter().map(String::as_str).collect();

    let empty = Vec::new();
    let mut new_models: Vec<&Value> = data["data"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|m| {
            let created = m["created"].as_i64().unwrap_or(0);
            created >= cutoff && (created > last_notified || !notified.contains(m["id"].as_str().unwrap_or("")))
        })
        .collect();
    new_models.sort_by_key(|m| -m["created"].as_i64().unwrap_or(0));

    if new_models.is_empty() {
        logln!(MOD, "OpenRouter: no new models since last check");
        return;
    }

    let visible: Vec<&Value> = new_models.iter().take(20).copied().collect();
    let rows: Vec<ModelRow> = visible
        .iter()
        .map(|m| {
            let id = m["id"].as_str().unwrap_or("").to_string();
            let name = m["name"].as_str().filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
            let date = match m["created"].as_i64().filter(|c| *c > 0).and_then(|c| chrono::DateTime::from_timestamp(c, 0)) {
                Some(dt) => dt.format("%d %b %Y").to_string(),
                None => "Unknown".to_string(),
            };
            let raw_desc = m["description"].as_str().unwrap_or("");
            let desc = if raw_desc.is_empty() {
                "No description".to_string()
            } else {
                let d = strip_leading_model_name(&name, raw_desc);
                if d.chars().count() > 220 {
                    format!("{}…", truncate(&d, 220).trim_end_matches(['.', ',', ';', ':', ' ']))
                } else {
                    d
                }
            };
            let context = m["context_length"].as_f64().or_else(|| m["top_provider"]["context_length"].as_f64()).unwrap_or(0.0);
            ModelRow {
                name,
                id,
                date,
                desc,
                context,
                price_in: m["pricing"]["prompt"].as_str().and_then(|s| s.parse().ok()),
                price_out: m["pricing"]["completion"].as_str().and_then(|s| s.parse().ok()),
            }
        })
        .collect();

    let extra = if new_models.len() > 20 {
        format!("{} more models in this update", new_models.len() - 20)
    } else {
        "Source: OpenRouter".to_string()
    };

    match render_new_models(&rows, new_models.len(), &extra) {
        Ok(png) => {
            let caption = format!(
                "<a href=\"https://openrouter.ai/models\"><b>New OpenRouter Models</b> ({})</a>",
                new_models.len()
            );
            tg.send_photo(crate::config::topic("TOPIC_ID_STATUS"), png, &caption).await;
            let mut ids: Vec<String> = state.notified_ids.clone();
            for m in &visible {
                if let Some(id) = m["id"].as_str() {
                    if !ids.iter().any(|x| x == id) {
                        ids.push(id.to_string());
                    }
                }
            }
            if ids.len() > 500 {
                ids.drain(..ids.len() - 500);
            }
            state.notified_ids = ids;
            let newest = new_models.iter().map(|m| m["created"].as_i64().unwrap_or(0)).max().unwrap_or(0);
            state.last_openrouter_notified = state.last_openrouter_notified.max(newest);
            save_state(state);
        }
        Err(e) => logln!(MOD, "image render failed ({e})"),
    }
    logln!(MOD, "OpenRouter: {} new model(s)", new_models.len());
}

// ================== LEADERBOARD (weekly / AA) ==================

struct LbRow {
    name: String,
    sub: String,
    value: f64,
    text: String,
}

struct LbConfig {
    title: String,
    subtitle: String,
    header_right: (String, String),
    model_column: String,
    metric_column: String,
    value_label: String,
    footer: String,
    footer_right: String,
    rows: Vec<LbRow>,
}

fn render_leaderboard(cfg: &LbConfig) -> Result<Vec<u8>> {
    let (w, pad, header_h, row_h, gap) = (1200u32, 48u32, 190u32, 90u32, 8u32);
    let h = header_h + cfg.rows.len() as u32 * (row_h + gap) - gap + 48;
    let colors = |k: &str| match k {
        "bg" => hex("#050706"),
        "panel" => hex("#0B0E0C"),
        "line" => hex("#202621"),
        "white" => hex("#F7F9F7"),
        "muted" => hex("#A4ADA6"),
        "green" => hex("#76B900"),
        "bright" => hex("#B7F34A"),
        _ => hex("#000000"),
    };
    let mut c = Canvas::new(w, h, colors("bg"))?;
    c.fill(0, 0, w, 8, colors("green"));
    c.text(pad as i32, 73, 48.0, Face::Bold, colors("white"), &cfg.title);
    c.text(pad as i32, 111, 22.0, Face::Mono, colors("bright"), &cfg.subtitle);
    c.text_right((w - pad) as i32, 72, 18.0, Face::Mono, colors("muted"), &cfg.header_right.0);
    c.text_right((w - pad) as i32, 104, 16.0, Face::Body, colors("muted"), &cfg.header_right.1);

    c.fill(pad as i32, 132, w - pad * 2, 34, colors("panel"));
    c.text((pad + 18) as i32, 154, 14.0, Face::MonoBold, colors("muted"), "RANK");
    c.text(146, 154, 14.0, Face::MonoBold, colors("muted"), &cfg.model_column);
    c.text_right((w - pad - 18) as i32, 154, 14.0, Face::MonoBold, colors("muted"), &cfg.metric_column);

    let max_value = cfg.rows.iter().map(|r| r.value).fold(1.0f64, f64::max);
    let (model_x, model_max_w, bar_x, bar_w, number_x) = (146i32, 590.0f32, 780i32, 190.0f32, (w - pad - 18) as i32);
    let mut y = header_h;

    for (i, r) in cfg.rows.iter().enumerate() {
        let bg = if i == 0 {
            hex("#10180C")
        } else if i % 2 == 1 {
            hex("#090C0A")
        } else {
            colors("bg")
        };
        c.fill(pad as i32, y as i32, w - pad * 2, row_h, bg);
        if i == 0 {
            c.fill(pad as i32, y as i32, 5, row_h, colors("green"));
        }
        let rank_color = if i == 0 { colors("bright") } else { colors("muted") };
        c.text((pad + 18) as i32, (y + 51) as i32, 24.0, Face::MonoBold, rank_color, &format!("{:02}", i + 1));

        let name = c.fit(Face::Bold, 27.0, &r.name, model_max_w);
        c.text(model_x, (y + 39) as i32, 27.0, Face::Bold, colors("white"), &name);
        let sub = c.fit(Face::Body, 17.0, &r.sub, model_max_w);
        c.text(model_x, (y + 66) as i32, 17.0, Face::Body, colors("muted"), &sub);

        let filled = ((bar_w as f64) * (r.value / max_value)).max(2.0) as u32;
        c.fill(bar_x, (y + 52) as i32, bar_w as u32, 5, colors("line"));
        let bar_color = if i == 0 { colors("bright") } else { colors("green") };
        c.fill(bar_x, (y + 52) as i32, filled, 5, bar_color);

        c.text_right(number_x, (y + 51) as i32, 28.0, Face::MonoBold, colors("white"), &r.text);
        c.text_right(number_x, (y + 73) as i32, 13.0, Face::Mono, colors("muted"), &cfg.value_label);
        c.fill(pad as i32, (y + row_h) as i32, w - pad * 2, 1, colors("line"));
        y += row_h + gap;
    }

    c.text(pad as i32, (h - 24) as i32, 15.0, Face::Body, colors("muted"), &cfg.footer);
    c.text_right((w - pad) as i32, (h - 24) as i32, 15.0, Face::MonoBold, colors("muted"), &cfg.footer_right);
    c.png()
}

fn format_tokens(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "N/A".into();
    }
    let fmt = |div: f64, suffix: &str| {
        let v = value / div;
        if value % div != 0.0 {
            format!("{v:.1}{suffix}")
        } else {
            format!("{:.0}{suffix}", v.round())
        }
    };
    if value >= 1e12 {
        fmt(1e12, "T")
    } else if value >= 1e9 {
        fmt(1e9, "B")
    } else if value >= 1e6 {
        fmt(1e6, "M")
    } else if value >= 1e3 {
        fmt(1e3, "K")
    } else {
        format!("{}", value.round())
    }
}

fn wita_now() -> chrono::DateTime<chrono::FixedOffset> {
    chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
}

async fn check_top_weekly(tg: &Tg, http: &reqwest::Client) {
    logln!(MOD, "Checking OpenRouter top weekly...");
    let (models, rankings) = tokio::join!(
        http.get("https://openrouter.ai/api/v1/models?sort=top-weekly&limit=10").send(),
        http.get("https://openrouter.ai/api/frontend/v1/rankings/discovery?view=week&metric=tokens").send()
    );
    let (Ok(models), Ok(rankings)) = (models, rankings) else {
        logln!(MOD, "Rankings fetch failed");
        return;
    };
    if !models.status().is_success() || !rankings.status().is_success() {
        logln!(MOD, "Rankings fetch failed: {} / {}", models.status(), rankings.status());
        return;
    }
    let (Ok(models_data), Ok(rankings_data)) = (models.json::<Value>().await, rankings.json::<Value>().await) else {
        logln!(MOD, "Rankings parse failed");
        return;
    };

    let mut totals: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    if let Some(map) = rankings_data["data"]["totals"].as_object() {
        for (key, entry) in map {
            let model = key.rsplit_once(':').map(|(m, _)| m).unwrap_or(key).to_string();
            let total = entry["t"].as_f64().unwrap_or(0.0);
            *totals.entry(model).or_insert(0.0) += total;
        }
    }

    let empty = Vec::new();
    let rows: Vec<LbRow> = models_data["data"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str().unwrap_or("");
            let canonical = m["canonical_slug"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| id.split_once(':').map(|(s, _)| s).unwrap_or(id));
            let tokens = *totals.get(canonical)?;
            let name = m["name"].as_str().unwrap_or(id);
            let name = name.split_once(':').map(|(_, rest)| rest.trim()).unwrap_or(name).to_string();
            let author = id.split('/').next().unwrap_or("").to_string();
            Some(LbRow {
                name,
                sub: author,
                value: tokens,
                text: format_tokens(tokens),
            })
        })
        .take(10)
        .collect();
    if rows.is_empty() {
        logln!(MOD, "Rankings: no rows parsed");
        return;
    }
    logln!(MOD, "Rankings: {} models, #1 {}", rows.len(), rows.first().map(|r| r.name.as_str()).unwrap_or(""));

    let now = wita_now();
    let year: i32 = now.format("%Y").to_string().parse().unwrap_or(2026);
    let day_of_year = now.ordinal() as f64;
    let weekday = now.format("%u").to_string().parse::<f64>().unwrap_or(1.0);
    let week = ((day_of_year + weekday) / 7.0).ceil() as u32;

    let cfg = LbConfig {
        title: "OpenRouter weekly".into(),
        subtitle: "TOP 10 BY TOTAL TOKENS".into(),
        header_right: (
            format!("WEEK {week:02} / {year}"),
            now.format("%d %B").to_string().to_uppercase(),
        ),
        model_column: "MODEL / PROVIDER".into(),
        metric_column: "TOKENS PROCESSED".into(),
        value_label: "TOKENS".into(),
        footer: "Prompt + completion tokens · Source: OpenRouter".into(),
        footer_right: "WEEKLY RANKING".into(),
        rows,
    };
    match render_leaderboard(&cfg) {
        Ok(png) => {
            tg.send_photo(
                crate::config::topic("TOPIC_ID_STATUS"),
                png,
                "<a href=\"https://openrouter.ai/models?order=top-weekly\"><b>OpenRouter Top Weekly Models</b></a>",
            )
            .await
        }
        Err(e) => logln!(MOD, "rankings image render failed ({e})"),
    }
}

// ================== ARTIFICIAL ANALYSIS ==================

struct AaRow {
    name: String,
    creator: String,
    context: String,
    intelligence: f64,
    cost_per_task: Option<f64>,
}

fn strip_tags(cell: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in cell.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn blocks<'a>(hay: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = hay;
    while let Some(start) = rest.find(open) {
        let tail = &rest[start..];
        let Some(end) = tail.find(close) else { break };
        out.push(&tail[..end + close.len()]);
        rest = &tail[end + close.len()..];
    }
    out
}

fn cells_of(row: &str) -> Vec<String> {
    blocks(row, "<td", "</td>")
        .into_iter()
        .chain(blocks(row, "<th", "</th>"))
        .map(strip_tags)
        .collect()
}

fn parse_number(value: &str) -> Option<f64> {
    let t: String = value.chars().filter(|c| !matches!(c, '$' | ',' | '*')).collect();
    let t = t.trim();
    if t.is_empty() || t == "--" {
        return None;
    }
    t.parse::<f64>().ok()
}

fn parse_aa_table(html: &str) -> Vec<AaRow> {
    let mut rows = Vec::new();
    for tr in blocks(html, "<tr", "</tr>") {
        let cells = cells_of(tr);
        if cells.len() < 3 {
            continue;
        }
        let name = cells[0].trim().to_string();
        let lower = name.to_lowercase();
        if name.is_empty()
            || ["features", "intelligence", "price", "speed", "latency", "model", "end-to-end response time"]
                .contains(&lower.as_str())
        {
            continue;
        }
        let is_ctx = |s: &str| {
            let s = s.trim();
            let (num, suffix) = match s.char_indices().last() {
                Some((i, c)) if c.is_alphabetic() => (&s[..i], Some(c.to_ascii_uppercase())),
                _ => (s, None),
            };
            matches!(suffix, Some('K') | Some('M') | Some('B')) && num.trim().parse::<f64>().is_ok()
        };
        let context = cells.get(1).filter(|c| is_ctx(c)).cloned().unwrap_or_default();
        let creator = if context.is_empty() {
            cells.get(1).cloned().unwrap_or_default()
        } else {
            cells.get(2).cloned().unwrap_or_default()
        };
        let offset = if context.is_empty() { 2 } else { 3 };
        let Some(intelligence) = cells.get(offset).and_then(|c| parse_number(c)) else {
            continue;
        };
        rows.push(AaRow {
            name,
            creator,
            context,
            intelligence,
            cost_per_task: cells.get(offset + 1).and_then(|c| parse_number(c)),
        });
    }
    rows
}

async fn check_artificial_analysis(tg: &Tg, http: &reqwest::Client) {
    logln!(MOD, "Checking Artificial Analysis leaderboard...");
    let html = match http
        .get("https://artificialanalysis.ai/leaderboards/models")
        .header("Accept", "text/html")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => {
            logln!(MOD, "AA fetch failed: {}", r.status());
            return;
        }
        Err(e) => {
            logln!(MOD, "AA fetch failed: {e}");
            return;
        }
    };
    let Some(table) = blocks(&html, "<table", "</table>").first().copied() else {
        logln!(MOD, "AA: no table in page");
        return;
    };
    let mut rows = parse_aa_table(table);
    if rows.is_empty() {
        logln!(MOD, "AA: no rows parsed");
        return;
    }
    rows.sort_by(|a, b| b.intelligence.partial_cmp(&a.intelligence).unwrap_or(std::cmp::Ordering::Equal));
    logln!(MOD, "AA: {} models, #1 {} ({})", rows.len(), rows[0].name, rows[0].intelligence);

    let now = wita_now();
    let cfg = LbConfig {
        title: "Artificial Analysis".into(),
        subtitle: "TOP 10 BY INTELLIGENCE INDEX".into(),
        header_right: (
            now.format("%Y").to_string(),
            now.format("%d %B").to_string().to_uppercase(),
        ),
        model_column: "MODEL / CREATOR".into(),
        metric_column: "INTELLIGENCE INDEX".into(),
        value_label: "INDEX".into(),
        footer: "Artificial Analysis Intelligence Index · Source: artificialanalysis.ai".into(),
        footer_right: "LEADERBOARD".into(),
        rows: rows
            .iter()
            .take(10)
            .map(|r| {
                let mut sub = vec![r.creator.clone()];
                if !r.context.is_empty() {
                    sub.push(format!("{} ctx", r.context));
                }
                if let Some(cost) = r.cost_per_task {
                    sub.push(format!("${cost}/task"));
                }
                LbRow {
                    name: r.name.clone(),
                    sub: sub.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "),
                    value: r.intelligence,
                    text: format!("{}", r.intelligence),
                }
            })
            .collect(),
    };
    match render_leaderboard(&cfg) {
        Ok(png) => {
            tg.send_photo(
                crate::config::topic("TOPIC_ID_STATUS"),
                png,
                "<a href=\"https://artificialanalysis.ai/leaderboards/models\"><b>Artificial Analysis Top Models</b></a>",
            )
            .await
        }
        Err(e) => logln!(MOD, "AA image render failed ({e})"),
    }
}

// ================== CONNECTION WATCHDOG ==================

async fn ping_host(host: &str) -> (bool, f64, String) {
    let (ok, out) = sh("ping", &["-c", "1", "-W", "3", host]).await;
    if ok {
        let ms = out
            .split("time=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        return (true, ms, String::new());
    }
    let reason = out
        .lines()
        .map(str::trim)
        .find(|l| {
            let l = l.to_lowercase();
            l.contains("unreachable") || l.contains("100% packet loss") || l.contains("unknown host") || l.contains("not known")
        })
        .unwrap_or("no reply")
        .to_string();
    (false, 0.0, reason)
}

async fn getprop(prop: &str) -> String {
    let (ok, out) = sh("getprop", &[prop]).await;
    if ok && !out.trim().is_empty() {
        out.trim().to_string()
    } else {
        "N/A".into()
    }
}

async fn connection_info(http: &reqwest::Client) -> (String, String) {
    let ip = match http.get("https://ipinfo.io/ip").send().await {
        Ok(r) => r.text().await.ok(),
        Err(_) => None,
    };
    let org = match http.get("https://ipinfo.io/org").send().await {
        Ok(r) => r.text().await.ok(),
        Err(_) => None,
    };
    let ip = ip.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "N/A".into());
    let isp = org
        .map(|s| {
            let t = s.trim().to_string();
            match t.split_once(' ') {
                Some((first, rest)) if first.starts_with("AS") && first[2..].chars().all(|c| c.is_ascii_digit()) => rest.to_string(),
                _ => t,
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "N/A".into());
    (ip, isp)
}

async fn send_connection_restored(tg: &Tg, http: &reqwest::Client, latency: f64, down_ms: i64, up_ms: i64) {
    let (model, codename) = tokio::join!(getprop("ro.product.model"), getprop("ro.product.device"));
    let (ip, isp) = connection_info(http).await;
    let now = chrono::Local::now();
    let message = format!(
        "<b>✅ Connection Restored</b>\n<blockquote>📱 Device: {}\n📟 Codename: {}\n⏰ Time: {}\n🌐 IP: {}\n🚀 ISP: {}\n🐢 Latency: {latency}ms\n⏱️ Downtime: {}\n⚡ Uptime: {}</blockquote>",
        escape_html(&model),
        escape_html(&codename),
        now.format("%d-%m-%Y, %H:%M:%S"),
        escape_html(&ip),
        escape_html(&isp),
        format_duration(((now_ms() - down_ms) / 1000).max(0) as u64),
        format_duration(((now_ms() - up_ms) / 1000).max(0) as u64),
    );
    tg.send(crate::config::topic("TOPIC_ID_CONNECTION"), &message).await;
}

pub async fn run() -> Result<()> {
    let tg = Tg::from_env()?;
    let http = crate::util::http()
        .timeout(Duration::from_secs(15))
        .user_agent("Norikimika-Bot")
        .build()?;
    let mut state = load_state();

    logln!(MOD, "Starting monitor service...");
    logln!(MOD, "Connection watchdog: {CONN_HOST} every {CONN_INTERVAL_SECS}s");

    // Connection watchdog
    {
        let tg = tg.clone_for_task()?;
        let http = http.clone();
        tokio::spawn(async move {
            let mut conn_up = true;
            let mut fails = 0u32;
            let mut state = load_state();
            loop {
                let (ping_ok, ms, reason) = ping_host(CONN_HOST).await;
                let code = if ping_ok {
                    http.head(CONN_URL)
                        .timeout(Duration::from_secs(5))
                        .send()
                        .await
                        .map(|r| r.status().as_u16())
                        .unwrap_or(0)
                } else {
                    0
                };
                if ping_ok && (code == 200 || code == 204) {
                    fails = 0;
                    if let Some(down) = state.conn_down_ms {
                        let down_ms = now_ms() - down;
                        let up_ms = if state.conn_up_ms > 0 { down - state.conn_up_ms } else { 0 };
                        let alerted = !conn_up;
                        state.conn_down_ms = None;
                        state.conn_up_ms = now_ms();
                        conn_up = true;
                        save_state(&state);
                        logln!(MOD, "Connection restored, downtime {}, uptime {}",
                            format_duration((down_ms / 1000).max(0) as u64),
                            format_duration((up_ms / 1000).max(0) as u64));
                        if alerted {
                            send_connection_restored(&tg, &http, ms, down_ms, up_ms).await;
                        }
                    }
                } else {
                    fails += 1;
                    let why = if ping_ok { format!("HTTP={code}") } else { reason };
                    logln!(MOD, "Connection lost {fails}/{CONN_FAIL_LIMIT} ({why})");
                    if state.conn_down_ms.is_none() {
                        state.conn_down_ms = Some(now_ms());
                        save_state(&state);
                    }
                    if fails >= CONN_FAIL_LIMIT {
                        fails = 0;
                        if conn_up {
                            conn_up = false;
                            logln!(MOD, "Connection down, will notify on restore");
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(CONN_INTERVAL_SECS)).await;
            }
        });
    }

    // Scheduler: tick every 30s, fire on minute boundaries (local time).
    let mut last_key = String::new();
    loop {
        let now = chrono::Local::now();
        let key = now.format("%Y%m%d%H%M").to_string();
        if key != last_key {
            last_key = key.clone();
            let minute = now.format("%M").to_string().parse::<u32>().unwrap_or(99);
            let hour = now.format("%H").to_string().parse::<u32>().unwrap_or(99);

            check_battery(&tg, &mut state).await;
            if minute % 15 == 0 {
                check_openrouter_models(&tg, &http, &mut state).await;
            }
            if minute == 0 && hour == 0 {
                send_usage_report(&tg).await;
                run_auto_speedtest(&tg).await;
            }
            if minute == 0 && hour == 12 {
                run_auto_speedtest(&tg).await;
            }
            if minute == 0 && hour == 1 {
                check_top_weekly(&tg, &http).await;
            }
            if minute == 30 && hour == 1 {
                check_artificial_analysis(&tg, &http).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

impl Tg {
    /// Telegram client isn't Clone (holds reqwest::Client + token) — rebuild from env.
    fn clone_for_task(&self) -> Result<Tg> {
        let _ = &self.client;
        Tg::from_env()
    }
}

/// Render both image layouts with sample data and write them out.
/// Used by `monitor --dry-run` so the canvas path is verifiable without tokens.
pub fn render_selftest() -> Result<Vec<std::path::PathBuf>> {
    let models: Vec<ModelRow> = (0..3)
        .map(|i| ModelRow {
            name: format!("Sample Model {i}"),
            id: format!("vendor/model-{i}"),
            date: "27 Oct 2026".into(),
            desc: "Sample description text used to verify wrapping, ellipsis and column layout on the canvas.".into(),
            context: 200000.0,
            price_in: Some(0.000001),
            price_out: Some(0.000002),
        })
        .collect();
    let lb = LbConfig {
        title: "OpenRouter weekly".into(),
        subtitle: "TOP 10 BY TOTAL TOKENS".into(),
        header_right: ("WEEK 43 / 2026".into(), "27 OCTOBER".into()),
        model_column: "MODEL / PROVIDER".into(),
        metric_column: "TOKENS PROCESSED".into(),
        value_label: "TOKENS".into(),
        footer: "Prompt + completion tokens · Source: OpenRouter".into(),
        footer_right: "WEEKLY RANKING".into(),
        rows: (0..3)
            .map(|i| LbRow {
                name: format!("Sample Model {i}"),
                sub: "vendor".into(),
                value: 1.5e9 / (i as f64 + 1.0),
                text: format_tokens(1.5e9 / (i as f64 + 1.0)),
            })
            .collect(),
    };

    let dir = crate::config::state_dir();
    let mut out = Vec::new();
    for (name, png) in [("selftest-models.png", render_new_models(&models, 3, "Source: OpenRouter")?), (
        "selftest-leaderboard.png",
        render_leaderboard(&lb)?,
    )] {
        let path = dir.join(name);
        std::fs::write(&path, png)?;
        out.push(path);
    }
    Ok(out)
}

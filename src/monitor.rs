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
/// Clean checks in a row before a restore is announced. One answer is not proof: a link that
/// just came back often answers once and drops again.
const CONN_STABLE_CHECKS: u32 = 2;
/// How many cycles a restore message may keep retrying before it is given up on.
const ALERT_MAX_ATTEMPTS: u32 = 10;

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

const SH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run a command, returning (success, stdout-or-why-it-failed).
async fn sh(cmd: &str, args: &[&str]) -> (bool, String) {
    let fut = tokio::process::Command::new(cmd).args(args).kill_on_drop(true).output();
    match tokio::time::timeout(SH_TIMEOUT, fut).await {
        Ok(Ok(out)) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(e)) => (false, format!("{cmd}: {e}")),
        Err(_) => (false, format!("{cmd} timed out after {}s", SH_TIMEOUT.as_secs())),
    }
}

async fn sh_shell(cmd: &str) -> (bool, String) {
    let fut = tokio::process::Command::new("sh").arg("-c").arg(cmd).kill_on_drop(true).output();
    match tokio::time::timeout(SH_TIMEOUT, fut).await {
        Ok(Ok(out)) => (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(e)) => (false, format!("`{cmd}`: {e}")),
        Err(_) => (false, format!("`{cmd}` timed out after {}s", SH_TIMEOUT.as_secs())),
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
        if let Err(e) = self.try_send(topic, html).await {
            logln!(MOD, "❌ Telegram failed: {e:#}");
        }
    }

    /// Same as `send`, but the caller decides what a failure means. The connection watchdog
    /// needs the error: right after a reconnect the send itself is what fails.
    async fn try_send(&self, topic: Option<i64>, html: &str) -> Result<()> {
        self.client.send_html(self.chat_id, topic, html).await
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
    let out = match crate::util::capture("vnstat", &["--json"]).await {
        Ok(out) => out,
        Err(e) => {
            logln!(MOD, "Daily usage vnstat failed: {e}");
            tg.send(crate::config::topic("TOPIC_ID_USAGE"), &crate::util::vnstat_hint(&e)).await;
            return;
        }
    };
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
    let out = match crate::util::capture("termux-battery-status", &[]).await {
        Ok(out) => out,
        Err(e) => {
            logln!(
                MOD,
                "Battery monitor failed: {e} — Termux needs `pkg install termux-api` and the Termux:API app"
            );
            return;
        }
    };
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

    // One save per poll at most, and none at all when nothing moved: this ran every 30s and
    // rewrote the whole state file up to three times for a battery that had not changed.
    let mut dirty = false;
    let charging = status == "CHARGING" || status == "FULL";
    let discharging = status == "DISCHARGING" || status == "NOT_CHARGING";
    if charging && (state.last_status == "DISCHARGING" || state.last_status == "NOT_CHARGING") {
        state.charge_start_ms = now_ms();
        dirty = true;
    } else if discharging && (state.last_status == "CHARGING" || state.last_status == "FULL") {
        state.use_start_ms = now_ms();
        dirty = true;
    }
    if status != "UNKNOWN" && status != state.last_status {
        state.last_status = status.clone();
        dirty = true;
    }
    if level < 100 && state.battery_full_sent {
        state.battery_full_sent = false;
        dirty = true;
    }
    if level > 20 && state.battery_low_sent {
        state.battery_low_sent = false;
        dirty = true;
    }
    if dirty {
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
    /// Media generators (image, video, audio) report `prompt`/`completion` as "0" because they
    /// do not sell tokens, which used to be printed as "Free". They are billed per output, so
    /// they get their own label and no token columns.
    is_media: bool,
    price_unit: Option<f64>,
}

/// "1 new model", not "1 new models".
fn models_count(n: usize) -> String {
    if n == 1 {
        "1 new model".to_string()
    } else {
        format!("{n} new models")
    }
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

/// OpenRouter sends prices as strings ("0.000003"), but a number shows up now and then.
fn price_value(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// One API entry turned into a row. Kept separate so the tests can feed real payloads through
/// exactly the same code the notification uses.
fn model_row(m: &Value) -> ModelRow {
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
    let (is_media, price_unit) = media_pricing(m);
    ModelRow {
        name,
        id,
        date,
        desc,
        context,
        price_in: price_value(&m["pricing"]["prompt"]),
        price_out: price_value(&m["pricing"]["completion"]),
        is_media,
        price_unit,
    }
}

/// Whether a model sells outputs instead of tokens, and what one output costs.
///
/// A model that emits only media (image, video, audio) reports `prompt`/`completion` as "0" —
/// calling that "Free" was the bug. A model that also emits text keeps its token prices,
/// because those are the numbers people actually compare.
fn media_pricing(m: &Value) -> (bool, Option<f64>) {
    let modalities: Vec<&str> = m["architecture"]["output_modalities"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let price_in = price_value(&m["pricing"]["prompt"]);
    let price_out = price_value(&m["pricing"]["completion"]);
    let price_unit = ["image_output", "image", "video", "audio", "speech", "request"]
        .iter()
        .find_map(|key| price_value(&m["pricing"][key]))
        .filter(|p| p.is_finite() && *p > 0.0);
    // Whatever the output is called, a model that charges per token sells tokens: OpenRouter
    // labels the output of clairic and joey-1 "decisions", nemotron "transcription" and
    // assemblyai "speech", yet all four are billed through `prompt`. Only when no token price
    // exists is it a per-output model.
    let token_priced = price_in.unwrap_or(0.0) > 0.0 || price_out.unwrap_or(0.0) > 0.0;
    let media_output = modalities
        .iter()
        .any(|o| matches!(*o, "image" | "video" | "audio" | "speech"));
    let media = !token_priced && (media_output || price_unit.is_some());
    (media, price_unit)
}

/// What goes in the two price columns. A media generator reports "0" for prompt and
/// completion because it does not sell tokens; printing "Free" for it is simply wrong.
fn price_cells(m: &ModelRow) -> (String, String) {
    if m.is_media {
        let out = match m.price_unit {
            Some(p) if p.is_finite() && p > 0.0 => {
                let decimals = if p >= 0.01 {
                    2
                } else if p >= 0.0001 {
                    4
                } else {
                    6
                };
                format!("From ${p:.decimals$}")
            }
            _ => "From".to_string(),
        };
        return ("—".to_string(), out);
    }
    // A model that only charges on one side reports "0" for the other. Printing "Free" there
    // claims something the data does not say, so an uncharged side is shown as a dash.
    let in_free = m.price_in.unwrap_or(0.0) <= 0.0;
    let out_free = m.price_out.unwrap_or(0.0) <= 0.0;
    if in_free && out_free {
        return ("Free".to_string(), "Free".to_string());
    }
    let side = |free: bool, v: Option<f64>| {
        if free {
            "—".to_string()
        } else {
            format_price(v)
        }
    };
    (side(in_free, m.price_in), side(out_free, m.price_out))
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
    c.text_right((w - pad) as i32, 72, 18.0, Face::Mono, colors("muted"), &models_count(total));
    c.text_right((w - pad) as i32, 104, 16.0, Face::Body, colors("muted"), &date_line);

    c.fill(pad as i32, 130, content_w, 34, colors("panel"));
    c.text((pad + 18) as i32, 153, 14.0, Face::MonoBold, colors("muted"), "#");
    c.text(146, 153, 14.0, Face::MonoBold, colors("muted"), "MODEL / PROVIDER / RELEASED");
    c.text_right(700, 153, 14.0, Face::MonoBold, colors("muted"), "CONTEXT");
    c.text_right(835, 153, 14.0, Face::MonoBold, colors("muted"), "INPUT / M");
    let output_header = if list.iter().any(|m| m.is_media) { "OUTPUT / UNIT" } else { "OUTPUT / M" };
    c.text_right(1030, 153, 14.0, Face::MonoBold, colors("muted"), output_header);

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

        // A context window means nothing for a model that answers with a video or an image.
        let context = if m.context > 0.0 {
            format_context(m.context)
        } else if m.is_media {
            "—".to_string()
        } else {
            format_context(m.context)
        };
        c.text_right(700, (y + 43) as i32, 20.0, Face::Mono, colors("white"), &context);
        let (price_in, price_out) = price_cells(m);
        c.text_right(835, (y + 43) as i32, 20.0, Face::Mono, colors("white"), &price_in);
        c.text_right(1030, (y + 43) as i32, 20.0, Face::Mono, colors("white"), &price_out);

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
    for m in &visible {
        let outputs: Vec<&str> = m["architecture"]["output_modalities"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let (media, unit) = media_pricing(m);
        logln!(
            MOD,
            "OpenRouter row {}: outputs={outputs:?} context={} prompt={:?} completion={:?} unit={unit:?} -> {}",
            m["id"].as_str().unwrap_or("?"),
            m["context_length"].as_i64().unwrap_or(0),
            price_value(&m["pricing"]["prompt"]),
            price_value(&m["pricing"]["completion"]),
            if media { "per output" } else { "per token" }
        );
    }
    let rows: Vec<ModelRow> = visible.iter().map(|m| model_row(m)).collect();

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
        Ok(r) => match r.text().await {
            Ok(text) => Some(text),
            Err(e) => {
                crate::util::log_once(MOD, "ipinfo-ip-body", &format!("[Connection] ipinfo/ip body failed: {e:#}"));
                None
            }
        },
        Err(e) => {
            crate::util::log_once(MOD, "ipinfo-ip", &format!("[Connection] ipinfo/ip failed: {e:#}"));
            None
        }
    };
    let org = match http.get("https://ipinfo.io/org").send().await {
        Ok(r) => match r.text().await {
            Ok(text) => Some(text),
            Err(e) => {
                crate::util::log_once(MOD, "ipinfo-org-body", &format!("[Connection] ipinfo/org body failed: {e:#}"));
                None
            }
        },
        Err(e) => {
            crate::util::log_once(MOD, "ipinfo-org", &format!("[Connection] ipinfo/org failed: {e:#}"));
            None
        }
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

/// When a run of failed checks becomes an outage worth reporting.
///
/// The outage starts at the first failure, but it only counts once `CONN_FAIL_LIMIT` checks in a
/// row have failed. Marking the state on the very first failure made a single lost packet reset
/// the uptime streak, so a link that had been up for hours reported "Uptime: 1m 13s".
fn outage_start(first_fail_ms: i64, fails: u32) -> Option<i64> {
    (fails >= CONN_FAIL_LIMIT).then_some(first_fail_ms)
}

/// A restore is only announced once the link has answered this many times in a row.
fn restore_is_stable(good_streak: u32) -> bool {
    good_streak >= CONN_STABLE_CHECKS
}

/// Next attempt number after a failed alert send, or `None` when the budget is spent.
fn next_alert_attempt(failed_attempts: u32) -> Option<u32> {
    let next = failed_attempts + 1;
    (next < ALERT_MAX_ATTEMPTS).then_some(next)
}

/// A restore that still has to reach Telegram. Kept around after a failed send: right after a
/// reconnect DNS and TLS are often not ready yet, so the first attempt is the one that fails.
#[derive(Clone)]
struct ConnAlert {
    restored_at: chrono::DateTime<chrono::Local>,
    latency: f64,
    down_ms: i64,
    up_ms: i64,
    attempts: u32,
}

/// One attempt at the restore message, with the failure handed back to the caller.
async fn try_send_alert(tg: &Tg, http: &reqwest::Client, alert: &ConnAlert) -> Result<()> {
    let (model, codename) = tokio::join!(getprop("ro.product.model"), getprop("ro.product.device"));
    let (ip, isp) = connection_info(http).await;
    let message = connection_restored_message(
        &model,
        &codename,
        &alert.restored_at.format("%d-%m-%Y, %H:%M:%S").to_string(),
        &ip,
        &isp,
        alert.latency,
        alert.down_ms,
        alert.up_ms,
    );
    tg.try_send(crate::config::topic("TOPIC_ID_CONNECTION"), &message).await
}

/// `down_ms`/`up_ms` are already durations. (The JS subtracted them from `Date.now()` again,
/// which produced the "20754d downtime" nonsense.)
#[allow(clippy::too_many_arguments)]
fn connection_restored_message(
    model: &str,
    codename: &str,
    time: &str,
    ip: &str,
    isp: &str,
    latency: f64,
    down_ms: i64,
    up_ms: i64,
) -> String {
    format!(
        "<b>✅ Connection Restored</b>\n<blockquote>📱 Device: {}\n📟 Codename: {}\n⏰ Time: {}\n🌐 IP: {}\n🚀 ISP: {}\n🐢 Latency: {latency}ms\n⏱️ Downtime: {}\n⚡ Uptime: {}</blockquote>",
        escape_html(model),
        escape_html(codename),
        time,
        escape_html(ip),
        escape_html(isp),
        format_duration((down_ms / 1000).max(0) as u64),
        format_duration((up_ms / 1000).max(0) as u64),
    )
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
            let mut good_streak = 0u32;
            let mut first_fail_ms: Option<i64> = None;
            let mut pending: Option<ConnAlert> = None;
            let mut state = load_state();
            // A fresh install has no streak yet: start counting from now instead of reporting 0s.
            if state.conn_up_ms == 0 {
                state.conn_up_ms = now_ms();
                save_state(&state);
            }
            loop {
                // Retry a restore message that did not make it out last cycle.
                if let Some(alert) = pending.take() {
                    pending = match try_send_alert(&tg, &http, &alert).await {
                        Ok(()) => {
                            logln!(MOD, "Connection alert delivered (attempt {})", alert.attempts + 1);
                            None
                        }
                        Err(e) => match next_alert_attempt(alert.attempts) {
                            Some(attempts) => {
                                logln!(MOD, "Connection alert attempt {attempts} failed, retrying: {e:#}");
                                Some(ConnAlert { attempts, ..alert })
                            }
                            None => {
                                let attempts = alert.attempts + 1;
                                logln!(MOD, "Connection alert dropped after {attempts} attempts: {e:#}");
                                None
                            }
                        },
                    };
                }

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
                    first_fail_ms = None;
                    good_streak += 1;
                    if let Some(down) = state.conn_down_ms {
                        // A link that just came back usually answers once and then drops again.
                        // Announcing on the first answer is what made the message arrive over a
                        // connection that was still unstable, or not arrive at all.
                        if !restore_is_stable(good_streak) {
                            crate::util::log_once(
                                MOD,
                                "conn-wait-stable",
                                "Connection answered once, waiting for it to hold before announcing",
                            );
                        } else {
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
                                let alert = ConnAlert {
                                    restored_at: chrono::Local::now(),
                                    latency: ms,
                                    down_ms,
                                    up_ms,
                                    attempts: 0,
                                };
                                pending = match try_send_alert(&tg, &http, &alert).await {
                                    Ok(()) => {
                                        logln!(MOD, "Connection alert delivered (attempt 1)");
                                        None
                                    }
                                    Err(e) => {
                                        logln!(MOD, "Connection alert attempt 1 failed, retrying: {e:#}");
                                        Some(ConnAlert { attempts: 1, ..alert })
                                    }
                                };
                            }
                        }
                    }
                } else {
                    fails += 1;
                    good_streak = 0;
                    let started = *first_fail_ms.get_or_insert_with(now_ms);
                    let why = if ping_ok { format!("HTTP={code}") } else { reason };
                    logln!(MOD, "Connection lost {fails}/{CONN_FAIL_LIMIT} ({why})");
                    // Only a real outage is written down; a blip that never reaches the limit
                    // leaves the uptime streak alone.
                    if let Some(start) = outage_start(started, fails) {
                        if state.conn_down_ms.is_none() {
                            state.conn_down_ms = Some(start);
                            save_state(&state);
                        }
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
            is_media: false,
            price_unit: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The seven models the notification actually rendered on 30 Oct 2026, copied from
    /// `GET /api/v1/models?sort=newest&output_modalities=all&limit=1000`.
    fn api_models() -> Vec<(&'static str, Value)> {
        vec![
            (
                "cloudflare/clairic",
                serde_json::json!({
                    "id": "cloudflare/clairic",
                    "name": "Cloudflare: Clairic",
                    "context_length": 65536,
                    "architecture": { "modality": "text+image->decisions", "output_modalities": ["decisions"] },
                    "pricing": { "prompt": "0.0000001", "completion": "0" }
                }),
            ),
            (
                "tbc/mischief-1",
                serde_json::json!({
                    "id": "tbc/mischief-1",
                    "name": "TBC: Mischief 1",
                    "context_length": 0,
                    "architecture": { "modality": "text+image->video", "output_modalities": ["video"] },
                    "pricing": { "prompt": "0", "completion": "0" }
                }),
            ),
            (
                "baseten/joey-1",
                serde_json::json!({
                    "id": "baseten/joey-1",
                    "name": "Baseten: Joey 1",
                    "context_length": 16384,
                    "architecture": { "modality": "text+image->decisions", "output_modalities": ["decisions"] },
                    "pricing": { "prompt": "0.00000001", "completion": "0" }
                }),
            ),
            (
                "sourceful/riverflow-v2.6-pro",
                serde_json::json!({
                    "id": "sourceful/riverflow-v2.6-pro",
                    "name": "Sourceful: Riverflow V2.6 Pro",
                    "context_length": 32768,
                    "architecture": { "modality": "text+image->image", "output_modalities": ["image"] },
                    "pricing": {
                        "prompt": "0",
                        "completion": "0",
                        "image_token": "0.0000311377245508982",
                        "image_output": "0.0000311377245508982"
                    }
                }),
            ),
            (
                "nvidia/nemotron-3.5-transcribe",
                serde_json::json!({
                    "id": "nvidia/nemotron-3.5-transcribe",
                    "name": "NVIDIA: Nemotron 3.5 Transcribe",
                    "context_length": 0,
                    "architecture": { "modality": "audio->transcription", "output_modalities": ["transcription"] },
                    "pricing": { "prompt": "0.0000333", "completion": "0" }
                }),
            ),
            (
                "assemblyai/assemblyai-tts-1",
                serde_json::json!({
                    "id": "assemblyai/assemblyai-tts-1",
                    "name": "AssemblyAI: TTS 1",
                    "context_length": 0,
                    "architecture": { "modality": "text->speech", "output_modalities": ["speech"] },
                    "pricing": { "prompt": "0.000015", "completion": "0" }
                }),
            ),
            (
                "particle-ai/particle-1",
                serde_json::json!({
                    "id": "particle-ai/particle-1",
                    "name": "Particle AI: Particle-1",
                    "context_length": 1048576,
                    "architecture": { "modality": "text+image->text", "output_modalities": ["text"] },
                    "pricing": { "prompt": "0.00000025", "completion": "0.000001" }
                }),
            ),
        ]
    }

    fn cells(id: &str) -> (String, String) {
        let models = api_models();
        let (_, m) = models.iter().find(|(name, _)| *name == id).expect("model in fixture");
        price_cells(&model_row(m))
    }

    #[test]
    fn the_real_october_release_renders_the_prices_openrouter_publishes() {
        // billed per prompt token, so the token price is the one to show; the side that is
        // reported as "0" gets a dash instead of claiming it is free
        assert_eq!(cells("cloudflare/clairic"), ("$0.10".to_string(), "—".to_string()));
        assert_eq!(cells("baseten/joey-1"), ("$0.01".to_string(), "—".to_string()));
        assert_eq!(
            cells("nvidia/nemotron-3.5-transcribe"),
            ("$33.30".to_string(), "—".to_string())
        );
        assert_eq!(cells("assemblyai/assemblyai-tts-1"), ("$15.00".to_string(), "—".to_string()));
        assert_eq!(
            cells("particle-ai/particle-1"),
            ("$0.25".to_string(), "$1.00".to_string())
        );
    }

    #[test]
    fn the_real_media_release_is_priced_per_output() {
        // riverflow publishes only an image price, mischief publishes no price at all
        assert_eq!(
            cells("sourceful/riverflow-v2.6-pro"),
            ("—".to_string(), "From $0.000031".to_string())
        );
        assert_eq!(cells("tbc/mischief-1"), ("—".to_string(), "From".to_string()));
    }

    #[test]
    fn every_fixture_model_is_classified_from_its_own_pricing() {
        let models = api_models();
        for (id, m) in &models {
            let row = model_row(m);
            let token_priced = row.price_in.unwrap_or(0.0) > 0.0 || row.price_out.unwrap_or(0.0) > 0.0;
            assert_eq!(row.is_media, !token_priced, "{id}: media iff no token price");
        }
    }

    #[test]
    fn a_text_model_with_no_charge_on_either_side_is_free() {
        let v: Value = serde_json::json!({
            "architecture": { "output_modalities": ["text"] },
            "pricing": { "prompt": "0", "completion": "0" }
        });
        assert_eq!(
            price_cells(&model_row(&v)),
            ("Free".to_string(), "Free".to_string())
        );
    }

    #[test]
    fn the_count_reads_naturally() {
        assert_eq!(models_count(0), "0 new models");
        assert_eq!(models_count(1), "1 new model");
        assert_eq!(models_count(2), "2 new models");
    }

    #[test]
    fn connection_restored_reports_real_durations() {
        let msg = connection_restored_message(
            "22021211RG",
            "munch",
            "28-10-2026, 16:07:31",
            "1.2.3.4",
            "ISP",
            42.0,
            45_000,
            3_725_000,
        );
        assert!(msg.contains("Downtime: 45s"), "{msg}");
        assert!(msg.contains("Uptime: 1h 2m"), "{msg}");
        assert!(!msg.contains("20754"), "{msg}");
    }

    #[test]
    fn one_clean_check_is_not_enough_to_announce_a_restore() {
        assert!(!restore_is_stable(0));
        assert!(!restore_is_stable(1), "a single answer is what made the alert miss");
        assert!(restore_is_stable(2));
        assert!(restore_is_stable(7));
    }

    #[test]
    fn a_blip_does_not_become_an_outage() {
        let started = 1_000;
        for fails in 1..CONN_FAIL_LIMIT {
            assert_eq!(outage_start(started, fails), None, "{fails} failed checks is not an outage");
        }
        assert_eq!(outage_start(started, CONN_FAIL_LIMIT), Some(started));
        assert_eq!(outage_start(started, CONN_FAIL_LIMIT + 3), Some(started));
    }

    #[test]
    fn alert_retries_stop_after_the_budget() {
        assert_eq!(next_alert_attempt(0), Some(1));
        assert_eq!(next_alert_attempt(ALERT_MAX_ATTEMPTS - 2), Some(ALERT_MAX_ATTEMPTS - 1));
        assert_eq!(next_alert_attempt(ALERT_MAX_ATTEMPTS - 1), None);
        assert_eq!(next_alert_attempt(ALERT_MAX_ATTEMPTS), None);
    }
}

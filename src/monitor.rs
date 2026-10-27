//! Port of monitor.js: usage report, battery watchdog, auto speedtest,
//! OpenRouter new-model + weekly ranking images, Artificial Analysis digest,
//! connection watchdog, monitor.json state and log housekeeping.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{Datelike, Local, TimeZone};
use serde_json::Value;

use crate::app::App;
use crate::util::{
    active_iface, data_dir, escape_html, font_dirs, format_duration_ms, format_kb, log_to, now_ms,
    run, run_timeout,
};

const LOG: &str = "monitor.log";
const INPUT_SUSPEND: &str = "/sys/class/power_supply/battery/input_suspend";
const CONN_URL: &str = "https://www.gstatic.com/generate_204";
const CONN_HOST: &str = "www.gstatic.com";
const CONN_FAIL_LIMIT: i64 = 7;
const CONN_INTERVAL: Duration = Duration::from_secs(10);

fn log(msg: &str) {
    log_to(LOG, msg);
}

// ====================== STATE ======================

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    battery80_sent: bool,
    battery20_sent: bool,
    last_status: String,
    charge_start_time: i64,
    use_start_time: i64,
    conn_up_epoch: i64,
    conn_down_epoch: Option<i64>,
    last_open_router_notified: i64,
    notified_open_router_model_ids: Vec<String>,
    /// Unknown keys from the JS bot survive a load/save round trip.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl Default for State {
    fn default() -> Self {
        let now = now_ms();
        Self {
            battery80_sent: false,
            battery20_sent: false,
            last_status: "UNKNOWN".into(),
            charge_start_time: now,
            use_start_time: now,
            conn_up_epoch: now,
            conn_down_epoch: None,
            last_open_router_notified: 0,
            notified_open_router_model_ids: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }
}

fn state_path() -> std::path::PathBuf {
    data_dir().join("monitor.json")
}

fn load_state() -> State {
    let Ok(raw) = std::fs::read_to_string(state_path()) else {
        return State::default();
    };
    if raw.trim().is_empty() {
        return State::default();
    }
    match serde_json::from_str::<State>(&raw) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("⚠️ monitor.json corrupted, resetting. {e}"));
            State::default()
        }
    }
}

fn state() -> &'static Arc<Mutex<State>> {
    static S: OnceLock<Arc<Mutex<State>>> = OnceLock::new();
    S.get_or_init(|| Arc::new(Mutex::new(load_state())))
}

fn save_state() {
    let snapshot = match state().lock() {
        Ok(s) => s.clone(),
        Err(_) => return,
    };
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_string_pretty(&snapshot) else {
        return;
    };
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    match std::fs::write(&tmp, text) {
        Ok(()) => {
            if let Err(e) = std::fs::rename(&tmp, &path) {
                log(&format!("Failed to save monitor state: {e}"));
            }
        }
        Err(e) => log(&format!("Failed to save monitor state: {e}")),
    }
}

// ====================== TELEGRAM ======================

async fn send(app: &App, text: &str, topic: Option<i64>) {
    if let Err(e) = app.tg.send_html(app.cfg.chat_id, topic, text).await {
        log(&format!("❌ Telegram failed: {e}"));
    }
}

// ====================== USAGE REPORT ======================

async fn usage_report(app: Arc<App>) {
    log("Running scheduled daily usage...");
    let active = active_iface().await;

    let out = run("vnstat", &["--json"]).await;
    let data: Value = match out {
        Ok(o) if o.ok() && o.stderr.trim().is_empty() => match serde_json::from_str(&o.stdout) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("Daily usage parse failed: {e}"));
                return;
            }
        },
        Ok(o) => {
            log(&format!(
                "Daily usage vnstat failed: {}",
                if o.stderr.trim().is_empty() { format!("code {}", o.code) } else { o.stderr.trim().to_string() }
            ));
            send(&app, "❌ Failed to get daily usage", app.cfg.topic_usage).await;
            return;
        }
        Err(e) => {
            log(&format!("Daily usage vnstat failed: {e}"));
            send(&app, "❌ Failed to get daily usage", app.cfg.topic_usage).await;
            return;
        }
    };

    let ifaces = data["interfaces"].as_array().cloned().unwrap_or_default();
    let chosen = ifaces
        .iter()
        .find(|i| i["name"].as_str() == active.as_deref())
        .or_else(|| {
            ifaces
                .iter()
                .filter(|i| i["name"].as_str() != Some("lo"))
                .max_by(|a, b| total_of(a).partial_cmp(&total_of(b)).unwrap_or(std::cmp::Ordering::Equal))
        })
        .or_else(|| ifaces.first())
        .cloned();

    let Some(iface_data) = chosen else {
        log("Daily usage parse failed: vnstat reported no interfaces");
        return;
    };
    let iface = iface_data["name"].as_str().unwrap_or("unknown");

    let today = iface_data["traffic"]["day"]
        .as_array()
        .and_then(|d| d.last())
        .filter(|v| !v.is_null())
        .map(|d| {
            let up = d["tx"].as_f64().unwrap_or(0.0);
            let down = d["rx"].as_f64().unwrap_or(0.0);
            (format_kb(up), format_kb(down), format_kb(up + down))
        });

    let body = match today {
        Some((up, down, total)) => format!(
            "📅 <b>Today</b>\nUpload : {up}\nDownload : {down}\nTotal : {total}"
        ),
        None => "No data".to_string(),
    };
    let report = format!(
        "<b>📊 Daily Usage Report ({iface})</b>\n<blockquote>{body}</blockquote>"
    );
    send(&app, &report, app.cfg.topic_usage).await;
}

fn total_of(i: &Value) -> f64 {
    i["traffic"]["total"]["rx"].as_f64().unwrap_or(0.0)
        + i["traffic"]["total"]["tx"].as_f64().unwrap_or(0.0)
}

// ====================== BATTERY ======================

/// Read-before-write keeps the repeated su calls free.
async fn set_input_suspend(value: u8) {
    let cmd = format!(
        "v=$(cat {INPUT_SUSPEND} 2>/dev/null); [ \"$v\" = \"{value}\" ] || {{ echo {value} > {INPUT_SUSPEND} && echo CHANGED; }}"
    );
    match run("su", &["-c", &cmd]).await {
        Ok(o) if o.ok() => {
            if o.stdout.contains("CHANGED") {
                log(&format!("input_suspend set to {value}"));
            }
        }
        Ok(o) => log(&format!("input_suspend={value} failed: {}", o.stderr.trim())),
        Err(e) => log(&format!("input_suspend={value} failed: {e}")),
    }
}

async fn battery_check(app: Arc<App>) {
    let out = match run("termux-battery-status", &[]).await {
        Ok(o) if o.ok() => o,
        Ok(o) => {
            log(&format!("Battery monitor failed: code {}", o.code));
            return;
        }
        Err(e) => {
            log(&format!("Battery monitor failed: {e}"));
            return;
        }
    };

    let data: Value = match serde_json::from_str(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("Battery monitor parse error: {e}"));
            return;
        }
    };

    let level = data["percentage"].as_i64().unwrap_or(0);
    let status = data["status"].as_str().unwrap_or("UNKNOWN").to_uppercase();
    let temperature = data["temperature"].as_f64().unwrap_or(0.0).round() as i64;

    {
        let mut dirty = false;
        let mut s = state().lock().unwrap();
        let charging = status == "CHARGING" || status == "FULL";
        let using = status == "DISCHARGING" || status == "NOT_CHARGING";

        if charging && (s.last_status == "DISCHARGING" || s.last_status == "NOT_CHARGING") {
            s.charge_start_time = now_ms();
            dirty = true;
        } else if using && (s.last_status == "CHARGING" || s.last_status == "FULL") {
            s.use_start_time = now_ms();
            dirty = true;
        }
        if status != "UNKNOWN" {
            s.last_status = status.clone();
            dirty = true;
        }
        if level < 100 && s.battery80_sent {
            s.battery80_sent = false;
            dirty = true;
        }
        if level > 20 && s.battery20_sent {
            s.battery20_sent = false;
            dirty = true;
        }
        drop(s);
        if dirty {
            save_state();
        }
    }

    if level >= 100 {
        set_input_suspend(1).await;
    } else if level <= 20 {
        set_input_suspend(0).await;
    }

    if level >= 100 {
        let charge_start = {
            let mut s = state().lock().unwrap();
            if s.battery80_sent {
                None
            } else {
                s.battery80_sent = true;
                Some(s.charge_start_time)
            }
        };
        if let Some(start) = charge_start {
            save_state();
            let duration = format_duration_ms(now_ms() - start);
            let text = format!(
                "<b>🔋 Battery Fully Charged</b>\n<blockquote>Percentage : {level}%\nStatus : {status}\nTemperature : {temperature}°C\nDuration : {duration}</blockquote>"
            );
            send(&app, &text, app.cfg.topic_status).await;
        }
    }

    if level <= 20 {
        let use_start = {
            let mut s = state().lock().unwrap();
            if s.battery20_sent {
                None
            } else {
                s.battery20_sent = true;
                Some(s.use_start_time)
            }
        };
        if let Some(start) = use_start {
            save_state();
            let duration = format_duration_ms(now_ms() - start);
            let text = format!(
                "<b>🪫 Low Battery Warning</b>\n<blockquote>Percentage : {level}%\nStatus : {status}\nTemperature : {temperature}°C\nDuration : {duration}</blockquote>"
            );
            send(&app, &text, app.cfg.topic_status).await;
        }
    }
}

// ====================== SPEEDTEST ======================

async fn auto_speedtest(app: Arc<App>) {
    log("Running scheduled speedtest...");
    let out = match run_timeout("speedtest-go", &["--json"], 180).await {
        Ok(o) if o.ok() => o,
        Ok(o) => {
            log(&format!("Scheduled speedtest failed: code {}", o.code));
            return;
        }
        Err(e) => {
            log(&format!("Scheduled speedtest failed: {e}"));
            return;
        }
    };

    let result: Value = match serde_json::from_str(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("Scheduled speedtest JSON parse failed: {e}"));
            return;
        }
    };

    let isp = result["user_info"]["Isp"].as_str().unwrap_or("Unknown");
    let server = &result["servers"][0];
    let name = server["name"].as_str().unwrap_or("Unknown");
    let country = server["country"].as_str().unwrap_or("-");
    let sponsor = server["sponsor"].as_str().unwrap_or("Unknown");
    let latency = server["latency"].as_f64().unwrap_or(0.0) / 1e6;
    let download = server["dl_speed"].as_f64().unwrap_or(0.0) / 125000.0;
    let upload = server["ul_speed"].as_f64().unwrap_or(0.0) / 125000.0;

    let text = format!(
        "<b>⚡ Speed Test Result</b>\n<blockquote>ISP : {}\nServer : {}\nCountry : {} {}\nPing : {latency:.2} ms\nDownload : {download:.2} Mbps\nUpload : {upload:.2} Mbps</blockquote>",
        escape_html(isp),
        escape_html(sponsor),
        escape_html(name),
        escape_html(country)
    );
    send(&app, text.trim(), app.cfg.topic_speed).await;
}

// ====================== SVG HELPERS ======================

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// ponytail: no glyph metrics inside the SVG builder, so text width is an
/// average advance estimate. Close enough for truncation/pagination; swap in
/// a real shaper only if the charts ever need pixel-exact fitting.
fn char_w(ch: char, size: f64, mono: bool) -> f64 {
    if mono {
        size * 0.602
    } else if ch.is_whitespace() {
        size * 0.318
    } else {
        size * 0.55
    }
}

fn text_w(s: &str, size: f64, mono: bool) -> f64 {
    s.chars().map(|c| char_w(c, size, mono)).sum()
}

fn fit(text: &str, max_w: f64, size: f64, mono: bool) -> String {
    if text_w(text, size, mono) <= max_w {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    loop {
        if chars.is_empty() {
            return "…".to_string();
        }
        let cand = format!("{}…", chars.iter().collect::<String>());
        if text_w(&cand, size, mono) <= max_w {
            return cand;
        }
        chars.pop();
    }
}

fn wrap(text: &str, max_w: f64, size: f64, mono: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let next = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if text_w(&next, size, mono) <= max_w {
            line = next;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        let mut cur = String::new();
        for ch in word.chars() {
            let mut trial = cur.clone();
            trial.push(ch);
            if text_w(&trial, size, mono) <= max_w {
                cur = trial;
            } else {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                }
                cur.push(ch);
            }
        }
        line = cur;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

struct Svg {
    b: String,
}

impl Svg {
    fn new(w: f64, h: f64) -> Self {
        Self {
            b: format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}">"#
            ),
        }
    }

    fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, fill: &str, rx: f64) {
        self.b.push_str(&format!(
            r#"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="{rx:.1}" fill="{fill}"/>"#
        ));
    }

    fn line(&mut self, x1: f64, y: f64, x2: f64, color: &str) {
        self.b.push_str(&format!(
            r#"<line x1="{x1:.1}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="{color}" stroke-width="1"/>"#
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, x: f64, y: f64, size: f64, fill: &str, weight: u32, mono: bool, end: bool, s: &str) {
        let family = if mono { "DejaVu Sans Mono" } else { "DejaVu Sans" };
        let anchor = if end { "end" } else { "start" };
        self.b.push_str(&format!(
            r#"<text x="{x:.1}" y="{y:.1}" font-family="{family}" font-size="{size:.0}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{}</text>"#,
            xml_escape(s)
        ));
    }

    fn finish(mut self) -> String {
        self.b.push_str("</svg>");
        self.b
    }
}

fn render_png(svg: &str, w: f64, h: f64) -> Option<Vec<u8>> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let mut opt = usvg::Options::default();
    let db = opt.fontdb_mut();
    db.load_system_fonts();
    for dir in font_dirs() {
        db.load_fonts_dir(dir);
    }

    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let mut pix = tiny_skia::Pixmap::new(w as u32, h as u32)?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pix.as_mut());
    pix.encode_png().ok()
}

// ====================== FORMATTERS ======================

fn format_context(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "Unknown".to_string();
    }
    if value >= 1e6 {
        let m = value / 1e6;
        if value % 1e6 != 0.0 {
            format!("{m:.1}M")
        } else {
            format!("{m:.0}M")
        }
    } else if value >= 1e3 {
        format!("{:.0}K", (value / 1e3).round())
    } else {
        format!("{:.0}", value.round())
    }
}

fn format_price(value: Option<f64>) -> String {
    let Some(n) = value else {
        return "N/A".to_string();
    };
    if !n.is_finite() || n < 0.0 {
        return "N/A".to_string();
    }
    if n == 0.0 {
        return "Free".to_string();
    }
    let per_million = n * 1e6;
    if per_million < 0.01 {
        format!("${per_million:.4}")
    } else {
        format!("${per_million:.2}")
    }
}

fn format_tokens(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "N/A".to_string();
    }
    let scale = |div: f64, suffix: &str| {
        let v = value / div;
        if value % div != 0.0 {
            format!("{v:.1}{suffix}")
        } else {
            format!("{v:.0}{suffix}")
        }
    };
    if value >= 1e12 {
        scale(1e12, "T")
    } else if value >= 1e9 {
        scale(1e9, "B")
    } else if value >= 1e6 {
        scale(1e6, "M")
    } else if value >= 1e3 {
        scale(1e3, "K")
    } else {
        format!("{:.0}", value.round())
    }
}

fn strip_trailing_punct(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':'))
}

// ====================== NEW MODELS IMAGE ======================

struct ModelRow {
    name: String,
    id: String,
    date: String,
    desc: String,
    context: f64,
    prompt: Option<f64>,
    completion: Option<f64>,
}

fn render_new_models(list: &[ModelRow], total: usize, extra: &str) -> (String, f64, f64) {
    const W: f64 = 1200.0;
    const PAD: f64 = 48.0;
    const HEADER_H: f64 = 174.0;
    const ROW_H: f64 = 152.0;
    const GAP: f64 = 8.0;
    const BG: &str = "#050706";
    const PANEL: &str = "#0B0E0C";
    const ROW: &str = "#090C0A";
    const LINE: &str = "#202621";
    const WHITE: &str = "#F7F9F7";
    const MUTED: &str = "#A4ADA6";
    const GREEN: &str = "#76B900";
    const BRIGHT: &str = "#B7F34A";
    const LEADER: &str = "#10180C";

    let content_w = W - PAD * 2.0;
    let h = HEADER_H + list.len() as f64 * (ROW_H + GAP) - GAP + 58.0;
    let mut s = Svg::new(W, h);

    s.rect(0.0, 0.0, W, h, BG, 0.0);
    s.rect(0.0, 0.0, W, 8.0, GREEN, 0.0);

    s.text(PAD, 73.0, 48.0, WHITE, 600, false, false, "New OpenRouter Models");
    s.text(PAD, 111.0, 22.0, BRIGHT, 400, true, false, "NEWEST RELEASES FIRST");
    s.text(W - PAD, 72.0, 18.0, MUTED, 400, true, true, &format!("{total} new models"));
    let (day, month, year) = makassar_date();
    s.text(W - PAD, 104.0, 16.0, MUTED, 400, false, true, &format!("{day} {month} {year}"));

    s.rect(PAD, 130.0, content_w, 34.0, PANEL, 2.0);
    s.text(PAD + 18.0, 153.0, 14.0, MUTED, 600, true, false, "#");
    s.text(146.0, 153.0, 14.0, MUTED, 600, true, false, "MODEL / PROVIDER / RELEASED");
    s.text(700.0, 153.0, 14.0, MUTED, 600, true, true, "CONTEXT");
    s.text(835.0, 153.0, 14.0, MUTED, 600, true, true, "INPUT / M");
    s.text(1030.0, 153.0, 14.0, MUTED, 600, true, true, "OUTPUT / M");

    for (i, m) in list.iter().enumerate() {
        let y = HEADER_H + i as f64 * (ROW_H + GAP);
        let bg = if i == 0 {
            LEADER
        } else if i % 2 == 1 {
            ROW
        } else {
            BG
        };
        s.rect(PAD, y, content_w, ROW_H, bg, 0.0);
        if i == 0 {
            s.rect(PAD, y, 5.0, ROW_H, GREEN, 0.0);
        }
        s.text(
            PAD + 18.0,
            y + 48.0,
            24.0,
            if i == 0 { BRIGHT } else { MUTED },
            700,
            true,
            false,
            &format!("{:02}", i + 1),
        );
        if i == 0 {
            s.text(PAD + 18.0, y + 72.0, 11.0, GREEN, 600, true, false, "LATEST");
        }

        s.text(146.0, y + 39.0, 30.0, WHITE, 600, false, false, &fit(&m.name, 470.0, 30.0, false));
        s.text(
            146.0,
            y + 65.0,
            16.0,
            MUTED,
            400,
            false,
            false,
            &fit(&format!("{} · {}", m.id, m.date), 520.0, 16.0, false),
        );

        s.text(700.0, y + 43.0, 20.0, WHITE, 600, true, true, &format_context(m.context));
        s.text(835.0, y + 43.0, 20.0, WHITE, 600, true, true, &format_price(m.prompt));
        s.text(1030.0, y + 43.0, 20.0, WHITE, 600, true, true, &format_price(m.completion));

        let desc_color = if i == 0 { "#DDE8DD" } else { "#C7D0C9" };
        let lines = wrap(&m.desc, 1000.0, 20.0, false);
        let first = lines.first().cloned().unwrap_or_else(|| "No description".to_string());
        s.text(146.0, y + 96.0, 20.0, desc_color, 400, false, false, &first);
        if lines.len() > 1 {
            let second = if lines.len() > 2 {
                format!("{}…", strip_trailing_punct(&lines[1]))
            } else {
                lines[1].clone()
            };
            s.text(146.0, y + 122.0, 20.0, desc_color, 400, false, false, &second);
        }

        s.line(PAD, y + ROW_H, W - PAD, LINE);
    }

    if !extra.is_empty() {
        s.text(PAD, h - 25.0, 15.0, MUTED, 400, false, false, extra);
    }
    (s.finish(), W, h)
}

// ====================== LEADERBOARD IMAGE ======================

struct LbRow {
    name: String,
    sub: String,
    value: f64,
    text: String,
}

struct LbCfg {
    title: String,
    subtitle: String,
    header_right: (String, String),
    model_column: String,
    metric_column: String,
    value_label: String,
    footer: String,
    footer_right: String,
}

fn render_leaderboard(cfg: &LbCfg, rows: &[LbRow]) -> (String, f64, f64) {
    const W: f64 = 1200.0;
    const PAD: f64 = 48.0;
    const HEADER_H: f64 = 190.0;
    const ROW_H: f64 = 90.0;
    const GAP: f64 = 8.0;
    const BG: &str = "#050706";
    const PANEL: &str = "#0B0E0C";
    const LINE: &str = "#202621";
    const WHITE: &str = "#F7F9F7";
    const MUTED: &str = "#A4ADA6";
    const GREEN: &str = "#76B900";
    const BRIGHT: &str = "#B7F34A";

    let h = HEADER_H + rows.len() as f64 * (ROW_H + GAP) - GAP + 48.0;
    let mut s = Svg::new(W, h);
    s.rect(0.0, 0.0, W, h, BG, 0.0);
    s.rect(0.0, 0.0, W, 8.0, GREEN, 0.0);

    s.text(PAD, 73.0, 48.0, WHITE, 600, false, false, &cfg.title);
    s.text(PAD, 111.0, 22.0, BRIGHT, 400, true, false, &cfg.subtitle);
    s.text(W - PAD, 72.0, 18.0, MUTED, 400, true, true, &cfg.header_right.0);
    s.text(W - PAD, 104.0, 16.0, MUTED, 400, false, true, &cfg.header_right.1);

    s.rect(PAD, 132.0, W - PAD * 2.0, 34.0, PANEL, 2.0);
    s.text(PAD + 18.0, 154.0, 14.0, MUTED, 600, true, false, "RANK");
    s.text(146.0, 154.0, 14.0, MUTED, 600, true, false, &cfg.model_column);
    s.text(W - PAD - 18.0, 154.0, 14.0, MUTED, 600, true, true, &cfg.metric_column);

    let max_value = rows.iter().map(|r| r.value).fold(1.0_f64, f64::max);
    let model_x = 146.0;
    let model_max_w = 590.0;
    let bar_x = 780.0;
    let bar_w = 190.0;
    let number_x = W - PAD - 18.0;

    for (i, r) in rows.iter().enumerate() {
        let y = HEADER_H + i as f64 * (ROW_H + GAP);
        let bg = if i == 0 {
            "#10180C"
        } else if i % 2 == 1 {
            "#090C0A"
        } else {
            BG
        };
        s.rect(PAD, y, W - PAD * 2.0, ROW_H, bg, 0.0);
        if i == 0 {
            s.rect(PAD, y, 5.0, ROW_H, GREEN, 0.0);
        }
        s.text(
            PAD + 18.0,
            y + 51.0,
            24.0,
            if i == 0 { BRIGHT } else { MUTED },
            700,
            true,
            false,
            &format!("{:02}", i + 1),
        );
        s.text(model_x, y + 39.0, 27.0, WHITE, 600, false, false, &fit(&r.name, model_max_w, 27.0, false));
        s.text(model_x, y + 66.0, 17.0, MUTED, 400, false, false, &fit(&r.sub, model_max_w, 17.0, false));

        let bar_width = (bar_w * r.value / max_value).max(2.0);
        s.rect(bar_x, y + 52.0, bar_w, 5.0, LINE, 0.0);
        s.rect(bar_x, y + 52.0, bar_width, 5.0, if i == 0 { BRIGHT } else { GREEN }, 0.0);

        s.text(number_x, y + 51.0, 28.0, WHITE, 700, true, true, &r.text);
        s.text(number_x, y + 73.0, 13.0, MUTED, 400, true, true, &cfg.value_label);

        s.line(PAD, y + ROW_H, W - PAD, LINE);
    }

    s.text(PAD, h - 24.0, 15.0, MUTED, 400, false, false, &cfg.footer);
    s.text(W - PAD, h - 24.0, 15.0, MUTED, 600, true, true, &cfg.footer_right);
    (s.finish(), W, h)
}

/// Asia/Makassar (UTC+8) date parts, as the JS bot formats them.
fn makassar_date() -> (String, String, u32) {
    let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let now = chrono::Utc::now().with_timezone(&tz);
    (
        format!("{:02}", now.day()),
        now.format("%B").to_string().to_uppercase(),
        now.year() as u32,
    )
}

fn makassar_now() -> chrono::DateTime<chrono::FixedOffset> {
    let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    chrono::Utc::now().with_timezone(&tz)
}

// ====================== OPENROUTER MODELS ======================

fn strip_leading_model_name(name: &str, description: &str) -> String {
    let original = description.trim();
    if original.is_empty() {
        return "No description".to_string();
    }
    let name = name.trim();
    let after_colon = match name.find(':') {
        Some(idx) => name[idx + 1..].trim_start().to_string(),
        None => name.to_string(),
    };
    let mut candidates: Vec<String> = Vec::new();
    for c in [name.to_string(), after_colon] {
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        if !original.to_lowercase().starts_with(&candidate.to_lowercase()) {
            continue;
        }
        let rest: String = original.chars().skip(candidate.chars().count()).collect();
        let rest = rest.trim_start();
        let rest = strip_leading_separator(rest);
        let rest = strip_leading_verb(rest);
        let rest = rest.trim_start();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    original.to_string()
}

fn strip_leading_separator(s: &str) -> &str {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if matches!(c, ':' | ',' | '-') => chars.as_str().trim_start_matches([' ', '\t']),
        _ => s,
    }
}

fn strip_leading_verb(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    for verb in ["is", "are"] {
        if lower.starts_with(verb) {
            let rest = &s[verb.len()..];
            if rest.starts_with([' ', '\t']) {
                return rest.trim_start_matches([' ', '\t']);
            }
        }
    }
    s
}

async fn check_openrouter_models(app: Arc<App>) {
    log("Checking OpenRouter models...");
    let url = "https://openrouter.ai/api/v1/models?sort=newest&output_modalities=all&limit=1000";
    let res = match app.http.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            log(&format!("OpenRouter models check failed: {e}"));
            return;
        }
    };
    if !res.status().is_success() {
        log(&format!("OpenRouter models fetch failed: {}", res.status().as_u16()));
        return;
    }
    let data: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => {
            log(&format!("OpenRouter models check failed: {e}"));
            return;
        }
    };

    let models = data["data"].as_array().cloned().unwrap_or_default();
    let cutoff = now_ms() as f64 / 1000.0 - 48.0 * 3600.0;
    let (last_notified, notified) = {
        let s = state().lock().unwrap();
        (s.last_open_router_notified as f64, s.notified_open_router_model_ids.clone())
    };

    let mut new_models: Vec<Value> = models
        .into_iter()
        .filter(|m| {
            let created = m["created"].as_f64().unwrap_or(0.0);
            let id = m["id"].as_str().unwrap_or_default();
            created >= cutoff && (created > last_notified || !notified.iter().any(|n| n == id))
        })
        .collect();
    new_models.sort_by(|a, b| {
        b["created"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["created"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if new_models.is_empty() {
        log("OpenRouter: no new models since last check");
        return;
    }

    let visible = &new_models[..new_models.len().min(20)];
    let list: Vec<ModelRow> = visible
        .iter()
        .map(|m| {
            let id = m["id"].as_str().unwrap_or_default().to_string();
            let name = m["name"].as_str().filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
            let date = match m["created"].as_i64() {
                Some(c) if c > 0 => chrono::DateTime::from_timestamp(c, 0)
                    .map(|d| d.format("%d %b %Y").to_string())
                    .unwrap_or_else(|| "Unknown".into()),
                _ => "Unknown".to_string(),
            };
            let desc = match m["description"].as_str().filter(|s| !s.is_empty()) {
                Some(d) => strip_leading_model_name(&name, d),
                None => "No description".to_string(),
            };
            let desc = if desc.chars().count() > 220 {
                let cut: String = desc.chars().take(220).collect();
                format!("{}…", strip_trailing_punct(cut.trim()))
            } else {
                desc
            };
            let context = m["context_length"]
                .as_f64()
                .filter(|c| *c > 0.0)
                .or_else(|| m["top_provider"]["context_length"].as_f64())
                .unwrap_or(0.0);
            ModelRow {
                name,
                id,
                date,
                desc,
                context,
                prompt: price_of(&m["pricing"]["prompt"]),
                completion: price_of(&m["pricing"]["completion"]),
            }
        })
        .collect();

    let extra = if new_models.len() > 20 {
        format!("{} more models in this update", new_models.len() - 20)
    } else {
        "Source: OpenRouter".to_string()
    };

    let (svg, w, h) = render_new_models(&list, new_models.len(), &extra);
    match render_png(&svg, w, h) {
        Some(png) => {
            let caption = format!(
                "<a href=\"https://openrouter.ai/models\"><b>New OpenRouter Models</b> ({})</a>",
                new_models.len()
            );
            match app
                .tg
                .send_photo_bytes(app.cfg.chat_id, app.cfg.topic_status, png, &caption)
                .await
            {
                Ok(()) => {
                    let newest = new_models
                        .iter()
                        .map(|m| m["created"].as_i64().unwrap_or(0))
                        .max()
                        .unwrap_or(0);
                    let mut s = state().lock().unwrap();
                    for m in visible {
                        if let Some(id) = m["id"].as_str() {
                            if !s.notified_open_router_model_ids.iter().any(|n| n == id) {
                                s.notified_open_router_model_ids.push(id.to_string());
                            }
                        }
                    }
                    let len = s.notified_open_router_model_ids.len();
                    if len > 500 {
                        s.notified_open_router_model_ids.drain(..len - 500);
                    }
                    s.last_open_router_notified = s.last_open_router_notified.max(newest);
                    drop(s);
                    save_state();
                }
                Err(e) => log(&format!("document send failed: {e}")),
            }
        }
        None => log("image render failed (resvg)"),
    }
    log(&format!("OpenRouter: {} new model(s)", new_models.len()));
}

fn price_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            t.parse::<f64>().ok()
        }
        _ => None,
    }
}

// ====================== WEEKLY RANKINGS ======================

fn iso_week_of(year: u32, month: u32, day: u32) -> u32 {
    // Mirrors the JS formula: ceil((dayOfYear + jan1.getUTCDay() + 1) / 7)
    let start = chrono::NaiveDate::from_ymd_opt(year as i32, 1, 1).unwrap();
    let current = chrono::NaiveDate::from_ymd_opt(year as i32, month, day).unwrap();
    let days = (current - start).num_days() as f64;
    let jan1_dow = start.weekday().num_days_from_sunday() as f64;
    (((days + jan1_dow + 1.0) / 7.0).ceil()) as u32
}

fn render_rankings(rows: &[(String, String, f64)]) -> (String, f64, f64) {
    let now = makassar_now();
    let year = now.year() as u32;
    let week = iso_week_of(year, now.month(), now.day());
    let (day, month, _) = makassar_date();
    let cfg = LbCfg {
        title: "OpenRouter weekly".into(),
        subtitle: "TOP 10 BY TOTAL TOKENS".into(),
        header_right: (format!("WEEK {week:02} / {year}"), format!("{day} {month}")),
        model_column: "MODEL / PROVIDER".into(),
        metric_column: "TOKENS PROCESSED".into(),
        value_label: "TOKENS".into(),
        footer: "Prompt + completion tokens · Source: OpenRouter".into(),
        footer_right: "WEEKLY RANKING".into(),
    };
    let lb: Vec<LbRow> = rows
        .iter()
        .map(|(name, author, tokens)| LbRow {
            name: name.clone(),
            sub: author.clone(),
            value: *tokens,
            text: format_tokens(*tokens),
        })
        .collect();
    render_leaderboard(&cfg, &lb)
}

async fn check_top_weekly(app: Arc<App>) {
    log("Checking OpenRouter top weekly...");
    let models_url = "https://openrouter.ai/api/v1/models?sort=top-weekly&limit=10";
    let rankings_url = "https://openrouter.ai/api/frontend/v1/rankings/discovery?view=week&metric=tokens";

    let (models_res, rankings_res) = tokio::join!(
        app.http.get(models_url).send(),
        app.http.get(rankings_url).send()
    );
    let models_res = match models_res {
        Ok(r) => r,
        Err(e) => {
            log(&format!("Rankings check failed: {e}"));
            return;
        }
    };
    let rankings_res = match rankings_res {
        Ok(r) => r,
        Err(e) => {
            log(&format!("Rankings check failed: {e}"));
            return;
        }
    };
    if !models_res.status().is_success() {
        log(&format!("Rankings model fetch failed: {}", models_res.status().as_u16()));
        return;
    }
    if !rankings_res.status().is_success() {
        log(&format!("Rankings token fetch failed: {}", rankings_res.status().as_u16()));
        return;
    }
    let (models_data, rankings_data) = tokio::join!(models_res.json::<Value>(), rankings_res.json::<Value>());
    let (Ok(models_data), Ok(rankings_data)) = (models_data, rankings_data) else {
        log("Rankings check failed: bad JSON");
        return;
    };

    let mut totals: Vec<(String, f64)> = Vec::new();
    if let Some(map) = rankings_data["data"]["totals"].as_object() {
        for (key, entry) in map {
            let model = strip_variant_suffix(key);
            let total = entry["t"].as_f64().unwrap_or(0.0);
            if !total.is_finite() {
                continue;
            }
            match totals.iter_mut().find(|(k, _)| *k == model) {
                Some((_, v)) => *v += total,
                None => totals.push((model, total)),
            }
        }
    }

    let rows: Vec<(String, String, f64)> = models_data["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|model| {
            let id = model["id"].as_str().unwrap_or_default().to_string();
            let canonical = model["canonical_slug"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| strip_port_suffix(&id));
            let tokens = totals.iter().find(|(k, _)| *k == canonical).map(|(_, v)| *v)?;
            let name = model["name"].as_str().filter(|s| !s.is_empty()).unwrap_or(&id);
            let name = strip_vendor_prefix(name);
            let author = id.split('/').next().unwrap_or_default().to_string();
            Some((name, author, tokens))
        })
        .take(10)
        .collect();

    if rows.is_empty() {
        log("Rankings: no rows parsed");
        return;
    }
    log(&format!("Rankings: {} models, #1 {}", rows.len(), rows[0].0));

    let (svg, w, h) = render_rankings(&rows);
    match render_png(&svg, w, h) {
        Some(png) => {
            let caption = "<a href=\"https://openrouter.ai/models?order=top-weekly\"><b>OpenRouter Top Weekly Models</b></a>";
            match app
                .tg
                .send_photo_bytes(app.cfg.chat_id, app.cfg.topic_status, png, caption)
                .await
            {
                Ok(()) => log("Rankings: weekly image sent"),
                Err(e) => log(&format!("top weekly photo send failed: {e}")),
            }
        }
        None => log("Rankings: image render failed (resvg)"),
    }
}

/// `vendor:free` / `:nitro` variant suffix.
fn strip_variant_suffix(key: &str) -> String {
    match key.rfind(':') {
        Some(idx) => {
            let tail = &key[idx + 1..];
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                key[..idx].to_string()
            } else {
                key.to_string()
            }
        }
        None => key.to_string(),
    }
}

fn strip_port_suffix(id: &str) -> String {
    match id.rfind(':') {
        Some(idx) if !id[idx + 1..].contains('/') => id[..idx].to_string(),
        _ => id.to_string(),
    }
}

fn strip_vendor_prefix(name: &str) -> String {
    match name.find(':') {
        Some(idx) => name[idx + 1..].trim_start().to_string(),
        None => name.to_string(),
    }
}

// ====================== ARTIFICIAL ANALYSIS ======================

struct AaRow {
    name: String,
    creator: String,
    context: String,
    intelligence: f64,
    cost_per_task: Option<f64>,
}

fn parse_aa_table(html: &str) -> Vec<AaRow> {
    let tr_re = regex::Regex::new(r"(?is)<tr.*?</tr>").unwrap();
    let cell_re = regex::Regex::new(r"(?is)<t[hd][^>]*>.*?</t[hd]>").unwrap();
    let tag_re = regex::Regex::new(r"(?is)<[^>]+>").unwrap();
    let ent_re = regex::Regex::new(r"&#(\d+);").unwrap();
    let ws_re = regex::Regex::new(r"\s+").unwrap();
    let num_re = regex::Regex::new(r"^-?\d+(\.\d+)?$").unwrap();
    let ctx_re = regex::Regex::new(r"(?i)^\d+(\.\d+)?\s?[KMB]$").unwrap();
    let hdr_re = regex::Regex::new(r"(?i)^(features|intelligence|price|speed|latency|model|end-to-end response time)$").unwrap();

    let clean = |c: &str| -> String {
        let no_tags = tag_re.replace_all(c, " ");
        let s = no_tags
            .replace("&amp;", "&")
            .replace("&nbsp;", " ")
            .replace("&lt;", "<")
            .replace("&gt;", ">");
        let s = ent_re.replace_all(&s, |caps: &regex::Captures| {
            let n: u32 = caps[1].parse().unwrap_or(32);
            char::from_u32(n).unwrap_or(' ').to_string()
        });
        ws_re.replace_all(&s, " ").trim().to_string()
    };

    let num = |v: &str| -> Option<f64> {
        let t = v.replace(['$', ',', '*'], "");
        let t = t.trim();
        if t.is_empty() || t == "--" || !num_re.is_match(t) {
            return None;
        }
        t.parse::<f64>().ok()
    };

    let mut rows = Vec::new();
    for tr in tr_re.find_iter(html) {
        let cells: Vec<String> = cell_re.find_iter(tr.as_str()).map(|m| clean(m.as_str())).collect();
        if cells.len() < 3 {
            continue;
        }
        let name = cells[0].clone();
        if name.is_empty() || hdr_re.is_match(&name) {
            continue;
        }
        let has_ctx = cells.get(1).map(|c| ctx_re.is_match(c)).unwrap_or(false);
        let context = if has_ctx { cells[1].clone() } else { String::new() };
        let creator = if has_ctx {
            cells.get(2).cloned().unwrap_or_default()
        } else {
            cells.get(1).cloned().unwrap_or_default()
        };
        let offset = if has_ctx { 3 } else { 2 };
        let Some(index) = cells.get(offset).and_then(|c| num(c)) else {
            continue;
        };
        rows.push(AaRow {
            name,
            creator,
            context,
            intelligence: index,
            cost_per_task: cells.get(offset + 1).and_then(|c| num(c)),
        });
    }
    rows
}

async fn check_artificial_analysis(app: Arc<App>) {
    log("Checking Artificial Analysis leaderboard...");
    let res = match app
        .http
        .get("https://artificialanalysis.ai/leaderboards/models")
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log(&format!("AA check failed: {e}"));
            return;
        }
    };
    if !res.status().is_success() {
        log(&format!("AA fetch failed: {}", res.status().as_u16()));
        return;
    }
    let html = match res.text().await {
        Ok(t) => t,
        Err(e) => {
            log(&format!("AA check failed: {e}"));
            return;
        }
    };

    let table_re = regex::Regex::new(r"(?is)<table.*?</table>").unwrap();
    let Some(table) = table_re.find(&html) else {
        log("AA: no table in page");
        return;
    };
    let mut rows = parse_aa_table(table.as_str());
    if rows.is_empty() {
        log("AA: no rows parsed");
        return;
    }
    rows.sort_by(|a, b| b.intelligence.partial_cmp(&a.intelligence).unwrap_or(std::cmp::Ordering::Equal));
    log(&format!(
        "AA: {} models, #1 {} ({})",
        rows.len(),
        rows[0].name,
        rows[0].intelligence
    ));

    let (day, month, year) = makassar_date();
    let cfg = LbCfg {
        title: "Artificial Analysis".into(),
        subtitle: "TOP 10 BY INTELLIGENCE INDEX".into(),
        header_right: (year.to_string(), format!("{day} {month}")),
        model_column: "MODEL / CREATOR".into(),
        metric_column: "INTELLIGENCE INDEX".into(),
        value_label: "INDEX".into(),
        footer: "Artificial Analysis Intelligence Index · Source: artificialanalysis.ai".into(),
        footer_right: "LEADERBOARD".into(),
    };
    let lb: Vec<LbRow> = rows
        .iter()
        .take(10)
        .map(|r| {
            let mut parts: Vec<String> = vec![r.creator.clone()];
            if !r.context.is_empty() {
                parts.push(format!("{} ctx", r.context));
            }
            if let Some(cost) = r.cost_per_task {
                parts.push(format!("${cost}/task"));
            }
            LbRow {
                name: r.name.clone(),
                sub: parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · "),
                value: r.intelligence,
                text: format!("{}", r.intelligence),
            }
        })
        .collect();

    let (svg, w, h) = render_leaderboard(&cfg, &lb);
    match render_png(&svg, w, h) {
        Some(png) => {
            let caption = "<a href=\"https://artificialanalysis.ai/leaderboards/models\"><b>Artificial Analysis Top Models</b></a>";
            match app
                .tg
                .send_photo_bytes(app.cfg.chat_id, app.cfg.topic_status, png, caption)
                .await
            {
                Ok(()) => log("AA: leaderboard image sent"),
                Err(e) => log(&format!("AA photo send failed: {e}")),
            }
        }
        None => log("AA: image render failed (resvg)"),
    }
}

// ====================== CONNECTION WATCHDOG ======================

async fn ping_host(host: &str) -> (bool, f64, String) {
    let out = run_timeout("ping", &["-c", "1", "-W", "3", host], 15).await;
    match out {
        Ok(o) if o.ok() => {
            let ms = regex::Regex::new(r"time=([\d.]+)")
                .ok()
                .and_then(|re| re.captures(&o.stdout))
                .and_then(|c| c[1].parse::<f64>().ok())
                .unwrap_or(0.0);
            (true, ms, String::new())
        }
        Ok(o) => {
            let reason = o
                .stdout
                .lines()
                .map(|l| l.trim())
                .find(|l| {
                    let l = l.to_ascii_lowercase();
                    l.contains("unreachable")
                        || l.contains("100% packet loss")
                        || l.contains("unknown host")
                        || l.contains("not known")
                })
                .unwrap_or("no reply")
                .to_string();
            (false, 0.0, reason)
        }
        Err(_) => (false, 0.0, "no reply".to_string()),
    }
}

async fn http_code(app: &App) -> u16 {
    match app
        .http
        .head(CONN_URL)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r.status().as_u16(),
        Err(_) => 0,
    }
}

async fn getprop(prop: &str) -> String {
    match run("getprop", &[prop]).await {
        Ok(o) if o.ok() => {
            let v = o.stdout.trim();
            if v.is_empty() { "N/A".to_string() } else { v.to_string() }
        }
        _ => "N/A".to_string(),
    }
}

async fn connection_info(app: &App) -> (String, String) {
    let ip = app.http.get("https://ipinfo.io/ip").send().await;
    let org = app.http.get("https://ipinfo.io/org").send().await;
    let (Ok(ip), Ok(org)) = (ip, org) else {
        return ("N/A".to_string(), "N/A".to_string());
    };
    let (Ok(ip), Ok(org)) = (ip.text().await, org.text().await) else {
        return ("N/A".to_string(), "N/A".to_string());
    };
    let ip = if ip.trim().is_empty() { "N/A".to_string() } else { ip.trim().to_string() };
    let isp = regex::Regex::new(r"^AS\d+\s+")
        .ok()
        .map(|re| re.replace(org.trim(), "").to_string())
        .unwrap_or_else(|| org.trim().to_string());
    let isp = if isp.is_empty() { "N/A".to_string() } else { isp };
    (ip, isp)
}

async fn send_connection_restored(app: &App, latency: f64, down_ms: i64, up_ms: i64) {
    let (model, codename, net) = tokio::join!(
        getprop("ro.product.model"),
        getprop("ro.product.device"),
        connection_info(app)
    );
    let now = Local::now();
    let timestamp = now.format("%d-%m-%Y, %H:%M:%S").to_string();
    let message = format!(
        "<b>✅ Connection Restored</b>\n<blockquote>📱 Device: {}\n📟 Codename: {}\n⏰ Time: {timestamp}\n🌐 IP: {}\n🚀 ISP: {}\n🐢 Latency: {latency}ms\n⏱️ Downtime: {}\n⚡ Uptime: {}</blockquote>",
        escape_html(&model),
        escape_html(&codename),
        escape_html(&net.0),
        escape_html(&net.1),
        format_duration_ms(down_ms),
        format_duration_ms(up_ms)
    );
    send(app, &message, app.cfg.topic_connection).await;
}

fn watchdog(app: Arc<App>) {
    tokio::spawn(async move {
        let mut conn_up = true;
        let mut fails: i64 = 0;
        loop {
            let started = std::time::Instant::now();
            let (ping_ok, latency, reason) = ping_host(CONN_HOST).await;
            let code = if ping_ok { http_code(&app).await } else { 0 };

            if ping_ok && (code == 200 || code == 204) {
                fails = 0;
                let restored = {
                    let mut s = state().lock().unwrap();
                    match s.conn_down_epoch {
                        Some(down_at) => {
                            let down_ms = now_ms() - down_at;
                            let up_ms = if s.conn_up_epoch > 0 { down_at - s.conn_up_epoch } else { 0 };
                            s.conn_down_epoch = None;
                            s.conn_up_epoch = now_ms();
                            Some((down_ms, up_ms))
                        }
                        None => None,
                    }
                };
                if let Some((down_ms, up_ms)) = restored {
                    let alerted = !conn_up;
                    conn_up = true;
                    save_state();
                    log(&format!(
                        "Connection restored, downtime {}, uptime {}",
                        format_duration_ms(down_ms),
                        format_duration_ms(up_ms)
                    ));
                    if alerted {
                        send_connection_restored(&app, latency, down_ms, up_ms).await;
                    }
                }
            } else {
                fails += 1;
                let why = if ping_ok { format!("HTTP={code}") } else { reason };
                log(&format!("Connection lost {fails}/{CONN_FAIL_LIMIT} ({why})"));
                {
                    let mut s = state().lock().unwrap();
                    if s.conn_down_epoch.is_none() {
                        s.conn_down_epoch = Some(now_ms());
                        drop(s);
                        save_state();
                    }
                }
                if fails >= CONN_FAIL_LIMIT {
                    fails = 0;
                    if conn_up {
                        conn_up = false;
                        log("Connection down, will notify on restore");
                    }
                }
            }

            let spent = started.elapsed();
            tokio::time::sleep(CONN_INTERVAL.saturating_sub(spent)).await;
        }
    });
}

// ====================== LOG HOUSEKEEPING ======================

/// ponytail: monitor.js never trims its own log; this keeps the file bounded
/// (last 1000 lines). Bump the cap if older history is ever needed.
fn trim_log() {
    let path = data_dir().join(LOG);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 1500 {
        return;
    }
    let kept = lines[lines.len() - 1000..].join("\n");
    let _ = std::fs::write(&path, format!("{kept}\n"));
}

// ====================== SCHEDULER ======================

fn at<F, Fut>(hour: u32, minute: u32, job: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let now = Local::now();
            let mut target = now
                .date_naive()
                .and_hms_opt(hour, minute, 0)
                .unwrap_or_else(|| now.naive_local());
            if target <= now.naive_local() {
                target += chrono::Duration::days(1);
            }
            let next = match Local.from_local_datetime(&target) {
                chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => d,
                chrono::LocalResult::None => now,
            };
            let secs = (next - now).num_seconds().max(1) as u64;
            tokio::time::sleep(Duration::from_secs(secs)).await;
            job().await;
        }
    });
}

/// Wall-clock aligned interval, like node-cron's `*/n * * * *`.
fn every_minutes<F, Fut>(n: u64, job: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let period = (n * 60) as i64;
        loop {
            let now = chrono::Utc::now().timestamp();
            let next = (now / period + 1) * period;
            tokio::time::sleep(Duration::from_secs((next - now).max(1) as u64)).await;
            job().await;
        }
    });
}

pub fn spawn(app: Arc<App>) {
    log("Starting monitor service...");
    log(&format!(
        "Connection watchdog: {CONN_HOST} every {}s",
        CONN_INTERVAL.as_secs()
    ));

    let _ = state();

    watchdog(app.clone());

    {
        let a = app.clone();
        at(0, 0, move || {
            let a = a.clone();
            async move { usage_report(a).await }
        });
    }
    for hour in [0, 12] {
        let a = app.clone();
        at(hour, 0, move || {
            let a = a.clone();
            async move { auto_speedtest(a).await }
        });
    }
    {
        let a = app.clone();
        every_minutes(1, move || {
            let a = a.clone();
            async move { battery_check(a).await }
        });
    }
    {
        let a = app.clone();
        every_minutes(15, move || {
            let a = a.clone();
            async move { check_openrouter_models(a).await }
        });
    }
    {
        let a = app.clone();
        at(1, 0, move || {
            let a = a.clone();
            async move { check_top_weekly(a).await }
        });
    }
    {
        let a = app.clone();
        at(1, 30, move || {
            let a = a.clone();
            async move { check_artificial_analysis(a).await }
        });
    }
    at(0, 10, move || async move { trim_log() });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_scale() {
        assert_eq!(format_tokens(1_500_000.0), "1.5M");
        assert_eq!(format_tokens(2_000_000.0), "2M");
        assert_eq!(format_tokens(950.0), "950");
        assert_eq!(format_tokens(-1.0), "N/A");
    }

    #[test]
    fn prices_match_js() {
        assert_eq!(format_price(None), "N/A");
        assert_eq!(format_price(Some(0.0)), "Free");
        assert_eq!(format_price(Some(0.000001)), "$1.00");
        assert_eq!(format_price(Some(0.000000001)), "$0.0010");
    }

    #[test]
    fn context_formats() {
        assert_eq!(format_context(0.0), "Unknown");
        assert_eq!(format_context(128_000.0), "128K");
        assert_eq!(format_context(1_048_576.0), "1.0M");
        assert_eq!(format_context(2_000_000.0), "2M");
    }

    #[test]
    fn strips_leading_model_name() {
        assert_eq!(strip_leading_model_name("Foo: Bar", "Foo: Bar is a great model"), "a great model");
        assert_eq!(strip_leading_model_name("Foo", "Something else entirely"), "Something else entirely");
        assert_eq!(strip_leading_model_name("Foo", ""), "No description");
    }

    #[test]
    fn aa_table_parses() {
        let html = r#"<table>
            <tr><th>Model</th><th>Creator</th><th>Intelligence</th></tr>
            <tr><td>GPT-5</td><td>OpenAI</td><td>70</td></tr>
            <tr><td>Claude</td><td>200K</td><td>Anthropic</td><td>68</td></tr>
        </table>"#;
        let rows = parse_aa_table(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "GPT-5");
        assert_eq!(rows[0].intelligence, 70.0);
        assert!(rows[1].context.contains("200K"));
        assert_eq!(rows[1].creator, "Anthropic");
    }

    #[test]
    fn wrap_and_fit() {
        assert_eq!(wrap("short text", 1000.0, 20.0, false).len(), 1);
        assert!(fit("a very long model name here", 40.0, 30.0, false).ends_with('…'));
        assert_eq!(fit("ok", 400.0, 20.0, false), "ok");
    }

    #[test]
    fn week_number_matches_js_formula() {
        // 2026-01-01 is a Thursday: day 0 + dow 4 + 1 => ceil(5/7) = 1
        assert_eq!(iso_week_of(2026, 1, 1), 1);
        // 2026-10-27: 299 days in, dow 4 => ceil((299+4+1)/7) = 44
        assert_eq!(iso_week_of(2026, 10, 27), 44);
    }

    #[test]
    fn svg_renders_png() {
        let rows: Vec<LbRow> = (0..10)
            .map(|i| LbRow {
                name: format!("anthropic/claude-sonnet-4.{i}"),
                sub: format!("anthropic · 200K ctx · $0.0{i}/task"),
                value: 10.0 - i as f64,
                text: format!("{}", 10 - i),
            })
            .collect();
        let cfg = LbCfg {
            title: "Artificial Analysis".into(),
            subtitle: "TOP 10 BY INTELLIGENCE INDEX".into(),
            header_right: ("2026".into(), "27 OCTOBER".into()),
            model_column: "MODEL / CREATOR".into(),
            metric_column: "INTELLIGENCE INDEX".into(),
            value_label: "INDEX".into(),
            footer: "Artificial Analysis Intelligence Index · Source: artificialanalysis.ai".into(),
            footer_right: "LEADERBOARD".into(),
        };
        let (svg, w, h) = render_leaderboard(&cfg, &rows);
        assert!(svg.starts_with("<svg"));
        let png = render_png(&svg, w, h).expect("svg must rasterize");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn new_models_svg_renders_png() {
        let list: Vec<ModelRow> = (0..4)
            .map(|i| ModelRow {
                name: format!("Vendor: Model {i} Turbo"),
                id: format!("vendor/model-{i}"),
                date: "27 Oct 2026".into(),
                desc: "A capable general purpose model with a long description that will need wrapping across two lines of the card body text.".into(),
                context: 1_048_576.0,
                prompt: Some(0.000001),
                completion: Some(0.000004),
            })
            .collect();
        let (svg, w, h) = render_new_models(&list, 4, "Source: OpenRouter");
        let png = render_png(&svg, w, h).expect("new models svg must rasterize");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}

use std::sync::Arc;

use serde_json::Value;

use crate::app::App;
use crate::tg::Msg;
use crate::util::{escape_html, format_kb, log_to, run};

pub async fn speed(app: Arc<App>, msg: Msg) {
    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let out = run("speedtest-go", &["--json"]).await;
    typing.abort();

    let text = match out {
        Ok(o) if o.ok() => match serde_json::from_str::<Value>(&o.stdout) {
            Ok(r) => {
                let isp = r["user_info"]["Isp"].as_str().unwrap_or("Unknown");
                let server = &r["servers"][0];
                let name = server["name"].as_str().unwrap_or("Unknown");
                let country = server["country"].as_str().unwrap_or("-");
                let sponsor = server["sponsor"].as_str().unwrap_or("Unknown");
                let latency = server["latency"].as_f64().unwrap_or(0.0) / 1e6;
                let dl = server["dl_speed"].as_f64().unwrap_or(0.0) / 125000.0;
                let ul = server["ul_speed"].as_f64().unwrap_or(0.0) / 125000.0;
                Some(format!(
                    "<b>⚡ Speed Test Result</b>\n<blockquote>ISP : {}\nServer : {}\nCountry : {} {}\nPing : {:.2} ms\nDownload : {:.2} Mbps\nUpload : {:.2} Mbps</blockquote>",
                    escape_html(isp),
                    escape_html(sponsor),
                    escape_html(name),
                    escape_html(country),
                    latency,
                    dl,
                    ul
                ))
            }
            Err(e) => {
                log_to("telegram.log", &format!("Speedtest JSON parse failed: {e}"));
                None
            }
        },
        Ok(o) => {
            log_to("telegram.log", &format!("Speedtest failed: code {}", o.code));
            None
        }
        Err(e) => {
            log_to("telegram.log", &format!("Speedtest failed: {e}"));
            None
        }
    };

    let text = text.unwrap_or_else(|| "❌ Failed to run speedtest".to_string());
    let mode = if text.starts_with("<b>") { Some("HTML") } else { None };
    let _ = app
        .tg
        .send_message(msg.chat_id, msg.thread, &text, mode, None, Some(msg.id))
        .await;
}

pub async fn reboot(app: Arc<App>, msg: Msg) {
    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let _ = app
        .tg
        .send_message(msg.chat_id, msg.thread, "♻️ Rebooting system...", None, None, Some(msg.id))
        .await;

    // Same destructive cleanup the JS bot performs before rebooting.
    let cleanup = run(
        "su",
        &["-c", "find /data/data -type f -name '*gms*' -print -exec rm {} \\;"],
    )
    .await;
    typing.abort();

    match cleanup {
        Ok(o) if o.ok() => {
            if !o.stdout.trim().is_empty() {
                log_to("telegram.log", o.stdout.trim());
            }
            let _ = run("su", &["-c", "reboot"]).await;
        }
        Ok(o) => {
            log_to("telegram.log", &format!("GMS cleanup failed: {}", o.stderr.trim()));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to reboot", None, None, Some(msg.id))
                .await;
        }
        Err(e) => {
            log_to("telegram.log", &format!("GMS cleanup failed: {e}"));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to reboot", None, None, Some(msg.id))
                .await;
        }
    }
}

pub async fn battery(app: Arc<App>, msg: Msg) {
    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let out = run("termux-battery-status", &[]).await;
    typing.abort();

    let text = match out {
        Ok(o) if o.ok() => match serde_json::from_str::<Value>(&o.stdout) {
            Ok(d) => format!(
                "<b>🔋 Battery Status</b>\n<pre>Percentage : {}%\nStatus : {}\nTemperature : {}°C</pre>",
                d["percentage"].as_i64().unwrap_or(0),
                d["status"].as_str().unwrap_or("UNKNOWN"),
                d["temperature"].as_f64().unwrap_or(0.0).round() as i64
            ),
            Err(e) => {
                log_to("telegram.log", &format!("Battery parse error: {e}"));
                "❌ Failed to parse battery info".to_string()
            }
        },
        Ok(o) => {
            log_to("telegram.log", &format!("Battery failed: code {}", o.code));
            "❌ Failed to get battery info".to_string()
        }
        Err(e) => {
            log_to("telegram.log", &format!("Battery failed: {e}"));
            "❌ Failed to get battery info".to_string()
        }
    };

    let mode = if text.starts_with("<b>") { Some("HTML") } else { None };
    let _ = app
        .tg
        .send_message(msg.chat_id, msg.thread, &text, mode, None, Some(msg.id))
        .await;
}

pub async fn usage(app: Arc<App>, msg: Msg) {
    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let out = run("vnstat", &["--json"]).await;
    let active = crate::util::active_iface().await;
    typing.abort();

    let text = match out {
        Ok(o) if o.ok() && o.stderr.trim().is_empty() => match serde_json::from_str::<Value>(&o.stdout) {
            Ok(data) => build_usage(&data, active.as_deref()),
            Err(e) => {
                log_to("telegram.log", &format!("vnstat parse failed: {e}"));
                "❌ Failed to parse usage".to_string()
            }
        },
        Ok(o) => {
            log_to("telegram.log", &format!("vnstat failed: {}", o.stderr.trim()));
            "❌ Failed to get usage".to_string()
        }
        Err(e) => {
            log_to("telegram.log", &format!("vnstat failed: {e}"));
            "❌ Failed to get usage".to_string()
        }
    };

    let mode = if text.starts_with("<b>") { Some("HTML") } else { None };
    let _ = app
        .tg
        .send_message(msg.chat_id, msg.thread, &text, mode, None, Some(msg.id))
        .await;
}

fn traffic_of(node: &Value) -> Option<String> {
    if node.is_null() {
        return None;
    }
    let up = node["tx"].as_f64().unwrap_or(0.0);
    let down = node["rx"].as_f64().unwrap_or(0.0);
    Some(format!(
        "Upload : {}\nDownload : {}\nTotal : ",
        format_kb(up),
        format_kb(down)
    ))
}

/// last element of a vnstat array
fn last_of(node: &Value) -> Option<&Value> {
    node.as_array().and_then(|a| a.last())
}

fn block(title: &str, entry: Option<&Value>) -> String {
    match entry.and_then(traffic_of) {
        Some(body) => {
            let up = entry.map(|e| e["tx"].as_f64().unwrap_or(0.0)).unwrap_or(0.0);
            let down = entry.map(|e| e["rx"].as_f64().unwrap_or(0.0)).unwrap_or(0.0);
            format!("{title}\n{body}{}\n", format_kb(up + down))
        }
        None => "No data\n".to_string(),
    }
}

fn build_usage(data: &Value, active: Option<&str>) -> String {
    let ifaces = data["interfaces"].as_array().cloned().unwrap_or_default();
    let chosen = ifaces
        .iter()
        .find(|i| i["name"].as_str() == active)
        .or_else(|| {
            ifaces
                .iter()
                .filter(|i| i["name"].as_str() != Some("lo"))
                .max_by(|a, b| {
                    let ta = a["traffic"]["total"]["rx"].as_f64().unwrap_or(0.0)
                        + a["traffic"]["total"]["tx"].as_f64().unwrap_or(0.0);
                    let tb = b["traffic"]["total"]["rx"].as_f64().unwrap_or(0.0)
                        + b["traffic"]["total"]["tx"].as_f64().unwrap_or(0.0);
                    ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
                })
        })
        .or_else(|| ifaces.first());

    let name = chosen.and_then(|c| c["name"].as_str()).unwrap_or("unknown");
    let traffic = chosen.map(|c| &c["traffic"]).unwrap_or(&Value::Null);

    let day = block("📅 <b>Today</b>", last_of(&traffic["day"]));
    let week = match last_of(&traffic["week"]) {
        Some(entry) => block("\n📆 <b>This Week</b>", Some(entry)),
        None => {
            let days = traffic["day"].as_array().cloned().unwrap_or_default();
            if days.is_empty() {
                "No data\n".to_string()
            } else {
                let last7: Vec<&Value> = days.iter().rev().take(7).collect();
                let tx: f64 = last7.iter().map(|d| d["tx"].as_f64().unwrap_or(0.0)).sum();
                let rx: f64 = last7.iter().map(|d| d["rx"].as_f64().unwrap_or(0.0)).sum();
                format!(
                    "\n📆 <b>This Week</b>\nUpload : {}\nDownload : {}\nTotal : {}\n",
                    format_kb(tx),
                    format_kb(rx),
                    format_kb(tx + rx)
                )
            }
        }
    };
    let month = block("\n🗓️ <b>This Month</b>", last_of(&traffic["month"]));

    format!("<b>📊 Data Usage ({name})</b>\n<blockquote>{day}{week}{month}</blockquote>")
        .trim()
        .to_string()
}

pub async fn approve(app: Arc<App>, msg: Msg) {
    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let out = run("su", &["-c", "input tap 150 1260 && sleep 2 && input tap 880 1435"]).await;
    typing.abort();

    let (text, mode) = match out {
        Ok(o) if o.ok() => {
            if !o.stdout.trim().is_empty() {
                log_to("telegram.log", o.stdout.trim());
            }
            ("✅ Approve executed", None)
        }
        Ok(o) => {
            log_to("telegram.log", &format!("Approve failed: {}", o.stderr.trim()));
            ("❌ Failed to run approve", None)
        }
        Err(e) => {
            log_to("telegram.log", &format!("Approve failed: {e}"));
            ("❌ Failed to run approve", None)
        }
    };
    let _ = app
        .tg
        .send_message(msg.chat_id, msg.thread, text, mode, None, Some(msg.id))
        .await;
}

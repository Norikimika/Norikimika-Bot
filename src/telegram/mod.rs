pub mod device;
pub mod media;
pub mod music;
pub mod price;
pub mod receipt;

use std::sync::Arc;

use serde_json::Value;

use crate::app::App;
use crate::tg::{Cb, Msg};

const HELP_TEXT: &str = "<b>🤖 Public Commands</b>\n\
<blockquote><b>✹ Price List</b>\n\
-> <code>/p [name]</code></blockquote>\n\
<b>⚠️ Admin Commands</b>\n\
<blockquote><b>✹ Approve</b>\n\
-> /approve</blockquote>\n\
<blockquote><b>✹ Battery Status</b>\n\
-> /battery</blockquote>\n\
<blockquote><b>✹ Download Music</b>\n\
-> <code>/music [title]</code>\n\
-> <code>/music2 [title]</code></blockquote>\n\
<blockquote><b>✹ Download TikTok/Douyin</b>\n\
-> <code>/tt [link]</code></blockquote>\n\
<blockquote><b>✹ Data Usage</b>\n\
-> /usage</blockquote>\n\
<blockquote><b>✹ Playback Controller</b>\n\
-> /playback</blockquote>\n\
<blockquote><b>✹ Reboot Device</b>\n\
-> /reboot</blockquote>\n\
<blockquote><b>✹ Speed Test</b>\n\
-> /speed</blockquote>";

pub async fn handle_update(app: Arc<App>, update: Value) {
    if let Some(cb) = Cb::parse(&update) {
        handle_callback(app, cb).await;
        return;
    }
    if let Some(msg) = Msg::parse(&update) {
        crate::util::log_to(
            "telegram.log",
            &format!("From {}: {}", msg.username, msg.log_summary()),
        );
        handle_message(app, msg).await;
    }
}

async fn handle_message(app: Arc<App>, msg: Msg) {
    let topic = app.cfg.topic_id;
    let in_topic = msg.thread == topic;

    // Photos go to the receipt flow in the intake topic, not the main topic.
    // handle_photo does its own admin gate + intake-topic check (JS adminOnly + topic filter).
    if !msg.photo.is_empty() {
        receipt::handle_photo(app.clone(), &msg).await;
        return;
    }

    let text = msg.text.trim().to_string();
    if text.is_empty() {
        return;
    }

    if !text.starts_with('/') {
        // Commands-only topic: mute everyone else for 5 minutes.
        if in_topic && !app.is_admin(msg.chat_id, msg.user_id).await {
            let until = chrono::Utc::now().timestamp() + 300;
            app.tg.mute_until(msg.chat_id, msg.user_id, until).await;
            let _ = app
                .tg
                .send_message(
                    msg.chat_id,
                    msg.thread,
                    "⛔ Commands only! You are muted for 5 minutes.",
                    None,
                    None,
                    Some(msg.id),
                )
                .await;
        }
        return;
    }

    let (cmd, arg) = split_command(&text);
    let addressed = cmd.to_ascii_lowercase();
    let topic_music = app.cfg.topic_music;
    let topic_music2 = app.cfg.topic_music2;

    match addressed.as_str() {
        // /p is the only public command.
        "p" => {
            if !in_topic {
                return;
            }
            price::handle(app, msg, arg).await;
        }
        "help" => {
            if !in_topic {
                return;
            }
            let typing = app.typing(msg.chat_id, msg.thread, "typing");
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, HELP_TEXT, Some("HTML"), None, Some(msg.id))
                .await;
            typing.abort();
        }
        _ => {
            if !in_topic {
                return;
            }
            // Unknown commands fall through to the help card with no admin gate,
            // exactly like the catch-all /^(\w+)/ handler in index.js.
            let known = matches!(
                addressed.as_str(),
                "speed" | "reboot" | "battery" | "usage" | "approve" | "music" | "music2" | "playback" | "tt"
            );
            if known && !app.is_admin(msg.chat_id, msg.user_id).await {
                app.deny(msg.chat_id, msg.thread, Some(msg.id), None, msg.user_id).await;
                return;
            }
            match addressed.as_str() {
                "speed" => device::speed(app, msg).await,
                "reboot" => device::reboot(app, msg).await,
                "battery" => device::battery(app, msg).await,
                "usage" => device::usage(app, msg).await,
                "approve" => device::approve(app, msg).await,
                "music" => music::handle_music(app, msg, topic_music).await,
                "music2" => music::handle_music(app, msg, topic_music2).await,
                "playback" => music::handle_playback(app, msg).await,
                "tt" => media::handle_tt(app, msg).await,
                // Unknown slash command inside the topic: show the help card.
                _ => {
                    let _ = app
                        .tg
                        .send_message(msg.chat_id, msg.thread, HELP_TEXT, Some("HTML"), None, Some(msg.id))
                        .await;
                }
            }
        }
    }
}

/// "/cmd@bot args" → ("cmd", "args")
fn split_command(text: &str) -> (String, String) {
    let body = text.trim_start_matches('/');
    let mut parts = body.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let cmd = head.split('@').next().unwrap_or_default().to_string();
    (cmd, parts.next().unwrap_or_default().trim().to_string())
}

async fn handle_callback(app: Arc<App>, cb: Cb) {
    if cb.thread != app.cfg.topic_id {
        return;
    }
    if !app.is_admin(cb.chat_id, cb.user_id).await {
        app.deny(cb.chat_id, cb.thread, None, Some(&cb.id), cb.user_id).await;
        return;
    }
    music::handle_callback(app, cb).await;
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::app::App;
use crate::tg::{Cb, Msg};
use crate::util::{escape_html, format_mmss, log_to, run, run_timeout};

const YT_AUDIO_FORMAT: &str = "251/140/bestaudio";
/// player_client=web_embedded: the only client that survives YouTube's bot check here.
const YT_PRIMARY_CLIENT_ARG: &str = "youtube:player_client=web_embedded";
const AUDIO_EXTS: [&str; 8] = ["webm", "m4a", "mp3", "opus", "ogg", "aac", "flac", "wav"];
const MAX_AUDIO_BYTES: u64 = 49 * 1024 * 1024;

fn ytdlp(app: &App, args: Vec<String>) -> Vec<String> {
    app.cfg.ytdlp_args(args)
}

fn args_slice(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

fn workdir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn find_downloaded_audio(file_id: &str) -> Option<PathBuf> {
    let dir = workdir();
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        let ext = Path::new(&name).extension().and_then(|x| x.to_str()).unwrap_or("");
        (name.starts_with(file_id) && AUDIO_EXTS.contains(&ext)).then(|| e.path())
    })
}

fn status_text(title: &str, performer: &str, duration: &str, status: &str) -> String {
    format!(
        "<b>🎵 Music Info</b>\n<blockquote><b>📝 Title:</b>\n-> <code>{}</code>\n<b>👤 Artist:</b>\n-> <code>{}</code>\n<b>⏳ Duration:</b>\n-> <code>{}</code>\n<b>📦 Status:</b>\n-> <code>{}</code></blockquote>",
        escape_html(title),
        escape_html(performer),
        duration,
        escape_html(status)
    )
}

async fn update_status(app: &App, chat: i64, message_id: i64, title: &str, performer: &str, duration: &str, status: &str) {
    let _ = app
        .tg
        .edit_html(chat, message_id, &status_text(title, performer, duration, status), None)
        .await;
}

pub async fn handle_music(app: Arc<App>, msg: Msg, target_topic: Option<i64>) {
    let query = msg.text.trim().trim_start_matches('/');
    let query = query
        .splitn(2, char::is_whitespace)
        .nth(1)
        .unwrap_or_default()
        .trim()
        .to_string();

    if query.is_empty() {
        let _ = app
            .tg
            .send_message(msg.chat_id, msg.thread, "❌ Please provide a song title or link.", None, None, Some(msg.id))
            .await;
        return;
    }

    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let thread = target_topic.or(msg.thread);

    let search_arg = if query.starts_with("http") {
        query.clone()
    } else if query.to_ascii_lowercase().starts_with("sc:") {
        format!("scsearch1:{}", query[3..].trim())
    } else {
        format!("ytsearch1:{query}")
    };

    let probe_args = ytdlp(
        &app,
        vec![
            "-f".into(), YT_AUDIO_FORMAT.into(),
            "--no-playlist".into(),
            "--extractor-args".into(), YT_PRIMARY_CLIENT_ARG.into(),
            "-j".into(), search_arg,
        ],
    );
    let probe = run_timeout("yt-dlp", &args_slice(&probe_args), 180).await;

    let info: Value = match probe {
        Ok(o) if o.ok() => match serde_json::from_str(&o.stdout) {
            Ok(v) => v,
            Err(e) => {
                typing.abort();
                log_to("telegram.log", &format!("[FETCH ERROR] bad yt-dlp JSON: {e}"));
                let _ = app
                    .tg
                    .send_message(msg.chat_id, msg.thread, "❌ Failed to fetch music.", None, None, Some(msg.id))
                    .await;
                return;
            }
        },
        Ok(o) => {
            typing.abort();
            log_to("telegram.log", &format!("[FETCH ERROR] yt-dlp: {}", o.stderr.trim()));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to fetch music.", None, None, Some(msg.id))
                .await;
            return;
        }
        Err(e) => {
            typing.abort();
            log_to("telegram.log", &format!("[FETCH ERROR] {e}"));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to fetch music.", None, None, Some(msg.id))
                .await;
            return;
        }
    };

    let title = ["track", "title"]
        .iter()
        .find_map(|k| info[*k].as_str())
        .unwrap_or("Unknown Title")
        .to_string();
    let performer = ["artist", "uploader", "uploader_id"]
        .iter()
        .find_map(|k| info[*k].as_str())
        .unwrap_or("Unknown Artist")
        .to_string();
    let duration = format_mmss(info["duration"].as_f64());
    let media_url = info["webpage_url"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| query.starts_with("http").then(|| query.clone()))
        .or_else(|| info["url"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let yt_id = info["id"].as_str().unwrap_or("audio").to_string();

    let file_id = format!("{}_{}", chrono::Utc::now().timestamp_millis(), yt_id);
    let audio_out = workdir().join(format!("{file_id}.%(ext)s"));
    let thumb_base = workdir().join(&file_id);
    let thumb_file = PathBuf::from(format!("{}.jpg", thumb_base.display()));

    let info_message = match app
        .tg
        .send_message(
            msg.chat_id,
            msg.thread,
            &status_text(&title, &performer, &duration, "Downloading"),
            Some("HTML"),
            None,
            Some(msg.id),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            typing.abort();
            log_to("telegram.log", &format!("[INFO CARD ERROR] {e}"));
            return;
        }
    };

    let dl_args = ytdlp(
        &app,
        vec![
            "-f".into(), YT_AUDIO_FORMAT.into(),
            "--no-playlist".into(),
            "--extractor-args".into(), YT_PRIMARY_CLIENT_ARG.into(),
            "-o".into(), audio_out.display().to_string(),
            "--write-thumbnail".into(),
            "--convert-thumbnails".into(), "jpg".into(),
            "-o".into(), format!("thumbnail:{}", thumb_base.display()),
            media_url,
        ],
    );
    let dl = run_timeout("yt-dlp", &args_slice(&dl_args), 600).await;

    let mut downloaded: Option<PathBuf> = None;
    let result: anyhow::Result<()> = async {
        match dl {
            Ok(o) if o.ok() => {}
            Ok(o) => anyhow::bail!("yt-dlp exit {}: {}", o.code, o.stderr.trim()),
            Err(e) => return Err(e),
        }
        let file = find_downloaded_audio(&file_id)
            .ok_or_else(|| anyhow::anyhow!("Downloaded audio file not found."))?;
        let size = std::fs::metadata(&file)?.len();
        downloaded = Some(file.clone());
        if size > MAX_AUDIO_BYTES {
            update_status(&app, msg.chat_id, info_message, &title, &performer, &duration, "Too Large").await;
            anyhow::bail!("file too large");
        }

        update_status(&app, msg.chat_id, info_message, &title, &performer, &duration, "Sending").await;
        typing.abort();
        app.tg.chat_action(msg.chat_id, thread, "upload_audio").await;

        let thumb = thumb_file.exists().then_some(thumb_file.as_path());
        app.tg
            .upload_audio(msg.chat_id, thread, &file, &title, &performer, info["duration"].as_f64(), thumb)
            .await?;
        update_status(&app, msg.chat_id, info_message, &title, &performer, &duration, "Success").await;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        if !e.to_string().contains("file too large") {
            log_to("telegram.log", &format!("[SEND AUDIO ERROR] {e}"));
            update_status(&app, msg.chat_id, info_message, &title, &performer, &duration, "Failed").await;
        }
    }

    // Cleanup: audio, jpg/webp/png thumbnail.
    if let Some(f) = downloaded {
        let _ = std::fs::remove_file(f);
    }
    for ext in ["jpg", "webp", "png"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.{ext}", thumb_base.display())));
    }
    typing.abort();
}

#[derive(Debug, Clone)]
struct Stream {
    name: String,
    volume: i64,
    max_volume: i64,
}

async fn volumes() -> anyhow::Result<Vec<Stream>> {
    let out = run("termux-volume", &[]).await?;
    if !out.ok() {
        anyhow::bail!("termux-volume failed");
    }
    let v: Value = serde_json::from_str(&out.stdout)?;
    Ok(v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|s| Stream {
            name: s["stream"].as_str().unwrap_or("music").to_string(),
            volume: s["volume"].as_i64().unwrap_or(0),
            max_volume: s["max_volume"].as_i64().unwrap_or(15),
        })
        .collect())
}

fn home_text(streams: &[Stream]) -> String {
    let mut body = String::new();
    for s in streams {
        body.push_str(&format!(
            "<b>Stream</b>\n-> {}\n<b>Current</b>\n-> {}\n<b>Maximum</b>\n-> {}\n",
            s.name, s.volume, s.max_volume
        ));
    }
    format!("<b>🔊 Playback Control</b>\n<blockquote>{body}</blockquote>")
}

fn home_keyboard(streams: &[Stream]) -> Value {
    json!({
        "inline_keyboard": streams
            .iter()
            .map(|s| json!([{ "text": s.name, "callback_data": format!("volume_select_{}", s.name) }]))
            .collect::<Vec<Value>>()
    })
}

fn control_text(s: &Stream) -> String {
    format!(
        "<b>🔊 Playback Control</b>\n<blockquote><b>Stream</b>\n-> {}\n<b>Current</b>\n-> {}\n<b>Maximum</b>\n-> {}</blockquote>",
        s.name, s.volume, s.max_volume
    )
}

fn control_keyboard(name: &str, s: &Stream) -> Value {
    let clamp = |v: i64| v.clamp(0, s.max_volume);
    json!({
        "inline_keyboard": [
            [
                {"text": "➖5", "callback_data": format!("volume_set_{name}_{}", clamp(s.volume - 5))},
                {"text": "➖1", "callback_data": format!("volume_set_{name}_{}", clamp(s.volume - 1))},
                {"text": "➕1", "callback_data": format!("volume_set_{name}_{}", clamp(s.volume + 1))},
                {"text": "➕5", "callback_data": format!("volume_set_{name}_{}", clamp(s.volume + 5))}
            ],
            [
                {"text": "🔇 Mute", "callback_data": format!("volume_set_{name}_0")},
                {"text": "🔊 Max", "callback_data": format!("volume_set_{name}_{}", s.max_volume)}
            ],
            [
                {"text": "⏮️", "callback_data": "media_previous"},
                {"text": "▶️", "callback_data": "media_play"},
                {"text": "⏹️", "callback_data": "media_stop"},
                {"text": "⏭️", "callback_data": "media_next"}
            ],
            [{"text": "⬅️ Back", "callback_data": "volume_home"}]
        ]
    })
}

pub async fn handle_playback(app: Arc<App>, msg: Msg) {
    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let streams = volumes().await;
    typing.abort();

    match streams {
        Ok(list) if !list.is_empty() => {
            let _ = app
                .tg
                .send_message(
                    msg.chat_id,
                    msg.thread,
                    &home_text(&list),
                    Some("HTML"),
                    Some(home_keyboard(&list)),
                    Some(msg.id),
                )
                .await;
        }
        Ok(_) => {
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to parse volume information", None, None, Some(msg.id))
                .await;
        }
        Err(e) => {
            log_to("telegram.log", &format!("Volume read failed: {e}"));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to get volume information", None, None, Some(msg.id))
                .await;
        }
    }
}

pub async fn handle_callback(app: Arc<App>, cb: Cb) {
    let data = cb.data.as_str();

    if let Some(stream) = data.strip_prefix("volume_select_") {
        match volumes().await {
            Ok(list) => match list.iter().find(|s| s.name.eq_ignore_ascii_case(stream)) {
                Some(s) => {
                    if let Some(mid) = cb.message_id {
                        let _ = app
                            .tg
                            .edit_html(cb.chat_id, mid, &control_text(s), Some(control_keyboard(stream, s)))
                            .await;
                    }
                    app.tg.answer_callback(&cb.id, "", false).await;
                }
                None => app.tg.answer_callback(&cb.id, "Invalid stream", true).await,
            },
            Err(_) => app.tg.answer_callback(&cb.id, "Failed to get volume information", true).await,
        }
        return;
    }

    if let Some(rest) = data.strip_prefix("volume_set_") {
        let mut parts = rest.rsplitn(2, '_');
        let value: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(-1);
        let stream = parts.next().unwrap_or_default().to_string();
        if value < 0 {
            app.tg.answer_callback(&cb.id, "Failed", true).await;
            return;
        }
        let set = run("termux-volume", &[stream.as_str(), &value.to_string()]).await;
        match set {
            Ok(o) if o.ok() => match volumes().await {
                Ok(list) => match list.iter().find(|s| s.name.eq_ignore_ascii_case(&stream)) {
                    Some(s) => {
                        if let Some(mid) = cb.message_id {
                            let _ = app
                                .tg
                                .edit_html(cb.chat_id, mid, &control_text(s), Some(control_keyboard(&stream, s)))
                                .await;
                        }
                        app.tg
                            .answer_callback(&cb.id, &format!("{}: {}/{}", s.name, s.volume, s.max_volume), false)
                            .await;
                    }
                    None => app.tg.answer_callback(&cb.id, "Invalid stream", true).await,
                },
                Err(_) => app.tg.answer_callback(&cb.id, "Failed to refresh volume", true).await,
            },
            _ => {
                log_to("telegram.log", "Volume set failed");
                app.tg.answer_callback(&cb.id, "Failed to set volume", true).await;
            }
        }
        return;
    }

    if data == "volume_home" {
        match volumes().await {
            Ok(list) => {
                if let Some(mid) = cb.message_id {
                    let _ = app
                        .tg
                        .edit_html(cb.chat_id, mid, &home_text(&list), Some(home_keyboard(&list)))
                        .await;
                }
                app.tg.answer_callback(&cb.id, "", false).await;
            }
            Err(_) => app.tg.answer_callback(&cb.id, "Failed", true).await,
        }
        return;
    }

    if let Some(action) = data.strip_prefix("media_") {
        if matches!(action, "play" | "next" | "previous" | "stop") {
            let out = run("cmd", &["media_session", "dispatch", action]).await;
            match out {
                Ok(o) if o.ok() => app.tg.answer_callback(&cb.id, &format!("{action} executed"), false).await,
                _ => app.tg.answer_callback(&cb.id, &format!("Failed: {action}"), true).await,
            }
        }
    }
}

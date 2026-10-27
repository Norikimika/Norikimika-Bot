use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::app::App;
use crate::tg::Msg;
use crate::util::{escape_html, log_to};

/// Telegram bot upload ceiling (50MB rounded down), same guard as music.rs.
const MAX_BYTES: u64 = 49 * 1024 * 1024;
const GROUP_LIMIT: usize = 10;
const MAX_TITLE: usize = 80;
const ERROR_URL_MISSING: &str =
    "❌ Please provide a TikTok/Douyin URL or reply to a message containing one.";

pub async fn handle_tt(app: Arc<App>, msg: Msg) {
    // TOPIC_ID_TIKTOK || threadId
    let target = app.cfg.topic_tiktok.or(msg.thread);
    let reply_text = msg.raw["reply_to_message"]["text"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let combined = format!("{} {}", strip_tt_arg(&msg.text), reply_text);

    let Some(url) = find_url(&combined) else {
        let _ = app
            .tg
            .send_message(msg.chat_id, msg.thread, ERROR_URL_MISSING, None, None, Some(msg.id))
            .await;
        return;
    };
    let platform = if is_douyin(&url) { "Douyin" } else { "TikTok" };

    let typing = app.typing(msg.chat_id, target, "upload_document");
    let dir = temp_dir();
    let _ = tokio::fs::create_dir_all(&dir).await;

    let info = match app
        .tg
        .send_message(
            msg.chat_id,
            msg.thread,
            &status_text(platform, "Fetching...", "Fetching...", "Downloading"),
            Some("HTML"),
            None,
            Some(msg.id),
        )
        .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            log_to("telegram.log", &format!("[TT INFO CARD ERROR] {e}"));
            None
        }
    };

    if let Err(e) = transfer(&app, &msg, &url, platform, target, &dir, info).await {
        log_to("telegram.log", &format!("[TT ERROR] {e}"));
        if let Some(mid) = info {
            update_status(&app, msg.chat_id, mid, platform, "Unknown", "Error", "Failed").await;
        }
    }

    typing.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

async fn transfer(
    app: &App,
    msg: &Msg,
    url: &str,
    platform: &str,
    target: Option<i64>,
    dir: &Path,
    info: Option<i64>,
) -> anyhow::Result<()> {
    let douyin = platform == "Douyin";
    let api = fetch_api(app, url, douyin).await?;
    let media = parse_media(&api, douyin);
    let title = truncate(&media.title, MAX_TITLE);

    if let Some(mid) = info {
        update_status(app, msg.chat_id, mid, platform, &media.uploader, &title, "Downloading").await;
    }

    let mut video: Option<PathBuf> = None;
    let mut photos: Vec<PathBuf> = Vec::new();
    let mut slides: Vec<(bool, PathBuf)> = Vec::new();

    if !media.live.is_empty() {
        for i in 0..media.live.len().max(media.images.len()) {
            match media.live.get(i).filter(|s| !s.is_empty()) {
                Some(u) => {
                    let p = save(app, dir, &format!("slide_{:02}.mp4", i + 1), u).await?;
                    slides.push((true, p));
                }
                None => {
                    if let Some(u) = media.images.get(i) {
                        let p = save(app, dir, &format!("slide_{:02}.jpg", i + 1), u).await?;
                        slides.push((false, p));
                    }
                }
            }
        }
    } else if !media.images.is_empty() {
        for (i, u) in media.images.iter().enumerate() {
            photos.push(save(app, dir, &format!("photo_{:02}.jpg", i + 1), u).await?);
        }
    } else {
        let u = media
            .video
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No video URL found in API response"))?;
        video = Some(save(app, dir, "video.mp4", u).await?);
    }

    if video.is_none() && photos.is_empty() && slides.is_empty() {
        anyhow::bail!("No media downloaded from {platform} response");
    }

    // Soundtrack is best effort, exactly like the JS try {} catch {}.
    let audio = match &media.music {
        Some(u) => save(app, dir, "audio.mp3", u).await.ok(),
        None => None,
    };

    if let Some(mid) = info {
        update_status(app, msg.chat_id, mid, platform, &media.uploader, &title, "Sending").await;
    }

    // ponytail: Tg has no mixed video+photo album, so live slides upload clips one by one
    // and the stills of each 10-item chunk go out as an album. Add a mixed media-group
    // helper to tg.rs if a single album per chunk ever matters.
    if !slides.is_empty() {
        for chunk in slides.chunks(GROUP_LIMIT) {
            let mut album: Vec<Vec<u8>> = Vec::new();
            for (is_video, path) in chunk {
                if *is_video {
                    if let Err(e) = app.tg.upload_video(msg.chat_id, target, path, "").await {
                        log_to("telegram.log", &format!("safeReplyWithVideo error: {e}"));
                    }
                } else if let Ok(bytes) = tokio::fs::read(path).await {
                    album.push(bytes);
                }
            }
            if !album.is_empty() {
                if let Err(e) = app.tg.send_media_group(msg.chat_id, target, album, "").await {
                    log_to("telegram.log", &format!("safeReplyWithMediaGroup error: {e}"));
                }
            }
        }
    } else if let Some(path) = &video {
        if let Err(e) = app.tg.upload_video(msg.chat_id, target, path, "").await {
            log_to("telegram.log", &format!("safeReplyWithVideo error: {e}"));
        }
    } else if photos.len() == 1 {
        match tokio::fs::read(&photos[0]).await {
            Ok(bytes) => {
                if let Err(e) = app.tg.send_photo_bytes(msg.chat_id, target, bytes, "").await {
                    log_to("telegram.log", &format!("safeReplyWithPhoto error: {e}"));
                }
            }
            Err(e) => log_to("telegram.log", &format!("safeReplyWithPhoto error: {e}")),
        }
    } else {
        for chunk in photos.chunks(GROUP_LIMIT) {
            let mut album: Vec<Vec<u8>> = Vec::new();
            for p in chunk {
                match tokio::fs::read(p).await {
                    Ok(bytes) => album.push(bytes),
                    Err(e) => log_to("telegram.log", &format!("safeReplyWithMediaGroup error: {e}")),
                }
            }
            if !album.is_empty() {
                if let Err(e) = app.tg.send_media_group(msg.chat_id, target, album, "").await {
                    log_to("telegram.log", &format!("safeReplyWithMediaGroup error: {e}"));
                }
            }
        }
    }

    if let Some(path) = &audio {
        let performer = if media.uploader.is_empty() { platform.to_string() } else { media.uploader.clone() };
        let audio_title = if title.is_empty() { format!("{platform} Audio") } else { title.clone() };
        if let Err(e) = app
            .tg
            .upload_audio(msg.chat_id, target, path, &audio_title, &performer, None, None)
            .await
        {
            log_to("telegram.log", &format!("safeReplyWithAudio error: {e}"));
        }
    }

    if let Some(mid) = info {
        update_status(app, msg.chat_id, mid, platform, &media.uploader, &title, "Success").await;
    }
    Ok(())
}

// ================== STATUS CARD ==================

fn status_text(platform: &str, uploader: &str, title: &str, status: &str) -> String {
    format!(
        "<b>🎬 {platform} Info</b>\n<blockquote><b>👤 Author:</b>\n-> <code>{}</code>\n<b>📝 Title:</b>\n-> <code>{}</code>\n<b>📦 Status:</b>\n-> <code>{status}</code></blockquote>",
        escape_html(uploader),
        escape_html(title)
    )
}

async fn update_status(app: &App, chat: i64, message_id: i64, platform: &str, uploader: &str, title: &str, status: &str) {
    let _ = app
        .tg
        .edit_html(chat, message_id, &status_text(platform, uploader, title, status), None)
        .await;
}

// ================== API ==================

async fn fetch_api(app: &App, target: &str, douyin: bool) -> anyhow::Result<Value> {
    let req = if douyin {
        app.http
            .get("https://douyin.wtf/api/hybrid/video_data")
            .query(&[("url", target), ("minimal", "true")])
    } else {
        app.http
            .get("https://www.tikwm.com/api/")
            .query(&[("url", target), ("hd", "1")])
    };
    let res = req.timeout(std::time::Duration::from_secs(120)).send().await?;
    let body: Value = res.json().await?;

    if douyin {
        if body["code"].as_i64() != Some(200) || body["data"].is_null() {
            anyhow::bail!(
                "{}",
                body["message"].as_str().unwrap_or("Failed to fetch media from Douyin API")
            );
        }
    } else if body["code"].as_i64() != Some(0) || body["data"].is_null() {
        anyhow::bail!(
            "{}",
            body["msg"].as_str().unwrap_or("Failed to fetch media from TikTok API")
        );
    }
    Ok(body)
}

struct Media {
    title: String,
    uploader: String,
    video: Option<String>,
    images: Vec<String>,
    live: Vec<String>,
    music: Option<String>,
}

/// First truthy string of `keys` inside `v` (JS `a || b || c`).
fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v[*k].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string()))
}

fn parse_media(api: &Value, douyin: bool) -> Media {
    let d = &api["data"];
    let uploader = first_str(&d["author"], &["nickname", "unique_id"]).unwrap_or_else(|| "Unknown".into());

    if douyin {
        let images = ["no_watermark_image_list", "watermark_image_list"]
            .iter()
            .find_map(|k| d["image_data"][*k].as_array().filter(|a| !a.is_empty()))
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
            .or_else(|| {
                d["images"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|img| {
                            let list = img["url_list"].as_array()?;
                            list.last().or_else(|| list.first()).and_then(|v| v.as_str()).map(String::from)
                        })
                        .collect()
                })
            })
            .unwrap_or_default();

        Media {
            title: first_str(d, &["desc"]).unwrap_or_else(|| "Douyin Post".into()),
            uploader,
            video: first_str(&d["video_data"], &["nwm_video_url", "nwm_video_url_HQ", "wm_video_url", "wm_video_url_HQ"]),
            images,
            live: Vec::new(),
            music: d["music"]["play_url"]["url_list"][0].as_str().map(String::from),
        }
    } else {
        Media {
            title: first_str(d, &["title"]).unwrap_or_else(|| "TikTok Post".into()),
            uploader,
            video: first_str(d, &["hdplay", "play"]),
            images: d["images"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_default(),
            live: d["live_images"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default(),
            music: first_str(&d["music_info"], &["play"]).or_else(|| first_str(d, &["music"])),
        }
    }
}

// ================== FETCH / FILES ==================

fn temp_dir() -> PathBuf {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    base.join(format!("tt_{}_{}", crate::util::now_ms(), random_suffix()))
}

fn random_suffix() -> String {
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    rand::thread_rng().sample_iter(&Alphanumeric).take(5).map(char::from).collect()
}

async fn save(app: &App, dir: &Path, name: &str, url: &str) -> anyhow::Result<PathBuf> {
    let bytes = app
        .http
        .get(url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await?
        .bytes()
        .await?;
    if bytes.len() as u64 > MAX_BYTES {
        anyhow::bail!("{name} is {}Mb, over the 49MB Telegram limit", bytes.len() / 1024 / 1024);
    }
    let path = dir.join(name);
    tokio::fs::write(&path, &bytes).await?;
    Ok(path)
}

// ================== URL PARSING ==================

fn find_url(text: &str) -> Option<String> {
    let specific = r"(?i)https?://(?:www\.|vm\.|vt\.|v\.)?(?:tiktok|douyin|iesdouyin)\.com/\S+";
    let generic = r"(?i)https?://\S+";
    for pattern in [specific, generic] {
        if let Some(m) = regex::Regex::new(pattern).ok().and_then(|re| re.find(text)) {
            return Some(m.as_str().to_string());
        }
    }
    None
}

fn is_douyin(url: &str) -> bool {
    regex::Regex::new(r"(?i)(?:douyin|iesdouyin)\.com")
        .map(|re| re.is_match(url))
        .unwrap_or(false)
}

/// `/tt@bot <url>` → `<url>`
fn strip_tt_arg(text: &str) -> String {
    let t = text.trim();
    let Some(head) = t.get(..3) else {
        return t.to_string();
    };
    if !head.eq_ignore_ascii_case("/tt") {
        return t.to_string();
    }
    let rest = &t[3..];
    match rest.strip_prefix('@') {
        Some(r) => r.splitn(2, char::is_whitespace).nth(1).unwrap_or_default().trim().to_string(),
        None => rest.trim().to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}...", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_args() {
        assert_eq!(strip_tt_arg("/tt https://vt.tiktok.com/ABC/"), "https://vt.tiktok.com/ABC/");
        assert_eq!(strip_tt_arg("/tt@bot   https://x.y/z"), "https://x.y/z");
        assert_eq!(
            find_url("look https://vm.tiktok.com/ZM123/ ok").as_deref(),
            Some("https://vm.tiktok.com/ZM123/")
        );
        assert!(is_douyin("https://www.iesdouyin.com/share/video/1"));
        assert!(!is_douyin("https://vm.tiktok.com/ZM123/"));
        assert_eq!(truncate("abcdef", 3), "abc...");
    }
}

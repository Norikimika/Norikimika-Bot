//! discord.js port — every non-voice feature.
//!
//! ponytail: voice audio (songbird/opus/DAVE) is out of scope for this crate. Every place the
//! JS started a stream now answers with a playback-unavailable notice instead. Hook point when
//! voice is added: `songbird::Serenity::new()` in `ready`, `manager.join()` where the `// ponytail:
//! voice` markers sit, and feed `Queue::songs[0]` into a real player.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};
use serenity::async_trait;
use serenity::builder::{
    CreateAttachment, CreateChannel, CreateEmbed, CreateEmbedFooter, CreateMessage, EditChannel,
};
use serenity::gateway::ActivityData;
use serenity::model::channel::{ChannelType, Message, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::gateway::{GatewayIntents, Ready};
use serenity::model::guild::Member;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::permissions::Permissions;
use serenity::model::user::User;
use serenity::model::voice::VoiceState;
use serenity::model::Colour;
use serenity::prelude::*;

use crate::app::App;
use crate::util::{self, run_timeout};

use songbird::events::{Event as VoiceEvent, TrackEvent};
use songbird::input::YoutubeDl;
use songbird::{Call, SerenityInit};

const CACHE_TTL: Duration = Duration::from_secs(3600);
const SEARCH_TIMEOUT: u64 = 15;
const PLAYLIST_TIMEOUT: u64 = 30;
const AUTOPLAY_TIMEOUT: u64 = 25;
const INFO_TIMEOUT: u64 = 20;
const SPOTIFY_MAX_TRACKS: usize = 50;
const CHATBOT_MAX_LENGTH: usize = 2000;
const CHATBOT_MAX_STORED_TURNS: usize = 200;
const CHATBOT_RECENT_TURNS: usize = 6;
const SEARCH_TOOL_MAX_TURNS: usize = 3;
const CHAT_SUMMARY_PREFIX: &str = "Earlier in this conversation (summarised):\n";
const STATS_CATEGORY_NAME: &str = "╰┈➤ Stats";
const STATS_DEFS: [(&str, &str); 3] =
    [("all", "🧩┃all members"), ("humans", "🪢┃members"), ("bots", "🤖┃bots")];

fn log(msg: &str) {
    util::log_to("discord.log", msg);
}

fn env_s(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

/// 800x400 welcome/goodbye card geometry, same as the canvas version.
const CARD_W: u32 = 800;
const CARD_H: u32 = 400;

#[derive(Clone, Debug, Default)]
struct Song {
    title: String,
    artist: String,
    url: String,
    id: Option<String>,
    /// Kept for parity with the JS `durationInSec`; only the voice player would read it.
    #[allow(dead_code)]
    duration: f64,
    requested_by: Option<(u64, String)>,
    is_manual: bool,
}

struct Queue {
    guild_name: String,
    text_channel: ChannelId,
    /// Voice channel the queue was created from. ponytail: voice — the bot never joins, so this
    /// is only used for the "alone" check once songbird lands.
    voice_channel: Option<ChannelId>,
    songs: Vec<Song>,
    last_song: Option<Song>,
    current_song: Option<Song>,
    autoplay: bool,
    fail_count: u32,
    playing: bool,
    played_ids: HashSet<String>,
    played_keys: HashSet<String>,
    played_urls: HashSet<String>,
}

impl Queue {
    fn new(guild_name: String, text_channel: ChannelId, voice_channel: Option<ChannelId>) -> Self {
        Self {
            guild_name,
            text_channel,
            voice_channel,
            songs: Vec::new(),
            last_song: None,
            current_song: None,
            autoplay: true,
            fail_count: 0,
            playing: false,
            played_ids: HashSet::new(),
            played_keys: HashSet::new(),
            played_urls: HashSet::new(),
        }
    }
}

#[derive(Default)]
struct State {
    queues: Mutex<HashMap<u64, Queue>>,
    video_cache: Mutex<HashMap<String, (Song, Instant)>>,
    radio_cache: Mutex<HashMap<String, (Vec<Song>, Instant)>>,
    history: Mutex<HashMap<u64, Vec<Value>>>,
    temp_voice: Mutex<HashSet<u64>>,
    alone: Mutex<HashMap<u64, tokio::task::JoinHandle<()>>>,
    /// Guilds whose voice call already carries the track-end handler.
    voice_hooked: Mutex<HashSet<u64>>,
    stats_running: AtomicBool,
}

struct Handler {
    app: Arc<App>,
    st: Arc<State>,
}

pub fn spawn(app: Arc<App>) {
    let token = match app.cfg.discord_token.clone() {
        Some(t) => t,
        None => {
            log("DISCORD_BOT_TOKEN is missing in .env file!");
            return;
        }
    };

    let st = Arc::new(State::default());
    load_chat_history(&st);

    tokio::spawn(async move {
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MEMBERS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::GUILD_VOICE_STATES;

        let handler = Handler { app, st };
        let mut client = match Client::builder(&token, intents)
            .event_handler(handler)
            .register_songbird()
            .await
        {
            Ok(c) => c,
            Err(e) => {
                log(&format!("Discord Login Error: {e}"));
                return;
            }
        };
        if let Err(e) = client.start().await {
            log(&format!("Discord client stopped: {e}"));
        }
    });
}

// ================== METADATA & URL NORMALIZERS ==================

fn video_id_patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"[?&]v=([a-zA-Z0-9_-]{11})",
            r"youtu\.be/([a-zA-Z0-9_-]{11})",
            r"youtube\.com/embed/([a-zA-Z0-9_-]{11})",
            r"youtube\.com/v/([a-zA-Z0-9_-]{11})",
            r"^([a-zA-Z0-9_-]{11})$",
        ]
        .iter()
        .map(|p| rx(p))
        .collect()
    })
}

fn extract_video_id(url: &str) -> Option<String> {
    let value = url.trim();
    if value.is_empty() {
        return None;
    }
    video_id_patterns()
        .iter()
        .find_map(|p| p.captures(value).map(|c| c[1].to_string()))
}

fn normalize_url(url: &str) -> String {
    if url.is_empty() {
        return url.to_string();
    }
    match extract_video_id(url) {
        Some(id) => format!("https://www.youtube.com/watch?v={id}"),
        None => url
            .replace("music.youtube.com", "www.youtube.com")
            .replace("m.youtube.com", "www.youtube.com"),
    }
}

fn clean_artist(value: &str) -> String {
    if value.trim().is_empty() {
        return "Unknown Artist".into();
    }
    let out = rx(r"(?i)\s*[-–—:]\s*(?:topic|official|vevo).*$")
        .replace_all(value, "")
        .to_string();
    let out = rx(r"\s+").replace_all(&out, " ").trim().to_string();
    if out.is_empty() {
        "Unknown Artist".into()
    } else {
        out
    }
}

fn clean_title(title: &str, artist: &str) -> String {
    let mut value = rx(r"\s+").replace_all(title, " ").trim().to_string();
    if !artist.is_empty() && artist != "Unknown Artist" {
        let esc = regex::escape(artist);
        value = rx(&format!(r"(?i)^{esc}\s*[-–—:]\s*")).replace(&value, "").to_string();
        value = rx(&format!(r"(?i)\s*[-–—:]\s*{esc}\b.*$"))
            .replace(&value, "")
            .to_string();
    }
    value = rx(r"(?i)\s*[\(\[][^\]\)]*(?:official|lyric|lirik|audio|video|mv|visualizer|remix|cover|lagu)[^\]\)]*[\)\]]")
        .replace_all(&value, "")
        .to_string();
    value = rx(r"(?i)\s*[-–—:|]\s*(?:official|lyric|lirik|audio|video|mv|visualizer|remix|cover|lagu).*$")
        .replace_all(&value, "")
        .to_string();
    value = rx(r"(?i)\s+lyrics?$").replace_all(&value, "").to_string();
    let value = rx(r"\s+").replace_all(&value, " ").trim().to_string();
    if value.is_empty() {
        title.to_string()
    } else {
        value
    }
}

fn normalize_song(mut song: Song) -> Song {
    let artist = clean_artist(&song.artist);
    song.title = clean_title(&song.title, &artist);
    song.artist = artist;
    song.url = normalize_url(&song.url);
    if song.id.is_none() {
        song.id = extract_video_id(&song.url);
    }
    song
}

fn normalize_song_key(title: &str, artist: &str) -> String {
    let t = rx(r"[\(\[][^\]\)]*[\)\]]").replace_all(title, "").to_string();
    let t: String = t
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let a = rx("(?i)topic|official|channel|vevo").replace_all(artist, "").to_string();
    let a: String = a
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("{a}:{t}")
}

fn song_key(song: &Song) -> String {
    normalize_song_key(&song.title, &song.artist)
}

fn song_url_key(song: &Song) -> String {
    match &song.id {
        Some(id) => format!("https://www.youtube.com/watch?v={id}"),
        None => normalize_url(&song.url),
    }
}

fn is_valid_song(song: &Song) -> bool {
    !song.title.is_empty() && !song.url.is_empty() && song.id.is_some()
}

fn song_id(song: &Song) -> Option<String> {
    song.id.clone().or_else(|| extract_video_id(&song.url))
}

// ================== YT-DLP ==================

async fn ytdlp(app: &App, args: Vec<String>, secs: u64) -> anyhow::Result<util::CmdOut> {
    let full = app.cfg.ytdlp_args(args);
    let slice: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
    run_timeout("yt-dlp", &slice, secs).await
}

fn build_track_from_item(item: &Value) -> Option<Song> {
    let id = item["id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| extract_video_id(item["webpage_url"].as_str().unwrap_or("")))
        .or_else(|| extract_video_id(item["url"].as_str().unwrap_or("")))?;
    let title = item["title"].as_str()?;
    if title.is_empty() || title == "[Private video]" || title == "[Deleted video]" {
        return None;
    }
    let artist = ["artist", "uploader", "channel", "creator"]
        .iter()
        .find_map(|k| item[*k].as_str())
        .unwrap_or("Unknown Artist");
    Some(normalize_song(Song {
        title: title.to_string(),
        artist: artist.to_string(),
        url: format!("https://www.youtube.com/watch?v={id}"),
        id: Some(id),
        duration: item["duration"].as_f64().unwrap_or(0.0),
        ..Default::default()
    }))
}

fn parse_entries(stdout: &str) -> Vec<Song> {
    let data: Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    data["entries"]
        .as_array()
        .map(|list| list.iter().filter_map(build_track_from_item).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(is_valid_song)
        .collect()
}

async fn search_yt(app: &App, query: &str, limit: usize) -> Vec<Song> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let args = vec![
        "--flat-playlist".into(),
        "--no-warnings".into(),
        "--no-cache-dir".into(),
        "--skip-download".into(),
        "--playlist-end".into(),
        limit.to_string(),
        "-J".into(),
        format!("ytsearch{limit}:{query}"),
    ];
    match ytdlp(app, args, SEARCH_TIMEOUT).await {
        Ok(o) if o.ok() => parse_entries(&o.stdout),
        Ok(o) => {
            log(&format!("[yt-dlp Search Error] {query}: {}", o.stderr.trim()));
            Vec::new()
        }
        Err(e) => {
            log(&format!("[yt-dlp Search Error] {query}: {e}"));
            Vec::new()
        }
    }
}

async fn playlist_tracks(app: &App, url: &str) -> Vec<Song> {
    if url.is_empty() {
        return Vec::new();
    }
    let args = vec![
        "--flat-playlist".into(),
        "--no-warnings".into(),
        "--no-cache-dir".into(),
        "--skip-download".into(),
        "-J".into(),
        url.to_string(),
    ];
    match ytdlp(app, args, PLAYLIST_TIMEOUT).await {
        Ok(o) if o.ok() => parse_entries(&o.stdout),
        Ok(o) => {
            log(&format!("[yt-dlp Playlist Error] {url}: {}", o.stderr.trim()));
            Vec::new()
        }
        Err(e) => {
            log(&format!("[yt-dlp Playlist Error] {url}: {e}"));
            Vec::new()
        }
    }
}

async fn cached_video_info(st: &State, id: &str) -> Option<Song> {
    let cache = st.video_cache.lock().await;
    match cache.get(id) {
        Some((song, at)) if at.elapsed() < CACHE_TTL => Some(song.clone()),
        _ => None,
    }
}

async fn fetch_video_info(app: &App, st: &State, id: &str) -> Option<Song> {
    if id.is_empty() {
        return None;
    }
    if let Some(hit) = cached_video_info(st, id).await {
        return Some(hit);
    }
    let url = format!("https://www.youtube.com/watch?v={id}");
    let args = vec![
        "--no-warnings".into(),
        "--no-cache-dir".into(),
        "--skip-download".into(),
        "-j".into(),
        url.clone(),
    ];
    let out = match ytdlp(app, args, INFO_TIMEOUT).await {
        Ok(o) if o.ok() => o,
        Ok(o) => {
            log(&format!("[Video Info Error] {id}: {}", o.stderr.trim()));
            return None;
        }
        Err(e) => {
            log(&format!("[Video Info Error] {id}: {e}"));
            return None;
        }
    };
    let json: Value = serde_json::from_str(&out.stdout).ok()?;
    let artist = ["artist", "uploader", "channel", "creator"]
        .iter()
        .find_map(|k| json[*k].as_str())
        .unwrap_or("Unknown Artist");
    let song = normalize_song(Song {
        title: json["title"].as_str().unwrap_or_default().to_string(),
        artist: artist.to_string(),
        url,
        id: Some(id.to_string()),
        duration: json["duration"].as_f64().unwrap_or(0.0),
        ..Default::default()
    });
    st.video_cache
        .lock()
        .await
        .insert(id.to_string(), (song.clone(), Instant::now()));
    Some(song)
}

async fn radio_tracks(app: &App, st: &State, video_id: &str) -> Vec<Song> {
    if video_id.is_empty() {
        return Vec::new();
    }
    {
        let cache = st.radio_cache.lock().await;
        if let Some((tracks, at)) = cache.get(video_id) {
            if at.elapsed() < CACHE_TTL {
                return tracks.clone();
            }
        }
    }
    let url = format!("https://music.youtube.com/watch?v={video_id}&list=RD{video_id}");
    let args = vec![
        "--flat-playlist".into(),
        "--no-warnings".into(),
        "--no-cache-dir".into(),
        "--skip-download".into(),
        "--playlist-end".into(),
        "15".into(),
        "-J".into(),
        url,
    ];
    let tracks = match ytdlp(app, args, AUTOPLAY_TIMEOUT).await {
        Ok(o) if o.ok() => parse_entries(&o.stdout),
        Ok(o) => {
            log(&format!("[Autoplay Radio Error] {}", o.stderr.trim()));
            Vec::new()
        }
        Err(e) => {
            log(&format!("[Autoplay Radio Error] {e}"));
            Vec::new()
        }
    };
    if !tracks.is_empty() {
        st.radio_cache
            .lock()
            .await
            .insert(video_id.to_string(), (tracks.clone(), Instant::now()));
    }
    tracks
}

// ================== EXTERNAL RESOLVERS ==================

async fn https_get(http: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let res = http
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/124")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("HTTP {} from {}", res.status(), url.split('?').next().unwrap_or(url));
    }
    Ok(res.text().await?)
}

async fn spotify_tracks(http: &reqwest::Client, kind: &str, id: &str) -> anyhow::Result<Vec<(String, String)>> {
    let html = https_get(http, &format!("https://open.spotify.com/embed/{kind}/{id}")).await?;
    let cap = rx(r#"(?s)<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#)
        .captures(&html)
        .ok_or_else(|| anyhow::anyhow!("Spotify embed data not found"))?;
    let data: Value = serde_json::from_str(&cap[1])?;
    let entity = &data["props"]["pageProps"]["state"]["data"]["entity"];
    if entity.is_null() {
        anyhow::bail!("Spotify entity missing");
    }
    if kind == "track" {
        let artist = entity["artists"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return Ok(vec![(entity["name"].as_str().unwrap_or_default().to_string(), artist)]);
    }
    Ok(entity["trackList"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(SPOTIFY_MAX_TRACKS)
        .filter_map(|t| {
            let title = t["title"].as_str()?.to_string();
            let artist = t["subtitle"]
                .as_str()
                .unwrap_or_default()
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            (!title.is_empty()).then_some((title, artist))
        })
        .collect())
}

async fn apple_tracks(http: &reqwest::Client, kind: &str, id: &str) -> anyhow::Result<Vec<(String, String)>> {
    let url = if kind == "song" {
        format!("https://itunes.apple.com/lookup?id={id}")
    } else {
        format!("https://itunes.apple.com/lookup?id={id}&entity=song")
    };
    let data: Value = serde_json::from_str(&https_get(http, &url).await?)?;
    let results = data["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        anyhow::bail!("Apple Music: not found");
    }
    let tracks: Vec<(String, String)> = results
        .iter()
        .filter(|r| r["wrapperType"] == "track")
        .filter_map(|t| {
            Some((
                t["trackName"].as_str()?.to_string(),
                t["artistName"].as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect();
    if kind == "song" {
        return tracks.into_iter().next().map(|t| vec![t]).ok_or_else(|| anyhow::anyhow!("Apple Music: no track data"));
    }
    if tracks.is_empty() {
        anyhow::bail!("Apple Music: album has no tracks");
    }
    Ok(tracks.into_iter().take(SPOTIFY_MAX_TRACKS).collect())
}

async fn soundcloud_track(http: &reqwest::Client, sc_url: &str) -> anyhow::Result<Vec<(String, String)>> {
    let enc: String = url_encode(sc_url);
    let body = https_get(http, &format!("https://soundcloud.com/oembed?format=json&url={enc}")).await?;
    let json: Value = serde_json::from_str(&body)?;
    let title = json["title"].as_str().unwrap_or_default();
    let idx = title.rfind(" by ").ok_or_else(|| anyhow::anyhow!("SoundCloud: unexpected oEmbed title"))?;
    Ok(vec![(title[..idx].to_string(), title[idx + 4..].trim().to_string())])
}

/// Minimal percent-encoding for the oEmbed query string.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn extract_spotify(query: &str) -> Option<(String, String)> {
    let c = rx(r"(?i)open\.spotify\.com/(?:intl-[a-z]{2}/)?(track|album|playlist)/([A-Za-z0-9]+)")
        .captures(query)?;
    Some((c[1].to_lowercase(), c[2].to_string()))
}

fn extract_apple(query: &str) -> Option<(String, String)> {
    let c = rx(r"(?i)music\.apple\.com/[a-z]{2}/(song|album)/(?:[^/?]+/)?(\d{6,})").captures(query)?;
    Some((c[1].to_lowercase(), c[2].to_string()))
}

fn extract_soundcloud(query: &str) -> Option<String> {
    let c = rx(r"(?i)https?://(?:www\.|m\.)?soundcloud\.com/(?!pages\b|discover\b)([a-zA-Z0-9_-]+)/(?!sets?\b)([a-zA-Z0-9_-]+)")
        .captures(query)?;
    Some(c[0].to_string())
}

/// 3 concurrent yt-dlp searches, 300ms apart, order preserved.
async fn resolve_meta_to_songs(
    app: Arc<App>,
    tracks: Vec<(String, String)>,
    requester: Option<(u64, String)>,
    label: &str,
) -> anyhow::Result<Vec<Song>> {
    if tracks.is_empty() {
        anyhow::bail!("No playable {label} tracks found");
    }
    log(&format!("[{label}] Resolving {} track(s)", tracks.len()));

    let mut found: Vec<Option<Song>> = vec![None; tracks.len()];
    for (base, chunk) in tracks.chunks(3).enumerate() {
        let mut set = tokio::task::JoinSet::new();
        for (i, (title, artist)) in chunk.iter().enumerate() {
            let app = app.clone();
            let query = format!("{artist} - {title}");
            set.spawn(async move { (base * 3 + i, search_yt(&app, &query, 1).await.into_iter().next()) });
        }
        while let Some(res) = set.join_next().await {
            if let Ok((i, Some(hit))) = res {
                found[i] = Some(hit);
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let songs: Vec<Song> = found
        .into_iter()
        .flatten()
        .map(|hit| {
            normalize_song(Song {
                requested_by: requester.clone(),
                is_manual: true,
                ..hit
            })
        })
        .filter(is_valid_song)
        .collect();

    log(&format!("[{label}] Matched {}/{} track(s) on YouTube", songs.len(), tracks.len()));
    Ok(songs)
}

// ================== CHATBOT MEMORY ==================

fn memory_file() -> std::path::PathBuf {
    util::data_dir().join("memory.json")
}

fn estimate_tokens(text: &str) -> usize {
    let wide = text
        .chars()
        .filter(|c| matches!(*c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6))
        .count();
    let rest = text.chars().count() - wide;
    wide + rest.div_ceil(4)
}

fn msg_text(m: &Value) -> &str {
    m["content"].as_str().unwrap_or("")
}

/// Drop oldest messages until the tail fits `budget`, never starting on an assistant turn.
fn trim_to_token_budget(list: &[Value], budget: usize) -> Vec<Value> {
    if list.is_empty() {
        return Vec::new();
    }
    let mut total = 0;
    let mut start = 0;
    for i in (0..list.len()).rev() {
        total += estimate_tokens(msg_text(&list[i]));
        if total > budget && i < list.len() - 1 {
            start = i + 1;
            break;
        }
    }
    if list.get(start).map(|m| m["role"] == "assistant").unwrap_or(false) {
        start += 1;
    }
    list[start.min(list.len())..].to_vec()
}

fn build_compression_prompt(previous_summary: &str, messages: &[Value]) -> String {
    let transcript = messages
        .iter()
        .map(|m| {
            format!(
                "{}: {}",
                if m["role"] == "assistant" { "Assistant" } else { "User" },
                msg_text(m)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prev = if previous_summary.is_empty() {
        "No earlier summary yet.".to_string()
    } else {
        format!(
            "Existing summary of earlier turns:\n{}",
            previous_summary.replace(CHAT_SUMMARY_PREFIX, "")
        )
    };
    [
        prev,
        String::new(),
        "New turns to fold in:".into(),
        transcript,
        String::new(),
        "Write one short summary of everything above that would let you answer a".into(),
        "follow-up question without seeing the raw turns. Keep names, numbers,".into(),
        "decisions and any task the user asked for. Drop pleasantries. No preamble,".into(),
        "just the summary.".into(),
    ]
    .join("\n")
}

async fn compress_to_token_budget(
    app: &App,
    endpoint: &str,
    key: &str,
    messages: &[Value],
    budget: usize,
) -> Vec<Value> {
    if messages.is_empty() {
        return Vec::new();
    }
    let fits = trim_to_token_budget(messages, budget);
    if fits.len() == messages.len() {
        return messages.to_vec();
    }

    let mut recent = messages[messages.len().saturating_sub(CHATBOT_RECENT_TURNS)..].to_vec();
    if recent.first().map(|m| m["role"] == "assistant").unwrap_or(false) {
        recent.remove(0);
    }
    let older = &messages[..messages.len() - recent.len()];
    if older.is_empty() {
        return trim_to_token_budget(&recent, budget);
    }

    let head_idx = older.iter().position(|m| msg_text(m).starts_with(CHAT_SUMMARY_PREFIX));
    let previous = head_idx.map(|i| msg_text(&older[i]).to_string()).unwrap_or_default();
    let to_fold: Vec<Value> = older
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != head_idx)
        .map(|(_, m)| m.clone())
        .collect();

    let prompt = build_compression_prompt(&previous, &to_fold);
    let summary = match request_chat_completion(app, endpoint, key, &prompt).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log(&format!("[Chatbot] Compression request error: {e}"));
            String::new()
        }
    };
    if summary.is_empty() {
        return trim_to_token_budget(&recent, budget);
    }

    let mut merged = vec![json!({"role": "system", "content": format!("{CHAT_SUMMARY_PREFIX}{summary}")})];
    merged.extend(recent.iter().cloned());

    let recent_tokens: usize = trim_to_token_budget(&recent, budget)
        .iter()
        .map(|m| estimate_tokens(msg_text(m)))
        .sum();
    if budget.saturating_sub(recent_tokens) < estimate_tokens(msg_text(&merged[0])) {
        return trim_to_token_budget(&recent, budget);
    }
    merged
}

fn load_chat_history(st: &State) {
    let path = memory_file();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let mut history = HashMap::new();
    for (user_id, turns) in map {
        let Ok(uid) = user_id.parse::<u64>() else { continue };
        let Some(list) = turns.as_array() else { continue };
        let mut kept: Vec<Value> = list
            .iter()
            .filter(|m| m["role"] == "user" || m["role"] == "assistant")
            .map(|m| json!({"role": m["role"], "content": m["content"].as_str().unwrap_or("")}))
            .collect();
        if kept.len() > CHATBOT_MAX_STORED_TURNS {
            kept.drain(..kept.len() - CHATBOT_MAX_STORED_TURNS);
        }
        history.insert(uid, kept);
    }
    let count = history.len();
    if let Ok(mut guard) = st.history.try_lock() {
        *guard = history;
    }
    log(&format!("[Chatbot] Restored chat history for {count} user(s)"));
}

fn save_chat_history(history: &HashMap<u64, Vec<Value>>) {
    let path = memory_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let map: serde_json::Map<String, Value> = history
        .iter()
        .map(|(k, v)| (k.to_string(), Value::Array(v.clone())))
        .collect();
    let tmp = path.with_extension(format!("{}tmp", util::now_ms()));
    let body = serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_default();
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ================== CHATBOT HTTP ==================

fn build_chatbot_endpoint(raw: &str) -> String {
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn extract_chatbot_text(data: &Value) -> String {
    let content = &data["choices"][0]["message"]["content"];
    if let Some(parts) = content.as_array() {
        return parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();
    }
    content.as_str().unwrap_or_default().trim().to_string()
}

async fn post_json(
    app: &App,
    endpoint: &str,
    key: &str,
    body: &Value,
    timeout: u64,
) -> anyhow::Result<(u16, Value)> {
    let res = app
        .http
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {key}"))
        .timeout(Duration::from_secs(timeout))
        .json(body)
        .send()
        .await?;
    let status = res.status().as_u16();
    let data = res.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, data))
}

/// Single-shot completion used by history compression.
async fn request_chat_completion(app: &App, endpoint: &str, key: &str, content: &str) -> anyhow::Result<String> {
    let body = json!({
        "model": app.cfg.nine_router_chatbot_model,
        "messages": [{"role": "user", "content": content}],
        "stream": false,
        "reasoning_effort": "high",
    });
    let (status, data) = post_json(app, endpoint, key, &body, 300).await?;
    if status != 200 {
        anyhow::bail!("Compression request failed: HTTP {status}");
    }
    Ok(extract_chatbot_text(&data))
}

fn search_tools() -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Search the web for current information, news, prices, or recent events. Use when the answer needs up-to-date data you may not have.",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string", "description": "The search query"}},
                "required": ["query"]
            }
        }
    }])
}

async fn web_search(app: &App, endpoint: &str, key: &str, query: &str) -> Option<Value> {
    let base = endpoint.replace("/chat/completions", "");
    let body = json!({
        "model": app.cfg.nine_router_search_model,
        "query": query,
        "search_type": "web",
        "max_results": 5,
    });
    match post_json(app, &format!("{base}/search"), key, &body, 60).await {
        Ok((status, data)) if status == 200 => Some(data),
        Ok((status, _)) => {
            log(&format!("[Chatbot] Web search failed: {status}"));
            None
        }
        Err(e) => {
            log(&format!("[Chatbot] Web search error: {e}"));
            None
        }
    }
}

fn format_search_results(data: &Value) -> Option<String> {
    let results = data["results"].as_array()?;
    if results.is_empty() {
        return None;
    }
    Some(
        results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let title = r["title"].as_str().unwrap_or("").trim();
                let url = r["url"].as_str().unwrap_or("").trim();
                let snippet = r["snippet"].as_str().unwrap_or("").trim();
                let snippet: String = snippet.chars().take(400).collect();
                let title = if title.is_empty() { "Untitled" } else { title };
                format!("### {}. [{title}]({url})\n{snippet}", i + 1).trim().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

async fn record_chat_turn(st: &State, user_id: u64, role: &str, content: &str) {
    let snapshot = {
        let mut history = st.history.lock().await;
        let list = history.entry(user_id).or_default();
        list.push(json!({"role": role, "content": content}));
        if list.len() > CHATBOT_MAX_STORED_TURNS {
            let drop = list.len() - CHATBOT_MAX_STORED_TURNS;
            list.drain(..drop);
        }
        history.clone()
    };
    save_chat_history(&snapshot);
}

async fn clear_chat_history(st: &State, user_id: u64) {
    let snapshot = {
        let mut history = st.history.lock().await;
        history.remove(&user_id);
        history.clone()
    };
    save_chat_history(&snapshot);
}

// ================== WELCOME / GOODBYE CARDS ==================

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn data_uri(bytes: &[u8], mime: &str) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

async fn fetch_image(app: &App, url: &str, label: &str) -> anyhow::Result<Vec<u8>> {
    let res = app
        .http
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (compatible; WelcomeBot/1.0)")
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    log(&format!(
        "[{label}] avatar HTTP {} {}",
        res.status(),
        res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("")
    ));
    if !res.status().is_success() {
        anyhow::bail!("{label} HTTP {}", res.status());
    }
    Ok(res.bytes().await?.to_vec())
}

/// Shrink the font until the estimated width fits the 740px text band (canvas measureText stand-in).
fn fit_size(text: &str, start: u32, min: u32) -> u32 {
    let mut size = start;
    let chars = text.chars().count() as f64;
    while chars * size as f64 * 0.62 > (CARD_W - 60) as f64 && size > min {
        size -= 2;
    }
    size
}

struct CardOpts<'a> {
    title: &'a str,
    text: &'a str,
    label: &'a str,
    title_color: &'a str,
}

/// Banner shipped inside the binary; there is no env/file override.
fn embedded_banner(label: &str) -> Option<&'static [u8]> {
    match label {
        "Welcome" => Some(include_bytes!("../banner/welcome.jpg")),
        "Goodbye" => Some(include_bytes!("../banner/goodbye.jpg")),
        _ => None,
    }
}

async fn build_card(app: &App, user: &User, opts: &CardOpts<'_>) -> anyhow::Result<Vec<u8>> {
    let avatar_url = match user.avatar_url() {
        Some(u) => format!("{u}?size=256"),
        None => user.default_avatar_url(),
    };
    let avatar = fetch_image(app, &avatar_url, opts.label).await?;

    let bg = embedded_banner(opts.label)
        .ok_or_else(|| anyhow::anyhow!("no embedded banner for {}", opts.label))?
        .to_vec();
    log(&format!("[{}] banner embedded {} bytes", opts.label, bg.len()));

    let title_size = fit_size(opts.title, 48, 20);
    let name_size = fit_size(&user.name, 40, 16);
    let text_size = fit_size(opts.text, 30, 14);

    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{CARD_W}" height="{CARD_H}">
<defs><clipPath id="av"><circle cx="400" cy="105" r="75"/></clipPath></defs>
<image href="{bg}" x="0" y="0" width="{CARD_W}" height="{CARD_H}" preserveAspectRatio="xMidYMid slice"/>
<rect x="0" y="0" width="{CARD_W}" height="{CARD_H}" fill="#000000" fill-opacity="0.55"/>
<image href="{av}" x="325" y="30" width="150" height="150" clip-path="url(#av)"/>
<circle cx="400" cy="105" r="75" fill="none" stroke="#ffffff" stroke-width="6"/>
<g font-family="Noto Serif, serif" font-weight="bold" text-anchor="middle">
<text x="400" y="245" font-size="{title_size}" fill="{title_color}">{title}</text>
<text x="400" y="295" font-size="{name_size}" fill="#ffffff">{name}</text>
<text x="400" y="345" font-size="{text_size}" fill="#facc15">{text}</text>
</g>
</svg>"##,
        bg = data_uri(&bg, "image/jpeg"),
        av = data_uri(&avatar, "image/png"),
        title = xml_escape(opts.title),
        name = xml_escape(&user.name),
        text = xml_escape(opts.text),
        title_color = opts.title_color,
    );

    render_png(&svg)
}

fn render_png(svg: &str) -> anyhow::Result<Vec<u8>> {
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_str(svg, &opt)?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(CARD_W, CARD_H)
        .ok_or_else(|| anyhow::anyhow!("pixmap alloc failed"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    Ok(pixmap.encode_png()?)
}

async fn send_card(
    ctx: &Context,
    channel_id: ChannelId,
    content: &str,
    png: Vec<u8>,
    file_name: &str,
) -> anyhow::Result<()> {
    channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .content(content)
                .add_file(CreateAttachment::bytes(png, file_name)),
        )
        .await?;
    Ok(())
}

// ================== PERMISSIONS / VOICE HELPERS ==================

async fn can_control(ctx: &Context, msg: &Message, current: Option<&Song>) -> bool {
    let Some((requester_id, _)) = current.and_then(|s| s.requested_by.clone()) else {
        return true;
    };
    if requester_id == msg.author.id.get() {
        return true;
    }
    let Some(guild_id) = msg.guild_id else { return false };
    let Ok(member) = guild_id.member(&ctx.http, msg.author.id).await else {
        return false;
    };
    let perms = match (
        ctx.cache.guild(guild_id),
        ctx.cache.guild(guild_id).and_then(|g| g.channels.get(&msg.channel_id).cloned()),
    ) {
        (Some(guild), Some(channel)) => guild.user_permissions_in(&channel, &member),
        (Some(guild), None) => guild.member_permissions(&member),
        _ => return false,
    };
    perms.administrator() || perms.manage_channels()
}

fn user_voice_channel(ctx: &Context, guild_id: GuildId, user_id: UserId) -> Option<ChannelId> {
    ctx.cache
        .guild(guild_id)
        .and_then(|g| g.voice_states.get(&user_id).and_then(|v| v.channel_id))
}

fn humans_in_channel(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> usize {
    ctx.cache
        .guild(guild_id)
        .map(|g| {
            g.voice_states
                .values()
                .filter(|v| v.channel_id == Some(channel_id))
                .filter(|v| !v.member.as_ref().map(|m| m.user.bot).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

async fn active_users(ctx: &Context, st: &State) -> usize {
    let channels: Vec<(GuildId, ChannelId)> = {
        let queues = st.queues.lock().await;
        queues
            .iter()
            .filter_map(|(guild, q)| {
                let channel = q.voice_channel?;
                Some((GuildId::new(*guild), channel))
            })
            .collect()
    };
    let mut users = HashSet::new();
    for (guild_id, channel_id) in channels {
        if let Some(guild) = ctx.cache.guild(guild_id) {
            for (user_id, state) in &guild.voice_states {
                if state.channel_id != Some(channel_id) {
                    continue;
                }
                if !state.member.as_ref().map(|m| m.user.bot).unwrap_or(false) {
                    users.insert(*user_id);
                }
            }
        }
    }
    users.len()
}

// ================== SERVER STATS CHANNELS ==================

fn stats_overwrites(guild_id: GuildId, bot_id: UserId) -> Vec<PermissionOverwrite> {
    vec![
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::CONNECT | Permissions::SEND_MESSAGES,
            kind: PermissionOverwriteType::Role(guild_id.everyone_role()),
        },
        PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL | Permissions::CONNECT | Permissions::MANAGE_CHANNELS,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(bot_id),
        },
    ]
}

async fn update_guild_stats(ctx: &Context, guild_id: GuildId) -> anyhow::Result<()> {
    // The cache can be cold or mid-update right after a restart: skip this pass
    // quietly instead of logging an error, the next tick picks the guild up.
    let cached = ctx.cache.guild(guild_id).map(|g| (g.name.clone(), g.member_count));
    let Some((guild_name, member_count)) = cached else {
        log(&format!("[Stats] {guild_id} not cached yet; skipping this pass"));
        return Ok(());
    };

    let all_count: Option<u64>;
    let mut bot_count: Option<u64> = None;
    let mut human_count: Option<u64> = None;

    let mut members: Vec<Member> = Vec::new();
    let mut after: Option<UserId> = None;
    let mut failed = false;
    loop {
        match guild_id.members(&ctx.http, Some(1000), after).await {
            Ok(page) => {
                if page.is_empty() {
                    break;
                }
                after = page.last().map(|m| m.user.id);
                members.extend(page);
                if members.len() >= 10_000 {
                    break;
                }
            }
            Err(e) => {
                log(&format!("[Stats] Full fetch failed for {guild_name}, degraded mode: {e}"));
                failed = true;
                break;
            }
        }
    }
    if failed {
        all_count = Some(member_count);
    } else {
        bot_count = Some(members.iter().filter(|m| m.user.bot).count() as u64);
        human_count = Some(members.iter().filter(|m| !m.user.bot).count() as u64);
        all_count = Some(members.len() as u64);
    }
    let Some(all_count) = all_count else { return Ok(()) };

    let values = [("all", Some(all_count)), ("humans", human_count), ("bots", bot_count)];
    let bot_id = ctx.cache.current_user().id;
    let channels = guild_id.channels(&ctx.http).await?;

    let categories: Vec<&serenity::model::channel::GuildChannel> = channels
        .values()
        .filter(|c| c.kind == ChannelType::Category && c.name.to_lowercase().contains("stats"))
        .collect();

    let category = match categories.first() {
        Some(c) => (*c).clone(),
        None => guild_id
            .create_channel(
                &ctx.http,
                CreateChannel::new(STATS_CATEGORY_NAME)
                    .kind(ChannelType::Category)
                    .permissions(stats_overwrites(guild_id, bot_id)),
            )
            .await?,
    };

    for extra in categories.iter().skip(1) {
        let _ = extra.id.delete(&ctx.http).await;
    }

    for (key, value) in values {
        let Some(value) = value else { continue };
        let Some((_, label)) = STATS_DEFS.iter().find(|(k, _)| *k == key) else { continue };
        let target = format!("{label}: {value}");
        let matches: Vec<&serenity::model::channel::GuildChannel> = channels
            .values()
            .filter(|c| {
                c.kind == ChannelType::Voice
                    && c.parent_id == Some(category.id)
                    && c.name.split(':').next().unwrap_or("").trim() == *label
            })
            .collect();
        match matches.first() {
            None => {
                guild_id
                    .create_channel(
                        &ctx.http,
                        CreateChannel::new(&target)
                            .kind(ChannelType::Voice)
                            .category(category.id)
                            .permissions(stats_overwrites(guild_id, bot_id)),
                    )
                    .await?;
            }
            Some(channel) => {
                if channel.name != target {
                    if let Err(e) = channel.id.edit(&ctx.http, EditChannel::new().name(&target)).await {
                        log(&format!("[Stats] Rename failed: {e}"));
                    }
                }
                for extra in matches.iter().skip(1) {
                    let _ = extra.id.delete(&ctx.http).await;
                }
            }
        }
    }
    Ok(())
}

async fn update_stats(st: &State, ctx: &Context) {
    if st.stats_running.swap(true, Ordering::SeqCst) {
        return;
    }
    for guild_id in ctx.cache.guilds() {
        if let Err(e) = update_guild_stats(ctx, guild_id).await {
            log(&format!("[Stats] Error in {guild_id}: {e}"));
        }
    }
    st.stats_running.store(false, Ordering::SeqCst);
}

// ================== CUSTOM VOICE CHANNELS (no audio) ==================

async fn setup_custom_voice(ctx: &Context, guild_id: GuildId) {
    let Ok(channels) = guild_id.channels(&ctx.http).await else { return };

    let trigger = channels
        .values()
        .find(|c| c.kind == ChannelType::Voice && c.name.to_lowercase().contains("┃custom"));

    match trigger {
        Some(channel) => {
            let _ = channel.id.edit(&ctx.http, EditChannel::new().position(0)).await;
        }
        None => {
            let voice_category = channels
                .values()
                .find(|c| c.kind == ChannelType::Category && c.name.to_lowercase().contains("voice"));
            let mut create = CreateChannel::new("➕┃custom").kind(ChannelType::Voice);
            if let Some(cat) = voice_category {
                create = create.category(cat.id);
            }
            match guild_id.create_channel(&ctx.http, create).await {
                Ok(created) => {
                    let _ = created.id.edit(&ctx.http, EditChannel::new().position(0)).await;
                    log(&format!("[Auto Setup] Created trigger channel at top in guild: {guild_id}"));
                }
                Err(e) => log(&format!("[Startup Auto-Setup Error] Guild {guild_id}: {e}")),
            }
        }
    }

    for channel in channels.values() {
        if channel.kind == ChannelType::Voice
            && channel.name.starts_with("🦕┃")
            && humans_in_channel(ctx, guild_id, channel.id) == 0
            && ctx
                .cache
                .guild(guild_id)
                .map(|g| !g.voice_states.values().any(|v| v.channel_id == Some(channel.id)))
                .unwrap_or(true)
        {
            let _ = channel
                .id
                .delete(&ctx.http)
                .await;
        }
    }
}

// ================== EMBED / MESSAGE HELPERS ==================

const RED: Colour = Colour(0xFF0000);
const GREEN: Colour = Colour(0x00FF00);
const BLUE: Colour = Colour(0x0099FF);
const YELLOW: Colour = Colour(0xFFFF00);
const AMBER: Colour = Colour(0xFFA500);

async fn send_embed(ctx: &Context, channel: ChannelId, embed: CreateEmbed) {
    let _ = channel
        .send_message(&ctx.http, CreateMessage::new().embed(embed))
        .await;
}

async fn send_error(ctx: &Context, channel: ChannelId, description: &str) {
    send_embed(ctx, channel, CreateEmbed::new().colour(RED).description(description)).await;
}

/// ponytail: voice — replace with songbird playback once audio is in scope.
async fn playback_unavailable(ctx: &Context, channel: ChannelId, song: Option<&Song>) {
    let description = match song {
        Some(song) => format!(
            "⚠️ Playback unavailable: voice audio is not implemented in this build. **{}** is queued but will not play.",
            song.title
        ),
        None => "⚠️ Playback unavailable: voice audio is not implemented in this build.".to_string(),
    };
    send_embed(ctx, channel, CreateEmbed::new().colour(AMBER).description(description)).await;
}

fn chatbot_context_tokens() -> usize {
    env_s("NINE_ROUTER_CHATBOT_CONTEXT")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4096)
        .max(1024)
}

impl Handler {
    // ================== CHATBOT ==================

    async fn handle_chat(&self, ctx: &Context, msg: &Message) {
        let app = &self.app;
        let (Some(url), Some(key)) = (&app.cfg.nine_router_url, &app.cfg.nine_router_key) else {
            log("[Chatbot] Missing NINE_ROUTER_URL or NINE_ROUTER_API_KEY");
            return;
        };
        let endpoint = build_chatbot_endpoint(url);
        let user_id = msg.author.id.get();
        let prompt: String = msg.content.trim().chars().take(CHATBOT_MAX_LENGTH).collect();

        // Typing expires after ~10s server-side; AI reasoning can take 60s+.
        let typing = {
            let http = ctx.http.clone();
            let channel = msg.channel_id;
            tokio::spawn(async move {
                loop {
                    let _ = channel.broadcast_typing(&http).await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            })
        };

        let reply = self.chat_reply(&endpoint, key, user_id, &prompt).await;
        typing.abort();

        match reply {
            Ok(text) if !text.is_empty() => {
                let out: String = text.chars().take(CHATBOT_MAX_LENGTH).collect();
                if let Err(e) = msg.reply(&ctx.http, out).await {
                    log(&format!("[Chatbot] reply failed: {e}"));
                }
            }
            Ok(_) => log("[Chatbot] Provider returned an empty response"),
            Err(e) => log(&format!("[Chatbot] {e}")),
        }
    }

    async fn chat_reply(&self, endpoint: &str, key: &str, user_id: u64, prompt: &str) -> anyhow::Result<String> {
        let app = &self.app;
        record_chat_turn(&self.st, user_id, "user", prompt).await;

        let history = {
            let history = self.st.history.lock().await;
            history.get(&user_id).cloned().unwrap_or_default()
        };
        let mut working = compress_to_token_budget(app, endpoint, key, &history, chatbot_context_tokens()).await;

        let mut turns = 0;
        let mut last_text = String::new();
        while turns <= SEARCH_TOOL_MAX_TURNS {
            turns += 1;
            let body = json!({
                "model": app.cfg.nine_router_chatbot_model,
                "messages": working.clone(),
                "stream": false,
                "reasoning_effort": "high",
                "tools": search_tools(),
            });
            let (status, data) = post_json(app, endpoint, key, &body, 300).await?;
            if status != 200 {
                if status == 400 {
                    clear_chat_history(&self.st, user_id).await;
                }
                log(&format!("[Chatbot] Provider HTTP {status}"));
                return Ok(String::new());
            }

            let message = data["choices"][0]["message"].clone();
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            let text = extract_chatbot_text(&data);
            if !text.is_empty() {
                last_text = text;
            }
            if calls.is_empty() {
                break;
            }

            // Model asked for search: run them, feed results back, loop again.
            working.push(message);
            for call in calls {
                if call["function"]["name"] != "web_search" {
                    continue;
                }
                let args: Value =
                    serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
                let query: String = args["query"].as_str().unwrap_or("").chars().take(500).collect();
                if query.is_empty() {
                    continue;
                }
                let formatted = match web_search(app, endpoint, key, &query).await {
                    Some(data) => format_search_results(&data).unwrap_or_else(|| "No results found.".into()),
                    None => "No results found.".into(),
                };
                working.push(json!({
                    "role": "tool",
                    "tool_call_id": call["id"],
                    "content": formatted,
                }));
                log(&format!("[Chatbot] web_search: \"{query}\" -> {} chars", formatted.len()));
            }
        }

        if !last_text.is_empty() {
            record_chat_turn(&self.st, user_id, "assistant", &last_text).await;
        }
        Ok(last_text)
    }
}

// ================== QUEUE STATE ==================

async fn destroy_queue(st: &State, guild_id: u64, delete: bool) {
    let mut queues = st.queues.lock().await;
    if let Some(queue) = queues.get_mut(&guild_id) {
        queue.playing = false;
        queue.songs.clear();
        queue.current_song = None;
        queue.fail_count = 0;
    }
    if delete {
        queues.remove(&guild_id);
    }
}


// ================== VOICE PLAYBACK ==================

/// Fires when the current track ends and moves the queue along.
struct TrackEnded {
    app: Arc<App>,
    st: Arc<State>,
    ctx: Context,
    guild_id: u64,
}

#[serenity::async_trait]
impl songbird::EventHandler for TrackEnded {
    async fn act(&self, _ev: &songbird::EventContext<'_>) -> Option<VoiceEvent> {
        advance_queue(&self.app, &self.st, &self.ctx, self.guild_id).await;
        None
    }
}

/// Join (or reuse) the guild's voice call and play `queue.songs[0]`.
async fn play_current(app: &Arc<App>, st: &Arc<State>, ctx: &Context, guild_id: u64) -> bool {
    let (song, text_channel, voice_channel) = {
        let queues = st.queues.lock().await;
        let Some(queue) = queues.get(&guild_id) else { return false };
        (queue.songs.first().cloned(), queue.text_channel, queue.voice_channel)
    };
    let (Some(song), Some(voice_channel)) = (song, voice_channel) else { return false };
    let Some(call) = voice_call(ctx, GuildId::new(guild_id), voice_channel).await else {
        return false;
    };

    {
        let mut handler = call.lock().await;
        let first_hook = st.voice_hooked.lock().await.insert(guild_id);
        if first_hook {
            handler.add_global_event(
                VoiceEvent::Track(TrackEvent::End),
                TrackEnded {
                    app: app.clone(),
                    st: st.clone(),
                    ctx: ctx.clone(),
                    guild_id,
                },
            );
        }
        handler.play_only_input(YoutubeDl::new(app.http.clone(), song.url.clone()).into());
    }

    {
        let mut queues = st.queues.lock().await;
        if let Some(queue) = queues.get_mut(&guild_id) {
            queue.playing = true;
            queue.current_song = Some(song.clone());
        }
    }

    let mut embed = CreateEmbed::new()
        .colour(GREEN)
        .description(format!("🎶 Now playing: [{}]({})", song.title, song.url));
    if let Some(id) = song_id(&song) {
        embed = embed.thumbnail(format!("https://img.youtube.com/vi/{id}/mqdefault.jpg"));
    }
    send_embed(ctx, text_channel, embed).await;
    true
}

async fn voice_call(
    ctx: &Context,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Option<Arc<tokio::sync::Mutex<Call>>> {
    let Some(manager) = songbird::get(ctx).await else {
        log("[Voice] songbird manager missing");
        return None;
    };
    match manager.join(guild_id, channel_id).await {
        Ok(call) => Some(call),
        Err(e) => {
            log(&format!("[Voice] join {guild_id} -> {channel_id} failed: {e}"));
            None
        }
    }
}

async fn leave_voice(ctx: &Context, st: &Arc<State>, guild_id: u64) {
    if let Some(manager) = songbird::get(ctx).await {
        let _ = manager.remove(GuildId::new(guild_id)).await;
    }
    st.voice_hooked.lock().await.remove(&guild_id);
}

/// Current track finished: drop it, then play the next one, autoplay, or stop.
async fn advance_queue(app: &Arc<App>, st: &Arc<State>, ctx: &Context, guild_id: u64) {
    let (next, empty, channel) = {
        let mut queues = st.queues.lock().await;
        let Some(queue) = queues.get_mut(&guild_id) else { return };
        if let Some(done) = queue.songs.first().cloned() {
            if let Some(id) = song_id(&done) {
                queue.played_ids.insert(id);
            }
            queue.played_keys.insert(song_key(&done));
            queue.played_urls.insert(song_url_key(&done));
            queue.last_song = Some(done);
        }
        if !queue.songs.is_empty() {
            queue.songs.remove(0);
        }
        (queue.songs.first().cloned(), queue.songs.is_empty(), queue.text_channel)
    };

    if next.is_some() {
        play_current(app, st, ctx, guild_id).await;
        return;
    }
    if empty {
        if let Some(song) = next_autoplay(app, st, guild_id).await {
            {
                let mut queues = st.queues.lock().await;
                if let Some(queue) = queues.get_mut(&guild_id) {
                    queue.songs.push(song);
                }
            }
            play_current(app, st, ctx, guild_id).await;
            return;
        }
        send_embed(
            ctx,
            channel,
            CreateEmbed::new().colour(BLUE).description("Queue finished."),
        )
        .await;
        leave_voice(ctx, st, guild_id).await;
    }
}

/// ponytail: voice — autoplay normally feeds the player; here it just keeps the queue stocked
/// with a radio suggestion so `!q` and `!ap` still behave.
async fn next_autoplay(app: &Arc<App>, st: &State, guild_id: u64) -> Option<Song> {
    let (base, played_ids, played_keys, played_urls, queued) = {
        let queues = st.queues.lock().await;
        let queue = queues.get(&guild_id)?;
        if !queue.autoplay {
            return None;
        }
        let base = queue.songs.first().or(queue.last_song.as_ref())?.clone();
        let queued: HashSet<String> = queue.songs.iter().filter_map(song_id).collect();
        (
            base,
            queue.played_ids.clone(),
            queue.played_keys.clone(),
            queue.played_urls.clone(),
            queued,
        )
    };

    let video_id = song_id(&base)?;
    for track in radio_tracks(app, st, &video_id).await {
        let candidate = normalize_song(Song {
            requested_by: base.requested_by.clone(),
            is_manual: false,
            ..track
        });
        if !is_valid_song(&candidate) {
            continue;
        }
        let Some(id) = song_id(&candidate) else { continue };
        if Some(id.clone()) == song_id(&base)
            || played_ids.contains(&id)
            || played_keys.contains(&song_key(&candidate))
            || played_urls.contains(&song_url_key(&candidate))
            || queued.contains(&id)
        {
            continue;
        }
        return Some(candidate);
    }
    None
}

// ================== COMMANDS ==================

impl Handler {
    async fn handle_command(
        &self,
        ctx: &Context,
        msg: &Message,
        guild_id: GuildId,
        command: &str,
        _args: &[&str],
    ) {
        let channel = msg.channel_id;
        let guild_name = ctx
            .cache
            .guild(guild_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        let voice_channel = user_voice_channel(ctx, guild_id, msg.author.id);
        let key = guild_id.get();

        match command {
            "h" => {
                let embed = CreateEmbed::new()
                    .colour(BLUE)
                    .title(format!("Available Commands - {guild_name}"))
                    .description(
                        "**Music**\n\
                         `!p` - Play a track by link or search term\n\
                         `!q` - Display the current song queue\n\
                         `!s` - Skip the current track\n\
                         `!l` - Stop playback and clear queue\n\
                         `!d` - Disconnect from voice channel\n\
                         `!ap` - Toggle automatic song recommendations",
                    )
                    .footer(CreateEmbedFooter::new(
                        "Skips and stops are restricted to the track requester or admins.",
                    ));
                send_embed(ctx, channel, embed).await;
            }

            "q" => {
                let snapshot = {
                    let mut queues = self.st.queues.lock().await;
                    queues.get_mut(&key).map(|queue| {
                        queue.text_channel = channel;
                        queue.guild_name = guild_name.clone();
                        (queue.songs.clone(), queue.autoplay)
                    })
                };
                let Some((songs, autoplay)) = snapshot else {
                    send_error(ctx, channel, "The queue is currently empty.").await;
                    return;
                };
                if songs.is_empty() {
                    send_error(ctx, channel, "The queue is currently empty.").await;
                    return;
                }

                let now_playing = &songs[0];
                let queued = &songs[1..];
                let req_info = match &now_playing.requested_by {
                    Some((id, _)) if now_playing.is_manual => format!(" | Requested by: <@{id}>"),
                    _ => " | *(Autoplay)*".to_string(),
                };
                let artist = if now_playing.artist.is_empty() || now_playing.artist == "Unknown Artist" {
                    String::new()
                } else {
                    format!("by {}", now_playing.artist)
                };
                let mut description = format!(
                    "**Now Playing:**\n[{}]({}) {}{req_info}\n\n**Up Next:**\n",
                    now_playing.title, now_playing.url, artist
                );
                if queued.is_empty() {
                    description.push_str("No upcoming songs manually queued.");
                } else {
                    let list = queued
                        .iter()
                        .take(10)
                        .enumerate()
                        .map(|(i, song)| {
                            let who = match &song.requested_by {
                                Some((id, _)) => format!("<@{id}>"),
                                None => "Autoplay".to_string(),
                            };
                            format!("{}. [{}]({}) - {who}", i + 1, song.title, song.url)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    description.push_str(&list);
                    if queued.len() > 10 {
                        description.push_str(&format!("\n\n...and {} more song(s).", queued.len() - 10));
                    }
                }

                let embed = CreateEmbed::new()
                    .colour(BLUE)
                    .title(format!("Music Queue - {guild_name}"))
                    .description(description)
                    .footer(CreateEmbedFooter::new(format!(
                        "Autoplay: {} | Total Songs in Queue: {}",
                        if autoplay { "Enabled" } else { "Disabled" },
                        songs.len()
                    )));
                send_embed(ctx, channel, embed).await;
            }

            "ap" => {
                let exists = {
                    let queues = self.st.queues.lock().await;
                    queues.contains_key(&key)
                };
                if !exists {
                    send_error(ctx, channel, "Play a song first to toggle autoplay!").await;
                    return;
                }
                if voice_channel.is_none() {
                    send_error(ctx, channel, "You need to join a voice channel first!").await;
                    return;
                }
                let current = {
                    let queues = self.st.queues.lock().await;
                    queues
                        .get(&key)
                        .and_then(|q| q.songs.first().or(q.last_song.as_ref()).cloned())
                };
                if !can_control(ctx, msg, current.as_ref()).await {
                    send_error(ctx, channel, "Only the person who played this song can toggle autoplay.").await;
                    return;
                }
                let enabled = {
                    let mut queues = self.st.queues.lock().await;
                    let queue = queues.get_mut(&key).expect("checked above");
                    queue.text_channel = channel;
                    queue.guild_name = guild_name.clone();
                    queue.autoplay = !queue.autoplay;
                    queue.autoplay
                };
                log(&format!("[Autoplay Toggle] Server: {guild_name} | {enabled}"));
                let embed = CreateEmbed::new()
                    .colour(if enabled { GREEN } else { RED })
                    .description(format!(
                        "Autoplay has been **{}**",
                        if enabled { "enabled" } else { "disabled" }
                    ));
                send_embed(ctx, channel, embed).await;
            }

            "s" => {
                let has_songs = {
                    let queues = self.st.queues.lock().await;
                    queues.get(&key).map(|q| !q.songs.is_empty()).unwrap_or(false)
                };
                if !has_songs {
                    send_error(ctx, channel, "No song is currently playing.").await;
                    return;
                }
                if voice_channel.is_none() {
                    send_error(ctx, channel, "You need to join a voice channel first!").await;
                    return;
                }
                let current = {
                    let mut queues = self.st.queues.lock().await;
                    let queue = queues.get_mut(&key).expect("checked above");
                    queue.text_channel = channel;
                    queue.guild_name = guild_name.clone();
                    queue.songs.first().cloned()
                };
                let Some(current) = current else {
                    send_error(ctx, channel, "No song is currently playing.").await;
                    return;
                };
                if !can_control(ctx, msg, Some(&current)).await {
                    let who = match &current.requested_by {
                        Some((id, _)) => format!("<@{id}>"),
                        None => "the requester".to_string(),
                    };
                    send_error(ctx, channel, &format!("Only {who} (or an Admin) can skip this track.")).await;
                    return;
                }

                let artist = if current.artist.is_empty() || current.artist == "Unknown Artist" {
                    String::new()
                } else {
                    format!(" by {}", current.artist)
                };
                let embed = CreateEmbed::new().colour(YELLOW).description(format!(
                    "**{}**{artist} was skipped by <@{}>",
                    current.title,
                    msg.author.id
                ));

                // ponytail: voice — `player.stop()` lives here.
                let (next_song, empty) = {
                    let mut queues = self.st.queues.lock().await;
                    let queue = queues.get_mut(&key).expect("checked above");
                    queue.playing = false;
                    queue.current_song = None;
                    if !queue.songs.is_empty() {
                        queue.songs.remove(0);
                    }
                    queue.last_song = Some(current.clone());
                    (queue.songs.first().cloned(), queue.songs.is_empty())
                };
                send_embed(ctx, channel, embed).await;

                if empty {
                    if let Some(song) = next_autoplay(&self.app, &self.st, key).await {
                        let mut queues = self.st.queues.lock().await;
                        if let Some(queue) = queues.get_mut(&key) {
                            queue.played_ids.insert(song_id(&song).unwrap_or_default());
                            queue.played_keys.insert(song_key(&song));
                            queue.played_urls.insert(song_url_key(&song));
                            queue.songs.push(song.clone());
                        }
                        drop(queues);
                        if !play_current(&self.app, &self.st, ctx, key).await {
                            playback_unavailable(ctx, channel, Some(&song)).await;
                        }
                    } else {
                        send_embed(
                            ctx,
                            channel,
                            CreateEmbed::new().colour(BLUE).description("Queue finished."),
                        )
                        .await;
                    }
                } else if !play_current(&self.app, &self.st, ctx, key).await {
                    playback_unavailable(ctx, channel, next_song.as_ref()).await;
                }
            }

            "l" => {
                if voice_channel.is_none() {
                    send_error(ctx, channel, "You need to join a voice channel first!").await;
                    return;
                }
                let exists = {
                    let queues = self.st.queues.lock().await;
                    queues
                        .get(&key)
                        .map(|q| q.playing || !q.songs.is_empty() || q.current_song.is_some())
                        .unwrap_or(false)
                };
                if !exists {
                    send_error(ctx, channel, "Nothing is currently playing.").await;
                    return;
                }
                let (current, requester) = {
                    let queues = self.st.queues.lock().await;
                    let queue = queues.get(&key).expect("checked above");
                    (
                        queue.songs.first().or(queue.last_song.as_ref()).cloned(),
                        queue
                            .songs
                            .first()
                            .or(queue.last_song.as_ref())
                            .and_then(|s| s.requested_by.clone()),
                    )
                };
                if !can_control(ctx, msg, current.as_ref()).await {
                    let who = match requester {
                        Some((id, _)) => format!("<@{id}>"),
                        None => "the requester".to_string(),
                    };
                    send_error(ctx, channel, &format!("Only {who} (or an Admin) can stop the player.")).await;
                    return;
                }
                log(&format!(
                    "[Stop Command] Server: {guild_name} | Stopped by {}",
                    msg.author.tag()
                ));
                destroy_queue(&self.st, key, false).await;
                send_embed(
                    ctx,
                    channel,
                    CreateEmbed::new().colour(YELLOW).description("Music stopped & queue cleared."),
                )
                .await;
            }

            "d" => {
                let exists = {
                    let queues = self.st.queues.lock().await;
                    queues.contains_key(&key)
                };
                if !exists {
                    send_error(ctx, channel, "Not connected to any voice channel.").await;
                    return;
                }
                if voice_channel.is_none() {
                    send_error(ctx, channel, "You need to join a voice channel first!").await;
                    return;
                }
                let (current, requester) = {
                    let queues = self.st.queues.lock().await;
                    let queue = queues.get(&key).expect("checked above");
                    (
                        queue.songs.first().cloned(),
                        queue.songs.first().and_then(|s| s.requested_by.clone()),
                    )
                };
                if !can_control(ctx, msg, current.as_ref()).await {
                    let who = match requester {
                        Some((id, _)) => format!("<@{id}>"),
                        None => "the requester".to_string(),
                    };
                    send_error(ctx, channel, &format!("Only {who} (or an Admin) can disconnect the player.")).await;
                    return;
                }
                log(&format!(
                    "[Disconnect Command] Server: {guild_name} | Disconnected by {}",
                    msg.author.tag()
                ));
                leave_voice(ctx, &self.st, key).await;
                destroy_queue(&self.st, key, true).await;
                send_embed(
                    ctx,
                    channel,
                    CreateEmbed::new()
                        .colour(RED)
                        .description("Music stopped and disconnected from voice channel."),
                )
                .await;
            }

            _ => {}
        }
    }
}

impl Handler {
    async fn command_play(
        &self,
        ctx: &Context,
        msg: &Message,
        guild_id: GuildId,
        guild_name: &str,
        voice_channel: Option<ChannelId>,
        args: &[&str],
    ) {
        let channel = msg.channel_id;
        let key = guild_id.get();

        // ponytail: voice — the JS also rejects users not sharing the bot's voice channel; the
        // bot never joins one here, so only the "you must be in voice" half applies.
        if voice_channel.is_none() {
            send_error(ctx, channel, "You need to join a voice channel first!").await;
            return;
        }
        let query = args.join(" ").trim().to_string();
        if query.is_empty() {
            send_error(ctx, channel, "Please provide a song title, playlist URL, or song URL!").await;
            return;
        }

        let requester = Some((msg.author.id.get(), msg.author.tag()));
        let is_playlist = rx(r"(?i)[?&]list=|/playlist").is_match(&query);
        let direct_id = extract_video_id(&query);
        let spotify = extract_spotify(&query);
        let apple = extract_apple(&query);
        let soundcloud = extract_soundcloud(&query);

        let app = self.app.clone();
        let resolved: anyhow::Result<(Vec<Song>, Option<String>)> = async {
            if let Some((kind, id)) = spotify {
                let label = format!("Spotify {kind}");
                let tracks = spotify_tracks(&app.http, &kind, &id).await?;
                let songs = resolve_meta_to_songs(app.clone(), tracks, requester.clone(), &label).await?;
                Ok((songs, Some(label)))
            } else if let Some((kind, id)) = apple {
                let label = format!("Apple Music {kind}");
                let tracks = apple_tracks(&app.http, &kind, &id).await?;
                let songs = resolve_meta_to_songs(app.clone(), tracks, requester.clone(), &label).await?;
                Ok((songs, Some(label)))
            } else if let Some(sc_url) = soundcloud {
                let tracks = soundcloud_track(&app.http, &sc_url).await?;
                let songs = resolve_meta_to_songs(app.clone(), tracks, requester.clone(), "SoundCloud").await?;
                Ok((songs, Some("SoundCloud".to_string())))
            } else if is_playlist && direct_id.is_none() {
                let songs = playlist_tracks(&app, &query)
                    .await
                    .into_iter()
                    .map(|t| normalize_song(Song {
                        requested_by: requester.clone(),
                        is_manual: true,
                        ..t
                    }))
                    .collect();
                Ok((songs, None))
            } else if let Some(id) = direct_id.clone() {
                let url = format!("https://www.youtube.com/watch?v={id}");
                let songs = fetch_video_info(&app, &self.st, &id)
                    .await
                    .map(|t| Song {
                        url,
                        requested_by: requester.clone(),
                        is_manual: true,
                        ..t
                    })
                    .into_iter()
                    .collect();
                Ok((songs, None))
            } else {
                let songs = search_yt(&app, &query, 10)
                    .await
                    .into_iter()
                    .take(1)
                    .map(|hit| normalize_song(Song {
                        requested_by: requester.clone(),
                        is_manual: true,
                        ..hit
                    }))
                    .collect();
                Ok((songs, None))
            }
        }
        .await;

        let (songs, source_label) = match resolved {
            Ok(v) => v,
            Err(e) => {
                log(&format!("Music Play Error: {e}"));
                send_error(ctx, channel, "An error occurred while searching or loading the track.").await;
                return;
            }
        };
        let songs: Vec<Song> = songs
            .into_iter()
            .map(normalize_song)
            .filter(is_valid_song)
            .collect();
        if songs.is_empty() {
            send_error(ctx, channel, "No valid tracks were found.").await;
            return;
        }

        let playlist_reply = is_playlist && direct_id.is_none();
        let (new_queue, was_playing, added) = {
            let mut queues = self.st.queues.lock().await;
            if !queues.contains_key(&key) {
                let mut queue = Queue::new(guild_name.to_string(), channel, voice_channel);
                queue.songs.extend(songs.iter().cloned());
                queues.insert(key, queue);
                (true, false, songs.len())
            } else {
                let queue = queues.get_mut(&key).expect("checked above");
                queue.text_channel = channel;
                queue.guild_name = guild_name.to_string();
                let was_playing = queue.playing;
                let existing: HashSet<String> = queue.songs.iter().filter_map(song_id).collect();
                let fresh: Vec<Song> = songs
                    .iter()
                    .filter(|song| match song_id(song) {
                        Some(id) => !existing.contains(&id),
                        None => false,
                    })
                    .cloned()
                    .collect();
                let added = fresh.len();
                queue.songs.extend(fresh);
                log(&format!("[Queue Added] Server: {guild_name} | Added {added} tracks"));
                (false, was_playing, added)
            }
        };

        let first = songs.first().cloned();
        if new_queue {
            if playlist_reply {
                send_embed(
                    ctx,
                    channel,
                    CreateEmbed::new().colour(GREEN).description(format!(
                        "Added **{}** tracks from playlist to queue!",
                        songs.len()
                    )),
                )
                .await;
            }
            if !play_current(&self.app, &self.st, ctx, key).await {
                playback_unavailable(ctx, channel, first.as_ref()).await;
            }
            return;
        }

        if was_playing {
            if let Some(label) = &source_label {
                send_embed(
                    ctx,
                    channel,
                    CreateEmbed::new().colour(GREEN).description(format!(
                        "Added **{added}** tracks from {label} to queue!"
                    )),
                )
                .await;
            } else if playlist_reply {
                send_embed(
                    ctx,
                    channel,
                    CreateEmbed::new().colour(GREEN).description(format!(
                        "Added **{added}** tracks from playlist to queue!"
                    )),
                )
                .await;
            } else if let Some(song) = &first {
                let artist = if song.artist.is_empty() || song.artist == "Unknown Artist" {
                    String::new()
                } else {
                    format!(" by {}", song.artist)
                };
                let mut embed = CreateEmbed::new().colour(GREEN).description(format!(
                    "Added to queue: [{}]({}){artist}",
                    song.title, song.url
                ));
                if let Some(id) = song_id(song) {
                    embed = embed.thumbnail(format!("https://img.youtube.com/vi/{id}/mqdefault.jpg"));
                }
                send_embed(ctx, channel, embed).await;
            }
            return;
        }

        if let Some(label) = &source_label {
            send_embed(
                ctx,
                channel,
                CreateEmbed::new().colour(GREEN).description(format!(
                    "Added **{}** tracks from {label} to queue!",
                    songs.len()
                )),
            )
            .await;
        } else if playlist_reply {
            send_embed(
                ctx,
                channel,
                CreateEmbed::new().colour(GREEN).description(format!(
                    "Added **{}** tracks from playlist to queue!",
                    songs.len()
                )),
            )
            .await;
        }
        playback_unavailable(ctx, channel, first.as_ref()).await;
    }
}

// ================== ALONE TIMER ==================

async fn refresh_alone_timer(st: Arc<State>, ctx: Context, guild_id: GuildId) {
    if let Some(handle) = st.alone.lock().await.remove(&guild_id.get()) {
        handle.abort();
    }
    let channel = {
        let queues = st.queues.lock().await;
        queues.get(&guild_id.get()).and_then(|q| q.voice_channel)
    };
    let Some(channel) = channel else { return };
    if humans_in_channel(&ctx, guild_id, channel) > 0 {
        return;
    }
    let busy = {
        let queues = st.queues.lock().await;
        queues
            .get(&guild_id.get())
            .map(|q| q.playing || !q.songs.is_empty() || q.current_song.is_some())
            .unwrap_or(false)
    };
    if !busy {
        return;
    }
    log(&format!("[Alone] Server: {guild_id} | no humans, stopping music in 5 min"));

    let inner = st.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        inner.alone.lock().await.remove(&guild_id.get());
        if humans_in_channel(&ctx, guild_id, channel) > 0 {
            return;
        }
        destroy_queue(&inner, guild_id.get(), false).await;
        log(&format!(
            "[Alone] Server: {guild_id} | stopped music, voice empty 5 min (staying in voice)"
        ));
    });
    st.alone.lock().await.insert(guild_id.get(), handle);
}

// ================== EVENT HANDLER ==================

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        log(&format!("Online as: {}", ready.user.tag()));

        {
            let ctx = ctx.clone();
            let st = self.st.clone();
            tokio::spawn(async move {
                loop {
                    let active = active_users(&ctx, &st).await;
                    ctx.set_activity(Some(ActivityData::listening(format!("!h | {active}"))));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }

        {
            let ctx = ctx.clone();
            let st = self.st.clone();
            tokio::spawn(async move {
                loop {
                    update_stats(&st, &ctx).await;
                    tokio::time::sleep(Duration::from_secs(300)).await;
                }
            });
        }

        for guild_id in ctx.cache.guilds() {
            setup_custom_voice(&ctx, guild_id).await;
        }
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        let Some(channel_id) = self.app.cfg.discord_welcome_channel else {
            log("[Welcome] DISCORD_WELCOME_CHANNEL_ID not set in .env");
            return;
        };
        let (title, text) = match (
            env_s("DISCORD_WELCOME_TITLE"),
            env_s("DISCORD_WELCOME_TEXT"),
        ) {
            (Some(t), Some(x)) => (t, x),
            _ => {
                log("[Welcome] DISCORD_WELCOME_TITLE / _TEXT not set in .env");
                return;
            }
        };
        let opts = CardOpts {
            title: &title,
            text: &text,
            label: "Welcome",
            title_color: "#7dd3fc",
        };
        let png = match build_card(&self.app, &member.user, &opts).await {
            Ok(png) => png,
            Err(e) => {
                log(&format!("[Welcome] card failed: {e}"));
                return;
            }
        };
        let count = ctx
            .cache
            .guild(member.guild_id)
            .map(|g| g.member_count)
            .unwrap_or(0);
        let content = format!("You are member **#{count}** <@{}>", member.user.id);
        if let Err(e) = send_card(&ctx, ChannelId::new(channel_id), &content, png, "welcome.png").await {
            log(&format!("[Welcome] Send failed: {e}"));
            return;
        }
        let guild_name = ctx
            .cache
            .guild(member.guild_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        log(&format!("[Welcome] Sent for {} in {guild_name}", member.user.tag()));
    }

    async fn guild_member_removal(
        &self,
        ctx: Context,
        guild_id: GuildId,
        user: User,
        _member: Option<Member>,
    ) {
        let Some(channel_id) = self.app.cfg.discord_goodbye_channel else {
            log("[Goodbye] DISCORD_GOODBYE_CHANNEL_ID not set in .env");
            return;
        };
        let (title, text) = match (
            env_s("DISCORD_GOODBYE_TITLE"),
            env_s("DISCORD_GOODBYE_TEXT"),
        ) {
            (Some(t), Some(x)) => (t, x),
            _ => {
                log("[Goodbye] DISCORD_GOODBYE_TITLE / _TEXT not set in .env");
                return;
            }
        };
        let opts = CardOpts {
            title: &title,
            text: &text,
            label: "Goodbye",
            title_color: "#ef4444",
        };
        let png = match build_card(&self.app, &user, &opts).await {
            Ok(png) => png,
            Err(e) => {
                log(&format!("[Goodbye] card failed: {e}"));
                return;
            }
        };
        let content = format!("See you <@{}>", user.id);
        if let Err(e) = send_card(&ctx, ChannelId::new(channel_id), &content, png, "goodbye.png").await {
            log(&format!("[Goodbye] Send failed: {e}"));
            return;
        }
        let guild_name = ctx
            .cache
            .guild(guild_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        log(&format!("[Goodbye] Sent for {} in {guild_name}", user.tag()));
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let Some(guild_id) = msg.guild_id else { return };

        if let Some(channel_id) = self.app.cfg.discord_chatbot_channel {
            if msg.channel_id.get() == channel_id && !msg.content.trim().is_empty() {
                self.handle_chat(&ctx, &msg).await;
                return;
            }
        }

        let content = msg.content.trim().to_string();
        if !content.starts_with('!') {
            return;
        }
        let _ = msg.channel_id.broadcast_typing(&ctx.http).await;

        let mut parts = content[1..].trim().split(' ').filter(|p| !p.is_empty());
        let command = parts.next().unwrap_or_default().to_lowercase();
        let args: Vec<&str> = parts.collect();

        let guild_name = ctx
            .cache
            .guild(guild_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        let channel_name = ctx
            .cache
            .guild(guild_id)
            .and_then(|g| g.channels.get(&msg.channel_id).map(|c| c.name.clone()))
            .unwrap_or_default();
        log(&format!(
            "[Command] Server: {guild_name} | From {} in #{channel_name}: {content}",
            msg.author.tag()
        ));

        if command == "p" {
            let voice_channel = user_voice_channel(&ctx, guild_id, msg.author.id);
            self.command_play(&ctx, &msg, guild_id, &guild_name, voice_channel, &args)
                .await;
            return;
        }
        self.handle_command(&ctx, &msg, guild_id, &command, &args).await;
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild_id) = new.guild_id.or_else(|| old.as_ref().and_then(|o| o.guild_id)) else {
            return;
        };

        if let Some(joined) = new.channel_id {
            let is_trigger = ctx
                .cache
                .guild(guild_id)
                .and_then(|g| g.channels.get(&joined).map(|c| c.name.to_lowercase().contains("┃custom")))
                .unwrap_or(false);
            if is_trigger {
                if let Some(member) = new.member.clone() {
                    let parent = ctx
                        .cache
                        .guild(guild_id)
                        .and_then(|g| g.channels.get(&joined).and_then(|c| c.parent_id));
                    let mut create = CreateChannel::new(format!("🦕┃{}", member.display_name()))
                        .kind(ChannelType::Voice)
                        .permissions(vec![PermissionOverwrite {
                            allow: Permissions::MANAGE_CHANNELS
                                | Permissions::CONNECT
                                | Permissions::SPEAK
                                | Permissions::MUTE_MEMBERS
                                | Permissions::DEAFEN_MEMBERS
                                | Permissions::MOVE_MEMBERS,
                            deny: Permissions::empty(),
                            kind: PermissionOverwriteType::Member(member.user.id),
                        }]);
                    if let Some(parent) = parent {
                        create = create.category(parent);
                    }
                    match guild_id.create_channel(&ctx.http, create).await {
                        Ok(created) => {
                            self.st.temp_voice.lock().await.insert(created.id.get());
                            if let Err(e) = member.move_to_voice_channel(&ctx.http, created.id).await {
                                log(&format!("[Custom Voice Error] Failed to move member: {e}"));
                            }
                            log(&format!(
                                "[Custom Voice] Created \"{}\" for {}",
                                created.name,
                                member.user.tag()
                            ));
                        }
                        Err(e) => log(&format!("[Custom Voice Error] Failed to create dynamic channel: {e}")),
                    }
                }
            }
        }

        if let Some(left) = old.as_ref().and_then(|o| o.channel_id) {
            let is_temp = self.st.temp_voice.lock().await.contains(&left.get());
            if is_temp {
                let empty = ctx
                    .cache
                    .guild(guild_id)
                    .map(|g| !g.voice_states.values().any(|v| v.channel_id == Some(left)))
                    .unwrap_or(false);
                if empty {
                    self.st.temp_voice.lock().await.remove(&left.get());
                    match left.delete(&ctx.http).await {
                        Ok(_) => log(&format!("[Custom Voice] Deleted empty channel {left}")),
                        Err(e) => log(&format!("[Custom Voice Error] Failed to delete channel: {e}")),
                    }
                }
            }
        }

        refresh_alone_timer(self.st.clone(), ctx.clone(), guild_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ids_from_every_url_shape() {
        for url in [
            "https://youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "dQw4w9WgXcQ",
        ] {
            assert_eq!(extract_video_id(url).as_deref(), Some("dQw4w9WgXcQ"));
        }
        assert_eq!(extract_video_id("not a video"), None);
        assert_eq!(
            normalize_url("https://music.youtube.com/watch?v=dQw4w9WgXcQ"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    #[test]
    fn titles_and_keys_are_cleaned() {
        assert_eq!(
            clean_title("Rick Astley - Never Gonna Give You Up (Official Video)", "Rick Astley"),
            "Never Gonna Give You Up"
        );
        assert_eq!(clean_artist("Rick Astley - Topic"), "Rick Astley");
        assert_eq!(
            normalize_song_key("Never Gonna Give You Up (Remastered)", "Rick Astley - Topic"),
            "rickastley:nevergonnagiveyouup"
        );
    }

    #[test]
    fn token_budget_keeps_tail_and_starts_on_user() {
        let msgs: Vec<Value> = (0..10)
            .map(|i| {
                json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": "x".repeat(400),
                })
            })
            .collect();
        let kept = trim_to_token_budget(&msgs, 250);
        assert!(kept.len() < msgs.len());
        assert_eq!(kept[0]["role"], "user");
    }

    #[test]
    fn card_svg_renders_png() {
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{CARD_W}" height="{CARD_H}">
<rect width="{CARD_W}" height="{CARD_H}" fill="#000000"/>
<image href="{}" x="325" y="30" width="150" height="150"/>
<text x="400" y="245" font-size="48" font-family="Noto Serif, serif" fill="#ffffff" text-anchor="middle">Welcome</text>
</svg>"##,
            data_uri(b"\x89PNG\r\n\x1a\n", "image/png")
        );
        // Unrenderable 1x1 image data is tolerated; the text layer must still produce a PNG.
        let png = render_png(&svg).expect("render");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn chatbot_endpoint_shapes() {
        assert_eq!(build_chatbot_endpoint("http://x/v1/"), "http://x/v1/chat/completions");
        assert_eq!(
            build_chatbot_endpoint("http://x/v1/chat/completions"),
            "http://x/v1/chat/completions"
        );
        assert_eq!(build_chatbot_endpoint("http://x"), "http://x/v1/chat/completions");
    }
}

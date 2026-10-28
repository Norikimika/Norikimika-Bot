//! Discord bot (port of discord.js): songbird voice playback with autoplay radio,
//! Spotify/Apple Music/SoundCloud/playlist resolvers, welcome/goodbye banner images,
//! server stats channels.
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

use serenity::all::{
    ChannelId, ChannelType, Client, Context, CreateAttachment, CreateChannel, CreateEmbed,
    CreateMessage, EditChannel, EventHandler, GatewayIntents, GuildId, Member, Message, Permissions,
    Ready, User,
};
use serenity::async_trait;
use songbird::input::{HttpRequest, Input};
use songbird::{
    Event as VoiceEvent, EventContext, EventHandler as VoiceEventHandler, SerenityInit, TrackEvent,
};
use songbird::tracks::PlayMode;
use tokio::sync::Mutex;

const MOD: &str = "discord";
/// Bundled banners (from the JS repo's `banner/`) — always used, no env path needed.
const WELCOME_BG: &[u8] = include_bytes!("../assets/banners/welcome.jpg");
const GOODBYE_BG: &[u8] = include_bytes!("../assets/banners/goodbye.jpg");
const SPOTIFY_MAX_TRACKS: usize = 50;
const ALONE_TIMEOUT_SECS: u64 = 300;
const PLAY_FAIL_LIMIT: u32 = 3;
const YT_FALLBACK_CLIENTS: &[&str] = &["web_safari", "mweb", "tv"];
const YT_PRIMARY_CLIENT_ARGS: [&str; 2] = ["--extractor-args", "youtube:player_client=android_vr"];
const STATS_DEFS: &[(&str, &str)] = &[
    ("all", "🧩┃all members"),
    ("humans", "🪢┃members"),
    ("bots", "🤖┃bots"),
];
const STATS_CATEGORY_NAME: &str = "╰┈➤ Stats";

// ================== song model ==================

#[derive(Clone, Default)]
struct Song {
    title: String,
    artist: String,
    url: String,
    id: Option<String>,
    requested_by: Option<u64>,
    manual: bool,
}

#[derive(Clone)]
struct GuildQueue {
    songs: Vec<Song>,
    last_song: Option<Song>,
    autoplay: bool,
    playing: bool,
    fail_count: u32,
    text_channel: Option<ChannelId>,
    played_ids: HashSet<String>,
    played_keys: HashSet<String>,
    played_urls: HashSet<String>,
}

impl Default for GuildQueue {
    fn default() -> Self {
        Self {
            songs: Vec::new(),
            last_song: None,
            autoplay: true,
            playing: false,
            fail_count: 0,
            text_channel: None,
            played_ids: HashSet::new(),
            played_keys: HashSet::new(),
            played_urls: HashSet::new(),
        }
    }
}

fn queues() -> &'static Mutex<HashMap<GuildId, GuildQueue>> {
    static QUEUES: OnceLock<Mutex<HashMap<GuildId, GuildQueue>>> = OnceLock::new();
    QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn radio_cache() -> &'static Mutex<HashMap<String, Vec<Song>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<Song>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn video_cache() -> &'static Mutex<HashMap<String, Song>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Song>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn alone_timers() -> &'static Mutex<HashMap<GuildId, tokio::task::JoinHandle<()>>> {
    static TIMERS: OnceLock<Mutex<HashMap<GuildId, tokio::task::JoinHandle<()>>>> = OnceLock::new();
    TIMERS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ================== text / metadata helpers ==================

fn extract_video_id(url: &str) -> Option<String> {
    let value = url.trim();
    let after = |marker: &str| -> Option<String> {
        let start = value.find(marker)? + marker.len();
        let id: String = value[start..].chars().take(11).collect();
        let ok = id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        ok.then_some(id)
    };
    after("v=")
        .or_else(|| after("youtu.be/"))
        .or_else(|| after("youtube.com/embed/"))
        .or_else(|| after("youtube.com/v/"))
        .or_else(|| {
            let ok = value.len() == 11 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            ok.then(|| value.to_string())
        })
}

fn normalize_url(url: &str) -> String {
    match extract_video_id(url) {
        Some(id) => format!("https://www.youtube.com/watch?v={id}"),
        None => url
            .replace("music.youtube.com", "www.youtube.com")
            .replace("m.youtube.com", "www.youtube.com"),
    }
}

fn clean_artist(value: &str) -> String {
    let mut value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return "Unknown Artist".into();
    }
    // "Official VEVO: Band" -> "Band"
    if let Some(pos) = value.find(':') {
        let head = value[..pos].to_lowercase();
        if head.contains("official") || head.contains("vevo") {
            value = value[pos + 1..].trim().to_string();
        }
    }
    let lowered = value.to_lowercase();
    let mut cut = value.len();
    for marker in ["-", "–", "—", ":"] {
        if let Some(pos) = lowered.find(marker) {
            let tail = &lowered[pos..];
            if tail.contains("topic") || tail.contains("official") || tail.contains("vevo") {
                cut = cut.min(pos);
            }
        }
    }
    let out = value[..cut].split_whitespace().collect::<Vec<_>>().join(" ");
    if out.is_empty() {
        "Unknown Artist".into()
    } else {
        out
    }
}

fn clean_title(title: &str, artist: &str) -> String {
    let mut value = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let artist = clean_artist(artist);
    if artist != "Unknown Artist" {
        let needle = artist.to_lowercase();
        if value.to_lowercase().starts_with(&needle) {
            value = value[artist.len()..].trim_start().trim_start_matches(['-', '–', '—', ':']).trim_start().to_string();
        }
        if let Some(pos) = value.to_lowercase().rfind(&needle) {
            let before = value[..pos].trim_end();
            let trimmed = before.trim_end_matches(['-', '–', '—', ':']).trim_end();
            if trimmed.len() < before.len() {
                value = trimmed.to_string();
            }
        }
    }
    for noise in [
        "(official", "[official", "(lyric", "[lyric", "(audio", "[audio", "(video", "[video",
        "(mv", "(visualizer", "(remix", "(cover", "(lagu",
    ] {
        if let Some(pos) = value.to_lowercase().find(noise) {
            value = value[..pos].trim_end().to_string();
        }
    }
    let lower = value.to_lowercase();
    if let Some(pos) = lower.rfind(" lyrics").or_else(|| lower.rfind(" lyric")) {
        value = value[..pos].to_string();
    }
    let out = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.is_empty() {
        title.to_string()
    } else {
        out
    }
}

fn normalize_song(mut song: Song) -> Song {
    song.artist = clean_artist(&song.artist);
    song.title = clean_title(&song.title, &song.artist);
    song.url = normalize_url(&song.url);
    if song.id.is_none() {
        song.id = extract_video_id(&song.url);
    }
    song
}

fn song_id(song: &Song) -> Option<String> {
    song.id.clone().or_else(|| extract_video_id(&song.url))
}

fn song_key(song: &Song) -> String {
    let norm = |s: &str, strip: &[&str]| {
        let mut t = s.to_lowercase();
        for token in strip {
            t = t.replace(token, "");
        }
        t.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()
    };
    format!(
        "{}:{}",
        norm(&song.artist, &["topic", "official", "channel", "vevo"]),
        norm(&song.title, &[])
    )
}

fn song_url_key(song: &Song) -> String {
    match song_id(song) {
        Some(id) => format!("https://www.youtube.com/watch?v={id}"),
        None => normalize_url(&song.url),
    }
}

fn is_valid_song(song: &Song) -> bool {
    !song.title.is_empty() && !song.url.is_empty() && song_id(song).is_some()
}

/// Skips/stops are limited to the requester of the current track or an admin.
fn can_control_queue(msg: &Message, queue: Option<&GuildQueue>) -> bool {
    let Some(queue) = queue else { return true };
    let Some(current) = queue.songs.first().or(queue.last_song.as_ref()) else { return true };
    let Some(requester) = current.requested_by else { return true };
    if requester == msg.author.id.get() {
        return true;
    }
    msg.member
        .as_ref()
        .and_then(|m| m.permissions)
        .map(|p| p.contains(Permissions::ADMINISTRATOR) || p.contains(Permissions::MANAGE_CHANNELS))
        .unwrap_or(false)
}

// ================== yt-dlp plumbing ==================

fn is_bot_check(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "confirm you're not a bot",
        "confirm youre not a bot",
        "sign in to confirm",
        "page needs to be reloaded",
        "precondition check failed",
        "requested format is not available",
        "no video formats found",
        "unable to download video data",
        "http error 403",
        "http error 429",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_client_arg(extra: &[String]) -> bool {
    extra.iter().any(|a| a.contains("player_client"))
}

/// Next unused extractor client, used to retry once YouTube throws a bot check at us.
fn next_bot_check_args(extra: &[String]) -> Option<Vec<String>> {
    let used: Vec<&str> = extra
        .iter()
        .filter_map(|a| a.strip_prefix("youtube:player_client="))
        .collect();
    YT_FALLBACK_CLIENTS
        .iter()
        .find(|c| !used.contains(c))
        .map(|c| vec!["--extractor-args".to_string(), format!("youtube:player_client={c}")])
}

async fn run_ytdlp(args: &[String], extra: &[String], target: &str, timeout: Duration) -> Result<String> {
    let mut all: Vec<String> = Vec::new();
    if let Some(proxy) = crate::config::get("YTDLP_PROXY").filter(|p| !p.is_empty()) {
        all.push("--proxy".to_string());
        all.push(proxy);
    }
    let mut client_args: Vec<String> = Vec::new();
    if !has_client_arg(args) && !has_client_arg(extra) {
        client_args = YT_PRIMARY_CLIENT_ARGS.iter().map(|s| s.to_string()).collect();
    }
    all.extend(args.iter().cloned());
    all.extend(client_args);
    all.extend(extra.iter().cloned());
    all.push(target.to_string());

    let child = tokio::process::Command::new("yt-dlp")
        .args(&all)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Ok(out)) => Err(anyhow!(
            "yt-dlp exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Err(e)) => Err(anyhow!("yt-dlp spawn error: {e}")),
        Err(_) => Err(anyhow!("yt-dlp timeout after {timeout:?}")),
    }
}

/// Run yt-dlp, retrying with a different YouTube client when it reports a bot check.
async fn spawn_ytdlp(args: Vec<String>, target: String, timeout: Duration) -> Result<String> {
    let mut extra: Vec<String> = Vec::new();
    loop {
        match run_ytdlp(&args, &extra, &target, timeout).await {
            Ok(out) => return Ok(out),
            Err(e) => match next_bot_check_args(&extra) {
                Some(next) if is_bot_check(&e.to_string()) => extra = next,
                _ => return Err(e),
            },
        }
    }
}

fn build_track_from_item(item: &Value) -> Option<Song> {
    let id = item["id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| item["webpage_url"].as_str().and_then(extract_video_id))
        .or_else(|| item["url"].as_str().and_then(extract_video_id))?;
    let title = item["title"].as_str().unwrap_or("");
    if title.is_empty() || title == "[Private video]" || title == "[Deleted video]" {
        return None;
    }
    let song = normalize_song(Song {
        title: title.to_string(),
        artist: item["artist"]
            .as_str()
            .or(item["uploader"].as_str())
            .or(item["channel"].as_str())
            .or(item["creator"].as_str())
            .unwrap_or("Unknown Artist")
            .to_string(),
        url: format!("https://www.youtube.com/watch?v={id}"),
        id: Some(id),
        ..Default::default()
    });
    is_valid_song(&song).then_some(song)
}

fn flat_entries(stdout: &str) -> Vec<Song> {
    let data: Value = serde_json::from_str(stdout).unwrap_or(Value::Null);
    data["entries"]
        .as_array()
        .map(|entries| entries.iter().filter_map(build_track_from_item).collect())
        .unwrap_or_default()
}

async fn search_yt_music(query: &str, limit: usize) -> Vec<Song> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let args = vec![
        "--flat-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-cache-dir".to_string(),
        "--skip-download".to_string(),
        "--playlist-end".to_string(),
        limit.to_string(),
        "-J".to_string(),
    ];
    match spawn_ytdlp(args, format!("ytsearch{limit}:{query}"), Duration::from_secs(60)).await {
        Ok(stdout) => flat_entries(&stdout),
        Err(e) => {
            logln!(MOD, "[yt-dlp Search Error] {query}: {e}");
            Vec::new()
        }
    }
}

async fn get_playlist_tracks(url: &str) -> Vec<Song> {
    if url.is_empty() {
        return Vec::new();
    }
    let args = vec![
        "--flat-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-cache-dir".to_string(),
        "--skip-download".to_string(),
        "-J".to_string(),
    ];
    match spawn_ytdlp(args, url.to_string(), Duration::from_secs(180)).await {
        Ok(stdout) => flat_entries(&stdout),
        Err(e) => {
            logln!(MOD, "[yt-dlp Playlist Error] {url}: {e}");
            Vec::new()
        }
    }
}

async fn get_radio_tracks(video_id: &str) -> Vec<Song> {
    if video_id.is_empty() {
        return Vec::new();
    }
    if let Some(cached) = radio_cache().lock().await.get(video_id) {
        return cached.clone();
    }
    let args = vec![
        "--flat-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-cache-dir".to_string(),
        "--skip-download".to_string(),
        "--playlist-end".to_string(),
        "15".to_string(),
        "-J".to_string(),
    ];
    let target = format!("https://music.youtube.com/watch?v={video_id}&list=RD{video_id}");
    match spawn_ytdlp(args, target, Duration::from_secs(90)).await {
        Ok(stdout) => {
            let tracks = flat_entries(&stdout);
            if !tracks.is_empty() {
                radio_cache().lock().await.insert(video_id.to_string(), tracks.clone());
            }
            tracks
        }
        Err(e) => {
            logln!(MOD, "[Autoplay Radio Error] {e}");
            Vec::new()
        }
    }
}

async fn fetch_video_info(video_id: &str) -> Option<Song> {
    if video_id.is_empty() {
        return None;
    }
    if let Some(cached) = video_cache().lock().await.get(video_id) {
        return Some(cached.clone());
    }
    let args = vec![
        "--no-warnings".to_string(),
        "--no-cache-dir".to_string(),
        "--skip-download".to_string(),
        "-j".to_string(),
    ];
    let stdout = match spawn_ytdlp(args, format!("https://www.youtube.com/watch?v={video_id}"), Duration::from_secs(60)).await {
        Ok(out) => out,
        Err(e) => {
            logln!(MOD, "[Video Info Error] {video_id}: {e}");
            return None;
        }
    };
    let data: Value = serde_json::from_str(&stdout).ok()?;
    let song = normalize_song(Song {
        title: data["title"].as_str().unwrap_or("").to_string(),
        artist: data["artist"]
            .as_str()
            .or(data["uploader"].as_str())
            .or(data["channel"].as_str())
            .unwrap_or("Unknown Artist")
            .to_string(),
        url: format!("https://www.youtube.com/watch?v={video_id}"),
        id: Some(video_id.to_string()),
        ..Default::default()
    });
    if !is_valid_song(&song) {
        return None;
    }
    video_cache().lock().await.insert(video_id.to_string(), song.clone());
    Some(song)
}

// ================== external resolvers ==================

async fn http_text(url: &str) -> Result<String> {
    let res = crate::util::http()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/124")
        .timeout(Duration::from_secs(15))
        .build()?
        .get(url)
        .send()
        .await?;
    if !res.status().is_success() {
        return Err(anyhow!("HTTP {} from {}", res.status(), url.split('?').next().unwrap_or(url)));
    }
    Ok(res.text().await?)
}

fn extract_spotify(query: &str) -> Option<(String, String)> {
    let marker = "open.spotify.com/";
    let rest = &query[query.find(marker)? + marker.len()..];
    let rest = rest
        .strip_prefix("intl-")
        .and_then(|r| r.split_once('/').map(|(_, tail)| tail))
        .unwrap_or(rest);
    let mut parts = rest.split('/');
    let kind = parts.next()?;
    if !matches!(kind, "track" | "album" | "playlist") {
        return None;
    }
    let id: String = parts.next()?.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!id.is_empty()).then(|| (kind.to_string(), id))
}

fn extract_apple(query: &str) -> Option<(String, String)> {
    let marker = "music.apple.com/";
    let rest = &query[query.find(marker)? + marker.len()..];
    let mut parts = rest.split('/');
    let _locale = parts.next()?;
    let kind = parts.next()?;
    if !matches!(kind, "song" | "album") {
        return None;
    }
    for part in parts {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 6 {
            return Some((kind.to_string(), digits));
        }
    }
    None
}

fn extract_soundcloud(query: &str) -> Option<String> {
    let start = query.find("soundcloud.com/")?;
    let head = &query[..start];
    let url: String = query[head.rfind("http").unwrap_or(0)..]
        .split_whitespace()
        .next()?
        .to_string();
    let tail = url.split("soundcloud.com/").nth(1)?;
    let mut parts = tail.split('/');
    let user = parts.next()?;
    let track = parts.next()?;
    if user == "pages" || user == "discover" || track.starts_with("sets") {
        return None;
    }
    Some(url)
}

async fn fetch_spotify_tracks(kind: &str, id: &str) -> Result<Vec<(String, String)>> {
    let html = http_text(&format!("https://open.spotify.com/embed/{kind}/{id}")).await?;
    let start = html.find("__NEXT_DATA__").ok_or_else(|| anyhow!("Spotify embed data not found"))?;
    let after = &html[start..];
    let json_start = after.find('>').ok_or_else(|| anyhow!("Spotify embed data not found"))? + 1;
    let json_end = after.find("</script>").ok_or_else(|| anyhow!("Spotify embed data not found"))?;
    let data: Value = serde_json::from_str(after[json_start..json_end].trim())?;
    let entity = &data["props"]["pageProps"]["state"]["data"]["entity"];
    if entity.is_null() {
        return Err(anyhow!("Spotify entity missing"));
    }
    if kind == "track" {
        let artists = entity["artists"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        return Ok(vec![(entity["name"].as_str().unwrap_or("").to_string(), artists)]);
    }
    let rows = entity["trackList"]
        .as_array()
        .map(|list| {
            list.iter()
                .take(SPOTIFY_MAX_TRACKS)
                .map(|t| {
                    (
                        t["title"].as_str().unwrap_or("").to_string(),
                        t["subtitle"].as_str().unwrap_or("").split(',').next().unwrap_or("").trim().to_string(),
                    )
                })
                .filter(|(title, _)| !title.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(rows)
}

async fn fetch_apple_tracks(kind: &str, id: &str) -> Result<Vec<(String, String)>> {
    let url = if kind == "song" {
        format!("https://itunes.apple.com/lookup?id={id}")
    } else {
        format!("https://itunes.apple.com/lookup?id={id}&entity=song")
    };
    let data: Value = serde_json::from_str(&http_text(&url).await?)?;
    let results = data["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return Err(anyhow!("Apple Music: not found"));
    }
    let tracks: Vec<(String, String)> = results
        .iter()
        .filter(|r| r["wrapperType"].as_str() == Some("track"))
        .map(|t| {
            (
                t["trackName"].as_str().unwrap_or("").to_string(),
                t["artistName"].as_str().unwrap_or("").to_string(),
            )
        })
        .filter(|(title, _)| !title.is_empty())
        .collect();
    if kind == "song" {
        tracks.first().cloned().map(|t| vec![t]).ok_or_else(|| anyhow!("Apple Music: no track data"))
    } else if tracks.is_empty() {
        Err(anyhow!("Apple Music: album has no tracks"))
    } else {
        Ok(tracks.into_iter().take(SPOTIFY_MAX_TRACKS).collect())
    }
}

async fn fetch_soundcloud_track(url: &str) -> Result<Vec<(String, String)>> {
    let body = http_text(&format!("https://soundcloud.com/oembed?format=json&url={}", urlencode(url))).await?;
    let data: Value = serde_json::from_str(&body)?;
    let title = data["title"].as_str().unwrap_or("");
    let idx = title.rfind(" by ").ok_or_else(|| anyhow!("SoundCloud: unexpected oEmbed title"))?;
    Ok(vec![(title[..idx].to_string(), title[idx + 4..].trim().to_string())])
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Resolve metadata rows to playable YouTube songs, 3 workers at a time.
async fn resolve_meta_to_songs(tracks: Vec<(String, String)>, requester: Option<u64>, label: &str) -> Vec<Song> {
    if tracks.is_empty() {
        return Vec::new();
    }
    logln!(MOD, "[{label}] Resolving {} track(s)", tracks.len());
    let mut found = Vec::new();
    for chunk in tracks.chunks(3) {
        let mut handles = Vec::new();
        for (artist, title) in chunk.iter().cloned() {
            handles.push(tokio::spawn(async move {
                search_yt_music(&format!("{artist} - {title}"), 1).await.into_iter().next()
            }));
        }
        for handle in handles {
            if let Ok(Some(mut hit)) = handle.await {
                hit.requested_by = requester;
                hit.manual = true;
                let song = normalize_song(hit);
                if is_valid_song(&song) {
                    found.push(song);
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    logln!(MOD, "[{label}] Matched {}/{} track(s) on YouTube", found.len(), tracks.len());
    found
}

// ================== playback ==================

async fn autoplay_candidate(queue: &GuildQueue, base: &Song) -> Option<Song> {
    let radio = get_radio_tracks(&song_id(base)?).await;
    for candidate in radio {
        if !is_valid_song(&candidate) {
            continue;
        }
        let Some(id) = song_id(&candidate) else { continue };
        if Some(id.clone()) == song_id(base) {
            continue;
        }
        if queue.played_ids.contains(&id)
            || queue.played_keys.contains(&song_key(&candidate))
            || queue.played_urls.contains(&song_url_key(&candidate))
        {
            continue;
        }
        let mut song = candidate;
        song.requested_by = base.requested_by;
        song.manual = false;
        return Some(normalize_song(song));
    }
    None
}

async fn send_embed_to(channel: Option<ChannelId>, http: &serenity::http::Http, description: &str, colour: u32) {
    if let Some(channel) = channel {
        let embed = CreateEmbed::new().colour(colour).description(description);
        let _ = channel.send_message(http, CreateMessage::new().embed(embed)).await;
    }
}

async fn destroy_queue(guild_id: GuildId) {
    if let Some(handle) = alone_timers().lock().await.remove(&guild_id) {
        handle.abort();
    }
    queues().lock().await.remove(&guild_id);
}

/// Drop the head of the queue and play whatever is next.
async fn advance_queue(ctx: &Context, guild_id: GuildId) {
    {
        let mut all = queues().lock().await;
        if let Some(queue) = all.get_mut(&guild_id) {
            queue.playing = false;
            if !queue.songs.is_empty() {
                queue.songs.remove(0);
            }
        } else {
            return;
        }
    }
    play_next(ctx, guild_id).await;
}

/// Enqueue the head of the queue into songbird; pulls a radio track first when autoplay is on.
fn play_next<'a>(
    ctx: &'a Context,
    guild_id: GuildId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let prepared = loop {
            let mut all = queues().lock().await;
            let Some(queue) = all.get_mut(&guild_id) else { return };
            if queue.playing {
                return;
            }
            if queue.fail_count >= PLAY_FAIL_LIMIT {
                let channel = queue.text_channel;
                drop(all);
                send_embed_to(channel, &ctx.http, "Too many consecutive playback errors.", 0xFF0000).await;
                destroy_queue(guild_id).await;
                return;
            }
            if queue.songs.is_empty() {
                match (queue.autoplay, queue.last_song.clone()) {
                    (true, Some(last)) => {
                        let snapshot = queue.clone();
                        drop(all);
                        match autoplay_candidate(&snapshot, &last).await {
                            Some(next) => {
                                let mut all = queues().lock().await;
                                let Some(queue) = all.get_mut(&guild_id) else { return };
                                queue.played_ids.insert(song_id(&next).unwrap_or_default());
                                queue.played_keys.insert(song_key(&next));
                                queue.played_urls.insert(song_url_key(&next));
                                queue.songs.push(next);
                                drop(all);
                                continue;
                            }
                            None => {
                                send_embed_to(snapshot.text_channel, &ctx.http, "Autoplay could not find another track.", 0xFF0000).await;
                                destroy_queue(guild_id).await;
                                return;
                            }
                        }
                    }
                    (true, None) | (false, _) => {
                        let channel = queue.text_channel;
                        drop(all);
                        send_embed_to(channel, &ctx.http, "Queue finished.", 0x0099FF).await;
                        destroy_queue(guild_id).await;
                        return;
                    }
                }
            }
            // reuse the guard we already hold — locking queues() again here deadlocked every
            // music command (tokio Mutex is not reentrant)
            let song = normalize_song(queue.songs[0].clone());
            if !is_valid_song(&song) {
                queue.songs.remove(0);
                drop(all);
                continue;
            }
            queue.songs[0] = song.clone();
            queue.last_song = Some(song.clone());
            queue.played_ids.insert(song_id(&song).unwrap_or_default());
            queue.played_keys.insert(song_key(&song));
            queue.played_urls.insert(song_url_key(&song));
            queue.playing = true;
            queue.fail_count = 0;
            let channel = queue.text_channel;
            break (song, channel);
        };

        let (song, channel) = prepared;
        let Some(songbird) = songbird::get(ctx).await.map(|s| s.clone()) else {
            logln!(MOD, "[Music] songbird not registered, cannot play {}", song.title);
            if let Some(queue) = queues().lock().await.get_mut(&guild_id) {
                queue.playing = false;
            }
            send_embed_to(channel, &ctx.http, "Voice is not available right now.", 0xFF0000).await;
            return;
        };
        let mut enqueued = false;
        if let Some(call) = songbird.get(guild_id) {
            let mut call = call.lock().await;
            match resolve_stream(&song.url).await {
                Ok(stream) => {
                    // songbird would send exactly this request; we send it first so a 403
                    // says why instead of surfacing as "played 0ns".
                    let input: Input = HttpRequest {
                        client: crate::util::sb_client(),
                        request: stream.url,
                        headers: stream.headers,
                        content_length: stream.len,
                    }
                    .into();
                    call.enqueue_input(input).await;
                    enqueued = true;
                }
                Err(e) => logln!(MOD, "[Music Resolve] {} | {e:#}", song.title),
            }
        }
        if !enqueued {
            {
                let mut all = queues().lock().await;
                if let Some(queue) = all.get_mut(&guild_id) {
                    queue.playing = false;
                    queue.fail_count += 1;
                }
            }
            send_embed_to(channel, &ctx.http, &format!("Failed to play: **{}**. Skipping...", song.title), 0xFF0000).await;
            advance_queue(ctx, guild_id).await;
            return;
        }

        let artist_text = (song.artist != "Unknown Artist").then(|| format!(" by {}", song.artist)).unwrap_or_default();
        let req_text = match (song.manual, song.requested_by) {
            (true, Some(id)) => format!("\nRequested by: <@{id}>"),
            _ => "\n*(Autoplay)*".to_string(),
        };
        let mut embed = CreateEmbed::new()
            .colour(0x00FF00)
            .description(format!("Started playing [{}]({}){}{}", song.title, song.url, artist_text, req_text));
        if let Some(id) = song_id(&song) {
            embed = embed.thumbnail(format!("https://img.youtube.com/vi/{id}/mqdefault.jpg"));
        }
        if let Some(channel) = channel {
            let _ = channel.send_message(&ctx.http, CreateMessage::new().embed(embed)).await;
        }
        logln!(MOD, "[Music Stream Start] {guild_id} | {} - {}", song.title, song.artist);
    })
}

/// A direct CDN stream plus the headers yt-dlp says it needs.
struct Stream {
    url: String,
    headers: reqwest12::header::HeaderMap,
    len: Option<u64>,
}

/// Resolve a watch URL to a playable stream and prove it answers before songbird sees it.
///
/// songbird's `YoutubeDl` keeps the CDN's reply to itself, so a 403 turned into
/// "played 0ns of the stream"; doing the request here means the log says what YouTube said.
/// Two extraction strategies: m4a/AAC (what symphonia can decode) and, if the CDN refuses,
/// the android_vr client.
async fn resolve_stream(web_url: &str) -> Result<Stream> {
    let mut last = String::from("no attempt ran");
    for (attempt, extra) in [
        ["-S", "ext:m4a,acodec:aac"],
        ["--extractor-args", "youtube:player_client=android_vr"],
    ]
    .iter()
    .enumerate()
    {
        let out = tokio::process::Command::new("yt-dlp")
            .args(["--js-runtimes", "node", "-j", "--no-playlist", "-f", "ba[abr>0][vcodec=none]/best"])
            .args(extra)
            .arg(web_url)
            .output()
            .await
            .map_err(|e| anyhow!("run yt-dlp: {e}"))?;
        if !out.status.success() {
            last = format!("yt-dlp {}", crate::util::truncate(String::from_utf8_lossy(&out.stderr).trim(), 160));
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            last = "yt-dlp printed no JSON".into();
            continue;
        };
        let Some(url) = v["url"].as_str().map(str::to_string) else {
            last = "yt-dlp found no stream url".into();
            continue;
        };
        let mut headers = reqwest12::header::HeaderMap::new();
        if let Some(map) = v["http_headers"].as_object() {
            for (k, val) in map {
                if let (Ok(name), Ok(value)) = (
                    reqwest12::header::HeaderName::from_bytes(k.as_bytes()),
                    reqwest12::header::HeaderValue::from_str(val.as_str().unwrap_or_default()),
                ) {
                    headers.insert(name, value);
                }
            }
        }
        let len = v["filesize"].as_u64().or_else(|| v["filesize_approx"].as_u64());
        let mut req = crate::util::sb_client().get(&url).headers(headers.clone());
        if let Some(l) = len {
            req = req.header(reqwest12::header::RANGE, format!("bytes=0-{}", l.saturating_sub(1)));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                logln!(
                    MOD,
                    "[Music HTTP] {status} {codec} {len:?} bytes",
                    status = resp.status(),
                    codec = v["acodec"].as_str().unwrap_or("?"),
                    len = len
                );
                return Ok(Stream { url, headers, len });
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last = format!("{status}: {}", crate::util::truncate(body.trim(), 160));
            }
            Err(e) => last = format!("{e:#}"),
        }
        logln!(MOD, "[Music HTTP] attempt {} refused: {last}", attempt + 1);
    }
    Err(anyhow!("{last}"))
}

struct TrackEndNotifier {
    guild_id: GuildId,
    ctx: Context,
}

#[async_trait]
impl VoiceEventHandler for TrackEndNotifier {
    async fn act(&self, ev: &EventContext<'_>) -> Option<VoiceEvent> {
        if let EventContext::Track(tracks) = ev {
            for (state, _) in tracks.iter() {
                logln!(
                    MOD,
                    "[Music End] {} | played {:?} of the stream",
                    self.guild_id,
                    state.play_time
                );
            }
        }
        advance_queue(&self.ctx, self.guild_id).await;
        None
    }
}

/// Without this a broken stream just looked like a fast skip.
struct TrackErrorNotifier {
    guild_id: GuildId,
}

#[async_trait]
impl VoiceEventHandler for TrackErrorNotifier {
    async fn act(&self, ev: &EventContext<'_>) -> Option<VoiceEvent> {
        if let EventContext::Track(tracks) = ev {
            for (state, _) in tracks.iter() {
                if let PlayMode::Errored(err) = &state.playing {
                    logln!(MOD, "[Music Error] {} | {err}", self.guild_id);
                }
            }
        }
        None
    }
}

async fn ensure_voice(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
    logln!(MOD, "[Voice] joining {channel_id} in {guild_id}");
    let songbird = songbird::get(ctx)
        .await
        .ok_or_else(|| anyhow!("songbird not registered"))?
        .clone();
    let handler = songbird
        .join(guild_id, channel_id)
        .await
        .map_err(|e| anyhow!("join failed: {e}"))?;
    {
        let mut call = handler.lock().await;
        call.add_global_event(
            VoiceEvent::Track(TrackEvent::End),
            TrackEndNotifier { guild_id, ctx: ctx.clone() },
        );
        call.add_global_event(
            VoiceEvent::Track(TrackEvent::Error),
            TrackErrorNotifier { guild_id },
        );
    }
    logln!(MOD, "[Voice] joined {channel_id}");
    Ok(())
}

fn humans_in(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> usize {
    let Some(guild) = ctx.cache.guild(guild_id) else { return 1 };
    guild
        .voice_states
        .values()
        .filter(|v| v.channel_id == Some(channel_id))
        .filter(|v| ctx.cache.user(v.user_id).map(|u| !u.bot).unwrap_or(true))
        .count()
}

/// Stop the music when the bot is left alone in its voice channel for 5 minutes.
async fn refresh_alone_timer(ctx: &Context, guild_id: GuildId) {
    if let Some(handle) = alone_timers().lock().await.remove(&guild_id) {
        handle.abort();
    }
    let Some(channel_id) = bot_voice_channel(ctx, guild_id).await else { return };
    if humans_in(ctx, guild_id, channel_id) > 0 {
        return;
    }
    let ctx = ctx.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(ALONE_TIMEOUT_SECS)).await;
        alone_timers().lock().await.remove(&guild_id);
        if let Some(channel_id) = bot_voice_channel(&ctx, guild_id).await {
            if humans_in(&ctx, guild_id, channel_id) > 0 {
                return;
            }
        }
        queues().lock().await.remove(&guild_id);
        if let Some(songbird) = songbird::get(&ctx).await.map(|s| s.clone()) {
            if let Some(call) = songbird.get(guild_id) {
                let call = call.lock().await;
                call.queue().stop();
            }
        }
        logln!(MOD, "[Alone] {guild_id} | stopped music, voice empty {ALONE_TIMEOUT_SECS}s");
    });
    alone_timers().lock().await.insert(guild_id, handle);
}

// ================== welcome / goodbye banners ==================

async fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
    let res = crate::util::http()
        .user_agent("Mozilla/5.0 (compatible; WelcomeBot/1.0)")
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.bytes().await.ok().map(|b| b.to_vec())
}

fn shrink_to(canvas: &crate::render::Canvas, face: crate::render::Face, text: &str, start: f32, min: f32, max_w: f32) -> f32 {
    let mut size = start;
    while canvas.fonts.measure(face, size, text) > max_w && size > min {
        size -= 2.0;
    }
    size
}

fn centered(canvas: &mut crate::render::Canvas, y: i32, size: f32, face: crate::render::Face, color: crate::render::Rgb, text: &str) {
    let width = canvas.fonts.measure(face, size, text);
    let x = (canvas.width() as f32 / 2.0 - width / 2.0).max(0.0) as i32;
    canvas.text(x, y, size, face, color, text);
}

/// 800x400 banner: cover-scaled background, darkened, circular avatar, three centred text rows.
/// Takes a `User` (not a `Member`): on leave the member is already gone from the guild, so a REST
/// lookup 404s and the goodbye banner was never sent.
async fn build_banner(user: &User, title: &str, text: &str, bg_bytes: &[u8], title_color: &str) -> Option<Vec<u8>> {
    // Ask for PNG: serenity's avatar_url() hands out WEBP/GIF, and the image crate here only has
    // the png + jpeg decoders, so a webp avatar silently produced an empty circle.
    let url = match &user.avatar {
        Some(hash) => format!("https://cdn.discordapp.com/avatars/{}/{hash}.png?size=256", user.id),
        None => user.default_avatar_url(),
    };
    let avatar = fetch_bytes(&url).await;
    if avatar.is_none() {
        logln!(MOD, "[Banner] avatar fetch failed for {} ({url})", user.name);
    }
    compose_banner(bg_bytes, avatar.as_deref(), &user.name, title, text, title_color)
}

/// Pure banner compositor (no network), so it can be exercised without a Discord gateway.
fn compose_banner(
    bg_bytes: &[u8],
    avatar_bytes: Option<&[u8]>,
    username: &str,
    title: &str,
    text: &str,
    title_color: &str,
) -> Option<Vec<u8>> {
    use crate::render::{hex, Face};
    const W: u32 = 800;
    const H: u32 = 400;
    const AV: u32 = 150;
    const AV_Y: u32 = 30;

    let bg = image::load_from_memory(bg_bytes).ok()?.to_rgba8();
    let mut canvas_img = image::RgbaImage::new(W, H);

    let scale = f64::max(W as f64 / bg.width() as f64, H as f64 / bg.height() as f64);
    let off_x = (W as f64 - bg.width() as f64 * scale) / 2.0;
    let off_y = (H as f64 - bg.height() as f64 * scale) / 2.0;
    for y in 0..H {
        for x in 0..W {
            let sx = ((x as f64 - off_x) / scale) as u32;
            let sy = ((y as f64 - off_y) / scale) as u32;
            if sx >= bg.width() || sy >= bg.height() {
                continue;
            }
            let p = bg.get_pixel(sx, sy);
            let dim = |c: u8| (c as f64 * 0.45) as u8;
            canvas_img.put_pixel(x, y, image::Rgba([dim(p[0]), dim(p[1]), dim(p[2]), 255]));
        }
    }

    // avatar, circle-clipped
    let av_x = (W - AV) / 2;
    if let Some(bytes) = avatar_bytes {
        match image::load_from_memory(bytes) {
            Ok(avatar) => {
                let avatar = image::imageops::resize(&avatar.to_rgba8(), AV, AV, image::imageops::FilterType::Lanczos3);
                let c = AV as f64 / 2.0;
                for y in 0..AV {
                    for x in 0..AV {
                        let (dx, dy) = (x as f64 - c + 0.5, y as f64 - c + 0.5);
                        if (dx * dx + dy * dy).sqrt() > c {
                            continue;
                        }
                        canvas_img.put_pixel(av_x + x, AV_Y + y, *avatar.get_pixel(x, y));
                    }
                }
            }
            Err(e) => logln!(MOD, "[Banner] avatar decode failed: {e}"),
        }
    }

    // white ring
    let (cx, cy, radius) = (W as f64 / 2.0, AV_Y as f64 + AV as f64 / 2.0, AV as f64 / 2.0);
    for step in 0..1440 {
        let theta = step as f64 * std::f64::consts::PI / 720.0;
        for w in 0..4 {
            let r = radius - w as f64;
            let (x, y) = (cx + r * theta.cos(), cy + r * theta.sin());
            if x >= 0.0 && y >= 0.0 && (x as u32) < W && (y as u32) < H {
                canvas_img.put_pixel(x as u32, y as u32, image::Rgba([255, 255, 255, 255]));
            }
        }
    }

    let fonts = crate::render::Fonts::load().ok()?;
    let mut canvas = crate::render::Canvas::from_image(canvas_img, fonts);
    let max_w = W as f32 - 60.0;
    let title_size = shrink_to(&canvas, Face::Bold, title, 48.0, 20.0, max_w);
    let user_size = shrink_to(&canvas, Face::Bold, username, 40.0, 16.0, max_w);
    let text_size = shrink_to(&canvas, Face::Bold, text, 30.0, 16.0, max_w);

    centered(&mut canvas, 250, title_size, Face::Bold, hex(title_color), title);
    centered(&mut canvas, 300, user_size, Face::Bold, hex("#ffffff"), username);
    centered(&mut canvas, 345, text_size, Face::Bold, hex("#facc15"), text);
    canvas.png().ok()
}

async fn handle_member_join(ctx: &Context, member: &Member) {
    let channel_id = crate::config::get("DISCORD_WELCOME_CHANNEL_ID").and_then(|v| v.parse::<u64>().ok());
    let (Some(channel_id), Some(text), Some(title)) = (
        channel_id,
        crate::config::get("DISCORD_WELCOME_TEXT"),
        crate::config::get("DISCORD_WELCOME_TITLE"),
    ) else {
        logln!(MOD, "[Welcome] DISCORD_WELCOME_CHANNEL_ID / _TEXT / _TITLE not set in .env");
        return;
    };
    let text = text.replace("{user}", &member.user.name);
    let Some(png) = build_banner(&member.user, &title, &text, WELCOME_BG, "#7dd3fc").await else {
        logln!(MOD, "[Welcome] banner render failed");
        return;
    };
    let count = ctx.cache.guild(member.guild_id).map(|g| g.member_count).unwrap_or(0);
    let _ = ChannelId::new(channel_id)
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .content(format!("You are member **#{count}** <@{}>", member.user.id))
                .add_file(CreateAttachment::bytes(png, "welcome.png")),
        )
        .await;
    logln!(MOD, "[Welcome] Sent for {}", member.user.name);
}

async fn handle_member_leave(ctx: &Context, _guild_id: GuildId, user: &User) {
    let channel_id = crate::config::get("DISCORD_GOODBYE_CHANNEL_ID").and_then(|v| v.parse::<u64>().ok());
    let (Some(channel_id), Some(text), Some(title)) = (
        channel_id,
        crate::config::get("DISCORD_GOODBYE_TEXT"),
        crate::config::get("DISCORD_GOODBYE_TITLE"),
    ) else {
        logln!(MOD, "[Goodbye] DISCORD_GOODBYE_CHANNEL_ID / _TEXT / _TITLE not set in .env");
        return;
    };
    let text = text.replace("{user}", &user.name);
    let Some(png) = build_banner(user, &title, &text, GOODBYE_BG, "#ef4444").await else {
        logln!(MOD, "[Goodbye] banner render failed");
        return;
    };
    let _ = ChannelId::new(channel_id)
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .content(format!("See you <@{}>", user.id))
                .add_file(CreateAttachment::bytes(png, "goodbye.png")),
        )
        .await;
    logln!(MOD, "[Goodbye] Sent for {}", user.name);
}

// ================== stats channels ==================

async fn update_stats(ctx: &Context) {
    for guild_id in ctx.cache.guilds() {
        if let Err(e) = update_stats_guild(ctx, guild_id).await {
            logln!(MOD, "[Stats] Error in {guild_id}: {e}");
        }
    }
}

async fn update_stats_guild(ctx: &Context, guild_id: GuildId) -> Result<()> {
    let (all, humans, bots) = match guild_id.members(&ctx.http, None, None).await {
        Ok(list) => {
            let bots = list.iter().filter(|m| m.user.bot).count();
            (list.len(), list.len() - bots, bots)
        }
        Err(_) => (ctx.cache.guild(guild_id).map(|g| g.member_count as usize).unwrap_or(0), 0, 0),
    };

    let channels = guild_id.channels(&ctx.http).await?;
    let category_id = match channels
        .values()
        .find(|c| c.kind == ChannelType::Category && c.name.to_lowercase().contains("stats"))
    {
        Some(category) => category.id,
        None => {
            guild_id
                .create_channel(&ctx.http, CreateChannel::new(STATS_CATEGORY_NAME).kind(ChannelType::Category))
                .await?
                .id
        }
    };

    for (key, label) in STATS_DEFS {
        let count = match *key {
            "all" => all,
            "humans" => humans,
            _ => bots,
        };
        if count == 0 && *key != "all" {
            continue;
        }
        let target = format!("{label}: {count}");
        let existing = channels.values().find(|c| {
            c.kind == ChannelType::Voice
                && c.parent_id == Some(category_id)
                && c.name.split(':').next().unwrap_or("").trim() == *label
        });
        match existing {
            Some(channel) if channel.name != target => {
                let _ = ctx.http.edit_channel(channel.id, &EditChannel::new().name(&target), None).await;
            }
            Some(_) => {}
            None => {
                let _ = guild_id
                    .create_channel(
                        &ctx.http,
                        CreateChannel::new(&target).kind(ChannelType::Voice).category(category_id),
                    )
                    .await;
            }
        }
    }
    Ok(())
}

// ================== helpers for commands ==================

fn embed(description: &str, colour: u32) -> CreateEmbed {
    CreateEmbed::new().colour(colour).description(description)
}

async fn reply_embed(ctx: &Context, msg: &Message, description: &str, colour: u32) {
    let _ = msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(embed(description, colour))).await;
}

async fn member_voice_channel(ctx: &Context, msg: &Message) -> Option<ChannelId> {
    let guild_id = msg.guild_id?;
    let guild = ctx.cache.guild(guild_id)?;
    let channel = guild.voice_states.get(&msg.author.id).and_then(|v| v.channel_id);
    if channel.is_none() {
        // voice states only reach the cache with the GUILDS + GUILD_VOICE_STATES intents
        logln!(
            MOD,
            "voice gate: no voice state for {} in guild {guild_id} ({} cached states)",
            msg.author.id,
            guild.voice_states.len()
        );
    }
    channel
}

async fn bot_voice_channel(ctx: &Context, guild_id: GuildId) -> Option<ChannelId> {
    let guild = ctx.cache.guild(guild_id)?;
    guild
        .voice_states
        .get(&ctx.cache.current_user().id)
        .and_then(|v| v.channel_id)
}

/// Reject when the caller is not in the bot's voice channel; returns true when allowed to proceed.
async fn require_same_voice(ctx: &Context, msg: &Message, guild_id: GuildId, action: &str) -> bool {
    let Some(channel) = member_voice_channel(ctx, msg).await else {
        reply_embed(ctx, msg, "You need to join a voice channel first!", 0xFF0000).await;
        return false;
    };
    if let Some(bot_channel) = bot_voice_channel(ctx, guild_id).await {
        if bot_channel != channel {
            reply_embed(ctx, msg, &format!("You must be in the same voice channel to {action}!"), 0xFF0000).await;
            return false;
        }
    }
    true
}

fn requester_tag(queue: Option<&GuildQueue>) -> String {
    queue
        .and_then(|q| q.songs.first().or(q.last_song.as_ref()))
        .and_then(|s| s.requested_by)
        .map(|id| format!("<@{id}>"))
        .unwrap_or_else(|| "the requester".into())
}

async fn stop_playback(ctx: &Context, guild_id: GuildId) {
    if let Some(songbird) = songbird::get(ctx).await.map(|s| s.clone()) {
        if let Some(call) = songbird.get(guild_id) {
            let call = call.lock().await;
            call.queue().stop();
        }
    }
}

// ================== commands ==================

const HELP_TEXT: &str = "**Music**\n\
`!p` - Play a track by link or search term\n\
`!q` - Display the current song queue\n\
`!s` - Skip the current track\n\
`!l` - Stop playback and clear queue\n\
`!d` - Disconnect from voice channel\n\
`!ap` - Toggle automatic song recommendations";

async fn cmd_help(ctx: &Context, msg: &Message) {
    let guild_name = msg.guild_id.and_then(|g| ctx.cache.guild(g)).map(|g| g.name.clone()).unwrap_or_default();
    let _ = msg
        .channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new().embed(
                CreateEmbed::new()
                    .colour(0x0099FF)
                    .title(format!("Available Commands - {guild_name}"))
                    .description(HELP_TEXT)
                    .footer(serenity::all::CreateEmbedFooter::new(
                        "Skips and stops are restricted to the track requester or admins.",
                    )),
            ),
        )
        .await;
}

async fn cmd_queue(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let guild_name = ctx.cache.guild(guild_id).map(|g| g.name.clone()).unwrap_or_default();
    let snapshot = queues().lock().await.get(&guild_id).cloned();
    let Some(queue) = snapshot.filter(|q| !q.songs.is_empty()) else {
        reply_embed(ctx, msg, "The queue is currently empty.", 0xFF0000).await;
        return;
    };

    let now = &queue.songs[0];
    let req_info = match (now.manual, now.requested_by) {
        (true, Some(id)) => format!(" | Requested by: <@{id}>"),
        _ => " | *(Autoplay)*".to_string(),
    };
    let artist_text = (now.artist != "Unknown Artist").then(|| format!("by {}", now.artist)).unwrap_or_default();
    let mut description = format!(
        "**Now Playing:**\n[{}]({}) {}{}\n\n**Up Next:**\n",
        now.title, now.url, artist_text, req_info
    );
    let upcoming = &queue.songs[1..];
    if upcoming.is_empty() {
        description += "No upcoming songs manually queued.";
    } else {
        for (i, song) in upcoming.iter().take(10).enumerate() {
            let who = song.requested_by.map(|id| format!("<@{id}>")).unwrap_or_else(|| "Autoplay".into());
            description += &format!("{}. [{}]({}) - {}\n", i + 1, song.title, song.url, who);
        }
        if upcoming.len() > 10 {
            description += &format!("\n...and {} more song(s).", upcoming.len() - 10);
        }
    }
    let _ = msg
        .channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new().embed(
                CreateEmbed::new()
                    .colour(0x0099FF)
                    .title(format!("Music Queue - {guild_name}"))
                    .description(description)
                    .footer(serenity::all::CreateEmbedFooter::new(format!(
                        "Autoplay: {} | Total Songs in Queue: {}",
                        if queue.autoplay { "Enabled" } else { "Disabled" },
                        queue.songs.len()
                    ))),
            ),
        )
        .await;
}

/// Turn a `!p` argument into candidate songs (playlist/Spotify/Apple/SoundCloud/link/search).
async fn resolve_query(query: &str, requester: Option<u64>) -> Vec<Song> {
    let is_playlist = query.contains("list=") || query.contains("/playlist");
    let direct_id = extract_video_id(query);

    let mut songs: Vec<Song> = if let Some((kind, id)) = extract_spotify(query) {
        match fetch_spotify_tracks(&kind, &id).await {
            Ok(tracks) => resolve_meta_to_songs(tracks, requester, &format!("Spotify {kind}")).await,
            Err(e) => {
                logln!(MOD, "[Spotify] {e}");
                Vec::new()
            }
        }
    } else if let Some((kind, id)) = extract_apple(query) {
        match fetch_apple_tracks(&kind, &id).await {
            Ok(tracks) => resolve_meta_to_songs(tracks, requester, &format!("AppleMusic {kind}")).await,
            Err(e) => {
                logln!(MOD, "[AppleMusic] {e}");
                Vec::new()
            }
        }
    } else if let Some(url) = extract_soundcloud(query) {
        match fetch_soundcloud_track(&url).await {
            Ok(tracks) => resolve_meta_to_songs(tracks, requester, "SoundCloud").await,
            Err(e) => {
                logln!(MOD, "[SoundCloud] {e}");
                Vec::new()
            }
        }
    } else if is_playlist && direct_id.is_none() {
        get_playlist_tracks(query)
            .await
            .into_iter()
            .map(|s| normalize_song(Song { requested_by: requester, manual: true, ..s }))
            .collect()
    } else if let Some(id) = direct_id {
        fetch_video_info(&id)
            .await
            .map(|mut info| {
                info.url = format!("https://www.youtube.com/watch?v={id}");
                info.requested_by = requester;
                info.manual = true;
                vec![normalize_song(info)]
            })
            .unwrap_or_default()
    } else {
        search_yt_music(query, 10)
            .await
            .into_iter()
            .next()
            .map(|hit| normalize_song(Song { requested_by: requester, manual: true, ..hit }))
            .into_iter()
            .collect()
    };
    songs.retain(is_valid_song);
    songs
}

async fn cmd_play(ctx: &Context, msg: &Message, query: &str) -> Result<()> {
    let Some(guild_id) = msg.guild_id else { return Ok(()) };
    if query.trim().is_empty() {
        reply_embed(ctx, msg, "Please provide a song title, playlist URL, or song URL!", 0xFF0000).await;
        return Ok(());
    }
    if !require_same_voice(ctx, msg, guild_id, "play music").await {
        return Ok(());
    }
    let voice_channel = member_voice_channel(ctx, msg).await.expect("checked above");

    let songs = resolve_query(query, Some(msg.author.id.get())).await;
    logln!(MOD, "[Resolve] {query:?} -> {} track(s)", songs.len());
    if songs.is_empty() {
        reply_embed(ctx, msg, "No valid tracks were found.", 0xFF0000).await;
        return Ok(());
    }

    let joined = tokio::time::timeout(Duration::from_secs(20), ensure_voice(ctx, guild_id, voice_channel)).await;
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            logln!(MOD, "[Voice] join failed: {e:#}");
            reply_embed(ctx, msg, &format!("Could not join your voice channel: {e}"), 0xFF0000).await;
            return Ok(());
        }
        Err(_) => {
            logln!(MOD, "[Voice] join timed out after 20s for {voice_channel}");
            reply_embed(ctx, msg, "Joining your voice channel timed out.", 0xFF0000).await;
            return Ok(());
        }
    }

    let is_playlist = query.contains("list=") || query.contains("/playlist");
    let collection = if query.contains("open.spotify.com/") {
        Some("Spotify")
    } else if query.contains("music.apple.com/") {
        Some("Apple Music")
    } else if query.contains("soundcloud.com/") {
        Some("SoundCloud")
    } else if is_playlist && extract_video_id(query).is_none() {
        Some("playlist")
    } else {
        None
    };

    let (was_playing, added) = {
        let mut all = queues().lock().await;
        let queue = all.entry(guild_id).or_default();
        queue.text_channel = Some(msg.channel_id);
        let existing: HashSet<String> = queue.songs.iter().filter_map(song_id).collect();
        let fresh: Vec<Song> = songs
            .into_iter()
            .filter(|s| song_id(s).map(|id| !existing.contains(&id)).unwrap_or(false))
            .collect();
        let was_playing = queue.playing;
        queue.songs.extend(fresh.iter().cloned());
        (was_playing, fresh)
    };
    logln!(MOD, "[Queue Added] {guild_id} | Added {} tracks", added.len());

    match collection {
        Some(label) if was_playing => {
            reply_embed(ctx, msg, &format!("Added **{}** tracks from {label} to queue!", added.len()), 0x00FF00).await;
        }
        Some(label) => {
            reply_embed(ctx, msg, &format!("Added **{}** tracks from {label} to queue!", added.len()), 0x00FF00).await;
        }
        None if was_playing => {
            if let Some(first) = added.first() {
                let artist_text = (first.artist != "Unknown Artist").then(|| format!(" by {}", first.artist)).unwrap_or_default();
                let mut card = CreateEmbed::new()
                    .colour(0x00FF00)
                    .description(format!("Added to queue: [{}]({}){}", first.title, first.url, artist_text));
                if let Some(id) = song_id(first) {
                    card = card.thumbnail(format!("https://img.youtube.com/vi/{id}/mqdefault.jpg"));
                }
                let _ = msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(card)).await;
            }
        }
        None => {}
    }

    if !was_playing {
        play_next(ctx, guild_id).await;
    }
    Ok(())
}

async fn cmd_skip(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let snapshot = queues().lock().await.get(&guild_id).cloned();
    let Some(queue) = snapshot.filter(|q| !q.songs.is_empty()) else {
        reply_embed(ctx, msg, "No song is currently playing.", 0xFF0000).await;
        return;
    };
    if !require_same_voice(ctx, msg, guild_id, "skip").await {
        return;
    }
    if !can_control_queue(msg, Some(&queue)) {
        let tag = requester_tag(Some(&queue));
        reply_embed(ctx, msg, &format!("Only {tag} (or an Admin) can skip this track."), 0xFF0000).await;
        return;
    }
    let current = queue.songs[0].clone();
    let artist_text = (current.artist != "Unknown Artist").then(|| format!(" by {}", current.artist)).unwrap_or_default();
    reply_embed(ctx, msg, &format!("**{}**{} was skipped by {}", current.title, artist_text, msg.author.name), 0xFFFF00).await;
    // stopping the track fires TrackEnd, which advances the queue
    stop_playback(ctx, guild_id).await;
}

async fn cmd_stop(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let snapshot = queues().lock().await.get(&guild_id).cloned();
    let playing = snapshot.as_ref().map(|q| q.playing || !q.songs.is_empty()).unwrap_or(false);
    if !playing {
        reply_embed(ctx, msg, "Nothing is currently playing.", 0xFF0000).await;
        return;
    }
    if !require_same_voice(ctx, msg, guild_id, "stop").await {
        return;
    }
    if !can_control_queue(msg, snapshot.as_ref()) {
        let tag = requester_tag(snapshot.as_ref());
        reply_embed(ctx, msg, &format!("Only {tag} (or an Admin) can stop the player."), 0xFF0000).await;
        return;
    }
    logln!(MOD, "[Stop Command] {guild_id} | Stopped by {}", msg.author.name);
    // drop the queue first so the TrackEnd handler has nothing to advance to
    destroy_queue(guild_id).await;
    stop_playback(ctx, guild_id).await;
    reply_embed(ctx, msg, "Music stopped & queue cleared.", 0xFFFF00).await;
}

async fn cmd_disconnect(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let connected = songbird::get(ctx).await.map(|s| s.get(guild_id).is_some()).unwrap_or(false);
    if !connected && queues().lock().await.get(&guild_id).is_none() {
        reply_embed(ctx, msg, "Not connected to any voice channel.", 0xFF0000).await;
        return;
    }
    if !require_same_voice(ctx, msg, guild_id, "disconnect").await {
        return;
    }
    let snapshot = queues().lock().await.get(&guild_id).cloned();
    if !can_control_queue(msg, snapshot.as_ref()) {
        let tag = requester_tag(snapshot.as_ref());
        reply_embed(ctx, msg, &format!("Only {tag} (or an Admin) can disconnect the player."), 0xFF0000).await;
        return;
    }
    logln!(MOD, "[Disconnect Command] {guild_id} | Disconnected by {}", msg.author.name);
    destroy_queue(guild_id).await;
    if let Some(songbird) = songbird::get(ctx).await.map(|s| s.clone()) {
        let _ = songbird.remove(guild_id).await;
    }
    reply_embed(ctx, msg, "Music stopped and disconnected from voice channel.", 0xFF0000).await;
}

async fn cmd_autoplay(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let snapshot = queues().lock().await.get(&guild_id).cloned();
    let Some(queue) = snapshot else {
        reply_embed(ctx, msg, "Play a song first to toggle autoplay!", 0xFF0000).await;
        return;
    };
    if !require_same_voice(ctx, msg, guild_id, "toggle autoplay").await {
        return;
    }
    if !can_control_queue(msg, Some(&queue)) {
        reply_embed(ctx, msg, "Only the person who played this song can toggle autoplay.", 0xFF0000).await;
        return;
    }
    let enabled = {
        let mut all = queues().lock().await;
        let queue = all.entry(guild_id).or_default();
        queue.autoplay = !queue.autoplay;
        queue.autoplay
    };
    logln!(MOD, "[Autoplay Toggle] {guild_id} | {enabled}");
    reply_embed(
        ctx,
        msg,
        &format!("Autoplay has been **{}**", if enabled { "enabled" } else { "disabled" }),
        if enabled { 0x00FF00 } else { 0xFF0000 },
    )
    .await;
}

// ================== gateway handler ==================

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        logln!(MOD, "Logged in as {}", ready.user.name);
        tokio::spawn(async move {
            loop {
                update_stats(&ctx).await;
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        });
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot || msg.guild_id.is_none() {
            return;
        }
        let content = msg.content.trim();
        if !content.starts_with('!') {
            return;
        }
        let _ = msg.channel_id.broadcast_typing(&ctx.http).await;
        let mut parts = content[1..].split_whitespace();
        let command = parts.next().unwrap_or("").to_lowercase();
        let query = parts.collect::<Vec<_>>().join(" ");

        if let Some(guild) = msg.guild_id {
            logln!(MOD, "[Command] Guild {guild} | From {} in #{}: {content}", msg.author.name, msg.channel_id);
        }

        match command.as_str() {
            "h" => cmd_help(&ctx, &msg).await,
            "q" => cmd_queue(&ctx, &msg).await,
            "p" => {
                if let Err(e) = cmd_play(&ctx, &msg, &query).await {
                    logln!(MOD, "[Play] {e}");
                    reply_embed(&ctx, &msg, "An error occurred while searching or loading the track.", 0xFF0000).await;
                }
            }
            "s" => cmd_skip(&ctx, &msg).await,
            "l" => cmd_stop(&ctx, &msg).await,
            "d" => cmd_disconnect(&ctx, &msg).await,
            "ap" => cmd_autoplay(&ctx, &msg).await,
            _ => {}
        }
        if let Some(guild_id) = msg.guild_id {
            refresh_alone_timer(&ctx, guild_id).await;
        }
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        handle_member_join(&ctx, &member).await;
    }

    async fn guild_member_removal(&self, ctx: Context, guild_id: GuildId, user: User, _member: Option<Member>) {
        handle_member_leave(&ctx, guild_id, &user).await;
    }
}

pub async fn run() -> Result<()> {
    let token = crate::config::req("DISCORD_BOT_TOKEN")?;
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MEMBERS;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler)
        .register_songbird()
        .await?;
    logln!(MOD, "Starting Discord client");
    client.start().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_video_ids_and_normalizes() {
        assert_eq!(extract_video_id("https://youtu.be/dQw4w9WgXcQ"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(extract_video_id("dQw4w9WgXcQ"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(extract_video_id("https://example.com/x"), None);
        assert_eq!(
            normalize_url("https://music.youtube.com/watch?v=dQw4w9WgXcQ"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    #[test]
    fn cleans_titles_and_artists() {
        assert_eq!(clean_artist("Someone - Topic"), "Someone");
        assert_eq!(clean_artist("Official VEVO: Band"), "Band");
        assert_eq!(clean_title("Band - Song (Official Video)", "Band"), "Song");
        assert_eq!(clean_title("Song [Lyrics]", "Unknown Artist"), "Song");
    }

    #[test]
    fn song_keys_ignore_noise() {
        let song = normalize_song(Song {
            title: "Song (Official Audio)".into(),
            artist: "Band - Topic".into(),
            url: "https://youtu.be/dQw4w9WgXcQ".into(),
            ..Default::default()
        });
        assert_eq!(song.title, "Song");
        assert_eq!(song.artist, "Band");
        assert_eq!(song_key(&song), "band:song");
        assert_eq!(song_url_key(&song), "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert!(is_valid_song(&song));
    }

    #[test]
    fn extracts_platform_links() {
        assert_eq!(
            extract_spotify("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"),
            Some(("playlist".into(), "37i9dQZF1DXcBWIGoYBM5M".into()))
        );
        assert_eq!(
            extract_apple("https://music.apple.com/id/album/example-name/1440857781"),
            Some(("album".into(), "1440857781".into()))
        );
        assert_eq!(
            extract_soundcloud("play https://soundcloud.com/artist/track-name now"),
            Some("https://soundcloud.com/artist/track-name".into())
        );
        assert!(extract_spotify("https://example.com").is_none());
    }

    #[test]
    fn bot_check_detection_and_fallbacks() {
        assert!(is_bot_check("Sign in to confirm you're not a bot"));
        assert!(is_bot_check("HTTP Error 429"));
        assert!(!is_bot_check("all good"));
        let used = vec!["youtube:player_client=web_safari".to_string()];
        assert_eq!(
            next_bot_check_args(&used),
            Some(vec!["--extractor-args".into(), "youtube:player_client=mweb".into()])
        );
        let all_used = vec![
            "youtube:player_client=web_safari".to_string(),
            "youtube:player_client=mweb".to_string(),
            "youtube:player_client=tv".to_string(),
        ];
        assert_eq!(next_bot_check_args(&all_used), None);
        assert!(!has_client_arg(&["--no-warnings".to_string()]));
    }

    #[test]
    fn composes_welcome_and_goodbye_banners() {
        // 256x256 two-tone avatar: circle clip must eat the corners
        let mut av = image::RgbaImage::new(256, 256);
        for (x, _, p) in av.enumerate_pixels_mut() {
            *p = if x < 128 {
                image::Rgba([240, 160, 40, 255])
            } else {
                image::Rgba([30, 30, 30, 255])
            };
        }
        let mut av_png = std::io::Cursor::new(Vec::new());
        av.write_to(&mut av_png, image::ImageFormat::Png).unwrap();

        // real bundled backgrounds (the same JPEGs the JS used in banner/)
        let cases = [
            ("welcome", WELCOME_BG, "norikimika", "WELCOME TO NORIKIMIKA", "You are member #42", "#7dd3fc"),
            ("goodbye", GOODBYE_BG, "norikimika", "GOODBYE", "See you again", "#ef4444"),
            (
                "long-name",
                WELCOME_BG,
                "a_really_long_discord_username_that_has_to_shrink_0123456789",
                "WELCOME TO A SERVER WITH A VERY LONG NAME INDEED",
                "You are member #1337",
                "#7dd3fc",
            ),
        ];
        for (label, bg, user, title, text, colour) in cases {
            let png = compose_banner(bg, Some(av_png.get_ref()), user, title, text, colour)
                .expect("compose banner");
            assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "{label}: not a PNG");
            let img = image::load_from_memory(&png).expect("decode");
            assert_eq!((img.width(), img.height()), (800, 400), "{label}: wrong size");
            // background must actually be there: darkened, i.e. not a flat black/white canvas
            let px = img.to_rgba8();
            let distinct = px.pixels().take(4000).map(|p| (p[0], p[1], p[2])).collect::<std::collections::HashSet<_>>();
            assert!(distinct.len() > 50, "{label}: background looks flat ({} colours)", distinct.len());
            let path = std::env::temp_dir().join(format!("banner-{label}.png"));
            std::fs::write(&path, &png).expect("write banner");
            println!("{label} -> {} ({} bytes)", path.display(), png.len());
        }
    }

    /// The exact stream songbird will fetch, decoded with the codecs we ship.
    /// Ignored by default (needs network + yt-dlp): `cargo test --release -- --ignored music`.
    #[tokio::test]
    #[ignore]
    async fn chosen_stream_decodes_to_pcm() {
        use symphonia::core::codecs::audio::AudioDecoderOptions;
        use symphonia::core::codecs::CodecParameters;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::{FormatOptions, TrackType};
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;

        let url = "https://www.youtube.com/watch?v=K-a8s8OLBSE";
        let out = tokio::process::Command::new("yt-dlp")
            .args(["-S", "ext:m4a,acodec:aac", "-f", "ba[abr>0][vcodec=none]/best", "-g", url])
            .output()
            .await
            .expect("run yt-dlp");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let direct = String::from_utf8_lossy(&out.stdout).lines().next().unwrap().trim().to_string();
        println!("picked: {}", &direct[..direct.find('?').unwrap_or(40).min(direct.len())]);

        let bytes = crate::util::client().get(&direct).send().await.unwrap().bytes().await.unwrap();
        println!("downloaded {} bytes", bytes.len());

        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("m4a");
        let probed = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .expect("probe should know this container");
        let mut format = probed;
        let track = format.default_track(TrackType::Audio).expect("an audio track").clone();
        let CodecParameters::Audio(params) = track.codec_params.as_ref().expect("codec params") else {
            panic!("default track is not audio")
        };
        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .expect("a decoder for this codec");
        let mut frames = 0u64;
        while let Ok(Some(packet)) = format.next_packet() {
            if packet.track_id != track.id {
                continue;
            }
            frames += decoder.decode(&packet).expect("decode packet").frames() as u64;
            if frames > 88_200 {
                break;
            }
        }
        println!("decoded {frames} frames");
        assert!(frames > 44_100, "only {frames} frames decoded");
    }

    /// The production path: yt-dlp resolve, then the same GET songbird will send.
    #[tokio::test]
    #[ignore]
    async fn resolve_stream_gets_a_playable_url() {
        let s = resolve_stream("https://www.youtube.com/watch?v=K-a8s8OLBSE")
            .await
            .expect("resolve a stream");
        println!("url {} len {:?}", &s.url[..70], s.len);
        assert!(s.len.unwrap_or(0) > 100_000, "suspiciously small stream");
    }
}

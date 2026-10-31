//! YouTube Music through the InnerTube API.
//!
//! Scraping `music.youtube.com/search` with yt-dlp stops working the moment the request goes
//! through a proxy: the page comes back with channel links and no titles, so search reports
//! "0 tracks" and everything falls back to plain YouTube — where the same query returns slowed,
//! reverb and tiktok edits of one song.
//!
//! InnerTube is the same catalogue behind that page, and it answers with JSON: title, artist,
//! duration and video id. The radio mix autoplay needs comes from there; the search box answer is
//! a tabbed page whose song rows are behind a second request, so search stays on `ytsearch` and
//! ranks what it gets (`discord::rank_matches`).

use serde_json::{json, Value};
use std::time::Duration;

/// The public web key music.youtube.com ships to every browser; not a credential.
const KEY: &str = "AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
const CLIENT_VERSION: &str = "1.20240101.01.00";

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration: f64,
}

fn context() -> Value {
    json!({
        "client": {
            "clientName": "WEB_REMIX",
            "clientVersion": CLIENT_VERSION,
            "hl": "en",
            "gl": "US",
        }
    })
}

async fn post(endpoint: &str, body: Value) -> Result<Value, String> {
    let url = format!("https://music.youtube.com/youtubei/v1/{endpoint}?key={KEY}&prettyPrint=false");
    // sb_client carries YTDLP_PROXY: the music endpoints are rate limited per IP exactly like the
    // media CDN, so they have to leave through the same door
    let response = crate::util::sb_client()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.youtube.com")
        .header("User-Agent", "Mozilla/5.0")
        .body(body.to_string())
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("{e:#}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| format!("{e:#}"))?;
    if !status.is_success() {
        return Err(format!(
            "InnerTube {endpoint} answered {status}: {}",
            crate::util::truncate(&text, 160)
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("InnerTube {endpoint} gave no JSON: {e}"))
}

/// Every occurrence of `key` anywhere in the tree; InnerTube nests its shelves by section.
fn collect<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    out.push(v);
                }
                collect(v, key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, key, out);
            }
        }
        _ => {}
    }
}

fn runs_text(node: Option<&Value>) -> String {
    node.and_then(|n| n.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|r| r.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// "4:09" or "1:02:33" as seconds; anything else is 0.
fn parse_duration(text: &str) -> f64 {
    let mut total = 0.0;
    let mut parts = 0;
    for piece in text.trim().split(':') {
        let Ok(value) = piece.trim().parse::<f64>() else {
            return 0.0;
        };
        total = total * 60.0 + value;
        parts += 1;
    }
    if parts == 0 { 0.0 } else { total }
}

/// One row of a radio/queue answer.
fn radio_track(item: &Value) -> Option<Track> {
    let id = item.get("videoId").and_then(Value::as_str)?;
    let title = runs_text(item.get("title"));
    if title.is_empty() {
        return None;
    }
    let byline = runs_text(item.get("longBylineText"));
    let artist = byline
        .split('•')
        .next()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let duration = item
        .pointer("/lengthText/simpleText")
        .and_then(Value::as_str)
        .map(parse_duration)
        .unwrap_or(0.0);
    Some(Track {
        id: id.to_string(),
        title,
        artist,
        duration,
    })
}

fn tracks_from(value: &Value, key: &str, parse: fn(&Value) -> Option<Track>, limit: usize) -> Vec<Track> {
    let mut items = Vec::new();
    collect(value, key, &mut items);
    let mut tracks: Vec<Track> = Vec::new();
    for item in items {
        if let Some(track) = parse(item) {
            if tracks.iter().any(|t| t.id == track.id) {
                continue;
            }
            tracks.push(track);
            if tracks.len() >= limit {
                break;
            }
        }
    }
    tracks
}

/// The "start radio" mix for a track: what autoplay should play next.
pub async fn radio(video_id: &str, limit: usize) -> Result<Vec<Track>, String> {
    if video_id.is_empty() {
        return Ok(Vec::new());
    }
    let body = json!({
        "context": context(),
        "videoId": video_id,
        "playlistId": format!("RDAMVM{video_id}"),
        "isAudioOnly": true,
    });
    let answer = post("next", body).await?;
    Ok(tracks_from(&answer, "playlistPanelVideoRenderer", radio_track, limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_radio_row_keeps_its_artist_and_length() {
        let row = json!({
            "videoId": "xyz",
            "title": {"runs": [{"text": "willow"}]},
            "longBylineText": {"runs": [{"text": "Taylor Swift • evermore • 2020"}]},
            "lengthText": {"simpleText": "3:34"}
        });
        let track = radio_track(&row).expect("radio rows always carry a video id");
        assert_eq!(track.artist, "Taylor Swift");
        assert_eq!(track.duration, 214.0);
    }

    #[test]
    fn durations_are_read_the_way_youtube_writes_them() {
        assert_eq!(parse_duration("4:09"), 249.0);
        assert_eq!(parse_duration("1:02:33"), 3753.0);
        assert_eq!(parse_duration("live"), 0.0);
    }
}

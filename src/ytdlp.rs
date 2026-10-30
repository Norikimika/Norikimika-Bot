//! Every yt-dlp call goes through here.
//!
//! One place for the client fallback, the proxy, the retry flags and the error classification.
//! `bot.rs` used to call yt-dlp on its own with a single hardcoded client and then log
//! "[FETCH ERROR] yt-dlp -j failed" — no reason, no retry, nothing to act on. A bot wall on that
//! one client meant every /music failed until the next yt-dlp release.

use anyhow::{anyhow, Result};
use std::time::Duration;

/// Why yt-dlp failed, read from what it printed.
///
/// Only a genuine refusal is worth another client; a format problem or a dead CDN is not.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Failure {
    /// YouTube wants a sign-in / cookie for this client. Another client can still work.
    BotWall,
    /// Our format selector asked for something the video does not have: not a client problem.
    Format,
    /// The CDN refused or dropped the media transfer.
    Cdn,
    Other,
}

pub fn classify(message: &str) -> Failure {
    let lower = message.to_lowercase();
    const BOT_WALL: &[&str] = &[
        "confirm you're not a bot",
        "confirm youre not a bot",
        "sign in to confirm",
        "page needs to be reloaded",
        "precondition check failed",
    ];
    const FORMAT: &[&str] = &[
        "requested format is not available",
        "no video formats found",
        "requested format not available",
    ];
    const CDN: &[&str] = &[
        "unable to download video data",
        "http error 403",
        "http error 429",
        "read timed out",
        "giving up after",
        "connection reset",
    ];
    let hit = |needles: &[&str]| needles.iter().any(|n| lower.contains(*n));
    if hit(BOT_WALL) {
        Failure::BotWall
    } else if hit(FORMAT) {
        Failure::Format
    } else if hit(CDN) {
        Failure::Cdn
    } else {
        Failure::Other
    }
}

/// Audio format chain: `ba` alone fails on videos that only expose a muxed stream, and a chain
/// always has a next option, so "Requested format is not available" is never our own doing.
pub const AUDIO_FORMAT_CHAIN: &str = "ba[abr>0][vcodec=none]/ba/b";

/// Clients tried in order. android_vr needs no PO token and still serves m4a/AAC.
pub const CLIENTS: &[&str] = &["android_vr", "web_safari", "mweb", "tv"];

fn client_args(client: &str) -> Vec<String> {
    vec![
        "--extractor-args".to_string(),
        format!("youtube:player_client={client}"),
    ]
}

fn has_client_arg(args: &[String]) -> bool {
    args.iter().any(|a| a.contains("player_client"))
}

fn base_args() -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    if let Some(proxy) = crate::config::get("YTDLP_PROXY").filter(|p| !p.is_empty()) {
        all.push("--proxy".to_string());
        all.push(proxy);
    }
    // transient CDN hiccups are worth a retry inside yt-dlp; that is a fix, not a dodge
    all.extend([
        "--retries".to_string(),
        "3".to_string(),
        "--fragment-retries".to_string(),
        "3".to_string(),
        "--socket-timeout".to_string(),
        "15".to_string(),
    ]);
    all
}

async fn run_once(
    args: &[String],
    client: Option<&str>,
    target: &str,
    timeout: Duration,
) -> Result<String> {
    let mut all = base_args();
    all.extend(args.iter().cloned());
    if let Some(client) = client {
        all.extend(client_args(client));
    }
    all.push(target.to_string());

    let child = tokio::process::Command::new("yt-dlp")
        .args(&all)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("yt-dlp could not start: {e}"))?;
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

/// Run yt-dlp, moving to the next client only when YouTube refused this one.
pub async fn run(
    args: Vec<String>,
    target: String,
    timeout: Duration,
    module: &str,
) -> Result<String> {
    // The caller pinned a client itself, so there is nothing to fall back to.
    if has_client_arg(&args) {
        return run_once(&args, None, &target, timeout).await;
    }
    let mut refused: Vec<&'static str> = Vec::new();
    let mut last = String::new();
    for client in CLIENTS {
        match run_once(&args, Some(client), &target, timeout).await {
            Ok(out) => return Ok(out),
            Err(e) => {
                let message = format!("{e:#}");
                let reason = classify(&message);
                logln!(
                    module,
                    "[yt-dlp] {client} failed ({reason:?}): {}",
                    crate::util::truncate(message.trim(), 300)
                );
                match reason {
                    Failure::BotWall => {
                        refused.push(client);
                        last = message;
                    }
                    Failure::Format => {
                        return Err(anyhow!(
                            "format selection failed, another client cannot help: {message}"
                        ))
                    }
                    Failure::Cdn | Failure::Other => return Err(e),
                }
            }
        }
    }
    Err(anyhow!(
        "every YouTube client refused the request ({refused:?}): {last}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_walls_are_recognised() {
        assert_eq!(
            classify("ERROR: Sign in to confirm you're not a bot"),
            Failure::BotWall
        );
        assert_eq!(classify("Precondition check failed."), Failure::BotWall);
    }

    #[test]
    fn format_errors_are_not_blamed_on_the_client() {
        assert_eq!(
            classify("ERROR: Requested format is not available"),
            Failure::Format
        );
        assert_eq!(classify("ERROR: HTTP Error 403: Forbidden"), Failure::Cdn);
        assert_eq!(classify("ERROR: something else entirely"), Failure::Other);
    }

    #[test]
    fn a_pinned_client_is_left_alone() {
        let args = vec![
            "--extractor-args".to_string(),
            "youtube:player_client=tv".to_string(),
        ];
        assert!(has_client_arg(&args));
        assert!(!has_client_arg(&["-j".to_string()]));
    }
}

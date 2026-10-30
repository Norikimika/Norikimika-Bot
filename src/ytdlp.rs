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
        // A client that needs a PO token we do not have skips its formats and then dies with a
        // 403 from the CDN. That 403 is not the CDN's fault, so it must not stop the fallback:
        // another client serves the same video fine.
        "po token",
        "po_token",
        "gvs po token",
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

/// Audio format chain: 140 is AAC 129k/44.1kHz in m4a, 251 is Opus 143k/48kHz in webm.
///
/// 140 comes first because Telegram stores the cover but only renders it for m4a/mp3 — the
/// response to sendAudio says "kept the cover" for a webm track and the client still shows nothing.
/// 140 is YouTube's own AAC stream, downloaded as-is: this is a different source format, not a
/// re-encode, so nothing is lost compared to the video's audio.
///
/// It stops at "any audio" instead of falling through to `b`: a client that only offers images
/// would otherwise hand us a jpeg and call it a download.
pub const AUDIO_FORMAT_CHAIN: &str = "140/251/bestaudio/ba";

/// Fallback clients, tried only after yt-dlp's own default set has failed.
///
/// Pinning one client used to be the only way to get around a bot wall, but YouTube now hands out
/// a different wall per client: `tv` answers "The page needs to be reloaded", `mweb` and
/// `android_vr` demand a PO token, `web_safari` offers images only. yt-dlp's default set is kept
/// current with all of that and works on videos every single pinned client refuses, so it goes
/// first and this list is only the safety net.
pub const CLIENTS: &[&str] = &["tv", "web_safari", "mweb", "android_vr"];

/// yt-dlp needs an external JavaScript runtime to solve YouTube's challenge; without one it warns
/// "n challenge solving failed" and most formats go missing. Deno is enabled by default, node and
/// bun are not, so whatever is installed gets passed explicitly.
const JS_RUNTIMES: &[&str] = &["deno", "node", "bun"];

fn on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

fn js_runtime() -> Option<&'static str> {
    static FOUND: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| JS_RUNTIMES.iter().copied().find(|rt| on_path(rt)))
}

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
    // A cookies.txt from a signed-in browser is the only thing that clears a bot wall for good;
    // every client falls back to it before giving up.
    if let Some(jar) = crate::config::get("YTDLP_COOKIES").filter(|p| !p.is_empty()) {
        all.push("--cookies".to_string());
        all.push(jar);
    }
    if let Some(rt) = js_runtime() {
        all.push("--js-runtimes".to_string());
        all.push(rt.to_string());
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

/// Run yt-dlp, moving to the next client on every failure the JS bot also retried.
///
/// That is all of them but an unrecognised error: a bot wall, a missing PO token, a format the
/// client does not serve and a CDN refusal are all client-specific in practice — the original
/// `isBotCheck` matched the same set. Only an error we cannot read stops the walk, because then
/// nothing says another client would do better.
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
    if js_runtime().is_none() {
        logln!(
            module,
            "[yt-dlp] no JavaScript runtime found ({}), YouTube challenges will fail: \
             install node (pkg install nodejs) or deno",
            JS_RUNTIMES.join("/")
        );
    }

    // First try yt-dlp's own client set: it tracks what YouTube currently serves.
    let mut tried: Vec<String> = Vec::new();
    match run_once(&args, None, &target, timeout).await {
        Ok(out) => return Ok(out),
        Err(e) => {
            let message = format!("{e:#}");
            let reason = classify(&message);
            logln!(
                module,
                "[yt-dlp] default clients failed ({reason:?}): {}",
                crate::util::truncate(message.trim(), 300)
            );
            if reason == Failure::Other {
                return Err(e);
            }
            tried.push(format!(
                "default {reason:?}: {}",
                crate::util::truncate(message.trim(), 200)
            ));
        }
    }

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
                if reason == Failure::Other {
                    return Err(e);
                }
                tried.push(format!(
                    "{client} {reason:?}: {}",
                    crate::util::truncate(message.trim(), 200)
                ));
            }
        }
    }
    Err(anyhow!("every attempt failed:\n{}", tried.join("\n")))
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
    fn a_missing_po_token_is_a_client_problem_not_a_cdn_one() {
        // real message: android_vr skipped every format, then the CDN answered 403
        let message = "WARNING: [youtube] abc: android_vr client https formats require a GVS PO Token \
which was not provided.\nERROR: unable to download video data: HTTP Error 403: Forbidden";
        assert_eq!(classify(message), Failure::BotWall, "another client must be tried");
        // a 403 with no PO token in it is still a CDN problem: retrying clients will not help
        assert_eq!(
            classify("ERROR: unable to download video data: HTTP Error 403: Forbidden"),
            Failure::Cdn
        );
    }

    #[test]
    fn a_runtime_that_is_not_installed_is_not_reported() {
        assert!(!on_path("norikimika-no-such-runtime"));
        assert!(JS_RUNTIMES.contains(&"node"));
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
    fn every_client_failure_the_js_bot_retried_is_retried_here() {
        // the JS bot's isBotCheck matched these, so a format or CDN error must not stop the walk
        for message in [
            "ERROR: Requested format is not available",
            "ERROR: No video formats found",
            "ERROR: unable to download video data: HTTP Error 403: Forbidden",
            "ERROR: HTTP Error 429: Too Many Requests",
        ] {
            assert_ne!(classify(message), Failure::Other, "{message} must be retried");
        }
        // nothing to act on: another client will not help
        assert_eq!(classify("ERROR: Unsupported URL: foo"), Failure::Other);
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

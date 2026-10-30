/// HTTP client builder with bundled webpki roots.
///
/// reqwest 0.13's default rustls verifier is `rustls-platform-verifier`, which panics on
/// Android/Termux ("Expect rustls-platform-verifier to be initialized" — it wants a
/// JNI-initialised app context). Building the config ourselves keeps the Android build working.
pub fn http() -> reqwest::ClientBuilder {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // pick the provider explicitly: with several rustls versions in the graph the crate-feature
    // auto-detection panics ("Could not automatically determine the process-level CryptoProvider")
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder().use_preconfigured_tls(tls)
}

/// Same as [`http`], already built with default settings.
pub fn client() -> reqwest::Client {
    http().build().expect("build http client")
}

/// Locate a privileged shell.
///
/// `su` is only on PATH when the bot is started from an interactive Termux shell; as a service it
/// inherited a PATH without `$PREFIX/bin`, and every `su -c` failed with a bare "No such file or
/// directory (os error 2)" that named nothing.
pub fn su() -> String {
    use std::path::PathBuf;
    use std::sync::OnceLock;
    static SU: OnceLock<String> = OnceLock::new();
    SU.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(prefix) = std::env::var_os("PREFIX") {
            candidates.push(PathBuf::from(prefix).join("bin/su"));
        }
        candidates.extend(
            [
                "/data/data/com.termux/files/usr/bin/su",
                "/system/bin/su",
                "/system/xbin/su",
                "/system/sbin/su",
                "/sbin/su",
                "/debug_ramdisk/su",
                "/su/bin/su",
            ]
            .map(PathBuf::from),
        );
        candidates
            .into_iter()
            .find(|path| path.exists())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "su".to_string())
    })
    .clone()
}

/// reqwest 0.12 client for songbird's types (`HttpRequest`, `YoutubeDl` live on 0.12).
pub fn sb_client() -> reqwest12::Client {
    reqwest12::Client::new()
}

/// Run a command and capture its stdout. On failure the error says *why* (spawn error, or exit
/// status plus stderr) so log lines are actionable instead of a bare "failed".
pub async fn capture(cmd: &str, args: &[&str]) -> std::result::Result<String, String> {
    match tokio::process::Command::new(cmd).args(args).output().await {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            // `ExitStatus`'s Display is "exit status: 3"; keep it to the code
            let code = match out.status.code() {
                Some(c) => c.to_string(),
                None => out.status.to_string(),
            };
            Err(if err.is_empty() {
                format!("exit {code}")
            } else {
                format!("exit {code}: {err}")
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Log a failure once per key.
///
/// Pollers (repo every few seconds, the connection watcher) would otherwise either flood the log
/// with the same error or, as before, drop it entirely; a new problem is still reported.
pub fn log_once(module: &str, key: &str, message: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let first = match seen.lock() {
        Ok(mut seen) => seen.insert(format!("{module}:{key}")),
        Err(_) => true,
    };
    if first {
        crate::logx::log_to(module, message);
    }
}

/// Install the process-wide rustls crypto provider.
///
/// Both `ring` (via serenity/songbird's reqwest 0.12) and `aws-lc-rs` (via reqwest 0.13) end up in
/// the dependency graph, so rustls cannot pick one itself: anything that builds a default TLS
/// config — e.g. songbird's `YoutubeDl` with `reqwest12::Client::new()` — panics with
/// "Could not automatically determine the process-level CryptoProvider". One call at startup fixes
/// all of them, including code inside dependencies.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Android denies `/proc/net/dev` to app UIDs and vnstat needs a database before it can report
/// anything, so "install vnstat" is the wrong advice once it is installed.
pub fn vnstat_hint(err: &str) -> String {
    let lower = err.to_lowercase();
    if lower.contains("database directory") || lower.contains("database is empty") {
        "❌ Usage unavailable: vnstat has no database yet — run `vnstatd` (it has to stay running).".into()
    } else if lower.contains("permission denied") {
        "❌ Usage unavailable: Android blocks /proc/net/dev for app UIDs, so `vnstatd` must run as root.".into()
    } else {
        format!("❌ Failed to get usage: {}", escape_html(err))
    }
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Truncate on a char boundary, appending "..." like the JS version.
/// Hide a bot token inside arbitrary text.
///
/// reqwest quotes the URL it failed on, and that URL contains the token: one upload timeout put a
/// live token into telegram.log and into the chat. Every log line goes through this first.
pub fn redact_token(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("bot") {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let after = &tail[3..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        let colon = after[digits..].starts_with(':');
        let secret = after[digits + usize::from(colon)..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .count();
        if digits > 0 && colon && secret >= 20 {
            out.push_str("bot<redacted>");
            rest = &after[digits + 1 + secret..];
        } else {
            out.push_str("bot");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max).collect();
    s.push_str("...");
    s
}

pub fn rupiah(num: f64) -> String {
    let n = num.round() as i64;
    let digits = n.abs().to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    format!("{}{}", if n < 0 { "-" } else { "" }, grouped)
}

pub fn format_duration(seconds: u64) -> String {
    let d = seconds / 86400;
    let h = (seconds % 86400) / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_truncates() {
        assert_eq!(escape_html("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
        assert_eq!(truncate("abcdef", 3), "abc...");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(rupiah(1234567.0), "1.234.567");
        assert_eq!(format_duration(90061), "1d 1h 1m");
    }


    #[test]
    fn log_once_keeps_the_first_message_per_key() {
        // no panic, no poisoning: the second call must be a no-op
        log_once("test", "same-key", "first");
        log_once("test", "same-key", "second");
        log_once("test", "other-key", "third");
    }

    #[test]
    fn app_client_builds_without_the_platform_verifier() {
        // reqwest 0.13's default client asks rustls-platform-verifier for roots, which panics on
        // Android; every request must go through this builder instead
        let _ = client();
    }

    #[test]
    fn su_is_found_without_a_usable_path() {
        // the bot may be started from a service whose PATH has no $PREFIX/bin
        let resolved = su();
        assert!(!resolved.is_empty());
        if resolved != "su" {
            assert!(std::path::Path::new(&resolved).exists(), "{resolved} must exist");
        }
    }

    #[test]
    fn vnstat_hint_names_the_real_problem() {
        let missing = vnstat_hint("exit 1: Error: Unable to open database directory \"/x\": No such file");
        assert!(missing.contains("no database"), "{missing}");
        let denied = vnstat_hint("exit 1: Error: Permission denied");
        assert!(denied.contains("root"), "{denied}");
        let other = vnstat_hint("exit 2: boom <b>");
        assert!(other.contains("boom &lt;b&gt;"), "{other}");
    }

    #[tokio::test]
    async fn capture_says_why_it_failed() {
        let missing = capture("definitely-not-a-command-xyz", &[]).await.unwrap_err();
        assert!(missing.contains("No such file"), "{missing}");

        let failed = capture("sh", &["-c", "echo boom >&2; exit 3"]).await.unwrap_err();
        assert!(failed.contains("exit 3") && failed.contains("boom"), "{failed}");

        assert_eq!(capture("sh", &["-c", "echo ok"]).await.unwrap().trim(), "ok");
    }

    /// Without the installed provider this is the exact call that panicked inside songbird.
    #[test]
    fn installing_the_provider_makes_default_tls_configs_work() {
        install_crypto_provider();
        // reaching the end without the "Could not automatically determine ... CryptoProvider"
        // panic is the assertion
        let _config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        // songbird builds exactly this client for yt-dlp; it panicked before the fix
        let _songbird_style = reqwest12::Client::new();
    }

    /// A timeout quotes the URL, and that URL carries the token: it reached telegram.log and the
    /// chat once already.
    #[test]
    fn bot_tokens_never_reach_a_log() {
        let leaked = "error sending request for url (https://api.telegram.org/bot123456789:AAF-xyz_ABCdefGHIjklMNOpqrSTUvwx/sendAudio)";
        let clean = redact_token(leaked);
        assert!(!clean.contains("AAF-xyz"), "token survived: {clean}");
        assert!(clean.contains("bot<redacted>/sendAudio"), "{clean}");
        // nothing token-shaped, nothing to hide
        assert_eq!(redact_token("bot is running"), "bot is running");
        assert_eq!(redact_token("robot:1"), "robot:1");
    }
}

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
}

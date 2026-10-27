use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

/// Everything the bot writes (logs + state) goes to `Log/` beside the binary,
/// so the install can live anywhere. Nothing is hardcoded to `~`.
pub fn data_dir() -> PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let dir = base.join("Log");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Font directories that exist on this platform (Termux, GNU/Linux, Android).
pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(prefix) = std::env::var("PREFIX") {
        dirs.push(PathBuf::from(format!("{prefix}/share/fonts")));
    }
    dirs.push(PathBuf::from("/usr/share/fonts"));
    dirs.push(PathBuf::from("/system/fonts"));
    dirs.retain(|d| d.is_dir());
    dirs
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn ts() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Append one line to `Log/<name>.log`, creating the directory first.
pub fn log_to(file: &str, msg: &str) {
    use std::io::Write;
    let path = data_dir().join(file);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "[{}] {}", ts(), msg);
        }
        Err(e) => eprintln!("log {} failed: {e}", path.display()),
    }
}

/// Telegram HTML-escapes text (escape-html package equivalent).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn human(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let mut parts: Vec<String> = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

pub fn format_duration_ms(ms: i64) -> String {
    human(ms)
}

/// mm:ss used by the music status cards.
pub fn format_mmss(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s.is_finite() && s >= 0.0 => {
            let total = s as u64;
            format!("{}:{:02}", total / 60, total % 60)
        }
        _ => "??:??".to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct CmdOut {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl CmdOut {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Run a command, never inheriting stdin, capturing both streams.
pub async fn run(cmd: &str, args: &[&str]) -> anyhow::Result<CmdOut> {
    run_timeout(cmd, args, 120).await
}

pub async fn run_timeout(cmd: &str, args: &[&str], secs: u64) -> anyhow::Result<CmdOut> {
    let out = tokio::time::timeout(
        Duration::from_secs(secs),
        Command::new(cmd).args(args).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{cmd} timed out after {secs}s"))??;
    Ok(CmdOut {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    })
}

/// Fire and forget, detached (used for reboot / media-player calls).
pub fn spawn_cmd(cmd: &str, args: &[&str]) {
    let cmd = cmd.to_string();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::spawn(async move {
        if let Err(e) = run(&cmd, &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await {
            log_to("telegram.log", &format!("{cmd} failed: {e}"));
        }
    });
}

/// Format bytes the way vnstat reports them (input is KiB).
pub fn format_kb(kb: f64) -> String {
    const MB: f64 = 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    if kb < MB {
        format!("{kb:.2} KB")
    } else if kb < GB {
        format!("{:.2} MB", kb / MB)
    } else if kb < TB {
        format!("{:.2} GB", kb / GB)
    } else {
        format!("{:.2} TB", kb / TB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_drops_zero_units() {
        assert_eq!(human(12_000), "12s");
        assert_eq!(human(5 * 60_000), "5m");
        assert_eq!(human(5 * 60_000 + 30_000), "5m 30s");
        assert_eq!(human(2 * 3600_000), "2h");
        assert_eq!(human(2 * 3600_000 + 3 * 60_000 + 4_000), "2h 3m 4s");
    }

    #[test]
    fn bytes_scale() {
        assert_eq!(format_kb(512.0), "512.00 KB");
        assert_eq!(format_kb(2048.0), "2.00 MB");
        assert_eq!(format_kb(3.0 * 1024.0 * 1024.0), "3.00 GB");
    }
}

pub async fn active_iface() -> Option<String> {
    let out = run("ip", &["route", "show", "default"]).await.ok()?;
    let re = regex::Regex::new(r"dev\s+(\S+)").ok()?;
    re.captures(&out.stdout).map(|c| c[1].to_string())
}

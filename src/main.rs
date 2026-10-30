#[macro_use]
mod logx;
mod bing;
mod bot;
mod config;
mod discord;
mod monitor;
mod notification;
mod render;
mod repo;
mod state;
mod telegram;
mod util;
mod ytdlp;

use anyhow::Result;
use std::future::Future;
use std::time::Duration;

const USAGE: &str = "\
Norikimika-Bot (rust)

usage: Norikimika-Bot [module] [--dry-run]

No module = run everything (repo + monitor + notification + bot + discord + bing),
the same single process the JS did.

modules:
  all           every module in one process (default)
  repo          GitHub/GitLab release + commit watcher
  monitor       device, usage, OpenRouter + connection monitors
  notification  Android notification -> Telegram bridge
  bot           Telegram command bot
  discord       Discord bot (voice, no chatbot)
  bing          Bing Rewards automation

env: BOT_TOKEN, CHAT_ID, TOPIC_ID_UPDATE, GITHUB_TOKEN, GITLAB_TOKEN,
     STATE_DIR (default $HOME/Norikimika-Bot)
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");

    util::install_crypto_provider();
    adopt_device_timezone();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    // flags are not module names: `--dry-run` alone means "everything, dry"
    let module = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);

    match module {
        Some("repo") => repo::run(dry_run).await,
        Some("monitor") if dry_run => {
            for path in monitor::render_selftest()? {
                println!("wrote {}", path.display());
            }
            Ok(())
        }
        Some("monitor") => monitor::run().await,
        Some("notification") => notification::run().await,
        Some("bing") => bing::run().await,
        Some("bot") => bot::run().await,
        Some("discord") => discord::run().await,
        Some("all") | None => run_all(dry_run).await,
        Some(other) => {
            eprintln!("unknown module: {other}\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// Supervise a module: a returned `Err`, a `?` on a missing env var or a panic used to leave the
/// process alive with that module silently gone (Telegram commands "dead" while the bot runs).
fn spawn_module<F, Fut>(name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(factory()).await {
                Ok(Ok(())) => logln!(name, "finished on its own"),
                Ok(Err(e)) => logln!(name, "stopped: {e:#}"),
                Err(e) if e.is_panic() => logln!(name, "panicked: {e}"),
                Err(e) => logln!(name, "aborted: {e}"),
            }
            logln!(name, "restarting in 15s");
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

/// Seconds until the next `hour:00` local time.
fn secs_until_from(now: chrono::DateTime<chrono::Local>, hour: u32) -> u64 {
    use chrono::{Datelike, Duration as D, TimeZone};
    // with_ymd_and_hms panics on a bad hour and returns None inside a DST gap
    let mut next = match chrono::Local.with_ymd_and_hms(now.year(), now.month(), now.day(), hour, 0, 0) {
        chrono::LocalResult::Single(t) => t,
        chrono::LocalResult::Ambiguous(earliest, _) => earliest,
        chrono::LocalResult::None => now + D::hours(1),
    };
    if next <= now {
        next += D::days(1);
    }
    (next - now).num_seconds().max(1) as u64
}

fn secs_until(hour: u32) -> u64 {
    secs_until_from(chrono::Local::now(), hour)
}

/// Pin the schedule to the device timezone and say so in the log.
///
/// A Termux process inherits no `TZ`, so `chrono::Local` depends on what bionic falls back to; the
/// nightly 01:00 job must not silently mean 01:00 UTC.
fn adopt_device_timezone() {
    if !std::env::var("TZ").map(|v| !v.is_empty()).unwrap_or(false) {
        if let Ok(out) = std::process::Command::new("getprop").arg("persist.sys.timezone").output() {
            let tz = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !tz.is_empty() {
                std::env::set_var("TZ", &tz);
            }
        }
    }
    let now = chrono::Local::now();
    println!(
        "local time {} (UTC{:+.1}h, TZ {})",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.offset().local_minus_utc() as f64 / 3600.0,
        std::env::var("TZ").unwrap_or_else(|_| "unset".into()),
    );
}

/// One process for every module, mirroring the JS `index.js` entry point.
async fn run_all(dry_run: bool) -> Result<()> {
    if dry_run {
        repo::run(true).await?;
        for path in monitor::render_selftest()? {
            println!("wrote {}", path.display());
        }
        return Ok(());
    }

    // repo every 5 minutes, bing daily at 01:00 — same cadence as the JS loop(), supervised so a
    // panic cannot leave the schedule silently dead
    spawn_module("repo", || async {
        loop {
            if let Err(e) = repo::run(false).await {
                logln!("repo", "error: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
    spawn_module("bing", || async {
        use chrono::Timelike;
        // a reboot or restart after 01:00 used to skip the whole day
        if chrono::Local::now().hour() >= 1 && !bing::ran_today() {
            logln!("bing", "catch-up: today's run has not happened yet");
            if let Err(e) = bing::run().await {
                logln!("bing", "error: {e:#}");
            }
        }
        loop {
            let wait = secs_until(1);
            logln!("bing", "next run in {wait}s at 01:00 local");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            if let Err(e) = bing::run().await {
                logln!("bing", "error: {e:#}");
            }
        }
    });

    // long-running modules; a missing env var only stops that one
    spawn_module("monitor", monitor::run);
    spawn_module("notification", notification::run);
    spawn_module("bot", bot::run);
    if config::get("DISCORD_BOT_TOKEN").is_some() {
        spawn_module("discord", discord::run);
    } else {
        logln!("discord", "skipped: DISCORD_BOT_TOKEN not set");
    }

    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tz_probe_tests {
    #[test]
    fn chrono_honours_tz_set_at_runtime() {
        std::env::set_var("TZ", "Asia/Makassar");
        let off = chrono::Local::now().offset().local_minus_utc();
        std::env::remove_var("TZ");
        assert_eq!(off, 8 * 3600, "chrono ignored the runtime TZ (offset {off}s)");
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn at(h: u32, m: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 29, h, m, 0).unwrap()
    }

    #[test]
    fn nightly_job_waits_for_one_am() {
        assert_eq!(secs_until_from(at(0, 30), 1), 1800);
        assert_eq!(secs_until_from(at(1, 0), 1), 86_400);
        assert_eq!(secs_until_from(at(23, 0), 1), 7200);
    }
}

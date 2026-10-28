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
     STATE_DIR (default /root/.local/share/Norikimika-Bot)
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");

    util::install_crypto_provider();

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

fn spawn_module(name: &'static str, fut: impl Future<Output = Result<()>> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            logln!("all", "{name} stopped: {e:#}");
        }
    });
}

/// Seconds until the next `hour:00` local time.
fn secs_until(hour: u32) -> u64 {
    use chrono::{Datelike, Duration as D, Local, TimeZone};
    let now = Local::now();
    let mut next = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hour, 0, 0)
        .unwrap();
    if next <= now {
        next = next + D::days(1);
    }
    (next - now).num_seconds().max(1) as u64
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

    // repo every 5 minutes, bing daily at 01:00 — same cadence as the JS loop()
    tokio::spawn(async move {
        loop {
            if let Err(e) = repo::run(false).await {
                logln!("all", "repo error: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(secs_until(1))).await;
            if let Err(e) = bing::run().await {
                logln!("all", "bing error: {e:#}");
            }
        }
    });

    // long-running modules; a missing env var only stops that one
    spawn_module("monitor", monitor::run());
    spawn_module("notification", notification::run());
    spawn_module("bot", bot::run());
    if config::get("DISCORD_BOT_TOKEN").is_some() {
        spawn_module("discord", discord::run());
    } else {
        logln!("all", "discord skipped: DISCORD_BOT_TOKEN not set");
    }

    std::future::pending::<()>().await;
    Ok(())
}

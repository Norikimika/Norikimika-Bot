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

const USAGE: &str = "\
Norikimika-Bot (rust)

usage: Norikimika-Bot <module> [--dry-run]

modules:
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

    match args.first().map(String::as_str) {
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
        Some(other) => {
            eprintln!("unknown module: {other}\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
        None => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

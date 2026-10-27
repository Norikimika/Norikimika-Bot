mod app;
mod bing;
mod config;
mod discord;
mod monitor;
mod notification;
mod repo;
mod telegram;
mod tg;
mod util;

use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let cfg = Arc::new(config::Config::load());
    if cfg.bot_token.is_empty() {
        eprintln!("BOT_TOKEN is empty; refusing to start.");
        std::process::exit(1);
    }

    let http = reqwest::Client::builder()
        .user_agent("LogError404Bot/0.1 (+rust)")
        .timeout(Duration::from_secs(60))
        .build()
        .expect("http client");

    let app = Arc::new(app::App::new(cfg.clone(), http));

    // Long-poll offset; the Bot API replays anything we did not confirm.
    let mut offset: i64 = 0;
    if let Ok(raw) = std::fs::read_to_string(util::backup_dir().join("tg_offset")) {
        offset = raw.trim().parse().unwrap_or(0);
    }

    app.tg.set_my_commands().await.ok();
    util::log_to("telegram.log", "Bot launched");
    println!("Bot launched");

    monitor::spawn(app.clone());
    repo::spawn(app.clone());
    notification::spawn(app.clone());
    discord::spawn(app.clone());
    bing::spawn(app.clone());
    util::spawn_cmd("termux-tts-speak", &["Bot launched"]);

    loop {
        match app.tg.get_updates(offset, 30).await {
            Ok(updates) => {
                for update in updates {
                    if let Some(id) = update["update_id"].as_i64() {
                        offset = id + 1;
                    }
                    telegram::handle_update(app.clone(), update).await;
                }
                let _ = std::fs::write(util::backup_dir().join("tg_offset"), offset.to_string());
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("Conflict: terminated by other getUpdates") {
                    util::log_to("telegram.log", &format!("Polling error: {msg}"));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

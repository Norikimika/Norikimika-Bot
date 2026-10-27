use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Config;
use crate::tg::Tg;

pub struct App {
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub tg: Tg,
    admin_cache: Mutex<HashMap<(i64, i64), (bool, Instant)>>,
}

impl App {
    pub fn new(cfg: Arc<Config>, http: reqwest::Client) -> Self {
        let tg = Tg::new(cfg.bot_token.clone(), http.clone());
        Self { cfg, http, tg, admin_cache: Mutex::new(HashMap::new()) }
    }

    /// 5 minute admin cache, same TTL as the JS bot.
    pub async fn is_admin(&self, chat: i64, user: i64) -> bool {
        let key = (chat, user);
        {
            let cache = self.admin_cache.lock().await;
            if let Some((is_admin, at)) = cache.get(&key) {
                if at.elapsed() < Duration::from_secs(300) {
                    return *is_admin;
                }
            }
        }
        let admins = self.tg.admins(chat).await;
        let is_admin = admins.contains(&user);
        self.admin_cache.lock().await.insert(key, (is_admin, Instant::now()));
        is_admin
    }

    /// Kick off a "typing…" refresh loop; abort the handle when done.
    pub fn typing(&self, chat: i64, thread: Option<i64>, action: &'static str) -> tokio::task::JoinHandle<()> {
        let tg = self.tg.clone();
        tokio::spawn(async move {
            loop {
                tg.chat_action(chat, thread, action).await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        })
    }

    pub async fn deny(&self, chat: i64, thread: Option<i64>, reply_to: Option<i64>, cb: Option<&str>, user: i64) {
        let until = chrono::Utc::now().timestamp() + 300;
        self.tg.mute_until(chat, user, until).await;
        if let Some(id) = cb {
            self.tg.answer_callback(id, "⛔ Not authorized. You are muted for 5 minutes.", true).await;
        } else {
            let _ = self
                .tg
                .send_message(chat, thread, "⛔ Not authorized. You are muted for 5 minutes.", None, None, reply_to)
                .await;
        }
    }
}

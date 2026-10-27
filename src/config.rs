use std::env;

fn s(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn i64_or(key: &str, default: i64) -> i64 {
    s(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn i64_opt(key: &str) -> Option<i64> {
    s(key).and_then(|v| v.trim().parse().ok())
}

fn bool_or(key: &str, default: bool) -> bool {
    match s(key) {
        Some(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

/// Everything read once from .env; modules may still read ad-hoc keys with env::var.
#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    pub chat_id: i64,
    pub topic_id: Option<i64>,
    pub topic_usage: Option<i64>,
    pub topic_status: Option<i64>,
    pub topic_speed: Option<i64>,
    pub topic_connection: Option<i64>,
    pub topic_music: Option<i64>,
    pub topic_music2: Option<i64>,
    pub topic_tiktok: Option<i64>,
    pub topic_intake: Option<i64>,
    pub topic_preview: Option<i64>,
    pub topic_update: Option<i64>,
    pub topic_v380: Option<i64>,
    pub topic_tuya: Option<i64>,
    pub topic_gopay: Option<i64>,
    pub topic_al_quran: Option<i64>,

    pub discord_token: Option<String>,
    pub discord_chatbot_channel: Option<u64>,
    pub discord_welcome_channel: Option<u64>,
    pub discord_goodbye_channel: Option<u64>,

    pub nine_router_url: Option<String>,
    pub nine_router_key: Option<String>,
    pub nine_router_model: String,
    pub nine_router_search_model: String,
    pub nine_router_tts_model: String,
    pub nine_router_chatbot_model: String,
    pub nine_router_chatbot_context: String,

    pub sheets_id: Option<String>,
    pub sheets_gid: Option<String>,
    pub github_token: Option<String>,
    pub gitlab_token: Option<String>,

    pub print_cmd: String,
    pub receipt_font: String,
    pub receipt_fontsize: Option<String>,
    pub store_name: String,
    pub store_address: String,
    pub ytdlp_proxy: Option<String>,

    pub auto_clear_enabled: bool,
    pub db_cleanup_cycles: i64,
    pub db_retention_seconds: i64,
    pub dedupe_window_seconds: i64,
    pub max_notifications: i64,
    pub pkill_throttle_seconds: i64,
    pub check_interval: i64,
}

impl Config {
    pub fn load() -> Self {
        Self {
            bot_token: s("BOT_TOKEN").unwrap_or_default(),
            chat_id: i64_or("CHAT_ID", 0),
            topic_id: i64_opt("TOPIC_ID"),
            topic_usage: i64_opt("TOPIC_ID_USAGE"),
            topic_status: i64_opt("TOPIC_ID_STATUS"),
            topic_speed: i64_opt("TOPIC_ID_SPEED"),
            topic_connection: i64_opt("TOPIC_ID_CONNECTION"),
            topic_music: i64_opt("TOPIC_ID_MUSIC"),
            topic_music2: i64_opt("TOPIC_ID_MUSIC2"),
            topic_tiktok: i64_opt("TOPIC_ID_TIKTOK"),
            topic_intake: i64_opt("TOPIC_ID_INTAKE").or(Some(416182)),
            topic_preview: i64_opt("TOPIC_ID_PREVIEW").or(Some(468585)),
            topic_update: i64_opt("TOPIC_ID_UPDATE"),
            topic_v380: i64_opt("TOPIC_ID_V380"),
            topic_tuya: i64_opt("TOPIC_ID_TUYA"),
            topic_gopay: i64_opt("TOPIC_ID_GOPAY"),
            topic_al_quran: i64_opt("TOPIC_ID_AL_QURAN"),

            discord_token: s("DISCORD_BOT_TOKEN"),
            discord_chatbot_channel: i64_opt("DISCORD_CHATBOT_CHANNEL_ID").map(|v| v as u64),
            discord_welcome_channel: i64_opt("DISCORD_WELCOME_CHANNEL_ID").map(|v| v as u64),
            discord_goodbye_channel: i64_opt("DISCORD_GOODBYE_CHANNEL_ID").map(|v| v as u64),

            nine_router_url: s("NINE_ROUTER_URL"),
            nine_router_key: s("NINE_ROUTER_API_KEY"),
            nine_router_model: s("NINE_ROUTER_MODEL").unwrap_or_else(|| "openai/gpt-4o-mini".into()),
            nine_router_search_model: s("NINE_ROUTER_SEARCH_MODEL").unwrap_or_default(),
            nine_router_tts_model: s("NINE_ROUTER_TTS_MODEL").unwrap_or_else(|| "edge-tts/id-ID-GadisNeural".into()),
            nine_router_chatbot_model: s("NINE_ROUTER_CHATBOT_MODEL").unwrap_or_else(|| "openai/gpt-4o-mini".into()),
            nine_router_chatbot_context: s("NINE_ROUTER_CHATBOT_CONTEXT").unwrap_or_default(),

            sheets_id: s("SHEETS_ID"),
            sheets_gid: s("SHEETS_GID"),
            github_token: s("GITHUB_TOKEN"),
            gitlab_token: s("GITLAB_TOKEN"),

            print_cmd: s("PRINT_CMD").unwrap_or_default(),
            receipt_font: s("STRUK_FONT").unwrap_or_default(),
            receipt_fontsize: s("STRUK_FONTSIZE"),
            store_name: s("STORE_NAME").unwrap_or_default(),
            store_address: s("STORE_ADDRESS").unwrap_or_default(),
            ytdlp_proxy: s("YTDLP_PROXY"),

            auto_clear_enabled: bool_or("AUTO_CLEAR_ENABLED", false),
            db_cleanup_cycles: i64_or("DB_CLEANUP_CYCLES", 0),
            db_retention_seconds: i64_or("DB_RETENTION_SECONDS", 0),
            dedupe_window_seconds: i64_or("DEDUPE_WINDOW_SECONDS", 60),
            max_notifications: i64_or("MAX_NOTIFICATIONS", 50),
            pkill_throttle_seconds: i64_or("PKILL_THROTTLE_SECONDS", 30),
            check_interval: i64_or("CHECK_INTERVAL", 60),
        }
    }

    /// yt-dlp arguments with the optional egress proxy prefix.
    pub fn ytdlp_args(&self, args: Vec<String>) -> Vec<String> {
        match &self.ytdlp_proxy {
            Some(p) => {
                let mut v = vec!["--proxy".to_string(), p.clone()];
                v.extend(args);
                v
            }
            None => args,
        }
    }
}

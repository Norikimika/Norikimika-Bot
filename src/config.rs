//! Env loading + shared config. Minimal .env reader (no dotenv crate).
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

static ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

fn load_dotenv() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let path = std::env::var("ENV_FILE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(".env"));
    if let Ok(raw) = std::fs::read_to_string(&path) {
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                map.insert(k.trim().to_string(), v.to_string());
            }
        }
    }
    map
}

/// Env var: real environment wins over .env file.
pub fn get(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    ENV.get_or_init(load_dotenv).get(key).cloned().filter(|v| !v.is_empty())
}

pub fn req(key: &str) -> anyhow::Result<String> {
    get(key).ok_or_else(|| anyhow::anyhow!("missing env var: {key}"))
}

pub fn get_or(key: &str, default: &str) -> String {
    get(key).unwrap_or_else(|| default.to_string())
}

pub fn int(key: &str, default: i64) -> i64 {
    get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Runtime dir for state/logs. Never the Termux home or ~/SERVER.
pub fn state_dir() -> PathBuf {
    let dir = get("STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root/.local/share/Norikimika-Bot"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn state_file(name: &str) -> PathBuf {
    state_dir().join(name)
}

pub fn chat_id() -> Option<i64> {
    get("CHAT_ID").and_then(|v| v.parse().ok())
}

pub fn topic(key: &str) -> Option<i64> {
    get(key).and_then(|v| v.parse().ok())
}

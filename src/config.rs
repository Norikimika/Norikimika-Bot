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

pub fn int(key: &str, default: i64) -> i64 {
    get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Where state/logs live when `STATE_DIR` is not set: `$HOME/Norikimika-Bot`.
///
/// A plain, visible directory in the home: these files are logs, state JSON and rendered
/// PNGs that get opened by hand, so hiding them under `.local/share` only makes them harder
/// to find. Using `$HOME` instead of a fixed `/root` keeps the same binary working on a
/// server and under Termux, where `/root` does not exist at all.
fn default_state_dir(home: Option<PathBuf>) -> PathBuf {
    match home {
        Some(home) => home.join("Norikimika-Bot"),
        None => std::env::temp_dir().join("Norikimika-Bot"),
    }
}

/// Runtime dir for state/logs.
pub fn state_dir() -> PathBuf {
    let dir = get("STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_state_dir(std::env::var_os("HOME").map(PathBuf::from)));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::util::log_once("config", "state-dir", &format!("cannot create {}: {e}", dir.display()));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_dir_follows_home() {
        let dir = default_state_dir(Some(PathBuf::from("/home/tester")));
        assert_eq!(dir, PathBuf::from("/home/tester/Norikimika-Bot"));
    }

    #[test]
    fn default_state_dir_falls_back_to_temp_without_home() {
        let dir = default_state_dir(None);
        assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
    }

    #[test]
    fn state_dir_exists_after_asking_for_it() {
        let dir = state_dir();
        assert!(dir.is_dir(), "{} must be created", dir.display());
        assert!(state_file("probe.json").starts_with(&dir));
    }
}

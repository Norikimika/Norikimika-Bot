//! Atomic JSON state map with corrupt-file quarantine (port of loadState/saveState).
use serde_json::{Map, Value};
use std::path::PathBuf;

pub struct State {
    pub path: PathBuf,
    pub map: Map<String, Value>,
}

impl State {
    pub fn load(name: &str) -> Self {
        let path = crate::config::state_file(name);
        let map = match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let clean: String = raw.replace('\0', "");
                if clean.trim().is_empty() {
                    Map::new()
                } else {
                    match serde_json::from_str::<Value>(&clean) {
                        Ok(Value::Object(m)) => m,
                        _ => {
                            let _ = std::fs::rename(&path, path.with_extension(format!(
                                "corrupt-{}",
                                chrono::Local::now().timestamp()
                            )));
                            Map::new()
                        }
                    }
                }
            }
            Err(_) => Map::new(),
        };
        Self { path, map }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.map.get(key).and_then(Value::as_str)
    }

    pub fn set_str(&mut self, key: &str, value: impl Into<String>) {
        self.map.insert(key.to_string(), Value::String(value.into()));
    }

    /// Write via tmp + rename so a crash never leaves half a file.
    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.map)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

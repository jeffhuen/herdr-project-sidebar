use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WRITE_INTERVAL: Duration = Duration::from_secs(5);
pub const ACTIVITY_FRESH_MS: u64 = 15 * 60 * 1000;
pub const ACTIVITY_STALE_MS: u64 = 120 * 60 * 1000;

#[derive(Default)]
pub struct ActivityStore {
    path: PathBuf,
    pub stamps: HashMap<String, u64>,
    last_write: Option<SystemTime>,
}

impl ActivityStore {
    pub fn new(state_dir: PathBuf) -> Self {
        let path = state_dir.join("activity.json");
        let stamps = match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            stamps,
            last_write: None,
        }
    }

    pub fn mark_working(&mut self, pane_id: &str, now_ms: u64) {
        self.stamps.insert(pane_id.to_owned(), now_ms);
        self.save(false);
    }

    pub fn save(&mut self, force: bool) {
        let now = SystemTime::now();
        if !force {
            if let Some(last) = self.last_write {
                if now.duration_since(last).unwrap_or_default() < WRITE_INTERVAL {
                    return;
                }
            }
        }
        self.last_write = Some(now);
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&self.stamps) {
            let _ = fs::write(&self.path, json);
        }
    }

    pub fn freshness(&self, pane_id: &str, now_ms: u64) -> Freshness {
        match self.stamps.get(pane_id) {
            Some(&at) => {
                let age = now_ms.saturating_sub(at);
                if age <= ACTIVITY_FRESH_MS {
                    Freshness::Fresh
                } else if age >= ACTIVITY_STALE_MS {
                    Freshness::Stale
                } else {
                    Freshness::Normal
                }
            }
            None => Freshness::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Normal,
    Stale,
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

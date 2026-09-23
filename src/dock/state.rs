use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::activity::{ActivityStore, Freshness};
use serde::{Deserialize, Serialize};

use super::model::{Project, State};

/// Explicit holds outvote adopted vetoes this long; afterwards recency is
/// unknowable and vetoes apply normally so stale pins always converge away.
const VETO_GRACE_SECS: u64 = 60;
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default)]
pub(super) struct Memory {
    pub(super) activity: ActivityStore,
    pub(super) collapsed_projects: BTreeSet<String>,
    pub(super) collapsed_worktrees: BTreeSet<String>,
    pub(super) pinned: BTreeSet<String>,
    /// Shared removal vetoes: "c:<project>", "w:<worktree>", "p:<project>".
    pub(super) dropped: BTreeSet<String>,
    /// Recent explicit holds override shared vetoes until VETO_GRACE_SECS expires.
    pub(super) touched: BTreeMap<String, u64>,
    /// "auto" detects a Nerd Font; "font" and "text" are explicit choices.
    pub(super) font_choice: String,
    pub(super) dirty_state: bool,
    pub(super) last_state_save: Option<std::time::Instant>,
    /// Checkout path -> branch from Git HEAD reads, cached for five seconds.
    pub(super) branches: BTreeMap<String, String>,
    pub(super) linked_checkouts: HashSet<String>,
    pub(super) branch_at: Option<std::time::Instant>,
}

impl Memory {
    pub(super) fn freshness(&self, terminal_id: &str, now_ms: u64) -> State {
        match self.activity.freshness(terminal_id, now_ms) {
            Freshness::Fresh => State::IdleFresh,
            Freshness::Normal => State::Idle,
            Freshness::Stale => State::IdleStale,
        }
    }
}

pub(super) fn state_path() -> std::path::PathBuf {
    crate::config::state_dir().join("state.json")
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistedState {
    collapsed_projects: BTreeSet<String>,
    collapsed_worktrees: BTreeSet<String>,
    pinned: BTreeSet<String>,
    dropped: BTreeSet<String>,
    #[serde(default = "auto_font")]
    font_choice: String,
}

fn auto_font() -> String {
    "auto".into()
}

fn read_state_file(path: &Path) -> PersistedState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| PersistedState { font_choice: auto_font(), ..PersistedState::default() })
}

pub(super) fn load_state(mem: &mut Memory, path: &Path) {
    let state = read_state_file(path);
    mem.collapsed_projects = state.collapsed_projects;
    mem.collapsed_worktrees = state.collapsed_worktrees;
    mem.pinned = state.pinned;
    mem.dropped = state.dropped;
    mem.font_choice = state.font_choice;
    apply_dropped(mem);
}

pub(super) fn touch_hold(mem: &mut Memory, key: &str) {
    mem.dropped.remove(key);
    mem.touched.insert(key.to_owned(), now_secs());
}

pub(super) fn touch_release(mem: &mut Memory, key: &str) {
    mem.touched.remove(key);
    mem.dropped.insert(key.to_owned());
}

fn apply_dropped(mem: &mut Memory) {
    for k in mem.dropped.iter() {
        let (ns, plain) = k.split_once(':').unwrap_or(("", k));
        match ns {
            "c" => {
                mem.collapsed_projects.remove(plain);
            }
            "w" => {
                mem.collapsed_worktrees.remove(plain);
            }
            "p" => {
                mem.pinned.remove(plain);
            }
            _ => {}
        }
    }
}

pub(super) fn save_state(mem: &mut Memory, force: bool, path: &Path) {
    let now = std::time::Instant::now();
    if !force {
        if !mem.dirty_state {
            return;
        }
        if let Some(t) = mem.last_state_save {
            if now.duration_since(t) < STATE_SAVE_DEBOUNCE {
                return;
            }
        }
    }
    mem.last_state_save = Some(now);
    mem.dirty_state = false;
    // Merge other docks' additions and vetoes; recent explicit holds win.
    let file = read_state_file(path);
    mem.dropped.extend(file.dropped);
    let now = now_secs();
    mem.touched
        .retain(|_, at| now.saturating_sub(*at) < VETO_GRACE_SECS);
    for t in mem.touched.keys() {
        mem.dropped.remove(t);
    }
    apply_dropped(mem);
    // Keep vetoes until an explicit hold clears them, including for offline docks.
    let union = |mut saved: BTreeSet<String>, ns: &str, own: &BTreeSet<String>| {
        saved.retain(|s| !mem.dropped.contains(&format!("{ns}:{s}")));
        saved.extend(own.iter().cloned());
        saved
    };
    let state = PersistedState {
        collapsed_projects: union(file.collapsed_projects, "c", &mem.collapsed_projects),
        collapsed_worktrees: union(file.collapsed_worktrees, "w", &mem.collapsed_worktrees),
        pinned: union(file.pinned, "p", &mem.pinned),
        font_choice: mem.font_choice.clone(),
        dropped: mem.dropped.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&state) {
        let _ = crate::util::atomic_write(path, &bytes);
    }
}

pub(super) fn sync_collapse(projects: &[Project], mem: &mut Memory, path: &Path) {
    // Keep saved folds for projects omitted from this view. Record unfolds as vetoes.
    for p in projects {
        let k = format!("c:{}", p.id);
        if p.collapsed {
            mem.collapsed_projects.insert(p.id.clone());
            mem.dropped.remove(&k);
        } else {
            mem.collapsed_projects.remove(&p.id);
            mem.dropped.insert(k);
        }
        for w in &p.worktrees {
            let k = format!("w:{}", w.key);
            if w.collapsed {
                mem.collapsed_worktrees.insert(w.key.clone());
                mem.dropped.remove(&k);
            } else {
                mem.collapsed_worktrees.remove(&w.key);
                mem.dropped.insert(k);
            }
        }
    }
    mem.dirty_state = true;
    // Explicit folds must survive a native pane close before the activity debounce.
    save_state(mem, true, path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_tiers_split_idle() {
        let mut mem = Memory::default();
        mem.activity.mark_working("terminal-alpha", 1_000_000);
        assert_eq!(mem.freshness("terminal-alpha", 1_060_000), State::IdleFresh);
        assert_eq!(mem.freshness("terminal-alpha", 4_600_000), State::Idle);
        assert_eq!(
            mem.freshness("terminal-alpha", 10_000_000),
            State::IdleStale
        );
        assert_eq!(mem.freshness("unseen", 1_000_000), State::Idle);
    }

    #[test]
    fn shared_veto_converges_across_instances() {
        let dir = std::env::temp_dir().join(format!("hps-conv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&state_path, r#"{"pinned":["w1Q"]}"#).unwrap();
        let mut a = Memory::default();
        let mut b = Memory::default();
        load_state(&mut a, &state_path);
        load_state(&mut b, &state_path);
        save_state(&mut a, true, &state_path);
        let persisted: serde_json::Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(persisted, serde_json::json!({
            "collapsed_projects": [], "collapsed_worktrees": [], "pinned": ["w1Q"],
            "dropped": [], "font_choice": "auto"
        }));
        load_state(&mut b, &state_path);
        assert!(b.pinned.contains("w1Q"));
        b.pinned.remove("w1Q");
        touch_release(&mut b, "p:w1Q");
        save_state(&mut b, true, &state_path);
        save_state(&mut a, true, &state_path);
        assert!(!a.pinned.contains("w1Q"));
        let mut c = Memory::default();
        load_state(&mut c, &state_path);
        assert!(!c.pinned.contains("w1Q"));
        // An offline holder must still adopt the removal on its next save.
        let mut e = Memory::default();
        e.pinned.insert("w1Q".into());
        save_state(&mut e, true, &state_path);
        let mut f = Memory::default();
        load_state(&mut f, &state_path);
        assert!(!f.pinned.contains("w1Q"));
        // Fresh re-pin in B overrules the veto and sticks globally.
        b.pinned.insert("w1Q".into());
        touch_hold(&mut b, "p:w1Q");
        save_state(&mut b, true, &state_path);
        let mut d = Memory::default();
        load_state(&mut d, &state_path);
        assert!(d.pinned.contains("w1Q"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

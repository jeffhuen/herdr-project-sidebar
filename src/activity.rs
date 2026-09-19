use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WRITE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
pub const ACTIVITY_FRESH_MS: u64 = 15 * 60 * 1000;
pub const ACTIVITY_STALE_MS: u64 = 120 * 60 * 1000;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Persisted {
    scope: String,
    stamps: HashMap<String, u64>,
}

#[derive(Default)]
pub struct ActivityStore {
    state_dir: PathBuf,
    session: Option<crate::ipc::Session>,
    pub stamps: HashMap<String, u64>,
    live: HashSet<String>,
    dirty: bool,
    last_write: Option<Instant>,
}

impl ActivityStore {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            ..Self::default()
        }
    }

    pub fn sync_session(&mut self, session: &crate::ipc::Session) -> io::Result<bool> {
        session.check()?;
        if self
            .session
            .as_ref()
            .is_some_and(|old| old.key() == session.key())
        {
            return Ok(false);
        }
        let saved = read_state(&self.path(session))?;
        self.dirty = saved.scope != session.key();
        self.stamps = if self.dirty {
            HashMap::new()
        } else {
            saved.stamps
        };
        self.live.clear();
        self.last_write = None;
        self.session = Some(session.clone());
        Ok(true)
    }

    fn path(&self, session: &crate::ipc::Session) -> PathBuf {
        self.state_dir
            .join(format!("activity-{:016x}.json", session.socket_hash()))
    }

    pub fn mark_working(&mut self, terminal_id: &str, now_ms: u64) {
        if terminal_id.trim().is_empty() {
            return;
        }
        let stamp = self.stamps.entry(terminal_id.to_owned()).or_default();
        if now_ms > *stamp {
            *stamp = now_ms;
            self.dirty = true;
        }
    }

    pub fn retain_live(&mut self, live: &HashSet<&str>) {
        let changed = self.live.len() != live.iter().filter(|id| !id.trim().is_empty()).count()
            || self.live.iter().any(|id| !live.contains(id.as_str()));
        if changed {
            self.live = live
                .iter()
                .filter(|id| !id.trim().is_empty())
                .map(|id| (*id).to_owned())
                .collect();
            self.dirty = true;
        }
        let before = self.stamps.len();
        self.stamps.retain(|id, _| self.live.contains(id));
        self.dirty |= self.stamps.len() != before;
    }

    pub fn save(&mut self, force: bool) -> io::Result<()> {
        if !self.dirty
            || (!force
                && self
                    .last_write
                    .is_some_and(|at| at.elapsed() < WRITE_INTERVAL))
        {
            return Ok(());
        }
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| io::Error::other("activity session is not synchronized"))?;
        session.check()?;
        fs::create_dir_all(&self.state_dir)?;
        let path = self.path(session);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        // Under the file lock, an old writer cannot replace a new session's
        // history. The scope lives in one file per socket, not one per restart.
        session.check()?;
        let mut saved = read_state(&path)?;
        if saved.scope != session.key() {
            saved = Persisted {
                scope: session.key().to_owned(),
                ..Persisted::default()
            };
        }
        for (id, at) in &self.stamps {
            if self.live.contains(id) {
                saved
                    .stamps
                    .entry(id.clone())
                    .and_modify(|old| *old = (*old).max(*at))
                    .or_insert(*at);
            }
        }
        saved.stamps.retain(|id, _| self.live.contains(id));
        let bytes = serde_json::to_vec(&saved)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "activity state exceeds size limit",
            ));
        }
        // The session lock serializes threads and processes, including temp-file use.
        let temporary = path.with_extension("tmp");
        let written = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        self.stamps = saved.stamps;
        self.last_write = Some(Instant::now());
        self.dirty = false;
        Ok(())
    }

    pub fn freshness(&self, terminal_id: &str, now_ms: u64) -> Freshness {
        match self.stamps.get(terminal_id) {
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

fn read_state(path: &Path) -> io::Result<Persisted> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Persisted::default()),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activity state exceeds size limit",
        ));
    }
    let mut saved: Persisted = serde_json::from_slice(&bytes)?;
    saved.stamps.retain(|id, _| !id.trim().is_empty());
    Ok(saved)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Directory {
        root: PathBuf,
        listener: UnixListener,
    }
    impl Directory {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hps-activity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let listener = UnixListener::bind(root.join("api.sock")).unwrap();
            Self { root, listener }
        }
        fn session(&self) -> crate::ipc::Session {
            crate::ipc::Session::at(self.root.join("api.sock")).unwrap()
        }
        fn store(&self) -> ActivityStore {
            let mut store = ActivityStore::new(self.root.clone());
            store.sync_session(&self.session()).unwrap();
            store
        }
        fn replace_server(&mut self) {
            fs::remove_file(self.root.join("api.sock")).unwrap();
            self.listener = UnixListener::bind(self.root.join("api.sock")).unwrap();
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn replacement_sessions_reject_old_writers_without_losing_new_history() {
        let mut directory = Directory::new();
        fs::write(directory.root.join("activity.json"), r#"{"pane:1":1000}"#).unwrap();
        let mut old = directory.store();
        assert_eq!(old.freshness("pane:1", 2_000), Freshness::Normal);
        old.mark_working("terminal", 1_000);
        old.retain_live(&HashSet::from(["terminal"]));
        old.save(true).unwrap();
        assert_eq!(
            directory.store().freshness("terminal", 2_000),
            Freshness::Fresh
        );

        directory.replace_server();
        let mut new = directory.store();
        assert_eq!(new.freshness("terminal", 2_000), Freshness::Normal);
        new.mark_working("new", 2_000);
        new.retain_live(&HashSet::from(["new"]));
        new.save(true).unwrap();
        old.mark_working("terminal", 3_000);
        assert_eq!(
            old.save(true).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        let saved = directory.store();
        assert_eq!(saved.freshness("new", 3_000), Freshness::Fresh);
        assert_eq!(saved.freshness("terminal", 3_000), Freshness::Normal);
    }

    #[test]
    fn merges_working_stamps_and_prunes_to_the_current_live_terminals() {
        let directory = Directory::new();
        let mut old = directory.store();
        old.mark_working("live", 1_000);
        old.mark_working("removed", 1_000);
        old.retain_live(&HashSet::from(["live", "removed"]));
        old.save(true).unwrap();
        let mut newer = directory.store();
        newer.mark_working("live", 2_000);
        newer.mark_working("new", 500);
        newer.retain_live(&HashSet::from(["live", "new"]));
        newer.save(true).unwrap();
        old.mark_working("live", 1_500);
        old.retain_live(&HashSet::from(["live", "new"]));
        old.save(true).unwrap();
        let saved = directory.store();
        assert_eq!(saved.freshness("live", 902_000), Freshness::Fresh);
        assert_eq!(saved.freshness("new", 1_000), Freshness::Fresh);
        assert_eq!(saved.freshness("removed", 2_000), Freshness::Normal);
    }

    #[test]
    fn independent_sockets_keep_independent_activity() {
        let directory = Directory::new();
        let other_socket = directory.root.join("other.sock");
        let _other_listener = UnixListener::bind(&other_socket).unwrap();
        let other_session = crate::ipc::Session::at(other_socket).unwrap();
        let mut first = directory.store();
        first.mark_working("terminal", 1_000);
        first.retain_live(&HashSet::from(["terminal"]));
        first.save(true).unwrap();
        let mut other = ActivityStore::new(directory.root.clone());
        other.sync_session(&other_session).unwrap();
        assert_eq!(other.freshness("terminal", 2_000), Freshness::Normal);
        other.retain_live(&HashSet::new());
        other.save(true).unwrap();
        assert_eq!(
            directory.store().freshness("terminal", 2_000),
            Freshness::Fresh
        );
    }

    #[test]
    fn failed_save_retries_without_losing_activity() {
        let directory = Directory::new();
        let mut store = directory.store();
        store.mark_working("terminal", 1_000);
        store.retain_live(&HashSet::from(["terminal"]));
        let temporary = store.path(&directory.session()).with_extension("tmp");
        fs::create_dir(&temporary).unwrap();
        assert!(store.save(false).is_err());
        fs::remove_dir(&temporary).unwrap();
        store.save(false).unwrap();
        assert_eq!(
            directory.store().freshness("terminal", 2_000),
            Freshness::Fresh
        );
    }
}

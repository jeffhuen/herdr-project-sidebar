//! Publish presentation tokens matching Radar's exact visual and behavioral contract.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::activity::{now_unix_ms, ActivityStore, Freshness};
use crate::{config, dock_control, icons, ipc};

const SOURCE: &str = "plugin:herdr-project-sidebar";
const SPIN_MS: u64 = 250;
const SETTINGS_POLL: Duration = Duration::from_millis(300);
const CONTROL_LIMIT: u64 = 8192;
const START_TIMEOUT: Duration = Duration::from_secs(30);
const READY_ENV: &str = "HERDR_PROJECT_SIDEBAR_READY";
const RECOVERY_GRACE: Duration = Duration::from_secs(30);
const COMMAND_START_ENV: &str = "HERDR_PROJECT_SIDEBAR_COMMAND_START";

const KEYS: [&str; 17] = [
    "group_parent",
    "group",
    "group_stale",
    "split_mark",
    "logo",
    "logo_working",
    "logo_stale",
    "harness_logo",
    "title_working",
    "title_done",
    "title_blocked",
    "title_idle_fresh",
    "title_idle",
    "title_idle_stale",
    "title_unknown",
    "gap",
    "ws_group",
];

const SPACE_KEYS: [&str; 18] = [
    "space_blocked",
    "space_done",
    "space_idle",
    "space_unknown",
    "space_none",
    "space_label",
    "space_working_claude",
    "space_working_gemini",
    "space_working_kimi",
    "space_working_qwen",
    "space_working_kiro",
    "space_working_cline",
    "space_working_kilo",
    "space_working_other",
    "space_logo_claude",
    "space_logo_gemini",
    "space_logo_kimi",
    "space_logo_qwen",
];

type Tokens = Map<String, Value>;
type RowTokens = BTreeMap<String, Tokens>;

fn socket_path() -> io::Result<PathBuf> {
    std::env::var_os("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HERDR_SOCKET_PATH is not set"))
}

fn daemon_paths() -> io::Result<(PathBuf, PathBuf)> {
    let hash = ipc::socket_hash(&socket_path()?);
    let dir = config::state_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok((
        dir.join(format!("daemon-{hash:016x}.lock")),
        dir.join(format!("d-{hash:016x}.sock")),
    ))
}

struct SocketFile(PathBuf);
impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn start() -> io::Result<()> {
    if !config::load()?.enabled {
        return Ok(());
    }
    start_inner(false)
}

fn start_inner(await_command: bool) -> io::Result<()> {
    let (_, ready) = daemon_paths()?;
    // Serialize launchers before spawning, not after each has created a daemon.
    let launch = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(ready.with_extension("start.lock"))?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match launch.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "sidebar launcher is busy",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    if UnixStream::connect(&ready).is_ok() {
        return Ok(());
    }
    let reply = config::state_dir().join(format!("r-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&reply)?;
    let _reply_file = SocketFile(reply.clone());
    listener.set_nonblocking(true)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ready.with_extension("log"))?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--daemon")
        .env(READY_ENV, &reply)
        .env(COMMAND_START_ENV, if await_command { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .process_group(0)
        .spawn()?;
    let deadline = Instant::now() + START_TIMEOUT;
    let result = (|| loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let mut line = String::new();
                BufReader::new(stream).take(8192).read_line(&mut line)?;
                let result: Value = serde_json::from_str(&line)?;
                return if result["ok"] == true {
                    Ok(())
                } else {
                    Err(io::Error::other(
                        result["error"]
                            .as_str()
                            .unwrap_or("sidebar initialization failed")
                            .to_owned(),
                    ))
                };
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            if let Ok((stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let result: Value = serde_json::from_reader(stream.take(8192))?;
                return if result["ok"] == true {
                    Ok(())
                } else {
                    Err(io::Error::other(
                        result["error"]
                            .as_str()
                            .unwrap_or("sidebar initialization failed")
                            .to_owned(),
                    ))
                };
            }
            return Err(io::Error::other(format!(
                "sidebar daemon exited before readiness: {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sidebar daemon initialization timed out",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DockRequest {
    command: dock_control::Command,
    caller_tab_id: Option<String>,
    session: String,
}

pub fn dock_command(command: dock_control::Command, session: &ipc::Session) -> io::Result<()> {
    session.check()?;
    let caller_tab_id = if !matches!(command, dock_control::Command::Toggle) {
        None
    } else if std::env::var_os("HERDR_PLUGIN_ACTION_ID").is_some() {
        // The action's pane may close before it runs; its tab is the toggle target.
        Some(
            std::env::var("HERDR_TAB_ID")
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
        )
    } else {
        match std::env::var("HERDR_PANE_ID") {
            Ok(inherited) => {
                let current = session.call("pane.current", json!({"caller_pane_id": inherited}))?;
                Some(
                    current["pane"]["tab_id"]
                        .as_str()
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "pane.current omitted tab_id",
                            )
                        })?
                        .to_owned(),
                )
            }
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
        }
    };
    session.check()?;
    start_inner(true)?;
    session.check()?;
    let (_, ready) = daemon_paths()?;
    let mut stream = UnixStream::connect(ready)?;
    stream.set_read_timeout(Some(START_TIMEOUT))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    session.check()?;
    serde_json::to_writer(
        &mut stream,
        &DockRequest {
            command,
            caller_tab_id,
            session: session.key().to_owned(),
        },
    )?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream.take(CONTROL_LIMIT)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated dock command reply",
        ));
    }
    let reply: Value = serde_json::from_str(&line)?;
    if reply["ok"] == true {
        Ok(())
    } else {
        Err(io::Error::other(
            reply["error"]
                .as_str()
                .unwrap_or("dock command failed")
                .to_owned(),
        ))
    }
}

fn handle_command(
    mut stream: UnixStream,
    controller: &mut dock_control::Controller,
) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let result = (|| {
        let mut line = String::new();
        BufReader::new((&mut stream).take(CONTROL_LIMIT)).read_line(&mut line)?;
        if line.is_empty() {
            return Ok(None);
        } // Readiness probe.
        if !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated dock command",
            ));
        }
        let request: DockRequest = serde_json::from_str(&line)?;
        controller.command(
            request.command,
            request.caller_tab_id.as_deref(),
            &request.session,
        )?;
        Ok(Some(()))
    })();
    let reply = match result {
        Ok(None) => return Ok(false),
        Ok(Some(())) => json!({"ok": true}),
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    };
    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")?;
    Ok(true)
}

fn notify_start(result: &io::Result<()>) -> io::Result<()> {
    let Some(path) = std::env::var_os(READY_ENV) else {
        return Ok(());
    };
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let reply = match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    };
    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")
}

pub fn run() -> io::Result<()> {
    let result = run_inner();
    if result.is_err() {
        let _ = notify_start(&result);
    }
    result
}

fn run_inner() -> io::Result<()> {
    let (lock_path, ready_path) = daemon_paths()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let deadline = Instant::now() + START_TIMEOUT;
            loop {
                if UnixStream::connect(&ready_path).is_ok() {
                    return notify_start(&Ok(()));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "existing sidebar daemon did not become ready",
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    match fs::remove_file(&ready_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut publisher = Publisher::default();
    let publication = publication_lock()?;
    let mut controller = dock_control::Controller::new(ready_path.with_extension("json"))?;
    let listener = UnixListener::bind(&ready_path)?;
    let _ready_file = SocketFile(ready_path);
    listener.set_nonblocking(true)?;
    let mut sync = ipc::SnapshotSync::new();
    let mut settings = config::load()?;
    let mut settings_at = Instant::now();
    let mut animation_session = None;
    let mut animation_at = Instant::now();
    let mut plugin_check_at = Instant::now();
    let mut lost_at: Option<Instant> = None;
    let mut last_refresh_error: Option<String> = None;
    // A first toggle must arrive before auto-open, or it would close the dock
    // that startup just created. Abandoned clients only defer startup briefly.
    let awaiting_command = std::env::var(COMMAND_START_ENV).as_deref() == Ok("1");
    let mut command_deadline = awaiting_command.then(|| Instant::now() + START_TIMEOUT);
    let mut first = !awaiting_command;
    if awaiting_command {
        notify_start(&Ok(()))?;
    }
    loop {
        let started = Instant::now();
        // Bound command work so queued callers cannot starve automatic following.
        for _ in 0..16 {
            match listener.accept() {
                Ok((stream, _)) => match handle_command(stream, &mut controller) {
                    Ok(true) => {
                        command_deadline = None;
                        sync.invalidate();
                    }
                    Ok(false) => {}
                    Err(error) => {
                        command_deadline = None;
                        eprintln!("sidebar control: {error}");
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        if command_deadline.is_some_and(|deadline| Instant::now() < deadline) {
            thread::sleep(ipc::SYNC_CHECK_INTERVAL.saturating_sub(started.elapsed()));
            continue;
        }
        command_deadline = None;
        if settings_at.elapsed() >= SETTINGS_POLL {
            settings_at = Instant::now();
            match config::load() {
                Ok(updated) if updated != settings => {
                    settings = updated;
                    sync.invalidate();
                }
                Ok(_) => {}
                Err(error) => eprintln!("sidebar settings: {error}"),
            }
        }
        publication.lock()?;
        let refreshed = match sync.poll() {
            Ok(Some(snapshot)) => Some((|| {
                lost_at = None;
                animation_session = None;
                if plugin_check_at.elapsed() >= Duration::from_secs(5) {
                    plugin_check_at = Instant::now();
                    let registry = snapshot.session.call("plugin.list", json!({}))?;
                    let enabled = registry["plugins"]
                        .as_array()
                        .ok_or_else(|| io::Error::other("Herdr omitted plugin registry"))?
                        .iter()
                        .any(|plugin| {
                            plugin["plugin_id"] == "herdr-project-sidebar"
                                && plugin["enabled"] == true
                        });
                    if !enabled {
                        let unconfigured = config::unconfigure();
                        settings.enabled = false;
                        sync.invalidate();
                        unconfigured?;
                    }
                }
                let result = refresh_daemon(&mut publisher, &mut controller, &snapshot, &settings);
                if result.is_ok() {
                    animation_session = Some(snapshot.session);
                    animation_at = Instant::now();
                }
                result
            })()),
            Ok(None) => None,
            Err(error) => {
                animation_session = None;
                Some(Err(error))
            }
        };
        let refreshed = if refreshed.is_none()
            && !sync.is_stale()
            && animation_at.elapsed() >= Duration::from_millis(SPIN_MS)
        {
            animation_at = Instant::now();
            animation_session.as_ref().map(|session| {
                // EXPERIMENT (remote-lag): freeze timer updates as well as snapshots.
                // publisher.animate(session, (now_unix_ms() / SPIN_MS) as usize).map(|_| true)
                publisher.animate(session, 0).map(|_| true)
            })
        } else {
            refreshed
        };
        publication.unlock()?;
        if let Some(refreshed) = refreshed {
            if first {
                notify_start(
                    &refreshed
                        .as_ref()
                        .map(|_| ())
                        .map_err(|error| io::Error::other(error.to_string())),
                )?;
                first = false;
            }
            match refreshed {
                Ok(false) => return Ok(()),
                Ok(true) => {
                    lost_at = None;
                    last_refresh_error = None;
                }
                Err(error) => {
                    let message = error.to_string();
                    if last_refresh_error.as_ref() != Some(&message) {
                        eprintln!("sidebar refresh: {message}");
                        last_refresh_error = Some(message);
                        sync.invalidate();
                    }
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) {
                        let lost = lost_at.get_or_insert_with(Instant::now);
                        if lost.elapsed() >= RECOVERY_GRACE {
                            return Ok(());
                        }
                    } else {
                        lost_at = None;
                    }
                }
            }
        }
        thread::sleep(ipc::SYNC_CHECK_INTERVAL.saturating_sub(started.elapsed()));
    }
}

fn refresh_daemon(
    publisher: &mut Publisher,
    controller: &mut dock_control::Controller,
    snapshot: &ipc::Snapshot,
    settings: &config::Settings,
) -> io::Result<bool> {
    // Both consumers run even if one fails; dock errors must not halt metadata.
    let published = publisher.refresh(snapshot, settings);
    let reconciled = controller.reconcile(snapshot, settings);
    reconciled?;
    published?;
    Ok(settings.enabled)
}

fn entries<'a>(snapshot: &'a Value, key: &str) -> io::Result<&'a [Value]> {
    snapshot
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("session.snapshot omitted {key}"),
            )
        })
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("").trim()
}

fn empty_tokens(keys: &[&str]) -> Tokens {
    keys.iter()
        .map(|key| ((*key).to_owned(), Value::Null))
        .collect()
}

fn retain_delta(tokens: &mut Tokens, live: Option<&Value>) {
    tokens.retain(|key, value| {
        if let Value::String(text) = value {
            // Match Herdr 0.9.1 metadata normalization, in place: trim, remove
            // controls, limit to 80 Unicode characters, then trim again.
            let leading = text.len() - text.trim_start().len();
            text.drain(..leading);
            text.truncate(text.trim_end().len());
            let mut remaining = 80;
            text.retain(|ch| {
                if ch.is_control() || remaining == 0 {
                    return false;
                }
                remaining -= 1;
                true
            });
            let leading = text.len() - text.trim_start().len();
            text.drain(..leading);
            text.truncate(text.trim_end().len());
            if text.is_empty() {
                *value = Value::Null;
            }
        }
        live.and_then(|tokens| tokens.get(key))
            .unwrap_or(&Value::Null)
            != &*value
    });
}

fn chunk_tokens(tokens: Tokens, limit: usize) -> Vec<Tokens> {
    let mut chunks = Vec::new();
    let mut current = Map::new();
    for (k, v) in tokens {
        current.insert(k, v);
        if current.len() >= limit {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn patch(session: &ipc::Session, pane: &str, tokens: Tokens) -> io::Result<()> {
    for chunk in chunk_tokens(tokens, 16) {
        session.call(
            "pane.report_metadata",
            json!({"pane_id": pane, "source": SOURCE, "tokens": chunk}),
        )?;
    }
    Ok(())
}

fn patch_workspace(session: &ipc::Session, workspace: &str, tokens: Tokens) -> io::Result<()> {
    for chunk in chunk_tokens(tokens, 16) {
        session.call(
            "workspace.report_metadata",
            json!({"workspace_id": workspace, "source": SOURCE, "tokens": chunk}),
        )?;
    }
    Ok(())
}

fn publication_lock() -> io::Result<fs::File> {
    let directory = config::state_dir();
    fs::create_dir_all(&directory)?;
    let hash = ipc::socket_hash(&socket_path()?);
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(format!("p-{hash:016x}.lock")))
}

pub fn refresh() -> io::Result<()> {
    // Serialize snapshot acquisition too: an older concurrent --refresh must
    // not publish metadata or a live-terminal set after a newer daemon refresh.
    let publication = publication_lock()?;
    publication.lock()?;
    let settings = config::load()?;
    let snapshot = ipc::session_snapshot()?;
    Publisher::default().refresh(&snapshot, &settings)
}

pub fn clear() -> io::Result<()> {
    // Use the controller only when no daemon owns it; keep daemon -> publication
    // lock order identical to run_inner so cleanup cannot race a live owner.
    let (lock_path, ready_path) = daemon_paths()?;
    let daemon = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let own_controller = match daemon.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    };
    let publication = publication_lock()?;
    publication.lock()?;
    let snapshot = ipc::session_snapshot()?;
    let cleared = clear_snapshot(&snapshot);
    if own_controller {
        let settings = config::load()?;
        if !settings.enabled {
            dock_control::Controller::new(ready_path.with_extension("json"))?
                .reconcile(&snapshot, &settings)?;
        }
    }
    cleared
}

fn clear_snapshot(snapshot: &ipc::Snapshot) -> io::Result<()> {
    let session = &snapshot.session;
    session.check()?;
    let panes = entries(&snapshot.data, "panes")?;
    let workspaces = entries(&snapshot.data, "workspaces")?;
    let mut failure = None;
    for pane in panes {
        if KEYS.iter().any(|key| pane["tokens"].get(*key).is_some()) {
            if let Err(error) = patch(session, text(pane, "pane_id"), empty_tokens(&KEYS)) {
                failure = Some(error);
            }
        }
    }
    for workspace in workspaces {
        if SPACE_KEYS
            .iter()
            .any(|key| workspace["tokens"].get(*key).is_some())
        {
            if let Err(error) = patch_workspace(
                session,
                text(workspace, "workspace_id"),
                empty_tokens(&SPACE_KEYS),
            ) {
                failure = Some(error);
            }
        }
    }
    if let Err(error) = session.call("agent.view.clear", json!({"source": SOURCE})) {
        failure = Some(error);
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub struct Publisher {
    pub activity: ActivityStore,
    animated: BTreeMap<String, (&'static str, String)>,
    grouped: Option<bool>,
    native_style: bool,
}

impl Default for Publisher {
    fn default() -> Self {
        Self {
            activity: ActivityStore::new(config::state_dir()),
            animated: BTreeMap::new(),
            grouped: None,
            native_style: false,
        }
    }
}

impl Publisher {
    fn animate(&mut self, session: &ipc::Session, step: usize) -> io::Result<()> {
        for (pane, (key, title)) in &mut self.animated {
            let (previous, text) = title
                .split_once(' ')
                .ok_or_else(|| io::Error::other("invalid animated title"))?;
            let mark = if *key == "title_working" {
                icons::spinner(step)
            } else {
                icons::blocked_mark(step)
            };
            if mark == previous {
                continue;
            }
            let next = format!("{mark} {text}");
            patch(
                session,
                pane,
                Map::from_iter([((*key).to_owned(), json!(next))]),
            )?;
            *title = next;
        }
        Ok(())
    }

    pub fn refresh(
        &mut self,
        snapshot: &ipc::Snapshot,
        settings: &config::Settings,
    ) -> io::Result<()> {
        let session = &snapshot.session;
        self.animated.clear();
        let agents = entries(&snapshot.data, "agents")?;
        let workspaces = entries(&snapshot.data, "workspaces")?;
        let tabs = entries(&snapshot.data, "tabs")?;
        let panes = entries(&snapshot.data, "panes")?;
        if agents
            .iter()
            .any(|agent| text(agent, "terminal_id").is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot agent is missing terminal_id",
            ));
        }
        if self.activity.sync_session(session)? {
            self.grouped = None;
            self.native_style = false;
        }
        let now_ms = now_unix_ms();
        for agent in agents {
            if text(agent, "agent_status") == "working" {
                self.activity
                    .mark_working(text(agent, "terminal_id"), now_ms);
            }
        }
        let live: HashSet<&str> = agents
            .iter()
            .map(|agent| text(agent, "terminal_id"))
            .collect();
        self.activity.retain_live(&live);
        self.activity.save(!settings.enabled)?;
        if !settings.enabled {
            return clear_snapshot(snapshot);
        }
        if !settings.project_style {
            if !self.native_style {
                clear_snapshot(snapshot)?;
                self.grouped = None;
                self.native_style = true;
            }
            return Ok(());
        }
        self.native_style = false;

        // EXPERIMENT (remote-lag): static spinner mark; revert to the line below.
        let spin_step = 0;
        // let spin_step = (now_ms / SPIN_MS) as usize;

        // 2. Generate row tokens
        let (wanted, wanted_workspaces) = rows(&RowsInput {
            agents,
            workspaces,
            tabs,
            panes,
            settings,
            activity: &self.activity,
            now_ms,
            spin_step,
        });

        // 3. Patch panes with chunked writes. Deltas compare against the live
        // snapshot tokens, never a private cache: external writes must not
        // leave stale beliefs installed.
        for pane in panes {
            let id = text(pane, "pane_id");
            if !wanted.contains_key(id) && KEYS.iter().any(|key| pane["tokens"].get(*key).is_some())
            {
                patch(session, id, empty_tokens(&KEYS))?;
            }
        }
        for (pane, mut tokens) in wanted {
            for key in ["title_working", "title_blocked"] {
                if let Some(title) = tokens[key].as_str() {
                    self.animated.insert(pane.clone(), (key, title.to_owned()));
                }
            }
            let live = panes.iter().find(|p| text(p, "pane_id") == pane);
            retain_delta(&mut tokens, live.map(|p| &p["tokens"]));
            patch(session, &pane, tokens)?;
        }

        // 4. Patch workspaces (same live-token rule as panes).
        for (workspace, mut tokens) in wanted_workspaces {
            let live = workspaces
                .iter()
                .find(|w| text(w, "workspace_id") == workspace);
            retain_delta(&mut tokens, live.map(|w| &w["tokens"]));
            patch_workspace(session, &workspace, tokens)?;
        }

        // 5. Update sort override
        if self.grouped != Some(settings.grouped) {
            let sort = if settings.grouped {
                json!([{"field": {"token": "ws_group"}, "order": "asc"}, {"field": "state_change_seq", "order": "desc"}, {"field": "tab_order", "order": "asc"}, {"field": "pane_order", "order": "asc"}])
            } else {
                json!([{"field": "state_change_seq", "order": "desc"}, {"field": "tab_order", "order": "asc"}, {"field": "pane_order", "order": "asc"}])
            };
            session.call(
                "agent.view.set",
                json!({"source": SOURCE, "label": if settings.grouped { "active" } else { "recent" }, "sort": sort}),
            )?;
            self.grouped = Some(settings.grouped);
        }

        Ok(())
    }
}

struct Workspace {
    id: String,
    group: String,
    project: String,
    label: String,
    branch: Option<String>,
    linked: bool,
    order: u64,
}

pub struct RowsInput<'a> {
    pub agents: &'a [Value],
    pub workspaces: &'a [Value],
    pub tabs: &'a [Value],
    pub panes: &'a [Value],
    pub settings: &'a config::Settings,
    pub activity: &'a ActivityStore,
    pub now_ms: u64,
    pub spin_step: usize,
}

pub fn rows(input: &RowsInput) -> (RowTokens, RowTokens) {
    let RowsInput {
        agents,
        workspaces,
        tabs,
        panes,
        settings,
        activity,
        now_ms,
        spin_step,
    } = *input;
    let mut cwd = HashMap::new();
    let mut ws_agents: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();

    for agent in agents {
        let path = if text(agent, "foreground_cwd").is_empty() {
            text(agent, "cwd")
        } else {
            text(agent, "foreground_cwd")
        };
        cwd.entry(text(agent, "workspace_id")).or_insert(path);
        let ws = text(agent, "workspace_id");
        ws_agents
            .entry(ws)
            .or_default()
            .push((text(agent, "agent"), text(agent, "agent_status")));
    }

    let mut spaces: HashMap<String, Workspace> = workspaces
        .iter()
        .enumerate()
        .map(|(index, ws)| {
            let id = text(ws, "workspace_id").to_owned();
            let tree = &ws["worktree"];
            let label = if text(ws, "label").is_empty() {
                id.clone()
            } else {
                text(ws, "label").to_owned()
            };
            let repo = text(tree, "repo_key");
            let project = if text(tree, "repo_name").is_empty() {
                label.clone()
            } else {
                text(tree, "repo_name").to_owned()
            };
            let checkout = text(tree, "checkout_path");
            let branch = if !settings.show_branch {
                None
            } else {
                git_branch(Path::new(if checkout.is_empty() {
                    cwd.get(id.as_str()).copied().unwrap_or("")
                } else {
                    checkout
                }))
            };
            (
                id.clone(),
                Workspace {
                    group: if repo.is_empty() {
                        format!("workspace:{id}")
                    } else {
                        format!("repo:{repo}")
                    },
                    id,
                    project,
                    label,
                    branch,
                    linked: tree["is_linked_worktree"].as_bool().unwrap_or(false),
                    order: ws["number"].as_u64().unwrap_or(index as u64),
                },
            )
        })
        .collect();

    for agent in agents {
        let id = text(agent, "workspace_id");
        spaces.entry(id.to_owned()).or_insert_with(|| Workspace {
            id: id.to_owned(),
            group: format!("workspace:{id}"),
            project: id.to_owned(),
            label: id.to_owned(),
            branch: if settings.show_branch {
                git_branch(Path::new(cwd.get(id).copied().unwrap_or("")))
            } else {
                None
            },
            linked: false,
            order: u64::MAX,
        });
    }

    // Build workspace tokens
    let mut workspace_rows = BTreeMap::new();
    for ws in workspaces {
        let ws_id = text(ws, "workspace_id");
        let label = text(ws, "label");
        let label = if label.is_empty() { ws_id } else { label };
        let mut tokens = empty_tokens(&SPACE_KEYS);

        // Rollup is native: WorkspaceInfo.agent_status/label rule. Vendor
        // only picks the suffixed icon key our own template keys on.
        let members = ws_agents.get(ws_id).map(Vec::as_slice).unwrap_or(&[]);
        let chosen = text(ws, "agent_status");
        let chosen = if chosen.is_empty() { "none" } else { chosen };
        let vendor = members
            .iter()
            .find(|&&(_, s)| s == chosen)
            .map(|&(v, _)| v)
            .unwrap_or("other");

        let mark_token = match chosen {
            "blocked" => "space_blocked".into(),
            "working" => match vendor {
                "claude" | "gemini" | "kimi" | "qwen" | "kiro" | "cline" | "kilo" => {
                    format!("space_working_{vendor}")
                }
                _ => "space_working_other".into(),
            },
            "done" => "space_done".into(),
            "idle" => "space_idle".into(),
            "unknown" => "space_unknown".into(),
            _ => "space_none".into(),
        };
        tokens.insert(mark_token, json!(icons::state_mark(chosen)));
        tokens.insert("space_label".into(), json!(label));

        // Space logos
        let mut seen_logos = HashSet::new();
        for &(v, _) in members {
            if seen_logos.insert(v) {
                let logo_key = match v {
                    "claude" | "gemini" | "kimi" | "qwen" | "kiro" | "cline" | "kilo" => {
                        format!("space_logo_{v}")
                    }
                    _ => "space_logo_other".into(),
                };
                let glyph = icons::logo(v, settings.icons).unwrap_or("");
                let text = if glyph.is_empty() {
                    v.to_owned()
                } else {
                    format!("{glyph} {v}")
                };
                tokens.insert(logo_key, json!(text));
            }
        }
        workspace_rows.insert(ws_id.to_owned(), tokens);
    }

    // Agent ordering
    let tab_order: HashMap<_, _> = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            (
                text(tab, "tab_id"),
                tab["number"].as_u64().unwrap_or(index as u64),
            )
        })
        .collect();
    let pane_order: HashMap<_, _> = panes
        .iter()
        .enumerate()
        .map(|(index, pane)| (text(pane, "pane_id"), index))
        .collect();
    let mut ordered: Vec<_> = agents
        .iter()
        .filter(|agent| !text(agent, "pane_id").is_empty())
        .collect();

    // Native recency: state_change_seq orders turns; local clocks never do.
    fn seq(agent: &Value) -> u64 {
        agent
            .get("state_change_seq")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }
    // Group boundaries must use the same group-first order as agent.view.set.
    ordered.sort_by(|a, b| {
        let wa = &spaces[text(a, "workspace_id")];
        let wb = &spaces[text(b, "workspace_id")];
        (
            settings.grouped.then_some(&wa.group),
            seq(b),
            wa.linked,
            wa.order,
            &wa.id,
            tab_order
                .get(text(a, "tab_id"))
                .copied()
                .unwrap_or(u64::MAX),
            pane_order
                .get(text(a, "pane_id"))
                .copied()
                .unwrap_or(usize::MAX),
            text(a, "pane_id"),
        )
            .cmp(&(
                settings.grouped.then_some(&wb.group),
                seq(a),
                wb.linked,
                wb.order,
                &wb.id,
                tab_order
                    .get(text(b, "tab_id"))
                    .copied()
                    .unwrap_or(u64::MAX),
                pane_order
                    .get(text(b, "pane_id"))
                    .copied()
                    .unwrap_or(usize::MAX),
                text(b, "pane_id"),
            ))
    });

    let mut seen_groups = HashSet::new();
    let mut result = BTreeMap::new();

    for (index, agent) in ordered.iter().enumerate() {
        let pane_id = text(agent, "pane_id");
        let ws = &spaces[text(agent, "workspace_id")];
        let status = text(agent, "agent_status");
        let mut tokens = empty_tokens(&KEYS);

        // Resolve display state
        let display = if status == "working" {
            "working"
        } else if status == "blocked" {
            "blocked"
        } else if status == "done" {
            "done"
        } else if status == "idle" {
            match activity.freshness(text(agent, "terminal_id"), now_ms) {
                Freshness::Fresh => "idle_fresh",
                Freshness::Stale => "idle_stale",
                Freshness::Normal => "idle",
            }
        } else {
            status
        };

        // Group header
        let is_head = seen_groups.insert(&ws.group);
        if settings.grouped {
            if is_head {
                let header_text = if ws.linked {
                    format!("└─ {}", ws.branch.as_deref().unwrap_or(&ws.label))
                } else {
                    ws.project.clone()
                };
                tokens.insert("group".into(), json!(header_text));
            }
            if ordered
                .get(index + 1)
                .is_some_and(|next| spaces[text(next, "workspace_id")].group != ws.group)
            {
                tokens.insert("gap".into(), json!("\u{200b}"));
            }
        }

        // Indent & Logo
        let indent = if settings.grouped && !is_head {
            "\u{200b}  "
        } else {
            ""
        };
        let vendor = text(agent, "agent");
        let logo_glyph = icons::logo(vendor, settings.icons);
        if let Some(glyph) = logo_glyph {
            tokens.insert("harness_logo".into(), json!(glyph));
        }
        if settings.icons != config::IconMode::None {
            let logo_str = format!("{indent}{}", logo_glyph.unwrap_or(vendor));
            match display {
                "working" => tokens.insert("logo_working".into(), json!(logo_str)),
                "idle_stale" => tokens.insert("logo_stale".into(), json!(logo_str)),
                _ => tokens.insert("logo".into(), json!(logo_str)),
            };
        }

        // Title with animated lead
        let raw_title = agent_title(agent, settings.show_title);
        let title_prefix = match display {
            "working" => format!("{} ", icons::spinner(spin_step)),
            "blocked" => format!("{} ", icons::blocked_mark(spin_step)),
            "done" => "✓ ".into(),
            "unknown" => "◌ ".into(),
            _ => "".into(),
        };
        let full_title = if settings.grouped {
            format!("{title_prefix}{raw_title}")
        } else {
            format!("{title_prefix}{} · {raw_title}", ws.label)
        };

        let title_token = match display {
            "working" => "title_working",
            "done" => "title_done",
            "blocked" => "title_blocked",
            "idle_fresh" => "title_idle_fresh",
            "idle_stale" => "title_idle_stale",
            "idle" => "title_idle",
            _ => "title_unknown",
        };
        tokens.insert(title_token.into(), json!(full_title));

        // Group token for the declarative native sort (recency itself is
        // the native state_change_seq field, not a local clock).
        tokens.insert("ws_group".into(), json!(ws.group));

        result.insert(pane_id.to_owned(), tokens);
    }

    (result, workspace_rows)
}

fn agent_title(agent: &Value, show_title: bool) -> &str {
    let name = text(agent, "agent");
    if !show_title {
        return name;
    }
    let supplied = text(agent, "title");
    if !supplied.is_empty() {
        return supplied;
    }
    let title = text(agent, "terminal_title_stripped");
    if title.is_empty() {
        return name;
    }
    let title = if matches!(name, "omp" | "pi") {
        title
            .strip_prefix("π ")
            .unwrap_or(title)
            .trim_start_matches(|ch: char| {
                ch.is_whitespace() || ch == '>' || ('\u{2800}'..='\u{28ff}').contains(&ch)
            })
    } else {
        title
    };
    if title.is_empty() {
        name
    } else {
        title
    }
}

pub fn git_branch(start: &Path) -> Option<String> {
    if !start.is_absolute() {
        return None;
    }
    for dir in start.ancestors() {
        let marker = dir.join(".git");
        let git = if marker.is_dir() {
            marker
        } else if marker.is_file() {
            let contents = fs::read_to_string(marker).ok()?;
            dir.join(contents.trim().strip_prefix("gitdir:")?.trim())
        } else {
            continue;
        };
        let head = fs::read_to_string(git.join("HEAD")).ok()?;
        let head = head.trim();
        if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
            return Some(branch.to_owned());
        }
        return (head.len() >= 7 && head.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| head[..7].to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn group_boundaries_follow_native_sort_with_interleaved_recency() {
        let agents = vec![
            json!({"pane_id":"a-new", "workspace_id":"a", "agent":"omp", "agent_status":"idle", "state_change_seq":40}),
            json!({"pane_id":"muse", "workspace_id":"b", "agent":"muse", "agent_status":"idle", "terminal_title_stripped":"muse-bridge", "state_change_seq":30}),
            json!({"pane_id":"a-old", "workspace_id":"a", "agent":"omp", "agent_status":"idle", "state_change_seq":20}),
            json!({"pane_id":"b-old", "workspace_id":"b", "agent":"omp", "agent_status":"idle", "state_change_seq":10}),
        ];
        let workspaces = vec![
            json!({"workspace_id":"a", "label":"project"}),
            json!({"workspace_id":"b", "label":"muse-bridge"}),
        ];
        let settings = config::Settings {
            show_branch: false,
            ..config::Settings::default()
        };
        let (panes, _) = rows(&RowsInput {
            agents: &agents,
            workspaces: &workspaces,
            tabs: &[],
            panes: &[],
            settings: &settings,
            activity: &ActivityStore::default(),
            now_ms: 0,
            spin_step: 0,
        });

        assert_eq!(panes["a-new"]["gap"], Value::Null);
        assert_eq!(panes["a-old"]["gap"], json!("\u{200b}"));
        assert_eq!(panes["muse"]["gap"], Value::Null);
        assert_eq!(panes["b-old"]["gap"], Value::Null);
        assert_eq!(panes["a-old"]["group"], Value::Null);
        assert_eq!(panes["b-old"]["group"], Value::Null);
        assert_eq!(panes["muse"]["logo"], "muse");
        assert_eq!(panes["muse"]["title_idle"], "muse-bridge");
    }

    #[test]
    fn metadata_delta_does_not_repeat_cleared_tokens() {
        let mut tokens = json!({
            "missing": null, "same": "label", "obsolete": null, "changed": "new"
        })
        .as_object()
        .unwrap()
        .clone();
        retain_delta(
            &mut tokens,
            Some(&json!({"same": "label", "obsolete": "old", "changed": "old"})),
        );
        assert_eq!(json!(tokens), json!({"obsolete": null, "changed": "new"}));
        retain_delta(
            &mut tokens,
            Some(&json!({"same": "label", "changed": "new"})),
        );
        assert!(tokens.is_empty());
    }

    #[test]
    fn metadata_delta_converges_after_wire_normalization() {
        let wanted = json!({
            "title_working": format!("⣷ {}", "文".repeat(80)),
            "group": "  pro\u{0000}ject\n ",
            "empty": "\u{0000} \n",
            "boundary": format!("{} suffix", "x".repeat(79))
        })
        .as_object()
        .unwrap()
        .clone();
        let mut tokens = wanted.clone();
        retain_delta(&mut tokens, None);
        let mut live = json!({
            "title_working": format!("⣷ {}", "文".repeat(78)),
            "group": "project",
            "boundary": "x".repeat(79)
        });
        assert_eq!(json!(tokens), live);
        let mut tokens = wanted.clone();
        retain_delta(&mut tokens, Some(&live));
        assert!(tokens.is_empty());
        live["title_working"] = json!("externally changed");
        let mut tokens = wanted;
        retain_delta(&mut tokens, Some(&live));
        assert_eq!(
            json!(tokens),
            json!({"title_working": format!("⣷ {}", "文".repeat(78))})
        );
    }

    #[test]
    fn animation_patches_only_changed_titles_without_fetching_snapshots() {
        let socket =
            std::env::temp_dir().join(format!("hps-animation-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket).unwrap();
        let _socket_file = SocketFile(socket.clone());
        listener.set_nonblocking(true).unwrap();
        let session = ipc::Session::at(socket).unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && requests.len() < 3 {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                writeln!(stream, "{}", json!({"id":request["id"],"result":{}})).unwrap();
                requests.push(request);
            }
            requests
        });
        let mut publisher = Publisher::default();
        publisher.animated.insert(
            "working".into(),
            ("title_working", format!("{} build", icons::spinner(0))),
        );
        publisher
            .animated
            .insert("blocked".into(), ("title_blocked", "? approval".into()));
        publisher.animate(&session, 0).unwrap();
        publisher.animate(&session, 1).unwrap();
        publisher.animate(&session, 3).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| request["method"] == "pane.report_metadata"));
        assert!(requests.iter().all(|request| request["params"]["tokens"]
            .as_object()
            .unwrap()
            .len()
            == 1));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["params"]["pane_id"] == "blocked")
                .count(),
            1
        );
        assert_eq!(publisher.animated["blocked"].1, "· approval");
        assert_eq!(
            publisher.animated["working"].1,
            format!("{} build", icons::spinner(3))
        );
    }

    #[test]
    fn branch_lookup_handles_relative_gitdirs_and_detached_heads() {
        let root = std::env::temp_dir().join(format!("hps-head-{}", std::process::id()));
        fs::create_dir_all(root.join("checkout/nested")).unwrap();
        fs::create_dir_all(root.join("git")).unwrap();
        fs::write(root.join("checkout/.git"), "gitdir: ../git\n").unwrap();
        fs::write(root.join("git/HEAD"), "ref: refs/heads/topic/branch\n").unwrap();
        assert_eq!(
            git_branch(&root.join("checkout/nested")).as_deref(),
            Some("topic/branch")
        );
        fs::write(root.join("git/HEAD"), "abcdef0123456789\n").unwrap();
        assert_eq!(
            git_branch(&root.join("checkout/nested")).as_deref(),
            Some("abcdef0")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_done_follows_authoritative_status() {
        let activity = ActivityStore::default();
        let agents = vec![
            json!({"terminal_id": "t1", "pane_id": "p1", "workspace_id": "w1", "agent": "claude", "agent_status": "idle", "focused": false}),
            json!({"terminal_id": "t2", "pane_id": "p2", "workspace_id": "w1", "agent": "claude", "agent_status": "done", "focused": false}),
        ];
        let workspaces = vec![json!({"workspace_id": "w1", "label": "demo"})];
        let settings = config::Settings::default();

        let (panes, _) = rows(&RowsInput {
            agents: &agents,
            workspaces: &workspaces,
            tabs: &[],
            panes: &[],
            settings: &settings,
            activity: &activity,
            now_ms: 1000,
            spin_step: 0,
        });
        // No hold: idle renders idle even with no focus event since done.
        assert!(panes["p1"]["title_idle"].is_string());
        assert_eq!(panes["p1"]["title_done"], Value::Null);
        assert!(panes["p2"]["title_done"].is_string());
    }

    #[test]
    fn freshness_follows_terminal_when_pane_address_is_reused() {
        let mut activity = ActivityStore::default();
        activity.mark_working("terminal-a", 1_000);
        let mut agents = vec![
            json!({"terminal_id": "terminal-a", "pane_id": "old", "workspace_id": "workspace", "agent": "claude", "agent_status": "idle"}),
        ];
        let workspaces = vec![json!({"workspace_id": "workspace", "label": "demo"})];
        let settings = config::Settings {
            show_branch: false,
            ..config::Settings::default()
        };
        let render = |agents: &[Value]| {
            rows(&RowsInput {
                agents,
                workspaces: &workspaces,
                tabs: &[],
                panes: &[],
                settings: &settings,
                activity: &activity,
                now_ms: 2_000,
                spin_step: 0,
            })
            .0
        };
        assert!(render(&agents)["old"]["title_idle_fresh"].is_string());
        agents[0]["pane_id"] = json!("new");
        agents.push(json!({"terminal_id": "terminal-b", "pane_id": "old", "workspace_id": "workspace", "agent": "claude", "agent_status": "idle"}));
        let remapped = render(&agents);
        assert!(remapped["new"]["title_idle_fresh"].is_string());
        assert!(remapped["old"]["title_idle"].is_string());
        assert_eq!(remapped["old"]["title_idle_fresh"], Value::Null);
    }

    #[test]
    fn malformed_agent_identity_cannot_prune_valid_history() {
        let mut publisher = Publisher::default();
        publisher.activity.mark_working("valid-terminal", 1_000);
        let socket = std::env::temp_dir().join(format!(
            "hps-malformed-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _listener = UnixListener::bind(&socket).unwrap();
        let _socket_file = SocketFile(socket.clone());
        let snapshot = json!({
            "agents": [
                {"terminal_id": "valid-terminal", "pane_id": "p1", "agent_status": "working"},
                {"pane_id": "p2", "agent_status": "working"}
            ],
            "workspaces": [],
            "tabs": [],
            "panes": []
        });
        let snapshot = ipc::Snapshot {
            session: ipc::Session::at(socket).unwrap(),
            data: snapshot,
        };
        assert_eq!(
            publisher
                .refresh(&snapshot, &config::Settings::default())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            publisher.activity.stamps,
            HashMap::from([("valid-terminal".to_owned(), 1_000)])
        );
    }
}

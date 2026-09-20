//! Bounded Herdr IPC and change-driven, authoritative snapshot reconciliation.
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub fn call(method: &str, params: Value) -> io::Result<Value> {
    call_at(&socket_path()?, method, params)
}

fn socket_path() -> io::Result<PathBuf> {
    std::env::var_os("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HERDR_SOCKET_PATH is not set"))
}

fn call_at(socket: &Path, method: &str, params: Value) -> io::Result<Value> {
    request(UnixStream::connect(socket)?, method, params)
}

fn request(mut stream: UnixStream, method: &str, params: Value) -> io::Result<Value> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let id = format!("herdr-project-sidebar:{method}");
    let request = json!({"id": id, "method": method, "params": params});
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    read_response(stream, &id)
}

fn read_response(stream: impl Read, id: &str) -> io::Result<Value> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    let mut line = String::new();
    BufReader::new(stream.take(LIMIT)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated Herdr response",
        ));
    }
    let mut response: Value = serde_json::from_str(&line)?;
    if response.get("id").and_then(Value::as_str) != Some(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected Herdr response id",
        ));
    }
    if let Some(error) = response.get("error") {
        return Err(io::Error::other(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Herdr request failed")
                .to_owned(),
        ));
    }
    response
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Herdr result"))
}

fn session_key_at(socket: &Path) -> io::Result<String> {
    let metadata = fs::metadata(socket)?;
    Ok(format!(
        "{:x}-{:x}-{:x}-{:x}",
        metadata.dev(),
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec()
    ))
}

/// A concrete server incarnation. Keep it with the data used to choose targets.
#[derive(Clone, Debug)]
pub struct Session {
    socket: PathBuf,
    key: String,
}

impl Session {
    pub fn current() -> io::Result<Self> {
        Self::at(socket_path()?)
    }

    pub fn at(socket: PathBuf) -> io::Result<Self> {
        let key = session_key_at(&socket)?;
        Ok(Self { socket, key })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn socket_hash(&self) -> u64 {
        socket_hash(&self.socket)
    }

    pub fn check(&self) -> io::Result<()> {
        if session_key_at(&self.socket)? != self.key {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Herdr session changed",
            ));
        }
        Ok(())
    }

    pub fn call(&self, method: &str, params: Value) -> io::Result<Value> {
        self.check()?;
        let stream = UnixStream::connect(&self.socket)?;
        self.check()?;
        // A later pathname replacement cannot redirect this connected stream.
        request(stream, method, params)
    }
}

pub fn socket_hash(socket: &Path) -> u64 {
    socket
        .canonicalize()
        .unwrap_or_else(|_| socket.to_owned())
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

#[derive(Debug)]
pub struct Snapshot {
    pub session: Session,
    pub data: Value,
}

pub fn session_snapshot() -> io::Result<Snapshot> {
    snapshot_at(&socket_path()?)
}

fn snapshot_at(socket: &Path) -> io::Result<Snapshot> {
    let session = Session::at(socket.to_owned())?;
    let mut result = session.call("session.snapshot", json!({}))?;
    let snapshot = result
        .get_mut("snapshot")
        .map(Value::take)
        .filter(Value::is_object)
        .ok_or_else(|| invalid("Herdr omitted the session snapshot"))?;
    for key in ["agents", "workspaces", "tabs", "panes", "layouts"] {
        if !snapshot[key].is_array() {
            return Err(invalid(format!("Herdr snapshot omitted {key}")));
        }
    }
    for agent in snapshot["agents"].as_array().unwrap() {
        for key in ["terminal_id", "pane_id", "tab_id", "workspace_id"] {
            if agent[key].as_str().is_none_or(|id| id.trim().is_empty()) {
                return Err(invalid(format!("Herdr agent omitted {key}")));
            }
        }
    }
    session.check()?;
    Ok(Snapshot {
        session,
        data: snapshot,
    })
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub const SYNC_CHECK_INTERVAL: Duration = Duration::from_millis(50);
const SNAPSHOT_FLOOR: Duration = Duration::from_millis(300);
const RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_LIMIT: usize = 4 * 1024 * 1024;
const SUBSCRIPTION_ID: &str = "herdr-project-sidebar:events";
const CHANGES: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.closed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "layout.updated",
];

struct Events {
    reader: BufReader<UnixStream>,
    pending: Vec<u8>,
}

impl Events {
    fn connect(socket: &Path, agents: &BTreeSet<String>) -> io::Result<Self> {
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let subscriptions: Vec<Value> = CHANGES
            .iter()
            .map(|kind| json!({"type":kind}))
            .chain(
                agents
                    .iter()
                    .map(|pane| json!({"type":"pane.agent_status_changed", "pane_id":pane})),
            )
            .collect();
        serde_json::to_writer(
            &mut stream,
            &json!({
                "id":SUBSCRIPTION_ID, "method":"events.subscribe",
                "params":{"subscriptions":subscriptions}
            }),
        )?;
        stream.write_all(b"\n")?;
        let mut reader = BufReader::new(stream);
        let mut ack = Vec::new();
        (&mut reader)
            .take(EVENT_LIMIT as u64)
            .read_until(b'\n', &mut ack)?;
        // Preserve buffered events following the acknowledgment.
        read_response(ack.as_slice(), SUBSCRIPTION_ID)?;
        reader.get_mut().set_nonblocking(true)?;
        Ok(Self {
            reader,
            pending: Vec::new(),
        })
    }

    fn drain(&mut self) -> io::Result<bool> {
        let mut changed = false;
        let mut bytes = [0u8; 8192];
        // A busy event producer cannot monopolize input or command handling.
        for _ in 0..8 {
            match self.reader.read(&mut bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Herdr event connection closed",
                    ))
                }
                Ok(count) => {
                    changed = true;
                    self.pending.extend_from_slice(&bytes[..count]);
                    if self.pending.len() > EVENT_LIMIT {
                        return Err(invalid("Herdr event exceeds the size limit"));
                    }
                    let mut consumed = 0;
                    while let Some(end) = self.pending[consumed..].iter().position(|b| *b == b'\n')
                    {
                        let end = consumed + end + 1;
                        let event: Value = serde_json::from_slice(&self.pending[consumed..end])?;
                        if event["event"].as_str().is_none_or(str::is_empty)
                            || !event["data"].is_object()
                        {
                            return Err(invalid("invalid Herdr event envelope"));
                        }
                        consumed = end;
                    }
                    self.pending.drain(..consumed);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(changed)
    }
}

/// Events invalidate a view; they never patch it. Subscribe before fetching a
/// baseline, serialize all reads, and periodically repair missed notifications.
/// Herdr's public API has no snapshot revision or event replay cursor.
pub struct SnapshotSync {
    socket: PathBuf,
    session: Option<String>,
    events: Option<Events>,
    agents: BTreeSet<String>,
    subscribed: BTreeSet<String>,
    dirty: bool,
    ready: bool,
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    retry_at: Option<Instant>,
}

impl SnapshotSync {
    pub fn new() -> Self {
        Self::at(socket_path().unwrap_or_default())
    }

    fn at(socket: PathBuf) -> Self {
        Self {
            socket,
            session: None,
            events: None,
            agents: BTreeSet::new(),
            subscribed: BTreeSet::new(),
            dirty: true,
            ready: false,
            last_attempt: None,
            last_success: None,
            retry_at: None,
        }
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn is_stale(&self) -> bool {
        !self.ready || self.dirty || self.events.is_none() || self.agents != self.subscribed
    }

    pub fn poll(&mut self) -> io::Result<Option<Snapshot>> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> io::Result<Option<Snapshot>> {
        let started = Instant::now();
        if self.events.is_none() && self.retry_at.is_some_and(|at| now < at) {
            return Ok(None);
        }
        let session = match session_key_at(&self.socket) {
            Ok(session) => session,
            Err(error) => return self.disconnected(now, error),
        };
        if self.session.as_ref() != Some(&session) {
            self.session = Some(session.clone());
            self.events = None;
            self.agents.clear();
            self.subscribed.clear();
            self.ready = false;
            self.dirty = true;
            self.last_attempt = None;
            self.last_success = None;
            self.retry_at = None;
        }
        if let Some(events) = &mut self.events {
            match events.drain() {
                Ok(changed) => self.dirty |= changed,
                Err(error) => return self.disconnected(now, error),
            }
        }
        if (self.events.is_none() || self.agents != self.subscribed)
            && self.retry_at.is_none_or(|at| now >= at)
        {
            match Events::connect(&self.socket, &self.agents) {
                Ok(events) => {
                    self.events = Some(events);
                    self.subscribed.clone_from(&self.agents);
                    self.dirty = true;
                    self.retry_at = None;
                }
                Err(error) if self.events.is_none() => return self.disconnected(now, error),
                Err(_) => {
                    // A pane may close while its subscription is being armed.
                    // Keep the old topology subscription and obtain a new roster.
                    self.retry_at = Some(now + RECONNECT_INTERVAL);
                    self.dirty = true;
                }
            }
        }
        // Connection setup can block; rate-limit the actual snapshot reads,
        // not the time before the subscription handshake began.
        let now = now + started.elapsed();
        let due = self.dirty
            || self
                .last_success
                .is_none_or(|at| now.duration_since(at) >= RECOVERY_INTERVAL);
        if !due
            || self
                .last_attempt
                .is_some_and(|at| now.duration_since(at) < SNAPSHOT_FLOOR)
        {
            return Ok(None);
        }
        self.last_attempt = Some(now);
        let snapshot = match snapshot_at(&self.socket) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.dirty = true;
                return Err(error);
            }
        };
        let current_session = match session_key_at(&self.socket) {
            Ok(session) => session,
            Err(error) => return self.disconnected(now, error),
        };
        if current_session != session {
            return self.disconnected(now, invalid("Herdr session changed while subscribing"));
        }
        self.agents = snapshot.data["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["pane_id"].as_str().unwrap().to_owned())
            .collect();
        // A newly discovered agent needs a subscription followed by another
        // baseline, closing the snapshot-to-subscribe gap.
        self.dirty = self.agents != self.subscribed;
        self.ready = true;
        self.last_success = Some(now);
        Ok(Some(snapshot))
    }

    fn disconnected(&mut self, now: Instant, error: io::Error) -> io::Result<Option<Snapshot>> {
        self.events = None;
        // A closed pane in the old roster must not prevent topology-only
        // reconnection and the authoritative baseline that discovers its removal.
        self.agents.clear();
        self.subscribed.clear();
        self.dirty = true;
        self.retry_at = Some(now + RECONNECT_INTERVAL);
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    struct Server {
        socket: PathBuf,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn new(socket: PathBuf, label: &str) -> Self {
            let listener = UnixListener::bind(&socket).unwrap();
            let mut state = json!({
                "agents":[], "workspaces":[{"workspace_id":"w1", "label":label}],
                "tabs":[], "panes":[], "layouts":[]
            });
            let mut subscribers: Vec<UnixStream> = Vec::new();
            let thread =
                thread::spawn(move || {
                    while let Ok((mut stream, _)) = listener.accept() {
                        let mut line = String::new();
                        if BufReader::new(&mut stream).read_line(&mut line).unwrap() == 0 {
                            continue;
                        }
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let events = request["method"] == "events.subscribe";
                        if events
                            && request["params"]["subscriptions"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|sub| {
                                    sub.get("pane_id").is_some_and(|pane| {
                                        !state["agents"]
                                            .as_array()
                                            .unwrap()
                                            .iter()
                                            .any(|agent| agent["pane_id"] == *pane)
                                    })
                                })
                        {
                            serde_json::to_writer(&mut stream, &json!({
                            "id":request["id"], "error":{"message":"subscription target closed"}
                        })).unwrap();
                            stream.write_all(b"\n").unwrap();
                            continue;
                        }
                        let result = match request["method"].as_str().unwrap() {
                            "session.snapshot" => json!({"snapshot":state}),
                            "test.change" => {
                                state["workspaces"][0]["label"] =
                                    request["params"]["label"].clone();
                                if let Some(agents) = request["params"].get("agents") {
                                    state["agents"] = agents.clone();
                                }
                                if request["params"]["notify"] == true {
                                    subscribers.retain_mut(|stream| {
                                        stream
                                            .write_all(
                                                b"{\"event\":\"workspace_renamed\",\"data\":{}}\n",
                                            )
                                            .is_ok()
                                    });
                                }
                                json!({})
                            }
                            "test.disconnect" => {
                                subscribers.clear();
                                json!({})
                            }
                            "test.stop" | "events.subscribe" => json!({}),
                            method => panic!("unexpected test request {method}"),
                        };
                        serde_json::to_writer(
                            &mut stream,
                            &json!({"id":request["id"], "result":result}),
                        )
                        .unwrap();
                        stream.write_all(b"\n").unwrap();
                        if request["method"] == "test.stop" {
                            break;
                        }
                        if events {
                            subscribers.push(stream);
                        }
                    }
                });
            Self {
                socket,
                thread: Some(thread),
            }
        }

        fn change(&self, label: &str, notify: bool) {
            call_at(
                &self.socket,
                "test.change",
                json!({"label":label, "notify":notify}),
            )
            .unwrap();
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = call_at(&self.socket, "test.stop", json!({}));
            self.thread.take().unwrap().join().unwrap();
            let _ = fs::remove_file(&self.socket);
        }
    }

    #[test]
    fn snapshots_follow_changes_recover_gaps_and_reset_on_server_replacement() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hps-sync-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("api.sock");
        let mut server = Server::new(socket.clone(), "initial");
        let mut sync = SnapshotSync::at(socket.clone());
        let start = Instant::now();
        let label = |snapshot: Snapshot| {
            snapshot.data["workspaces"][0]["label"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert!(sync.is_stale());
        assert_eq!(label(sync.poll_at(start).unwrap().unwrap()), "initial");
        assert!(!sync.is_stale());
        server.change("renamed", true);
        assert!(sync
            .poll_at(start + Duration::from_millis(100))
            .unwrap()
            .is_none());
        assert!(sync.is_stale());
        assert_eq!(
            label(
                sync.poll_at(start + Duration::from_secs(1))
                    .unwrap()
                    .unwrap()
            ),
            "renamed"
        );
        assert!(!sync.is_stale());

        // Notifications are hints, not a lossless log; the recovery read repairs a gap.
        server.change("missed notification", false);
        assert!(sync
            .poll_at(start + Duration::from_secs(2))
            .unwrap()
            .is_none());
        assert_eq!(
            label(
                sync.poll_at(start + Duration::from_secs(7))
                    .unwrap()
                    .unwrap()
            ),
            "missed notification"
        );
        call_at(&server.socket, "test.disconnect", json!({})).unwrap();
        assert!(sync.poll_at(start + Duration::from_secs(8)).is_err());
        assert!(sync.is_stale());
        server.change("reconnected", false);
        assert_eq!(
            label(
                sync.poll_at(start + Duration::from_secs(10))
                    .unwrap()
                    .unwrap()
            ),
            "reconnected"
        );

        let old_session = snapshot_at(&socket).unwrap().session;
        // Keep the old connection alive while replacing the pathname.
        let retired = dir.join("retired.sock");
        fs::rename(&socket, &retired).unwrap();
        server.socket = retired;
        let replacement = Server::new(socket, "new server");
        assert_eq!(
            old_session
                .call("test.change", json!({"label":"wrong session"}))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            label(
                sync.poll_at(start + Duration::from_secs(11))
                    .unwrap()
                    .unwrap()
            ),
            "new server"
        );
        assert!(!sync.is_stale());
        drop(replacement);
        drop(server);
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn reconnect_discards_closed_pane_subscriptions_before_rebuilding_the_roster() {
        let dir = std::env::temp_dir().join(format!("hps-closed-reconnect-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("api.sock");
        let server = Server::new(socket.clone(), "initial");
        call_at(&socket, "test.change", json!({
            "label":"agent present",
            "agents":[{"terminal_id":"terminal", "workspace_id":"w1", "tab_id":"w1:t1", "pane_id":"w1:p1"}]
        })).unwrap();
        let mut sync = SnapshotSync::at(socket.clone());
        let start = Instant::now();
        assert_eq!(
            sync.poll_at(start).unwrap().unwrap().data["agents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        sync.poll_at(start + Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(!sync.is_stale());

        call_at(&socket, "test.disconnect", json!({})).unwrap();
        call_at(
            &socket,
            "test.change",
            json!({"label":"agent closed", "agents":[]}),
        )
        .unwrap();
        assert!(sync.poll_at(start + Duration::from_secs(2)).is_err());
        let recovered = sync
            .poll_at(start + Duration::from_secs(4))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.data["workspaces"][0]["label"], "agent closed");
        assert!(recovered.data["agents"].as_array().unwrap().is_empty());
        assert!(!sync.is_stale());
        drop(server);
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn fragmented_events_are_bounded_and_disconnect_is_not_an_empty_snapshot() {
        let (read, mut write) = UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let mut events = Events {
            reader: BufReader::new(read),
            pending: Vec::new(),
        };
        write.write_all(b"{\"event\":\"pane_moved\",").unwrap();
        assert!(events.drain().unwrap());
        write
            .write_all(b"\"data\":{}}\n{\"event\":\"pane_closed\",\"data\":{}}\n")
            .unwrap();
        assert!(events.drain().unwrap());
        assert!(events.pending.is_empty());
        write
            .write_all(b"{\"error\":\"lost subscription\"}\n")
            .unwrap();
        assert!(events.drain().is_err());
        events.pending = vec![b' '; EVENT_LIMIT];
        write.write_all(b"x").unwrap();
        assert!(events.drain().is_err());
        events.pending.clear();
        drop(write);
        assert_eq!(
            events.drain().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_failed_mismatched_and_incomplete_responses() {
        assert!(read_response(&b"{\"id\":\"x\",\"result\":{}}\n"[..], "x").is_ok());
        for reply in [
            "{\"id\":\"x\",\"error\":{\"message\":\"pane closed\"}}\n",
            "{\"id\":\"other\",\"result\":{}}\n",
            "{\"id\":\"x\",\"result\":{}}",
            "{\"id\":\"x\"}\n",
        ] {
            assert!(read_response(reply.as_bytes(), "x").is_err());
        }
    }
}

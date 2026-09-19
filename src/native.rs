//! Publish presentation tokens matching Radar's exact visual and behavioral contract.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::activity::{now_unix_ms, ActivityStore, Freshness};
use crate::{config, icons, ipc};

const SOURCE: &str = "plugin:herdr-project-sidebar";
const SPIN_MS: u64 = 150;
// Recovery bound, not change detection: semantic transitions arrive via the
// event wake hints; this only bounds how stale an idle view can get.
const CATCH_ALL: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const READY_ENV: &str = "HERDR_PROJECT_SIDEBAR_READY";

const KEYS: [&str; 17] = [
    "group_parent", "group", "group_stale", "split_mark",
    "logo", "logo_working", "logo_stale", "harness_logo",
    "title_working", "title_done", "title_blocked",
    "title_idle_fresh", "title_idle", "title_idle_stale", "title_unknown",
    "gap", "ws_group",
];

const SPACE_KEYS: [&str; 20] = [
    "space_blocked", "space_done", "space_idle", "space_unknown", "space_none", "space_label",
    "space_working_claude", "space_working_gemini", "space_working_kimi",
    "space_working_deepseek", "space_working_qwen", "space_working_kiro",
    "space_working_cline", "space_working_kilo", "space_working_other",
    "space_logo_claude", "space_logo_gemini", "space_logo_kimi",
    "space_logo_deepseek", "space_logo_qwen",
];

type Tokens = Map<String, Value>;
type RowTokens = BTreeMap<String, Tokens>;

fn socket_path() -> io::Result<PathBuf> {
    std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HERDR_SOCKET_PATH is not set"))
}

fn daemon_paths() -> io::Result<(PathBuf, PathBuf)> {
    let socket = socket_path()?;
    let socket = socket.canonicalize().unwrap_or(socket);
    let hash = socket.as_os_str().as_bytes().iter().fold(0xcbf29ce484222325u64,
        |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3));
    let dir = config::state_dir();
    fs::create_dir_all(&dir)?;
    Ok((dir.join(format!("daemon-{hash:016x}.lock")), dir.join(format!("d-{hash:016x}.sock"))))
}

struct SocketFile(PathBuf);
impl Drop for SocketFile {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

pub fn start() -> io::Result<()> {
    if !config::load()?.enabled { return clear(); }
    let (_, ready) = daemon_paths()?;
    if UnixStream::connect(&ready).is_ok() { return Ok(()); }
    let reply = config::state_dir().join(format!("r-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&reply)?;
    let _reply_file = SocketFile(reply.clone());
    listener.set_nonblocking(true)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--daemon").env(READY_ENV, &reply)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .process_group(0).spawn()?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let mut line = String::new();
                BufReader::new(stream).take(8192).read_line(&mut line)?;
                let result: Value = serde_json::from_str(&line)?;
                return if result["ok"] == true { Ok(()) } else {
                    Err(io::Error::other(result["error"].as_str().unwrap_or("sidebar initialization failed").to_owned()))
                };
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            if let Ok((stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let result: Value = serde_json::from_reader(stream.take(8192))?;
                return if result["ok"] == true { Ok(()) } else {
                    Err(io::Error::other(result["error"].as_str().unwrap_or("sidebar initialization failed").to_owned()))
                };
            }
            return Err(io::Error::other(format!("sidebar daemon exited before readiness: {status}")));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "sidebar daemon initialization timed out"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn notify_start(result: &io::Result<()>) -> io::Result<()> {
    let Some(path) = std::env::var_os(READY_ENV) else { return Ok(()); };
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let reply = match result { Ok(()) => json!({"ok": true}), Err(error) => json!({"ok": false, "error": error.to_string()}) };
    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")
}

pub fn run() -> io::Result<()> {
    let result = run_inner();
    if result.is_err() { let _ = notify_start(&result); }
    result
}

fn run_inner() -> io::Result<()> {
    let (lock_path, ready_path) = daemon_paths()?;
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(lock_path)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let deadline = Instant::now() + START_TIMEOUT;
            loop {
                if UnixStream::connect(&ready_path).is_ok() { return notify_start(&Ok(())); }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "existing sidebar daemon did not become ready"));
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
    let initial_refresh = publisher.refresh()?;
    if initial_refresh.is_none() { return notify_start(&Ok(())); }
    let mut events = Some(subscribe()?);
    let listener = UnixListener::bind(&ready_path)?;
    let _ready_file = SocketFile(ready_path);
    listener.set_nonblocking(true)?;
    notify_start(&Ok(()))?;
    let socket = socket_path()?;
    let mut next_deadline = initial_refresh.unwrap_or_else(|| Instant::now() + CATCH_ALL);

    loop {
        while listener.accept().is_ok() {}
        let mut disconnected = false;
        if let Some(reader) = events.as_mut() {
            let timeout = next_deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
            reader.get_ref().set_read_timeout(Some(timeout))?;
            let mut bytes = [0; 16384];
            match reader.read(&mut bytes) {
                Ok(0) => disconnected = true,
                Ok(_) => {}
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(_) => disconnected = true,
            }
        } else {
            let wait = next_deadline.saturating_duration_since(Instant::now());
            thread::sleep(wait);
            match subscribe() {
                Ok(reader) => {
                    events = Some(reader);
                    publisher = Publisher::default();
                }
                Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => return Ok(()),
                Err(error) => eprintln!("sidebar subscription: {error}"),
            }
        }
        if disconnected {
            events = None;
            if !socket.exists() { return Ok(()); }
        }
        match publisher.refresh() {
            Ok(None) => return Ok(()),
            Ok(Some(due)) => {
                next_deadline = due;
            }
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) && !socket.exists() => return Ok(()),
            Err(error) => {
                eprintln!("sidebar refresh: {error}");
                thread::sleep(Duration::from_millis(SPIN_MS));
                next_deadline = Instant::now() + CATCH_ALL;
            }
        }
    }
}

fn subscribe() -> io::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(socket_path()?)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let kinds = ["pane.created", "pane.closed", "pane.exited", "pane.agent_detected", "pane.moved",
        "pane.focused", "tab.focused",
        "workspace.created", "workspace.closed", "workspace.renamed", "workspace.moved", "workspace.reordered",
        "worktree.created", "worktree.opened", "worktree.removed", "tab.created", "tab.closed", "tab.moved"];
    let subscriptions: Vec<_> = kinds.iter().map(|kind| json!({"type": kind})).collect();
    serde_json::to_writer(&mut stream, &json!({"id": "hps-events", "method": "events.subscribe", "params": {"subscriptions": subscriptions}}))?;
    stream.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    (&mut reader).take(65536).read_line(&mut line)?;
    let ack: Value = serde_json::from_str(&line)?;
    if ack["id"] != "hps-events" || ack["result"]["type"] != "subscription_started" {
        return Err(io::Error::other(format!("Herdr event subscription rejected: {ack}")));
    }
    Ok(reader)
}

fn entries<'a>(snapshot: &'a Value, key: &str) -> io::Result<&'a [Value]> {
    snapshot.get(key).and_then(Value::as_array).map(Vec::as_slice)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("session.snapshot omitted {key}")))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("").trim()
}

fn empty_tokens(keys: &[&str]) -> Tokens {
    keys.iter().map(|key| ((*key).to_owned(), Value::Null)).collect()
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

fn patch(pane: &str, tokens: Tokens) -> io::Result<()> {
    for chunk in chunk_tokens(tokens, 16) {
        ipc::call("pane.report_metadata", json!({"pane_id": pane, "source": SOURCE, "tokens": chunk}))?;
    }
    Ok(())
}

fn patch_workspace(workspace: &str, tokens: Tokens) -> io::Result<()> {
    for chunk in chunk_tokens(tokens, 16) {
        ipc::call("workspace.report_metadata", json!({"workspace_id": workspace, "source": SOURCE, "tokens": chunk}))?;
    }
    Ok(())
}

pub fn refresh() -> io::Result<()> {
    Publisher::default().refresh().map(|_| ())
}

pub fn clear() -> io::Result<()> {
    let response = ipc::call("session.snapshot", json!({}))?;
    let panes = entries(&response["snapshot"], "panes")?;
    let workspaces = entries(&response["snapshot"], "workspaces")?;
    let mut failure = None;
    for pane in panes {
        if KEYS.iter().any(|key| pane["tokens"].get(*key).is_some()) {
            if let Err(error) = patch(text(pane, "pane_id"), empty_tokens(&KEYS)) { failure = Some(error); }
        }
    }
    for workspace in workspaces {
        if SPACE_KEYS.iter().any(|key| workspace["tokens"].get(*key).is_some()) {
            if let Err(error) = patch_workspace(text(workspace, "workspace_id"), empty_tokens(&SPACE_KEYS)) { failure = Some(error); }
        }
    }
    if let Err(error) = ipc::call("agent.view.clear", json!({"source": SOURCE})) { failure = Some(error); }
    match failure { Some(error) => Err(error), None => Ok(()) }
}

pub struct Publisher {
    pub activity: ActivityStore,
    branches: BTreeMap<String, String>,
    branch_at: Option<Instant>,
    last: HashMap<String, Tokens>,
    last_workspaces: HashMap<String, Tokens>,
    grouped: Option<bool>,
    native_style: bool,
}

impl Default for Publisher {
    fn default() -> Self {
        Self {
            activity: ActivityStore::new(config::state_dir()),
            branches: BTreeMap::new(),
            branch_at: None,
            last: HashMap::new(),
            last_workspaces: HashMap::new(),
            grouped: None,
            native_style: false,
        }
    }
}

impl Publisher {
    pub fn refresh(&mut self) -> io::Result<Option<Instant>> {
        let settings = config::load()?;
        if !settings.enabled { clear()?; return Ok(None); }
        if !settings.project_style {
            if !self.native_style {
                clear()?;
                self.last.clear();
                self.last_workspaces.clear();
                self.grouped = None;
                self.native_style = true;
            }
            return Ok(Some(Instant::now() + CATCH_ALL));
        }
        self.native_style = false;
        let response = ipc::call("session.snapshot", json!({}))?;
        let snapshot = &response["snapshot"];
        let agents = entries(snapshot, "agents")?;
        let workspaces = entries(snapshot, "workspaces")?;
        let tabs = entries(snapshot, "tabs")?;
        let panes = entries(snapshot, "panes")?;

        let now_ms = now_unix_ms();
        let spin_step = (now_ms / SPIN_MS) as usize;
        let mut has_working = false;
        let mut has_blocked = false;
        let mut next_wake_ms = now_ms + CATCH_ALL.as_millis() as u64;

        // Branch map at TTL: one socket round trip per repo via native
        // worktree.list (5s); the per-row git probe is fallback only.
        {
            let mut roots: Vec<String> = workspaces
                .iter()
                .filter_map(|w| {
                    let r = text(&w["worktree"], "repo_root");
                    if r.is_empty() { None } else { Some(r.to_owned()) }
                })
                .collect();
            roots.sort();
            roots.dedup();
            if self.branch_at.map(|t| t.elapsed() >= Duration::from_secs(5)).unwrap_or(true) {
                self.branches = crate::ipc::branch_map(&roots);
                self.branch_at = Some(Instant::now());
            }
        }

        // 1. Process agent state transitions and holds
        for agent in agents {
            let pane_id = text(agent, "pane_id");
            let status = text(agent, "agent_status");

            if status == "working" {
                self.activity.mark_working(pane_id, now_ms);
                has_working = true;
            }

            if status == "blocked" {
                has_blocked = true;
            }

        }

        // Prune vanished panes: holds must never outlive their pane.
        let live: HashSet<&str> = agents.iter().map(|a| text(a, "pane_id")).collect();
        self.activity.stamps.retain(|k, _| live.contains(k.as_str()));

        // Determine next wake deadline
        let now_instant = Instant::now();
        if has_working || has_blocked {
            next_wake_ms = next_wake_ms.min(now_ms + SPIN_MS);
        }

        // 2. Generate row tokens
        let (wanted, wanted_workspaces) = rows(&RowsInput {
            agents,
            workspaces,
            tabs,
            panes,
            settings: &settings,
            activity: &self.activity,
            branches: &self.branches,
            now_ms,
            spin_step,
        });

        // 3. Patch panes with chunked writes. Deltas compare against the live
        // snapshot tokens, never a private cache: external writes must not
        // leave stale beliefs installed.
        for pane in panes {
            let id = text(pane, "pane_id");
            if !wanted.contains_key(id) && KEYS.iter().any(|key| pane["tokens"].get(*key).is_some()) {
                patch(id, empty_tokens(&KEYS))?;
            }
        }
        for (pane, tokens) in wanted {
            let live = panes.iter().find(|p| text(p, "pane_id") == pane);
            let delta: Tokens = tokens.iter()
                .filter(|(key, value)| live.and_then(|p| p["tokens"].get(*key)) != Some(*value))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !delta.is_empty() {
                patch(&pane, delta)?;
            }
        }

        // 4. Patch workspaces (same live-token rule as panes).
        for (workspace, tokens) in wanted_workspaces {
            let live = workspaces.iter().find(|w| text(w, "workspace_id") == workspace);
            let delta: Tokens = tokens.iter()
                .filter(|(key, value)| live.and_then(|w| w["tokens"].get(*key)) != Some(*value))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !delta.is_empty() {
                patch_workspace(&workspace, delta)?;
            }
        }

        // 5. Update sort override
        if self.grouped != Some(settings.grouped) {
            let sort = if settings.grouped {
                json!([{"field": {"token": "ws_group"}, "order": "asc"}, {"field": "state_change_seq", "order": "desc"}, {"field": "tab_order", "order": "asc"}, {"field": "pane_order", "order": "asc"}])
            } else {
                json!([{"field": "state_change_seq", "order": "desc"}, {"field": "tab_order", "order": "asc"}, {"field": "pane_order", "order": "asc"}])
            };
            ipc::call("agent.view.set", json!({"source": SOURCE, "label": if settings.grouped { "active" } else { "recent" }, "sort": sort}))?;
            self.grouped = Some(settings.grouped);
        }

        let sleep_duration = Duration::from_millis(next_wake_ms.saturating_sub(now_ms).max(10));
        Ok(Some(now_instant + sleep_duration))
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
    pub branches: &'a BTreeMap<String, String>,
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
        branches,
        now_ms,
        spin_step,
    } = *input;
    let mut cwd = HashMap::new();
    let mut ws_agents: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();

    for agent in agents {
        let path = if text(agent, "foreground_cwd").is_empty() { text(agent, "cwd") } else { text(agent, "foreground_cwd") };
        cwd.entry(text(agent, "workspace_id")).or_insert(path);
        let ws = text(agent, "workspace_id");
        ws_agents.entry(ws).or_default().push((text(agent, "agent"), text(agent, "agent_status")));
    }

    let mut spaces: HashMap<String, Workspace> = workspaces.iter().enumerate().map(|(index, ws)| {
        let id = text(ws, "workspace_id").to_owned();
        let tree = &ws["worktree"];
        let label = if text(ws, "label").is_empty() { id.clone() } else { text(ws, "label").to_owned() };
        let repo = text(tree, "repo_key");
        let project = if text(tree, "repo_name").is_empty() { label.clone() } else { text(tree, "repo_name").to_owned() };
        let checkout = text(tree, "checkout_path");
        let branch = if !settings.show_branch {
            None
        } else {
            branches
                .get(checkout.trim_end_matches('/'))
                .filter(|b| !b.is_empty())
                .map(|b| b.to_owned())
                .or_else(|| {
                    git_branch(Path::new(if checkout.is_empty() { cwd.get(id.as_str()).copied().unwrap_or("") } else { checkout }))
                })
        };
        (id.clone(), Workspace {
            group: if repo.is_empty() { format!("workspace:{id}") } else { format!("repo:{repo}") },
            id, project, label, branch, linked: tree["is_linked_worktree"].as_bool().unwrap_or(false),
            order: ws["number"].as_u64().unwrap_or(index as u64),
        })
    }).collect();

    for agent in agents {
        let id = text(agent, "workspace_id");
        spaces.entry(id.to_owned()).or_insert_with(|| Workspace {
            id: id.to_owned(), group: format!("workspace:{id}"), project: id.to_owned(),
            label: id.to_owned(),
            branch: if settings.show_branch { git_branch(Path::new(cwd.get(id).copied().unwrap_or(""))) } else { None },
            linked: false, order: u64::MAX,
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
                "claude" | "gemini" | "kimi" | "deepseek" | "qwen" | "kiro" | "cline" | "kilo" => {
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
                    "claude" | "gemini" | "kimi" | "deepseek" | "qwen" | "kiro" | "cline" | "kilo" => {
                        format!("space_logo_{v}")
                    }
                    _ => "space_logo_other".into(),
                };
                let glyph = icons::logo(v, settings.icons).unwrap_or("");
                let text = if glyph.is_empty() { v.to_owned() } else { format!("{glyph} {v}") };
                tokens.insert(logo_key, json!(text));
            }
        }
        workspace_rows.insert(ws_id.to_owned(), tokens);
    }

    // Agent ordering
    let tab_order: HashMap<_, _> = tabs.iter().enumerate().map(|(index, tab)| (text(tab, "tab_id"), tab["number"].as_u64().unwrap_or(index as u64))).collect();
    let pane_order: HashMap<_, _> = panes.iter().enumerate().map(|(index, pane)| (text(pane, "pane_id"), index)).collect();
    let mut ordered: Vec<_> = agents.iter().filter(|agent| !text(agent, "pane_id").is_empty()).collect();

    // Native recency: state_change_seq orders turns; local clocks never do.
    fn seq(agent: &Value) -> u64 {
        agent.get("state_change_seq").and_then(Value::as_u64).unwrap_or(0)
    }
    ordered.sort_by(|a, b| {
        let wa = &spaces[text(a, "workspace_id")];
        let wb = &spaces[text(b, "workspace_id")];
        (seq(b), &wa.group, wa.linked, wa.order, &wa.id,
            tab_order.get(text(a, "tab_id")).copied().unwrap_or(u64::MAX),
            pane_order.get(text(a, "pane_id")).copied().unwrap_or(usize::MAX), text(a, "pane_id"))
            .cmp(&(seq(a), &wb.group, wb.linked, wb.order, &wb.id,
                tab_order.get(text(b, "tab_id")).copied().unwrap_or(u64::MAX),
                pane_order.get(text(b, "pane_id")).copied().unwrap_or(usize::MAX), text(b, "pane_id")))
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
            match activity.freshness(pane_id, now_ms) {
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
            if ordered.get(index + 1).is_some_and(|next| spaces[text(next, "workspace_id")].group != ws.group) {
                tokens.insert("gap".into(), json!("\u{200b}"));
            }
        }

        // Indent & Logo
        let indent = if settings.grouped && !is_head { "\u{200b}  " } else { "" };
        let vendor = text(agent, "agent");
        let logo_glyph = icons::logo(vendor, settings.icons);
        if let Some(glyph) = logo_glyph {
            let logo_str = format!("{indent}{glyph}");
            tokens.insert("harness_logo".into(), json!(glyph));
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
    if !show_title { return name; }
    let supplied = text(agent, "title");
    if !supplied.is_empty() { return supplied; }
    let title = text(agent, "terminal_title_stripped");
    if title.is_empty() { return name; }
    let title = if matches!(name, "omp" | "pi") {
        title.strip_prefix("π ").unwrap_or(title)
            .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '>' || ('\u{2800}'..='\u{28ff}').contains(&ch))
    } else { title };
    if title.is_empty() { name } else { title }
}

fn git_branch(start: &Path) -> Option<String> {
    if !start.is_absolute() { return None; }
    for dir in start.ancestors() {
        let marker = dir.join(".git");
        let git = if marker.is_dir() { marker } else if marker.is_file() {
            let contents = fs::read_to_string(marker).ok()?;
            dir.join(contents.trim().strip_prefix("gitdir:")?.trim())
        } else { continue; };
        let head = fs::read_to_string(git.join("HEAD")).ok()?;
        let head = head.trim();
        if let Some(branch) = head.strip_prefix("ref: refs/heads/") { return Some(branch.to_owned()); }
        return (head.len() >= 7 && head.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| head[..7].to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_and_blocked_pulse() {
        let frame0 = icons::spinner(0);
        let frame1 = icons::spinner(1);
        assert_ne!(frame0, frame1);
        assert_eq!(icons::blocked_mark(0), "?");
        assert_eq!(icons::blocked_mark(5), "·");
    }

    #[test]
    fn test_done_follows_authoritative_status() {
        let activity = ActivityStore::default();
        let agents = vec![
            json!({"pane_id": "p1", "workspace_id": "w1", "agent": "claude", "agent_status": "idle", "focused": false}),
            json!({"pane_id": "p2", "workspace_id": "w1", "agent": "claude", "agent_status": "done", "focused": false}),
        ];
        let workspaces = vec![
            json!({"workspace_id": "w1", "label": "demo"}),
        ];
        let settings = config::Settings::default();

        let (panes, _) = rows(&RowsInput {
            agents: &agents,
            workspaces: &workspaces,
            tabs: &[],
            panes: &[],
            settings: &settings,
            activity: &activity,
            branches: &BTreeMap::new(),
            now_ms: 1000,
            spin_step: 0,
        });
        // No hold: idle renders idle even with no focus event since done.
        assert!(panes["p1"]["title_idle"].is_string());
        assert_eq!(panes["p1"]["title_done"], Value::Null);
        assert!(panes["p2"]["title_done"].is_string());
    }
}

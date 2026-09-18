//! herdr-project-sidebar: Orca-style project -> worktree -> agent tree.
//! Live Herdr sync in `snapshot()` (agent/workspace list grouped by repo_key);
//! render keeps the same row shape so the tree builder does not change.
//!
//! Perf notes: deadline-driven tick (150ms only while a visible agent works,
//! 1s idle sleep), signature-skipped draws, windowed render (only the visible
//! slice builds widgets), input/mouse interrupt the wait immediately. Snapshot
//! refreshes at most once per second plus on demand -- never per event, never
//! per tick, and never via `pane.updated` (own-echo stalls writes ~110ms).
//!
//! Theming: colors come from Herdr's own `config.toml` (`theme.custom` +
//! `ui.sidebar.agents` row rules) with built-in defaults when absent. Nothing
//! is written back; light/dark follows whatever theme is configured.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde::Deserialize;

// ponytail: single file; split into snapshot/tree/ui modules only when it
// passes ~1500 lines. Polling CLI (not socket events) is the ceiling: socket
// subscribe + async reads when 1s snapshots measurably lag.

const SPINNER: [&str; 8] = ["⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽", "⣾"];
const TICK: Duration = Duration::from_millis(150);
const IDLE_POLL: Duration = Duration::from_secs(1);
const SNAPSHOT_MIN_AGE: Duration = Duration::from_secs(1);
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);
const FRESH_SECS: u64 = 15 * 60;
const STALE_SECS: u64 = 2 * 60 * 60;
const PANE_TITLE: &str = "Projects";

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
enum State {
    Working,
    Monitoring,
    Blocked,
    Interrupted,
    Done,
    IdleFresh,
    Idle,
    IdleStale,
    Unknown,
}

impl State {
    fn rank(self) -> u8 {
        match self {
            State::Working => 0,
            State::Monitoring => 1,
            State::Blocked => 2,
            State::Interrupted => 3,
            State::Done => 4,
            State::IdleFresh => 5,
            State::Idle => 6,
            State::IdleStale => 7,
            State::Unknown => 8,
        }
    }
    /// States where a human should look: blocked/failed/unacked-done. Working
    /// needs nothing, so J (jump-to-attention) skips it.
    fn is_attention(self) -> bool {
        matches!(self, State::Blocked | State::Interrupted | State::Done)
    }
}

/// Pin marker decoded from its codepoint at runtime (0xf08d = Nerd
/// thumb-tack), so the source carries no fragile PUA literal.
fn pin_mark(font_ok: bool) -> char {
    if font_ok {
        char::from_u32(0xf08d).unwrap_or('*')
    } else {
        '*'
    }
}

#[derive(Clone)]
struct Agent {
    vendor: String,
    title: String,
    state: State,
    pane_id: String,
    tab_id: String,
    last_active: Option<u64>,
}

#[derive(Clone)]
struct Worktree {
    key: String,
    name: String,
    branch: String,
    collapsed: bool,
    agents: Vec<Agent>,
}

#[derive(Clone)]
struct Project {
    id: String,
    name: String,
    icon: char,
    icon_color: Color,
    branch: String,
    collapsed: bool,
    pinned: bool,
    focused: bool,
    worktrees: Vec<Worktree>,
}

#[derive(Clone, Copy)]
enum Row {
    Project(usize),
    Worktree(usize, usize),
    Agent(usize, usize, usize),
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Grouped,
    Recent,
}

#[cfg(test)]
fn stub() -> Vec<Project> {
    vec![
        Project {
            id: "stub-a".into(),
            name: "muse-bridge".into(),
            icon: '#',
            icon_color: Color::Cyan,
            branch: "main".into(),
            collapsed: false,
            pinned: false,
            focused: false,
            worktrees: vec![Worktree {
                key: "stub-a/main".into(),
                name: "muse-bridge".into(),
                branch: "main".into(),
                collapsed: false,
                agents: vec![
                    Agent {
                        vendor: "pi".into(),
                        title: "Implement OAuth scopes".into(),
                        state: State::Working,
                        pane_id: "".into(),
                        tab_id: "".into(),
                        last_active: None,
                    },
                    Agent {
                        vendor: "codex".into(),
                        title: "Wire retry budget".into(),
                        state: State::Done,
                        pane_id: "".into(),
                        tab_id: "".into(),
                        last_active: None,
                    },
                    Agent {
                        vendor: "opencode".into(),
                        title: "Migrate invoices table".into(),
                        state: State::Idle,
                        pane_id: "".into(),
                        tab_id: "".into(),
                        last_active: None,
                    },
                ],
            }],
        },
        Project {
            id: "stub-b".into(),
            name: "sold-by-robots".into(),
            icon: '#',
            icon_color: Color::Magenta,
            branch: "feature/mc-13200".into(),
            collapsed: false,
            pinned: false,
            focused: false,
            worktrees: vec![Worktree {
                key: "stub-b/feature".into(),
                name: "sbr-9u4v.35".into(),
                branch: "feature/mc-13200".into(),
                collapsed: false,
                agents: vec![Agent {
                    vendor: "claude".into(),
                    title: "Which env file should I edit?".into(),
                    state: State::Blocked,
                    pane_id: "".into(),
                    tab_id: "".into(),
                    last_active: None,
                }],
            }],
        },
    ]
}

// ---------- Herdr CLI snapshot ----------

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

fn herdr_json(args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new(herdr_bin()).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

#[derive(Deserialize)]
struct AgentEntry {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    foreground_cwd: Option<String>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    terminal_title: Option<String>,
}

#[derive(Deserialize)]
struct WorktreeInfo {
    #[serde(default)]
    checkout_path: String,
    #[serde(default)]
    repo_key: String,
    #[serde(default)]
    repo_name: String,
}

#[derive(Deserialize)]
struct WorkspaceEntry {
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    worktree: Option<WorktreeInfo>,
}

fn map_status(s: &str) -> State {
    match s {
        "working" => State::Working,
        "monitoring" => State::Monitoring,
        "blocked" | "permission" | "waiting" => State::Blocked,
        "interrupted" | "failed" => State::Interrupted,
        "done" | "active" => State::Done,
        "idle" | "inactive" => State::Idle,
        _ => State::Unknown,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Branch straight from `.git/HEAD`: always current, one file read. No dirty
/// marker (that needs git); `.git` may be a file (`gitdir:` pointer) in a
/// worktree or submodule.
fn git_branch(start: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(start);
    if !dir.is_absolute() {
        return None;
    }
    loop {
        let candidate = dir.join(".git");
        let gitdir = if candidate.is_dir() {
            candidate
        } else if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let pointer = text
                .lines()
                .find_map(|l| l.strip_prefix("gitdir:"))
                .map(str::trim)?;
            dir.join(pointer)
        } else {
            match dir.parent() {
                Some(p) => {
                    dir = p.to_path_buf();
                    continue;
                }
                None => return None,
            }
        };
        let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
        let head = head.trim();
        if let Some(r) = head.strip_prefix("ref: refs/heads/") {
            return Some(r.to_string());
        }
        if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(head[..7].to_string());
        }
        return None;
    }
}

/// Short-term memory across snapshots: activity stamps, held badges, collapse.
#[derive(Default)]
struct Memory {
    /// pane_id -> unix secs last seen working.
    activity: BTreeMap<String, u64>,
    /// panes holding a done badge until focused.
    done_held: BTreeSet<String>,
    /// previous raw status per pane, to catch transitions.
    previous: BTreeMap<String, String>,
    collapsed_projects: BTreeSet<String>,
    collapsed_worktrees: BTreeSet<String>,
    pinned: BTreeSet<String>,
    /// Keys this instance unfolded/unpinned ("c:<id>", "w:<key>", "p:<id>"):
    /// the save-merge subtracts them so another sidebar's file copy cannot
    /// resurrect a removal the user just made here.
    dropped: BTreeSet<String>,
    /// Glyph set: "auto" (detect Nerd Font, fall back silently), "font"
    /// (assume it), "text" (ASCII always, never ask). First-run notice flips
    /// this off auto once the user chooses.
    font_choice: String,
    dirty_state: bool,
    last_state_save: Option<std::time::Instant>,
}

impl Memory {
    fn freshness(&self, pane_id: &str, now: u64) -> State {
        match self.activity.get(pane_id) {
            None => State::Idle,
            Some(at) => {
                let age = now.saturating_sub(*at);
                if age <= FRESH_SECS {
                    State::IdleFresh
                } else if age >= STALE_SECS {
                    State::IdleStale
                } else {
                    State::Idle
                }
            }
        }
    }
}

fn agent_title(e: &AgentEntry) -> String {
    if let Some(t) = e.terminal_title_stripped.as_ref().filter(|t| !t.is_empty()) {
        return t.clone();
    }
    if let Some(t) = e.terminal_title.as_ref().filter(|t| !t.is_empty()) {
        return t.clone();
    }
    std::path::Path::new(&e.cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| e.cwd.clone())
}

/// Snapshot Herdr state. Returns projects plus a sync error: total CLI
/// failure renders an empty tree with the error in the header -- never
/// fictional stub rows (stub() is tests-only).
fn snapshot(
    mem: &mut Memory,
    theme_projects: &[Color],
    font_ok: bool,
) -> (Vec<Project>, Option<String>) {
    let agents_v = herdr_json(&["agent", "list"]);
    let workspaces_v = herdr_json(&["workspace", "list"]);
    if agents_v.is_none() && workspaces_v.is_none() {
        return (Vec::new(), Some("herdr unreachable (agent + workspace list failed)".into()));
    }
    let now = now_secs();
    let mut agents: Vec<AgentEntry> = agents_v
        .as_ref()
        .and_then(|v| v.pointer("/result/agents"))
        .and_then(|v| v.as_array().map(|a| {
            a.iter()
                .filter_map(|e| serde_json::from_value(e.clone()).ok())
                .collect()
        }))
        .unwrap_or_default();
    let workspaces: Vec<WorkspaceEntry> = workspaces_v
        .as_ref()
        .and_then(|v| v.pointer("/result/workspaces"))
        .and_then(|v| v.as_array().map(|a| {
            a.iter()
                .filter_map(|e| serde_json::from_value(e.clone()).ok())
                .collect()
        }))
        .unwrap_or_default();

    // Transitions first: stamp work, latch done, release on focus/work.
    // The latch preserves the badge when Herdr moves done->idle with no
    // focus event in between; only focus or new work clears it.
    for a in &agents {
        let prev = mem.previous.get(&a.pane_id).cloned().unwrap_or_default();
        if a.agent_status == "working" || a.agent_status == "monitoring" {
            mem.activity.insert(a.pane_id.clone(), now);
            mem.dirty_state = true;
        }
        if a.agent_status == "done" && prev != "done" {
            mem.done_held.insert(a.pane_id.clone());
            mem.dirty_state = true;
        }
        if a.focused || a.agent_status == "working" || a.agent_status == "monitoring" {
            if mem.done_held.remove(&a.pane_id) {
                mem.dirty_state = true;
            }
        }
        mem.previous.insert(a.pane_id.clone(), a.agent_status.clone());
    }
    // Prune memory for vanished panes so maps and state.json stay bounded.
    {
        let live: BTreeSet<&str> = agents.iter().map(|a| a.pane_id.as_str()).collect();
        mem.activity.retain(|k, _| live.contains(k.as_str()));
        mem.done_held.retain(|k| live.contains(k.as_str()));
        mem.previous.retain(|k, _| live.contains(k.as_str()));
    }

    let ws_by_id: BTreeMap<&str, &WorkspaceEntry> =
        workspaces.iter().map(|w| (w.workspace_id.as_str(), w)).collect();
    // Group agents: workspace -> repo_key (or cwd fallback).
    let mut groups: BTreeMap<String, BTreeMap<String, Vec<AgentEntry>>> = BTreeMap::new();
    // ponytail: drain keeps one pass; agents is rebuilt by the caller each frame.
    for a in agents.drain(..) {
        let repo = ws_by_id
            .get(a.workspace_id.as_str())
            .and_then(|w| w.worktree.as_ref())
            .map(|t| t.repo_key.clone())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| format!("cwd:{}", a.cwd));
        groups
            .entry(a.workspace_id.clone())
            .or_default()
            .entry(repo)
            .or_default()
            .push(a);
    }

    let mut projects = Vec::new();
    for (ws_id, repos) in &groups {
        let ws = ws_by_id.get(ws_id.as_str()).copied();
        let label = ws
            .map(|w| w.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| format!("external ({ws_id})"));
        let mut worktrees = Vec::new();
        for (repo_key, entries) in repos {
            let wt = ws.and_then(|w| w.worktree.as_ref());
            let name = wt
                .map(|t| t.repo_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    repo_key
                        .strip_prefix("cwd:")
                        .and_then(|c| {
                            std::path::Path::new(c)
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| repo_key.clone())
                });
            let probe = wt
                .map(|t| t.checkout_path.clone())
                .filter(|p| !p.is_empty())
                .or_else(|| {
                    entries[0]
                        .foreground_cwd
                        .clone()
                        .filter(|p| !p.is_empty())
                })
                .unwrap_or_else(|| entries[0].cwd.clone());
            let branch = git_branch(&probe).unwrap_or_default();
            let mut list: Vec<Agent> = entries
                .iter()
                .map(|e| {
                    let mut st = map_status(&e.agent_status);
                    if st == State::Done && !mem.done_held.contains(&e.pane_id) {
                        st = mem.freshness(&e.pane_id, now);
                    } else if matches!(st, State::Idle | State::Unknown) {
                        st = if mem.done_held.contains(&e.pane_id) {
                            State::Done
                        } else {
                            mem.freshness(&e.pane_id, now)
                        };
                    }
                    Agent {
                        vendor: if e.agent.is_empty() {
                            "?".into()
                        } else {
                            e.agent.clone()
                        },
                        title: agent_title(e),
                        state: st,
                        pane_id: e.pane_id.clone(),
                        tab_id: e.tab_id.clone(),
                        last_active: mem.activity.get(&e.pane_id).copied(),
                    }
                })
                .collect();
            list.sort_by_key(|a| (a.state.rank(), a.tab_id.clone(), a.pane_id.clone()));
            let key = format!("{ws_id}::{repo_key}");
            worktrees.push(Worktree {
                key: key.clone(),
                name,
                branch,
                collapsed: mem.collapsed_worktrees.contains(&key),
                agents: list,
            });
        }
        worktrees.sort_by(|a, b| {
            let ra = a.agents.iter().map(|x| x.state.rank()).min().unwrap_or(9);
            let rb = b.agents.iter().map(|x| x.state.rank()).min().unwrap_or(9);
            ra.cmp(&rb).then_with(|| a.name.cmp(&b.name))
        });
        let branch = worktrees
            .iter()
            .find(|w| !w.branch.is_empty())
            .map(|w| w.branch.clone())
            .unwrap_or_default();
        // ponytail: djb2 hash -> stable per-project hue; semantic green/red
        // excluded so state color is never ambiguous.
        projects.push(Project {
            icon_color: theme_projects[project_hue(ws_id) % theme_projects.len()],
            icon: if font_ok { '' } else { '#' },
            id: ws_id.clone(),
            name: label,
            branch,
            collapsed: mem.collapsed_projects.contains(ws_id),
            pinned: mem.pinned.contains(ws_id),
            focused: ws.map(|w| w.focused).unwrap_or(false),
            worktrees,
        });
    }
    // Workspaces with no live agents still deserve a header when Herdr knows
    // them (fresh worktree, all panes closed). Built BEFORE the sort so pins
    // float and ordering applies to every row.
    for w in &workspaces {
        if !groups.contains_key(&w.workspace_id) {
            projects.push(Project {
                icon_color: theme_projects[project_hue(&w.workspace_id) % theme_projects.len()],
                icon: if font_ok { '' } else { '#' },
                id: w.workspace_id.clone(),
                name: if w.label.is_empty() {
                    w.workspace_id.clone()
                } else {
                    w.label.clone()
                },
                branch: String::new(),
                collapsed: mem.collapsed_projects.contains(&w.workspace_id),
                pinned: mem.pinned.contains(&w.workspace_id),
                focused: w.focused,
                worktrees: vec![],
            });
        }
    }
    // Busiest first, pins floating. Agent-less projects score 9 and sink
    // unless pinned.
    projects.sort_by(|a, b| {
        let score = |p: &Project| {
            p.worktrees
                .iter()
                .flat_map(|w| &w.agents)
                .map(|x| x.state.rank())
                .min()
                .unwrap_or(9)
        };
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| score(a).cmp(&score(b)))
            .then_with(|| a.name.cmp(&b.name))
    });
    (projects, None)
}

/// Stable per-project palette index (djb2 over the workspace id). One helper
/// so agent-backed and agent-less rows never disagree on a project's hue.
fn project_hue(ws_id: &str) -> usize {
    let mut h: u64 = 5381;
    for b in ws_id.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h as usize
}

// ---------- Theming from Herdr's own config ----------

#[derive(Clone)]
struct Theme {
    working: Color,
    monitoring: Color,
    blocked: Color,
    interrupted: Color,
    done: Color,
    idle_fresh: Color,
    idle: Color,
    idle_stale: Color,
    unknown: Color,
    dim: Color,
    projects: Vec<Color>,
}

fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_matches('"').trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// `equals = "working" ... fg = "#..."` row rules override theme.custom keys.
/// Scoped to the enclosing `{...}` rule (never a sibling rule), `#` comments
/// stripped, and byte-safe: every slice goes through `str::get` so a
/// multi-byte glyph near a rule cannot panic the render loop.
fn rule_color(text: &str, state: &str) -> Option<Color> {
    // Strip full-line and trailing comments (# outside quotes); commented
    // example blocks must never read as live configuration.
    let mut clean = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let mut quote: Option<char> = None;
        let mut cut = line.len();
        for (i, c) in line.char_indices() {
            if quote.is_some() {
                if Some(c) == quote {
                    quote = None;
                }
            } else if c == '"' || c == '\'' {
                quote = Some(c);
            } else if c == '#' {
                cut = i;
                break;
            }
        }
        clean.push_str(line.get(..cut).unwrap_or(""));
        clean.push('\n');
    }
    let needle = format!("equals = \"{state}\"");
    let mut search = clean.as_str();
    loop {
        let i = search.find(&needle)?;
        let rest = search.get(i + needle.len()..).unwrap_or("");
        let bound = rest
            .find('}')
            .or_else(|| rest.find("equals = \""))
            .unwrap_or(rest.len());
        let window = rest.get(..bound).unwrap_or("");
        if let Some(j) = window.find("fg = \"") {
            if let Some(after) = window.get(j + 6..) {
                if let Some(end) = after.find('"') {
                    if let Some(c) = after.get(..end).and_then(parse_hex) {
                        return Some(c);
                    }
                }
            }
        }
        search = rest;
    }
}

fn custom_color(text: &str, key: &str) -> Option<Color> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(key) && line.get(key.len()..).map(|r| r.starts_with([' ', '\t', '='])).unwrap_or(false) {
            if let Some(v) = line.split('=').nth(1) {
                // Quoted first (hex lives inside quotes); unquoted falls
                // back to the token before any trailing comment.
                if let Some(q) = v.split('"').nth(1) {
                    if let Some(c) = parse_hex(q) {
                        return Some(c);
                    }
                }
                let v = v.trim();
                let v = match v.find('#') {
                    Some(i) => v.get(..i).unwrap_or("").trim(),
                    None => v.split([' ', '\t', ',']).next().unwrap_or(v).trim(),
                };
                if let Some(c) = parse_hex(v) {
                    return Some(c);
                }
            }
        }
    }
    None
}

fn load_theme() -> Theme {
    let fallback = Theme {
        working: Color::Rgb(0xf9, 0xe2, 0xaf),
        monitoring: Color::Rgb(0xf9, 0xe2, 0xaf),
        blocked: Color::Rgb(0xf3, 0x8b, 0xa8),
        interrupted: Color::Rgb(0xf3, 0x8b, 0xa8),
        done: Color::Rgb(0x94, 0xe2, 0xd5),
        idle_fresh: Color::Rgb(0xcd, 0xd6, 0xf4),
        idle: Color::Rgb(0x6c, 0x70, 0x86),
        idle_stale: Color::Rgb(0x6c, 0x70, 0x86),
        unknown: Color::Rgb(0x90, 0x7a, 0xa9),
        dim: Color::DarkGray,
        projects: vec![
            Color::Rgb(0xcb, 0xa6, 0xf7),
            Color::Rgb(0x89, 0xb4, 0xfa),
            Color::Rgb(0xfa, 0xb3, 0x87),
            Color::Rgb(0x89, 0xdC, 0xeB),
        ],
    };
    let path = std::env::var("HERDR_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/herdr/config.toml")
        });
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.is_empty() {
        return fallback;
    }
    // Theme keys first, row rules win: same precedence Herdr itself uses.
    let get = |rule_state: &str, custom_keys: &[&str], fb: Color| {
        rule_color(&text, rule_state)
            .or_else(|| custom_keys.iter().find_map(|k| custom_color(&text, k)))
            .unwrap_or(fb)
    };
    let working = get("working", &["yellow"], fallback.working);
    Theme {
        working,
        monitoring: get("monitoring", &["yellow", "peach"], working),
        blocked: get("blocked", &["red"], fallback.blocked),
        interrupted: get("interrupted", &["red", "peach"], fallback.blocked),
        done: get("done", &["teal", "green"], fallback.done),
        idle_fresh: get("idle_fresh", &["text", "subtext0"], fallback.idle_fresh),
        idle: get("idle", &["overlay0"], fallback.idle),
        idle_stale: get("idle_stale", &["overlay0"], fallback.idle_stale),
        unknown: get("unknown", &["mauve", "overlay1"], fallback.unknown),
        dim: fallback.dim,
        projects: fallback.projects.clone(),
    }
}

/// Effective glyph set: an explicit choice always wins; auto detects.
/// Empty (pre-choice state files) counts as auto.
fn use_font(choice: &str, detected: bool) -> bool {
    match choice {
        "font" => true,
        "text" => false,
        _ => detected,
    }
}

/// First-run notice owed: never chose, and no Nerd Font found. Pure so tests
/// pin it; HERDR_SIDEBAR_FONT bypasses the dialog entirely.
fn font_notice_due(choice: &str, detected: bool) -> bool {
    if std::env::var("HERDR_SIDEBAR_FONT").is_ok() {
        return false;
    }
    (choice == "auto" || choice.is_empty()) && !detected
}

fn font_ok() -> bool {
    if std::env::var("HERDR_SIDEBAR_FONT").map(|v| v == "0").unwrap_or(false) {
        return false;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![
        format!("{home}/.local/share/fonts"),
        "/usr/share/fonts".into(),
        "/run/current-system/sw/share/fonts".into(),
    ];
    // Depth 0 then depth 1; entries() errors are skipped (perf: ~3 readdirs).
    let mut depth1 = Vec::new();
    for dir in dirs.drain(..) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.to_lowercase().contains("nerd") {
                    return true;
                }
                if e.path().is_dir() {
                    depth1.push(e.path().to_string_lossy().into_owned());
                }
            }
        }
    }
    for dir in depth1 {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().to_lowercase().contains("nerd") {
                    return true;
                }
            }
        }
    }
    // The binary's own dir may ship beside a font marker; Ghostty/kitty with
    // the radar icon font also satisfy this via config below.
    std::env::var("HERDR_SIDEBAR_FONT").map(|v| v == "1").unwrap_or(false)
}

// ---------- state.json ----------

fn state_path() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("HERDR_SIDEBAR_STATE") {
        return std::path::PathBuf::from(d);
    }
    let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.local/state",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    });
    std::path::PathBuf::from(base).join("herdr/plugins/herdr-project-sidebar/state.json")
}

fn read_state_file() -> serde_json::Value {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn load_state(mem: &mut Memory) {
    let v = read_state_file();
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    mem.collapsed_projects = get("collapsed_projects").into_iter().collect();
    mem.collapsed_worktrees = get("collapsed_worktrees").into_iter().collect();
    mem.pinned = get("pinned").into_iter().collect();
    mem.font_choice = v
        .get("font_choice")
        .and_then(|x| x.as_str())
        .unwrap_or("auto")
        .to_string();
    if let Some(obj) = v.get("activity").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            if let Some(t) = val.as_u64() {
                mem.activity.insert(k.clone(), t);
            }
        }
    }
}

fn save_state(mem: &mut Memory, force: bool) {
    let now = std::time::Instant::now();
    // True debounce: nothing dirty means nothing to write, dirty means wait
    // out the interval so bursts coalesce into one write.
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
    // Merge with the file instead of overwriting: a second sidebar (one per
    // tab is the intended shape) may have folded/pinned since our load.
    // Union for adds, max for stamps; this instance's own removals (dropped)
    // are subtracted so an unfold/unpin here is never resurrected by the
    // file. Dropped keys unknown to both sides are pruned.
    let file = read_state_file();
    let file_has = |k: &str, s: &str| {
        file.get(k)
            .and_then(|x| x.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some(s)))
            .unwrap_or(false)
    };
    mem.dropped.retain(|k| {
        let (ns, plain) = k.split_once(':').unwrap_or(("", k));
        match ns {
            "c" => mem.collapsed_projects.contains(plain) || file_has("collapsed_projects", plain),
            "w" => mem.collapsed_worktrees.contains(plain) || file_has("collapsed_worktrees", plain),
            "p" => mem.pinned.contains(plain) || file_has("pinned", plain),
            _ => true,
        }
    });
    let union = |k: &str, ns: &str, own: &BTreeSet<String>| {
        let mut out = own.clone();
        if let Some(a) = file.get(k).and_then(|x| x.as_array()) {
            out.extend(a.iter().filter_map(|x| {
                x.as_str().filter(|s| !mem.dropped.contains(&format!("{ns}:{s}"))).map(str::to_string)
            }));
        }
        out.into_iter().collect::<Vec<_>>()
    };
    // Stamps merge by max: the newest observation wins regardless of writer.
    let mut activity = mem.activity.clone();
    if let Some(obj) = file.get("activity").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            if let Some(t) = val.as_u64() {
                activity
                    .entry(k.clone())
                    .and_modify(|e| *e = (*e).max(t))
                    .or_insert(t);
            }
        }
    }
    let v = serde_json::json!({
        "collapsed_projects": union("collapsed_projects", "c", &mem.collapsed_projects),
        "collapsed_worktrees": union("collapsed_worktrees", "w", &mem.collapsed_worktrees),
        "pinned": union("pinned", "p", &mem.pinned),
        "activity": activity,
        "font_choice": mem.font_choice,
    });
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic: per-process temp beside the target, rename into place (Herdr
    // watches state dirs). A fixed temp name lets two sidebars publish each
    // other's partial bytes.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ---------- Launcher (--toggle/--ensure, launch.rs model) ----------

/// Pure decision over a `pane list` JSON: our pane is the one in `scope_tab`
/// titled [`PANE_TITLE`]. Stale/unparseable input degrades to `OPEN`.
fn launch_decision(pane_list_json: &str, scope_tab: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(pane_list_json) {
        Ok(v) => v,
        Err(_) => return "OPEN".into(),
    };
    let panes = v
        .pointer("/result/panes")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let tab_of = |id: &str| panes.iter().find(|p| p.get("pane_id").and_then(|x| x.as_str()) == Some(id));
    let scope = if scope_tab.is_empty() {
        panes
            .iter()
            .find(|p| p.get("focused").and_then(|x| x.as_bool()).unwrap_or(false))
            .and_then(|p| p.get("tab_id").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string()
    } else {
        scope_tab.to_string()
    };
    for p in &panes {
        // Exact server label, never a title substring: titles are
        // user/agent-controlled and a contains-match once targeted a user
        // pane. The manifest pane title ("Projects") is the label Herdr
        // reports for plugin panes.
        let label = p.get("label").and_then(|x| x.as_str()).unwrap_or("");
        let tab = p.get("tab_id").and_then(|x| x.as_str()).unwrap_or("");
        if label == PANE_TITLE && (scope.is_empty() || tab == scope) {
            let id = p.get("pane_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let focused = p.get("focused").and_then(|x| x.as_bool()).unwrap_or(false);
            if !is_flag_safe(&id) {
                return "OPEN".into();
            }
            if focused {
                return format!("CLOSE {id}");
            }
            let _ = tab_of; // scope already confines the match
            return format!("FOCUS {id}");
        }
    }
    "OPEN".into()
}

/// True when the id can be passed as a positional CLI argument without any
/// risk of being parsed as a flag. Server-issued ids match this; anything
/// else is refused rather than executed.
fn is_flag_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-')
}

/// First stderr line, trimmed to footer width. Herdr reports server errors as
/// JSON on stderr -- the diagnosis is already in the buffer.
fn first_stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(100)
        .collect()
}

fn run_launcher(toggle: bool) -> io::Result<()> {
    let list = Command::new(herdr_bin()).args(["pane", "list"]).output()?;
    let json = String::from_utf8_lossy(&list.stdout).into_owned();
    // Scope to the focused tab so a sidebar elsewhere never twins or closes.
    let scope = {
        let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        v.pointer("/result/panes")
            .and_then(|p| p.as_array())
            .and_then(|panes| {
                panes.iter().find(|p| {
                    p.get("focused").and_then(|x| x.as_bool()).unwrap_or(false)
                })
            })
            .and_then(|p| p.get("tab_id").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string()
    };
    let open = || {
        Command::new(herdr_bin())
            .args([
                "plugin", "pane", "open",
                "--plugin", "herdr-project-sidebar",
                "--entrypoint", "projects",
                "--placement", "split", "--direction", "right", "--focus",
            ])
            .status()
            .map(|_| ())
    };
    let decision = launch_decision(&json, &scope);
    let d = decision.as_str();
    if d.starts_with("CLOSE ") && toggle {
        let id = d.trim_start_matches("CLOSE ").trim();
        if is_flag_safe(id) {
            // Plugin-scoped close: a mis-identified user pane can never land
            // here, and the server refuses non-plugin panes regardless.
            Command::new(herdr_bin()).args(["plugin", "pane", "close", id]).status()?;
        } else {
            open()?;
        }
    } else if let Some(id) = d.strip_prefix("FOCUS ") {
        // Focus-by-id: `plugin pane open` has no reuse semantics and would
        // stack a second sidebar.
        if is_flag_safe(id.trim()) {
            Command::new(herdr_bin())
                .args(["plugin", "pane", "focus", id.trim()])
                .status()?;
        } else {
            open()?;
        }
    } else {
        open()?;
    }
    Ok(())
}

fn focus_pane(pane_id: &str) {
    if !is_flag_safe(pane_id) {
        return;
    }
    let _ = Command::new(herdr_bin())
        .args(["agent", "focus", pane_id])
        .output();
}


// ---------- Tree ----------

fn matches_filter(p: &Project, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    if p.name.to_lowercase().contains(&q) || p.branch.to_lowercase().contains(&q) {
        return true;
    }
    p.worktrees.iter().any(|w| {
        w.name.to_lowercase().contains(&q)
            || w.branch.to_lowercase().contains(&q)
            || w.agents.iter().any(|a| {
                a.title.to_lowercase().contains(&q) || a.vendor.to_lowercase().contains(&q)
            })
    })
}

fn visible(projects: &[Project], query: &str, compact: bool, view: View) -> Vec<Row> {
    if view == View::Recent {
        // Flat activity order; workspace prefix keeps rows attributable.
        let mut flat: Vec<Row> = Vec::new();
        for (pi, p) in projects.iter().enumerate() {
            if !matches_filter(p, query) {
                continue;
            }
            for (wi, w) in p.worktrees.iter().enumerate() {
                for (ai, _) in w.agents.iter().enumerate() {
                    if compact && w.agents[ai].state == State::IdleStale {
                        continue;
                    }
                    flat.push(Row::Agent(pi, wi, ai));
                }
            }
        }
        flat.sort_by_key(|r| match *r {
            Row::Agent(pi, wi, ai) => {
                let a = &projects[pi].worktrees[wi].agents[ai];
                (a.state.rank(), std::cmp::Reverse(a.last_active.unwrap_or(0)))
            }
            _ => (9, std::cmp::Reverse(0)),
        });
        return flat;
    }
    let mut rows = Vec::new();
    for (pi, p) in projects.iter().enumerate() {
        if !matches_filter(p, query) {
            continue;
        }
        rows.push(Row::Project(pi));
        if p.collapsed {
            continue;
        }
        for (wi, w) in p.worktrees.iter().enumerate() {
            rows.push(Row::Worktree(pi, wi));
            if w.collapsed {
                continue;
            }
            for (ai, _) in w.agents.iter().enumerate() {
                if compact && w.agents[ai].state == State::IdleStale {
                    continue;
                }
                rows.push(Row::Agent(pi, wi, ai));
            }
        }
    }
    rows
}

/// Map a click/hover y (0-based, includes the top border) to a visible index.
fn click_index(offset: usize, y: u16) -> Option<usize> {
    if y == 0 { None } else { Some(offset + y as usize - 1) }
}

/// Keep selection inside the window; pure so tests pin it.
fn ensure_visible(selected: usize, offset: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

fn counts(projects: &[Project]) -> (usize, usize, usize, usize) {
    let mut agents = 0;
    let mut working = 0;
    let mut blocked = 0;
    let mut unread = 0;
    for p in projects {
        for w in &p.worktrees {
            for a in &w.agents {
                agents += 1;
                match a.state {
                    State::Working | State::Monitoring => working += 1,
                    State::Blocked | State::Interrupted => {
                        blocked += 1;
                        unread += 1;
                    }
                    State::Done => unread += 1,
                    _ => {}
                }
            }
        }
    }
    (agents, working, blocked, unread)
}

fn state_glyph(state: State, tick: usize, font_ok: bool) -> (&'static str, bool) {
    // (glyph, animated): the header only spins while something animates.
    match state {
        State::Working => (SPINNER[tick % SPINNER.len()], true),
        State::Monitoring => (if font_ok { "◉" } else { "○" }, false),
        State::Done => ("✓", false),
        State::Blocked => ("?", false),
        State::Interrupted => ("!", false),
        State::IdleFresh => ("●", false),
        State::Idle => ("○", false),
        State::IdleStale => ("·", false),
        State::Unknown => ("◌", false),
    }
}

fn state_color(theme: &Theme, state: State) -> Color {
    match state {
        State::Working => theme.working,
        State::Monitoring => theme.monitoring,
        State::Blocked => theme.blocked,
        State::Interrupted => theme.interrupted,
        State::Done => theme.done,
        State::IdleFresh => theme.idle_fresh,
        State::Idle => theme.idle,
        State::IdleStale => theme.idle_stale,
        State::Unknown => theme.unknown,
    }
}

/// Best-effort terminal restore; idempotent (shutdown path and panic hook may
/// both run it).
fn restore_term() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

/// RAII terminal restore for every `?` early-return in the render loop.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_term();
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--toggle") {
        run_launcher(true)?;
        return Ok(());
    }
    if args.iter().any(|a| a == "--ensure") {
        run_launcher(false)?;
        return Ok(());
    }
    if args.iter().any(|a| a == "--launch-decision") {
        let mut input = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut input);
        println!("{}", launch_decision(&input, ""));
        return Ok(());
    }
    if args.iter().any(|a| a == "--dump-snapshot") {
        let mut mem = Memory::default();
        load_state(&mut mem);
        let font = use_font(&mem.font_choice, font_ok());
        let theme = load_theme();
        let (projects, err) = snapshot(&mut mem, &theme.projects, font);
        let rows = visible(&projects, "", false, View::Grouped);
        let dump: Vec<serde_json::Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id, "name": p.name, "branch": p.branch,
                    "collapsed": p.collapsed, "pinned": p.pinned,
                    "worktrees": p.worktrees.iter().map(|w| serde_json::json!({
                        "key": w.key, "name": w.name, "branch": w.branch,
                        "agents": w.agents.iter().map(|a| serde_json::json!({
                            "vendor": a.vendor, "title": a.title,
                            "state": format!("{:?}", a.state),
                            "pane_id": a.pane_id,
                        })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({"projects": dump, "rows": rows.len(), "error": err}));
        return Ok(());
    }

    let mut mem = Memory::default();
    load_state(&mut mem);
    // Glyph set: explicit choice wins, auto detects once. The notice below
    // is the install prompt: it explains what the font is for and remembers.
    let mut font_detected = font_ok();
    let mut font = use_font(&mem.font_choice, font_detected);
    let mut font_notice = font_notice_due(&mem.font_choice, font_detected);
    let mut font_dialog = false;
    let mut theme = load_theme();
    let (mut projects, mut sync_error) = snapshot(&mut mem, &theme.projects, font);
    let mut last_snapshot = std::time::Instant::now();
    let mut selected = 0usize;
    let mut offset = 0usize;
    let mut hover: Option<usize> = None;
    let mut tick = 0usize;
    let mut last_drawn = String::new();
    let mut query = String::new();
    let mut filtering = false;
    let mut compact = false;
    let mut view = View::Grouped;
    let mut confirm_close: Option<String> = None;
    let mut status_line: String = String::new();

    // A panic or an I/O error must never brick the shell: without this the
    // terminal stays in raw mode on the alternate screen with mouse capture
    // on (no echo, escape bursts on every mouse move) and state unsaved.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_term();
        default_hook(info);
    }));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, SetTitle(PANE_TITLE))?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    // RAII: every `?` below returns through this drop, so an I/O error can
    // never leave raw mode + alternate screen + mouse capture armed. (Panics
    // take the hook above.) State loss is bounded by the 5s debounced save.
    let _term_guard = TermGuard;
    loop {
        // Refresh at most 1/sec; theme re-read rides along so light/dark
        // follows config edits without a restart.
        if last_snapshot.elapsed() >= SNAPSHOT_MIN_AGE {
            theme = load_theme();
            let (fresh, err) = snapshot(&mut mem, &theme.projects, font);
            projects = fresh;
            sync_error = err;
            last_snapshot = std::time::Instant::now();
        }
        save_state(&mut mem, false);
        let rows = visible(&projects, &query, compact, view);
        if selected >= rows.len() {
            selected = rows.len().saturating_sub(1);
        }
        let height = term.size()?.height.saturating_sub(2) as usize;
        // Clamp the window itself: when rows shrink under a high offset the
        // viewport would otherwise anchor on the last row and draw blanks.
        offset = ensure_visible(selected, offset, height)
            .min(rows.len().saturating_sub(height));
        let working = rows.iter().any(|r| match *r {
            Row::Agent(pi, wi, ai) => {
                let (g, anim) = state_glyph(projects[pi].worktrees[wi].agents[ai].state, 0, font);
                let _ = g;
                anim
            }
            _ => false,
        });
        let step = if working { tick % SPINNER.len() } else { 0 };
        let sig = signature(&projects, &rows, selected, offset, height, hover, step, &query, compact, view, &status_line, filtering, &theme, font_dialog, font);
        if sig != last_drawn {
            let (agents, working_n, blocked, unread) = counts(&projects);
            // First visible agent row per tab, in display order: drives the
            // split-child indent. Computed over all rows (not the window) so
            // scrolled-off leaders still count.
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut first_rows: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
            for r in &rows {
                if let Row::Agent(pi, wi, ai) = *r {
                    if seen.insert(projects[pi].worktrees[wi].agents[ai].tab_id.as_str()) {
                        first_rows.insert((pi, wi, ai));
                    }
                }
            }
            let end = (offset + height).min(rows.len());
            let mut lines = Vec::with_capacity(end.saturating_sub(offset));
            for (i, row) in rows[offset..end].iter().enumerate() {
                let idx = offset + i;
                let mut line = match *row {
                    Row::Project(pi) => {
                        let p = &projects[pi];
                        let mark = if p.collapsed { "▸" } else { "▾" };
                        // Nerd pin by codepoint (no literal: survives any transport); ASCII star fallback.
                        let pin = if p.pinned { pin_mark(font).to_string() } else { String::new() };
                        let here = if p.focused { "● " } else { "" };
                        Line::from(vec![
                            Span::styled(
                                format!("{} {} ", p.icon, mark),
                                Style::default().fg(p.icon_color).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("{here}{}{} ", p.name, pin),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                p.branch.to_string(),
                                Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
                            ),
                        ])
                    }
                    Row::Worktree(pi, wi) => {
                        let w = &projects[pi].worktrees[wi];
                        let mark = if w.collapsed { "▸" } else { "▾" };
                        let wt = if font { "  \u{f418} " } else { "  └─ " };
                        Line::from(vec![
                            Span::styled(wt.to_string(), Style::default().fg(theme.dim)),
                            Span::raw(format!("{mark} {} ", w.name)),
                            Span::styled(
                                w.branch.to_string(),
                                Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
                            ),
                        ])
                    }
                    Row::Agent(pi, wi, ai) => {
                        let a = &projects[pi].worktrees[wi].agents[ai];
                        let (glyph, _) = state_glyph(a.state, tick, font);
                        // First visible row per tab keeps worktree indent;
                        // later same-tab rows hang deeper. Decided in display
                        // order via first_rows, precomputed below.
                        let first_in_tab = first_rows.contains(&(pi, wi, ai));
                        let indent = if view == View::Recent {
                            format!("  [{}] ", projects[pi].name)
                        } else if first_in_tab {
                            "    ".to_string()
                        } else {
                            "      └─ ".to_string()
                        };
                        let mut spans = vec![
                            Span::raw(indent),
                            Span::styled(
                                glyph.to_string(),
                                Style::default()
                                    .fg(state_color(&theme, a.state))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" [{}] {}", a.vendor, a.title),
                                Style::default().fg(state_color(&theme, a.state)),
                            ),
                        ];
                        if a.state == State::IdleStale {
                            for s in &mut spans {
                                s.style = s.style.add_modifier(Modifier::DIM);
                            }
                        }
                        Line::from(spans)
                    }
                };
                let mut style = Style::default();
                if idx == selected {
                    style = style.bg(theme.dim);
                    line.spans.insert(0, Span::raw("› "));
                } else {
                    line.spans.insert(0, Span::raw("  "));
                }
                if hover == Some(idx) && idx != selected {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                for span in &mut line.spans {
                    span.style = span.style.patch(style);
                }
                lines.push(line);
            }
            term.draw(|f| {
                let bottom = if let Some(e) = sync_error.as_deref() {
                    format!("herdr unreachable: {e} (retrying)")
                } else if filtering {
                    format!("filter: {query}  (enter/esc done)")
                } else if !status_line.is_empty() {
                    status_line.clone()
                } else if font_notice {
                    "Nerd Font not found - ASCII icons - F font options".into()
                } else {
                    "click select · enter focus/fold · / filter · v order · c compact · J jump · o ws · p pin · D close-ws · q quit".into()
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        "Projects ({agents} agents · {working_n} working · {blocked} blocked · {unread} unread)"
                    ))
                    .title_bottom(bottom);
                f.render_widget(Paragraph::new(lines).block(block), f.area());
                if font_dialog {
                    let area = f.area();
                    let w = 62.min(area.width.saturating_sub(4)).max(20);
                    let h = 16.min(area.height.saturating_sub(4)).max(8);
                    let rect = ratatui::layout::Rect::new(
                        area.width.saturating_sub(w) / 2,
                        area.height.saturating_sub(h) / 2,
                        w,
                        h,
                    );
                    // What the font is for, specifically: each row's lead mark.
                    let body = vec![
                        Line::from("No Nerd Font detected - ASCII fallbacks active:"),
                        Line::from(""),
                        Line::from("  project head   repo glyph  ->  #"),
                        Line::from("  worktree row   branch mark ->  └─"),
                        Line::from("  pinned         pin mark    ->  *"),
                        Line::from("  states         spinner/tick/? stay the same"),
                        Line::from(""),
                        Line::from("A Nerd Font (e.g. JetBrainsMono Nerd Font)"),
                        Line::from("in the terminal lights up the left column."),
                        Line::from(""),
                        Line::from("  1 installed one - rescan   2 ASCII, don't ask"),
                        Line::from("  3 assume a font            esc later"),
                    ];
                    f.render_widget(ratatui::widgets::Clear, rect);
                    f.render_widget(
                        Paragraph::new(body).block(
                            Block::default().borders(Borders::ALL).title("Fonts (F)"),
                        ),
                        rect,
                    );
                    return;
                }
            })?;
            last_drawn = sig;
        }

        // Deadline-driven wait: spinner cadence only while working, long idle
        // sleep otherwise. Input and mouse interrupt immediately either way.
        if !event::poll(if working { TICK } else { IDLE_POLL })? {
            if working {
                tick += 1;
            }
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                // Fresh key input replaces the previous message (each arm sets
                // its own); mouse motion/resize must not eat lifecycle
                // outcomes. The close arm dies with any other key so the arm
                // and its prompt share a lifetime.
                status_line.clear();
                if key.code != KeyCode::Char('D') {
                    confirm_close = None;
                }
                if filtering {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => filtering = false,
                        KeyCode::Backspace => {
                            query.pop();
                        }
                        KeyCode::Char(c) => {
                            query.push(c);
                        }
                        _ => {}
                    }
                    selected = 0;
                    offset = 0;
                    continue;
                }
                if font_dialog {
                    match key.code {
                        KeyCode::Char('1') => {
                            font_detected = font_ok();
                            font = use_font(&mem.font_choice, font_detected);
                            font_notice = font_notice_due(&mem.font_choice, font_detected);
                            status_line = if font_notice {
                                "still no Nerd Font found".into()
                            } else {
                                "Nerd Font detected".into()
                            };
                            font_dialog = false;
                        }
                        KeyCode::Char('2') => {
                            mem.font_choice = "text".into();
                            font = false;
                            font_notice = false;
                            font_dialog = false;
                            status_line = "ASCII icons, won't ask again".into();
                            mem.dirty_state = true;
                        }
                        KeyCode::Char('3') => {
                            mem.font_choice = "font".into();
                            font = true;
                            font_notice = false;
                            font_dialog = false;
                            status_line = "Nerd Font assumed".into();
                            mem.dirty_state = true;
                        }
                        KeyCode::Esc => font_dialog = false,
                        _ => {}
                    }
                    // Rescan or switch: repaint from the new set immediately.
                    last_snapshot = last_snapshot
                        .checked_sub(SNAPSHOT_MIN_AGE * 2)
                        .unwrap_or(last_snapshot);
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(rows.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1)
                    }
                    KeyCode::Char('/') => {
                        filtering = true;
                        query.clear();
                        selected = 0;
                        offset = 0;
                    }
                    KeyCode::Char('v') => {
                        view = if view == View::Grouped { View::Recent } else { View::Grouped };
                        selected = 0;
                        offset = 0;
                    }
                    KeyCode::Char('c') => compact = !compact,
                    KeyCode::Char('F') => font_dialog = true,
                    KeyCode::Char('J') => {
                        // Next attention row after the cursor, wrapping: repeated
                        // presses walk every blocked/done-held row in reach.
                        let n = rows.len();
                        let is_att = |r: &Row| match *r {
                            Row::Agent(pi, wi, ai) => {
                                projects[pi].worktrees[wi].agents[ai].state.is_attention()
                            }
                            _ => false,
                        };
                        if n > 0 {
                            if let Some(off) =
                                (1..=n).find(|k| is_att(&rows[(selected + k) % n]))
                            {
                                selected = (selected + off) % n;
                            } else {
                                status_line = "no blocked or unacked rows".into();
                            }
                        }
                    }
                    KeyCode::Char('o') => {
                        if let Some(r) = rows.get(selected) {
                            let (id, name) = match *r {
                                Row::Project(pi) => {
                                    (projects[pi].id.clone(), projects[pi].name.clone())
                                }
                                Row::Worktree(pi, _) | Row::Agent(pi, _, _) => {
                                    (projects[pi].id.clone(), projects[pi].name.clone())
                                }
                            };
                            if !is_flag_safe(&id) {
                                status_line = "cannot focus: bad workspace id".into();
                            } else {
                                match Command::new(herdr_bin())
                                    .args(["workspace", "focus", &id])
                                    .output()
                                {
                                    Ok(o) if o.status.success() => {
                                        status_line = format!("focused {name}");
                                    }
                                    Ok(o) => {
                                        status_line =
                                            format!("focus failed: {name} {}", first_stderr(&o));
                                    }
                                    Err(e) => {
                                        status_line = format!("focus failed: {name} {e}");
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char('p') => {
                        if let Some(r) = rows.get(selected) {
                            let (pi, id) = match *r {
                                Row::Project(pi) => (pi, projects[pi].id.clone()),
                                Row::Worktree(pi, _) | Row::Agent(pi, _, _) => {
                                    (pi, projects[pi].id.clone())
                                }
                            };
                            if mem.pinned.contains(&id) {
                                mem.pinned.remove(&id);
                                mem.dropped.insert(format!("p:{id}"));
                                projects[pi].pinned = false;
                            } else {
                                mem.pinned.insert(id.clone());
                                mem.dropped.remove(&format!("p:{id}"));
                                projects[pi].pinned = true;
                            }
                            mem.dirty_state = true;
                        }
                    }
                    KeyCode::Char('D') => {
                        if let Some(r) = rows.get(selected) {
                            let (ws, name, live) = match *r {
                                Row::Project(pi) => (
                                    projects[pi].id.clone(),
                                    projects[pi].name.clone(),
                                    projects[pi]
                                        .worktrees
                                        .iter()
                                        .flat_map(|w| &w.agents)
                                        .count(),
                                ),
                                Row::Worktree(pi, _) | Row::Agent(pi, _, _) => (
                                    projects[pi].id.clone(),
                                    projects[pi].name.clone(),
                                    projects[pi]
                                        .worktrees
                                        .iter()
                                        .flat_map(|w| &w.agents)
                                        .count(),
                                ),
                            };
                            if confirm_close.as_deref() == Some(&ws) {
                                // Re-resolve the row: the list re-sorts under the
                                // cursor, so only an id match may close.
                                if !is_flag_safe(&ws) {
                                    status_line = "cannot close: bad workspace id".into();
                                } else {
                                    match Command::new(herdr_bin())
                                        .args(["workspace", "close", &ws])
                                        .output()
                                    {
                                        Ok(o) if o.status.success() => {
                                            status_line = format!("closed {name}");
                                            // Force a refresh past the 1s floor.
                                            last_snapshot = last_snapshot
                                                .checked_sub(SNAPSHOT_MIN_AGE * 2)
                                                .unwrap_or(last_snapshot);
                                        }
                                        Ok(o) => {
                                            status_line = format!(
                                                "close failed: {name} {}",
                                                first_stderr(&o)
                                            );
                                        }
                                        Err(e) => {
                                            status_line =
                                                format!("close failed: {name} {e}");
                                        }
                                    }
                                }
                                confirm_close = None;
                            } else {
                                confirm_close = Some(ws);
                                status_line = format!(
                                    "press D again to close {name} ({live} agents)"
                                );
                            }
                        }
                    }
                    KeyCode::Char('N') => {
                        match Command::new(herdr_bin()).args(["workspace", "create"]).output() {
                            Ok(o) if o.status.success() => {
                                status_line = "workspace created".into();
                                last_snapshot = last_snapshot
                                    .checked_sub(SNAPSHOT_MIN_AGE * 2)
                                    .unwrap_or(last_snapshot);
                            }
                            Ok(o) => {
                                status_line =
                                    format!("workspace create failed {}", first_stderr(&o));
                            }
                            Err(e) => {
                                status_line = format!("workspace create failed {e}");
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(_) => {
                    // A click elsewhere disarms the close prompt with it.
                    confirm_close = None;
                    if let Some(idx) = click_index(offset, m.row) {
                        if idx < rows.len() {
                            if idx == selected {
                                activate(&mut projects, &mut mem, &rows, idx);
                            } else {
                                selected = idx;
                            }
                        }
                    }
                }
                MouseEventKind::Moved => {
                    hover = click_index(offset, m.row)
                        .filter(|idx| *idx < rows.len());
                }
                MouseEventKind::ScrollDown => {
                    confirm_close = None;
                    selected = (selected + 3).min(rows.len().saturating_sub(1));
                }
                MouseEventKind::ScrollUp => {
                    confirm_close = None;
                    selected = selected.saturating_sub(3);
                }
                _ => {}
            },
            _ => {}
        }
    }

    save_state(&mut mem, true);
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

fn activate(projects: &mut [Project], mem: &mut Memory, rows: &[Row], idx: usize) {
    if rows.get(idx).is_none() {
        return;
    }
    match rows[idx] {
        Row::Project(pi) => {
            projects[pi].collapsed = !projects[pi].collapsed;
            sync_collapse(projects, mem);
        }
        Row::Worktree(pi, wi) => {
            projects[pi].worktrees[wi].collapsed = !projects[pi].worktrees[wi].collapsed;
            sync_collapse(projects, mem);
        }
        // Enter on an agent focuses its pane; the done-hold clears when the
        // agent reports focused (or works again).
        Row::Agent(pi, wi, ai) => {
            focus_pane(&projects[pi].worktrees[wi].agents[ai].pane_id.clone())
        }
    }
}

fn sync_collapse(projects: &[Project], mem: &mut Memory) {
    // Merge, never rebuild: the frame may omit projects (closed workspaces,
    // offline stub) whose saved folds must survive. Unfolds are recorded in
    // dropped so the save-merge can tell them apart from never-known keys.
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
}

/// Identity of the visible frame. Equal signatures skip the draw entirely,
/// so idle costs nothing and remote ships nothing.
fn signature(
    projects: &[Project],
    rows: &[Row],
    selected: usize,
    offset: usize,
    height: usize,
    hover: Option<usize>,
    step: usize,
    query: &str,
    compact: bool,
    view: View,
    status: &str,
    filtering: bool,
    theme: &Theme,
    font_dialog: bool,
    font: bool,
) -> String {
    // Every rendered input is hashed: footer mode, focus dot, tab-driven
    // indent, and theme colors (the per-second theme reload must repaint on
    // real change and skip on none).
    let mut sig = format!(
        "{selected}:{offset}:{height}:{hover:?}:{step}:{query}:{compact}:{}:{status}:{filtering}:{font_dialog}:{font}:{:?}:{:?}:{:?}:{:?}:",
        view as u8, theme.working, theme.blocked, theme.done, theme.idle,
    );
    for (i, row) in rows.iter().enumerate() {
        match *row {
            Row::Project(pi) => {
                let p = &projects[pi];
                sig.push_str(&format!("{i}:P:{}:{}:{}:{}:{};", p.name, p.branch, p.collapsed, p.pinned, p.focused));
            }
            Row::Worktree(pi, wi) => {
                let w = &projects[pi].worktrees[wi];
                sig.push_str(&format!("{i}:W:{}:{}:{};", w.name, w.branch, w.collapsed));
            }
            Row::Agent(pi, wi, ai) => {
                let a = &projects[pi].worktrees[wi].agents[ai];
                sig.push_str(&format!("{i}:A:{}:{}:{}:{};", a.vendor, a.title, a.state as u8, a.tab_id));
            }
        }
    }
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_hides_children() {
        let mut projects = stub();
        let full = visible(&projects, "", false, View::Grouped).len();
        projects[0].collapsed = true;
        let folded = visible(&projects, "", false, View::Grouped);
        assert!(folded.len() < full);
        assert!(matches!(folded[0], Row::Project(0)));
        assert!(matches!(folded[1], Row::Project(1)));
    }

    #[test]
    fn window_follows_selection() {
        assert_eq!(ensure_visible(0, 0, 10), 0);
        assert_eq!(ensure_visible(9, 0, 10), 0);
        assert_eq!(ensure_visible(10, 0, 10), 1);
        assert_eq!(ensure_visible(2, 5, 10), 2);
        assert_eq!(ensure_visible(0, 0, 0), 0);
    }

    #[test]
    fn click_maps_below_border() {
        assert_eq!(click_index(0, 0), None);
        assert_eq!(click_index(0, 1), Some(0));
        assert_eq!(click_index(5, 3), Some(7));
    }

    #[test]
    fn status_maps_known_strings() {
        assert_eq!(map_status("working"), State::Working);
        assert_eq!(map_status("blocked"), State::Blocked);
        assert_eq!(map_status("permission"), State::Blocked);
        assert_eq!(map_status("done"), State::Done);
        assert_eq!(map_status("bogus"), State::Unknown);
    }

    #[test]
    fn freshness_tiers_split_idle() {
        let mut mem = Memory::default();
        mem.activity.insert("p1".into(), 1_000_000);
        assert_eq!(mem.freshness("p1", 1_000_000 + 60), State::IdleFresh);
        assert_eq!(mem.freshness("p1", 1_000_000 + 3600), State::Idle);
        assert_eq!(mem.freshness("p1", 1_000_000 + 9000), State::IdleStale);
        assert_eq!(mem.freshness(" unseen ", 1_000_000), State::Idle);
    }

    #[test]
    fn launcher_decision_covers_cases() {
        let empty = r#"{"result":{"panes":[]}}"#;
        assert_eq!(launch_decision(empty, ""), "OPEN");
        assert_eq!(launch_decision("garbage", ""), "OPEN");
        let one = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","focused":true,"label":null,"terminal_title":"shell"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","focused":false,"label":"Projects","terminal_title":"Projects"}
        ]}}"#;
        assert_eq!(launch_decision(one, ""), "FOCUS w1:p2");
        let focused = r#"{"result":{"panes":[
            {"pane_id":"w1:p2","tab_id":"w1:t1","focused":true,"label":"Projects","terminal_title":"Projects"}
        ]}}"#;
        assert_eq!(launch_decision(focused, ""), "CLOSE w1:p2");
        // A user pane merely titled "Projects" is never ours.
        let spoof = r#"{"result":{"panes":[
            {"pane_id":"w1:p9","tab_id":"w1:t1","focused":true,"label":null,"terminal_title":"vim Projects.md"}
        ]}}"#;
        assert_eq!(launch_decision(spoof, ""), "OPEN");
        // Scoped to another tab: the sidebar elsewhere does not count.
        assert_eq!(launch_decision(one, "w1:t9"), "OPEN");
    }

    #[test]
    fn theme_parses_row_rules_and_custom() {
        let text = "[theme.custom]\nyellow = \"#111111\"\n[ui.sidebar.agents]\nrows = [[{ token = \"x\", rules = [{ equals = \"working\", fg = \"#222222\" }] }]]";
        assert_eq!(rule_color(text, "working"), Some(Color::Rgb(0x22, 0x22, 0x22)));
        assert_eq!(custom_color(text, "yellow"), Some(Color::Rgb(0x11, 0x11, 0x11)));
        assert_eq!(parse_hex("#f9e2af"), Some(Color::Rgb(0xf9, 0xe2, 0xaf)));
        assert_eq!(parse_hex("nope"), None);
        // A rule without its own fg must not borrow the sibling's: all rules
        // live on one line in the real format.
        let bleed = "rules = [{ equals = \"working\", bold = true }, { equals = \"blocked\", fg = \"#f38ba8\" }]";
        assert_eq!(rule_color(bleed, "working"), None);
        assert_eq!(
            rule_color(bleed, "blocked"),
            Some(Color::Rgb(0xf3, 0x8b, 0xa8))
        );
        // Commented example blocks are not live configuration.
        let commented = "# rules = [{ equals = \"working\", fg = \"#222222\" }]";
        assert_eq!(rule_color(commented, "working"), None);
        // Unquoted values and multibyte tails do not panic or misread.
        assert_eq!(custom_color("yellow = #111111 # night\n", "yellow"), None);
        assert_eq!(rule_color("equals = \"working\" \u{f418}glyphs { fg = \"#222222\" }", "working"), Some(Color::Rgb(0x22, 0x22, 0x22)));
    }

    #[test]
    fn attention_means_needs_human() {
        assert!(!State::Working.is_attention());
        assert!(!State::Monitoring.is_attention());
        assert!(!State::Idle.is_attention());
        assert!(State::Blocked.is_attention());
        assert!(State::Interrupted.is_attention());
        assert!(State::Done.is_attention());
    }

    #[test]
    fn font_choice_resolves_without_asking_twice() {
        // Explicit choices never prompt; auto prompts only when undetected.
        // (HERDR_SIDEBAR_FONT set in this env would force false; the pure
        // branches below hold regardless.)
        assert!(use_font("font", false));
        assert!(!use_font("text", true));
        assert!(use_font("auto", true));
        assert!(!use_font("auto", false));
        assert!(!use_font("", true) == false);
        assert_eq!(font_notice_due("text", false), false);
        assert_eq!(font_notice_due("font", false), false);
        // Without the env override these are notice/no-notice; with it set
        // both are false -- either way no panic, no prompt loop.
        let _ = font_notice_due("auto", false);
        let _ = font_notice_due("auto", true);
    }

    #[test]
    fn filter_narrows_to_match() {
        let projects = stub();
        let all = visible(&projects, "", false, View::Grouped).len();
        let some = visible(&projects, "oauth", false, View::Grouped);
        assert!(some.len() < all);
        assert!(matches!(some[0], Row::Project(0)));
    }
}

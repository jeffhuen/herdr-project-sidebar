//! herdr-project-sidebar: Collapsible project -> worktree -> agent tree.
//! Change-driven socket snapshots feed the custom tree. Failed refreshes keep
//! the last view but disable native actions until synchronization recovers.
//! Rendering remains signature-skipped and windowed, with 150ms animation.
//!
//! Theming: colors come from Herdr's own `config.toml` (`theme.custom` +
//! `ui.sidebar.agents` row rules) with built-in defaults when absent. Nothing
//! is written back; light/dark follows whatever theme is configured.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::activity::{now_unix_ms, ActivityStore, Freshness};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use serde::Deserialize;

const SPINNER: [&str; 8] = ["⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽", "⣾"];
const TICK: Duration = Duration::from_millis(150);
const IDLE_POLL: Duration = Duration::from_millis(300);
const NO_SELECTION: usize = usize::MAX;
const BRANCH_TTL: Duration = Duration::from_secs(5);
/// Explicit holds outvote adopted vetoes this long; afterwards recency is
/// unknowable and vetoes apply normally so stale pins always converge away.
const VETO_GRACE_SECS: u64 = 60;
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

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
    label: String,
    title: String,
    state: State,
    terminal_id: String,
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    seq: u64,
    focused: bool,
}

#[derive(Clone)]
struct Worktree {
    key: String,
    name: String,
    branch: String,
    collapsed: bool,
    depth: usize,
    /// Owning workspace: the focus target for this row.
    workspace_id: String,
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
    /// Member workspace ids (one for Solo). Header focus targets the first.
    workspaces: Vec<String>,
    worktrees: Vec<Worktree>,
}

#[derive(Clone, Copy)]
enum Row {
    Project(usize),
    Worktree(usize, usize),
    Agent(usize, usize, usize),
}

fn row_key<'a>(projects: &'a [Project], rows: &[Row], idx: usize) -> Option<(u8, &'a str)> {
    Some(match *rows.get(idx)? {
        Row::Project(pi) => (0, &projects[pi].id),
        Row::Worktree(pi, wi) => (1, &projects[pi].worktrees[wi].key),
        Row::Agent(pi, wi, ai) => (2, &projects[pi].worktrees[wi].agents[ai].terminal_id),
    })
}

fn row_index(projects: &[Project], rows: &[Row], key: (u8, &str)) -> Option<usize> {
    (0..rows.len()).find(|&idx| row_key(projects, rows, idx) == Some(key))
}

fn restore_selection(
    projects: &[Project],
    rows: &[Row],
    key: Option<(u8, &str)>,
    browsing: bool,
) -> usize {
    let preserved = key.and_then(|key| row_index(projects, rows, key));
    if browsing {
        return preserved.unwrap_or(NO_SELECTION);
    }
    let focused = projects
        .iter()
        .flat_map(|p| &p.worktrees)
        .flat_map(|w| &w.agents)
        .find(|a| a.focused)
        .map(|a| a.pane_id.as_str());
    seat_row(projects, rows, focused)
        .or(preserved)
        .unwrap_or(NO_SELECTION)
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
            workspaces: vec![],
            worktrees: vec![Worktree {
                key: "stub-a/main".into(),
                name: "main".into(),
                branch: "main".into(),
                collapsed: false,
                depth: 0,
                workspace_id: "".into(),
                agents: vec![
                    Agent {
                        vendor: "pi".into(),
                        label: "pi".into(),
                        title: "Implement OAuth scopes".into(),
                        state: State::Working,
                        terminal_id: "alpha".into(),
                        pane_id: "w1:p1".into(),
                        workspace_id: "".into(),
                        tab_id: "".into(),
                        seq: 0,
                        focused: false,
                    },
                    Agent {
                        vendor: "codex".into(),
                        label: "codex".into(),
                        title: "Wire retry budget".into(),
                        state: State::Done,
                        terminal_id: "beta".into(),
                        pane_id: "w1:p2".into(),
                        workspace_id: "".into(),
                        tab_id: "".into(),
                        seq: 0,
                        focused: false,
                    },
                    Agent {
                        vendor: "opencode".into(),
                        label: "opencode".into(),
                        title: "Migrate invoices table".into(),
                        state: State::Idle,
                        terminal_id: "gamma".into(),
                        pane_id: "w1:p3".into(),
                        workspace_id: "".into(),
                        tab_id: "".into(),
                        seq: 0,
                        focused: false,
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
            workspaces: vec![],
            worktrees: vec![Worktree {
                key: "stub-b/feature".into(),
                name: "sbr-9u4v.35".into(),
                branch: "feature/mc-13200".into(),
                collapsed: false,
                depth: 0,
                workspace_id: "".into(),
                agents: vec![Agent {
                    vendor: "claude".into(),
                    label: "claude".into(),
                    title: "Which env file should I edit?".into(),
                    state: State::Blocked,
                    terminal_id: "delta".into(),
                    pane_id: "w2:p1".into(),
                    workspace_id: "".into(),
                    tab_id: "".into(),
                    seq: 0,
                    focused: false,
                }],
            }],
        },
    ]
}

// ---------- Snapshot tree ----------

#[derive(Deserialize)]
struct AgentEntry {
    terminal_id: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    display_agent: Option<String>,
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
    state_change_seq: u64,
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
    #[serde(default)]
    repo_root: String,
    #[serde(default)]
    is_linked_worktree: bool,
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

/// Main checkout member: the workspace checked out at the repo root. Sort
/// position is activity, not ancestry, so the parent line is pinned by path
/// truth, never by who's busiest.
fn main_member<'a>(
    members: &[&'a str],
    ws_by_id: &BTreeMap<&str, &WorkspaceEntry>,
) -> Option<&'a str> {
    members.iter().copied().find(|m| {
        ws_by_id
            .get(*m)
            .and_then(|w| w.worktree.as_ref())
            .is_some_and(|t| {
                let c = t.checkout_path.trim_end_matches('/');
                !c.is_empty() && c == t.repo_root.trim_end_matches('/')
            })
    })
}

/// Nesting depth of a checkout inside its repo: 0 for the main line, 1 for a
/// sibling checkout, deeper for worktrees inside worktrees. Strict path
/// containment only; equal or unknown paths never nest (level 1, sibling).
fn nest_level(probes: &BTreeMap<&str, &str>, main: Option<&str>, member: &str) -> usize {
    if Some(member) == main {
        return 0;
    }
    let mut depth = 0;
    let mut cur = member;
    let mut cur_path = probes.get(member).copied().unwrap_or("");
    loop {
        // Nearest strict container: longest checkout path above cur. Paths
        // strictly shorten each hop, so this always terminates.
        let mut best: Option<(&str, &str)> = None;
        for (m, p) in probes.iter() {
            if *m == cur || p.is_empty() || cur_path.is_empty() {
                continue;
            }
            if cur_path.len() > p.len()
                && cur_path.starts_with(*p)
                && cur_path[p.len()..].starts_with('/')
                && best.is_none_or(|(_, bp): (&str, &str)| p.len() > bp.len())
            {
                best = Some((m, p));
            }
        }
        match best {
            Some((m, p)) => {
                depth += 1;
                cur = m;
                cur_path = p;
            }
            None => break,
        }
    }
    1 + depth
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
            let p = dir.parent()?;
            dir = p.to_path_buf();
            continue;
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

/// True when `start` is a linked git worktree: `.git` is a file whose
/// `gitdir:` pointer runs through a `/worktrees/` dir. Submodules also use
/// a `.git` file but point at `/modules/`, so they stay plain checkouts.
fn checkout_is_linked(start: &str) -> bool {
    let text = match std::fs::read_to_string(std::path::Path::new(start).join(".git")) {
        Ok(text) => text,
        Err(_) => return false,
    };
    text.lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .is_some_and(|pointer| pointer.contains("/worktrees/"))
}

/// Short-term memory across snapshots: activity stamps, held badges, collapse.
#[derive(Default)]
struct Memory {
    activity: ActivityStore,
    collapsed_projects: BTreeSet<String>,
    collapsed_worktrees: BTreeSet<String>,
    pinned: BTreeSet<String>,
    /// Keys this instance unfolded/unpinned ("c:<id>", "w:<key>", "p:<id>"):
    /// the save-merge subtracts them so another sidebar's file copy cannot
    /// resurrect a removal the user just made here.
    dropped: BTreeSet<String>,
    /// Keys explicitly folded/pinned HERE, with unix secs: our fresh holds
    /// overrule adopted vetoes so a deliberate re-pin sticks. Touched expires
    /// (VETO_GRACE) so a stale pin can never outvote a later unpin elsewhere.
    touched: BTreeMap<String, u64>,
    /// Glyph set: "auto" (detect Nerd Font, fall back silently), "font"
    /// (assume it), "text" (ASCII always, never ask). First-run notice flips
    /// this off auto once the user chooses.
    font_choice: String,
    dirty_state: bool,
    last_state_save: Option<std::time::Instant>,
    /// checkout path -> branch, via native worktree.list (5s TTL).
    branches: BTreeMap<String, String>,
    linked_checkouts: HashSet<String>,
    branch_at: Option<std::time::Instant>,
}

impl Memory {
    fn freshness(&self, terminal_id: &str, now_ms: u64) -> State {
        match self.activity.freshness(terminal_id, now_ms) {
            Freshness::Fresh => State::IdleFresh,
            Freshness::Normal => State::Idle,
            Freshness::Stale => State::IdleStale,
        }
    }
}

/// Refresh optional branch metadata independently of snapshot tree construction.
fn refresh_branches(snap: &serde_json::Value, mem: &mut Memory) {
    if mem.branch_at.is_some_and(|at| at.elapsed() < BRANCH_TTL) {
        return;
    }
    let workspaces = snap["workspaces"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let agents = snap["agents"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let roots: BTreeSet<&str> = workspaces
        .iter()
        .filter_map(|w| w.pointer("/worktree/repo_root").and_then(|v| v.as_str()))
        .filter(|root| !root.is_empty())
        .collect();
    mem.branches =
        crate::ipc::branch_map(&roots.into_iter().map(str::to_owned).collect::<Vec<_>>());
    mem.linked_checkouts.clear();
    let probes: BTreeSet<&str> = workspaces
        .iter()
        .filter_map(|w| {
            w.pointer("/worktree/checkout_path")
                .and_then(|v| v.as_str())
        })
        .chain(agents.iter().filter_map(|a| {
            a["foreground_cwd"]
                .as_str()
                .filter(|p| !p.is_empty())
                .or_else(|| a["cwd"].as_str())
        }))
        .filter(|probe| !probe.is_empty())
        .collect();
    for probe in probes {
        let key = probe.trim_end_matches('/');
        if !mem.branches.contains_key(key) {
            if let Some(branch) = git_branch(probe) {
                mem.branches.insert(key.to_owned(), branch);
            }
        }
        if checkout_is_linked(probe) {
            mem.linked_checkouts.insert(probe.to_owned());
        }
    }
    mem.branch_at = Some(std::time::Instant::now());
}

fn install_snapshot(
    snap: &crate::ipc::Snapshot,
    mem: &mut Memory,
    theme_projects: &[Color],
    font: bool,
) -> io::Result<Vec<Project>> {
    if mem.activity.sync_session(&snap.session)? {
        mem.branches.clear();
        mem.linked_checkouts.clear();
        mem.branch_at = None;
    }
    refresh_branches(&snap.data, mem);
    snap.session.check()?;
    let now_ms = now_unix_ms();
    let mut projects = snapshot(&snap.data, mem, theme_projects, font, now_ms)?;
    let live = projects
        .iter()
        .flat_map(|p| &p.worktrees)
        .flat_map(|w| &w.agents)
        .map(|a| a.terminal_id.as_str())
        .collect();
    mem.activity.retain_live(&live);
    for a in projects
        .iter()
        .flat_map(|p| &p.worktrees)
        .flat_map(|w| &w.agents)
    {
        if matches!(a.state, State::Working | State::Monitoring) {
            mem.activity.mark_working(&a.terminal_id, now_ms);
        }
    }
    // Convert old member folds once. Keeping the alias would resurrect an
    // unfolded repo on its next snapshot or another dock's state merge.
    for project in &mut projects {
        if project.id.starts_with("repo:") {
            let mut migrated = false;
            for workspace in &project.workspaces {
                if mem.collapsed_projects.remove(workspace) {
                    touch_release(mem, &format!("c:{workspace}"));
                    migrated = true;
                }
            }
            if migrated {
                let key = format!("c:{}", project.id);
                if !mem.dropped.contains(&key) {
                    mem.collapsed_projects.insert(project.id.clone());
                    project.collapsed = true;
                }
                mem.dirty_state = true;
            }
        }
    }
    Ok(projects)
}

fn agent_title(e: &AgentEntry) -> String {
    let name = e.agent.as_str();
    if let Some(t) = e.title.as_deref().filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    let raw = e
        .terminal_title_stripped
        .as_deref()
        .filter(|t| !t.is_empty())
        .or_else(|| e.terminal_title.as_deref().filter(|t| !t.is_empty()));

    if let Some(t) = raw {
        let clean = if matches!(name, "omp" | "pi") {
            t.strip_prefix("π ")
                .or_else(|| t.strip_prefix("π"))
                .unwrap_or(t)
                .trim_start_matches(|ch: char| {
                    ch.is_whitespace() || ch == '>' || ('\u{2800}'..='\u{28ff}').contains(&ch)
                })
                .trim_start()
        } else {
            t
        };
        if !clean.is_empty() && clean != "None" {
            return clean.to_string();
        }
    }

    if !name.is_empty() && name != "?" {
        return name.to_string();
    }

    std::path::Path::new(&e.cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| e.cwd.clone())
}

/// Row index of an agent pane in the current view, if it is listed.
fn focus_row(projects: &[Project], rows: &[Row], pane_id: &str) -> Option<usize> {
    rows.iter().position(|r| match *r {
        Row::Agent(pi, wi, ai) => projects[pi].worktrees[wi].agents[ai].pane_id == pane_id,
        _ => false,
    })
}

/// Seat target: the focused session's row, else the focused space's header.
/// Never outside the active space.
fn seat_row(projects: &[Project], rows: &[Row], focused_pane: Option<&str>) -> Option<usize> {
    focused_pane
        .and_then(|id| focus_row(projects, rows, id))
        .or_else(|| {
            rows.iter()
                .position(|r| matches!(*r, Row::Project(pi) if projects[pi].focused))
        })
}

/// Build the tree from one complete snapshot. No transport or filesystem I/O.
fn snapshot(
    snap: &serde_json::Value,
    mem: &Memory,
    theme_projects: &[Color],
    font_ok: bool,
    now_ms: u64,
) -> io::Result<Vec<Project>> {
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut agents: Vec<AgentEntry> = serde_json::from_value(snap["agents"].clone())
        .map_err(|e| invalid(format!("invalid snapshot agents: {e}")))?;
    let workspaces: Vec<WorkspaceEntry> = serde_json::from_value(snap["workspaces"].clone())
        .map_err(|e| invalid(format!("invalid snapshot workspaces: {e}")))?;
    let mut terminals = HashSet::new();
    if workspaces.iter().any(|w| w.workspace_id.trim().is_empty())
        || agents.iter().any(|a| {
            a.terminal_id.trim().is_empty()
                || !terminals.insert(a.terminal_id.as_str())
                || a.pane_id.trim().is_empty()
                || !workspaces.iter().any(|w| w.workspace_id == a.workspace_id)
        })
    {
        return Err(invalid(
            "snapshot has missing or duplicate agent identity or workspace".into(),
        ));
    }
    drop(terminals);
    let ws_by_id: BTreeMap<&str, &WorkspaceEntry> = workspaces
        .iter()
        .map(|w| (w.workspace_id.as_str(), w))
        .collect();
    fn repo_of(ws_by_id: &BTreeMap<&str, &WorkspaceEntry>, ws_id: &str) -> Option<String> {
        ws_by_id
            .get(ws_id)
            .and_then(|w| w.worktree.as_ref())
            .map(|t| t.repo_key.clone())
            .filter(|k| !k.is_empty())
    }
    /// One repo shared by many checkouts is one project; a workspace Herdr
    /// never attached to a repo stands alone exactly as before.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum PJKey {
        Repo(String),
        Solo(String),
    }
    // Group agents: repo -> member workspace -> agents. Solo workspaces keep
    // the old repo_key/cwd split so multi-root spaces still separate.
    let mut groups: BTreeMap<PJKey, BTreeMap<String, Vec<AgentEntry>>> = BTreeMap::new();
    // ponytail: drain keeps one pass; agents is rebuilt by the caller each frame.
    for a in agents.drain(..) {
        match repo_of(&ws_by_id, &a.workspace_id) {
            Some(repo) => {
                groups
                    .entry(PJKey::Repo(repo))
                    .or_default()
                    .entry(a.workspace_id.clone())
                    .or_default()
                    .push(a);
            }
            None => {
                groups
                    .entry(PJKey::Solo(a.workspace_id.clone()))
                    .or_default()
                    .entry(format!("cwd:{}", a.cwd))
                    .or_default()
                    .push(a);
            }
        }
    }
    // Agent-less checkouts still seed a member: Herdr knows the worktree
    // (fresh branch, all panes closed) and the branch probes from disk.
    // or_insert (not or_insert_with): the repo usually exists already via a
    // sibling checkout, and only the missing member is added.
    for w in &workspaces {
        match repo_of(&ws_by_id, &w.workspace_id) {
            Some(repo) => {
                groups
                    .entry(PJKey::Repo(repo))
                    .or_default()
                    .entry(w.workspace_id.clone())
                    .or_default();
            }
            None => {
                // An occupied Solo workspace already has its cwd groups.
                groups
                    .entry(PJKey::Solo(w.workspace_id.clone()))
                    .or_insert_with(|| BTreeMap::from([(w.workspace_id.clone(), Vec::new())]));
            }
        }
    }

    let mut projects = Vec::new();
    for (key, members) in &groups {
        // Solo identity is the workspace; Repo identity is the shared repo.
        let (id, solo_ws) = match key {
            PJKey::Repo(repo) => (format!("repo:{repo}"), None),
            PJKey::Solo(ws_id) => (ws_id.clone(), ws_by_id.get(ws_id.as_str()).copied()),
        };
        let solo_label = solo_ws
            .map(|w| w.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| match key {
                PJKey::Repo(_) => String::new(),
                PJKey::Solo(ws_id) => format!("external ({ws_id})"),
            });
        // Repo ancestry before rows: the main checkout anchors the parent
        // line (never sort luck), containment sets each row's depth.
        let member_ids: Vec<&str> = members.keys().map(String::as_str).collect();
        let (main, depths): (Option<String>, BTreeMap<String, usize>) = match key {
            PJKey::Repo(_) => {
                let main = main_member(&member_ids, &ws_by_id).map(str::to_owned);
                let probes: BTreeMap<&str, &str> = member_ids
                    .iter()
                    .filter_map(|m| {
                        ws_by_id
                            .get(*m)
                            .and_then(|w| w.worktree.as_ref())
                            .map(|t| (*m, t.checkout_path.trim_end_matches('/')))
                            .filter(|(_, c)| !c.is_empty())
                    })
                    .collect();
                let main_ref = main.as_deref();
                let depths = member_ids
                    .iter()
                    .map(|m| (m.to_string(), nest_level(&probes, main_ref, m)))
                    .collect();
                (main, depths)
            }
            PJKey::Solo(_) => (None, BTreeMap::new()),
        };
        let mut worktrees = Vec::new();
        for (sub, entries) in members {
            let member_ws = match key {
                PJKey::Repo(_) => ws_by_id.get(sub.as_str()).copied(),
                PJKey::Solo(_) => solo_ws,
            };
            let wt = member_ws.and_then(|w| w.worktree.as_ref());
            // Agent-less solo members contribute no row; the project below
            // renders the bare header exactly as before.
            if entries.is_empty() && wt.is_none() {
                continue;
            }
            let repo_key = match key {
                PJKey::Repo(repo) => repo.clone(),
                PJKey::Solo(_) => sub.clone(),
            };
            let name = wt
                .map(|t| t.repo_name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| member_ws.map(|w| w.label.clone()).filter(|l| !l.is_empty()))
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
                    entries
                        .first()
                        .and_then(|e| e.foreground_cwd.clone())
                        .filter(|p| !p.is_empty())
                })
                .or_else(|| entries.first().map(|e| e.cwd.clone()))
                .unwrap_or_default();
            let branch = if probe.is_empty() {
                String::new()
            } else {
                mem.branches
                    .get(probe.trim_end_matches('/'))
                    .filter(|b| !b.is_empty())
                    .cloned()
                    .unwrap_or_default()
            };
            let mut list: Vec<Agent> = entries
                .iter()
                .map(|e| {
                    let mut st = map_status(&e.agent_status);
                    if st == State::Idle {
                        st = mem.freshness(&e.terminal_id, now_ms);
                    }
                    Agent {
                        vendor: if e.agent.is_empty() {
                            "?".into()
                        } else {
                            e.agent.clone()
                        },
                        label: e
                            .display_agent
                            .clone()
                            .filter(|l| !l.is_empty())
                            .unwrap_or_else(|| {
                                if e.agent.is_empty() {
                                    "?".into()
                                } else {
                                    e.agent.clone()
                                }
                            }),
                        title: agent_title(e),
                        state: st,
                        terminal_id: e.terminal_id.clone(),
                        pane_id: e.pane_id.clone(),
                        tab_id: e.tab_id.clone(),
                        workspace_id: e.workspace_id.clone(),
                        seq: e.state_change_seq,
                        focused: e.focused,
                    }
                })
                .collect();
            list.sort_by_key(|a| (a.tab_id.clone(), a.pane_id.clone()));
            let linked =
                wt.is_some_and(|t| t.is_linked_worktree) || mem.linked_checkouts.contains(&probe);
            let checkout_name = std::path::Path::new(&probe)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty());
            // Linked rows carry the worktree's own name, plain checkouts the
            // branch. The repo name is never repeated: the header owns it.
            // Places read from the tree icon; no trailing slash needed.
            let label = if branch.is_empty() {
                name
            } else if linked {
                checkout_name.unwrap_or_else(|| branch.clone())
            } else {
                branch.clone()
            };
            let wt_key = match key {
                PJKey::Repo(_) => format!("{sub}::{probe}"),
                PJKey::Solo(_) => format!("{id}::{repo_key}"),
            };
            // Solo keeps the old shape (first row flat, rest one in); Repo
            // depth comes from ancestry, defaulting to sibling level.
            let depth = match key {
                PJKey::Repo(_) => depths.get(sub.as_str()).copied().unwrap_or(1),
                PJKey::Solo(_) => usize::from(!worktrees.is_empty()),
            };
            worktrees.push(Worktree {
                key: wt_key.clone(),
                name: label,
                branch,
                collapsed: mem.collapsed_worktrees.contains(&wt_key),
                depth,
                workspace_id: match key {
                    PJKey::Repo(_) => sub.clone(),
                    PJKey::Solo(_) => id.clone(),
                },
                agents: list,
            });
        }
        // The main checkout owns the parent line even when idle; activity
        // sorts the rest. Key prefix match is exact: ids never contain "::".
        if let Some(m) = &main {
            let prefix = format!("{m}::");
            if let Some(pos) = worktrees.iter().position(|w| w.key.starts_with(&prefix)) {
                let row = worktrees.remove(pos);
                worktrees.insert(0, row);
            }
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
        let member_ids: Vec<&str> = match key {
            PJKey::Repo(_) => members.keys().map(String::as_str).collect(),
            PJKey::Solo(ws_id) => vec![ws_id.as_str()],
        };
        let ws_of = |m: &str| ws_by_id.get(m).copied();
        // Repo name: shared repo_name wins, then a member label, then the
        // repo path's parent (repo_key itself usually ends in `.git`).
        let name = match key {
            PJKey::Solo(_) => solo_label.clone(),
            PJKey::Repo(repo) => member_ids
                .iter()
                .filter_map(|m| ws_of(m))
                .filter_map(|w| w.worktree.as_ref().map(|t| t.repo_name.clone()))
                .find(|n| !n.is_empty())
                .or_else(|| {
                    member_ids
                        .iter()
                        .filter_map(|m| ws_of(m))
                        .map(|w| w.label.clone())
                        .find(|l| !l.is_empty())
                })
                .unwrap_or_else(|| {
                    std::path::Path::new(repo)
                        .parent()
                        .and_then(|d| d.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| repo.clone())
                }),
        };
        let (pinned, focused, collapsed) = match key {
            PJKey::Repo(_) => (
                mem.pinned.contains(&id),
                member_ids
                    .iter()
                    .filter_map(|m| ws_of(m))
                    .any(|w| w.focused),
                mem.collapsed_projects.contains(&id),
            ),
            PJKey::Solo(_) => (
                mem.pinned.contains(&id),
                solo_ws.map(|w| w.focused).unwrap_or(false),
                mem.collapsed_projects.contains(&id),
            ),
        };
        projects.push(Project {
            icon_color: theme_projects[project_hue(&id) % theme_projects.len()],
            icon: if font_ok { '' } else { '#' },
            id,
            name,
            branch,
            collapsed,
            pinned,
            focused,
            workspaces: member_ids.iter().map(|m| m.to_string()).collect(),
            worktrees,
        });
    }
    // Agent-less members seed rows above, so every project here already
    // carries its worktrees (possibly empty) and the sort below applies.
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
    Ok(projects)
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
        if line.starts_with(key)
            && line
                .get(key.len()..)
                .map(|r| r.starts_with([' ', '\t', '=']))
                .unwrap_or(false)
        {
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
            Color::Rgb(0x89, 0xdc, 0xeb),
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
    if std::env::var("HERDR_SIDEBAR_FONT")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
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
                if e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("nerd")
                {
                    return true;
                }
            }
        }
    }
    // The binary's own dir may ship beside a font marker; Ghostty/kitty with
    // the radar icon font also satisfy this via config below.
    std::env::var("HERDR_SIDEBAR_FONT")
        .map(|v| v == "1")
        .unwrap_or(false)
}

// ---------- state.json ----------

fn state_path() -> std::path::PathBuf {
    crate::config::state_dir().join("state.json")
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
    mem.dropped = get("dropped").into_iter().collect();
    apply_dropped(mem);
    mem.font_choice = v
        .get("font_choice")
        .and_then(|x| x.as_str())
        .unwrap_or("auto")
        .to_string();
}

/// Fold/unfold removals shared across instances: an unpin in one dock vetoes
/// the key everywhere until no instance holds it anymore. Split "ns:id".
fn drop_parts(key: &str) -> (&str, &str) {
    key.split_once(':').unwrap_or(("", key))
}

/// Subtract shared drops from our own sets (adopt other instances' unpins).
fn touch_hold(mem: &mut Memory, key: &str) {
    mem.dropped.remove(key);
    mem.touched.insert(key.to_owned(), now_secs());
}

fn touch_release(mem: &mut Memory, key: &str) {
    mem.touched.remove(key);
    mem.dropped.insert(key.to_owned());
}

fn apply_dropped(mem: &mut Memory) {
    for k in mem.dropped.iter() {
        let (ns, plain) = drop_parts(k);
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
    // Merge with the file: a dock in another native session may have saved
    // folds or pins since our load.
    // Union for adds. Removals are SHARED vetoes: adopt the
    // file's dropped set (another dock's unpin applies here too), republish
    // the merge, prune vetoes nothing holds anymore. Explicit holds here
    // (touched) overrule adopted vetoes, so a deliberate re-pin sticks.
    let file = read_state_file();
    mem.dropped.extend(
        file.get("dropped")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)))
            .into_iter()
            .flatten(),
    );
    let now = now_secs();
    mem.touched
        .retain(|_, at| now.saturating_sub(*at) < VETO_GRACE_SECS);
    for t in mem.touched.keys() {
        mem.dropped.remove(t);
    }
    apply_dropped(mem);
    // Vetoes stay sticky once any side records them: a third idle dock that
    // missed the window still converges on its next save (it adopts before it
    // unions), and only an explicit hold here clears them (see touched).
    let union = |k: &str, ns: &str, own: &BTreeSet<String>| {
        let mut out = own.clone();
        if let Some(a) = file.get(k).and_then(|x| x.as_array()) {
            out.extend(a.iter().filter_map(|x| {
                x.as_str()
                    .filter(|s| !mem.dropped.contains(&format!("{ns}:{s}")))
                    .map(str::to_string)
            }));
        }
        out.into_iter().collect::<Vec<_>>()
    };
    let v = serde_json::json!({
        "collapsed_projects": union("collapsed_projects", "c", &mem.collapsed_projects),
        "collapsed_worktrees": union("collapsed_worktrees", "w", &mem.collapsed_worktrees),
        "pinned": union("pinned", "p", &mem.pinned),
        "font_choice": mem.font_choice,
        "dropped": mem.dropped.iter().collect::<Vec<_>>(),
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

/// Native pane navigation also works if agent detection changes after rendering.
fn focus_session(session: &crate::ipc::Session, pane_id: &str) -> io::Result<()> {
    session
        .call("pane.focus", serde_json::json!({ "pane_id": pane_id }))
        .map(|_| ())
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
            || w.agents
                .iter()
                .any(|a| a.title.to_lowercase().contains(&q) || a.label.to_lowercase().contains(&q))
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
                (a.state.rank(), std::cmp::Reverse(a.seq))
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
/// Visual line -> row mapping for the current page: None entries are
/// non-selectable padding (one blank at the top, one between groups).
/// A worktree stays attached to its project header; the gap opens only above
/// later worktrees and above every project. Agents always sit directly under
/// their worktree.
fn visual_rows_for_page(rows: &[Row], offset: usize, height: usize) -> Vec<Option<usize>> {
    if height == 0 {
        return Vec::new();
    }
    let mut vis = Vec::with_capacity(height);
    vis.push(None); // breathing room above the tree
    let mut first = true;
    for (idx, row) in rows.iter().enumerate().skip(offset) {
        let follows_project = match vis.last() {
            Some(Some(prev)) => matches!(rows[*prev], Row::Project(_)),
            _ => false,
        };
        // Gap above every project and above later worktrees so rows never
        // touch. Gaps always render blank: attachment reads from the tree
        // grammar (fork/tree/connector), never from rules.
        let gap = !first
            && (matches!(row, Row::Project(_))
                || (matches!(row, Row::Worktree(_, _)) && !follows_project));
        // Fill by visual lines, never row count: gaps inflate the page, and
        // a row-count window plus truncate amputates the tail (cursor included).
        if vis.len() + if gap { 2 } else { 1 } > height {
            break;
        }
        if gap {
            vis.push(None);
        }
        vis.push(Some(idx));
        first = false;
    }
    vis
}

/// Slide the row window until the cursor is actually drawn: row-count
/// offsets cannot see gap inflation, so a far cursor needs a nudge past
/// the estimate. Always terminates (offset is bounded by the rows).
fn window_for_selected(
    rows: &[Row],
    mut offset: usize,
    height: usize,
    selected: usize,
) -> (Vec<Option<usize>>, usize) {
    let mut vis = visual_rows_for_page(rows, offset, height);
    if height >= 2 && selected < rows.len() {
        while !vis.contains(&Some(selected)) && offset + 1 < rows.len() {
            offset += 1;
            vis = visual_rows_for_page(rows, offset, height);
        }
    }
    (vis, offset)
}

/// Map a mouse y (absolute terminal row, content from row 0) through the
/// current visual mapping. Padding and the bottom toolbar hit None.
fn visual_hit(vis: &[Option<usize>], y: u16) -> Option<usize> {
    vis.get(y as usize).copied().flatten()
}

/// Which modal control a click lands on. Button precedence mirrors the
/// render order: Close, the width arrows, then rows. The arrows sit inside
/// row 0's own rect, so rows come last or the width buttons become dead.
enum ModalHit {
    Close,
    Less,
    More,
    Row(usize),
}

fn modal_hit(
    col: u16,
    row: u16,
    close: ratatui::layout::Rect,
    less: ratatui::layout::Rect,
    more: ratatui::layout::Rect,
    rows: &[ratatui::layout::Rect; 10],
) -> Option<ModalHit> {
    let at = |r: ratatui::layout::Rect| {
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
    };
    if at(close) {
        Some(ModalHit::Close)
    } else if at(less) {
        Some(ModalHit::Less)
    } else if at(more) {
        Some(ModalHit::More)
    } else {
        rows.iter().position(|r| at(*r)).map(ModalHit::Row)
    }
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
        State::Blocked => (crate::icons::blocked_mark(tick), false),
        State::Interrupted => ("!", false),
        State::IdleFresh => ("●", false),
        State::Idle => ("·", false),
        State::IdleStale => ("·", false),
        State::Unknown => ("◌", false),
    }
}

fn vendor_color(vendor: &str) -> Color {
    match vendor {
        "claude" => Color::Rgb(0xd9, 0x77, 0x57),
        "gemini" => Color::Rgb(0x42, 0x85, 0xf4),
        "kimi" => Color::Rgb(0x17, 0x83, 0xff),
        "deepseek" => Color::Rgb(0x4d, 0x6b, 0xfe),
        "qwen" => Color::Rgb(0x61, 0x5c, 0xed),
        "kiro" => Color::Rgb(0x90, 0x46, 0xff),
        "cline" => Color::Rgb(0x58, 0x68, 0x76),
        "kilo" => Color::Rgb(0x9a, 0x98, 0x08),
        "omp" => Color::Rgb(0xcb, 0xa6, 0xf7),
        "pi" => Color::Rgb(0xfa, 0xb3, 0x87),
        _ => Color::Rgb(0xc7, 0x8a, 0x1f),
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

pub fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--dump-snapshot") {
        let mut mem = Memory {
            activity: ActivityStore::new(crate::config::state_dir().join("dock")),
            ..Memory::default()
        };
        load_state(&mut mem);
        let font = use_font(&mem.font_choice, font_ok());
        let theme = load_theme();
        let result = (|| {
            let snap = crate::ipc::session_snapshot()?;
            install_snapshot(&snap, &mut mem, &theme.projects, font)
        })();
        let (projects, err) = match result {
            Ok(projects) => (projects, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
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
        println!(
            "{}",
            serde_json::json!({"projects": dump, "rows": rows.len(), "error": err})
        );
        return Ok(());
    }

    let mut mem = Memory {
        activity: ActivityStore::new(crate::config::state_dir().join("dock")),
        ..Memory::default()
    };
    load_state(&mut mem);
    // Glyph set: explicit choice wins, auto detects once. The notice below
    // is the install prompt: it explains what the font is for and remembers.
    let mut font_detected = font_ok();
    let mut font = use_font(&mem.font_choice, font_detected);
    let mut font_notice = font_notice_due(&mem.font_choice, font_detected);
    let mut font_dialog = false;
    let mut visual_rows: Vec<Option<usize>> = Vec::new();
    let mut settings_dialog = false;
    let mut settings_row = 0usize;
    let mut settings_offset = 0usize;
    let mut settings_obj = crate::config::load().unwrap_or_default();
    let mut settings_btn = ratatui::layout::Rect::default();
    let mut close_btn = ratatui::layout::Rect::default();
    let mut less_btn = ratatui::layout::Rect::default();
    let mut more_btn = ratatui::layout::Rect::default();
    let mut row_rects = [ratatui::layout::Rect::default(); 10];
    let mut theme = load_theme();
    let mut sync = crate::ipc::SnapshotSync::new();
    let mut projects: Vec<Project> = Vec::new();
    let mut session: Option<crate::ipc::Session> = None;
    let mut sync_error: Option<String> = None;
    let mut last_config = std::time::Instant::now();
    let mut last_tick = std::time::Instant::now();
    let mut selected = NO_SELECTION;
    let mut browsing = false;
    let mut pressed: Option<(u8, String)> = None;
    let mut pending_activation: Option<PendingActivation> = None;
    let mut offset = 0usize;
    let mut tick = 0usize;
    let mut hover: Option<usize> = None;
    let mut last_drawn = String::new();
    let mut query = String::new();
    let mut filtering = false;
    let mut compact = false;
    let mut view = View::Grouped;
    let mut confirm_close: Option<Vec<String>> = None;
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        SetTitle(PANE_TITLE)
    )?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    // RAII: every `?` below returns through this drop, so an I/O error can
    // never leave raw mode + alternate screen + mouse capture armed. (Panics
    // take the hook above.) State loss is bounded by the 5s debounced save.
    let _term_guard = TermGuard;
    loop {
        if last_config.elapsed() >= Duration::from_secs(1) {
            if !settings_dialog {
                settings_obj = crate::config::load()?;
            }
            theme = load_theme();
            for project in &mut projects {
                project.icon_color =
                    theme.projects[project_hue(&project.id) % theme.projects.len()];
                project.icon = if font { '' } else { '#' };
            }
            last_config = std::time::Instant::now();
        }
        let refresh = sync.poll().and_then(|snap| {
            let Some(snap) = snap else { return Ok(None) };
            let fresh = install_snapshot(&snap, &mut mem, &theme.projects, font)?;
            Ok(Some((snap.session, fresh)))
        });
        match refresh {
            Ok(Some((fresh_session, fresh))) => {
                if session.as_ref().map(crate::ipc::Session::key) != Some(fresh_session.key()) {
                    selected = NO_SELECTION;
                    pressed = None;
                    pending_activation = None;
                    confirm_close = None;
                    browsing = false;
                    offset = 0;
                }
                let previous_rows = visible(&projects, &query, compact, view);
                let key = row_key(&projects, &previous_rows, selected)
                    .map(|(kind, id)| (kind, id.to_owned()));
                projects = fresh;
                session = Some(fresh_session);
                let current_rows = visible(&projects, &query, compact, view);
                selected = restore_selection(
                    &projects,
                    &current_rows,
                    key.as_ref().map(|(kind, id)| (*kind, id.as_str())),
                    browsing || filtering || settings_dialog || confirm_close.is_some(),
                );
                if selected == NO_SELECTION {
                    confirm_close = None;
                }
                hover = None;
                if pending_activation.as_ref().is_some_and(|intent| {
                    row_index(&projects, &current_rows, (intent.key.0, &intent.key.1)).is_none()
                }) {
                    pending_activation = None;
                    selected = NO_SELECTION;
                }
                sync_error = None;
            }
            Ok(None) => {}
            Err(error) => {
                sync_error = Some(error.to_string());
                pressed = None;
                pending_activation = None;
                confirm_close = None;
                sync.invalidate();
            }
        }
        let activity_error = mem
            .activity
            .save(false)
            .err()
            .map(|error| error.to_string());
        save_state(&mut mem, false);
        let rows = visible(&projects, &query, compact, view);
        if !sync.is_stale() && sync_error.is_none() {
            if let Some(intent) = pending_activation.take() {
                if let Some((idx, target)) = intent.resolve(&projects, &rows) {
                    selected = idx;
                    browsing = false;
                    if let Some(session) = &session {
                        if let Err(error) = activate_target(session, target) {
                            status_line = error.to_string();
                        }
                        sync.invalidate();
                    }
                } else {
                    selected = NO_SELECTION;
                }
            }
        }
        if selected != NO_SELECTION && selected >= rows.len() {
            selected = rows.len().saturating_sub(1);
        }
        let height = term.size()?.height.saturating_sub(1) as usize;
        // Clamp the window itself: when rows shrink under a high offset the
        // viewport would otherwise anchor on the last row and draw blanks.
        if selected != NO_SELECTION {
            offset = ensure_visible(selected, offset, height);
        }
        offset = offset.min(rows.len().saturating_sub(height));
        let working = rows.iter().any(|r| match *r {
            Row::Agent(pi, wi, ai) => {
                let (g, anim) = state_glyph(projects[pi].worktrees[wi].agents[ai].state, 0, font);
                let _ = g;
                anim
            }
            _ => false,
        });
        let step = if working { tick % SPINNER.len() } else { 0 };
        let mut sig = signature(&SigInput {
            projects: &projects,
            rows: &rows,
            selected,
            offset,
            height,
            hover,
            step,
            query: &query,
            compact,
            view,
            status: &status_line,
            filtering,
            theme: &theme,
            font_dialog,
            font,
            settings_dialog,
            settings_row,
            settings_obj: &settings_obj,
        });
        sig.push_str(&format!(
            "{sync_error:?}:{activity_error:?}:{}",
            sync.is_stale()
        ));
        if sig != last_drawn {
            let (agents, working_n, blocked, unread) = counts(&projects);
            // First visible agent row per tab, in display order: drives the
            // split-child indent. Computed over all rows (not the window) so
            // scrolled-off leaders still count. Same-tab agents sort
            // contiguously (see snapshot sort), so followers always hang
            // under their own tab leader, never a foreign tab's row.
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut first_rows: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
            for r in &rows {
                if let Row::Agent(pi, wi, ai) = *r {
                    if seen.insert(projects[pi].worktrees[wi].agents[ai].tab_id.as_str()) {
                        first_rows.insert((pi, wi, ai));
                    }
                }
            }
            let (vis, slid) = window_for_selected(&rows, offset, height, selected);
            offset = slid;
            let mut lines = Vec::with_capacity(vis.len());
            for entry in &vis {
                let Some(idx) = entry else {
                    lines.push(Line::from(""));
                    continue;
                };
                let idx = *idx;
                let row = &rows[idx];
                let mut line = match *row {
                    Row::Project(pi) => {
                        let p = &projects[pi];
                        let mark = if p.collapsed { "▸" } else { "▾" };
                        // Nerd pin by codepoint (no literal: survives any transport); ASCII star fallback.
                        let pin = if p.pinned {
                            pin_mark(font).to_string()
                        } else {
                            String::new()
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{} {} ", p.icon, mark),
                                Style::default()
                                    .fg(p.icon_color)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("{}{} ", p.name, pin),
                                if p.focused {
                                    Style::default()
                                        .fg(theme.working)
                                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                                } else {
                                    Style::default().add_modifier(Modifier::BOLD)
                                },
                            ),
                        ])
                    }
                    Row::Worktree(pi, wi) => {
                        let w = &projects[pi].worktrees[wi];
                        let mark = if w.collapsed { "▸" } else { "▾" };
                        // Places, not pointers: the main line carries the branch
                        // fork, peer checkouts a tree; only true children take
                        // the └─ connector (plus one indent per level below).
                        // No-font tokens follow git CLI: `*` = current branch,
                        // `+` = worktree-linked.
                        let lead = if w.depth == 0 {
                            let fork = if font {
                                char::from_u32(0xf418).unwrap_or('*')
                            } else {
                                '*'
                            };
                            format!("  {fork} ")
                        } else {
                            let tree = if font {
                                char::from_u32(0xf1bb).unwrap_or('+')
                            } else {
                                '+'
                            };
                            let link = if w.depth > 1 { "└─ " } else { "" };
                            format!("  {}{link}{tree} ", "  ".repeat(w.depth - 1))
                        };
                        Line::from(vec![
                            Span::styled(lead, Style::default().fg(theme.dim)),
                            Span::raw(format!("{mark} {} ", w.name)),
                        ])
                    }
                    Row::Agent(pi, wi, ai) => {
                        let a = &projects[pi].worktrees[wi].agents[ai];
                        let (glyph, _) = state_glyph(a.state, tick, font);
                        let v_color = vendor_color(&a.vendor);
                        let icon_mode = settings_obj.icons;
                        let logo = crate::icons::logo(&a.vendor, icon_mode).unwrap_or(&a.vendor);
                        // First row of a tab sits shallow; later same-tab rows
                        // hang deeper off their tab leader. Same-tab agents are
                        // contiguous by sort, so this never nests foreign tabs.
                        let first_in_tab = first_rows.contains(&(pi, wi, ai));
                        let indent = if view == View::Recent {
                            format!("  [{}] ", projects[pi].name)
                        } else if first_in_tab {
                            "    ".to_string()
                        } else {
                            "      └─ ".to_string()
                        };
                        let title_style = if a.state == State::Working {
                            Style::default().fg(v_color).add_modifier(Modifier::BOLD)
                        } else if a.state == State::IdleStale {
                            Style::default()
                                .fg(theme.idle_stale)
                                .add_modifier(Modifier::DIM)
                        } else {
                            Style::default().fg(state_color(&theme, a.state))
                        };

                        let mut spans = vec![
                            Span::raw(indent),
                            Span::styled(
                                glyph.to_string(),
                                Style::default()
                                    .fg(state_color(&theme, a.state))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            Span::styled(
                                format!("{logo} {}", a.label),
                                Style::default().fg(v_color),
                            ),
                            Span::raw(" · "),
                            Span::styled(&a.title, title_style),
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
                    // The worktree mark is drawn in dim ink, which vanishes on
                    // the selected fill: lift it to the accent while selected.
                    if matches!(rows[idx], Row::Worktree(_, _)) {
                        if let Some(icon) = line.spans.first_mut() {
                            icon.style = Style::default()
                                .fg(theme.working)
                                .add_modifier(Modifier::BOLD);
                        }
                    }
                    line.spans.insert(
                        0,
                        Span::styled(
                            "› ",
                            Style::default()
                                .fg(theme.working)
                                .add_modifier(Modifier::BOLD),
                        ),
                    );
                } else if hover == Some(idx) {
                    line.spans.insert(0, Span::raw("· "));
                } else if matches!(rows[idx], Row::Agent(pi, wi, ai) if projects[pi].worktrees[wi].agents[ai].focused)
                {
                    // Herdr focus, independent of the cursor: survives j/k.
                    line.spans.insert(
                        0,
                        Span::styled(
                            "» ",
                            Style::default()
                                .fg(theme.working)
                                .add_modifier(Modifier::BOLD),
                        ),
                    );
                } else {
                    line.spans.insert(0, Span::raw("  "));
                }
                for span in &mut line.spans {
                    span.style = span.style.patch(style);
                }
                lines.push(line);
            }
            visual_rows = vis;
            term.draw(|f| {
                let area = f.area();
                if area.height == 0 || area.width == 0 {
                    return;
                }
                // Content above the bottom toolbar.
                // When settings is open, dim the background so the modal reads crisply.
                let bg_dim = if settings_dialog { Modifier::DIM } else { Modifier::empty() };
                let dim_lines: Vec<Line> = lines.iter().map(|l| {
                    let mut nl = l.clone();
                    for s in &mut nl.spans { s.style = s.style.add_modifier(bg_dim); }
                    nl
                }).collect();
                let content_h = area.height.saturating_sub(1);
                let content_rect = ratatui::layout::Rect::new(area.x, area.y, area.width, content_h);
                f.render_widget(Paragraph::new(dim_lines), content_rect);

                // Bottom toolbar: transient messages replace the summary, not Settings.
                let footer_y = area.bottom().saturating_sub(1);
                let btn_text = " [⚙ Settings] ";
                let btn_w = (btn_text.chars().count() as u16).min(area.width);
                settings_btn = ratatui::layout::Rect::new(area.right() - btn_w, footer_y, btn_w, 1);
                let mut footer_style = Style::default().fg(theme.dim);
                let bottom = if let Some(e) = sync_error.as_deref() {
                    format!("herdr unreachable: {e} (retrying)")
                } else if sync.is_stale() {
                    "refreshing Herdr snapshot".into()
                } else if filtering {
                    format!("filter: {query}  (enter/esc done)")
                } else if !status_line.is_empty() {
                    status_line.clone()
                } else if let Some(error) = &activity_error {
                    format!("activity save failed: {error}")
                } else if font_notice {
                    "Nerd Font not found - ASCII icons - F font options".into()
                } else {
                    footer_style = Style::default().fg(theme.working).add_modifier(Modifier::BOLD);
                    format!(" {agents} agents · {working_n} working · {blocked} blocked · {unread} unread")
                };
                let footer_rect = ratatui::layout::Rect::new(area.x, footer_y, area.width - btn_w, 1);
                f.render_widget(
                    Paragraph::new(bottom).style(footer_style),
                    footer_rect,
                );
                f.render_widget(
                    Paragraph::new(btn_text).style(Style::default().fg(theme.idle_fresh).add_modifier(Modifier::BOLD)),
                    settings_btn,
                );

                // Settings dialog opens above its bottom-right button.
                if settings_dialog {
                    let card_w = area.width.saturating_sub(2).clamp(24, 58).min(area.width);
                    let card_h = area.height.saturating_sub(4).clamp(12, 28).min(content_h);
                    let card_x = area.right().saturating_sub(card_w);
                    let card_y = footer_y.saturating_sub(card_h).max(area.y);
                    let card_rect = ratatui::layout::Rect::new(card_x, card_y, card_w, card_h);
                    f.render_widget(ratatui::widgets::Clear, card_rect);
                    let vals = crate::settings::values(&settings_obj);
                    let labels = crate::settings::LABELS;
                    let cap = ((card_h.saturating_sub(6)) as usize / 2).max(1).min(labels.len());
                    settings_offset = ensure_visible(settings_row, settings_offset, cap);
                    let shown = cap.min(labels.len().saturating_sub(settings_offset));
                    let mut title = String::from("Projects Settings");
                    if settings_offset > 0 {
                        title.push_str(" ↑");
                    }
                    if settings_offset + shown < labels.len() {
                        title.push_str(" ↓");
                    }
                    let block = Block::default().borders(Borders::ALL).title(title);
                    f.render_widget(block, card_rect);

                    close_btn = ratatui::layout::Rect::new(card_rect.right().saturating_sub(9), card_rect.y, 8, 1);
                    f.render_widget(Paragraph::new("[Close]").style(Style::default().fg(theme.working)), close_btn);

                    row_rects = [ratatui::layout::Rect::default(); 10];
                    for i in 0..shown {
                        let r = settings_offset + i;
                        let ry = card_rect.y + 2 + (i as u16) * 2;
                        let rw_rect = ratatui::layout::Rect::new(card_rect.x + 2, ry, card_rect.width.saturating_sub(4), 1);
                        row_rects[r] = rw_rect;
                        let is_sel = r == settings_row;
                        let r_style = if is_sel {
                            Style::default().add_modifier(Modifier::REVERSED)
                         } else {
                            Style::default()
                        };

                        // Compact arrows: the buttons and keys move the divider
                        // itself; help text and the footer hint say which side
                        // that grows. Fits narrow cards without spilling over
                        // the label and border.
                        let val_str = if r == 0 {
                            format!("[←] {} [→]", settings_obj.width)
                        } else {
                            vals[r].clone()
                        };
                        // Display cells, not bytes: arrows are 3 bytes per cell.
                        let val_w = val_str.chars().count() as u16;
                        let lbl_w = rw_rect.width.saturating_sub(val_w + 1);
                        let lbl_text = format!("{} {}", if is_sel { ">" } else { " " }, labels[r]);
                        f.render_widget(Paragraph::new(lbl_text).style(r_style), ratatui::layout::Rect::new(rw_rect.x, ry, lbl_w, 1));
                        let val_x = rw_rect.right().saturating_sub(val_w);
                        let val_rect = ratatui::layout::Rect::new(val_x, ry, val_w, 1);
                        f.render_widget(Paragraph::new(val_str.as_str()).style(r_style), val_rect);
                        if r == 0 {
                            less_btn = ratatui::layout::Rect::new(val_rect.x, ry, 3, 1);
                            more_btn = ratatui::layout::Rect::new(val_rect.right().saturating_sub(3), ry, 3, 1);
                        }
                    }
                    let help_y = card_rect.y + 2 + (shown as u16) * 2 + 1;
                    if help_y < card_rect.bottom().saturating_sub(2) {
                        let help_text = crate::settings::HELP[settings_row];
                        f.render_widget(
                            Paragraph::new(help_text).wrap(ratatui::widgets::Wrap { trim: true }).style(Style::default().fg(theme.dim)),
                            ratatui::layout::Rect::new(card_rect.x + 2, help_y, card_rect.width.saturating_sub(4), card_rect.bottom().saturating_sub(help_y + 1)),
                        );
                    }
                    f.render_widget(
                        Paragraph::new("↑↓/jk move · ←→/hl change · click · esc closes").style(Style::default().fg(theme.dim)),
                        ratatui::layout::Rect::new(card_rect.x + 2, card_rect.bottom() - 2, card_rect.width.saturating_sub(4), 1),
                    );
                }

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
                }
            })?;
            last_drawn = sig;
        }

        // Keep the animation deadline independent of event synchronization.
        let wait = if working {
            TICK.saturating_sub(last_tick.elapsed())
        } else {
            IDLE_POLL
        }
        .min(crate::ipc::SYNC_CHECK_INTERVAL);
        if !event::poll(wait)? {
            if working && last_tick.elapsed() >= TICK {
                tick = tick.wrapping_add(1);
                last_tick = std::time::Instant::now();
            }
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                pressed = None;
                if !matches!(key.code, KeyCode::Enter | KeyCode::Char('o'))
                    || filtering
                    || settings_dialog
                    || font_dialog
                {
                    pending_activation = None;
                }
                // Fresh key input replaces the previous message (each arm sets
                // its own); mouse motion/resize must not eat lifecycle
                status_line.clear();
                // Keys drive the selection now: a parked hover would paint a
                // second highlighted row next to it.
                hover = None;
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
                if settings_dialog {
                    match key.code {
                        KeyCode::Esc
                        | KeyCode::Char('s')
                        | KeyCode::Char('q')
                        | KeyCode::Char('c') => {
                            settings_dialog = false;
                            let _ = crate::config::update(|s| *s = settings_obj.clone());
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            settings_row = settings_row.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            settings_row = (settings_row + 1)
                                .min(crate::settings::LABELS.len().saturating_sub(1));
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            crate::settings::change(&mut settings_obj, settings_row, false);
                            crate::config::update(|s| *s = settings_obj.clone())?;
                        }
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                            crate::settings::change(&mut settings_obj, settings_row, true);
                            crate::config::update(|s| *s = settings_obj.clone())?;
                        }
                        _ => {}
                    }
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
                            save_state(&mut mem, true);
                        }
                        KeyCode::Char('3') => {
                            mem.font_choice = "font".into();
                            font = true;
                            font_notice = false;
                            font_dialog = false;
                            status_line = "Nerd Font assumed".into();
                            mem.dirty_state = true;
                            save_state(&mut mem, true);
                        }
                        KeyCode::Esc => font_dialog = false,
                        _ => {}
                    }
                    // Rescan or switch: repaint from the new set immediately.
                    sync.invalidate();
                    continue;
                }
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('o' | 'D' | 'N'))
                    && (sync_error.is_some()
                        || session.is_none()
                        || (sync.is_stale() && matches!(key.code, KeyCode::Char('D' | 'N'))))
                {
                    pending_activation = None;
                    confirm_close = None;
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Enter | KeyCode::Char('o') => {
                        pending_activation = PendingActivation::new(
                            &projects,
                            &rows,
                            selected,
                            key.code == KeyCode::Char('o'),
                        );
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = if selected == NO_SELECTION {
                            0
                        } else {
                            selected.saturating_add(1).min(rows.len().saturating_sub(1))
                        };
                        browsing = true;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.min(rows.len()).saturating_sub(1);
                        browsing = true;
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        fold_at(&mut projects, &mut mem, &rows, selected, true);
                        browsing = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        fold_at(&mut projects, &mut mem, &rows, selected, false);
                        browsing = true;
                    }
                    KeyCode::Char('/') => {
                        filtering = true;
                        query.clear();
                        selected = 0;
                        offset = 0;
                    }
                    KeyCode::Char('v') => {
                        view = if view == View::Grouped {
                            View::Recent
                        } else {
                            View::Grouped
                        };
                        selected = 0;
                        offset = 0;
                    }
                    KeyCode::Char('c') => compact = !compact,
                    KeyCode::Char('F') => font_dialog = true,
                    KeyCode::Char('s') | KeyCode::Char(',') => {
                        settings_dialog = true;
                        settings_obj = crate::config::load().unwrap_or_default();
                    }
                    KeyCode::Char('[') | KeyCode::Char('-') => {
                        settings_obj.width = settings_obj.width.saturating_sub(2).max(24);
                        let _ = crate::config::update(|s| s.width = settings_obj.width);
                        status_line = format!("dock width: {}", settings_obj.width);
                    }
                    KeyCode::Char(']') | KeyCode::Char('+') | KeyCode::Char('=') => {
                        settings_obj.width = settings_obj.width.saturating_add(2).min(80);
                        let _ = crate::config::update(|s| s.width = settings_obj.width);
                        status_line = format!("dock width: {}", settings_obj.width);
                    }
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
                            let start = if selected == NO_SELECTION {
                                n - 1
                            } else {
                                selected % n
                            };
                            if let Some(off) = (1..=n).find(|k| is_att(&rows[(start + k) % n])) {
                                selected = (start + off) % n;
                                browsing = true;
                            } else {
                                status_line = "no blocked or unacked rows".into();
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
                                touch_release(&mut mem, &format!("p:{id}"));
                                projects[pi].pinned = false;
                            } else {
                                mem.pinned.insert(id.clone());
                                touch_hold(&mut mem, &format!("p:{id}"));
                                projects[pi].pinned = true;
                            }
                            mem.dirty_state = true;
                            save_state(&mut mem, true);
                        }
                    }
                    KeyCode::Char('D') => {
                        let Some(session) = &session else { continue };
                        if let Some(r) = rows.get(selected) {
                            // Native workspace ids: a merged header closes all
                            // its member spaces, a worktree/agent row its own.
                            let (wss, name, live): (Vec<String>, String, usize) = match *r {
                                Row::Project(pi) => (
                                    projects
                                        .get(pi)
                                        .map(|p| p.workspaces.clone())
                                        .unwrap_or_default(),
                                    projects.get(pi).map(|p| p.name.clone()).unwrap_or_default(),
                                    projects
                                        .get(pi)
                                        .map(|p| p.worktrees.iter().flat_map(|w| &w.agents).count())
                                        .unwrap_or(0),
                                ),
                                Row::Agent(pi, wi, ai) => (
                                    projects
                                        .get(pi)
                                        .and_then(|p| p.worktrees.get(wi))
                                        .and_then(|w| w.agents.get(ai))
                                        .map(|a| vec![a.workspace_id.clone()])
                                        .unwrap_or_default(),
                                    projects.get(pi).map(|p| p.name.clone()).unwrap_or_default(),
                                    projects
                                        .get(pi)
                                        .map(|p| p.worktrees.iter().flat_map(|w| &w.agents).count())
                                        .unwrap_or(0),
                                ),
                                Row::Worktree(pi, wi) => (
                                    projects
                                        .get(pi)
                                        .and_then(|p| p.worktrees.get(wi))
                                        .map(|w| vec![w.workspace_id.clone()])
                                        .unwrap_or_default(),
                                    projects.get(pi).map(|p| p.name.clone()).unwrap_or_default(),
                                    projects
                                        .get(pi)
                                        .and_then(|p| p.worktrees.get(wi))
                                        .map(|w| w.agents.len())
                                        .unwrap_or(0),
                                ),
                            };
                            if confirm_close.as_deref() == Some(wss.as_slice()) {
                                // Re-resolve the row: the list re-sorts under the
                                // cursor, so only an id match may close. Plain
                                // workspace.close per id, never close_group.
                                let mut closed = 0;
                                let mut failed = 0;
                                for ws in &wss {
                                    let ok = session
                                        .call(
                                            "workspace.close",
                                            serde_json::json!({ "workspace_id": ws }),
                                        )
                                        .is_ok();
                                    if ok {
                                        closed += 1;
                                    } else {
                                        failed += 1;
                                    }
                                }
                                if failed == 0 {
                                    status_line = format!("closed {name}");
                                } else if closed == 0 {
                                    status_line = format!("close failed: {name}");
                                } else {
                                    status_line =
                                        format!("closed {closed}, failed {failed}: {name}");
                                }
                                confirm_close = None;
                                sync.invalidate();
                            } else {
                                confirm_close = Some(wss);
                                status_line =
                                    format!("press D again to close {name} ({live} agents)");
                            }
                        }
                    }
                    KeyCode::Char('N') => {
                        let Some(session) = &session else { continue };
                        let created = session
                            .call("workspace.create", serde_json::json!({}))
                            .is_ok();
                        if created {
                            status_line = "workspace created".into();
                        } else {
                            status_line = "workspace create failed".into();
                        }
                        sync.invalidate();
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    pressed = None;
                    pending_activation = None;
                    confirm_close = None;
                    if settings_dialog {
                        match modal_hit(m.column, m.row, close_btn, less_btn, more_btn, &row_rects)
                        {
                            Some(ModalHit::Close) => {
                                settings_dialog = false;
                                let _ = crate::config::update(|s| *s = settings_obj.clone());
                            }
                            Some(ModalHit::Less) => {
                                crate::settings::change(&mut settings_obj, 0, false);
                                crate::config::update(|s| *s = settings_obj.clone())?;
                            }
                            Some(ModalHit::More) => {
                                crate::settings::change(&mut settings_obj, 0, true);
                                crate::config::update(|s| *s = settings_obj.clone())?;
                            }
                            Some(ModalHit::Row(i)) => {
                                settings_row = i;
                                crate::settings::change(&mut settings_obj, i, true);
                                let _ = crate::config::update(|s| *s = settings_obj.clone());
                            }
                            None => {}
                        }
                        continue;
                    }

                    let contains = |r: ratatui::layout::Rect| {
                        m.column >= r.x
                            && m.column < r.x + r.width
                            && m.row >= r.y
                            && m.row < r.y + r.height
                    };
                    if contains(settings_btn) {
                        settings_dialog = true;
                        settings_obj = crate::config::load().unwrap_or_default();
                        continue;
                    }

                    if let Some(idx) = visual_hit(&visual_rows, m.row) {
                        if idx < rows.len() {
                            selected = idx;
                            hover = None;
                            browsing = true;
                            pressed = row_key(&projects, &rows, idx)
                                .map(|(kind, id)| (kind, id.to_owned()));
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    pending_activation = None;
                    if let Some((kind, id)) = pressed.take() {
                        if sync_error.is_none() && session.is_some() {
                            pending_activation = visual_hit(&visual_rows, m.row)
                                .and_then(|idx| release_target(&projects, &rows, (kind, &id), idx));
                        }
                    }
                }
                MouseEventKind::Moved => {
                    if settings_dialog {
                        if let Some(ModalHit::Row(i)) =
                            modal_hit(m.column, m.row, close_btn, less_btn, more_btn, &row_rects)
                        {
                            settings_row = i;
                        }
                    } else {
                        hover = visual_hit(&visual_rows, m.row).filter(|idx| *idx < rows.len());
                    }
                }
                MouseEventKind::ScrollDown => {
                    pending_activation = None;
                    if settings_dialog {
                        settings_row =
                            (settings_row + 1).min(crate::settings::LABELS.len().saturating_sub(1));
                    } else {
                        confirm_close = None;
                        selected = if selected == NO_SELECTION {
                            0
                        } else {
                            selected.saturating_add(3).min(rows.len().saturating_sub(1))
                        };
                        pressed = None;
                        browsing = true;
                    }
                }
                MouseEventKind::ScrollUp => {
                    pending_activation = None;
                    if settings_dialog {
                        settings_row = settings_row.saturating_sub(1);
                    } else {
                        confirm_close = None;
                        selected = selected.min(rows.len()).saturating_sub(3);
                        pressed = None;
                        browsing = true;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    save_state(&mut mem, true);
    mem.activity.save(true)?;
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    match session {
        Some(session) => crate::native::dock_command(crate::dock_control::Command::Close, &session),
        None => Ok(()),
    }
}

fn focus_workspace(session: &crate::ipc::Session, workspace_id: &str) -> io::Result<()> {
    session
        .call(
            "workspace.focus",
            serde_json::json!({ "workspace_id": workspace_id }),
        )
        .map(|_| ())
}

fn fold_at(projects: &mut [Project], mem: &mut Memory, rows: &[Row], idx: usize, collapsed: bool) {
    let Some(row) = rows.get(idx) else {
        return;
    };
    match *row {
        Row::Project(pi) => {
            projects[pi].collapsed = collapsed;
            let k = format!("c:{}", projects[pi].id);
            if collapsed {
                touch_hold(&mut *mem, &k);
            } else {
                touch_release(&mut *mem, &k);
            }
            sync_collapse(projects, mem);
        }
        Row::Worktree(pi, wi) => {
            projects[pi].worktrees[wi].collapsed = collapsed;
            let k = format!("w:{}", projects[pi].worktrees[wi].key);
            if collapsed {
                touch_hold(&mut *mem, &k);
            } else {
                touch_release(&mut *mem, &k);
            }
            sync_collapse(projects, mem);
        }
        Row::Agent(_, _, _) => {}
    }
}

/// What a row activation means, resolved purely: headers and worktree rows
/// address their space (native-sidebar parity), agent rows a full session.
#[derive(Debug, PartialEq, Eq)]
enum Activation {
    Space(String),
    Session(String),
}

fn activation_target(projects: &[Project], rows: &[Row], idx: usize) -> Option<Activation> {
    match *rows.get(idx)? {
        Row::Project(pi) => projects
            .get(pi)
            .and_then(|p| p.workspaces.first())
            .filter(|w| !w.is_empty())
            .map(|w| Activation::Space(w.clone())),
        Row::Worktree(pi, wi) => projects
            .get(pi)
            .and_then(|p| p.worktrees.get(wi))
            .map(|w| w.workspace_id.clone())
            .filter(|w| !w.is_empty())
            .map(Activation::Space),
        Row::Agent(pi, wi, ai) => projects
            .get(pi)
            .and_then(|p| p.worktrees.get(wi))
            .and_then(|w| w.agents.get(ai))
            .filter(|a| !a.terminal_id.is_empty() && !a.pane_id.is_empty())
            .map(|a| Activation::Session(a.pane_id.clone())),
    }
}

fn release_target(
    projects: &[Project],
    rows: &[Row],
    pressed: (u8, &str),
    released: usize,
) -> Option<PendingActivation> {
    if row_key(projects, rows, released) != Some(pressed) {
        return None;
    }
    PendingActivation::new(projects, rows, released, false)
}

/// Keep one row identity, not the native address it had before a refresh.
struct PendingActivation {
    key: (u8, String),
    workspace_only: bool,
}

impl PendingActivation {
    fn new(projects: &[Project], rows: &[Row], idx: usize, workspace_only: bool) -> Option<Self> {
        let (kind, id) = row_key(projects, rows, idx)?;
        Some(Self {
            key: (kind, id.to_owned()),
            workspace_only,
        })
    }

    fn resolve(&self, projects: &[Project], rows: &[Row]) -> Option<(usize, Activation)> {
        let idx = row_index(projects, rows, (self.key.0, &self.key.1))?;
        let target = if self.workspace_only {
            match rows[idx] {
                Row::Agent(pi, wi, ai) => {
                    let id = &projects[pi].worktrees[wi].agents[ai].workspace_id;
                    if id.is_empty() {
                        return None;
                    }
                    Activation::Space(id.clone())
                }
                _ => activation_target(projects, rows, idx)?,
            }
        } else {
            activation_target(projects, rows, idx)?
        };
        Some((idx, target))
    }
}

fn activate_target(session: &crate::ipc::Session, target: Activation) -> io::Result<()> {
    match target {
        Activation::Space(ws) => focus_workspace(session, &ws),
        Activation::Session(pane) => focus_session(session, &pane),
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
    // Explicit folds must survive a native pane close before the activity debounce.
    save_state(mem, true);
}

/// Identity of the visible frame. Equal signatures skip the draw entirely,
/// so idle costs nothing and remote ships nothing.
struct SigInput<'a> {
    projects: &'a [Project],
    rows: &'a [Row],
    selected: usize,
    offset: usize,
    height: usize,
    hover: Option<usize>,
    step: usize,
    query: &'a str,
    compact: bool,
    view: View,
    status: &'a str,
    filtering: bool,
    theme: &'a Theme,
    font_dialog: bool,
    font: bool,
    settings_dialog: bool,
    settings_row: usize,
    settings_obj: &'a crate::config::Settings,
}

fn signature(input: &SigInput) -> String {
    let SigInput {
        projects,
        rows,
        selected,
        offset,
        height,
        hover,
        step,
        query,
        compact,
        view,
        status,
        filtering,
        theme,
        font_dialog,
        font,
        settings_dialog,
        settings_row,
        settings_obj,
    } = *input;
    // Every rendered input is hashed: footer mode, focus dot, tab-driven
    // indent, and theme colors (the per-second theme reload must repaint on
    // real change and skip on none).
    let mut sig = format!(
        "{selected}:{offset}:{height}:{hover:?}:{step}:{query}:{compact}:{}:{status}:{filtering}:{font_dialog}:{font}:{:?}:{:?}:{:?}:{:?}:{settings_dialog}:{settings_row}:{settings_obj:?}:",
        view as u8, theme.working, theme.blocked, theme.done, theme.idle,
    );
    for (i, row) in rows.iter().enumerate() {
        match *row {
            Row::Project(pi) => {
                let p = &projects[pi];
                sig.push_str(&format!(
                    "{i}:P:{}:{}:{}:{}:{};",
                    p.name, p.branch, p.collapsed, p.pinned, p.focused
                ));
            }
            Row::Worktree(pi, wi) => {
                let w = &projects[pi].worktrees[wi];
                sig.push_str(&format!("{i}:W:{}:{}:{};", w.name, w.branch, w.collapsed));
            }
            Row::Agent(pi, wi, ai) => {
                let a = &projects[pi].worktrees[wi].agents[ai];
                sig.push_str(&format!(
                    "{i}:A:{}:{}:{}:{}:{}:{};",
                    a.vendor, a.label, a.title, a.state as u8, a.tab_id, a.focused
                ));
            }
        }
    }
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsing_retains_target_when_projects_reorder() {
        let mut projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        let selected = rows
            .iter()
            .position(|row| matches!(row, Row::Agent(0, 0, 0)))
            .unwrap();
        let target = activation_target(&projects, &rows, selected);
        let (kind, id) = row_key(&projects, &rows, selected).unwrap();
        let id = id.to_owned();
        projects.reverse();
        let rows = visible(&projects, "", false, View::Grouped);
        let selected = row_index(&projects, &rows, (kind, &id)).unwrap();
        assert_eq!(activation_target(&projects, &rows, selected), target);
        projects.clear();
        assert_eq!(row_index(&projects, &[], (kind, &id)), None);
    }

    #[test]
    fn browsing_follows_terminal_moves_but_never_reuses_a_removed_row() {
        let mut projects = stub();
        let old_rows = visible(&projects, "", false, View::Grouped);
        let old_index = row_index(&projects, &old_rows, (2, "alpha")).unwrap();
        projects[0].worktrees[0].agents[0].pane_id = "w9:p8".into();
        projects[0].worktrees[0].agents.swap(0, 1);
        let rows = visible(&projects, "", false, View::Grouped);
        let selected = restore_selection(&projects, &rows, Some((2, "alpha")), true);
        assert_eq!(
            activation_target(&projects, &rows, selected),
            Some(Activation::Session("w9:p8".into()))
        );
        projects[0].worktrees[0]
            .agents
            .retain(|a| a.terminal_id != "alpha");
        let rows = visible(&projects, "", false, View::Grouped);
        assert_eq!(
            activation_target(&projects, &rows, old_index),
            Some(Activation::Session("w1:p2".into()))
        );
        let selected = restore_selection(&projects, &rows, Some((2, "alpha")), true);
        assert_eq!(selected, NO_SELECTION);
        assert_eq!(activation_target(&projects, &rows, selected), None);
        assert_eq!(
            restore_selection(&projects, &rows, None, true),
            NO_SELECTION
        );
    }

    #[test]
    fn held_header_click_resolves_current_workspace_and_checks_release_identity() {
        let mut projects = stub();
        projects[0].workspaces = vec!["old-workspace".into()];
        let rows = visible(&projects, "", false, View::Grouped);
        let header = row_index(&projects, &rows, (0, "stub-a")).unwrap();
        assert_eq!(
            activation_target(&projects, &rows, header),
            Some(Activation::Space("old-workspace".into()))
        );
        let pending = release_target(&projects, &rows, (0, "stub-a"), header).unwrap();
        projects[0].workspaces = vec!["current-workspace".into()];
        assert_eq!(
            pending.resolve(&projects, &rows).map(|(_, target)| target),
            Some(Activation::Space("current-workspace".into()))
        );
        let other = row_index(&projects, &rows, (0, "stub-b")).unwrap();
        assert!(release_target(&projects, &rows, (0, "stub-a"), other).is_none());
    }

    #[test]
    fn pending_activation_follows_terminal_moves_without_numeric_fallback() {
        let mut projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        let idx = row_index(&projects, &rows, (2, "alpha")).unwrap();
        let pending = PendingActivation::new(&projects, &rows, idx, false).unwrap();
        let workspace = PendingActivation::new(&projects, &rows, idx, true).unwrap();
        projects[0].worktrees[0].agents[0].pane_id = "w9:p8".into();
        projects[0].worktrees[0].agents[0].workspace_id = "w9".into();
        projects[0].worktrees[0].agents.swap(0, 1);
        let rows = visible(&projects, "", false, View::Grouped);
        assert_eq!(
            pending.resolve(&projects, &rows).map(|(_, target)| target),
            Some(Activation::Session("w9:p8".into()))
        );
        assert_eq!(
            workspace
                .resolve(&projects, &rows)
                .map(|(_, target)| target),
            Some(Activation::Space("w9".into()))
        );
        projects[0].worktrees[0]
            .agents
            .retain(|a| a.terminal_id != "alpha");
        let rows = visible(&projects, "", false, View::Grouped);
        assert_eq!(pending.resolve(&projects, &rows), None);
        assert_eq!(workspace.resolve(&projects, &rows), None);
    }

    #[test]
    fn solo_cwd_groups_target_only_the_real_workspace() {
        let snap = serde_json::json!({
            "agents": [
                {"terminal_id":"alpha", "pane_id":"w1:p1", "workspace_id":"w1", "cwd":"/one"},
                {"terminal_id":"beta", "pane_id":"w1:p2", "workspace_id":"w1", "cwd":"/two"}
            ],
            "workspaces": [{"workspace_id":"w1", "label":"Solo",
                "worktree":{"checkout_path":"/one", "repo_key":""}}]
        });
        let projects = snapshot(&snap, &Memory::default(), &[Color::Cyan], false, 0).unwrap();
        assert_eq!(projects[0].workspaces, vec!["w1"]);
        assert_eq!(projects[0].worktrees.len(), 2);
        let rows = visible(&projects, "", false, View::Grouped);
        for (idx, row) in rows.iter().enumerate() {
            if matches!(row, Row::Project(_) | Row::Worktree(_, _)) {
                assert_eq!(
                    activation_target(&projects, &rows, idx),
                    Some(Activation::Space("w1".into()))
                );
            }
        }
    }

    #[test]
    fn repo_pin_uses_canonical_identity_after_members_change() {
        let mut snap = serde_json::json!({
            "agents": [],
            "workspaces": [{"workspace_id":"w1",
                "worktree":{"repo_key":"/repo/.git", "checkout_path":"/repo"}}]
        });
        let mut mem = Memory::default();
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        mem.pinned.insert(projects[0].id.clone());
        snap["workspaces"][0]["workspace_id"] = serde_json::json!("w2");
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        assert!(projects[0].pinned);
        assert_eq!(projects[0].workspaces, vec!["w2"]);
        mem.pinned.clear();
        mem.pinned.insert("w2".into());
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        assert!(!projects[0].pinned);
    }

    #[test]
    fn idle_history_follows_terminal_not_reused_pane_address() {
        let mut mem = Memory::default();
        mem.activity.mark_working("alpha", 1_000);
        let snap = serde_json::json!({
            "agents": [
                {"terminal_id":"alpha", "pane_id":"w1:p9", "workspace_id":"w1", "agent_status":"idle"},
                {"terminal_id":"beta", "pane_id":"w1:p1", "workspace_id":"w1", "agent_status":"idle"}
            ],
            "workspaces": [{"workspace_id":"w1"}]
        });
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 2_000).unwrap();
        let agents = &projects[0].worktrees[0].agents;
        assert_eq!(
            agents
                .iter()
                .find(|a| a.terminal_id == "alpha")
                .unwrap()
                .state,
            State::IdleFresh
        );
        assert_eq!(
            agents
                .iter()
                .find(|a| a.terminal_id == "beta")
                .unwrap()
                .state,
            State::Idle
        );
        let mut malformed = snap.clone();
        malformed["agents"][0]["terminal_id"] = serde_json::json!("");
        assert!(snapshot(&malformed, &mem, &[Color::Cyan], false, 2_000).is_err());
        malformed["agents"][0]
            .as_object_mut()
            .unwrap()
            .remove("terminal_id");
        assert!(snapshot(&malformed, &mem, &[Color::Cyan], false, 2_000).is_err());
        assert_eq!(mem.freshness("alpha", 2_000), State::IdleFresh);
    }

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
    fn visual_rows_pad_header_and_separate_projects() {
        let projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        assert!(rows.len() > 2);
        let vis = visual_rows_for_page(&rows, 0, rows.len() + 4);
        // One blank above the tree.
        assert_eq!(vis[0], None);
        assert_eq!(vis[1], Some(0));
        // Exactly one blank between the two projects.
        let second = rows
            .iter()
            .position(|r| matches!(r, Row::Project(1)))
            .unwrap();
        let at = vis.iter().position(|v| *v == Some(second)).unwrap();
        assert_eq!(vis[at - 1], None);
        // A worktree stays attached to its project header; later worktrees
        // take one blank gap so rows never touch (no rules anywhere).
        let wt = rows
            .iter()
            .position(|r| matches!(r, Row::Worktree(0, 0)))
            .unwrap();
        let atw = vis.iter().position(|v| *v == Some(wt)).unwrap();
        assert!(vis[atw - 1].is_some());
        let two_trees = vec![
            Row::Project(0),
            Row::Worktree(0, 0),
            Row::Agent(0, 0, 0),
            Row::Worktree(0, 1),
        ];
        let vis2 = visual_rows_for_page(&two_trees, 0, 8);
        assert_eq!(vis2, vec![None, Some(0), Some(1), Some(2), None, Some(3)]);
        // Short window: the tail worktree still fits by visual lines…
        let vis3 = visual_rows_for_page(&two_trees, 0, 7);
        assert_eq!(vis3, vec![None, Some(0), Some(1), Some(2), None, Some(3)]);
        // …and when it cannot fit, the window slides until the cursor draws.
        let (vis4, off) = window_for_selected(&two_trees, 0, 5, 3);
        assert!(vis4.contains(&Some(3)));
        assert!(off > 0);
        let ag = rows
            .iter()
            .position(|r| matches!(r, Row::Agent(0, 0, 0)))
            .unwrap();
        let ata = vis.iter().position(|v| *v == Some(ag)).unwrap();
        assert!(vis[ata - 1].is_some());
        // Mouse hits: top padding misses, first row hits, toolbar below content misses.
        assert_eq!(visual_hit(&vis, 0), None);
        assert_eq!(visual_hit(&vis, 1), Some(0));
        assert_eq!(visual_hit(&vis, vis.len() as u16), None);
        // Never overflows the content height.
        assert!(visual_rows_for_page(&rows, 0, 3).len() <= 3);
        assert!(visual_rows_for_page(&rows, 0, 0).is_empty());
    }

    #[test]
    fn linked_detection_reads_gitdir_pointer() {
        let root = std::env::temp_dir().join(format!("hps-linked-{}", std::process::id()));
        let plain = root.join("plain");
        let linked = root.join("linked");
        let sub = root.join("sub");
        for dir in [&plain, &linked, &sub] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::create_dir_all(plain.join(".git")).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /repo/.git/worktrees/wt1\n").unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert!(!checkout_is_linked(plain.to_str().unwrap()));
        assert!(checkout_is_linked(linked.to_str().unwrap()));
        assert!(!checkout_is_linked(sub.to_str().unwrap()));
        assert!(!checkout_is_linked(root.join("missing").to_str().unwrap()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn modal_hit_prefers_buttons_over_rows() {
        use ratatui::layout::Rect;
        let close = Rect::new(50, 1, 8, 1);
        let less = Rect::new(10, 4, 3, 1);
        let more = Rect::new(20, 4, 3, 1);
        let mut rows = [Rect::default(); 10];
        rows[0] = Rect::new(4, 4, 36, 1);
        rows[3] = Rect::new(4, 10, 36, 1);
        assert!(matches!(
            modal_hit(51, 1, close, less, more, &rows),
            Some(ModalHit::Close)
        ));
        // The arrows sit inside row 0's rect: they must win over the row.
        assert!(matches!(
            modal_hit(11, 4, close, less, more, &rows),
            Some(ModalHit::Less)
        ));
        assert!(matches!(
            modal_hit(21, 4, close, less, more, &rows),
            Some(ModalHit::More)
        ));
        assert!(matches!(
            modal_hit(5, 4, close, less, more, &rows),
            Some(ModalHit::Row(0))
        ));
        assert!(matches!(
            modal_hit(5, 10, close, less, more, &rows),
            Some(ModalHit::Row(3))
        ));
        assert!(modal_hit(0, 0, close, less, more, &rows).is_none());
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
    fn theme_parses_row_rules_and_custom() {
        let text = "[theme.custom]\nyellow = \"#111111\"\n[ui.sidebar.agents]\nrows = [[{ token = \"x\", rules = [{ equals = \"working\", fg = \"#222222\" }] }]]";
        assert_eq!(
            rule_color(text, "working"),
            Some(Color::Rgb(0x22, 0x22, 0x22))
        );
        assert_eq!(
            custom_color(text, "yellow"),
            Some(Color::Rgb(0x11, 0x11, 0x11))
        );
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
        assert_eq!(
            rule_color(
                "equals = \"working\" \u{f418}glyphs { fg = \"#222222\" }",
                "working"
            ),
            Some(Color::Rgb(0x22, 0x22, 0x22))
        );
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
        assert!(use_font("", true));
        assert!(!font_notice_due("text", false));
        assert!(!font_notice_due("font", false));
        // Without the env override these are notice/no-notice; with it set
        // both are false -- either way no panic, no prompt loop.
        let _ = font_notice_due("auto", false);
        let _ = font_notice_due("auto", true);
    }

    #[test]
    fn ancestry_pins_main_and_nests_paths() {
        use std::collections::BTreeMap;
        let ws = |checkout: &str, root: &str| WorkspaceEntry {
            workspace_id: String::new(),
            label: String::new(),
            focused: false,
            worktree: Some(WorktreeInfo {
                checkout_path: checkout.into(),
                repo_key: String::new(),
                repo_name: String::new(),
                repo_root: root.into(),
                is_linked_worktree: true,
            }),
        };
        let main = ws("/repo/", "/repo/");
        let sib = ws("/wt/sib", "/repo/");
        let sub = ws("/wt/sib/child", "/repo/");
        let by_id: BTreeMap<&str, &WorkspaceEntry> = [("wC", &main), ("w1", &sib), ("w2", &sub)]
            .into_iter()
            .collect();
        let members = ["wC", "w1", "w2"];
        // Main checkout anchors even when sorted last; absent root = None.
        assert_eq!(main_member(&members, &by_id), Some("wC"));
        assert_eq!(main_member(&["w1", "w2"], &by_id), None);
        let probes: BTreeMap<&str, &str> =
            [("wC", "/repo"), ("w1", "/wt/sib"), ("w2", "/wt/sib/child")]
                .into_iter()
                .collect();
        assert_eq!(nest_level(&probes, Some("wC"), "wC"), 0);
        assert_eq!(nest_level(&probes, Some("wC"), "w1"), 1);
        assert_eq!(nest_level(&probes, Some("wC"), "w2"), 2);
        // Unknown probe never nests; equal paths never contain.
        assert_eq!(nest_level(&probes, Some("wC"), "w9"), 1);
        let flat: BTreeMap<&str, &str> = [("a", "/x"), ("b", "/x2")].into_iter().collect();
        assert_eq!(nest_level(&flat, None, "b"), 1);
    }

    #[test]
    fn agent_title_prefers_native_metadata() {
        let mut e = AgentEntry {
            terminal_id: "alpha".into(),
            agent: "claude".into(),
            agent_status: String::new(),
            pane_id: String::new(),
            tab_id: String::new(),
            workspace_id: String::new(),
            cwd: "/repo".into(),
            foreground_cwd: None,
            focused: false,
            state_change_seq: 0,
            title: Some("Supplied".into()),
            display_agent: None,
            terminal_title_stripped: Some("Stripped".into()),
            terminal_title: None,
        };
        assert_eq!(agent_title(&e), "Supplied");
        e.title = None;
        assert_eq!(agent_title(&e), "Stripped");
    }

    #[test]
    fn activation_maps_rows_to_native_targets() {
        let mut projects = stub();
        projects[0].workspaces = vec!["wA".into()];
        let rows = visible(&projects, "", false, View::Grouped);
        let header = rows
            .iter()
            .position(|r| matches!(*r, Row::Project(0)))
            .unwrap();
        assert_eq!(
            activation_target(&projects, &rows, header),
            Some(Activation::Space("wA".into()))
        );
        // Empty member list: no target, no action.
        projects[1].workspaces = vec![];
        let header1 = rows
            .iter()
            .position(|r| matches!(*r, Row::Project(1)))
            .unwrap();
        assert_eq!(activation_target(&projects, &rows, header1), None);
        assert_eq!(activation_target(&projects, &rows, 9999), None);
    }

    #[test]
    fn shared_veto_converges_across_instances() {
        // Two live docks, one file: B's unpin must survive A's saves, and a
        // re-pin must stick globally. Deterministic via a temp state dir.
        let dir = std::env::temp_dir().join(format!("hps-conv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
        let mut a = Memory::default();
        let mut b = Memory::default();
        load_state(&mut a);
        load_state(&mut b);
        // Legacy pin in the file, nobody touched it this session.
        a.pinned.insert("w1Q".into());
        save_state(&mut a, true);
        load_state(&mut b);
        assert!(b.pinned.contains("w1Q"));
        // B unpins: file loses X and carries the veto.
        b.pinned.remove("w1Q");
        touch_release(&mut b, "p:w1Q");
        save_state(&mut b, true);
        // A saves with no live hold: adopts the veto, X stays gone.
        save_state(&mut a, true);
        assert!(!a.pinned.contains("w1Q"));
        let mut c = Memory::default();
        load_state(&mut c);
        assert!(!c.pinned.contains("w1Q"));
        // Stale holder that missed the veto window: pre-veto memory plus a
        // save must adopt, never resurrect.
        let mut e = Memory::default();
        e.pinned.insert("w1Q".into());
        save_state(&mut e, true);
        let mut f = Memory::default();
        load_state(&mut f);
        assert!(!f.pinned.contains("w1Q"));
        // Fresh re-pin in B overrules the veto and sticks globally.
        b.pinned.insert("w1Q".into());
        touch_hold(&mut b, "p:w1Q");
        save_state(&mut b, true);
        let mut d = Memory::default();
        load_state(&mut d);
        assert!(d.pinned.contains("w1Q"));
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seat_row_prefers_session_then_space() {
        let mut projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        // No focus anywhere: nowhere to sit.
        assert_eq!(seat_row(&projects, &rows, None), None);
        // Non-agent focus (dock pane, plain shell): the space header.
        projects[1].focused = true;
        let rows = visible(&projects, "", false, View::Grouped);
        let want = rows.iter().position(|r| matches!(*r, Row::Project(1)));
        assert!(want.is_some());
        assert_eq!(seat_row(&projects, &rows, Some("w9:plain")), want);
        // A focused session wins over its own space header.
        projects[1].worktrees[0].agents[0].focused = true;
        projects[1].worktrees[0].agents[0].pane_id = "w9:p1".into();
        let pid = "w9:p1";
        assert_eq!(
            seat_row(&projects, &rows, Some(pid)),
            rows.iter().position(|r| matches!(*r, Row::Agent(1, 0, 0)))
        );
    }

    #[test]
    fn focus_row_finds_agent_pane() {
        let mut projects = stub();
        projects[0].worktrees[0].agents[0].focused = true;
        let pid = projects[0].worktrees[0].agents[0].pane_id.clone();
        let rows = visible(&projects, "", false, View::Grouped);
        let want = rows.iter().position(|r| matches!(*r, Row::Agent(0, 0, 0)));
        assert_eq!(focus_row(&projects, &rows, &pid), want);
        assert!(want.is_some());
        assert_eq!(focus_row(&projects, &rows, "nope"), None);
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

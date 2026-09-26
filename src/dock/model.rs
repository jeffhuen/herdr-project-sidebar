use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::time::Duration;

use crate::activity::now_unix_ms;
use ratatui::style::Color;
use serde::Deserialize;

use super::state::{touch_release, Memory};

pub(super) const NO_SELECTION: usize = usize::MAX;
const BRANCH_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub(super) enum State {
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
    /// J skips work that does not need human attention.
    pub(super) fn is_attention(self) -> bool {
        matches!(self, State::Blocked | State::Interrupted | State::Done)
    }
}


#[derive(Clone)]
pub(super) struct Agent {
    pub(super) vendor: String,
    pub(super) label: String,
    pub(super) title: String,
    pub(super) state: State,
    pub(super) terminal_id: String,
    pub(super) pane_id: String,
    pub(super) tab_id: String,
    pub(super) workspace_id: String,
    pub(super) cwd: String,
    pub(super) session_ref: Option<AgentSession>,
    pub(super) seq: u64,
    pub(super) focused: bool,
}

impl Agent {
    /// Plain terminals have no agent state, so they never outrank an agent.
    fn rank(&self) -> u8 {
        if self.vendor == "terminal" {
            9
        } else {
            self.state.rank()
        }
    }
}

#[derive(Clone)]
pub(super) struct Worktree {
    pub(super) key: String,
    pub(super) name: String,
    pub(super) branch: String,
    pub(super) path: String,
    pub(super) repo_root: String,
    pub(super) collapsed: bool,
    pub(super) depth: usize,
    /// Owning workspace: the focus target for this row.
    pub(super) workspace_id: String,
    pub(super) focused: bool,
    pub(super) agents: Vec<Agent>,
}

#[derive(Clone)]
pub(super) struct Project {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) icon: char,
    pub(super) icon_color: Color,
    pub(super) branch: String,
    pub(super) collapsed: bool,
    pub(super) pinned: bool,
    pub(super) focused: bool,
    /// Member workspace ids (one for Solo). Header focus targets the first.
    pub(super) workspaces: Vec<String>,
    /// All member pane directories, including non-agent shells but excluding the dock.
    pub(super) directories: Vec<String>,
    pub(super) worktrees: Vec<Worktree>,
}

#[derive(Clone, Copy)]
pub(super) enum Row {
    Project(usize),
    Worktree(usize, usize),
    Agent(usize, usize, usize),
}

pub(super) fn row_key<'a>(projects: &'a [Project], rows: &[Row], idx: usize) -> Option<(u8, &'a str)> {
    Some(match *rows.get(idx)? {
        Row::Project(pi) => (0, &projects[pi].id),
        Row::Worktree(pi, wi) => (1, &projects[pi].worktrees[wi].key),
        Row::Agent(pi, wi, ai) => (2, &projects[pi].worktrees[wi].agents[ai].terminal_id),
    })
}

pub(super) fn row_index(projects: &[Project], rows: &[Row], key: (u8, &str)) -> Option<usize> {
    (0..rows.len()).find(|&idx| row_key(projects, rows, idx) == Some(key))
}

pub(super) fn restore_selection(
    projects: &[Project],
    rows: &[Row],
    key: Option<(u8, &str)>,
    browsing: bool,
) -> usize {
    let preserved = key.and_then(|key| row_index(projects, rows, key));
    if browsing {
        return preserved.unwrap_or(NO_SELECTION);
    }
    seat_row(projects, rows)
        .or(preserved)
        .unwrap_or(NO_SELECTION)
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum View {
    Grouped,
    Recent,
}

#[derive(Clone, Deserialize, PartialEq)]
pub(super) struct AgentSession {
    #[serde(default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) value: String,
}

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
    agent_session: Option<AgentSession>,
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

/// Identify the main checkout by path, not activity order.
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
    mem.branches.clear();
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
            if let Some(branch) = crate::git::git_branch(std::path::Path::new(probe)) {
                mem.branches.insert(key.to_owned(), branch);
            }
        }
        if checkout_is_linked(probe) {
            mem.linked_checkouts.insert(probe.to_owned());
        }
    }
    mem.branch_at = Some(std::time::Instant::now());
}

pub(super) fn install_snapshot(
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
    let mut live = HashSet::new();
    for a in projects
        .iter()
        .flat_map(|p| &p.worktrees)
        .flat_map(|w| &w.agents)
    {
        live.insert(a.terminal_id.as_str());
        if matches!(a.state, State::Working | State::Monitoring) {
            mem.activity.mark_working(&a.terminal_id, now_ms);
        }
    }
    mem.activity.retain_live(&live);
    // Remove member aliases so an unfolded repo cannot regain its saved fold.
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
        // The dock also accepts a bare π prefix; native titles require "π ".
        let clean = crate::icons::clean_agent_title(name, t, true);
        if !clean.is_empty() && clean != "None" {
            return clean.to_string();
        }
    }

    if !name.is_empty() && name != "?" && name != "terminal" {
        return name.to_string();
    }

    std::path::Path::new(&e.cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| e.cwd.clone())
}


/// Prefer the focused agent, then its worktree or project.
fn seat_row(projects: &[Project], rows: &[Row]) -> Option<usize> {
    rows.iter()
        .position(|r| matches!(*r, Row::Agent(pi, wi, ai) if projects[pi].worktrees[wi].agents[ai].focused))
        .or_else(|| {
            rows.iter().position(|r| match *r {
                Row::Worktree(pi, wi) => projects[pi].worktrees[wi].focused,
                _ => false,
            })
        })
        .or_else(|| {
            rows.iter()
                .position(|r| matches!(*r, Row::Project(pi) if projects[pi].focused))
        })
}

/// Build the tree from one complete snapshot. No transport or filesystem I/O.
pub(super) fn snapshot(
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
    let existing_pane_ids: HashSet<&str> = agents.iter().map(|a| a.pane_id.as_str()).collect();
    let ws_by_id: BTreeMap<&str, &WorkspaceEntry> = workspaces
        .iter()
        .map(|w| (w.workspace_id.as_str(), w))
        .collect();
    let mut workspace_directories: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut extra_terminals = Vec::new();
    fn is_plugin_pane(pane: &serde_json::Value) -> bool {
        if pane.pointer("/tokens/hps_dock").and_then(|v| v.as_str()) == Some("projects") {
            return true;
        }
        if let Some(tokens) = pane.get("tokens").and_then(|t| t.as_object()) {
            if tokens.keys().any(|k| {
                k.starts_with("herdr-")
                    || k.starts_with("hps_")
                    || k.contains("dock")
                    || k.contains("sidebar")
                    || k.contains("plugin")
            }) {
                return true;
            }
        }
        if pane.get("label").and_then(|l| l.as_str()).is_some_and(|l| !l.is_empty())
            && pane.get("agent").and_then(|a| a.as_str()).is_none_or(|a| a.is_empty())
        {
            return true;
        }
        false
    }
    for pane in snap["panes"].as_array().into_iter().flatten() {
        if is_plugin_pane(pane) {
            continue;
        }
        let Some(workspace) = pane["workspace_id"].as_str() else {
            continue;
        };
        let directory = pane["foreground_cwd"]
            .as_str()
            .filter(|path| !path.is_empty())
            .or_else(|| pane["cwd"].as_str())
            .unwrap_or("");
        workspace_directories
            .entry(workspace)
            .or_default()
            .insert(directory);

        let Some(pane_id) = pane["pane_id"].as_str().filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        let Some(terminal_id) = pane["terminal_id"].as_str().filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        if !workspaces.iter().any(|w| w.workspace_id == workspace) {
            continue;
        }
        if existing_pane_ids.contains(pane_id) || !terminals.insert(terminal_id) {
            continue;
        }
        let cwd = pane["cwd"].as_str().unwrap_or("").to_owned();
        let foreground_cwd = pane["foreground_cwd"].as_str().filter(|s| !s.is_empty()).map(String::from);
        let tab_id = pane["tab_id"].as_str().unwrap_or("").to_owned();
        let focused = pane["focused"].as_bool().unwrap_or(false);
        let terminal_title = pane["terminal_title"].as_str().map(String::from);
        let terminal_title_stripped = pane["terminal_title_stripped"].as_str().map(String::from);
        extra_terminals.push(AgentEntry {
            terminal_id: terminal_id.to_owned(),
            agent: "terminal".to_owned(),
            title: None,
            display_agent: Some("terminal".to_owned()),
            agent_status: "idle".to_owned(),
            pane_id: pane_id.to_owned(),
            tab_id,
            workspace_id: workspace.to_owned(),
            cwd,
            foreground_cwd,
            agent_session: None,
            focused,
            state_change_seq: 0,
            terminal_title_stripped,
            terminal_title,
        });
    }
    agents.extend(extra_terminals);
    let tab_order: HashMap<&str, usize> = snap["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(idx, tab)| {
            let id = tab.get("tab_id")?.as_str()?;
            Some((id, idx))
        })
        .collect();
    let pane_order: HashMap<&str, usize> = snap["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(idx, pane)| {
            let id = pane.get("pane_id")?.as_str()?;
            Some((id, idx))
        })
        .collect();
    fn repo_of(ws_by_id: &BTreeMap<&str, &WorkspaceEntry>, ws_id: &str) -> Option<String> {
        ws_by_id
            .get(ws_id)
            .and_then(|w| w.worktree.as_ref())
            .map(|t| t.repo_key.clone())
            .filter(|k| !k.is_empty())
    }
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum PJKey {
        Repo(String),
        Solo(String),
    }
    // Solo workspaces group by cwd so multi-root spaces stay separate.
    let mut groups: BTreeMap<PJKey, BTreeMap<String, Vec<AgentEntry>>> = BTreeMap::new();
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
    // Preserve checkouts even when they have no agent panes.
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
        let member_ids: Vec<&str> = match key {
            PJKey::Repo(_) => members.keys().map(String::as_str).collect(),
            PJKey::Solo(ws_id) => vec![ws_id.as_str()],
        };
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
                    let is_terminal = e.agent == "terminal";
                    let vendor = if e.agent.is_empty() { "?" } else { &e.agent };
                    Agent {
                        vendor: vendor.to_owned(),
                        label: if is_terminal {
                            String::new()
                        } else {
                            e.display_agent
                                .clone()
                                .filter(|l| !l.is_empty())
                                .unwrap_or_else(|| vendor.to_owned())
                        },
                        title: agent_title(e),
                        state: st,
                        terminal_id: e.terminal_id.clone(),
                        pane_id: e.pane_id.clone(),
                        tab_id: e.tab_id.clone(),
                        workspace_id: e.workspace_id.clone(),
                        cwd: e
                            .foreground_cwd
                            .as_ref()
                            .filter(|cwd| !cwd.is_empty())
                            .unwrap_or(&e.cwd)
                            .clone(),
                        session_ref: e.agent_session.clone(),
                        seq: e.state_change_seq,
                        focused: e.focused,
                    }
                })
                .collect();
            list.sort_by(|a, b| {
                let to_a = tab_order.get(a.tab_id.as_str()).copied().unwrap_or(usize::MAX);
                let to_b = tab_order.get(b.tab_id.as_str()).copied().unwrap_or(usize::MAX);
                let po_a = pane_order.get(a.pane_id.as_str()).copied().unwrap_or(usize::MAX);
                let po_b = pane_order.get(b.pane_id.as_str()).copied().unwrap_or(usize::MAX);
                (to_a, a.tab_id.as_str(), po_a, a.pane_id.as_str())
                    .cmp(&(to_b, b.tab_id.as_str(), po_b, b.pane_id.as_str()))
            });
            let linked =
                wt.is_some_and(|t| t.is_linked_worktree) || mem.linked_checkouts.contains(&probe);
            let checkout_name = std::path::Path::new(&probe)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty());
            // Linked rows carry the worktree's own name, plain checkouts the
            // branch. The repo name is never repeated: the header owns it.
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
            let depth = match key {
                PJKey::Repo(_) => depths.get(sub.as_str()).copied().unwrap_or(1),
                PJKey::Solo(_) => usize::from(!worktrees.is_empty()),
            };
            worktrees.push(Worktree {
                key: wt_key.clone(),
                name: label,
                branch,
                path: wt
                    .map(|wt| wt.checkout_path.clone())
                    .filter(|path| !path.is_empty())
                    .or_else(|| entries.first().map(|entry| entry.cwd.clone()))
                    .unwrap_or_default(),
                repo_root: wt.map(|wt| wt.repo_root.clone()).unwrap_or_default(),
                collapsed: mem.collapsed_worktrees.contains(&wt_key),
                depth,
                workspace_id: match key {
                    PJKey::Repo(_) => sub.clone(),
                    PJKey::Solo(_) => id.clone(),
                },
                focused: member_ws.map_or(false, |w| w.focused),
                agents: list,
            });
        }
        // IDs cannot contain "::", so this prefix identifies one workspace.
        if let Some(m) = &main {
            let prefix = format!("{m}::");
            if let Some(pos) = worktrees.iter().position(|w| w.key.starts_with(&prefix)) {
                let row = worktrees.remove(pos);
                worktrees.insert(0, row);
            }
        }
        worktrees.sort_by(|a, b| {
            let ra = a.agents.iter().map(Agent::rank).min().unwrap_or(9);
            let rb = b.agents.iter().map(Agent::rank).min().unwrap_or(9);
            ra.cmp(&rb).then_with(|| a.name.cmp(&b.name))
        });
        let branch = worktrees
            .iter()
            .find(|w| !w.branch.is_empty())
            .map(|w| w.branch.clone())
            .unwrap_or_default();
        let ws_of = |m: &str| ws_by_id.get(m).copied();
        // repo_key usually ends in ".git"; use its parent for the fallback name.
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
        let pinned = mem.pinned.contains(&id);
        let focused = member_ids.iter().filter_map(|m| ws_of(m)).any(|w| w.focused);
        let collapsed = mem.collapsed_projects.contains(&id);
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
            directories: member_ids
                .iter()
                .filter_map(|id| workspace_directories.get(*id))
                .flat_map(|paths| paths.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            worktrees,
        });
    }
    projects.sort_by(|a, b| {
        let score = |p: &Project| {
            p.worktrees
                .iter()
                .flat_map(|w| &w.agents)
                .map(Agent::rank)
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

/// Stable palette index shared by occupied and empty projects.
pub(super) fn project_hue(ws_id: &str) -> usize {
    let mut h: u64 = 5381;
    for b in ws_id.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h as usize
}

fn matches_filter(p: &Project, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
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

pub(super) fn visible(projects: &[Project], query: &str, compact: bool, view: View) -> Vec<Row> {
    let query = query.to_lowercase();
    if view == View::Recent {
        // Flat activity order; workspace prefix keeps rows attributable.
        let mut flat = Vec::new();
        for (pi, p) in projects.iter().enumerate() {
            if !matches_filter(p, &query) {
                continue;
            }
            for (wi, w) in p.worktrees.iter().enumerate() {
                for (ai, _) in w.agents.iter().enumerate() {
                    if compact && w.agents[ai].state == State::IdleStale {
                        continue;
                    }
                    flat.push((pi, wi, ai));
                }
            }
        }
        flat.sort_by_key(|&(pi, wi, ai)| {
            let a = &projects[pi].worktrees[wi].agents[ai];
            (a.rank(), std::cmp::Reverse(a.seq))
        });
        return flat.into_iter().map(|(pi, wi, ai)| Row::Agent(pi, wi, ai)).collect();
    }
    let mut rows = Vec::new();
    for (pi, p) in projects.iter().enumerate() {
        if !matches_filter(p, &query) {
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
    vis.push(None);
    let mut first = true;
    for (idx, row) in rows.iter().enumerate().skip(offset) {
        let follows_project = match vis.last() {
            Some(Some(prev)) => matches!(rows[*prev], Row::Project(_)),
            _ => false,
        };
        let gap = !first
            && (matches!(row, Row::Project(_))
                || (matches!(row, Row::Worktree(_, _)) && !follows_project));
        // Count padding too, or the selected row can fall off the page.
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

/// Include padding when shifting the viewport to the selection.
pub(super) fn window_for_selected(
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
pub(super) fn visual_hit(vis: &[Option<usize>], y: u16) -> Option<usize> {
    vis.get(y as usize).copied().flatten()
}


pub(super) fn ensure_visible(selected: usize, offset: usize, height: usize) -> usize {
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

pub(super) fn counts(projects: &[Project]) -> (usize, usize, usize, usize) {
    let mut agents = 0;
    let mut working = 0;
    let mut blocked = 0;
    let mut unread = 0;
    for p in projects {
        for w in &p.worktrees {
            for a in &w.agents {
                if a.vendor == "terminal" {
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::input::{activation_target, Activation};
    use super::super::stub;

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
                    activation_target(&projects, &rows, idx, false),
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
    fn tab_and_pane_order_determines_session_sequence() {
        let mut snap = serde_json::json!({
            "workspaces": [{"workspace_id": "w1", "label": "test"}],
            "tabs": [
                {"tab_id": "tab-z", "number": 16},
                {"tab_id": "tab-a", "number": 23}
            ],
            "panes": [
                {"pane_id": "pane-z2", "workspace_id": "w1"},
                {"pane_id": "pane-z1", "workspace_id": "w1"},
                {"pane_id": "pane-a1", "workspace_id": "w1"}
            ],
            "agents": [
                {"terminal_id": "t-a1", "pane_id": "pane-a1", "tab_id": "tab-a", "workspace_id": "w1"},
                {"terminal_id": "t-z1", "pane_id": "pane-z1", "tab_id": "tab-z", "workspace_id": "w1"},
                {"terminal_id": "t-z2", "pane_id": "pane-z2", "tab_id": "tab-z", "workspace_id": "w1"}
            ]
        });
        let mem = Memory::default();
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        let term_ids: Vec<&str> = projects[0].worktrees[0]
            .agents
            .iter()
            .map(|a| a.terminal_id.as_str())
            .collect();
        assert_eq!(term_ids, vec!["t-z2", "t-z1", "t-a1"]);

        // Reorder tabs: tab-a (number 23) placed at index 0, tab-z (number 16) at index 1.
        // The array position (visual tab bar slot) must take precedence over the immutable tab number.
        snap["tabs"] = serde_json::json!([
            {"tab_id": "tab-a", "number": 23},
            {"tab_id": "tab-z", "number": 16}
        ]);
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        let term_ids: Vec<&str> = projects[0].worktrees[0]
            .agents
            .iter()
            .map(|a| a.terminal_id.as_str())
            .collect();
        assert_eq!(term_ids, vec!["t-a1", "t-z2", "t-z1"]);
    }

    #[test]
    fn terminals_included_in_tab_order() {
        let mut snap = serde_json::json!({
            "workspaces": [{"workspace_id": "w1", "label": "test"}],
            "tabs": [
                {"tab_id": "tab-1", "number": 1},
                {"tab_id": "tab-2", "number": 2},
                {"tab_id": "tab-3", "number": 3}
            ],
            "panes": [
                {"pane_id": "p-agent", "terminal_id": "t-agent", "tab_id": "tab-1", "workspace_id": "w1"},
                {"pane_id": "p-shell", "terminal_id": "t-shell", "tab_id": "tab-2", "workspace_id": "w1", "terminal_title": "zsh"},
                {"pane_id": "p-agent2", "terminal_id": "t-agent2", "tab_id": "tab-3", "workspace_id": "w1"}
            ],
            "agents": [
                {"terminal_id": "t-agent", "pane_id": "p-agent", "tab_id": "tab-1", "workspace_id": "w1", "agent": "omp"},
                {"terminal_id": "t-agent2", "pane_id": "p-agent2", "tab_id": "tab-3", "workspace_id": "w1", "agent": "claude"}
            ]
        });
        let mem = Memory::default();
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        let agents = &projects[0].worktrees[0].agents;
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].terminal_id, "t-agent");
        assert_eq!(agents[0].vendor, "omp");
        assert_eq!(agents[1].terminal_id, "t-shell");
        assert_eq!(agents[1].vendor, "terminal");
        assert_eq!(agents[1].title, "zsh");
        assert_eq!(agents[2].terminal_id, "t-agent2");
        assert_eq!(agents[2].vendor, "claude");

        let (count, _, _, _) = counts(&projects);
        assert_eq!(count, 2);

        snap["tabs"] = serde_json::json!([
            {"tab_id": "tab-2", "number": 2},
            {"tab_id": "tab-1", "number": 1},
            {"tab_id": "tab-3", "number": 3}
        ]);
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        let agents = &projects[0].worktrees[0].agents;
        assert_eq!(agents[0].terminal_id, "t-shell");
        assert_eq!(agents[1].terminal_id, "t-agent");
        assert_eq!(agents[2].terminal_id, "t-agent2");
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
    fn terminal_only_worktree_does_not_outrank_agent_worktree() {
        let snap = serde_json::json!({
            "workspaces": [
                {"workspace_id": "w1", "worktree": {"repo_key": "/repo/.git", "repo_name": "repo",
                    "checkout_path": "/repo", "is_linked_worktree": false}},
                {"workspace_id": "w2", "worktree": {"repo_key": "/repo/.git", "repo_name": "repo",
                    "checkout_path": "/wt/aaa", "is_linked_worktree": true}}
            ],
            "tabs": [{"tab_id": "w1:t1"}, {"tab_id": "w2:t1"}],
            "panes": [
                {"pane_id": "w1:p1", "terminal_id": "t-agent", "tab_id": "w1:t1", "workspace_id": "w1", "cwd": "/repo"},
                {"pane_id": "w2:p1", "terminal_id": "t-shell", "tab_id": "w2:t1", "workspace_id": "w2", "cwd": "/wt/aaa"}
            ],
            "agents": [
                {"terminal_id": "t-agent", "pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1",
                    "agent": "omp", "agent_status": "idle", "cwd": "/repo"}
            ]
        });
        let mut mem = Memory::default();
        mem.branches.insert("/repo".into(), "main".into());
        mem.branches.insert("/wt/aaa".into(), "aaa".into());
        let projects = snapshot(&snap, &mem, &[Color::Cyan], false, 0).unwrap();
        let names: Vec<&str> = projects[0].worktrees.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["main", "aaa"]);
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
        assert_eq!(vis[0], None);
        assert_eq!(vis[1], Some(0));
        let second = rows
            .iter()
            .position(|r| matches!(r, Row::Project(1)))
            .unwrap();
        let at = vis.iter().position(|v| *v == Some(second)).unwrap();
        assert_eq!(vis[at - 1], None);
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
        let vis3 = visual_rows_for_page(&two_trees, 0, 7);
        assert_eq!(vis3, vec![None, Some(0), Some(1), Some(2), None, Some(3)]);
        let (vis4, off) = window_for_selected(&two_trees, 0, 5, 3);
        assert!(vis4.contains(&Some(3)));
        assert!(off > 0);
        let ag = rows
            .iter()
            .position(|r| matches!(r, Row::Agent(0, 0, 0)))
            .unwrap();
        let ata = vis.iter().position(|v| *v == Some(ag)).unwrap();
        assert!(vis[ata - 1].is_some());
        assert_eq!(visual_hit(&vis, 0), None);
        assert_eq!(visual_hit(&vis, 1), Some(0));
        assert_eq!(visual_hit(&vis, vis.len() as u16), None);
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
    fn status_maps_known_strings() {
        assert_eq!(map_status("working"), State::Working);
        assert_eq!(map_status("blocked"), State::Blocked);
        assert_eq!(map_status("permission"), State::Blocked);
        assert_eq!(map_status("done"), State::Done);
        assert_eq!(map_status("bogus"), State::Unknown);
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
        assert_eq!(main_member(&members, &by_id), Some("wC"));
        assert_eq!(main_member(&["w1", "w2"], &by_id), None);
        let probes: BTreeMap<&str, &str> =
            [("wC", "/repo"), ("w1", "/wt/sib"), ("w2", "/wt/sib/child")]
                .into_iter()
                .collect();
        assert_eq!(nest_level(&probes, Some("wC"), "wC"), 0);
        assert_eq!(nest_level(&probes, Some("wC"), "w1"), 1);
        assert_eq!(nest_level(&probes, Some("wC"), "w2"), 2);
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
            agent_session: None,
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
    fn seat_row_prefers_session_then_space() {
        let mut projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        assert_eq!(seat_row(&projects, &rows), None);
        projects[1].focused = true;
        let rows = visible(&projects, "", false, View::Grouped);
        let want = rows.iter().position(|r| matches!(*r, Row::Project(1)));
        assert!(want.is_some());
        assert_eq!(seat_row(&projects, &rows), want);
        projects[1].worktrees[0].agents[0].focused = true;
        assert_eq!(
            seat_row(&projects, &rows),
            rows.iter().position(|r| matches!(*r, Row::Agent(1, 0, 0)))
        );
        projects[1].worktrees[0].agents[0].focused = false;
        projects[1].worktrees[0].focused = true;
        let rows = visible(&projects, "", false, View::Grouped);
        let want_wt = rows.iter().position(|r| matches!(*r, Row::Worktree(1, 0)));
        assert!(want_wt.is_some());
        assert_eq!(seat_row(&projects, &rows), want_wt);
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

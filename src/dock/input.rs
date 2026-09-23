use std::io;
use std::ops::ControlFlow::{self, Break, Continue};
use std::path::Path;

use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};
use ratatui::text::Span;

use super::model::{row_index, row_key, visual_hit, Project, Row, Worktree, NO_SELECTION};
use super::state::{save_state, sync_collapse, touch_hold, touch_release, Memory};
use super::theme::{font_notice_due, font_ok, use_font, worktree_prefix};
use super::Dock;

impl Dock {
    pub(super) fn input(
        &mut self,
        input: Event,
        rows: &[Row],
        output: &mut impl io::Write,
    ) -> ControlFlow<()> {
        if matches!(input, Event::Resize(..)) {
            self.last_drawn.clear();
        }
        if matches!(input, Event::FocusLost) {
            self.copy_menu.close();
            self.settings_dialog = false;
            self.font_dialog = false;
            reset_transient(&mut self.pressed, &mut self.pending_activation, Some(&mut self.hover));
            return Continue(());
        }
        if matches!(input, Event::FocusGained) {
            return Continue(());
        }
        if self.settings_dialog {
            if matches!(input, Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release)
                || matches!(input, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown))
            {
                self.status_line.clear();
            }
            match self.settings_ui.input(&input) {
                crate::settings::Outcome::Close => {
                    self.settings_dialog = false;
                    self.status_line.clear();
                }
                crate::settings::Outcome::Selected => self.status_line.clear(),
                crate::settings::Outcome::Edit { row, forward } => {
                    self.status_line = crate::settings::apply(&mut self.settings_obj, row, forward)
                        .unwrap_or_else(|error| error.to_string());
                }
                crate::settings::Outcome::None => {}
            }
            reset_transient(&mut self.pressed, &mut self.pending_activation, Some(&mut self.hover));
            return Continue(());
        }
        if let Event::Mouse(mouse) = input {
            if mouse.kind == MouseEventKind::Down(MouseButton::Right)
                && self.copy_menu.covers(mouse.column, mouse.row)
            {
                self.copy_menu.close();
                return Continue(());
            }
        }
        if matches!(input, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Right))
        {
            self.copy_menu.close();
        } else if self.copy_menu.is_open() {
            if let Some(message) = self.copy_menu.input(
                &input,
                &self.projects,
                self.session.as_ref(),
                self.sync_error.is_none() && !self.sync.is_stale(),
                output,
            ) {
                self.status_line = message;
            }
            return Continue(());
        }
        match input {
            Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                self.pressed = None;
                if !matches!(key.code, KeyCode::Enter | KeyCode::Char('o'))
                    || self.filtering
                    || self.settings_dialog
                    || self.font_dialog
                {
                    self.pending_activation = None;
                }
                // Mouse motion and resize must not clear status messages.
                self.status_line.clear();
                // Keys drive the selection now: a parked hover would paint a
                // second highlighted row next to it.
                self.hover = None;
                if self.filtering {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                        KeyCode::Backspace => {
                            self.query.pop();
                        }
                        KeyCode::Char(c) => {
                            self.query.push(c);
                        }
                        _ => {}
                    }
                    self.selected = 0;
                    self.offset = 0;
                    return Continue(());
                }
                if self.font_dialog {
                    match key.code {
                        KeyCode::Char('1') => {
                            self.font_detected = font_ok();
                            self.font = use_font(&self.mem.font_choice, self.font_detected);
                            self.font_notice = font_notice_due(&self.mem.font_choice, self.font_detected);
                            self.status_line = if self.font_notice {
                                "still no Nerd Font found".into()
                            } else {
                                "Nerd Font detected".into()
                            };
                            self.font_dialog = false;
                            self.sync.invalidate();
                        }
                        KeyCode::Char('2') => {
                            self.mem.font_choice = "text".into();
                            self.font = false;
                            self.font_notice = false;
                            self.font_dialog = false;
                            self.status_line = "ASCII icons, won't ask again".into();
                            self.mem.dirty_state = true;
                            save_state(&mut self.mem, true, &self.state_path);
                            self.sync.invalidate();
                        }
                        KeyCode::Char('3') => {
                            self.mem.font_choice = "font".into();
                            self.font = true;
                            self.font_notice = false;
                            self.font_dialog = false;
                            self.status_line = "Nerd Font assumed".into();
                            self.mem.dirty_state = true;
                            save_state(&mut self.mem, true, &self.state_path);
                            self.sync.invalidate();
                        }
                        KeyCode::Esc => self.font_dialog = false,
                        _ => {}
                    }
                    return Continue(());
                }
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('o' | 'D' | 'N'))
                    && (self.sync_error.is_some()
                        || self.session.is_none()
                        || (self.sync.is_stale() && matches!(key.code, KeyCode::Char('D' | 'N'))))
                {
                    self.pending_activation = None;
                    return Continue(());
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Break(()),
                    KeyCode::Char('m') => {
                        let y = self.visual_rows
                            .iter()
                            .position(|row| *row == Some(self.selected))
                            .unwrap_or(0);
                        self.copy_menu.open(&self.projects, &rows, self.selected, (0, y as u16));
                    }
                    KeyCode::Enter | KeyCode::Char('o') => {
                        self.pending_activation = PendingActivation::new(
                            &self.projects,
                            &rows,
                            self.selected,
                            key.code == KeyCode::Char('o'),
                        );
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.selected = if self.selected == NO_SELECTION {
                            0
                        } else {
                            self.selected.saturating_add(1).min(rows.len().saturating_sub(1))
                        };
                        self.browsing = true;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.selected = self.selected.min(rows.len()).saturating_sub(1);
                        self.browsing = true;
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        fold_at(
                            &mut self.projects,
                            &mut self.mem,
                            &rows,
                            self.selected,
                            Some(true),
                            &self.state_path,
                        );
                        self.browsing = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        fold_at(
                            &mut self.projects,
                            &mut self.mem,
                            &rows,
                            self.selected,
                            Some(false),
                            &self.state_path,
                        );
                        self.browsing = true;
                    }
                    KeyCode::Char('/') => {
                        self.filtering = true;
                        self.query.clear();
                        self.selected = 0;
                        self.offset = 0;
                    }
                    KeyCode::Char('v') => {
                        self.status_line = crate::settings::apply(&mut self.settings_obj, 3, true)
                            .unwrap_or_else(|error| error.to_string());
                    }
                    KeyCode::Char('c') => self.compact = !self.compact,
                    KeyCode::Char('F') => self.font_dialog = true,
                    KeyCode::Char('s') | KeyCode::Char(',') => {
                        self.settings_dialog = true;
                        self.settings_obj = crate::config::load().unwrap_or_default();
                    }
                    KeyCode::Char('[') | KeyCode::Char('-') => {
                        let new_width = self.settings_obj.width.saturating_sub(2).max(24);
                        if new_width != self.settings_obj.width {
                            self.settings_obj.width = new_width;
                            let _ = crate::config::update(|s| s.width = self.settings_obj.width);
                            self.status_line = format!("dock width: {}", self.settings_obj.width);
                        }
                    }
                    KeyCode::Char(']') | KeyCode::Char('+') | KeyCode::Char('=') => {
                        let new_width = self.settings_obj.width.saturating_add(2).min(80);
                        if new_width != self.settings_obj.width {
                            self.settings_obj.width = new_width;
                            let _ = crate::config::update(|s| s.width = self.settings_obj.width);
                            self.status_line = format!("dock width: {}", self.settings_obj.width);
                        }
                    }
                    KeyCode::Char('J') => {
                        let n = rows.len();
                        let is_att = |r: &Row| match *r {
                            Row::Agent(pi, wi, ai) => {
                                self.projects[pi].worktrees[wi].agents[ai].state.is_attention()
                            }
                            _ => false,
                        };
                        if n > 0 {
                            let start = if self.selected == NO_SELECTION {
                                n - 1
                            } else {
                                self.selected % n
                            };
                            if let Some(off) = (1..=n).find(|k| is_att(&rows[(start + k) % n])) {
                                self.selected = (start + off) % n;
                                self.browsing = true;
                            } else {
                                self.status_line = "no blocked or unacked rows".into();
                            }
                        }
                    }
                    KeyCode::Char('p') => {
                        if let Some(r) = rows.get(self.selected) {
                            let (pi, id) = match *r {
                                Row::Project(pi) => (pi, self.projects[pi].id.clone()),
                                Row::Worktree(pi, _) | Row::Agent(pi, _, _) => {
                                    (pi, self.projects[pi].id.clone())
                                }
                            };
                            if self.mem.pinned.contains(&id) {
                                self.mem.pinned.remove(&id);
                                touch_release(&mut self.mem, &format!("p:{id}"));
                                self.projects[pi].pinned = false;
                            } else {
                                self.mem.pinned.insert(id.clone());
                                touch_hold(&mut self.mem, &format!("p:{id}"));
                                self.projects[pi].pinned = true;
                            }
                            self.mem.dirty_state = true;
                            save_state(&mut self.mem, true, &self.state_path);
                        }
                    }
                    KeyCode::Char('D') => {
                        if let Some(row) = rows.get(self.selected) {
                            let (workspace_id, name) = match *row {
                                Row::Project(pi) => {
                                    let project = &self.projects[pi];
                                    if project.workspaces.len() > 1 {
                                        self.status_line = "Close a worktree row instead".into();
                                        return Continue(());
                                    }
                                    let Some(id) = project.workspaces.first() else { return Continue(()) };
                                    (id, &project.name)
                                }
                                Row::Worktree(pi, wi) => {
                                    let worktree = &self.projects[pi].worktrees[wi];
                                    (&worktree.workspace_id, &worktree.name)
                                }
                                Row::Agent(pi, wi, ai) => {
                                    let worktree = &self.projects[pi].worktrees[wi];
                                    (&worktree.agents[ai].workspace_id, &worktree.name)
                                }
                            };
                            self.copy_menu.confirm_close(workspace_id.clone(), name.clone());
                        }
                    }
                    KeyCode::Char('N') => {
                        let Some(active_session) = &self.session else { return Continue(()) };
                        self.status_line = create_workspace(active_session, &self.projects);
                        self.sync.invalidate();
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Right) if !self.settings_dialog && !self.font_dialog => {
                    reset_transient(&mut self.pressed, &mut self.pending_activation, Some(&mut self.hover));
                    self.status_line.clear();
                    if let Some(index) = visual_hit(&self.visual_rows, m.row) {
                        self.selected = index;
                        self.browsing = true;
                        self.copy_menu.open(&self.projects, &rows, index, (m.column, m.row));
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    reset_transient(&mut self.pressed, &mut self.pending_activation, None);
                    let point = (m.column, m.row).into();
                    if self.new_btn.contains(point) {
                        if let Some(active_session) = &self.session {
                            self.status_line = create_workspace(active_session, &self.projects);
                            self.sync.invalidate();
                        }
                        return Continue(());
                    }
                    if self.settings_btn.contains(point) {
                        self.status_line.clear();
                        self.settings_dialog = true;
                        self.settings_obj = crate::config::load().unwrap_or_default();
                        return Continue(());
                    }

                    if let Some(idx) = visual_hit(&self.visual_rows, m.row) {
                        if idx < rows.len() {
                            self.selected = idx;
                            self.hover = None;
                            self.browsing = true;
                            self.pressed = row_key(&self.projects, &rows, idx)
                                .map(|(kind, id)| (kind, id.to_owned()));
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.pending_activation = None;
                    if let Some((kind, id)) = self.pressed.take() {
                        if let Some(idx) = visual_hit(&self.visual_rows, m.row)
                            .and_then(|idx| release_target(&self.projects, &rows, (kind, &id), idx))
                        {
                            let is_fold = match rows[idx] {
                                Row::Project(_) => true,
                                Row::Worktree(pi, wi) => self.projects
                                    .get(pi)
                                    .and_then(|p| p.worktrees.get(wi))
                                    .is_some_and(|w| is_worktree_fold_hit(w, m.column, self.font)),
                                Row::Agent(_, _, _) => false,
                            };
                            if is_fold {
                                fold_at(&mut self.projects, &mut self.mem, &rows, idx, None, &self.state_path);
                            } else if self.sync_error.is_none() && self.session.is_some() {
                                self.pending_activation =
                                    PendingActivation::new(&self.projects, &rows, idx, false);
                            }
                        }
                    }
                }
                MouseEventKind::Moved => {
                    self.hover = visual_hit(&self.visual_rows, m.row).filter(|idx| *idx < rows.len());
                }
                MouseEventKind::ScrollDown => {
                    reset_transient(&mut self.pressed, &mut self.pending_activation, None);
                    self.selected = if self.selected == NO_SELECTION {
                        0
                    } else {
                        self.selected.saturating_add(3).min(rows.len().saturating_sub(1))
                    };
                    self.browsing = true;
                }
                MouseEventKind::ScrollUp => {
                    reset_transient(&mut self.pressed, &mut self.pending_activation, None);
                    self.selected = self.selected.min(rows.len()).saturating_sub(3);
                    self.browsing = true;
                }
                _ => {}
            },
            _ => {}
        }
        Continue(())
    }
}

/// Native pane navigation also works if agent detection changes after rendering.
fn focus_session(session: &crate::ipc::Session, pane_id: &str) -> io::Result<()> {
    session
        .call("pane.focus", serde_json::json!({ "pane_id": pane_id }))
        .map(|_| ())
}

/// Let Herdr apply its cwd policy and focus the new workspace.
fn create_workspace(session: &crate::ipc::Session, projects: &[Project]) -> String {
    let source = projects
        .iter()
        .flat_map(|p| &p.worktrees)
        .find(|w| w.focused)
        .map(|w| w.workspace_id.as_str());
    match session.call(
        "workspace.create",
        serde_json::json!({ "focus": true, "source_workspace_id": source }),
    ) {
        Ok(_) => "workspace created".into(),
        Err(error) => format!("workspace create failed: {error}"),
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

fn is_worktree_fold_hit(w: &Worktree, col: u16, font: bool) -> bool {
    !w.agents.is_empty()
        && usize::from(col) < worktree_prefix(w, font).iter().map(Span::width).sum()
}

// None toggles; keyboard left/right request an explicit state.
fn fold_at(
    projects: &mut [Project],
    mem: &mut Memory,
    rows: &[Row],
    idx: usize,
    collapsed: Option<bool>,
    path: &Path,
) {
    let Some(row) = rows.get(idx) else {
        return;
    };
    match *row {
        Row::Project(pi) => {
            let collapsed = collapsed.unwrap_or(!projects[pi].collapsed);
            if projects[pi].collapsed == collapsed {
                return;
            }
            projects[pi].collapsed = collapsed;
            let k = format!("c:{}", projects[pi].id);
            if collapsed {
                touch_hold(&mut *mem, &k);
            } else {
                touch_release(&mut *mem, &k);
            }
            sync_collapse(projects, mem, path);
        }
        Row::Worktree(pi, wi) => {
            if projects[pi].worktrees[wi].agents.is_empty() {
                return;
            }
            let collapsed = collapsed.unwrap_or(!projects[pi].worktrees[wi].collapsed);
            if projects[pi].worktrees[wi].collapsed == collapsed {
                return;
            }
            projects[pi].worktrees[wi].collapsed = collapsed;
            let k = format!("w:{}", projects[pi].worktrees[wi].key);
            if collapsed {
                touch_hold(&mut *mem, &k);
            } else {
                touch_release(&mut *mem, &k);
            }
            sync_collapse(projects, mem, path);
        }
        Row::Agent(_, _, _) => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Activation {
    Space(String),
    Session(String),
}

pub(super) fn activation_target(projects: &[Project], rows: &[Row], idx: usize, workspace_only: bool) -> Option<Activation> {
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
            .and_then(|a| {
                if workspace_only {
                    (!a.workspace_id.is_empty()).then(|| Activation::Space(a.workspace_id.clone()))
                } else {
                    (!a.terminal_id.is_empty() && !a.pane_id.is_empty())
                        .then(|| Activation::Session(a.pane_id.clone()))
                }
            }),
    }
}

fn release_target(
    projects: &[Project],
    rows: &[Row],
    pressed: (u8, &str),
    released: usize,
) -> Option<usize> {
    (row_key(projects, rows, released) == Some(pressed)).then_some(released)
}

/// Keep one row identity, not the native address it had before a refresh.
pub(super) struct PendingActivation {
    pub(super) key: (u8, String),
    workspace_only: bool,
}

pub(super) fn reset_transient(
    pressed: &mut Option<(u8, String)>,
    pending_activation: &mut Option<PendingActivation>,
    hover: Option<&mut Option<usize>>,
) {
    *pressed = None;
    *pending_activation = None;
    if let Some(hover) = hover {
        *hover = None;
    }
}

impl PendingActivation {
    fn new(projects: &[Project], rows: &[Row], idx: usize, workspace_only: bool) -> Option<Self> {
        let (kind, id) = row_key(projects, rows, idx)?;
        Some(Self {
            key: (kind, id.to_owned()),
            workspace_only,
        })
    }

    pub(super) fn resolve(&self, projects: &[Project], rows: &[Row]) -> Option<(usize, Activation)> {
        let idx = row_index(projects, rows, (self.key.0, &self.key.1))?;
        let target = activation_target(projects, rows, idx, self.workspace_only)?;
        Some((idx, target))
    }
}

pub(super) fn activate_target(session: &crate::ipc::Session, target: Activation) -> io::Result<()> {
    match target {
        Activation::Space(ws) => focus_workspace(session, &ws),
        Activation::Session(pane) => focus_session(session, &pane),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::model::{restore_selection, visible, View};
    use super::super::state::load_state;
    use super::super::theme::{load_theme, Theme};
    use super::super::view::{signature, SigInput};
    use super::super::stub;
    use ratatui::{text::Line, widgets::Paragraph, Terminal};

    fn sig_input<'a>(projects: &'a [Project], rows: &'a [Row], theme: &'a Theme,
                     settings: &'a crate::config::Settings) -> SigInput<'a> {
        SigInput {
            projects, rows, theme, settings_obj: settings,
            selected: 0, offset: 0, height: 20, hover: None, step: 0, query: "",
            compact: false, view: View::Grouped, status: "", filtering: false,
            font_dialog: false, font: false, settings_dialog: false, settings_row: 0,
        }
    }

    #[test]
    fn browsing_retains_target_when_projects_reorder() {
        let mut projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        let selected = rows
            .iter()
            .position(|row| matches!(row, Row::Agent(0, 0, 0)))
            .unwrap();
        let target = activation_target(&projects, &rows, selected, false);
        let (kind, id) = row_key(&projects, &rows, selected).unwrap();
        let id = id.to_owned();
        projects.reverse();
        let rows = visible(&projects, "", false, View::Grouped);
        let selected = row_index(&projects, &rows, (kind, &id)).unwrap();
        assert_eq!(activation_target(&projects, &rows, selected, false), target);
        projects.clear();
        assert_eq!(row_index(&projects, &[], (kind, &id)), None);
    }

    #[test]
    fn browsing_and_pending_activation_follow_terminal_moves_without_reusing_removed_rows() {
        let mut projects = stub();
        let old_rows = visible(&projects, "", false, View::Grouped);
        let old_index = row_index(&projects, &old_rows, (2, "alpha")).unwrap();
        let pending = PendingActivation::new(&projects, &old_rows, old_index, false).unwrap();
        let workspace = PendingActivation::new(&projects, &old_rows, old_index, true).unwrap();
        projects[0].worktrees[0].agents[0].pane_id = "w9:p8".into();
        projects[0].worktrees[0].agents[0].workspace_id = "w9".into();
        projects[0].worktrees[0].agents.swap(0, 1);
        let rows = visible(&projects, "", false, View::Grouped);
        let selected = restore_selection(&projects, &rows, Some((2, "alpha")), true);
        assert_eq!(
            activation_target(&projects, &rows, selected, false),
            Some(Activation::Session("w9:p8".into()))
        );
        assert_eq!(
            pending.resolve(&projects, &rows).map(|(_, target)| target),
            Some(Activation::Session("w9:p8".into()))
        );
        assert_eq!(
            workspace.resolve(&projects, &rows).map(|(_, target)| target),
            Some(Activation::Space("w9".into()))
        );
        projects[0].worktrees[0]
            .agents
            .retain(|a| a.terminal_id != "alpha");
        let rows = visible(&projects, "", false, View::Grouped);
        assert_eq!(
            activation_target(&projects, &rows, old_index, false),
            Some(Activation::Session("w1:p2".into()))
        );
        let selected = restore_selection(&projects, &rows, Some((2, "alpha")), true);
        assert_eq!(selected, NO_SELECTION);
        assert_eq!(activation_target(&projects, &rows, selected, false), None);
        assert_eq!(
            restore_selection(&projects, &rows, None, true),
            NO_SELECTION
        );
        assert_eq!(pending.resolve(&projects, &rows), None);
        assert_eq!(workspace.resolve(&projects, &rows), None);
    }

    #[test]
    fn changing_order_preserves_agent_targets_and_clears_hidden_headers() {
        let projects = stub();
        let grouped = visible(&projects, "", false, View::Grouped);
        let agent = row_index(&projects, &grouped, (2, "alpha")).unwrap();
        let recent = visible(&projects, "", false, View::Recent);
        let selected = restore_selection(
            &projects,
            &recent,
            row_key(&projects, &grouped, agent),
            true,
        );
        assert_eq!(
            activation_target(&projects, &recent, selected, false),
            Some(Activation::Session("w1:p1".into()))
        );
        let selected = restore_selection(&projects, &recent, Some((0, "stub-a")), true);
        assert_eq!(activation_target(&projects, &recent, selected, false), None);
    }

    #[test]
    fn header_click_checks_identity_and_enter_resolves_current_workspace() {
        let mut projects = stub();
        projects[0].workspaces = vec!["old-workspace".into()];
        let rows = visible(&projects, "", false, View::Grouped);
        let header = row_index(&projects, &rows, (0, "stub-a")).unwrap();
        assert_eq!(
            activation_target(&projects, &rows, header, false),
            Some(Activation::Space("old-workspace".into()))
        );
        let clicked = release_target(&projects, &rows, (0, "stub-a"), header).unwrap();
        let pending = PendingActivation::new(&projects, &rows, clicked, false).unwrap();
        projects[0].workspaces = vec!["current-workspace".into()];
        assert_eq!(
            pending.resolve(&projects, &rows).map(|(_, target)| target),
            Some(Activation::Space("current-workspace".into()))
        );
        let other = row_index(&projects, &rows, (0, "stub-b")).unwrap();
        assert!(release_target(&projects, &rows, (0, "stub-a"), other).is_none());
    }

    #[test]
    fn worktree_click_activates_workspace_and_fold_arrow_folds() {
        let dir = std::env::temp_dir().join(format!("hps-fold-click-{}", std::process::id()));
        let path = dir.join("state.json");
        let mut projects = stub();
        projects[0].worktrees[0].workspace_id = "w1".into();
        let mut mem = Memory::default();
        let rows = visible(&projects, "", false, View::Grouped);
        let idx = row_index(&projects, &rows, (1, "stub-a/main")).unwrap();
        for font in [false, true] {
            for depth in [0, 1, 3] {
                projects[0].worktrees[0].depth = depth;
                for mark in ["▾", "▸"] {
                    let worktree = &projects[0].worktrees[0];
                    let mut spans = Vec::from(worktree_prefix(worktree, font));
                    spans.push(Span::raw(worktree.name.clone()));
                    let line = Line::from(spans);
                    let name_col = (line.width() - Span::raw(&worktree.name).width()) as u16;
                    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
                    terminal.draw(|frame| frame.render_widget(Paragraph::new(line), frame.area())).unwrap();
                    let buffer = terminal.backend().buffer();
                    let marker_col = (0..80).find(|&x| buffer[(x, 0)].symbol() == mark).unwrap();
                    assert!((0..=marker_col).all(|col| is_worktree_fold_hit(worktree, col, font)));
                    assert!(!is_worktree_fold_hit(worktree, name_col, font));
                    let clicked = release_target(&projects, &rows, (1, "stub-a/main"), idx).unwrap();
                    assert_eq!(
                        PendingActivation::new(&projects, &rows, clicked, false).unwrap()
                            .resolve(&projects, &rows).map(|(_, target)| target),
                        Some(Activation::Space("w1".into()))
                    );
                    fold_at(&mut projects, &mut mem, &rows, clicked, None, &path);
                    let visible = visible(&projects, "", false, View::Grouped);
                    assert_eq!(row_index(&projects, &visible, (2, "alpha")).is_none(), mark == "▾");
                }
            }
        }

        let theme = load_theme();
        let settings = crate::config::Settings::default();
        let mut sig_before = String::new();
        signature(
            &sig_input(&projects, &rows, &theme, &settings),
            &mut sig_before,
        );
        projects[0].worktrees[0].agents.clear();
        let worktree = &projects[0].worktrees[0];
        assert!((0..=u16::MAX).all(|col| !is_worktree_fold_hit(worktree, col, false)));
        fold_at(&mut projects, &mut mem, &rows, idx, Some(true), &path);
        assert!(!projects[0].worktrees[0].collapsed);
        assert_eq!(activation_target(&projects, &rows, idx, false), Some(Activation::Space("w1".into())));
        let rows_after = visible(&projects, "", false, View::Grouped);
        let mut sig_after = String::new();
        signature(
            &sig_input(&projects, &rows_after, &theme, &settings),
            &mut sig_after,
        );
        assert_ne!(sig_before, sig_after, "signature must invalidate when worktree agents become empty");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn activation_maps_rows_to_native_targets() {
        let mut projects = stub();
        let rows = visible(&projects, "", false, View::Grouped);
        projects[1].workspaces = vec![];
        let header1 = rows
            .iter()
            .position(|r| matches!(*r, Row::Project(1)))
            .unwrap();
        assert_eq!(activation_target(&projects, &rows, header1, false), None);
        assert_eq!(activation_target(&projects, &rows, 9999, false), None);
    }

    #[test]
    fn mixed_folding_preserves_children_and_noop_folds_do_not_write() {
        let dir = std::env::temp_dir().join(format!("hps-fold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let mut projects = stub();
        let mut mem = Memory::default();
        let rows = visible(&projects, "", false, View::Grouped);
        let project = row_index(&projects, &rows, (0, "stub-a")).unwrap();
        fold_at(&mut projects, &mut mem, &rows, project, Some(false), &path);
        assert!(!path.exists(), "a no-op must not create a state file");

        for (key, project_closed, branch_closed) in [
            ((1, "stub-a/main"), false, true),
            ((0, "stub-a"), true, true),
            ((0, "stub-a"), false, true),
            ((1, "stub-a/main"), false, false),
            ((1, "stub-a/main"), false, true),
            ((1, "stub-a/main"), false, false),
        ] {
            let rows = visible(&projects, "", false, View::Grouped);
            let idx = row_index(&projects, &rows, key).unwrap();
            assert_eq!(release_target(&projects, &rows, key, idx), Some(idx));
            fold_at(&mut projects, &mut mem, &rows, idx, None, &path);
            let rows = visible(&projects, "", false, View::Grouped);
            assert_eq!(
                row_index(&projects, &rows, (1, "stub-a/main")).is_none(),
                project_closed
            );
            assert_eq!(
                row_index(&projects, &rows, (2, "alpha")).is_none(),
                project_closed || branch_closed
            );
            let mut saved = Memory::default();
            load_state(&mut saved, &path);
            assert_eq!(saved.collapsed_projects.contains("stub-a"), project_closed);
            assert_eq!(
                saved.collapsed_worktrees.contains("stub-a/main"),
                branch_closed
            );
        }

        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let rows = visible(&projects, "", false, View::Grouped);
        for key in [(0, "stub-a"), (1, "stub-a/main")] {
            let idx = row_index(&projects, &rows, key).unwrap();
            fold_at(&mut projects, &mut mem, &rows, idx, Some(false), &path);
        }
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

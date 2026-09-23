//! Copy-only row menus. Clipboard writes use Herdr's OSC 52 forwarding.
use super::{row_key, AgentSession, Project, Row, Theme};
use crate::ipc::Session;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    thread::JoinHandle,
};

#[derive(Clone, PartialEq)]
struct Field {
    key: &'static str,
    label: &'static str,
    value: Value,
}

impl Field {
    fn text(&self) -> String {
        match &self.value {
            Value::String(value) => value.clone(),
            Value::Array(values) => values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            value => value.to_string(),
        }
    }
}
#[derive(Clone, PartialEq, Debug)]
pub(super) enum MenuItem {
    Action {
        id: &'static str,
        label: &'static str,
        desc: &'static str,
    },
    Separator,
    Copy {
        field_idx: Option<usize>,
        label: String,
    },
}

impl MenuItem {
    fn label(&self) -> &str {
        match self {
            MenuItem::Action { label, .. } => label,
            MenuItem::Separator => "",
            MenuItem::Copy { label, .. } => label.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Dialog {
    Rename {
        workspace_id: String,
        input: String,
    },
    ConfirmCloseWorkspace {
        workspace_id: String,
        name: String,
    },
    ConfirmDeleteWorktree {
        workspace_id: String,
        name: String,
    },
    NewWorktree {
        workspace_id: String,
        input: String,
    },
    OpenWorktree {
        workspace_id: String,
        input: String,
    },
}

impl Dialog {
    /// The editable text of prompt dialogs; confirmations have none.
    fn input_mut(&mut self) -> Option<&mut String> {
        match self {
            Dialog::Rename { input, .. }
            | Dialog::NewWorktree { input, .. }
            | Dialog::OpenWorktree { input, .. } => Some(input),
            Dialog::ConfirmCloseWorkspace { .. } | Dialog::ConfirmDeleteWorktree { .. } => None,
        }
    }
}

#[derive(Default)]
pub(super) struct Menus {
    current: Option<Menu>,
    dialog: Option<Dialog>,
    /// Last drawn dialog card, confirm button, clear hint and cancel hint.
    dialog_hits: [Rect; 4],
    lookup: Option<JoinHandle<(u64, String, io::Result<Value>)>>,
    serial: u64,
    pub revision: u64,
}

struct Menu {
    serial: u64,
    target: (u8, String),
    agent: Option<(String, Option<AgentSession>)>,
    anchor: (u16, u16),
    fields: Vec<Field>,
    items: Vec<MenuItem>,
    selected: usize,
    offset: usize,
    rect: Rect,
    shown: usize,
    message: String,
    lookup_path: Option<String>,
    lookup_started: bool,
    repository: Option<Value>,
    workspace_id: String,
    workspace_label: String,
}

fn find(projects: &[Project], target: &(u8, String)) -> Option<Row> {
    for (pi, project) in projects.iter().enumerate() {
        if target.0 == 0 && project.id == target.1 {
            return Some(Row::Project(pi));
        }
        for (wi, worktree) in project.worktrees.iter().enumerate() {
            if target.0 == 1 && worktree.key == target.1 {
                return Some(Row::Worktree(pi, wi));
            }
            if target.0 == 2 {
                if let Some(ai) = worktree
                    .agents
                    .iter()
                    .position(|agent| agent.terminal_id == target.1)
                {
                    return Some(Row::Agent(pi, wi, ai));
                }
            }
        }
    }
    None
}

fn fields(projects: &[Project], row: Row) -> (Vec<Field>, Option<String>) {
    let pi = match row {
        Row::Project(pi) | Row::Worktree(pi, _) | Row::Agent(pi, _, _) => pi,
    };
    let project = &projects[pi];
    let mut fields = Vec::new();
    let mut add = |key, label, value: &str| {
        if !value.is_empty() {
            fields.push(Field {
                key,
                label,
                value: json!(value),
            });
        }
    };
    let mut lookup = None;
    match row {
        Row::Project(_) => {
            add("project", "Copy project name", &project.name);
            let known = !project.worktrees.is_empty()
                && project
                    .worktrees
                    .iter()
                    .all(|worktree| !worktree.repo_root.is_empty());
            let paths: std::collections::BTreeSet<&str> = if known {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.repo_root.as_str())
                    .collect()
            } else {
                project
                    .directories
                    .iter()
                    .map(String::as_str)
                    .filter(|path| !path.is_empty())
                    .collect()
            };
            let complete = known || project.directories.iter().all(|path| !path.is_empty());
            if paths.len() == 1 && complete {
                let path = *paths.first().unwrap();
                add(
                    if known {
                        "repository_path"
                    } else {
                        "directory"
                    },
                    if known {
                        "Copy project path"
                    } else {
                        "Copy directory path"
                    },
                    path,
                );
                if !known {
                    lookup = Some(path.to_owned());
                }
            }
            if let Some(key) = project.id.strip_prefix("repo:") {
                add("repository_key", "Copy repository key", key);
            }
            if !project.workspaces.is_empty() {
                fields.push(Field {
                    key: "workspace_ids",
                    label: if project.workspaces.len() == 1 {
                        "Copy workspace ID"
                    } else {
                        "Copy workspace IDs"
                    },
                    value: json!(project.workspaces),
                });
            }
            if !paths.is_empty() && (paths.len() > 1 || !complete) {
                fields.push(Field {
                    key: "directories",
                    label: "Copy directory paths",
                    value: json!(paths),
                });
            }
        }
        Row::Worktree(_, wi) => {
            let worktree = &project.worktrees[wi];
            add(
                if worktree.repo_root.is_empty() {
                    "directory"
                } else {
                    "checkout_path"
                },
                if worktree.repo_root.is_empty() {
                    "Copy directory path"
                } else {
                    "Copy worktree path"
                },
                &worktree.path,
            );
            add("branch", "Copy branch name", &worktree.branch);
            add("workspace_id", "Copy workspace ID", &worktree.workspace_id);
            add("repository_path", "Copy project path", &worktree.repo_root);
            if worktree.repo_root.is_empty() && !worktree.path.is_empty() {
                lookup = Some(worktree.path.clone());
            }
        }
        Row::Agent(_, wi, ai) => {
            let agent = &project.worktrees[wi].agents[ai];
            add("directory", "Copy working directory", &agent.cwd);
            add("pane_id", "Copy pane ID", &agent.pane_id);
            add("terminal_id", "Copy terminal ID", &agent.terminal_id);
            add("tab_id", "Copy tab ID", &agent.tab_id);
            add("workspace_id", "Copy workspace ID", &agent.workspace_id);
            if let Some(session) = &agent.session_ref {
                let (key, label) = match session.kind.as_str() {
                    "path" => ("agent_session_path", "Copy agent session path"),
                    "id" => ("agent_session_id", "Copy agent session ID"),
                    _ => ("agent_session_reference", "Copy session reference"),
                };
                add(key, label, &session.value);
            }
        }
    }
    (fields, lookup)
}
fn build_items(row: Row, projects: &[Project], fields: &[Field]) -> Vec<MenuItem> {
    let mut items = Vec::new();
    match row {
        Row::Worktree(pi, wi) => {
            let worktree = &projects[pi].worktrees[wi];
            let is_linked = worktree.depth > 0
                || (!worktree.repo_root.is_empty() && worktree.path != worktree.repo_root);
            items.push(MenuItem::Action {
                id: "rename",
                label: "Rename",
                desc: "Rename workspace label",
            });
            items.push(MenuItem::Action {
                id: "close",
                label: "Close",
                desc: "Close workspace",
            });
            if is_linked {
                items.push(MenuItem::Action {
                    id: "delete_worktree",
                    label: "Delete worktree checkout...",
                    desc: "Remove worktree and delete directory",
                });
            } else {
                items.push(MenuItem::Action {
                    id: "new_worktree",
                    label: "New worktree",
                    desc: "Create and open new git worktree",
                });
                items.push(MenuItem::Action {
                    id: "open_worktree",
                    label: "Open worktree...",
                    desc: "Open existing git worktree",
                });
            }
            items.push(MenuItem::Separator);
            items.push(MenuItem::Copy {
                field_idx: None,
                label: "Copy reference".into(),
            });
            for (i, f) in fields.iter().enumerate() {
                items.push(MenuItem::Copy {
                    field_idx: Some(i),
                    label: f.label.to_owned(),
                });
            }
        }
        Row::Project(pi) => {
            let project = &projects[pi];
            if !project.workspaces.is_empty() {
                // A repo header can stand for several workspaces. Rename and
                // Close must target exactly one, so a multi-workspace header
                // leaves them to its worktree rows.
                if project.workspaces.len() == 1 {
                    items.push(MenuItem::Action {
                        id: "rename",
                        label: "Rename",
                        desc: "Rename workspace label",
                    });
                    items.push(MenuItem::Action {
                        id: "close",
                        label: "Close",
                        desc: "Close workspace",
                    });
                }
                items.push(MenuItem::Action {
                    id: "new_worktree",
                    label: "New worktree",
                    desc: "Create and open new git worktree",
                });
                items.push(MenuItem::Action {
                    id: "open_worktree",
                    label: "Open worktree...",
                    desc: "Open existing git worktree",
                });
                items.push(MenuItem::Separator);
            }
            items.push(MenuItem::Copy {
                field_idx: None,
                label: "Copy reference".into(),
            });
            for (i, f) in fields.iter().enumerate() {
                items.push(MenuItem::Copy {
                    field_idx: Some(i),
                    label: f.label.to_owned(),
                });
            }
        }
        Row::Agent(_, _, _) => {
            items.push(MenuItem::Copy {
                field_idx: None,
                label: "Copy reference".into(),
            });
            for (i, f) in fields.iter().enumerate() {
                items.push(MenuItem::Copy {
                    field_idx: Some(i),
                    label: f.label.to_owned(),
                });
            }
        }
    }
    items
}


impl Menu {
    fn refresh(&mut self, projects: &[Project]) -> bool {
        let Some(row) = find(projects, &self.target) else {
            return false;
        };
        if let Row::Agent(pi, wi, ai) = row {
            let agent = &projects[pi].worktrees[wi].agents[ai];
            if !self.agent.as_ref().is_some_and(|(vendor, session)| {
                vendor == &agent.vendor && session == &agent.session_ref
            }) {
                return false;
            }
        }
        let (mut fields, lookup) = fields(projects, row);
        if lookup != self.lookup_path {
            self.repository = None;
            self.lookup_started = false;
        }
        self.lookup_path = lookup;
        if let Some(repository) = &self.repository {
            if let Some(path) = repository["repo_root"]
                .as_str()
                .filter(|path| !path.is_empty())
            {
                let field = Field {
                    key: "repository_path",
                    label: "Copy project path",
                    value: json!(path),
                };
                if let Some(directory) = fields
                    .iter_mut()
                    .find(|field| self.target.0 == 0 && field.key == "directory")
                {
                    *directory = field;
                } else {
                    fields.push(field);
                }
            }
            if let Some(key) = repository["repo_key"]
                .as_str()
                .filter(|key| !key.is_empty())
            {
                fields.push(Field {
                    key: "repository_key",
                    label: "Copy repository key",
                    value: json!(key),
                });
            }
            if self.target.0 == 1 {
                if let Some(path) = repository["source_checkout_path"]
                    .as_str()
                    .filter(|path| !path.is_empty())
                {
                    if let Some(field) = fields.iter_mut().find(|field| field.key == "directory") {
                        *field = Field {
                            key: "checkout_path",
                            label: "Copy worktree path",
                            value: json!(path),
                        };
                    }
                }
            }
        }
        let action_key = |field: &Field| match (self.target.0, field.key) {
            (0, "directory") => "repository_path",
            (1, "directory") => "checkout_path",
            (_, key) => key,
        };
        let prev_item = self.items.get(self.selected).cloned();
        let prev_field_key = match &prev_item {
            Some(MenuItem::Copy { field_idx: Some(i), .. }) => {
                self.fields.get(*i).map(action_key)
            }
            _ => None,
        };
        self.fields = fields;
        self.items = build_items(row, projects, &self.fields);
        if let Some(prev_key) = prev_field_key {
            self.selected = self
                .items
                .iter()
                .position(|it| match it {
                    MenuItem::Copy { field_idx: Some(i), .. } => {
                        self.fields.get(*i).map(action_key) == Some(prev_key)
                    }
                    _ => false,
                })
                .unwrap_or(0);
        } else if let Some(MenuItem::Action { id: prev_id, .. }) = prev_item {
            self.selected = self
                .items
                .iter()
                .position(|it| match it {
                    MenuItem::Action { id, .. } => id == &prev_id,
                    _ => false,
                })
                .unwrap_or(0);
        } else {
            self.selected = self
                .items
                .iter()
                .position(|it| matches!(it, MenuItem::Copy { field_idx: None, .. }))
                .unwrap_or(0);
        }
        if matches!(self.items.get(self.selected), Some(MenuItem::Separator)) {
            self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
        }
        true
    }
}

impl Menus {
    pub fn is_open(&self) -> bool {
        self.current.is_some() || self.dialog.is_some()
    }

    pub fn covers(&self, x: u16, y: u16) -> bool {
        if self.dialog.is_some() {
            return true;
        }
        self.current
            .as_ref()
            .is_some_and(|menu| menu.rect.contains((x, y).into()))
    }

    pub fn close(&mut self) {
        // Non-short-circuit `|`: both must be cleared even when the first is set.
        let changed = self.current.take().is_some() | self.dialog.take().is_some();
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn open(&mut self, projects: &[Project], rows: &[Row], index: usize, anchor: (u16, u16)) {
        self.close();
        let Some((kind, id)) = row_key(projects, rows, index) else {
            return;
        };
        let agent = match rows[index] {
            Row::Agent(pi, wi, ai) => {
                let agent = &projects[pi].worktrees[wi].agents[ai];
                Some((agent.vendor.clone(), agent.session_ref.clone()))
            }
            _ => None,
        };
        let (workspace_id, workspace_label) = match rows[index] {
            Row::Worktree(pi, wi) => {
                let w = &projects[pi].worktrees[wi];
                (w.workspace_id.clone(), w.name.clone())
            }
            Row::Project(pi) => {
                let p = &projects[pi];
                (
                    p.workspaces.first().cloned().unwrap_or_default(),
                    p.name.clone(),
                )
            }
            Row::Agent(pi, wi, ai) => {
                let a = &projects[pi].worktrees[wi].agents[ai];
                (a.workspace_id.clone(), a.label.clone())
            }
        };
        let (fields, lookup_path) = fields(projects, rows[index]);
        let items = build_items(rows[index], projects, &fields);
        self.serial = self.serial.wrapping_add(1);
        self.current = Some(Menu {
            serial: self.serial,
            target: (kind, id.to_owned()),
            agent,
            anchor,
            fields,
            items,
            selected: 0,
            offset: 0,
            rect: Rect::default(),
            shown: 0,
            message: String::new(),
            lookup_path,
            lookup_started: false,
            repository: None,
            workspace_id,
            workspace_label,
        });
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn refresh(&mut self, projects: &[Project]) {
        let Some(menu) = &mut self.current else {
            return;
        };
        let before = menu.fields.clone();
        if !menu.refresh(projects) {
            self.close();
        } else if menu.fields != before {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// At most one bounded, on-demand metadata request. Never part of snapshot polling.
    pub fn poll(&mut self, projects: &[Project], session: Option<&Session>, ready: bool) {
        if self.lookup.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Ok((serial, path, result)) = self.lookup.take().unwrap().join() {
                if let Some(menu) = &mut self.current {
                    if menu.serial == serial && menu.lookup_path.as_ref() == Some(&path) {
                        if let Ok(value) = result {
                            menu.repository = Some(value["source"].clone());
                        }
                        menu.refresh(projects);
                        self.revision = self.revision.wrapping_add(1);
                    }
                }
            }
        }
        if !ready || self.lookup.is_some() {
            return;
        }
        let (Some(menu), Some(session)) = (&mut self.current, session) else {
            return;
        };
        let Some(path) = menu.lookup_path.as_ref().filter(|_| !menu.lookup_started) else {
            return;
        };
        let path = path.clone();
        let session = session.clone();
        let serial = menu.serial;
        menu.lookup_started = true;
        match std::thread::Builder::new()
            .name("copy-reference".into())
            .spawn(move || {
                let cwd = std::path::Path::new(&path).components().as_path();
                let result = session.call("worktree.list", json!({ "cwd": cwd }));
                (serial, path, result)
            }) {
            Ok(thread) => self.lookup = Some(thread),
            Err(error) => menu.message = format!("Repository lookup failed: {error}"),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, theme: &Theme, ready: bool, anchor_y: Option<u16>) {
        if let Some(dialog) = self.dialog.as_ref() {
            self.dialog_hits = Self::draw_dialog(frame, theme, dialog);
            return;
        }
        if anchor_y.is_none() {
            self.close();
        }
        let Some(menu) = &mut self.current else {
            return;
        };
        let area = frame.area();
        let title = match menu.target.0 {
            0 => "Project",
            1 => "Worktree",
            _ => "Agent references",
        };
        let text_style = Style::default().fg(theme.idle_fresh);
        let key_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let copy_hint = Line::from(vec![
            Span::styled("[Enter]", key_style),
            Span::styled(" Select", text_style),
        ]);
        let close_hint = Line::from(vec![
            Span::styled("[Esc]", key_style),
            Span::styled(" Close", text_style),
        ]);
        let hint_width = copy_hint.width() + 3 + close_hint.width();
        let label_width = menu
            .items
            .iter()
            .map(|item| item.label().len())
            .max()
            .unwrap_or(0);
        let width = (label_width.max(hint_width).max(title.len()) as u16 + 6)
            .min(area.width.saturating_sub(2));
        let text_width = width.saturating_sub(6);
        let footer_height = if usize::from(text_width) < hint_width {
            2
        } else {
            1
        };
        let wanted_height = menu.items.len() as u16 + 6 + footer_height;
        let bottom = area.bottom().saturating_sub(1);
        let anchor_y = anchor_y
            .unwrap()
            .clamp(area.y, bottom.saturating_sub(1).max(area.y));
        let above = anchor_y.saturating_sub(area.y);
        let below = bottom.saturating_sub(anchor_y + 1);
        let opens_below = below >= wanted_height || below >= above;
        let height = wanted_height.min(if opens_below { below } else { above });
        menu.rect = Rect::default();
        menu.shown = 0;
        if width < 8 || height < footer_height + 4 {
            self.close();
            return;
        }
        let rect = Rect::new(
            menu.anchor.0.clamp(area.x + 1, area.right().saturating_sub(width + 1)),
            if opens_below {
                anchor_y + 1
            } else {
                anchor_y - height
            },
            width,
            height,
        );
        let preview_height = if height >= footer_height + 6 { 2 } else { 1 };
        let preview_gap = u16::from(preview_height == 2);
        menu.rect = rect;
        menu.shown = usize::from(height - 2 - footer_height - preview_height - preview_gap);
        menu.offset = super::ensure_visible(menu.selected, menu.offset, menu.shown);
        let _title_width = usize::from(width - 4)
            - usize::from(menu.offset > 0)
            - usize::from(menu.offset + menu.shown < menu.items.len());
        let mut title_str = format!(" {title} ");
        if menu.offset > 0 {
            title_str.push('↑');
        }
        if menu.offset + menu.shown < menu.items.len() {
            title_str.push('↓');
        }
        for y in area.y..area.bottom() {
            if y != anchor_y {
                for x in area.x..area.right() {
                    frame.buffer_mut()[(x, y)].modifier.insert(Modifier::DIM);
                }
            }
        }
        let bg_color = Color::Rgb(0x18, 0x18, 0x25);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let cell = &mut frame.buffer_mut()[(x, y)];
                cell.set_symbol(" ");
                cell.set_bg(bg_color);
                cell.modifier = Modifier::empty();
            }
        }
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::styled(
                    title_str,
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(bg_color))
                .border_style(Style::default().fg(theme.accent)),
            rect,
        );
        for (line, index) in (menu.offset..menu.items.len())
            .take(menu.shown)
            .enumerate()
        {
            let item = &menu.items[index];
            match item {
                MenuItem::Separator => {
                    frame.render_widget(
                        Paragraph::new("").style(Style::default()),
                        Rect::new(rect.x + 2, rect.y + 1 + line as u16, width.saturating_sub(4), 1),
                    );
                }
                _ => {
                    let label = item.label();
                    let selected = index == menu.selected;
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(if ready { theme.idle_fresh } else { theme.dim })
                            .bg(bg_color)
                    };
                    frame.render_widget(
                        Paragraph::new(format!("{} {label}", if selected { ">" } else { " " }))
                            .style(style),
                        Rect::new(rect.x + 2, rect.y + 1 + line as u16, width.saturating_sub(4), 1),
                    );
                }
            }
        }
        let preview = if !ready {
            "Waiting for Herdr".to_owned()
        } else if !menu.message.is_empty() {
            menu.message.clone()
        } else {
            match menu.items.get(menu.selected) {
                Some(MenuItem::Action { desc, .. }) => (*desc).to_owned(),
                Some(MenuItem::Separator) => String::new(),
                Some(MenuItem::Copy { field_idx: None, .. }) => "IDs and paths as JSON".to_owned(),
                Some(MenuItem::Copy { field_idx: Some(i), .. }) => {
                    menu.fields.get(*i).map(|f| f.text()).unwrap_or_default()
                }
                None => String::new(),
            }
        };
        let preview: String = preview
            .chars()
            .take(usize::from(text_width) * 2)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        frame.render_widget(
            Paragraph::new(preview)
                .wrap(Wrap { trim: false })
                .style(text_style.bg(bg_color)),
            Rect::new(
                rect.x + 4,
                rect.bottom() - 1 - footer_height - preview_height,
                text_width,
                preview_height,
            ),
        );
        let hints = if footer_height == 1 {
            let mut line = copy_hint;
            line.spans
                .push(Span::styled(" · ", Style::default().fg(theme.dim)));
            line.spans.extend(close_hint.spans);
            vec![line]
        } else {
            vec![copy_hint, close_hint]
        };
        for (i, hint) in hints.into_iter().enumerate() {
            frame.render_widget(
                Paragraph::new(hint),
                Rect::new(
                    rect.x + 4,
                    rect.bottom() - 1 - footer_height + i as u16,
                    text_width,
                    1,
                ),
            );
        }
    }

    fn draw_dialog(frame: &mut Frame, theme: &Theme, dialog: &Dialog) -> [Rect; 4] {
        let area = frame.area();
        let (title, prompt, input, btn_label, is_danger) = match dialog {
            Dialog::Rename { input, .. } => (
                "rename workspace",
                "",
                Some(input.as_str()),
                "⏎save",
                false,
            ),
            Dialog::ConfirmCloseWorkspace { .. } => (
                "close workspace",
                "",
                None,
                "⏎close",
                true,
            ),
            Dialog::ConfirmDeleteWorktree { .. } => (
                "delete worktree checkout",
                "",
                None,
                "⏎delete",
                true,
            ),
            Dialog::NewWorktree { input, .. } => (
                "new worktree",
                "branch: ",
                Some(input.as_str()),
                "⏎create",
                false,
            ),
            Dialog::OpenWorktree { input, .. } => (
                "open worktree",
                "branch or path: ",
                Some(input.as_str()),
                "⏎open",
                false,
            ),
        };

        let card_w = area.width.saturating_sub(2).clamp(24, 46);
        let card_h = 7.min(area.height.saturating_sub(2));
        let rect = Rect::new(
            area.x + area.width.saturating_sub(card_w) / 2,
            area.y + area.height.saturating_sub(card_h) / 2,
            card_w,
            card_h,
        );

        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.buffer_mut()[(x, y)].modifier.insert(Modifier::DIM);
            }
        }

        let bg_color = Color::Rgb(0x18, 0x18, 0x25);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let cell = &mut frame.buffer_mut()[(x, y)];
                cell.set_symbol(" ");
                cell.set_bg(bg_color);
                cell.modifier = Modifier::empty();
            }
        }

        frame.render_widget(Clear, rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::styled(
                    format!(" {title} "),
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(bg_color))
                .border_style(Style::default().fg(theme.accent)),
            rect,
        );

        let content_text = if let Dialog::ConfirmDeleteWorktree { name, .. } = dialog {
            format!("  Delete worktree checkout {name}?")
        } else if let Dialog::ConfirmCloseWorkspace { name, .. } = dialog {
            format!("  Close workspace {name}?")
        } else if let Some(inp) = input {
            format!("  {prompt}{inp}█")
        } else {
            format!("  {prompt}")
        };
        frame.render_widget(
            Paragraph::new(content_text).style(Style::default().fg(theme.idle_fresh).bg(bg_color)),
            Rect::new(rect.x + 1, rect.y + 2, rect.width.saturating_sub(2), 1),
        );

        let inner_w = rect.width.saturating_sub(2);
        let btn_style = if is_danger {
            Style::default()
                .fg(theme.blocked)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        };
        let mut controls = Vec::new();
        if inner_w >= 34 {
            controls.push(Span::styled("   ", Style::default().bg(bg_color)));
            controls.push(Span::styled(format!(" {btn_label} "), btn_style));
            controls.push(Span::styled("   ", Style::default().bg(bg_color)));
            if input.is_some() {
                controls.push(Span::styled("^c clear", Style::default().fg(theme.dim).bg(bg_color)));
                controls.push(Span::styled("   ", Style::default().bg(bg_color)));
            }
            controls.push(Span::styled("esc cancel", Style::default().fg(theme.dim).bg(bg_color)));
        } else if inner_w >= 26 {
            controls.push(Span::styled(" ", Style::default().bg(bg_color)));
            controls.push(Span::styled(format!(" {btn_label} "), btn_style));
            controls.push(Span::styled("  ", Style::default().bg(bg_color)));
            if input.is_some() {
                controls.push(Span::styled("^c", Style::default().fg(theme.dim).bg(bg_color)));
                controls.push(Span::styled("  ", Style::default().bg(bg_color)));
            }
            controls.push(Span::styled("esc cancel", Style::default().fg(theme.dim).bg(bg_color)));
        } else {
            controls.push(Span::styled(format!("{btn_label}"), btn_style));
            controls.push(Span::styled(" ", Style::default().bg(bg_color)));
            if input.is_some() {
                controls.push(Span::styled("^c", Style::default().fg(theme.dim).bg(bg_color)));
                controls.push(Span::styled(" ", Style::default().bg(bg_color)));
            }
            controls.push(Span::styled("esc", Style::default().fg(theme.dim).bg(bg_color)));
        }

        let row = Rect::new(rect.x + 1, rect.y + 4, inner_w, 1);
        // Hit rects follow the rendered spans (confirm button, clear and
        // cancel hints), clipped to the row like the paragraph is.
        let mut x = row.x;
        let [mut confirm, mut clear, mut cancel] = [Rect::default(); 3];
        for (i, span) in controls.iter().enumerate() {
            let w = span.width() as u16;
            let hit = Rect::new(x, row.y, w, 1).intersection(row);
            if span.content.contains(btn_label) {
                confirm = hit;
            } else if span.content.starts_with("^c") {
                clear = hit;
            } else if i + 1 == controls.len() {
                cancel = hit;
            }
            x = x.saturating_add(w);
        }
        frame.render_widget(
            Paragraph::new(Line::from(controls)).style(Style::default().bg(bg_color)),
            row,
        );
        [rect, confirm, clear, cancel]
    }

    fn input_dialog(&mut self, event: &Event, session: Option<&Session>) -> Option<String> {
        let dialog = self.dialog.as_mut()?;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc => return self.cancel_dialog(),
                KeyCode::Char('c')
                    if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    if let Some(input) = dialog.input_mut() {
                        input.clear();
                        self.revision = self.revision.wrapping_add(1);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(input) = dialog.input_mut() {
                        input.pop();
                        self.revision = self.revision.wrapping_add(1);
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(input) = dialog.input_mut() {
                        input.push(ch);
                        self.revision = self.revision.wrapping_add(1);
                    }
                }
                KeyCode::Enter => return self.submit_dialog(session),
                _ => {}
            },
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
            {
                let [card, confirm, clear, cancel] = self.dialog_hits;
                let at = |r: Rect| r.contains((mouse.column, mouse.row).into());
                if at(clear) {
                    if let Some(input) = dialog.input_mut() {
                        input.clear();
                        self.revision = self.revision.wrapping_add(1);
                    }
                    return None;
                }
                if at(confirm) {
                    return self.submit_dialog(session);
                }
                if at(cancel) || !at(card) {
                    return self.cancel_dialog();
                }
            }
            _ => {}
        }
        None
    }

    fn cancel_dialog(&mut self) -> Option<String> {
        self.dialog = None;
        self.revision = self.revision.wrapping_add(1);
        Some(String::new())
    }

    /// Enter and the confirm button share one path.
    fn submit_dialog(&mut self, session: Option<&Session>) -> Option<String> {
        let d = self.dialog.take()?;
        self.revision = self.revision.wrapping_add(1);
        let Some(session) = session else {
            return Some("Herdr session unavailable".into());
        };
        match d {
            Dialog::Rename { workspace_id, input } => {
                let label = input.trim();
                if label.is_empty() {
                    return Some("Workspace name cannot be empty".into());
                }
                match session.call(
                    "workspace.rename",
                    json!({ "workspace_id": workspace_id, "label": label }),
                ) {
                    Ok(_) => Some(format!("Renamed workspace to {label}")),
                    Err(e) => Some(format!("Rename failed: {e}")),
                }
            }
            Dialog::ConfirmCloseWorkspace { workspace_id, name } => {
                match session.call("workspace.close", json!({ "workspace_id": workspace_id })) {
                    Ok(_) => Some(format!("Closed workspace {name}")),
                    Err(e) => Some(format!("Close failed: {e}")),
                }
            }
            Dialog::ConfirmDeleteWorktree { workspace_id, name } => {
                match session.call(
                    "worktree.remove",
                    json!({ "workspace_id": workspace_id, "force": false }),
                ) {
                    Ok(_) => Some(format!("Deleted worktree {name}")),
                    Err(e) => Some(format!("Delete failed: {e}")),
                }
            }
            Dialog::NewWorktree { workspace_id, input } => {
                let branch = input.trim();
                if branch.is_empty() {
                    return Some("Branch name cannot be empty".into());
                }
                match session.call(
                    "worktree.create",
                    json!({ "workspace_id": workspace_id, "branch": branch }),
                ) {
                    Ok(_) => Some(format!("Created worktree {branch}")),
                    Err(e) => Some(format!("Create worktree failed: {e}")),
                }
            }
            Dialog::OpenWorktree { workspace_id, input } => {
                let target = input.trim();
                if target.is_empty() {
                    return Some("Target cannot be empty".into());
                }
                match session.call(
                    "worktree.open",
                    json!({ "workspace_id": workspace_id, "branch": target }),
                ) {
                    Ok(_) => Some(format!("Opened worktree {target}")),
                    Err(e) => Some(format!("Open worktree failed: {e}")),
                }
            }
        }
    }

    pub fn input(
        &mut self,
        event: &Event,
        projects: &[Project],
        session: Option<&Session>,
        ready: bool,
        output: &mut impl Write,
    ) -> Option<String> {
        if self.dialog.is_some() {
            return self.input_dialog(event, session);
        }
        let menu = self.current.as_mut()?;
        let selected_before = menu.selected;
        let had_message = !menu.message.is_empty();
        menu.message.clear();
        let mut copy = false;
        let mut close = false;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 'm') => close = true,
                KeyCode::Enter => copy = true,
                KeyCode::Up | KeyCode::Char('k') => {
                    let mut prev = menu.selected.saturating_sub(1);
                    if matches!(menu.items.get(prev), Some(MenuItem::Separator)) {
                        prev = prev.saturating_sub(1);
                    }
                    menu.selected = prev;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let mut next = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
                    if matches!(menu.items.get(next), Some(MenuItem::Separator)) {
                        next = (next + 1).min(menu.items.len().saturating_sub(1));
                    }
                    menu.selected = next;
                }
                KeyCode::Home => menu.selected = 0,
                KeyCode::End => menu.selected = menu.items.len().saturating_sub(1),
                _ => {}
            },
            Event::Mouse(mouse) => {
                let hit = if mouse.column > menu.rect.x
                    && mouse.column < menu.rect.right().saturating_sub(1)
                    && mouse.row > menu.rect.y
                    && mouse.row < menu.rect.y + 1 + menu.shown as u16
                {
                    let idx = menu.offset + usize::from(mouse.row - menu.rect.y - 1);
                    if idx < menu.items.len() && !matches!(menu.items[idx], MenuItem::Separator) {
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(index) = hit {
                            menu.selected = index;
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(index) = hit {
                            menu.selected = index;
                            copy = true;
                        } else {
                            close = true;
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        let mut next = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
                        if matches!(menu.items.get(next), Some(MenuItem::Separator)) {
                            next = (next + 1).min(menu.items.len().saturating_sub(1));
                        }
                        menu.selected = next;
                    }
                    MouseEventKind::ScrollUp => {
                        let mut prev = menu.selected.saturating_sub(1);
                        if matches!(menu.items.get(prev), Some(MenuItem::Separator)) {
                            prev = prev.saturating_sub(1);
                        }
                        menu.selected = prev;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if menu.selected != selected_before || had_message {
            self.revision = self.revision.wrapping_add(1);
        }
        if close {
            self.close();
            return Some(String::new());
        }
        if !copy {
            return None;
        }

        if let Some(item) = menu.items.get(menu.selected).cloned() {
            match item {
                MenuItem::Action { id, .. } => {
                    let ws_id = menu.workspace_id.clone();
                    let ws_label = menu.workspace_label.clone();
                    match id {
                        "rename" => {
                            self.current = None;
                            self.dialog = Some(Dialog::Rename {
                                workspace_id: ws_id,
                                 input: ws_label,
                             });
                             self.revision = self.revision.wrapping_add(1);
                             return None;
                         }
                        "close" => {
                            self.current = None;
                            self.dialog = Some(Dialog::ConfirmCloseWorkspace {
                                workspace_id: ws_id,
                                name: ws_label,
                            });
                            self.revision = self.revision.wrapping_add(1);
                            return None;
                        }
                        "delete_worktree" => {
                             self.current = None;
                             self.dialog = Some(Dialog::ConfirmDeleteWorktree {
                                 workspace_id: ws_id,
                                 name: ws_label,
                             });
                             self.revision = self.revision.wrapping_add(1);
                             return None;
                         }
                        "new_worktree" => {
                             self.current = None;
                             self.dialog = Some(Dialog::NewWorktree {
                                 workspace_id: ws_id,
                                 input: String::new(),
                             });
                             self.revision = self.revision.wrapping_add(1);
                             return None;
                         }
                        "open_worktree" => {
                             self.current = None;
                             self.dialog = Some(Dialog::OpenWorktree {
                                 workspace_id: ws_id,
                                 input: String::new(),
                             });
                             self.revision = self.revision.wrapping_add(1);
                             return None;
                         }
                         _ => {}
                     }
                 }
                MenuItem::Separator => return None,
                MenuItem::Copy { field_idx, .. } => {
                    if !ready || session.is_none() {
                        return None;
                    }
                    let result = (|| {
                        session.unwrap().check()?;
                        if !menu.refresh(projects) {
                            return Err(io::Error::other("Reference target disappeared"));
                        }
                        let text = match field_idx {
                            None => {
                                let mut reference: serde_json::Map<String, Value> = menu
                                    .fields
                                    .iter()
                                    .map(|field| (field.key.to_owned(), field.value.clone()))
                                    .collect();
                                let socket = session.unwrap().socket().to_str().ok_or_else(|| {
                                    io::Error::other(
                                        "Herdr socket path is not UTF-8; copy individual fields",
                                    )
                                })?;
                                reference.insert("herdr_socket".into(), json!(socket));
                                reference.insert(
                                    "kind".into(),
                                    json!(match menu.target.0 {
                                        0 => "project",
                                        1 => "worktree",
                                        _ => "agent",
                                    }),
                                );
                                if let Some((vendor, _)) = &menu.agent {
                                    reference.insert("agent".into(), json!(vendor));
                                }
                                if let Ok(host) = std::env::var("HOSTNAME")
                                    .or_else(|_| std::fs::read_to_string("/etc/hostname"))
                                {
                                    reference.insert("host".into(), json!(host.trim()));
                                }
                                serde_json::to_string_pretty(&reference)?
                            }
                            Some(i) => menu.fields[i].text(),
                        };
                        write_clipboard(output, &text)
                    })();
                    match result {
                        Ok(()) => {
                            self.close();
                            return Some("Clipboard request sent".into());
                        }
                        Err(error) => {
                            menu.message = format!("Copy failed: {error}");
                            self.revision = self.revision.wrapping_add(1);
                            return None;
                        }
                    }
                }
            }
        }
        None
    }
}

/// A single bounded write; Herdr forwards OSC 52 to the viewing client, including SSH.
fn write_clipboard(output: &mut impl Write, text: &str) -> io::Result<()> {
    if text.len() > 64 * 1024 {
        return Err(io::Error::other(
            "Reference exceeds 64 KiB; copy an individual field",
        ));
    }
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut sequence = Vec::with_capacity(text.len().div_ceil(3) * 4 + 8);
    sequence.extend_from_slice(b"\x1b]52;c;");
    for chunk in text.as_bytes().chunks(3) {
        let bits = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        sequence.extend_from_slice(&[
            ALPHABET[(bits >> 18) as usize],
            ALPHABET[((bits >> 12) & 63) as usize],
            if chunk.len() > 1 {
                ALPHABET[((bits >> 6) & 63) as usize]
            } else {
                b'='
            },
            if chunk.len() > 2 {
                ALPHABET[(bits & 63) as usize]
            } else {
                b'='
            },
        ]);
    }
    sequence.push(7);
    output.write_all(&sequence)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use super::super::Worktree;

    #[test]
    fn menu_preserves_its_source_row_and_mouse_targets_at_viewport_edges() {
        use ratatui::{backend::TestBackend, Terminal};

        let projects = super::super::stub();
        let theme = super::super::load_theme();
        for (width, height, source_y) in [(36, 24, 1), (80, 24, 21), (24, 18, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut menus = Menus::default();
            // The row can move after opening, for example after a snapshot refresh.
            menus.open(&projects, &[Row::Project(0)], 0, (width - 1, 1));
            let draw = |menus: &mut Menus, terminal: &mut Terminal<TestBackend>| {
                terminal
                    .draw(|frame| {
                        frame.render_widget(
                            Paragraph::new("source row"),
                            Rect::new(0, source_y, width, 1),
                        );
                        menus.draw(frame, &theme, true, Some(source_y));
                    })
                    .unwrap();
            };
            draw(&mut menus, &mut terminal);
            let menu = menus.current.as_ref().unwrap();
            let rect = menu.rect;
            assert!(rect.bottom() <= source_y || rect.y > source_y);
            assert!(rect.x > 0 && rect.right() < width);
            assert!(rect.bottom() < height, "the dock toolbar must stay visible");
            let source: String = (0..10)
                .map(|x| terminal.backend().buffer()[(x, source_y)].symbol())
                .collect();
            assert_eq!(source, "source row");
            for y in 0..height {
                for x in 0..width {
                    assert_eq!(
                        terminal.backend().buffer()[(x, y)]
                            .modifier
                            .contains(Modifier::DIM),
                        y != source_y && !rect.contains((x, y).into()),
                        "only the menu and its source row should remain undimmed"
                    );
                }
            }

            // Scroll to the final action, then hover the first visible action.
            menus.input(
                &Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
                &projects,
                None,
                true,
                &mut io::sink(),
            );
            draw(&mut menus, &mut terminal);
            let menu = menus.current.as_ref().unwrap();
            let first = menu.offset;
            let rect = menu.rect;
            if first > 0 {
                assert!(
                    (rect.x..rect.right())
                        .any(|x| terminal.backend().buffer()[(x, rect.y)].symbol() == "↑"),
                    "hidden actions need a visible scroll indicator"
                );
            }
            menus.input(
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: rect.x + 4,
                    row: rect.y + 1,
                    modifiers: KeyModifiers::NONE,
                }),
                &projects,
                None,
                true,
                &mut io::sink(),
            );
            assert_eq!(menus.current.as_ref().unwrap().selected, first);
            draw(&mut menus, &mut terminal);
            assert!(terminal.backend().buffer()[(rect.x + 4, rect.y + 1)]
                .modifier
                .contains(Modifier::REVERSED));
            menus.input(
                &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                &projects,
                None,
                true,
                &mut io::sink(),
            );
            draw(&mut menus, &mut terminal);
            assert!(!menus.is_open());
            assert!(!terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::DIM)));
        }
    }

    #[test]
    fn undrawable_menu_does_not_capture_input() {
        use ratatui::{backend::TestBackend, Terminal};

        let projects = super::super::stub();
        let theme = super::super::load_theme();
        for (width, height, row) in [(32, 10, 4), (36, 8, 3)] {
            let mut menus = Menus::default();
            menus.open(&projects, &[Row::Project(0)], 0, (0, row));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| menus.draw(frame, &theme, true, Some(row)))
                .unwrap();
            assert!(!menus.is_open(), "an invisible menu must not capture input");
        }
    }

    #[test]
    fn short_menu_surfaces_clipboard_write_errors() {
        use ratatui::{backend::TestBackend, Terminal};

        struct RejectCopy;
        impl Write for RejectCopy {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("clipboard denied"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let socket = std::env::temp_dir().join(format!(
            "hps-menu-error-{}-{}.sock",
            std::process::id(),
            super::super::now_unix_ms()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let session = Session::at(socket.clone()).unwrap();
        let projects = super::super::stub();
        let mut menus = Menus::default();
        menus.open(&projects, &[Row::Project(0)], 0, (0, 6));
        menus.input(
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &projects,
            Some(&session),
            true,
            &mut RejectCopy,
        );
        let mut terminal = Terminal::new(TestBackend::new(32, 14)).unwrap();
        terminal
            .draw(|frame| menus.draw(frame, &super::super::load_theme(), true, Some(6)))
            .unwrap();
        drop(listener);
        std::fs::remove_file(socket).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("clipboard"),
            "copy errors must remain visible in short menus"
        );
    }

    #[test]
    fn menu_repaint_signal_ignores_input_that_changes_nothing() {
        let projects = super::super::stub();
        let mut menus = Menus::default();
        menus.open(&projects, &[Row::Project(0)], 0, (0, 0));
        let mut output = io::sink();
        let revision = menus.revision;
        // The dock consumes revision as its repaint signal.
        for input in [
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column: u16::MAX,
                row: u16::MAX,
                modifiers: KeyModifiers::NONE,
            }),
        ] {
            menus.input(&input, &projects, None, true, &mut output);
            assert_eq!(menus.revision, revision);
        }
        menus.input(
            &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &projects,
            None,
            true,
            &mut output,
        );
        assert_ne!(menus.revision, revision, "selection changes must repaint");
    }

    #[test]
    fn clipboard_encodes_bytes_without_terminal_control_injection() {
        for (text, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("λ\n\u{1b}", "zrsKGw=="),
        ] {
            let mut output = Vec::new();
            write_clipboard(&mut output, text).unwrap();
            assert_eq!(output, format!("\x1b]52;c;{encoded}\x07").as_bytes());
        }
        let mut output = Vec::new();
        assert!(write_clipboard(&mut output, &"x".repeat(64 * 1024 + 1)).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn copy_tracks_terminal_moves_and_rejects_stale_or_replaced_agents() {
        let socket = std::env::temp_dir().join(format!(
            "hps-menu-{}-{}.sock",
            std::process::id(),
            super::super::now_unix_ms()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let session = Session::at(socket.clone()).unwrap();
        let mut projects = super::super::stub();
        let mut menus = Menus::default();
        menus.open(&projects, &[Row::Agent(0, 0, 0)], 0, (0, 0));
        let mut output = Vec::new();
        let down = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        menus.input(&down, &projects, Some(&session), true, &mut output);
        menus.input(&down, &projects, Some(&session), true, &mut output);
        let mut agent = projects[0].worktrees[0].agents.remove(0);
        agent.pane_id = "w2:p9".into();
        agent.workspace_id = "w2".into();
        projects[1].worktrees[0].agents.push(agent);
        menus.refresh(&projects);
        menus.input(&enter, &projects, Some(&session), false, &mut output);
        assert!(
            output.is_empty(),
            "stale references must not reach the clipboard"
        );
        menus.input(&enter, &projects, Some(&session), true, &mut output);
        assert_eq!(output, b"\x1b]52;c;dzI6cDk=\x07");
        output.clear();
        menus.open(&projects, &[Row::Agent(1, 0, 1)], 0, (0, 0));
        projects[1].worktrees[0].agents[1].session_ref = Some(AgentSession {
            kind: "path".into(),
            value: "/new/conversation.jsonl".into(),
        });
        menus.refresh(&projects);
        menus.input(&enter, &projects, Some(&session), true, &mut output);
        assert!(
            output.is_empty(),
            "a replacement conversation must not inherit an open menu"
        );
        menus.open(&projects, &[Row::Agent(1, 0, 1)], 0, (0, 0));
        let end = Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        menus.input(&end, &projects, Some(&session), true, &mut output);
        menus.input(&enter, &projects, Some(&session), true, &mut output);
        assert_eq!(output, b"\x1b]52;c;L25ldy9jb252ZXJzYXRpb24uanNvbmw=\x07");
        output.clear();
        menus.open(&projects, &[Row::Agent(1, 0, 1)], 0, (0, 0));
        projects[1].worktrees[0].agents.remove(1);
        menus.refresh(&projects);
        menus.input(&enter, &projects, Some(&session), true, &mut output);
        assert!(
            output.is_empty(),
            "a removed target must not copy another row"
        );
        // A project includes ordinary shell panes, not just the displayed agents.
        projects = super::super::snapshot(&json!({
            "workspaces": [{"workspace_id":"w1","label":"Mixed paths"}],
            "agents": [{"terminal_id":"agent","pane_id":"w1:p1","workspace_id":"w1","cwd":"/agent-repo"}],
            "panes": [
                {"workspace_id":"w1","cwd":"/agent-repo"},
                {"workspace_id":"w1","cwd":"/other-repo"},
                {"workspace_id":"w1","cwd":"/plugin","tokens":{"hps_dock":"projects"}}
            ]
        }), &super::super::Memory::default(), &[Color::White], false, 0).unwrap();
        menus.open(&projects, &[Row::Project(0)], 0, (0, 0));
        menus.input(&end, &projects, Some(&session), true, &mut output);
        menus.input(&enter, &projects, Some(&session), true, &mut output);
        assert_eq!(output, b"\x1b]52;c;L2FnZW50LXJlcG8KL290aGVyLXJlcG8=\x07");
        output.clear();

        use std::os::unix::ffi::OsStringExt;
        let invalid_path =
            socket.with_extension(std::ffi::OsString::from_vec(b"sock-\xff".to_vec()));
        let invalid_listener = std::os::unix::net::UnixListener::bind(&invalid_path).unwrap();
        let invalid_session = Session::at(invalid_path.clone()).unwrap();
        menus.open(&projects, &[Row::Project(0)], 0, (0, 0));
        let revision = menus.revision;
        menus.input(&enter, &projects, Some(&invalid_session), true, &mut output);
        assert!(
            output.is_empty(),
            "a non-UTF-8 socket must fail instead of copying a corrupt path"
        );
        assert!(menus.is_open(), "copy failure must leave the menu usable");
        assert_ne!(menus.revision, revision, "copy errors must repaint");
        drop(invalid_listener);
        std::fs::remove_file(invalid_path).unwrap();
        drop(listener);
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn worktree_context_menu_items_and_theming() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut projects = super::super::stub();
        projects[0].worktrees[0].workspace_id = "w1".into();
        // Linked worktree: depth 1
        projects[0].worktrees.push(Worktree {
            key: "stub-a/linked".into(),
            name: "linked-tree".into(),
            branch: "linked".into(),
            path: "/projects/linked".into(),
            repo_root: "/projects/repo".into(),
            collapsed: false,
            depth: 1,
            workspace_id: "w2D".into(),
            focused: false,
            agents: vec![],
        });

        let rows = super::super::visible(&projects, "", false, super::super::View::Grouped);
        let primary_idx = super::super::row_index(&projects, &rows, (1, "stub-a/main")).unwrap();
        let linked_idx = super::super::row_index(&projects, &rows, (1, "stub-a/linked")).unwrap();

        let mut menus = Menus::default();

        // Primary worktree menu
        menus.open(&projects, &rows, primary_idx, (10, 5));
        let menu = menus.current.as_ref().unwrap();
        let labels: Vec<&str> = menu.items.iter().map(|it| it.label()).collect();
        assert_eq!(
            &labels[..4],
            &["Rename", "Close", "New worktree", "Open worktree..."]
        );
        assert!(matches!(menu.items[4], MenuItem::Separator));
        assert_eq!(menu.items[5].label(), "Copy reference");

        // Linked worktree menu
        menus.open(&projects, &rows, linked_idx, (10, 5));
        let menu = menus.current.as_ref().unwrap();
        let labels: Vec<&str> = menu.items.iter().map(|it| it.label()).collect();
        assert_eq!(
            &labels[..3],
            &["Rename", "Close", "Delete worktree checkout..."]
        );
        assert!(matches!(menu.items[3], MenuItem::Separator));
        assert_eq!(menu.items[4].label(), "Copy reference");

        // Theming: border and selection style
        let theme = super::super::load_theme();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|f| menus.draw(f, &theme, true, Some(5)))
            .unwrap();
        let menu = menus.current.as_ref().unwrap();
        let rect = menu.rect;
        // Selected item follows Herdr's ui.accent, like native popups.
        let sel_cell = &terminal.backend().buffer()[(rect.x + 4, rect.y + 1)];
        assert!(sel_cell.modifier.contains(Modifier::REVERSED));
        assert_eq!(sel_cell.fg, theme.accent);
    }

    #[test]
    fn rename_dialog_workflow() {
        let mut projects = super::super::stub();
        projects[0].worktrees[0].workspace_id = "w1".into();
        let rows = super::super::visible(&projects, "", false, super::super::View::Grouped);

        let mut menus = Menus::default();
        menus.open(&projects, &rows, 1, (10, 5)); // open worktree menu

        // Press Enter on Rename (index 0)
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        menus.input(&enter, &projects, None, true, &mut io::sink());

        assert!(menus.dialog.is_some());
        assert!(menus.current.is_none());
        if let Some(Dialog::Rename { workspace_id, input }) = &menus.dialog {
            assert_eq!(workspace_id, "w1");
            assert_eq!(input, "main");
        } else {
            panic!("expected Rename dialog");
        }

        // Type new name
        let ch_a = Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        menus.input(&ch_a, &projects, None, true, &mut io::sink());
        if let Some(Dialog::Rename { input, .. }) = &menus.dialog {
            assert_eq!(input, "main2");
        }

        // Backspace
        let bs = Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        menus.input(&bs, &projects, None, true, &mut io::sink());
        if let Some(Dialog::Rename { input, .. }) = &menus.dialog {
            assert_eq!(input, "main");
        }

        // Ctrl+C clears
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        menus.input(&ctrl_c, &projects, None, true, &mut io::sink());
        if let Some(Dialog::Rename { input, .. }) = &menus.dialog {
            assert_eq!(input, "");
        }

        // Esc cancels
        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        menus.input(&esc, &projects, None, true, &mut io::sink());
        assert!(menus.dialog.is_none());
        assert!(!menus.is_open());
    }

    #[test]
    fn dialog_buttons_respond_to_mouse_where_drawn() {
        use ratatui::{backend::TestBackend, Terminal};

        let theme = super::super::load_theme();
        let projects = super::super::stub();
        let click = |column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        // Locate text on screen, not in our own rect math.
        let locate = |terminal: &Terminal<TestBackend>, text: &str| {
            let buffer = terminal.backend().buffer();
            let want: Vec<char> = text.chars().collect();
            (0..buffer.area.height)
                .find_map(|y| {
                    let row: Vec<char> = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                        .collect();
                    row.windows(want.len())
                        .position(|w| w == want.as_slice())
                        .map(|x| (x as u16, y))
                })
                .unwrap_or_else(|| panic!("{text:?} not drawn"))
        };
        let dialogs = [
            (Dialog::Rename { workspace_id: "w1".into(), input: "demo".into() }, "⏎save"),
            (Dialog::ConfirmCloseWorkspace { workspace_id: "w1".into(), name: "demo".into() }, "⏎close"),
            (Dialog::ConfirmDeleteWorktree { workspace_id: "w1".into(), name: "demo".into() }, "⏎delete"),
            (Dialog::NewWorktree { workspace_id: "w1".into(), input: "demo".into() }, "⏎create"),
            (Dialog::OpenWorktree { workspace_id: "w1".into(), input: "demo".into() }, "⏎open"),
        ];
        for width in [60, 30, 24] {
            for (dialog, button) in &dialogs {
                let case = format!("{button} at width {width}");
                let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
                let mut menus = Menus::default();
                let open = |menus: &mut Menus, terminal: &mut Terminal<TestBackend>| {
                    menus.dialog = Some(dialog.clone());
                    terminal.draw(|f| menus.draw(f, &theme, true, Some(0))).unwrap();
                };
                open(&mut menus, &mut terminal);
                let card = menus.dialog_hits[0];
                menus.input(&click(card.x + 1, card.y + 1), &projects, None, true, &mut io::sink());
                assert!(menus.dialog.is_some(), "{case}: a click inside the card must not dismiss");

                if menus.dialog.as_mut().and_then(Dialog::input_mut).is_some() {
                    let (x, y) = locate(&terminal, "^c");
                    let msg = menus.input(&click(x, y), &projects, None, true, &mut io::sink());
                    assert_eq!(msg, None, "{case}");
                    assert_eq!(
                        menus.dialog.as_mut().and_then(Dialog::input_mut).map(|s| s.as_str()),
                        Some(""),
                        "{case}: clicking ^c clears the input and keeps the dialog"
                    );
                }

                let (x, y) = locate(&terminal, button);
                let msg = menus.input(&click(x + 2, y), &projects, None, true, &mut io::sink());
                assert_eq!(msg.as_deref(), Some("Herdr session unavailable"), "{case}");
                assert!(menus.dialog.is_none(), "{case}");

                open(&mut menus, &mut terminal);
                let (x, y) = locate(&terminal, "esc");
                let msg = menus.input(&click(x, y), &projects, None, true, &mut io::sink());
                assert_eq!(msg.as_deref(), Some(""), "{case}");
                assert!(menus.dialog.is_none(), "{case}");
            }
        }
    }

    #[test]
    fn multi_workspace_header_never_offers_close_or_rename() {
        let mut projects = super::super::stub();
        let ids = |projects: &[Project]| {
            let (fields, _) = fields(projects, Row::Project(0));
            build_items(Row::Project(0), projects, &fields)
                .into_iter()
                .filter_map(|item| match item {
                    MenuItem::Action { id, .. } => Some(id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        projects[0].workspaces = vec!["w1".into()];
        assert!(ids(&projects).contains(&"close") && ids(&projects).contains(&"rename"));
        // Which of two workspaces "Close" would hit is arbitrary: offer neither.
        projects[0].workspaces = vec!["w1".into(), "w2".into()];
        let multi = ids(&projects);
        assert!(!multi.contains(&"close") && !multi.contains(&"rename"), "{multi:?}");
    }
}

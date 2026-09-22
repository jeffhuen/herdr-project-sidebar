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

#[derive(Default)]
pub(super) struct Menus {
    current: Option<Menu>,
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
    selected: usize,
    offset: usize,
    rect: Rect,
    shown: usize,
    message: String,
    lookup_path: Option<String>,
    lookup_started: bool,
    repository: Option<Value>,
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
        let selected_key = self
            .selected
            .checked_sub(1)
            .and_then(|index| self.fields.get(index))
            .map(action_key);
        self.selected = selected_key
            .and_then(|key| {
                fields
                    .iter()
                    .position(|field| action_key(field) == key)
                    .map(|index| index + 1)
            })
            .unwrap_or(0);
        self.fields = fields;
        true
    }
}

impl Menus {
    pub fn is_open(&self) -> bool {
        self.current.is_some()
    }

    pub fn covers(&self, x: u16, y: u16) -> bool {
        self.current
            .as_ref()
            .is_some_and(|menu| menu.rect.contains((x, y).into()))
    }

    pub fn close(&mut self) {
        if self.current.take().is_some() {
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
        let (fields, lookup_path) = fields(projects, rows[index]);
        self.serial = self.serial.wrapping_add(1);
        self.current = Some(Menu {
            serial: self.serial,
            target: (kind, id.to_owned()),
            agent,
            anchor,
            fields,
            selected: 0,
            offset: 0,
            rect: Rect::default(),
            shown: 0,
            message: String::new(),
            lookup_path,
            lookup_started: false,
            repository: None,
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
        if anchor_y.is_none() {
            self.close();
        }
        let Some(menu) = &mut self.current else {
            return;
        };
        let area = frame.area();
        let title = match menu.target.0 {
            0 => "Project references",
            1 => "Worktree references",
            _ => "Agent references",
        };
        let text_style = Style::default().fg(theme.idle_fresh);
        let key_style = Style::default()
            .fg(theme.working)
            .add_modifier(Modifier::BOLD);
        let copy_hint = Line::from(vec![
            Span::styled("[Enter]", key_style),
            Span::styled(" Copy", text_style),
        ]);
        let close_hint = Line::from(vec![
            Span::styled("[Esc]", key_style),
            Span::styled(" Close", text_style),
        ]);
        let hint_width = copy_hint.width() + 3 + close_hint.width();
        let label_width = menu
            .fields
            .iter()
            .map(|field| field.label.len())
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
        let wanted_height = menu.fields.len() as u16 + 6 + footer_height;
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
            menu.anchor.0.clamp(area.x + 1, area.right() - width - 1),
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
        let title_width = usize::from(width - 4)
            - usize::from(menu.offset > 0)
            - usize::from(menu.offset + menu.shown <= menu.fields.len());
        let title = if title.len() > title_width {
            title.strip_suffix(" references").unwrap_or(title)
        } else {
            title
        };
        // Titles are fixed ASCII; leave the scroll markers visible even in tiny panes.
        let title = &title[..title.len().min(title_width)];
        let mut title = format!(" {title} ");
        if menu.offset > 0 {
            title.push('↑');
        }
        if menu.offset + menu.shown <= menu.fields.len() {
            title.push('↓');
        }
        for y in area.y..area.bottom() {
            if y != anchor_y {
                for x in area.x..area.right() {
                    frame.buffer_mut()[(x, y)].modifier.insert(Modifier::DIM);
                }
            }
        }
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::styled(title, text_style))
                .style(Style::default().bg(Color::Reset))
                .border_style(Style::default().fg(theme.dim)),
            rect,
        );
        for (line, index) in (menu.offset..=menu.fields.len())
            .take(menu.shown)
            .enumerate()
        {
            let label = if index == 0 {
                "Copy reference"
            } else {
                menu.fields[index - 1].label
            };
            let selected = index == menu.selected;
            let style = Style::default().fg(if ready { theme.idle_fresh } else { theme.dim });
            let style = if selected {
                style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                style
            };
            frame.render_widget(
                Paragraph::new(format!("{} {label}", if selected { ">" } else { " " }))
                    .style(style),
                Rect::new(rect.x + 2, rect.y + 1 + line as u16, width - 4, 1),
            );
        }
        let preview = if !ready {
            "Waiting for Herdr".to_owned()
        } else if !menu.message.is_empty() {
            menu.message.clone()
        } else if menu.selected == 0 {
            "IDs and paths as JSON".to_owned()
        } else {
            menu.fields[menu.selected - 1].text()
        };
        let preview: String = preview
            .chars()
            .take(usize::from(text_width) * 2)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        frame.render_widget(
            Paragraph::new(preview)
                .wrap(Wrap { trim: false })
                .style(text_style),
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
        frame.render_widget(
            Paragraph::new(hints),
            Rect::new(
                rect.x + 4,
                rect.bottom() - 1 - footer_height,
                text_width,
                footer_height,
            ),
        );
    }

    pub fn input(
        &mut self,
        event: &Event,
        projects: &[Project],
        session: Option<&Session>,
        ready: bool,
        output: &mut impl Write,
    ) -> Option<String> {
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
                KeyCode::Up | KeyCode::Char('k') => menu.selected = menu.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    menu.selected = (menu.selected + 1).min(menu.fields.len())
                }
                KeyCode::Home => menu.selected = 0,
                KeyCode::End => menu.selected = menu.fields.len(),
                _ => {}
            },
            Event::Mouse(mouse) => {
                let hit = if mouse.column > menu.rect.x
                    && mouse.column < menu.rect.right().saturating_sub(1)
                    && mouse.row > menu.rect.y
                    && mouse.row < menu.rect.y + 1 + menu.shown as u16
                {
                    Some(menu.offset + usize::from(mouse.row - menu.rect.y - 1))
                        .filter(|index| *index <= menu.fields.len())
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
                        menu.selected = (menu.selected + 1).min(menu.fields.len())
                    }
                    MouseEventKind::ScrollUp => menu.selected = menu.selected.saturating_sub(1),
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
        if !ready || session.is_none() {
            return None;
        }
        let result = (|| {
            session.unwrap().check()?;
            if !menu.refresh(projects) {
                return Err(io::Error::other("Reference target disappeared"));
            }
            let text = if menu.selected == 0 {
                let mut reference: serde_json::Map<String, Value> = menu
                    .fields
                    .iter()
                    .map(|field| (field.key.to_owned(), field.value.clone()))
                    .collect();
                let socket = session.unwrap().socket().to_str().ok_or_else(|| {
                    io::Error::other("Herdr socket path is not UTF-8; copy individual fields")
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
                if let Ok(host) =
                    std::env::var("HOSTNAME").or_else(|_| std::fs::read_to_string("/etc/hostname"))
                {
                    reference.insert("host".into(), json!(host.trim()));
                }
                serde_json::to_string_pretty(&reference)?
            } else {
                menu.fields[menu.selected - 1].text()
            };
            write_clipboard(output, &text)
        })();
        match result {
            Ok(()) => {
                self.close();
                Some("Clipboard request sent".into())
            }
            Err(error) => {
                menu.message = format!("Copy failed: {error}");
                self.revision = self.revision.wrapping_add(1);
                None
            }
        }
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
}

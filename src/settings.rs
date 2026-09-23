use std::io;
use std::time::Duration;

use crate::config::{self, IconMode, Settings};
use crossterm::{
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};

pub const LABELS: [&str; 10] = [
    "Width",
    "Dock side",
    "Auto open",
    "Order",
    "Branches",
    "Task titles",
    "Quiet idle titles",
    "Agent icons",
    "Sidebar style",
    "Install compact font",
];
pub const HELP: [&str; 10] = [
    "Dock split width in columns, 24-80. Left/Right changes by 2.",
    "Dock split opens on the right or left edge. Left/Right flips the side.",
    "Open a missing dock automatically. Closing it snoozes that tab for this Herdr session; toggle reopens it.",
    "Project groups nest linked worktrees. Recent uses Herdr's state-change order.",
    "Show branch and git status in Spaces, and worktree branches in Agents.",
    "Show the session's task title, or just its agent name.",
    "Use a quieter, readable color for idle titles. State marks and lifecycle come from Herdr.",
    "Text needs no extra font. Font uses the compact face and requires a terminal codepoint map. None hides logos.",
    "Projects or your original Herdr rows. Also toggle with prefix+p; visibility is unchanged.",
    "Install the optional icon face for this user. Map U+E1A0-U+E1B0 to Herdr Agent Icons Compact in your terminal.",
];

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableFocusChange, LeaveAlternateScreen);
    }
}

pub fn values(settings: &Settings) -> [String; 10] {
    let on = |value| if value { "On" } else { "Off" }.to_owned();
    let side = || if settings.dock_right { "Right" } else { "Left" }.to_owned();
    [
        format!("[-] {} [+]", settings.width),
        side(),
        on(settings.auto_open),
        if settings.grouped {
            "Projects"
        } else {
            "Recent"
        }
        .into(),
        on(settings.show_branch),
        on(settings.show_title),
        on(settings.dim_idle),
        match settings.icons {
            IconMode::Text => "Text",
            IconMode::Font => "Font",
            IconMode::None => "None",
        }
        .into(),
        if settings.project_style {
            "Projects"
        } else {
            "Herdr"
        }
        .into(),
        "[Install]".into(),
    ]
}

fn change(settings: &mut Settings, row: usize, forward: bool) {
    match row {
        0 => {
            settings.width = if forward {
                settings.width.saturating_add(2).min(80)
            } else {
                settings.width.saturating_sub(2).max(24)
            }
        }
        1 => settings.dock_right = !settings.dock_right,
        2 => settings.auto_open = !settings.auto_open,
        3 => settings.grouped = !settings.grouped,
        4 => settings.show_branch = !settings.show_branch,
        5 => settings.show_title = !settings.show_title,
        6 => settings.dim_idle = !settings.dim_idle,
        7 => {
            settings.icons = match (settings.icons, forward) {
                (IconMode::Text, true) | (IconMode::None, false) => IconMode::Font,
                (IconMode::Font, true) | (IconMode::Text, false) => IconMode::None,
                _ => IconMode::Text,
            }
        }
        8 => settings.project_style = !settings.project_style,
        _ => {}
    }
}

pub fn apply(settings: &mut Settings, row: usize, forward: bool) -> io::Result<String> {
    if row == 9 {
        return crate::icons::install().map(|_| "Installed. Map U+E1A0-U+E1B0 to Herdr Agent Icons Compact in your terminal, then choose Font.".to_owned());
    }
    let mut candidate = settings.clone();
    change(&mut candidate, row, forward);
    if candidate == *settings {
        return Ok(String::new());
    }
    *settings = config::update(|current| change(current, row, forward))?;
    crate::native::start()?;
    Ok(if row == 2 {
        "Saved. An open dock still follows tabs; manually closed tabs stay snoozed."
    } else {
        "Saved."
    }
    .to_owned())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    None,
    Selected,
    Edit { row: usize, forward: bool },
    Close,
}

pub struct SettingsUi {
    pub selected: usize,
    offset: usize,
    card: bool,
    hits: [Rect; 10],
    less: Rect,
    more: Rect,
    close: Rect,
}

impl SettingsUi {
    pub fn new(card: bool) -> Self {
        Self {
            selected: 0, offset: 0, card,
            hits: [Rect::default(); 10],
            less: Rect::default(), more: Rect::default(), close: Rect::default(),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, settings: &Settings,
                message: &str, accent: Color, dim: Color) {
        self.hits.fill(Rect::default());
        self.less = Rect::default();
        self.more = Rect::default();
        let height = if self.card {
            (area.height.saturating_sub(6) / 2).max(1)
        } else {
            area.height.saturating_sub(8).max(1)
        }.min(LABELS.len() as u16) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        let shown = height.min(LABELS.len() - self.offset);
        let inset = if self.card { 2 } else { 0 };
        let step = if self.card { 2 } else { 1 };
        if self.card {
            frame.render_widget(Clear, area);
            let mut title = String::from("Projects Settings");
            if self.offset > 0 { title.push_str(" ↑"); }
            if self.offset + shown < LABELS.len() { title.push_str(" ↓"); }
            frame.render_widget(Block::default().borders(Borders::ALL).title(title), area);
        }
        self.close = Rect::new(
            area.right().saturating_sub(if self.card { 9 } else { 7 }),
            area.y, area.width.min(if self.card { 8 } else { 7 }), 1,
        );
        frame.render_widget(Paragraph::new("[Close]").style(Style::default().fg(accent)), self.close);
        let mut rows = values(settings);
        if self.card { rows[0] = format!("[←] {} [→]", settings.width); }
        for row in self.offset..self.offset + shown {
            let rect = Rect::new(area.x + inset, area.y + 2 + (row - self.offset) as u16 * step,
                                 area.width.saturating_sub(inset * 2), 1);
            if rect.y >= area.bottom() { break; }
            self.hits[row] = rect;
            let style = if row == self.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else { Style::default() };
            if !self.card { frame.render_widget(Paragraph::new("").style(style), rect); }
            let value_width = (rows[row].chars().count() as u16).min(rect.width);
            let label_width = rect.width.saturating_sub(value_width + 1);
            frame.render_widget(
                Paragraph::new(format!("{} {}", if row == self.selected { ">" } else { " " }, LABELS[row])).style(style),
                Rect::new(rect.x, rect.y, label_width, 1),
            );
            let value = Rect::new(rect.right() - value_width, rect.y, value_width, 1);
            frame.render_widget(Paragraph::new(rows[row].as_str()).style(style), value);
            if row == 0 && value_width == rows[row].chars().count() as u16 {
                self.less = Rect::new(value.x, value.y, 3, 1);
                self.more = Rect::new(value.right() - 3, value.y, 3, 1);
            }
        }
        let y = area.y + 3 + shown as u16 * step;
        if y < area.bottom().saturating_sub(2) {
            frame.render_widget(
                Paragraph::new(if message.is_empty() { HELP[self.selected] } else { message })
                    .wrap(Wrap { trim: true }).style(Style::default().fg(dim)),
                Rect::new(area.x + inset, y, area.width.saturating_sub(inset * 2),
                          area.bottom().saturating_sub(y + if self.card { 1 } else { 2 })),
            );
        }
        if area.height > 1 {
            frame.render_widget(
                Paragraph::new(if self.card {
                    "↑↓/jk move · ←→/hl change · click · esc closes"
                } else { "Click to edit | arrows/jk/hl | Esc/s/q close" }).style(Style::default().fg(dim)),
                Rect::new(area.x + inset, area.bottom() - if self.card { 2 } else { 1 },
                          area.width.saturating_sub(inset * 2), 1),
            );
        }
    }

    pub fn input(&mut self, event: &Event) -> Outcome {
        let previous = self.selected;
        let mut edit = None;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 's') => return Outcome::Close,
                KeyCode::Char('c') if self.card || key.modifiers.contains(KeyModifiers::CONTROL) => return Outcome::Close,
                KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(9),
                KeyCode::Left | KeyCode::Char('h') => edit = Some(false),
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => edit = Some(true),
                KeyCode::Char(' ') if !self.card => edit = Some(true),
                _ => {}
            },
            Event::Mouse(mouse) => {
                let point = Position::new(mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.close.contains(point) { return Outcome::Close; }
                        if self.less.contains(point) || self.more.contains(point) {
                            if !self.card { self.selected = 0; }
                            return Outcome::Edit { row: 0, forward: self.more.contains(point) };
                        }
                        if let Some(row) = self.hits.iter().position(|hit| hit.contains(point)) {
                            self.selected = row;
                            if self.card || row != 0 { edit = Some(true); }
                        }
                    }
                    MouseEventKind::Moved if self.card => {
                        if !self.close.contains(point) && !self.less.contains(point) && !self.more.contains(point) {
                            if let Some(row) = self.hits.iter().position(|hit| hit.contains(point)) {
                                self.selected = row;
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(1),
                    MouseEventKind::ScrollDown => self.selected = (self.selected + 1).min(9),
                    _ => {}
                }
            }
            _ => {}
        }
        if let Some(forward) = edit {
            Outcome::Edit { row: self.selected, forward }
        } else if previous != self.selected {
            Outcome::Selected
        } else { Outcome::None }
    }
}

pub fn run() -> io::Result<()> {
    let mut settings = config::load()?;
    enable_raw_mode()?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableFocusChange)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = SettingsUi::new(false);
    let mut dirty = true;
    let mut message = String::new();
    loop {
        if dirty {
            terminal.draw(|frame| ui.draw(frame, frame.area(), &settings, &message, Color::Reset, Color::Reset))?;
            dirty = false;
        }
        if !event::poll(Duration::from_secs(1))? {
            match config::load() {
                Ok(next) if next != settings => { settings = next; dirty = true; }
                Err(error) => {
                    let text = error.to_string();
                    dirty |= text != message;
                    message = text;
                }
                _ => {}
            }
            continue;
        }
        let event = event::read()?;
        dirty |= matches!(event, Event::Resize(_, _));
        let previous = ui.selected;
        let outcome = ui.input(&event);
        if ui.selected != previous {
            message.clear();
            dirty = true;
        }
        match outcome {
            Outcome::Close => break,
            Outcome::Edit { row, forward } => {
                let before = settings.clone();
                match apply(&mut settings, row, forward) {
                    Ok(msg) => {
                        if settings != before || !msg.is_empty() {
                            message = msg;
                            dirty = true;
                        }
                    }
                    Err(error) => { message = error.to_string(); dirty = true; }
                }
            }
            Outcome::None | Outcome::Selected => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_controls_edit_their_rows_and_scrolling_exposes_later_settings() {
        use crossterm::event::{KeyEvent, MouseEvent};
        for card in [false, true] {
            let mut ui = SettingsUi::new(card);
            let mut settings = Settings::default();
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(58, 18)).unwrap();
            terminal.draw(|frame| ui.draw(frame, frame.area(), &settings, "", Color::Cyan, Color::DarkGray)).unwrap();
            let click = |x, y| Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y,
                modifiers: KeyModifiers::NONE,
            });
            let position = |terminal: &Terminal<ratatui::backend::TestBackend>, needle: &str| {
                (0..18).find_map(|y| {
                    let line: String = (0..58).map(|x| terminal.backend().buffer()[(x, y)].symbol()).collect();
                    line.find(needle).map(|byte| (line[..byte].chars().count() as u16, y))
                }).unwrap()
            };
            for (needle, width) in [(if card { "[←]" } else { "[-]" }, 28),
                                    (if card { "[→]" } else { "[+]" }, 30)] {
                let (x, y) = position(&terminal, needle);
                let Outcome::Edit { row, forward } = ui.input(&click(x, y)) else { panic!("width control did not edit") };
                change(&mut settings, row, forward);
                assert_eq!(settings.width, width);
            }
            let (x, y) = position(&terminal, "Dock side");
            let Outcome::Edit { row, forward } = ui.input(&click(x, y)) else { panic!("row did not edit") };
            change(&mut settings, row, forward);
            assert!(!settings.dock_right);
            for _ in 0..8 {
                ui.input(&Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
            }
            terminal.draw(|frame| ui.draw(frame, frame.area(), &settings, "", Color::Cyan, Color::DarkGray)).unwrap();
            let (x, y) = position(&terminal, "[Install]");
            assert_eq!(ui.input(&click(x, y)), Outcome::Edit { row: 9, forward: true });
            let (x, y) = position(&terminal, "[Close]");
            assert_eq!(ui.input(&click(x, y)), Outcome::Close);
        }
    }

    #[test]
    fn dock_side_row_toggles_without_shifting_width() {
        let mut settings = Settings::default();
        assert!(settings.dock_right);
        assert_eq!(values(&settings)[1], "Right");
        change(&mut settings, 1, true);
        assert!(!settings.dock_right);
        assert_eq!(values(&settings)[1], "Left");
        change(&mut settings, 1, false);
        assert!(settings.dock_right);
        change(&mut settings, 0, true);
        assert_eq!(settings.width, 32);
    }

    #[test]
    fn apply_unchanged_settings_returns_saved_without_side_effects() {
        let mut settings = Settings::default();
        settings.width = 24;
        let result = apply(&mut settings, 0, false).unwrap();
        assert_eq!(result, "");
        assert_eq!(settings.width, 24);
    }
}

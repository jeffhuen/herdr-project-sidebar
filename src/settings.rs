use std::io;
use std::time::Duration;

use crate::config::{self, IconMode, Settings};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Paragraph, Wrap},
    Terminal,
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
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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

pub fn change(settings: &mut Settings, row: usize, forward: bool) {
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

pub fn run() -> io::Result<()> {
    let mut settings = config::load()?;
    enable_raw_mode()?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut selected = 0usize;
    let mut offset = 0usize;
    let mut dirty = true;
    let mut message = String::new();
    let mut hits = [Rect::default(); 10];
    let mut less = Rect::default();
    let mut more = Rect::default();
    let mut close = Rect::default();
    loop {
        if dirty {
            terminal.draw(|frame| {
                let area = frame.area();
                hits.fill(Rect::default());
                less = Rect::default();
                more = Rect::default();
                close = Rect::new(area.right().saturating_sub(7), area.y, area.width.min(7), 1);
                frame.render_widget(Paragraph::new("[Close]"), close);
                let height = area
                    .height
                    .saturating_sub(8)
                    .max(1)
                    .min(LABELS.len() as u16) as usize;
                if selected < offset {
                    offset = selected;
                }
                if selected >= offset + height {
                    offset = selected + 1 - height;
                }
                let rows = values(&settings);
                for row in offset..(offset + height).min(LABELS.len()) {
                    let rect = Rect::new(area.x, area.y + 2 + (row - offset) as u16, area.width, 1);
                    if rect.y >= area.bottom() {
                        break;
                    }
                    hits[row] = rect;
                    let style = if row == selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    frame.render_widget(Paragraph::new("").style(style), rect);
                    let value_width = (rows[row].len() as u16).min(rect.width);
                    let label_width = rect.width.saturating_sub(value_width + 1);
                    frame.render_widget(
                        Paragraph::new(format!(
                            "{} {}",
                            if row == selected { ">" } else { " " },
                            LABELS[row]
                        ))
                        .style(style),
                        Rect::new(rect.x, rect.y, label_width, 1),
                    );
                    let value = Rect::new(rect.right() - value_width, rect.y, value_width, 1);
                    frame.render_widget(Paragraph::new(rows[row].as_str()).style(style), value);
                    if row == 0 && value_width == rows[row].len() as u16 {
                        less = Rect::new(value.x, value.y, 3, 1);
                        more = Rect::new(value.right() - 3, value.y, 3, 1);
                    }
                }
                let y = area.y + 3 + height as u16;
                if y < area.bottom().saturating_sub(2) {
                    let text = if message.is_empty() {
                        HELP[selected]
                    } else {
                        &message
                    };
                    frame.render_widget(
                        Paragraph::new(text).wrap(Wrap { trim: true }),
                        Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y + 2)),
                    );
                }
                if area.height > 1 {
                    frame.render_widget(
                        Paragraph::new("Click to edit | arrows/jk/hl | Esc/s/q close"),
                        Rect::new(area.x, area.bottom() - 1, area.width, 1),
                    );
                }
            })?;
            dirty = false;
        }
        if !event::poll(Duration::from_secs(1))? {
            match config::load() {
                Ok(next) if next != settings => {
                    settings = next;
                    dirty = true;
                }
                Err(error) => {
                    let text = error.to_string();
                    dirty |= text != message;
                    message = text;
                }
                _ => {}
            }
            continue;
        }
        let mut edit = None;
        let previous = selected;
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 's') => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(9),
                KeyCode::Left | KeyCode::Char('h') => edit = Some(false),
                KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::Enter => edit = Some(true),
                _ => {}
            },
            Event::Mouse(mouse) => {
                let point = Position::new(mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) if close.contains(point) => break,
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(row) = hits.iter().position(|hit| hit.contains(point)) {
                            selected = row;
                            if row != 0 {
                                edit = Some(true);
                            } else if less.contains(point) {
                                edit = Some(false);
                            } else if more.contains(point) {
                                edit = Some(true);
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => selected = selected.saturating_sub(1),
                    MouseEventKind::ScrollDown => selected = (selected + 1).min(9),
                    _ => {}
                }
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
        if selected != previous {
            message.clear();
            dirty = true;
        }
        if let Some(forward) = edit {
            let result = if selected == 9 {
                crate::icons::install().map(|_| "Installed. Map U+E1A0-U+E1B0 to Herdr Agent Icons Compact in your terminal, then choose Font.".to_owned())
            } else {
                config::update(|current| change(current, selected, forward)).and_then(|saved| {
                    settings = saved;
                    crate::reload().map_err(|error| {
                        io::Error::other(format!("Saved; Herdr reload failed: {error}"))
                    })?;
                    crate::native::start()?;
                    Ok(if selected == 2 {
                        "Saved. An open dock still follows tabs; manually closed tabs stay snoozed."
                    } else {
                        "Saved."
                    }
                    .to_owned())
                })
            };
            message = result.unwrap_or_else(|error| error.to_string());
            dirty = true;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Width still lives at row 0 after the insert.
        change(&mut settings, 0, true);
        assert_eq!(settings.width, 32);
        assert_eq!(LABELS.len(), 10);
        assert_eq!(HELP.len(), 10);
        assert_eq!(values(&settings).len(), 10);
    }
}

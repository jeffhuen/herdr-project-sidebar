use super::{Menus, Theme, POPUP_BG};
use crate::ipc::Session;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use serde_json::json;

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

impl Menus {
    pub(super) fn draw_dialog(frame: &mut Frame, theme: &Theme, dialog: &Dialog) -> [Rect; 4] {
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

        frame.render_widget(Clear, rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::styled(
                    format!(" {title} "),
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(POPUP_BG))
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
            Paragraph::new(content_text).style(Style::default().fg(theme.idle_fresh).bg(POPUP_BG)),
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
        let (leading, gap, padding, clear_label, cancel_label) = if inner_w >= 34 {
            ("   ", "   ", " ", "^c clear", "esc cancel")
        } else if inner_w >= 26 {
            (" ", "  ", " ", "^c", "esc cancel")
        } else {
            ("", " ", "", "^c", "esc")
        };
        let gap_style = Style::default().bg(POPUP_BG);
        let hint_style = gap_style.fg(theme.dim);
        let mut controls = vec![
            Span::styled(leading, gap_style),
            Span::styled(format!("{padding}{btn_label}{padding}"), btn_style),
            Span::styled(gap, gap_style),
        ];
        if input.is_some() {
            controls.push(Span::styled(clear_label, hint_style));
            controls.push(Span::styled(gap, gap_style));
        }
        controls.push(Span::styled(cancel_label, hint_style));

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
            Paragraph::new(Line::from(controls)).style(Style::default().bg(POPUP_BG)),
            row,
        );
        [rect, confirm, clear, cancel]
    }

    pub(super) fn input_dialog(&mut self, event: &Event, session: Option<&Session>) -> Option<String> {
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
            Event::Paste(text) => {
                if let Some(input) = dialog.input_mut() {
                    input.extend(text.chars().filter(|c| !c.is_control()));
                    self.revision = self.revision.wrapping_add(1);
                }
            }
            _ => {}
        }
        None
    }

    fn cancel_dialog(&mut self) -> Option<String> {
        self.close();
        Some(String::new())
    }

    /// Enter and the confirm button share one path.
    fn submit_dialog(&mut self, session: Option<&Session>) -> Option<String> {
        let d = self.dialog.take()?;
        self.revision = self.revision.wrapping_add(1);
        let Some(session) = session else {
            return Some("Herdr session unavailable".into());
        };
        let empty_message = match &d {
            Dialog::Rename { input, .. } if input.trim().is_empty() => "Workspace name cannot be empty",
            Dialog::NewWorktree { input, .. } if input.trim().is_empty() => "Branch name cannot be empty",
            Dialog::OpenWorktree { input, .. } if input.trim().is_empty() => "Target cannot be empty",
            _ => "",
        };
        if !empty_message.is_empty() {
            return Some(empty_message.into());
        }
        let (method, params, success, value, failure) = match &d {
            Dialog::Rename { workspace_id, input } => {
                let label = input.trim();
                ("workspace.rename", json!({ "workspace_id": workspace_id, "label": label }),
                    "Renamed workspace to", label, "Rename failed")
            }
            Dialog::ConfirmCloseWorkspace { workspace_id, name } => (
                "workspace.close", json!({ "workspace_id": workspace_id }),
                "Closed workspace", name.as_str(), "Close failed",
            ),
            Dialog::ConfirmDeleteWorktree { workspace_id, name } => (
                "worktree.remove", json!({ "workspace_id": workspace_id, "force": false }),
                "Deleted worktree", name.as_str(), "Delete failed",
            ),
            Dialog::NewWorktree { workspace_id, input } => {
                let branch = input.trim();
                ("worktree.create", json!({ "workspace_id": workspace_id, "branch": branch }),
                    "Created worktree", branch, "Create worktree failed")
            }
            Dialog::OpenWorktree { workspace_id, input } => {
                let target = input.trim();
                ("worktree.open", json!({ "workspace_id": workspace_id, "branch": target }),
                    "Opened worktree", target, "Open worktree failed")
            }
        };
        Some(match session.call(method, params) {
            Ok(_) => format!("{success} {value}"),
            Err(e) => format!("{failure}: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use std::io;

    #[test]
    fn rename_dialog_workflow() {
        let mut projects = crate::dock::stub();
        projects[0].worktrees[0].workspace_id = "w1".into();
        let rows = crate::dock::visible(&projects, "", false, crate::dock::View::Grouped);

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

        let theme = crate::dock::load_theme();
        let projects = crate::dock::stub();
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
}

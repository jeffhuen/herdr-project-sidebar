use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;

use ratatui::backend::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use super::model::{counts, Project, Row, State, View};
use super::theme::{mute, state_color, state_glyph, vendor_color, worktree_prefix, Theme};
use super::Dock;

impl Dock {
    pub(super) fn draw(
        &mut self,
        term: &mut Terminal<CrosstermBackend<io::Stdout>>,
        rows: &[Row],
        window: Vec<Option<usize>>,
        activity_error: Option<&str>,
    ) -> io::Result<()> {
        let (agents, working_n, blocked, unread) = counts(&self.projects);
        // Include offscreen tab leaders when indenting split panes.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut first_rows: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
        for r in rows {
            if let Row::Agent(pi, wi, ai) = *r {
                if seen.insert(self.projects[pi].worktrees[wi].agents[ai].tab_id.as_str()) {
                    first_rows.insert((pi, wi, ai));
                }
            }
        }
        let vis = window;
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
                    let p = &self.projects[pi];
                    let mark = if p.collapsed { "▸" } else { "▾" };
                    let pin = if p.pinned {
                        (if self.font { '\u{f08d}' } else { '*' }).to_string()
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
                                    .fg(self.theme.working)
                                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                            } else {
                                Style::default().add_modifier(Modifier::BOLD)
                            },
                        ),
                    ])
                }
                Row::Worktree(pi, wi) => {
                    let w = &self.projects[pi].worktrees[wi];
                    let mut spans = Vec::from(worktree_prefix(w, self.font));
                    spans[1].style = Style::default().fg(self.theme.dim);
                    spans.push(Span::raw(format!("{} ", w.name)));
                    Line::from(spans)
                }
                Row::Agent(pi, wi, ai) => {
                    let a = &self.projects[pi].worktrees[wi].agents[ai];
                    let (glyph, _) = state_glyph(a.state, self.tick, self.font);
                    let v_color = vendor_color(&a.vendor);
                    let icon_mode = self.settings_obj.icons;
                    let label = match crate::icons::logo(&a.vendor, icon_mode) {
                        Some(logo) if a.label.is_empty() => logo.to_string(),
                        Some(logo) => format!("{logo} {}", a.label),
                        None => a.label.clone(),
                    };
                    let first_in_tab = first_rows.contains(&(pi, wi, ai));
                    let indent = if self.view == View::Recent {
                        format!("  [{}] ", self.projects[pi].name)
                    } else if first_in_tab {
                        "    ".to_string()
                    } else {
                        "      └─ ".to_string()
                    };
                    let title_style = if a.state == State::Working {
                        Style::default().fg(v_color).add_modifier(Modifier::BOLD)
                    } else if a.state == State::IdleStale {
                        Style::default()
                            .fg(self.theme.idle_stale)
                            .add_modifier(Modifier::DIM)
                    } else {
                        Style::default().fg(state_color(&self.theme, a.state))
                    };

                    let mut spans = vec![
                        Span::raw(indent),
                        Span::styled(
                            glyph.to_string(),
                            Style::default()
                                .fg(state_color(&self.theme, a.state))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(label, Style::default().fg(v_color)),
                        Span::raw(if self.settings_obj.show_title && !a.title.is_empty() { " · " } else { "" }),
                        Span::styled(
                            if self.settings_obj.show_title {
                                a.title.as_str()
                            } else {
                                ""
                            },
                            title_style,
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
            let cursor = if idx == self.selected {
                style = style.bg(self.theme.dim);
                // Lift the dim worktree icon above the selected background.
                if matches!(rows[idx], Row::Worktree(_, _)) {
                    line.spans[1].style = Style::default()
                        .fg(self.theme.working).add_modifier(Modifier::BOLD);
                }
                Span::styled("› ", Style::default().fg(self.theme.working).add_modifier(Modifier::BOLD))
            } else if self.hover == Some(idx) {
                Span::raw("· ")
            } else if matches!(rows[idx], Row::Agent(pi, wi, ai) if self.projects[pi].worktrees[wi].agents[ai].focused) {
                Span::styled("» ", Style::default().fg(self.theme.working).add_modifier(Modifier::BOLD))
            } else {
                Span::raw("  ")
            };
            if matches!(rows[idx], Row::Worktree(_, _)) {
                line.spans[0] = cursor;
            } else {
                line.spans.insert(0, cursor);
            }
            for span in &mut line.spans {
                span.style = span.style.patch(style);
            }
            lines.push(line);
        }
        self.visual_rows = vis;
        term.draw(|f| {
            let area = f.area();
            if area.height == 0 || area.width == 0 {
                return;
            }
            let bg_dim = if self.settings_dialog { Modifier::DIM } else { Modifier::empty() };
            let dim_lines: Vec<Line> = lines.iter().map(|l| {
                let mut nl = l.clone();
                for s in &mut nl.spans { s.style = s.style.add_modifier(bg_dim); }
                nl
            }).collect();
            let content_h = area.height.saturating_sub(2);
            let content_rect = ratatui::layout::Rect::new(area.x, area.y, area.width, content_h);
            f.render_widget(Paragraph::new(dim_lines), content_rect);

            let summary_y = area.bottom().saturating_sub(2);
            let buttons_y = area.bottom().saturating_sub(1);

            let dim = Style::default().fg(self.theme.dim);
            let (bottom, footer_style) = if let Some(e) = self.sync_error.as_deref() {
                (format!("herdr unreachable: {e} (retrying)"), dim)
            } else if self.sync.is_stale() {
                ("refreshing Herdr snapshot".into(), dim)
            } else if self.filtering {
                (format!("filter: {}  (enter/esc done)", self.query), dim)
            } else if !self.status_line.is_empty() {
                (self.status_line.clone(), dim)
            } else if let Some(error) = &activity_error {
                (format!("activity save failed: {error}"), dim)
            } else if self.font_notice {
                ("Nerd Font not found - ASCII icons - F font options".into(), dim)
            } else {
                (
                    format!(" {agents} agents · {working_n} working · {blocked} blocked · {unread} unread"),
                    Style::default().fg(mute(self.theme.working)),
                )
            };
            let summary_rect = ratatui::layout::Rect::new(area.x, summary_y, area.width, 1);
            f.render_widget(
                Paragraph::new(bottom).style(footer_style),
                summary_rect,
            );

            let new_text = " [+ New] ";
            let new_w = (new_text.chars().count() as u16).min(area.width / 2);
            self.new_btn = ratatui::layout::Rect::new(area.x, buttons_y, new_w, 1);
            f.render_widget(
                Paragraph::new(new_text).style(Style::default().fg(mute(self.theme.accent))),
                self.new_btn,
            );

            let btn_text = " [⚙ Settings] ";
            let btn_w = (btn_text.chars().count() as u16).min(area.width.saturating_sub(new_w));
            self.settings_btn = ratatui::layout::Rect::new(area.right().saturating_sub(btn_w), buttons_y, btn_w, 1);
            f.render_widget(
                Paragraph::new(btn_text).style(Style::default().fg(mute(self.theme.accent))),
                self.settings_btn,
            );

            if self.settings_dialog {
                let card_w = area.width.saturating_sub(2).clamp(24, 58).min(area.width);
                let card_h = area.height.saturating_sub(4).clamp(12, 28).min(content_h);
                let card_x = area.right().saturating_sub(card_w);
                let card_y = buttons_y.saturating_sub(card_h).max(area.y);
                let card_rect = ratatui::layout::Rect::new(card_x, card_y, card_w, card_h);
                self.settings_ui.draw(f, card_rect, &self.settings_obj, &self.status_line, self.theme.working, self.theme.dim);
            }

            if self.font_dialog {
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
            if self.copy_menu.is_open() {
                let menu_anchor = self.visual_rows.iter().position(|row| *row == Some(self.selected));
                self.copy_menu.draw(f, &self.theme, self.sync_error.is_none() && !self.sync.is_stale(), menu_anchor.map(|y| y as u16));
            }
        })?;
        Ok(())
    }
}

/// Identity of the rendered inputs. Equal signatures skip terminal drawing.
pub(super) struct SigInput<'a> {
    pub(super) projects: &'a [Project],
    pub(super) rows: &'a [Row],
    pub(super) selected: usize,
    pub(super) offset: usize,
    pub(super) height: usize,
    pub(super) hover: Option<usize>,
    pub(super) step: usize,
    pub(super) query: &'a str,
    pub(super) compact: bool,
    pub(super) view: View,
    pub(super) status: &'a str,
    pub(super) filtering: bool,
    pub(super) theme: &'a Theme,
    pub(super) font_dialog: bool,
    pub(super) font: bool,
    pub(super) settings_dialog: bool,
    pub(super) settings_row: usize,
    pub(super) settings_obj: &'a crate::config::Settings,
}

pub(super) fn signature(input: &SigInput, sig: &mut String) {
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
    sig.clear();
    write!(
        sig,
        "{selected}:{offset}:{height}:{hover:?}:{step}:{query}:{compact}:{}:{status}:{filtering}:{font_dialog}:{font}:{:?}:{:?}:{:?}:{:?}:{:?}:{settings_dialog}:{settings_row}:{settings_obj:?}:",
        view as u8, theme.working, theme.blocked, theme.done, theme.idle, theme.accent,
    ).unwrap();
    for (i, row) in rows.iter().enumerate() {
        match *row {
            Row::Project(pi) => {
                let p = &projects[pi];
                write!(
                    sig,
                    "{i}:P:{}:{}:{}:{}:{};",
                    p.name, p.branch, p.collapsed, p.pinned, p.focused
                )
                .unwrap();
            }
            Row::Worktree(pi, wi) => {
                let w = &projects[pi].worktrees[wi];
                write!(
                    sig,
                    "{i}:W:{}:{}:{}:{}:{};",
                    w.name,
                    w.branch,
                    w.collapsed,
                    w.agents.is_empty(),
                    w.focused
                )
                .unwrap();
            }
            Row::Agent(pi, wi, ai) => {
                let a = &projects[pi].worktrees[wi].agents[ai];
                write!(
                    sig,
                    "{i}:A:{}:{}:{}:{}:{}:{};",
                    a.vendor, a.label, a.title, a.state as u8, a.tab_id, a.focused
                )
                .unwrap();
            }
        }
    }
}

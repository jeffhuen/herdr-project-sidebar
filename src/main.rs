//! herdr-project-sidebar: Orca-style project -> worktree -> agent tree.
//! Static stub data. Live Herdr sync (events.subscribe + agent.list /
//! workspace.list snapshot) plugs into `snapshot()` without touching render.
//!
//! Perf notes: deadline-driven tick (150ms only while a visible agent works,
//! 1s idle sleep), signature-skipped draws, windowed render (only the visible
//! slice builds widgets), input/mouse interrupt the wait immediately.

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

// ponytail: single-file skeleton, split into snapshot/tree/ui modules when
// the Herdr socket sync lands.

const SPINNER: [&str; 8] = ["⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽", "⣾"];
const TICK: Duration = Duration::from_millis(150);
const IDLE_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq)]
enum State {
    Working,
    Done,
    Blocked,
    Idle,
}

struct Agent {
    vendor: &'static str,
    title: &'static str,
    state: State,
}

struct Worktree {
    name: &'static str,
    branch: &'static str,
    collapsed: bool,
    agents: Vec<Agent>,
}

struct Project {
    name: &'static str,
    branch: &'static str,
    collapsed: bool,
    worktrees: Vec<Worktree>,
}

#[derive(Clone, Copy)]
enum Row {
    Project(usize),
    Worktree(usize, usize),
    Agent(usize, usize, usize),
}

fn stub() -> Vec<Project> {
    vec![
        Project {
            name: "muse-bridge",
            branch: "main",
            collapsed: false,
            worktrees: vec![Worktree {
                name: "muse-bridge",
                branch: "main",
                collapsed: false,
                agents: vec![
                    Agent { vendor: "pi", title: "Implement OAuth scopes", state: State::Working },
                    Agent { vendor: "codex", title: "Wire retry budget", state: State::Done },
                    Agent { vendor: "opencode", title: "Migrate invoices table", state: State::Idle },
                ],
            }],
        },
        Project {
            name: "sold-by-robots",
            branch: "feature/mc-13200",
            collapsed: false,
            worktrees: vec![Worktree {
                name: "sbr-9u4v.35",
                branch: "feature/mc-13200",
                collapsed: false,
                agents: vec![Agent {
                    vendor: "claude",
                    title: "Which env file should I edit?",
                    state: State::Blocked,
                }],
            }],
        },
    ]
}

// Future: replace stub() with a socket snapshot. Keep the same shape so the
// tree builder below does not change.
fn snapshot() -> Vec<Project> {
    stub()
}

fn visible(projects: &[Project]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (pi, p) in projects.iter().enumerate() {
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

fn counts(projects: &[Project]) -> (usize, usize, usize) {
    let mut agents = 0;
    let mut working = 0;
    let mut blocked = 0;
    for p in projects {
        for w in &p.worktrees {
            for a in &w.agents {
                agents += 1;
                match a.state {
                    State::Working => working += 1,
                    State::Blocked => blocked += 1,
                    _ => {}
                }
            }
        }
    }
    (agents, working, blocked)
}

fn state_glyph(state: State, tick: usize) -> (&'static str, Color) {
    match state {
        State::Working => (SPINNER[tick % SPINNER.len()], Color::Yellow),
        State::Done => ("✓", Color::Green),
        State::Blocked => ("?", Color::Red),
        State::Idle => ("·", Color::DarkGray),
    }
}

fn main() -> io::Result<()> {
    let mut projects = snapshot();
    let mut selected = 0usize;
    let mut offset = 0usize;
    let mut hover: Option<usize> = None;
    let mut tick = 0usize;
    let mut last_drawn = String::new();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    loop {
        let rows = visible(&projects);
        if selected >= rows.len() {
            selected = rows.len().saturating_sub(1);
        }
        let height = term.size()?.height.saturating_sub(2) as usize;
        offset = ensure_visible(selected, offset, height);
        let working = rows.iter().any(|r| match *r {
            Row::Agent(pi, wi, ai) => {
                projects[pi].worktrees[wi].agents[ai].state == State::Working
            }
            _ => false,
        });
        let step = if working { tick % SPINNER.len() } else { 0 };
        let sig = signature(&projects, &rows, selected, offset, height, hover, step);
        if sig != last_drawn {
            let (agents, working_n, blocked) = counts(&projects);
            let end = (offset + height).min(rows.len());
            let mut lines = Vec::with_capacity(end - offset);
            for (i, row) in rows[offset..end].iter().enumerate() {
                let idx = offset + i;
                let mut line = match *row {
                    Row::Project(pi) => {
                        let p = &projects[pi];
                        let mark = if p.collapsed { "▸" } else { "▾" };
                        Line::from(vec![
                            Span::raw(format!("{mark} {} ", p.name)),
                            Span::styled(
                                p.branch.to_string(),
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::DIM),
                            ),
                        ])
                    }
                    Row::Worktree(pi, wi) => {
                        let w = &projects[pi].worktrees[wi];
                        let mark = if w.collapsed { "▸" } else { "▾" };
                        Line::from(vec![
                            Span::raw("  "),
                            Span::raw(format!("{mark} {} ", w.name)),
                            Span::styled(
                                w.branch.to_string(),
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::DIM),
                            ),
                        ])
                    }
                    Row::Agent(pi, wi, ai) => {
                        let a = &projects[pi].worktrees[wi].agents[ai];
                        let (glyph, color) = state_glyph(a.state, tick);
                        Line::from(vec![
                            Span::raw("    "),
                            Span::styled(
                                glyph.to_string(),
                                Style::default().fg(color).add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(" [{}] {}", a.vendor, a.title)),
                        ])
                    }
                };
                let mut style = Style::default();
                if idx == selected {
                    style = style.bg(Color::DarkGray);
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
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Projects ({agents} agents · {working_n} working · {blocked} blocked)"))
                    .title_bottom("click select · enter fold · ✓ done ? blocked · idle · q quit");
                f.render_widget(Paragraph::new(lines).block(block), f.area());
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
            Event::Key(key) => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(rows.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1)
                }
                KeyCode::Enter | KeyCode::Tab | KeyCode::Char(' ') => {
                    toggle(&mut projects, &rows, selected)
                }
                _ => {}
            },
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(_) => {
                    if let Some(idx) = click_index(offset, m.row) {
                        if idx < rows.len() {
                            if idx == selected {
                                toggle(&mut projects, &rows, idx);
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
                    selected = (selected + 3).min(rows.len().saturating_sub(1));
                }
                MouseEventKind::ScrollUp => {
                    selected = selected.saturating_sub(3);
                }
                _ => {}
            },
            _ => {}
        }
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

fn toggle(projects: &mut [Project], rows: &[Row], idx: usize) {
    match rows[idx] {
        Row::Project(pi) => projects[pi].collapsed = !projects[pi].collapsed,
        Row::Worktree(pi, wi) => {
            projects[pi].worktrees[wi].collapsed = !projects[pi].worktrees[wi].collapsed
        }
        Row::Agent(..) => {}
    }
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
) -> String {
    let mut sig = format!("{selected}:{offset}:{height}:{hover:?}:{step}:");
    for (i, row) in rows.iter().enumerate() {
        match *row {
            Row::Project(pi) => {
                let p = &projects[pi];
                sig.push_str(&format!("{i}:P:{}:{}:{};", p.name, p.branch, p.collapsed));
            }
            Row::Worktree(pi, wi) => {
                let w = &projects[pi].worktrees[wi];
                sig.push_str(&format!("{i}:W:{}:{}:{};", w.name, w.branch, w.collapsed));
            }
            Row::Agent(pi, wi, ai) => {
                let a = &projects[pi].worktrees[wi].agents[ai];
                let s = match a.state {
                    State::Working => 'w',
                    State::Done => 'd',
                    State::Blocked => 'b',
                    State::Idle => 'i',
                };
                sig.push_str(&format!("{i}:A:{}:{}:{s};", a.vendor, a.title));
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
        let full = visible(&projects).len();
        projects[0].collapsed = true;
        let folded = visible(&projects);
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
}

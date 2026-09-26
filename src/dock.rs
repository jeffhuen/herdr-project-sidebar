//! herdr-project-sidebar: Collapsible project -> worktree -> agent tree.
//! Change-driven socket snapshots feed the custom tree. Failed refreshes keep
//! the last view but disable native actions until synchronization recovers.
//! Rendering is signature-skipped and windowed, with animation capped at 4fps.
//!
//! Theming: colors come from Herdr's own `config.toml` (`theme.custom` +
//! `ui.sidebar.agents` row rules) with built-in defaults when absent. Nothing
//! is written back; light/dark follows whatever theme is configured.


use std::fmt::Write as _;
use std::io;
use std::time::Duration;

use crate::activity::ActivityStore;
#[cfg(test)]
use crate::activity::now_unix_ms;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

mod input;
mod menu;
mod model;
mod state;
mod theme;
mod view;
#[cfg(test)]
mod fixture;

use input::{activate_target, reset_transient, PendingActivation};
use model::{
    ensure_visible, install_snapshot, project_hue, restore_selection, row_index, row_key,
    visible, window_for_selected, AgentSession, Project, Row, View, NO_SELECTION,
};
#[cfg(test)]
use model::{snapshot, Worktree};
use state::{load_state, save_state, state_path, Memory};
use theme::{font_notice_due, font_ok, load_theme, state_glyph, use_font, Theme};
use view::{signature, SigInput};
#[cfg(test)]
use fixture::stub;

const TICK: Duration = Duration::from_millis(300);
const IDLE_POLL: Duration = Duration::from_millis(300);
const STATUS_TTL: Duration = Duration::from_secs(4);

const PANE_TITLE: &str = "Projects";

struct Dock {
    state_path: std::path::PathBuf,
    mem: Memory,
    font_detected: bool,
    font: bool,
    font_notice: bool,
    font_dialog: bool,
    copy_menu: menu::Menus,
    visual_rows: Vec<Option<usize>>,
    settings_dialog: bool,
    settings_ui: crate::settings::SettingsUi,
    settings_obj: crate::config::Settings,
    new_btn: ratatui::layout::Rect,
    settings_btn: ratatui::layout::Rect,
    theme: Theme,
    sync: crate::ipc::SnapshotSync,
    projects: Vec<Project>,
    session: Option<crate::ipc::Session>,
    sync_error: Option<String>,
    last_config: std::time::Instant,
    last_tick: std::time::Instant,
    selected: usize,
    browsing: bool,
    pressed: Option<(u8, String)>,
    pending_activation: Option<PendingActivation>,
    offset: usize,
    tick: usize,
    hover: Option<usize>,
    last_drawn: String,
    sig: String,
    query: String,
    filtering: bool,
    compact: bool,
    view: View,
    status_line: String,
    status_seen: (String, std::time::Instant),
}

impl Dock {
    fn new(state_path: std::path::PathBuf) -> Self {
        let mut mem = Memory {
            activity: ActivityStore::new(crate::config::state_dir().join("dock")),
            ..Memory::default()
        };
        load_state(&mut mem, &state_path);
        let font_detected = font_ok();
        let font = use_font(&mem.font_choice, font_detected);
        let font_notice = font_notice_due(&mem.font_choice, font_detected);
        Self {
            state_path,
            mem,
            font_detected,
            font,
            font_notice,
            font_dialog: false,
            copy_menu: menu::Menus::default(),
            visual_rows: Vec::new(),
            settings_dialog: false,
            settings_ui: crate::settings::SettingsUi::new(true),
            settings_obj: crate::config::load().unwrap_or_default(),
            new_btn: ratatui::layout::Rect::default(),
            settings_btn: ratatui::layout::Rect::default(),
            theme: load_theme(),
            sync: crate::ipc::SnapshotSync::new(),
            projects: Vec::new(),
            session: None,
            sync_error: None,
            last_config: std::time::Instant::now(),
            last_tick: std::time::Instant::now(),
            selected: NO_SELECTION,
            browsing: false,
            pressed: None,
            pending_activation: None,
            offset: 0usize,
            tick: 0usize,
            hover: None,
            last_drawn: String::new(),
            sig: String::new(),
            query: String::new(),
            filtering: false,
            compact: false,
            view: View::Grouped,
            status_line: String::new(),
            status_seen: (String::new(), std::time::Instant::now()),
        }
    }
}

/// Idempotent restoration for normal exit, errors, and panics.
fn restore_term() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, DisableFocusChange, DisableBracketedPaste);
}

struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_term();
    }
}

pub fn run() -> io::Result<()> {
    let state_path = state_path();
    // Herdr otherwise consumes plain right-clicks before they reach the dock.
    if let Ok(pane_id) = std::env::var("HERDR_PANE_ID") {
        crate::ipc::call(
            "pane.input.set",
            serde_json::json!({"pane_id": pane_id, "right_click": "pane"}),
        )?;
    }

    let mut dock = Dock::new(state_path);

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
        EnableFocusChange,
        EnableBracketedPaste,
        SetTitle(PANE_TITLE)
    )?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let _term_guard = TermGuard;
    loop {
        if dock.last_config.elapsed() >= Duration::from_secs(1) {
            if !dock.settings_dialog {
                dock.settings_obj = crate::config::load()?;
            }
            dock.theme = load_theme();
            for project in &mut dock.projects {
                project.icon_color =
                    dock.theme.projects[project_hue(&project.id) % dock.theme.projects.len()];
                project.icon = if dock.font { '' } else { '#' };
            }
            dock.last_config = std::time::Instant::now();
        }
        let next_view = if dock.settings_obj.grouped {
            View::Grouped
        } else {
            View::Recent
        };
        if next_view != dock.view {
            let previous_rows = visible(&dock.projects, &dock.query, dock.compact, dock.view);
            let key = row_key(&dock.projects, &previous_rows, dock.selected)
                .map(|(kind, id)| (kind, id.to_owned()));
            dock.view = next_view;
            let current_rows = visible(&dock.projects, &dock.query, dock.compact, dock.view);
            dock.selected = restore_selection(
                &dock.projects,
                &current_rows,
                key.as_ref().map(|(kind, id)| (*kind, id.as_str())),
                dock.browsing || dock.filtering || dock.settings_dialog || dock.copy_menu.is_open(),
            );
            dock.offset = 0;
            reset_transient(&mut dock.pressed, &mut dock.pending_activation, Some(&mut dock.hover));
            dock.copy_menu.close();
        }
        let refresh = dock.sync.poll().and_then(|snap| {
            let Some(snap) = snap else { return Ok(None) };
            let fresh = install_snapshot(&snap, &mut dock.mem, &dock.theme.projects, dock.font)?;
            Ok(Some((snap.session, fresh)))
        });
        match refresh {
            Ok(Some((fresh_session, fresh))) => {
                if dock.session.as_ref().map(crate::ipc::Session::key) != Some(fresh_session.key()) {
                    dock.copy_menu.close();
                    dock.selected = NO_SELECTION;
                    reset_transient(&mut dock.pressed, &mut dock.pending_activation, None);
                    dock.browsing = false;
                    dock.offset = 0;
                }
                let previous_rows = visible(&dock.projects, &dock.query, dock.compact, dock.view);
                let key = row_key(&dock.projects, &previous_rows, dock.selected)
                    .map(|(kind, id)| (kind, id.to_owned()));
                dock.projects = fresh;
                dock.session = Some(fresh_session);
                dock.copy_menu.refresh(&dock.projects);
                let current_rows = visible(&dock.projects, &dock.query, dock.compact, dock.view);
                dock.selected = restore_selection(
                    &dock.projects,
                    &current_rows,
                    key.as_ref().map(|(kind, id)| (*kind, id.as_str())),
                    dock.browsing
                        || dock.filtering
                        || dock.settings_dialog
                        || dock.copy_menu.is_open(),
                );
                dock.hover = None;
                if dock.pending_activation.as_ref().is_some_and(|intent| {
                    row_index(&dock.projects, &current_rows, (intent.key.0, &intent.key.1)).is_none()
                }) {
                    dock.pending_activation = None;
                    dock.selected = NO_SELECTION;
                }
                dock.sync_error = None;
            }
            Ok(None) => {}
            Err(error) => {
                dock.sync_error = Some(error.to_string());
                reset_transient(&mut dock.pressed, &mut dock.pending_activation, None);
                dock.copy_menu.close();
                dock.sync.invalidate();
            }
        }
        dock.copy_menu.poll(
            &dock.projects,
            dock.session.as_ref(),
            dock.sync_error.is_none() && !dock.sync.is_stale(),
        );
        let activity_error = dock.mem
            .activity
            .save(false)
            .err()
            .map(|error| error.to_string());
        save_state(&mut dock.mem, false, &dock.state_path);
        let rows = visible(&dock.projects, &dock.query, dock.compact, dock.view);
        if !dock.sync.is_stale() && dock.sync_error.is_none() {
            if let Some(intent) = dock.pending_activation.take() {
                if let Some((idx, target)) = intent.resolve(&dock.projects, &rows) {
                    dock.selected = idx;
                    dock.browsing = false;
                    if let Some(active_session) = &dock.session {
                        if let Err(error) = activate_target(active_session, target) {
                            dock.status_line = error.to_string();
                        }
                        dock.sync.invalidate();
                    }
                } else {
                    dock.selected = NO_SELECTION;
                }
            }
        }
        if dock.selected != NO_SELECTION && dock.selected >= rows.len() {
            dock.selected = rows.len().saturating_sub(1);
        }
        let height = term.size()?.height.saturating_sub(2) as usize;
        // Clamp the window itself: when rows shrink under a high offset the
        // viewport would otherwise anchor on the last row and draw blanks.
        if dock.selected != NO_SELECTION {
            dock.offset = ensure_visible(dock.selected, dock.offset, height);
        }
        dock.offset = dock.offset.min(rows.len().saturating_sub(height));
        let (window, slid) = window_for_selected(&rows, dock.offset, height, dock.selected);
        dock.offset = slid;
        if dock.status_line != dock.status_seen.0 {
            dock.status_seen = (dock.status_line.clone(), std::time::Instant::now());
        } else if !dock.status_line.is_empty() && dock.status_seen.1.elapsed() >= STATUS_TTL {
            dock.status_line.clear();
        }
        let working = window.iter().flatten().any(|&idx| match rows[idx] {
            Row::Agent(pi, wi, ai) => {
                let (g, anim) = state_glyph(dock.projects[pi].worktrees[wi].agents[ai].state, 0, dock.font);
                let _ = g;
                anim
            }
            _ => false,
        });
        // Input must not starve animation or create a zero-timeout busy loop.
        if working && dock.last_tick.elapsed() >= TICK {
            dock.tick = dock.tick.wrapping_add(1);
            dock.last_tick = std::time::Instant::now();
        }
        let step = if working { dock.tick % crate::icons::FRAMES.len() } else { 0 };
        signature(
            &SigInput {
                projects: &dock.projects,
                rows: &rows,
                selected: dock.selected,
                offset: dock.offset,
                height: height,
                hover: dock.hover,
                step: step,
                query: &dock.query,
                compact: dock.compact,
                view: dock.view,
                status: &dock.status_line,
                filtering: dock.filtering,
                theme: &dock.theme,
                font_dialog: dock.font_dialog,
                font: dock.font,
                settings_dialog: dock.settings_dialog,
                settings_row: dock.settings_ui.selected,
                settings_obj: &dock.settings_obj,
            },
            &mut dock.sig,
        );
        write!(
            dock.sig,
            "{:?}:{activity_error:?}:{}:{}",
            dock.sync_error,
            dock.sync.is_stale(),
            dock.copy_menu.revision
        )
        .unwrap();
        if dock.sig != dock.last_drawn {
            dock.draw(&mut term, &rows, window, activity_error.as_deref())?;
            std::mem::swap(&mut dock.last_drawn, &mut dock.sig);
        }

        // Keep the animation deadline independent of event synchronization.
        let wait = if working {
            TICK.saturating_sub(dock.last_tick.elapsed())
        } else {
            IDLE_POLL
        }
        .min(if dock.sync.is_stale() || dock.copy_menu.is_open() {
            crate::ipc::SYNC_CHECK_INTERVAL
        } else {
            IDLE_POLL
        });
        if !event::poll(wait)? {
            continue;
        }
        let input = event::read()?;
        if dock.input(input, &rows, term.backend_mut()).is_break() {
            break;
        }
    }

    save_state(&mut dock.mem, true, &dock.state_path);
    dock.mem.activity.save(true)?;
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste
    )?;
    match dock.session {
        Some(session) => crate::native::dock_command(crate::dock_control::Command::Close, &session),
        None => Ok(()),
    }
}

use ratatui::style::Color;
use ratatui::text::Span;

use super::model::{State, Worktree};

#[derive(Clone)]
pub(super) struct Theme {
    pub(super) working: Color,
    pub(super) monitoring: Color,
    pub(super) blocked: Color,
    pub(super) interrupted: Color,
    pub(super) done: Color,
    pub(super) idle_fresh: Color,
    pub(super) idle: Color,
    pub(super) idle_stale: Color,
    pub(super) unknown: Color,
    pub(super) dim: Color,
    /// Herdr's `[ui] accent`: pane borders, popups, navigation highlights.
    pub(super) accent: Color,
    pub(super) projects: Vec<Color>,
}

/// Footer colors retain their hue with reduced saturation and brightness.
const MUTE_SATURATION: f32 = 0.6;
const MUTE_BRIGHTNESS: f32 = 0.85;

/// Pull toward the color's own luma gray (keeps hue), then darken. Named
/// terminal colors have no RGB to adjust and pass through unchanged.
pub(super) fn mute(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let gray = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    let ch = |c: u8| ((gray + (f32::from(c) - gray) * MUTE_SATURATION) * MUTE_BRIGHTNESS).round() as u8;
    Color::Rgb(ch(r), ch(g), ch(b))
}

fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_matches('"').trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn rule_color(doc: &toml_edit::DocumentMut, state: &str) -> Option<Color> {
    doc.get("ui")?.get("sidebar")?.get("agents")?.get("rows")?.as_array()?
        .iter().filter_map(|row| row.as_array()).flatten()
        .filter_map(|token| token.as_inline_table()?.get("rules")?.as_array()).flatten()
        .filter_map(|rule| rule.as_inline_table())
        .filter(|rule| rule.get("equals").and_then(|v| v.as_str()) == Some(state))
        .find_map(|rule| parse_hex(rule.get("fg")?.as_str()?))
}

fn custom_color(doc: &toml_edit::DocumentMut, key: &str) -> Option<Color> {
    parse_hex(doc.get("theme")?.get("custom")?.get(key)?.as_str()?)
}

pub(super) fn load_theme() -> Theme {
    let fallback = Theme {
        working: Color::Rgb(0xf9, 0xe2, 0xaf),
        monitoring: Color::Rgb(0xf9, 0xe2, 0xaf),
        blocked: Color::Rgb(0xf3, 0x8b, 0xa8),
        interrupted: Color::Rgb(0xf3, 0x8b, 0xa8),
        done: Color::Rgb(0x94, 0xe2, 0xd5),
        idle_fresh: Color::Rgb(0xcd, 0xd6, 0xf4),
        idle: Color::Rgb(0x6c, 0x70, 0x86),
        idle_stale: Color::Rgb(0x6c, 0x70, 0x86),
        unknown: Color::Rgb(0x90, 0x7a, 0xa9),
        dim: Color::DarkGray,
        // Herdr's documented `[ui] accent` default.
        accent: Color::Cyan,
        projects: vec![
            Color::Rgb(0xcb, 0xa6, 0xf7),
            Color::Rgb(0x89, 0xb4, 0xfa),
            Color::Rgb(0xfa, 0xb3, 0x87),
            Color::Rgb(0x89, 0xdc, 0xeb),
        ],
    };
    let text = std::fs::read_to_string(crate::config::herdr_path()).unwrap_or_default();
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return fallback;
    };
    let get = |rule_state: &str, custom_keys: &[&str], fb: Color| {
        rule_color(&doc, rule_state)
            .or_else(|| custom_keys.iter().find_map(|k| custom_color(&doc, k)))
            .unwrap_or(fb)
    };
    let working = get("working", &["yellow"], fallback.working);
    let accent = doc.get("ui").and_then(|ui| ui.get("accent"))
        .and_then(|accent| accent.as_str()?.parse::<Color>().ok())
        .unwrap_or(fallback.accent);
    Theme {
        working,
        monitoring: get("monitoring", &["yellow", "peach"], working),
        blocked: get("blocked", &["red"], fallback.blocked),
        interrupted: get("interrupted", &["red", "peach"], fallback.blocked),
        done: get("done", &["teal", "green"], fallback.done),
        idle_fresh: get("idle_fresh", &["text", "subtext0"], fallback.idle_fresh),
        idle: get("idle", &["overlay0"], fallback.idle),
        idle_stale: get("idle_stale", &["overlay0"], fallback.idle_stale),
        unknown: get("unknown", &["mauve", "overlay1"], fallback.unknown),
        accent,
        dim: fallback.dim,
        projects: fallback.projects.clone(),
    }
}

pub(super) fn use_font(choice: &str, detected: bool) -> bool {
    match choice {
        "font" => true,
        "text" => false,
        _ => detected,
    }
}

pub(super) fn font_notice_due(choice: &str, detected: bool) -> bool {
    if std::env::var("HERDR_SIDEBAR_FONT").is_ok() {
        return false;
    }
    (choice == "auto" || choice.is_empty()) && !detected
}

pub(super) fn font_ok() -> bool {
    match std::env::var("HERDR_SIDEBAR_FONT").as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![
        std::path::PathBuf::from(format!("{home}/.local/share/fonts")),
        "/usr/share/fonts".into(),
        "/run/current-system/sw/share/fonts".into(),
    ];
    for depth in 0..=1 {
        let mut children = Vec::new();
        for dir in dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().to_lowercase().contains("nerd") {
                        return true;
                    }
                    if depth == 0 && entry.path().is_dir() {
                        children.push(entry.path());
                    }
                }
            }
        }
        dirs = children;
    }
    false
}

pub(super) fn state_glyph(state: State, tick: usize, font_ok: bool) -> (&'static str, bool) {
    // Only working agents advance the animation timer.
    match state {
        State::Working => (crate::icons::spinner(tick), true),
        State::Monitoring => (if font_ok { "◉" } else { "○" }, false),
        State::Done => ("✓", false),
        State::Blocked => (crate::icons::blocked_mark(tick), false),
        State::Interrupted => ("!", false),
        State::IdleFresh => ("●", false),
        State::Idle => ("·", false),
        State::IdleStale => ("·", false),
        State::Unknown => ("◌", false),
    }
}

pub(super) fn vendor_color(vendor: &str) -> Color {
    match vendor {
        "claude" => Color::Rgb(0xd9, 0x77, 0x57),
        "gemini" => Color::Rgb(0x42, 0x85, 0xf4),
        "kimi" => Color::Rgb(0x17, 0x83, 0xff),
        "deepseek" => Color::Rgb(0x4d, 0x6b, 0xfe),
        "qwen" => Color::Rgb(0x61, 0x5c, 0xed),
        "kiro" => Color::Rgb(0x90, 0x46, 0xff),
        "cline" => Color::Rgb(0x58, 0x68, 0x76),
        "kilo" => Color::Rgb(0x9a, 0x98, 0x08),
        "omp" => Color::Rgb(0xcb, 0xa6, 0xf7),
        "pi" => Color::Rgb(0xfa, 0xb3, 0x87),
        "terminal" | "sh" | "shell" => Color::Rgb(0x89, 0xb4, 0xfa),
        _ => Color::Rgb(0xc7, 0x8a, 0x1f),
    }
}

pub(super) fn state_color(theme: &Theme, state: State) -> Color {
    match state {
        State::Working => theme.working,
        State::Monitoring => theme.monitoring,
        State::Blocked => theme.blocked,
        State::Interrupted => theme.interrupted,
        State::Done => theme.done,
        State::IdleFresh => theme.idle_fresh,
        State::Idle => theme.idle,
        State::IdleStale => theme.idle_stale,
        State::Unknown => theme.unknown,
    }
}

pub(super) fn worktree_prefix(w: &Worktree, font: bool) -> [Span<'static>; 3] {
    let lead = if w.depth == 0 {
        format!("  {} ", if font { '\u{f418}' } else { '*' })
    } else {
        let tree = if font { '\u{f1bb}' } else { '+' };
        let link = if w.depth > 1 { "└─ " } else { "" };
        format!("  {}{link}{tree} ", "  ".repeat(w.depth - 1))
    };
    let mark = if w.agents.is_empty() { "" } else if w.collapsed { "▸ " } else { "▾ " };
    [Span::raw("  "), Span::raw(lead), Span::raw(mark)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_keeps_hue_but_is_quieter() {
        let spread = |c: Color| match c {
            Color::Rgb(r, g, b) => (r.max(g).max(b), r.max(g).max(b) - r.min(g).min(b)),
            _ => unreachable!(),
        };
        for raw in [Color::Rgb(0xf9, 0xe2, 0xaf), Color::Rgb(0xcb, 0xa6, 0xf7)] {
            let (bright, sat) = spread(raw);
            let (m_bright, m_sat) = spread(mute(raw));
            assert!(m_bright < bright && m_sat < sat, "{raw:?} -> {:?}", mute(raw));
        }
        let Color::Rgb(r, g, b) = mute(Color::Rgb(0xcb, 0xa6, 0xf7)) else { unreachable!() };
        assert!(b > r && r > g);
        assert_eq!(mute(Color::Cyan), Color::Cyan);
    }

    #[test]
    fn theme_parses_row_rules_and_custom() {
        let text: toml_edit::DocumentMut = "[theme.custom]\nyellow = \"#111111\"\n[ui.sidebar.agents]\nrows = [[{ token = \"x\", rules = [{ equals = \"working\", fg = \"#222222\" }] }]]".parse().unwrap();
        assert_eq!(
            rule_color(&text, "working"),
            Some(Color::Rgb(0x22, 0x22, 0x22))
        );
        assert_eq!(
            custom_color(&text, "yellow"),
            Some(Color::Rgb(0x11, 0x11, 0x11))
        );
        assert_eq!(parse_hex("#f9e2af"), Some(Color::Rgb(0xf9, 0xe2, 0xaf)));
        assert_eq!(parse_hex("nope"), None);
        // A rule must not borrow a sibling's foreground color.
        let bleed: toml_edit::DocumentMut = "[ui.sidebar.agents]\nrows = [[{rules = [{ equals = \"working\", bold = true }, { equals = \"blocked\", fg = \"#f38ba8\" }]}]]".parse().unwrap();
        assert_eq!(rule_color(&bleed, "working"), None);
        assert_eq!(
            rule_color(&bleed, "blocked"),
            Some(Color::Rgb(0xf3, 0x8b, 0xa8))
        );
        let commented: toml_edit::DocumentMut = "# rules = [{ equals = \"working\", fg = \"#222222\" }]".parse().unwrap();
        assert_eq!(rule_color(&commented, "working"), None);
        assert!("yellow = #111111 # night\n".parse::<toml_edit::DocumentMut>().is_err());
        let glyphs: toml_edit::DocumentMut = "[ui.sidebar.agents]\nrows = [[{token = '\u{f418}', rules = [{equals = 'working', fg = '#222222'}]}]]".parse().unwrap();
        assert_eq!(rule_color(&glyphs, "working"), Some(Color::Rgb(0x22, 0x22, 0x22)));
    }

    #[test]
    fn font_choice_resolves_without_asking_twice() {
        assert!(use_font("font", false));
        assert!(!use_font("text", true));
        assert!(use_font("auto", true));
        assert!(!use_font("auto", false));
        assert!(use_font("", true));
        assert!(!font_notice_due("text", false));
        assert!(!font_notice_due("font", false));
    }
}

use std::io;

use toml_edit::{DocumentMut, Item};

use super::native_patch::get;
use super::{parse, Settings};

pub(super) fn sidebar_templates(document: &DocumentMut, settings: &Settings) -> io::Result<DocumentMut> {
    let light = matches!(
        get(document, "theme.name").and_then(Item::as_str),
        Some(
            "catppuccin-latte"
                | "tokyo-night-day"
                | "gruvbox-light"
                | "one-light"
                | "solarized-light"
                | "kanagawa-lotus"
                | "rose-pine-dawn"
        )
    );
    let ink = if light { "#16161c" } else { "#e9e9f0" };
    let (idle_fresh, idle_normal, idle_stale, idle, subtle) = if light {
        ("#416c4f", "#6b6259", "#69696d", "#6e738d", "#7c7f93")
    } else {
        ("#95bba2", "#a99e92", "#8b8e9c", "#8f95ab", "#a8abbd")
    };
    let done = "#4c9a5a";
    let blocked = "#c04a4a";
    let unknown = "#907aa9";
    let none = "#9a9eb3";
    let brand_other = "#c78a1f";
    let brands = [
        ("claude", "#d97757"),
        ("gemini", "#4285f4"),
        ("kimi", "#1783ff"),
        ("qwen", "#615ced"),
        ("cline", "#586876"),
        ("kilo", "#9a9808"),
        ("omp", "#cba6f7"),
    ];
    let rules = brands
        .iter()
        .filter_map(|(agent, color)| {
            crate::icons::logo(agent, settings.icons)
                .map(|glyph| format!(r#"{{ contains = "{glyph}", fg = "{color}" }}"#))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let cell = |token: &str, color: &str, bold: bool| {
        format!(r#"{{ token = "{token}", fg = "{color}", bold = {bold}, dim = false }}"#)
    };
    let logo_cell = |token: &str| {
        if rules.is_empty() {
            format!(r#"{{ token = "{token}", fg = "{ink}", bold = false, dim = false }}"#)
        } else {
            format!(
                r#"{{ token = "{token}", fg = "{ink}", bold = false, dim = false, rules = [{rules}] }}"#
            )
        }
    };
    let agent_rows = |working_color: &str| {
        format!(
            r#"[[{group_parent}], [{group}, {group_stale}], [{split_mark}, {logo}, {logo_working}, {logo_stale}, {title_working}, {title_done}, {title_blocked}, {title_idle_fresh}, {title_idle}, {title_idle_stale}, {title_unknown}], ["$gap"]]"#,
            group_parent = cell("$group_parent", idle_stale, true),
            group = cell("$group", subtle, true),
            group_stale = cell("$group_stale", idle_stale, true),
            split_mark = cell("$split_mark", idle_stale, false),
            logo = logo_cell("$logo"),
            logo_working = logo_cell("$logo_working"),
            logo_stale = cell("$logo_stale", idle_stale, false),
            title_working = cell("$title_working", working_color, true),
            title_done = cell("$title_done", done, false),
            title_blocked = cell("$title_blocked", blocked, false),
            title_idle_fresh = cell("$title_idle_fresh", idle_fresh, false),
            title_idle = cell("$title_idle", idle_normal, false),
            title_idle_stale = cell("$title_idle_stale", idle_stale, false),
            title_unknown = cell("$title_unknown", unknown, false),
        )
    };
    let space_working_marks = brands
        .iter()
        .map(|(agent, color)| cell(&format!("$space_working_{agent}"), color, true))
        .collect::<Vec<_>>()
        .join(", ");
    let space_logos = brands
        .iter()
        .map(|(agent, color)| cell(&format!("$space_logo_{agent}"), color, false))
        .collect::<Vec<_>>()
        .join(", ");
    let space_rows = format!(
        r#"[[{blocked_mark}, {space_working_marks}, {working_other}, {done_mark}, {idle_mark}, {unknown_mark}, {none_mark}, {label}], [{space_logos}, {logo_other}]]"#,
        blocked_mark = cell("$space_blocked", blocked, true),
        space_working_marks = space_working_marks,
        working_other = cell("$space_working_other", brand_other, true),
        done_mark = cell("$space_done", done, true),
        idle_mark = cell("$space_idle", idle, false),
        unknown_mark = cell("$space_unknown", unknown, false),
        none_mark = cell("$space_none", none, false),
        label = cell("$space_label", none, false),
        space_logos = space_logos,
        logo_other = cell("$space_logo_other", ink, false),
    );
    let mut text = format!(
        "[agents]\nrows = {}\n[spaces]\nrows = {}\n",
        agent_rows(brand_other),
        space_rows
    );
    // Canonical known-agent IDs, unconditionally: a missing remote-cache file
    // says nothing about bundled manifests or local overrides.
    text.push_str("[agents.rows_by_agent]\n");
    for (agent, color) in brands {
        text.push_str(&format!("{agent} = {}\n", agent_rows(color)));
    }
    parse(&text)
}


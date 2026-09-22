use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

const PLUGIN: &str = "herdr-project-sidebar";
const SETTINGS_ACTION: &str = "herdr-project-sidebar.settings";
const STYLE_ACTION: &str = "herdr-project-sidebar.toggle-style";
const DOCK_ACTION: &str = "herdr-project-sidebar.toggle-projects";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconMode {
    #[default]
    Text,
    Font,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub width: u16,
    pub dock_right: bool,
    pub auto_open: bool,
    pub grouped: bool,
    pub show_branch: bool,
    pub show_title: bool,
    pub dim_idle: bool,
    pub icons: IconMode,
    pub enabled: bool,
    pub project_style: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            width: 30,
            dock_right: true,
            auto_open: true,
            grouped: true,
            show_branch: true,
            show_title: true,
            dim_idle: true,
            icons: IconMode::Text,
            enabled: true,
            project_style: true,
        }
    }
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn herdr_path() -> PathBuf {
    env::var_os("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".config"))
                .join("herdr/config.toml")
        })
}
fn settings_path() -> PathBuf {
    env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| parent(&herdr_path()).join("plugins/config").join(PLUGIN))
        .join("config.toml")
}

pub fn state_dir() -> PathBuf {
    env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".local/state"))
                .join("herdr/plugins")
                .join(PLUGIN)
        })
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn read_text(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn parse(text: &str) -> io::Result<DocumentMut> {
    text.parse()
        .map_err(|error| invalid(format!("Invalid TOML: {error}")))
}

fn decode(document: &DocumentMut) -> io::Result<Settings> {
    let mut settings = Settings::default();
    if let Some(item) = document.get("width") {
        let width = item
            .as_integer()
            .ok_or_else(|| invalid("width must be an integer"))?;
        if !(24..=80).contains(&width) {
            return Err(invalid("width must be between 24 and 80"));
        }
        settings.width = width as u16;
    }
    for (key, target) in [
        ("dock_right", &mut settings.dock_right),
        ("auto_open", &mut settings.auto_open),
        ("grouped", &mut settings.grouped),
        ("show_branch", &mut settings.show_branch),
        ("show_title", &mut settings.show_title),
        ("dim_idle", &mut settings.dim_idle),
        ("enabled", &mut settings.enabled),
        ("project_style", &mut settings.project_style),
    ] {
        if let Some(item) = document.get(key) {
            *target = item
                .as_bool()
                .ok_or_else(|| invalid(format!("{key} must be a boolean")))?;
        }
    }
    if let Some(item) = document.get("icons") {
        settings.icons = match item.as_str() {
            Some("text") => IconMode::Text,
            Some("font") => IconMode::Font,
            Some("none") => IconMode::None,
            _ => return Err(invalid("icons must be text, font, or none")),
        };
    }
    Ok(settings)
}

fn encode(document: &mut DocumentMut, settings: &Settings) {
    document["width"] = value(i64::from(settings.width));
    for (key, setting) in [
        ("dock_right", settings.dock_right),
        ("auto_open", settings.auto_open),
        ("grouped", settings.grouped),
        ("show_branch", settings.show_branch),
        ("show_title", settings.show_title),
        ("dim_idle", settings.dim_idle),
        ("enabled", settings.enabled),
        ("project_style", settings.project_style),
    ] {
        document[key] = value(setting);
    }
    document["icons"] = value(match settings.icons {
        IconMode::Text => "text",
        IconMode::Font => "font",
        IconMode::None => "none",
    });
}

pub fn load() -> io::Result<Settings> {
    decode(&parse(&read_text(&settings_path())?)?)
}

pub fn update(change: impl FnOnce(&mut Settings)) -> io::Result<Settings> {
    update_at(
        &herdr_path(),
        &settings_path(),
        &state_dir(),
        false,
        change,
        crate::reload,
    )
}

pub fn configure() -> io::Result<Settings> {
    update_at(
        &herdr_path(),
        &settings_path(),
        &state_dir(),
        true,
        |settings| {
            settings.enabled = true;
            settings.project_style = true;
        },
        crate::reload,
    )
}

pub fn unconfigure() -> io::Result<()> {
    update_at(
        &herdr_path(),
        &settings_path(),
        &state_dir(),
        true,
        |settings| settings.enabled = false,
        crate::reload,
    )
    .map(|_| ())
}

fn lock(path: &Path) -> io::Result<File> {
    fs::create_dir_all(parent(path))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

fn atomic_write(path: &Path, text: &str) -> io::Result<()> {
    if read_text(path)? == text && path.exists() {
        return Ok(());
    }
    fs::create_dir_all(parent(path))?;
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent(path))?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

// Store only changed fragments. Never restore an entire config over later edits.
#[derive(Default, Serialize, Deserialize)]
struct Ownership {
    config_path: PathBuf,
    changes: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
struct Change {
    path: String,
    original: Option<String>,
    written: Option<String>,
}

fn fragment(item: Option<&Item>) -> Option<String> {
    item.filter(|item| !item.is_none()).map(|item| {
        let mut document = DocumentMut::new();
        document["value"] = item.clone();
        document.to_string()
    })
}

fn unfragment(text: &Option<String>) -> io::Result<Option<Item>> {
    text.as_ref()
        .map(|text| {
            parse(text)?
                .remove("value")
                .ok_or_else(|| invalid("Missing ownership fragment"))
        })
        .transpose()
}

fn get<'a>(document: &'a DocumentMut, path: &str) -> Option<&'a Item> {
    let mut parts = path.split('.');
    let mut item = document.get(parts.next()?)?;
    for part in parts {
        item = item.get(part)?;
    }
    Some(item)
}

fn put(document: &mut DocumentMut, path: &str, replacement: Option<Item>) -> io::Result<()> {
    let mut parts = path.split('.').peekable();
    let mut table: &mut dyn toml_edit::TableLike = document.as_table_mut();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if let Some(item) = replacement {
                table.insert(part, item);
            } else {
                table.remove(part);
            }
            return Ok(());
        }
        if !table.contains_key(part) {
            if replacement.is_none() {
                return Ok(());
            }
            let mut child = Table::new();
            child.set_implicit(true);
            table.insert(part, Item::Table(child));
        }
        table = table
            .get_mut(part)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| invalid(format!("{path}: {part} must be a table")))?;
    }
    Err(invalid("Empty managed configuration path"))
}

fn own(
    document: &mut DocumentMut,
    ownership: &mut Ownership,
    path: &str,
    replacement: Option<Item>,
) -> io::Result<()> {
    let original = fragment(get(document, path));
    put(document, path, replacement)?;
    let written = fragment(get(document, path));
    if let Some(change) = ownership
        .changes
        .iter_mut()
        .find(|change| change.path == path)
    {
        if original != change.written || change.original == change.written {
            change.original = original;
        }
        change.written = written;
    } else if original != written {
        ownership.changes.push(Change {
            path: path.to_owned(),
            original,
            written,
        });
    }
    Ok(())
}

fn has_binding(item: &Item, binding: &str) -> bool {
    let matches = |text: &str| {
        let text = text.trim().to_ascii_lowercase();
        text == binding || (binding == "prefix+comma" && text == "prefix+,")
    };
    item.as_str().is_some_and(matches)
        || item
            .as_array()
            .is_some_and(|array| array.iter().filter_map(|v| v.as_str()).any(matches))
}

fn legacy_command(table: &Table) -> bool {
    table.get("type").and_then(Item::as_str) == Some("plugin_action")
        && table.get("key").is_some_and(|k| has_binding(k, "prefix+p"))
        && matches!(
            table.get("command").and_then(Item::as_str),
            Some("herdr-project-sidebar.open-projects" | "herdr-project-sidebar.toggle-projects")
        )
}

fn sidebar_templates(document: &DocumentMut, settings: &Settings) -> io::Result<DocumentMut> {
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

fn install(
    document: &mut DocumentMut,
    ownership: &mut Ownership,
    settings: &Settings,
) -> io::Result<()> {
    // Native sidebar geometry and startup visibility stay the user's: width
    // here is dock-split-only, auto_open gates dock autoload. Collapsed mode
    // remains ours while the dock stands in for the sidebar.
    own(
        document,
        ownership,
        "ui.sidebar_collapsed_mode",
        Some(value("hidden")),
    )?;
    if settings.project_style {
        let templates = sidebar_templates(document, settings)?;
        for path in ["agents.rows", "agents.rows_by_agent", "spaces.rows"] {
            own(
                document,
                ownership,
                &format!("ui.sidebar.{path}"),
                get(&templates, path).cloned(),
            )?;
        }
        for section in ["agents", "spaces"] {
            own(
                document,
                ownership,
                &format!("ui.sidebar.{section}.row_gap"),
                Some(value(0)),
            )?;
        }
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
        own(
            document,
            ownership,
            "theme.custom.active_row_bg",
            Some(value(if light { "#b9cdf2" } else { "#414868" })),
        )?;
        own(
            document,
            ownership,
            "ui.tab_bar_right_separator",
            Some(value(" · ")),
        )?;
        // Herdr owns scheduling. The formatter reads only the active cwd's HEAD.
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let tabbar_block = parse(&format!(
            r#"tab_bar_right = [{{ type = 'command', command = "exec '{exe}' --format-tabbar", interval_seconds = 6, timeout_seconds = 2 }}]"#
        ))?;
        own(
            document,
            ownership,
            "ui.tab_bar_right",
            get(&tabbar_block, "tab_bar_right").cloned(),
        )?;
    } else {
        for change in &mut ownership.changes {
            if change.path.starts_with("ui.sidebar.")
                || change.path == "theme.custom.active_row_bg"
                || change.path.starts_with("ui.tab_bar_right")
            {
                if fragment(get(document, &change.path)) == change.written {
                    put(document, &change.path, unfragment(&change.original)?)?;
                }
                // While native styling is active, later user edits become the
                // baseline for the next Projects -> native round trip.
                change.original = fragment(get(document, &change.path));
                change.written = change.original.clone();
            }
        }
    }

    let mut commands = match get(document, "keys.command") {
        None => ArrayOfTables::new(),
        Some(item) if item.as_array().is_some_and(|array| array.is_empty()) => ArrayOfTables::new(),
        Some(item) => item
            .clone()
            .into_array_of_tables()
            .map_err(|_| invalid("keys.command must be an array of tables"))?,
    };
    commands.retain(|table| !legacy_command(table));
    // prefix+p now belongs to the explicitly requested style toggle. Retain
    // any other previous-tab aliases and restore the original on uninstall.
    let previous = get(document, "keys.previous_tab")
        .cloned()
        .unwrap_or_else(|| value("prefix+p"));
    if has_binding(&previous, "prefix+p") {
        let next = if let Some(array) = previous.as_array() {
            let mut array = array.clone();
            array.retain(|item| {
                item.as_str()
                    .is_none_or(|text| !text.eq_ignore_ascii_case("prefix+p"))
            });
            Item::Value(toml_edit::Value::Array(array))
        } else {
            value("")
        };
        own(document, ownership, "keys.previous_tab", Some(next))?;
    }
    let mut added = ArrayOfTables::new();
    for (binding, action, description) in [
        ("prefix+comma", SETTINGS_ACTION, "Project sidebar settings"),
        (
            "prefix+p",
            STYLE_ACTION,
            "Projects / standard Herdr styling",
        ),
        ("prefix+a", DOCK_ACTION, "Toggle Projects dock split"),
    ] {
        if let Some(keys) = document.get("keys").and_then(Item::as_table_like) {
            for (key, item) in keys.iter() {
                if key != "command" && has_binding(item, binding) {
                    return Err(invalid(format!(
                        "{binding} is already assigned to keys.{key}"
                    )));
                }
            }
        }
        let mut bound = false;
        for command in commands.iter() {
            if command
                .get("key")
                .is_some_and(|key| has_binding(key, binding))
            {
                if command.get("type").and_then(Item::as_str) != Some("plugin_action")
                    || command.get("command").and_then(Item::as_str) != Some(action)
                {
                    return Err(invalid(format!(
                        "{binding} already belongs to another command"
                    )));
                }
                bound = true;
            }
        }
        if !bound {
            let mut command = Table::new();
            command["key"] = value(binding);
            command["type"] = value("plugin_action");
            command["command"] = value(action);
            command["description"] = value(description);
            added.push(command.clone());
            commands.push(command);
        }
    }
    if !added.is_empty() {
        if let Some(change) = ownership
            .changes
            .iter_mut()
            .find(|change| change.path == "keys.command")
        {
            let previous = unfragment(&change.written)?;
            if let Some(previous) = previous.as_ref().and_then(Item::as_array_of_tables) {
                for entry in previous.iter() {
                    added.push(entry.clone());
                }
            }
            change.written = fragment(Some(&Item::ArrayOfTables(added)));
        } else {
            ownership.changes.push(Change {
                path: "keys.command".into(),
                original: None,
                written: fragment(Some(&Item::ArrayOfTables(added))),
            });
        }
    }
    put(
        document,
        "keys.command",
        Some(Item::ArrayOfTables(commands)),
    )
}

fn restore(document: &mut DocumentMut, ownership: &Ownership) -> io::Result<()> {
    for change in &ownership.changes {
        if change.path == "keys.command" {
            // Remove only exact entries we added; leave later user edits alone.
            let written = unfragment(&change.written)?;
            let written = written.as_ref().and_then(Item::as_array_of_tables);
            if let Some(current) = get(document, "keys.command").and_then(Item::as_array_of_tables)
            {
                let mut current = current.clone();
                if let Some(written) = written {
                    current.retain(|table| {
                        !written
                            .iter()
                            .any(|entry| entry.to_string() == table.to_string())
                    });
                }
                put(document, "keys.command", Some(Item::ArrayOfTables(current)))?;
            }
        } else if fragment(get(document, &change.path)) == change.written {
            put(document, &change.path, unfragment(&change.original)?)?;
        }
    }
    Ok(())
}

fn update_at(
    native_path: &Path,
    preferences: &Path,
    state: &Path,
    force_reload: bool,
    change: impl FnOnce(&mut Settings),
    reload: impl FnOnce() -> io::Result<()>,
) -> io::Result<Settings> {
    // Both locks are stable siblings, never the inodes replaced by rename.
    let _preferences_lock = lock(&preferences.with_extension("lock"))?;
    let native_path = match fs::canonicalize(native_path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::path::absolute(native_path)?,
        Err(error) => return Err(error),
    };
    let _native_lock = lock(&native_path.with_extension("herdr-project-sidebar.lock"))?;
    let original = read_text(&native_path)?;
    let mut native = parse(&original)?;
    let mut document = parse(&read_text(preferences)?)?;
    let mut settings = decode(&document)?;
    change(&mut settings);
    settings.width = settings.width.clamp(24, 80);
    encode(&mut document, &settings);
    let journal_path = state.join("managed-config.json");
    let journal_text = read_text(&journal_path)?;
    let mut ownership: Ownership = if journal_text.is_empty() {
        Ownership {
            config_path: native_path.clone(),
            changes: Vec::new(),
        }
    } else {
        serde_json::from_str(&journal_text)
            .map_err(|error| invalid(format!("Invalid configuration ownership record: {error}")))?
    };
    if ownership.config_path != native_path {
        return Err(invalid("Configuration ownership record belongs to another HERDR_CONFIG_PATH; use a separate HERDR_PLUGIN_STATE_DIR"));
    }
    if settings.enabled {
        install(&mut native, &mut ownership, &settings)?;
    } else {
        restore(&mut native, &ownership)?;
    }
    fs::create_dir_all(state)?;
    let backup = state.join("config.original.toml");
    if !backup.exists() {
        atomic_write(&backup, &original)?;
        if let Ok(metadata) = fs::metadata(&native_path) {
            fs::set_permissions(&backup, metadata.permissions())?;
        }
    }
    if settings.enabled {
        atomic_write(
            &journal_path,
            &serde_json::to_string_pretty(&ownership).map_err(io::Error::other)?,
        )?;
    }
    let rendered = native.to_string();
    let pending_reload = state.join("config-reload.pending");
    if rendered != original || force_reload {
        // Record before replacing the config so a failed reload is not mistaken
        // for an applied, unchanged config on the next update.
        atomic_write(&pending_reload, "")?;
    }
    atomic_write(&native_path, &rendered)?;
    atomic_write(preferences, &document.to_string())?;
    if pending_reload.exists() {
        reload()
            .map_err(|error| io::Error::other(format!("Saved; Herdr reload failed: {error}")))?;
        fs::remove_file(pending_reload)?;
    }
    if !settings.enabled && journal_path.exists() {
        fs::remove_file(journal_path)?;
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_preserves_width_and_side() {
        let settings = Settings {
            width: 42,
            dock_right: false,
            ..Default::default()
        };
        let mut document = parse("").unwrap();
        encode(&mut document, &settings);
        assert_eq!(decode(&document).unwrap(), settings);
    }

    #[test]
    fn preserves_user_edits_and_restores_owned_fragments() {
        let mut document = parse(
            r##"# User configuration
[theme.custom]
accent = "#123456"
[ui]
sidebar_width = 43
[ui.sidebar.agents]
rows = [[{ token = "agent", fg = "#123456", bold = true }]]
row_gap = 2
[ui.sidebar.agents.rows_by_agent]
claude = [[{ token = "terminal_title", fg = "#654321" }]]
[ui.sidebar.spaces]
rows = [[{ token = "workspace", fg = "#345678" }]]
row_gap = 1
[keys]
toggle_sidebar = ["prefix+b", "alt+b"]
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "herdr-project-sidebar.toggle-projects"
[[keys.command]]
key = "prefix+x"
type = "shell"
command = "echo keep"
"##,
        )
        .unwrap();
        let original_styles: Vec<_> = [
            "ui.sidebar.agents.rows",
            "ui.sidebar.agents.row_gap",
            "ui.sidebar.agents.rows_by_agent",
            "ui.sidebar.spaces.rows",
            "ui.sidebar.spaces.row_gap",
        ]
        .into_iter()
        .map(|path| (path, fragment(get(&document, path))))
        .collect();
        let mut ownership = Ownership::default();
        install(&mut document, &mut ownership, &Settings::default()).unwrap();
        let installed = document.to_string();
        document = parse(&installed).unwrap();
        install(&mut document, &mut ownership, &Settings::default()).unwrap();
        assert_eq!(document.to_string(), installed);
        let native_style = Settings {
            project_style: false,
            ..Default::default()
        };
        install(&mut document, &mut ownership, &native_style).unwrap();
        for (path, original) in &original_styles {
            assert_eq!(&fragment(get(&document, path)), original);
        }
        assert_eq!(
            get(&document, "ui.sidebar_width").and_then(Item::as_integer),
            Some(43)
        );
        assert!(document.to_string().contains(STYLE_ACTION));
        assert!(document.to_string().contains(SETTINGS_ACTION));
        install(&mut document, &mut ownership, &Settings::default()).unwrap();
        assert_eq!(document.to_string(), installed);
        install(&mut document, &mut ownership, &native_style).unwrap();
        let edited_agents =
            parse(r##"rows = [[{ token = "terminal_title", fg = "#aabbcc" }]]"##).unwrap();
        put(
            &mut document,
            "ui.sidebar.agents.rows",
            edited_agents.get("rows").cloned(),
        )
        .unwrap();
        let edited_agents = fragment(get(&document, "ui.sidebar.agents.rows"));
        install(&mut document, &mut ownership, &Settings::default()).unwrap();
        let edited_spaces =
            parse(r##"rows = [[{ token = "workspace", fg = "#ddeeff" }]]"##).unwrap();
        put(
            &mut document,
            "ui.sidebar.spaces.rows",
            edited_spaces.get("rows").cloned(),
        )
        .unwrap();
        let edited_spaces = fragment(get(&document, "ui.sidebar.spaces.rows"));
        document["ui"]["sidebar_width"] = value(47);
        document["theme"]["custom"]["accent"] = value("#abcdef");
        let mut added = Table::new();
        added["key"] = value("prefix+y");
        added["type"] = value("shell");
        added["command"] = value("echo later");
        document["keys"]["command"]
            .as_array_of_tables_mut()
            .unwrap()
            .push(added);
        install(&mut document, &mut ownership, &Settings::default()).unwrap();
        document["ui"]["sidebar_width"] = value(47);
        restore(&mut document, &ownership).unwrap();
        assert_eq!(
            get(&document, "ui.sidebar_width").and_then(Item::as_integer),
            Some(47)
        );
        assert_eq!(
            get(&document, "theme.custom.accent").and_then(Item::as_str),
            Some("#abcdef")
        );
        assert_eq!(
            fragment(get(&document, "ui.sidebar.agents.rows")),
            edited_agents
        );
        assert_eq!(
            fragment(get(&document, "ui.sidebar.spaces.rows")),
            edited_spaces
        );
        for (path, original) in &original_styles {
            if !path.ends_with(".rows") {
                assert_eq!(&fragment(get(&document, path)), original);
            }
        }
        assert!(!document.to_string().contains(SETTINGS_ACTION));
        assert!(document.to_string().contains("echo keep"));
        assert!(document.to_string().contains("echo later"));
        assert!(!document
            .to_string()
            .contains("herdr-project-sidebar.toggle-projects"));
        assert!(!has_binding(
            get(&document, "keys.toggle_sidebar").unwrap(),
            "prefix+p"
        ));
        assert!(get(&document, "keys.previous_tab").is_none());
    }

    #[test]
    fn reloads_only_changed_native_config_and_retries_failed_reload() {
        let root = env::temp_dir().join(format!(
            "hps-reload-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let native = root.join("config.toml");
        let preferences = root.join("plugin/config.toml");
        let state = root.join("state");
        let reloads = std::cell::Cell::new(0);
        let reload = || {
            reloads.set(reloads.get() + 1);
            Ok(())
        };
        update_at(&native, &preferences, &state, false, |_| {}, reload).unwrap();
        assert_eq!(reloads.get(), 1);
        let installed = fs::read_to_string(&native).unwrap();
        let settings = update_at(
            &native,
            &preferences,
            &state,
            false,
            |settings| settings.width = 42,
            reload,
        )
        .unwrap();
        assert_eq!(settings.width, 42);
        assert_eq!(fs::read_to_string(&native).unwrap(), installed);
        update_at(&native, &preferences, &state, false, |_| {}, reload).unwrap();
        assert_eq!(reloads.get(), 1);
        assert!(update_at(
            &native,
            &preferences,
            &state,
            false,
            |settings| settings.project_style = false,
            || Err(io::Error::other("reload unavailable")),
        )
        .is_err());
        update_at(&native, &preferences, &state, false, |_| {}, reload).unwrap();
        assert_eq!(reloads.get(), 2);
        update_at(&native, &preferences, &state, false, |_| {}, reload).unwrap();
        assert_eq!(reloads.get(), 2);
        // Explicit configure applies the shared file to the invoking server.
        update_at(&native, &preferences, &state, true, |_| {}, reload).unwrap();
        assert_eq!(reloads.get(), 3);
        update_at(
            &native,
            &preferences,
            &state,
            false,
            |settings| settings.enabled = false,
            reload,
        )
        .unwrap();
        assert_eq!(reloads.get(), 4);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_config_and_shortcut_collision_leave_files_unchanged() {
        let root = env::temp_dir().join(format!(
            "hps-config-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let native = root.join("config.toml");
        let preferences = root.join("plugin/config.toml");
        let state = root.join("state");
        for input in [
            "[ui\nsidebar_width = 30",
            "[keys]\nsettings = 'prefix+comma'\n",
        ] {
            fs::write(&native, input).unwrap();
            assert!(update_at(
                &native,
                &preferences,
                &state,
                false,
                |_| {},
                || { panic!("invalid configuration must not reload Herdr") }
            )
            .is_err());
            assert_eq!(fs::read_to_string(&native).unwrap(), input);
            assert!(!preferences.exists());
            assert!(!state.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
}

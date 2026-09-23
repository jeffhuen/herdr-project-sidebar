use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use super::templates::sidebar_templates;
use super::{decode, encode, parse, read_text, Settings};
use crate::util::{atomic_write, invalid, open_lock};

const SETTINGS_ACTION: &str = "herdr-project-sidebar.settings";
const STYLE_ACTION: &str = "herdr-project-sidebar.toggle-style";
const DOCK_ACTION: &str = "herdr-project-sidebar.toggle-projects";

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

pub(super) fn get<'a>(document: &'a DocumentMut, path: &str) -> Option<&'a Item> {
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

fn lock(path: &Path) -> io::Result<File> {
    let file = open_lock(path)?;
    file.lock()?;
    Ok(file)
}

fn write_text(path: &Path, text: &str) -> io::Result<()> {
    if read_text(path)? == text && path.exists() {
        return Ok(());
    }
    atomic_write(path, text.as_bytes())
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

pub(super) fn update_at(
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
        write_text(&backup, &original)?;
        if let Ok(metadata) = fs::metadata(&native_path) {
            fs::set_permissions(&backup, metadata.permissions())?;
        }
    }
    if settings.enabled {
        write_text(
            &journal_path,
            &serde_json::to_string_pretty(&ownership).map_err(io::Error::other)?,
        )?;
    }
    let rendered = native.to_string();
    let pending_reload = state.join("config-reload.pending");
    if rendered != original || force_reload {
        // Record before replacing the config so a failed reload is not mistaken
        // for an applied, unchanged config on the next update.
        write_text(&pending_reload, "")?;
    }
    write_text(&native_path, &rendered)?;
    write_text(preferences, &document.to_string())?;
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
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

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

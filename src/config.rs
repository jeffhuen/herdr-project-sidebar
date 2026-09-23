use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{value, DocumentMut};

use crate::util::invalid;
use native_patch::update_at;

mod native_patch;
mod templates;

const PLUGIN: &str = "herdr-project-sidebar";

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

pub(crate) fn herdr_path() -> PathBuf {
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
}

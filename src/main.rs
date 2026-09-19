//! Native Herdr rows and a transient settings popup. Herdr owns the sidebar.
use serde_json::json;
use std::io;

pub mod activity;
mod config;
pub mod dock;
mod dock_control;
mod icons;
mod ipc;
mod native;
mod settings;
pub mod tabbar;

fn reload() -> io::Result<()> {
    let result = ipc::call("server.reload_config", json!({}))?;
    if result["status"] == "applied" {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Herdr configuration reload did not fully apply: {result}"
        )))
    }
}

fn main() -> io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None | Some("dock" | "--dock") => dock::run(),
        Some("--toggle" | "toggle") => native::dock_command(dock_control::Command::Toggle),
        Some("--ensure" | "ensure") => native::dock_command(dock_control::Command::Ensure),
        Some("--dump-snapshot") => dock::run(),
        Some("--configure") => {
            config::configure()?;
            reload()?;
            native::start()
        }
        Some("--start") => {
            if config::load()?.enabled {
                config::update(|_| ())?;
                reload()?;
            }
            native::start()
        }
        Some("--toggle-style") => {
            config::update(|settings| settings.project_style = !settings.project_style)?;
            reload()?;
            native::start()?;
            native::refresh()
        }
        Some("--daemon") => native::run(),
        Some("--format-tabbar") => tabbar::format_tabbar(),
        Some("--refresh") => native::refresh(),
        Some("--unconfigure") => {
            config::unconfigure()?;
            native::start()?;
            native::clear()?;
            reload()
        }
        Some("--settings") => ipc::call(
            "plugin.pane.open",
            json!({
                "plugin_id": "herdr-project-sidebar", "entrypoint": "settings"
            }),
        )
        .map(|_| ()),
        Some("--settings-ui") => settings::run(),
        Some("--install-font") => {
            println!("{}", icons::install()?.display());
            println!("Map U+E1A0-U+E1B0 to Herdr Agent Icons Compact in your terminal.");
            Ok(())
        }
        Some("--help" | "-h") => {
            println!(
                "herdr-project-sidebar\n\
                Terminal dock mode:\n\
                    (no args)       Run interactive project sidebar dock\n\
                    --toggle        Close dock in this tab, otherwise move/open and focus it\n\
                    --ensure        Follow this tab without focusing or overriding a manual close\n\
                Native sidebar mode:\n\
                    --configure     Apply native rows and start the publisher\n\
                    --settings      Open the settings popup\n\
                    --toggle-style  Switch Projects / standard Herdr styling\n\
                    --install-font  Install the compact icon face for this user\n\
                    --refresh       Refresh native metadata once\n\
                    --unconfigure   Restore owned configuration and clear metadata\n\
                    --start         Start the publisher when enabled"
            );
            Ok(())
        }
        Some(flag) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown option {flag}; use --help"),
        )),
    }
}

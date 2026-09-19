//! One session-owned dock, transported by Herdr without restarting its process.
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{self, Settings};
use crate::ipc;

const SOURCE: &str = "plugin:herdr-project-sidebar:dock";

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Ensure,
    Toggle,
    Close,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    session: (u64, u64),
    terminal: Option<String>,
    tab: Option<String>,
    snoozed: BTreeSet<String>,
    // Written before open: a lost reply must never cause a second launch.
    opening: bool,
}

pub struct Controller {
    path: PathBuf,
    state: State,
    saved: Option<State>,
}

#[derive(Clone)]
struct Pane {
    id: String,
    terminal: String,
    tab: String,
}

impl Pane {
    fn parse(value: &Value) -> io::Result<Self> {
        Ok(Self {
            id: text(value, "pane_id")?.to_owned(),
            terminal: text(value, "terminal_id")?.to_owned(),
            tab: text(value, "tab_id")?.to_owned(),
        })
    }
}

impl Controller {
    pub fn new(state_path: PathBuf) -> io::Result<Self> {
        let socket = std::env::var_os("HERDR_SOCKET_PATH")
            .ok_or_else(|| invalid("HERDR_SOCKET_PATH is not set"))?;
        let metadata = fs::metadata(socket)?;
        let session = (metadata.dev(), metadata.ino());
        let mut state: State = match fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error),
        };
        if state.session != session {
            state = State {
                session,
                ..State::default()
            };
        }
        let mut controller = Self {
            path: state_path,
            state,
            saved: None,
        };
        controller.save()?;
        Ok(controller)
    }

    pub fn reconcile(&mut self, snapshot: &Value, settings: &Settings) -> io::Result<()> {
        let dock = self.observe(snapshot)?;
        if !settings.enabled {
            return self.close(dock.as_ref(), None);
        }
        let Some(target) = target(snapshot, None)? else {
            return Ok(());
        };
        if self.state.snoozed.contains(&target.tab) {
            return Ok(());
        }
        if dock.is_none() && !settings.auto_open {
            return Ok(());
        }
        self.place(snapshot, dock, &target, settings, false)
    }

    pub fn command(&mut self, command: Command, caller_tab_id: Option<&str>) -> io::Result<()> {
        let response = ipc::call("session.snapshot", json!({}))?;
        let snapshot = &response["snapshot"];
        let settings = config::load()?;
        if matches!(command, Command::Ensure) {
            return self.reconcile(snapshot, &settings);
        }
        let dock = self.observe(snapshot)?;
        if matches!(command, Command::Close) {
            return self.close(dock.as_ref(), dock.as_ref().map(|pane| pane.tab.as_str()));
        }
        if !settings.enabled {
            return Err(io::Error::other("Projects is disabled in settings"));
        }
        let target = target(snapshot, caller_tab_id)?
            .ok_or_else(|| invalid("no caller or focused pane for Projects"))?;
        if dock.as_ref().is_some_and(|pane| pane.tab == target.tab) {
            return self.close(dock.as_ref(), Some(&target.tab));
        }
        self.state.snoozed.remove(&target.tab);
        self.save()?;
        self.place(snapshot, dock, &target, &settings, true)
    }

    fn save(&mut self) -> io::Result<()> {
        if self.saved.as_ref() == Some(&self.state) {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, &self.state)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        self.saved = Some(self.state.clone());
        Ok(())
    }

    fn observe(&mut self, snapshot: &Value) -> io::Result<Option<Pane>> {
        let panes = array(snapshot, "panes")?;
        let tabs = array(snapshot, "tabs")?;
        array(snapshot, "layouts")?;
        self.state
            .snoozed
            .retain(|id| tabs.iter().any(|tab| tab["tab_id"] == *id));
        let mut owned = panes.iter().filter(|pane| {
            pane.pointer("/tokens/hps_dock").and_then(Value::as_str) == Some("projects")
                || self
                    .state
                    .terminal
                    .as_deref()
                    .is_some_and(|id| pane["terminal_id"] == id)
        });
        let dock = owned.next().map(Pane::parse).transpose()?;
        if owned.next().is_some() {
            return Err(io::Error::other(
                "multiple owned Projects panes; refusing to choose or close one",
            ));
        }
        if let Some(pane) = &dock {
            self.state.terminal = Some(pane.terminal.clone());
            self.state.tab = Some(pane.tab.clone());
            self.state.opening = false;
        } else if self.state.terminal.take().is_some() {
            // Native close/process exit owns removal. Do not resurrect it in the same tab.
            if let Some(tab) = self.state.tab.take() {
                self.state.snoozed.insert(tab);
            }
        }
        self.save()?;
        Ok(dock)
    }

    fn close(&mut self, dock: Option<&Pane>, tab: Option<&str>) -> io::Result<()> {
        if let Some(tab) = tab {
            self.state.snoozed.insert(tab.to_owned());
        }
        self.save()?;
        if let Some(pane) = dock {
            ipc::call("pane.close", json!({"pane_id": pane.id}))?;
            self.state.terminal = None;
            self.state.tab = None;
            self.save()?;
        }
        Ok(())
    }

    fn place(
        &mut self,
        snapshot: &Value,
        dock: Option<Pane>,
        target: &Pane,
        settings: &Settings,
        focus: bool,
    ) -> io::Result<()> {
        if self.state.opening && dock.is_none() {
            return Err(io::Error::other(format!(
                "Projects open has an uncertain result; refusing a duplicate launch. Inspect native panes, close any unmarked Projects pane, then stop the sidebar daemon and remove {} before retrying",
                self.path.display()
            )));
        }
        let destination = layout(snapshot, &target.tab)?;
        if destination["zoomed"] == true {
            return Ok(());
        }
        if let Some(pane) = &dock {
            if layout(snapshot, &pane.tab)?["zoomed"] == true {
                return Ok(());
            }
        }
        let before = current()?;
        // The snapshot is a decision input, not authority to undo a later user focus.
        if !focus && snapshot["focused_pane_id"] != before.id {
            return Ok(());
        }
        let mut live_layout;
        let pane = match dock {
            Some(pane) if pane.tab == target.tab => {
                live_layout = destination.clone();
                pane
            }
            Some(pane) => {
                let content = content_target(destination, None, settings.dock_right)?;
                let response = ipc::call(
                    "pane.move",
                    json!({
                        "pane_id": pane.id,
                        "destination": {"type": "tab", "tab_id": target.tab,
                            "target_pane_id": content, "split": "right",
                            "ratio": initial_ratio(destination, &content, settings.width, settings.dock_right)?},
                        "focus": false
                    }),
                )?;
                let moved = &response["move_result"];
                let remapped = Pane::parse(&moved["pane"])?;
                if remapped.terminal != pane.terminal {
                    return Err(invalid("pane.move changed the dock terminal identity"));
                }
                self.state.tab = Some(remapped.tab.clone());
                self.save()?;
                if moved["changed"] == false {
                    return match moved["reason"].as_str() {
                        Some("zoomed_tab" | "same_tab") => Ok(()),
                        _ => Err(invalid("pane.move did not move the dock")),
                    };
                }
                if moved["changed"] != true || remapped.tab != target.tab {
                    return Err(invalid("pane.move omitted the confirmed destination"));
                }
                live_layout = moved["target_layout"].clone();
                remapped
            }
            None => {
                let content = content_target(destination, None, settings.dock_right)?;
                self.state.opening = true;
                self.save()?;
                let opened = ipc::call(
                    "plugin.pane.open",
                    json!({
                        "plugin_id": "herdr-project-sidebar", "entrypoint": "projects",
                        "placement": "split", "direction": "right", "focus": false,
                        "target_pane_id": content
                    }),
                );
                let response = match opened {
                    Ok(response) => response,
                    Err(error) => {
                        // ipc::call uses Other without an OS code only for a native error reply.
                        if error.kind() == io::ErrorKind::Other && error.raw_os_error().is_none() {
                            self.state.opening = false;
                            self.save()?;
                        }
                        return Err(error);
                    }
                };
                let pane = Pane::parse(&response["plugin_pane"]["pane"])?;
                self.state.terminal = Some(pane.terminal.clone());
                self.state.tab = Some(pane.tab.clone());
                self.state.opening = false;
                self.save()?;
                ipc::call(
                    "pane.report_metadata",
                    json!({
                        "pane_id": pane.id, "source": SOURCE, "tokens": {"hps_dock": "projects"}
                    }),
                )?;
                live_layout =
                    ipc::call("pane.layout", json!({"pane_id": pane.id}))?["layout"].clone();
                pane
            }
        };
        if pane.tab != target.tab {
            return Err(invalid("dock is not in the requested tab"));
        }
        let edge = content_target(&live_layout, None, settings.dock_right)?;
        let dock_edge = horizontal_edge(pane_rect(&live_layout, &pane.id)?, settings.dock_right)?;
        let wanted_edge = horizontal_edge(pane_rect(&live_layout, &edge)?, settings.dock_right)?;
        if wanted_edge > dock_edge + 1.0 {
            // Swap always focuses its source, even for background tabs. Restore the
            // actual previous focus, not the content pane we happened to split.
            let latest = current()?;
            if latest.terminal != before.terminal {
                return Ok(());
            }
            let swapped = ipc::call(
                "pane.swap",
                json!({
                    "source_pane_id": pane.id, "target_pane_id": edge
                }),
            );
            let restore = (|| {
                if focus {
                    return Ok(());
                }
                let after = current()?;
                if after.terminal == pane.terminal && before.terminal != pane.terminal {
                    ipc::call("pane.focus", json!({"pane_id": before.id}))?;
                }
                Ok::<(), io::Error>(())
            })();
            let response = swapped?;
            restore?;
            if response["swap"]["changed"] != true {
                return Err(invalid(
                    "pane.swap did not place Projects at the requested edge",
                ));
            }
            live_layout = response["swap"]["layout"].clone();
        }
        self.size(&pane, &live_layout, settings.width)?;
        // Recover a missing token after a successful open whose metadata reply failed.
        if array(snapshot, "panes")?.iter().any(|p| {
            p["terminal_id"] == pane.terminal
                && p.pointer("/tokens/hps_dock").and_then(Value::as_str) != Some("projects")
        }) {
            ipc::call(
                "pane.report_metadata",
                json!({
                    "pane_id": pane.id, "source": SOURCE, "tokens": {"hps_dock": "projects"}
                }),
            )?;
        }
        if focus {
            ipc::call("pane.focus", json!({"pane_id": pane.id}))?;
        }
        Ok(())
    }

    fn size(&mut self, pane: &Pane, layout: &Value, width: u16) -> io::Result<()> {
        if layout["zoomed"] == true {
            return Ok(());
        }
        let rect = pane_rect(layout, &pane.id)?;
        let visible = number(rect, "width")?;
        if (visible - f64::from(width)).abs() <= 1.0 {
            return Ok(());
        }
        let exported = ipc::call("layout.export", json!({"pane_id": pane.id}))?;
        if exported["layout"]["zoomed"] == true {
            return Ok(());
        }
        let area_width = number(&layout["area"], "width")?;
        let Some(split) = dock_split(
            &exported["layout"]["root"],
            &pane.id,
            area_width,
            &mut Vec::new(),
        )?
        else {
            return Ok(());
        };
        let desired = width_ratio(split.width, split.leaf_width - visible, width, split.right);
        if (desired - split.ratio).abs() < 0.00001 {
            return Ok(());
        }
        ipc::call(
            "layout.set_split_ratio",
            json!({
                "pane_id": pane.id, "path": split.path, "ratio": desired
            }),
        )?;
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn text<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid(format!("native response omitted {key}")))
}

fn array<'a>(value: &'a Value, key: &str) -> io::Result<&'a [Value]> {
    value[key]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(format!("native response omitted {key}")))
}

fn number(value: &Value, key: &str) -> io::Result<f64> {
    value[key]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| invalid(format!("native response omitted {key}")))
}

fn current() -> io::Result<Pane> {
    Pane::parse(&ipc::call("pane.current", json!({}))?["pane"])
}

fn target(snapshot: &Value, caller_tab: Option<&str>) -> io::Result<Option<Pane>> {
    let id = match caller_tab {
        Some(tab) => Some(text(layout(snapshot, tab)?, "focused_pane_id")?),
        None => snapshot["focused_pane_id"].as_str(),
    };
    let Some(id) = id else {
        return Ok(None);
    };
    let pane = array(snapshot, "panes")?
        .iter()
        .find(|pane| pane["pane_id"] == id)
        .ok_or_else(|| invalid("caller or focused pane is absent from the current snapshot"))?;
    Pane::parse(pane).map(Some)
}

fn layout<'a>(snapshot: &'a Value, tab: &str) -> io::Result<&'a Value> {
    array(snapshot, "layouts")?
        .iter()
        .find(|layout| layout["tab_id"] == tab)
        .ok_or_else(|| invalid("target tab has no current layout"))
}

fn pane_rect<'a>(layout: &'a Value, id: &str) -> io::Result<&'a Value> {
    array(layout, "panes")?
        .iter()
        .find(|pane| pane["pane_id"] == id)
        .map(|pane| &pane["rect"])
        .ok_or_else(|| invalid("pane has no current rectangle"))
}

// Native insertion splits one leaf, not the entire tab. Use the outermost
// content leaf; existing multi-row layouts keep their native tree and processes.
fn content_target(layout: &Value, exclude: Option<&str>, right: bool) -> io::Result<String> {
    let mut chosen: Option<(&str, f64)> = None;
    for pane in array(layout, "panes")? {
        let id = text(pane, "pane_id")?;
        if Some(id) == exclude {
            continue;
        }
        let x = number(&pane["rect"], "x")?;
        let edge = if right {
            x + number(&pane["rect"], "width")?
        } else {
            -x
        };
        if chosen.is_none_or(|(_, old)| edge > old) {
            chosen = Some((id, edge));
        }
    }
    chosen
        .map(|(id, _)| id.to_owned())
        .ok_or_else(|| invalid("no content pane available for Projects"))
}

fn initial_ratio(layout: &Value, id: &str, width: u16, right: bool) -> io::Result<f64> {
    Ok(width_ratio(
        number(pane_rect(layout, id)?, "width")?,
        0.0,
        width,
        right,
    ))
}

fn horizontal_edge(rect: &Value, right: bool) -> io::Result<f64> {
    let x = number(rect, "x")?;
    Ok(if right {
        x + number(rect, "width")?
    } else {
        -x
    })
}

fn width_ratio(total: f64, chrome: f64, width: u16, right: bool) -> f64 {
    let fraction = ((f64::from(width) + chrome.max(0.0)) / total.max(1.0)).clamp(0.1, 0.9);
    if right {
        1.0 - fraction
    } else {
        fraction
    }
}

struct Split {
    path: Vec<bool>,
    width: f64,
    leaf_width: f64,
    ratio: f64,
    right: bool,
}

// Return the nearest horizontal ancestor, tracking native round-to-cell splits
// so chrome is measured once rather than guessed or iteratively resized.
fn dock_split(
    node: &Value,
    pane: &str,
    width: f64,
    path: &mut Vec<bool>,
) -> io::Result<Option<Split>> {
    if node["type"] == "pane" {
        return Ok(None);
    }
    if node["type"] != "split" {
        return Err(invalid("invalid exported layout node"));
    }
    let ratio = number(node, "ratio")?;
    let horizontal = node["direction"] == "right";
    let first_width = if horizontal {
        (width * ratio).round()
    } else {
        width
    };
    for (right, key, child_width) in [
        (false, "first", first_width),
        (
            true,
            "second",
            if horizontal {
                width - first_width
            } else {
                width
            },
        ),
    ] {
        let child = &node[key];
        if !contains_pane(child, pane) {
            continue;
        }
        path.push(right);
        let deeper = dock_split(child, pane, child_width, path)?;
        path.pop();
        if deeper.is_some() {
            return Ok(deeper);
        }
        if horizontal {
            return Ok(Some(Split {
                path: path.clone(),
                width,
                leaf_width: child_width,
                ratio,
                right,
            }));
        }
    }
    Ok(None)
}

fn contains_pane(node: &Value, pane: &str) -> bool {
    node["pane_id"] == pane
        || (node["type"] == "split"
            && (contains_pane(&node["first"], pane) || contains_pane(&node["second"], pane)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_dock_width_uses_its_split_and_native_limits() {
        let root = json!({"type":"split", "direction":"right", "ratio":0.4,
            "first":{"type":"pane", "pane_id":"unrelated"},
            "second":{"type":"split", "direction":"right", "ratio":0.5,
                "first":{"type":"pane", "pane_id":"content"},
                "second":{"type":"pane", "pane_id":"dock"}}});
        let split = dock_split(&root, "dock", 200.0, &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(split.path, vec![true]);
        assert_eq!(split.width, 120.0);
        assert_eq!(split.leaf_width, 60.0);
        assert!((width_ratio(split.width, 2.0, 28, split.right) - 0.75).abs() < 0.00001);
        assert!((width_ratio(20.0, 0.0, 80, true) - 0.1).abs() < 0.00001);
        assert!((width_ratio(1000.0, 0.0, 24, false) - 0.1).abs() < 0.00001);
        assert!(dock_split(&root, "absent", 200.0, &mut Vec::new())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toggle_keeps_its_tab_after_the_invoking_dock_closes() {
        let snapshot = json!({
            "focused_pane_id": "other",
            "layouts": [{"tab_id": "origin", "focused_pane_id": "content"}],
            "panes": [
                {"pane_id": "content", "terminal_id": "term_content", "tab_id": "origin"},
                {"pane_id": "other", "terminal_id": "term_other", "tab_id": "elsewhere"}
            ]
        });
        assert_eq!(
            target(&snapshot, Some("origin")).unwrap().unwrap().id,
            "content"
        );
        assert!(target(&snapshot, Some("closed_tab")).is_err());
    }
}

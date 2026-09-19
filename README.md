# herdr-project-sidebar

Project and agent management for Herdr, providing two modes:
1. **Terminal Dock:** an interactive Ratatui split pane with a collapsible `project -> worktree -> agent` tree, keyboard navigation, and Enter/click socket focusing.
2. **Native Sidebar:** customizes Herdr's built-in Agents and Spaces lists with project headers, nested worktree branches, vendor-colored icons, status titles, and a settings popup.

## Build and enable

Requires Herdr 0.9.1+, Rust 1.89+, and Linux or macOS.

```sh
cargo build --release
cargo test --release
herdr plugin link /path/to/herdr-project-sidebar
herdr plugin action invoke configure --plugin herdr-project-sidebar
```

`plugin link` does not build or run startup hooks. The configure action starts
one metadata publisher per server and backs up the configuration before editing
it. Future server starts launch the publisher automatically. No server restart
is needed.

## Controls

- `prefix+p` switches between Projects styling and your previous Herdr rows.
  It changes both the row layout and the sort override, without moving focus,
  closing the sidebar, or creating panes. This replaces the default previous-tab
  binding on `prefix+p`; `prefix+n` and indexed tab shortcuts remain available.
- `prefix+b` keeps Herdr's normal hide/show behavior across tabs and workspaces.
  Existing custom sidebar-toggle bindings remain unchanged.
- `prefix+,` opens settings. Click values or use arrows, `j/k`, and `h/l` to edit.
  Click **Close**, or press `Esc`, `s`, or `q` to close the popup.
- Click native agent rows to focus sessions. Use Herdr's Spaces groups and
  navigator for folding, searching, worktrees, and workspace operations.

The prefix is `Ctrl+b` unless you changed it in Herdr. Conflicting custom
bindings cause configuration to fail without replacing the file.
Herdr 0.9.1 does not expose plugin actions in its mouse menus, so settings must
be opened by shortcut or CLI. Once open, every control supports the mouse.
The native sidebar stays on the left; there is no right-dock setting.

## Settings

Settings persist in `config.toml` under the directory printed by:

```sh
herdr plugin config-dir herdr-project-sidebar
```

- **Width:** 24-80 columns, applied live through Herdr's native width bounds.
- **Start open by default:** initial visibility for a new client. Herdr's saved
  manual hide/show choice takes precedence, including after reattachment.
- **Order:** project/worktree groups in workspace order, or recent native state
  changes. This does not create a separate activity history.
- **Branches:** worktree headers and native Spaces branch/git-status rows.
- **Task titles:** the native session title (including title overrides), or just the agent name.
- **Quiet idle titles:** use a readable, muted color for Herdr's idle state.
- **Agent icons:** text, compact font, or none. Text is the default.
- **Sidebar style:** Projects or your previous Herdr rows, also on `prefix+p`.
- **Install compact font:** copies the optional face into your user font directory.

Working titles are bold, blocked titles red, native completions green, and unknown
states purple. Agent logos use vendor colors. Project groups have aligned rows
and one blank line between groups. Colors adapt to the configured light or dark
theme when configuration is applied; the host theme itself is preserved.

Status comes directly from Herdr. Agent-row marks are static; the plugin has no
separate completion, blocked-state, inactivity, desktop-theme, or tab-bar engine.
Herdr still owns the Spaces/Agents split. Drag its horizontal divider to give
Agents more room; Herdr 0.9.1 reserves at least 10% and three rows for Spaces.

With `herdr --remote … --remote-keybindings server`, the native client still
reads its own local row templates and fonts. Server keybindings do not synchronize
those settings. The current server-only installation cannot apply styling,
width changes, or the Projects/native-style round trip to that remote renderer.
Client-local integration is required; installing the font on the server is not
sufficient.

## Compact icons

The bundled face derives from
[qintmb/herdr-icon-agent-ui](https://github.com/qintmb/herdr-icon-agent-ui),
with the same 17 agent codepoints. Outlines fit within 520x620 font units rather
than the upstream 760x760 box, on the same 600-unit cell advance. The separate
family name leaves other plugins' icon fonts unchanged.

Install through settings or the `install-font` action. Then map the face in the
terminal that displays Herdr. For Ghostty:

```ini
font-codepoint-map = U+E1A0-U+E1B0="Herdr Agent Icons Compact"
```

For kitty:

```conf
symbol_map U+E1A0-U+E1B0 Herdr Agent Icons Compact
```

Reload your terminal's font configuration, then choose **Font** in settings.
Installing a font on an SSH server does not install it on your workstation.
Other terminals need equivalent font fallback support, or use **Text**.

`tools/compact_font.py` reproduces the bundled font from the pinned upstream
face with FontTools. Python is not required to run the plugin. Attribution,
upstream revision, and license notices are in `assets/NOTICE`.

## Restore the previous configuration

Before unlinking or uninstalling:

```sh
herdr plugin action invoke unconfigure --plugin herdr-project-sidebar
```

This stops the publisher, clears its metadata and sort override, and restores
only configuration values that still match the plugin's last write. Later user
edits and unrelated bindings remain. The original configuration backup and
ownership record live under `HERDR_PLUGIN_STATE_DIR`.

Version 0.2 replaces the old terminal dock. Its launcher, per-tab snoozes,
collapse/pin/activity state, and `open-projects`/`toggle-projects` actions are
removed. Old `state.json` and `settings.json` files are left untouched and are
not read. Close any old dock manually after switching to the native version.

## Runtime

One Rust publisher reads Herdr's atomic `session.snapshot`, wakes on topology
events, and checks titles and settings on a one-second heartbeat. It writes only
changed display tokens. It never reports agent state or subscribes to
`pane.updated`, and it spawns no command per event. Ratatui runs only while the
settings popup is open.

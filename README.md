# herdr-project-sidebar

Project and agent management for Herdr, providing two modes:
1. **Terminal Dock:** an interactive Ratatui split pane with a collapsible `project -> worktree -> agent` tree, keyboard navigation, and Enter/click socket focusing.
2. **Native Sidebar:** customizes Herdr's built-in Agents and Spaces lists with project headers, nested worktree branches, vendor-colored icons, status titles, and a settings popup.

These are separate presentation implementations, not interchangeable skins.
Both use Herdr's agent and workspace state. The dock uses native APIs for
navigation and pane placement; native-sidebar styling does not control it.

## Build and enable

Requires Herdr 0.9.1+, Rust 1.89+, and Linux or macOS.

```sh
cargo build --release
cargo test --release
herdr plugin link /path/to/herdr-project-sidebar
herdr plugin action invoke configure --plugin herdr-project-sidebar
```

`plugin link` does not build or run startup hooks. The configure action starts
one controller per server, applies native row styling, and opens the dock when
automatic opening is enabled. Future server starts launch the controller
automatically. No server restart is needed.

## Controls

- `prefix+a` closes the dock in the current tab, or moves/opens it there.
  Closing it suppresses automatic opening in that tab until you toggle it open.
  Toggle actions retain their originating tab even if the invoking dock pane
  closes before the action runs.
- In the dock, `j/k` or arrows browse, `Enter` activates, and `h/l` fold/unfold.
  Browsing does not snap back after a timeout. Click and release on a row to
  activate it. `v` switches the saved order, `p` pins, `/` filters,
  `[`/`]` change width, and `q` closes.
  Agent counts and **Settings** appear in the bottom bar. The settings popup opens above it.
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
Herdr's native sidebar stays on the left. The terminal dock supports either side.

## Settings

Settings persist in `config.toml` under the directory printed by:

```sh
herdr plugin config-dir herdr-project-sidebar
```

- **Width:** 24-80 columns for the dock split. Herdr's split-ratio limits can
  constrain the width in narrow layouts.
- **Dock side:** left or right, applied to the existing pane without restarting it.
- **Auto open:** create a missing dock in tabs you have not manually
  closed it in. An already open dock continues following when this is off.
- **Order:** project/worktree groups in workspace order, or recent native state
  changes. Both views share this preference; the dock's `v` shortcut updates it.
  This does not create a separate activity history.
- **Branches:** worktree headers and native Spaces branch/git-status rows.
- **Task titles:** show the native session title (including title overrides) in
  both views, or just the agent name.
- **Quiet idle titles:** currently has no effect. Both renderers use their idle
  colors regardless of this setting.
- **Agent icons:** text, compact font, or none. Text is the default.
  **None** keeps the agent name and status marks.
- **Sidebar style:** Projects or your previous Herdr rows, also on `prefix+p`.
- **Install compact font:** copies the optional face into your user font directory.
  Both settings popups show installation instructions or an error.

Working titles are bold, blocked titles red, native completions green, and unknown
states purple. Agent logos use vendor colors. Project groups have aligned rows
and one blank line between groups. Colors adapt to the configured light or dark
theme when configuration is applied; the host theme itself is preserved.

Agent status comes directly from Herdr. Native row marks are static; the dock
animates working rows and uses activity timestamps only for idle freshness.
Herdr owns the Spaces/Agents split. Drag its horizontal divider to give Agents
more room. The tab-bar formatter is scheduled separately by Herdr.

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

This stops the controller, closes its dock, clears its metadata and sort override,
and restores only configuration values that still match the plugin's last write.
Later user edits and unrelated bindings remain. The original configuration backup,
ownership record, and dock pin/fold state live under `HERDR_PLUGIN_STATE_DIR`.

## Runtime

The native publisher and terminal dock each subscribe to Herdr's semantic topology
and agent-status events. Notifications trigger full `session.snapshot` reads,
coalesced on a 300ms floor, with a five-second recovery read for missed changes.
Native working/blocked animation still refreshes on the 300ms floor. The dock
animates independently, skips unchanged frames, and renders a window around the
browsing cursor. Neither process subscribes to `pane.updated`; focus events do
not spawn launchers, and metadata writes contain only changed tokens.

Agent selection and activity history use `terminal_id`; native commands use the
current pane and workspace IDs. A moved agent keeps its selection and history;
a removed browsed agent clears selection rather than selecting its replacement
row. Repository pins use the canonical repository key.

Every snapshot carries its socket incarnation. Snapshot-derived commands check
that incarnation before and after connecting, so an old view cannot target a
replacement server's reused IDs. Activity files are scoped to that incarnation
and bounded to one file per socket path. Legacy unscoped activity is not reused.
Reconnection obtains a new baseline; the daemon exits after 30 seconds of
continuous missing/refused connections instead of running indefinitely.

Failed snapshots retain the last valid tree. Non-destructive focus actions wait
for a usable snapshot, coalescing to the latest intent; failure, cancellation,
target removal, or session replacement discards the intent. Destructive actions
are refused while the view is stale.

The public plugin API has no boot/revision snapshot stream or event replay
cursor, unlike Herdr's native client protocol. Herdr 0.9.1 also checks each
per-agent status subscription at 100ms intervals on the server. Fewer full
snapshots do not imply an equivalent reduction in server-side work.

Placement uses native splits and absolute ratios. Moving into or out of a zoomed
tab waits until you unzoom it. Existing multi-row layouts are preserved; the dock
splits an edge content pane rather than rebuilding the whole tab.

The terminal dock follows server-global focus, not an individual client's view.
A single dock cannot occupy different tabs for two clients at once. Public focus
commands can also redirect other clients. Use Herdr's native lists when independent
client navigation is required.

Herdr 0.9.1 can deliver a terminal click before its queued pane-focus operation.
That late operation can override the dock's requested destination. Each click
produces at most one activation, delayed if a snapshot refresh is pending; the
dock does not retry focus. Keyboard Enter avoids this mouse ordering race. Tracked in
[Herdr #4390](https://github.com/herdrdev/herdr/issues/4390).

# herdr-project-sidebar

Project and agent management for Herdr, providing two modes:
1. **Terminal Dock:** an interactive Ratatui split pane with a collapsible `project -> worktree -> agent` tree, keyboard navigation, and Enter/click socket focusing.
2. **Native Sidebar:** customizes Herdr's built-in Agents and Spaces lists with project headers, nested worktree branches, vendor-colored icons, status titles, and a settings popup.

These are separate presentation implementations, not interchangeable skins.
Both use Herdr's agent and workspace state. The dock uses native APIs for
navigation and pane placement; native-sidebar styling does not control it.

## Install v0.2.0

Requires Herdr 0.9.1+, Rust 1.89+, Git, and Linux or macOS.

```sh
herdr plugin install jeffhuen/herdr-project-sidebar --ref v0.2.0
herdr plugin action invoke configure --plugin herdr-project-sidebar
```

Herdr builds the plugin from source. The release has no precompiled binaries.
See the [changelog](CHANGELOG.md) for changes and known limitations.
This release was verified on Linux; macOS runtime behavior was not checked.

## Build and link a checkout

```sh
git clone --branch v0.2.0 --depth 1 https://github.com/jeffhuen/herdr-project-sidebar.git
cd herdr-project-sidebar
cargo build --release --locked
cargo test --release --locked
herdr plugin link .
herdr plugin action invoke configure --plugin herdr-project-sidebar
```

`plugin link` does not build or run startup hooks. The configure action starts
one controller per server, applies native row styling, and opens the dock when
automatic opening is enabled. Future server starts launch the controller
automatically. No server restart is needed.

Before rebuilding an already linked checkout, stop the old controller:

```sh
herdr plugin action invoke unconfigure --plugin herdr-project-sidebar
```

After the build, run the configure action again. This replaces the plugin's
controller and dock without restarting Herdr or closing agent panes.

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

### Copy references (Unreleased)

In the terminal dock, press `m` on a browsed row or use `Ctrl+right-click` to
forward the click through Herdr. Plain right-click also works when forwarded.
Use arrows or `j/k`, then `Enter`, or click an action. `Esc` or an outside click
closes the menu without activating a row.

- **Project:** name, available repository path/key, and all member workspace IDs.
- **Worktree:** checkout path, branch name, and owning workspace ID.
- **Agent:** working directory, pane/terminal/tab/workspace IDs, and the agent
  session reference when supplied by Herdr. Pi/OMP session files are copied as
  paths, not presented as session UUIDs.
- **Copy reference:** a JSON block with available fields, host, and Herdr socket.

A terminal ID follows a moved agent; its workspace-qualified pane ID can change.
An open menu follows that terminal and reads its current IDs before copying.
Unknown repository paths are labelled as directories; ambiguous project paths
are listed together rather than choosing one child.

Clipboard writes travel through Herdr to the viewing client, not a clipboard
program on the server. Over SSH, the outer terminal must allow OSC 52 writes.
“Clipboard request sent” confirms the request, not an OS clipboard acknowledgement.
Menus do not change the normal snapshot cadence or add metadata writes.

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
- **Branches:** use branch names for linked-worktree headings in the native
  Agents list. When off, use workspace labels. This does not control dock
  headers or Herdr's own Spaces rows.
- **Task titles:** show the native session title (including title overrides) in
  both views, or just the agent name.
- **Quiet idle titles:** currently has no effect. Both renderers use their idle
  colors regardless of this setting.
- **Agent icons:** text, compact font, or none. Text is the default.
  **None** keeps the agent name and status marks.
- **Sidebar style:** Projects or your previous Herdr rows, also on `prefix+p`.
  This changes the native lists only, not the terminal dock.
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
Animations are capped at four frames per second. Native animation patches cached
title marks without reading a snapshot or running dock reconciliation. Title text
changes without a subscribed event can take about five seconds to appear.
The dock animates only working rows in its rendered window, skips unchanged frames,
and reuses its signature buffers. Idle input polling waits up to 300ms; menus and
pending synchronization use 50ms. Keyboard input wakes the poll immediately.
Neither process subscribes to `pane.updated`; focus events do not spawn launchers,
and metadata writes contain only changed tokens.

With the dock open, there are two persistent plugin processes per socket: the
publisher/controller and the dock. Herdr still schedules a short-lived tab-bar
formatter every six seconds. It reads the supplied cwd and local Git HEAD files,
without running Git or enumerating workspaces and worktrees. Branch labels use the
nearest checkout, including a submodule's own branch or detached HEAD.

Concurrent launchers serialize before spawning a publisher. Disabled `--start`
calls do nothing, and failed startup kills and reaps the child. A periodic registry
check detects native plugin disable or removal, restores owned configuration, and
closes the dock before exiting. `--unconfigure` can also close an orphaned dock
without starting a replacement publisher.

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

The optimized build used the earlier two-frames-per-second animation cap in the
following measurements.

In an isolated Herdr 0.9.1 before/after check with six 120×40 PTY clients and one
working agent over 12 seconds, combined publisher/dock snapshot calls fell from
37 to 4. Aggregate client terminal output fell from 64,074 to 27,026 bytes (58%).
An idle run produced no client terminal output and no publisher or dock writes.
A separate run kept all six clients connected with 12 working agents. These are
local checks, not Mosh network measurements or verification of the reported remote
freeze.

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

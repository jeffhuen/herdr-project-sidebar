# Changelog

## [Unreleased]

## [0.2.3] - 2026-09-26

### Fixed

- The dock and native sidebar now order sessions by visual tab position. Moving
  tabs in the tab bar updates the session order under each worktree. Previously,
  the dock sorted sessions by immutable tab IDs, which ignored tab moves. Both
  renderers now read the snapshot tab array index rather than the creation
  number.

## [0.2.2] - 2026-09-23

Internal cleanup for maintainability. Smaller modules and shared helpers replace
duplicated code, with about 800 fewer lines. Behavior changes only as listed below.

### Changed

- `D` opens the Close dialog, like **Close** in the row menu. On a project header
  that covers several workspaces, `D` shows a hint instead.
- Menus and dialogs close when the dock loses focus, for example after a click in
  another pane.
- The dock and the settings popup share one settings view.

### Fixed

- The dock reads its theme from Herdr's config file: `HERDR_CONFIG_PATH`, then
  `$XDG_CONFIG_HOME/herdr/config.toml`, then `~/.config/herdr/config.toml`.
- Space logos for kiro, cline, kilo, and other agents clear when those agents
  leave a workspace or the plugin is unconfigured.
- A click on a worktree row's fold marker folds the row. Before, it opened the
  workspace.

### Removed

- The undocumented `--dump-snapshot` flag.
- The native spinner timer. Native status marks were already static.

## [0.2.1] - 2026-09-23

### Added

- Copy-only menus for project, worktree, and agent rows in the terminal dock.
  Open with `m` or right-click.
- Copy individual paths and native IDs, or a JSON reference with host and Herdr
  socket context. Agent session paths remain distinct from terminal and pane IDs.
- Forward clipboard writes through Herdr to the viewing client using OSC 52,
  including SSH sessions. Repository details are looked up only on demand.
- Keep open menus attached to stable row identities. Refresh moved agents' IDs
  and dismiss menus when their target, conversation, or server disappears.
- Workspace actions in the dock menus, above the copy references. Linked
  worktree rows offer **Rename**, **Close**, and **Delete worktree checkout...**.
  Main checkout rows offer **Rename**, **Close**, **New worktree**, and
  **Open worktree...**.
- Project headers offer **New worktree** and **Open worktree...**. They also
  offer **Rename** and **Close** when the project has one workspace.
- **Close** and **Delete worktree checkout...** ask for confirmation.
  **Delete worktree checkout...** does not force the removal of a checkout that
  has changes.
- Dialogs respond to the keyboard and the mouse. Enter or the confirm button
  submits. `^c` clears the input. Esc, the `esc cancel` hint, or a click outside
  the dialog cancels.
- Menus and dialogs use Herdr's `[ui] accent` color and an opaque background.
  Dialog controls use shorter labels in narrow docks.
- A **[+ New]** button in the footer, opposite **[⚙ Settings]**. The agent
  summary moves to a separate line above the buttons.

### Changed

- Restore dock-only animation at a 300ms interval. Keep native spinner marks static
  during the remote-lag trial without disabling status or title updates.
  Title-only changes without a subscribed event can take about five seconds to appear.
- Animate only working rows in the dock's rendered window, reuse frame-signature
  buffers, and reduce idle polling while preserving immediate keyboard wakeups.
- Read local Git HEAD files for branch labels instead of enumerating workspaces
  and worktrees. Use the nearest checkout, including submodules. Run the native
  tab-bar formatter with `exec` instead of retaining a shell parent.
- Serialize each IPC request once and write it as one complete JSON line.
- **[+ New]** and `N` focus the terminal of the new workspace. Herdr sets its
  directory with its CWD policy, using the current workspace as the source.
- The footer uses muted theme colors without bold: the accent for the buttons
  and the working color for the agent summary. `MUTE_SATURATION` and
  `MUTE_BRIGHTNESS` in `src/dock.rs` set the amount.
- Footer messages clear after 4 seconds and the agent summary returns. An
  expired "press D again" prompt also clears, so the next `D` asks again.
- Worktree rows without agents do not show a fold marker.

### Fixed

- Place native-sidebar separators between project groups rather than between
  sessions whose global recency interleaves with another project.
- Label agents without mapped icons by harness name, so sessions such as Muse
  remain distinct from project headings. Explicitly disabled icons stay hidden.
- Route plain right-clicks to the dock on startup so reopening it preserves access
  to its reference menus.
- Repaint open copy menus only when their visible state changes. Repeated mouse
  motion over the same item and ignored keys no longer produce terminal writes;
  selection changes and copy errors still repaint.
- Invalidate session snapshots in the font dialog only when a font option is chosen;
  ignored keys and Esc no longer force snapshot fetches or repaints.
- Skip configuration updates and server reloads when setting adjustments result in
  identical values, including width limits in the popup and dock shortcuts.
- Return early from fold commands when a project or worktree is already in the
  requested collapsed state, eliminating redundant `state.json` rewrites.
- Back off persistent refresh errors by invalidating synchronization only on new
  error occurrences instead of retrying at the 300ms snapshot floor.
- Do not start a publisher during `--unconfigure`; close orphaned docks even when
  their publisher has already exited.
- Serialize concurrent startup callers before spawning, give child initialization
  its own readiness deadline, and kill and reap the child on startup failure.
  Disabled `--start` calls no longer launch a daemon.
- Detect native plugin disable or removal through periodic registry checks, then
  restore owned configuration, clear metadata, close the dock, and exit.
- Clicking a worktree row opens its workspace, as in the native sidebar.
  Clicking the fold marker still folds the row.
- The cursor stays on a clicked worktree row and does not jump to the project
  header.

### Known issues

- A remote, multi-client freeze has been reported. Its cause remains under
  investigation; a fix for the freeze has not been verified.

## [0.2.0] - 2026-09-20

First tagged release. Earlier development builds were available from `main`.

### Added

- A terminal dock with project and worktree groups, agent rows, keyboard and mouse
  navigation, filtering, pins, and width and side controls.
- Project styling for Herdr's native Agents and Spaces lists, with vendor-colored
  logos, status titles, and restoration of the previous configuration.
- Shared settings and an optional compact icon font, installable from either
  settings popup.

### Fixed

- Preserve agent selection and activity history across pane moves. Clear selection
  when a browsed agent disappears instead of activating its replacement row.
- Scope snapshot-derived commands and activity history to the current server
  connection so stale IDs cannot target a replacement server.
- Refresh status and topology from native events, with rate-limited snapshots and
  recovery reads. Retain the last valid tree after snapshot failures.
- Preserve existing pane layouts and the originating tab of dock actions. Respect
  tabs where the dock was manually closed.
- Place agent counts and Settings in one bottom bar, with the popup above it.
- Persist Order changes from both settings and the dock's `v` shortcut. Keep the
  selected agent when the order changes inside the popup.
- Honor Task titles in the dock, including removal of the title separator.
- Show each agent name once when Agent icons is set to None, retaining status marks.
- Run the font installer from the dock and display success or failure. Restore the
  count summary when the popup closes by mouse.
- Preserve preference changes made in another settings window when closing the
  dock popup.

### Distribution and verification

- Source release for the plugin's declared Linux and macOS platforms. Herdr builds
  locally with `cargo build --release --locked`; no precompiled binaries are included.
- Requires Herdr 0.9.1+, Rust 1.89+, and Git.
- Verified on Linux with Herdr 0.9.1: 45 release tests, keyboard and mouse settings
  checks, restart persistence, font installation and error handling, and live dock
  replacement without losing preferences or agent panes. macOS runtime behavior
  was not checked.

### Known limitations

- Branches controls linked-worktree headings in the native Agents list only. It
  does not control dock headers or Herdr's own Spaces rows.
- Quiet idle titles has no effect. Sidebar style changes the native lists only.
- Remote native clients need their own row configuration and fonts. Server-side
  installation does not configure the workstation renderer.
- The dock follows server-global focus, not separate views for multiple clients.
- Herdr 0.9.1 can override a mouse activation with a late pane-focus operation.
  Keyboard Enter avoids this race. See [Herdr #4390](https://github.com/herdrdev/herdr/issues/4390).

[Unreleased]: https://github.com/jeffhuen/herdr-project-sidebar/compare/v0.2.3...main
[0.2.3]: https://github.com/jeffhuen/herdr-project-sidebar/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/jeffhuen/herdr-project-sidebar/releases/tag/v0.2.2
[0.2.1]: https://github.com/jeffhuen/herdr-project-sidebar/releases/tag/v0.2.1
[0.2.0]: https://github.com/jeffhuen/herdr-project-sidebar/releases/tag/v0.2.0

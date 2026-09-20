# Changelog

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

[0.2.0]: https://github.com/jeffhuen/herdr-project-sidebar/releases/tag/v0.2.0

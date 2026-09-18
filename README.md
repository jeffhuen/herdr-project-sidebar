# herdr-project-sidebar

Orca-style project sidebar for Herdr. Live `workspace -> worktree -> agent` tree, git-aware, event-driven.

Status: full replacement for the default workspace sidebar (`cargo test --release` 10/10, release clean, linked).
Live Herdr sync in `snapshot()` (agent/workspace list grouped by `repo_key`, branch via `.git/HEAD`),
socket focus on Enter/click (`herdr agent focus`), collapse/pins/activity in `state.json`,
`--ensure`/`--toggle` launcher with `open-projects` + `toggle-projects` actions.
Theming reads Herdr's own `config.toml` (`theme.custom` + sidebar row rules) with defaults fallback.

Run: `cargo run --release` (`q` quits, `j/k` moves, `enter` focuses/folds, `/` filters,
`v` grouped/recent, `c` compact, `J` jump-to-attention, `o` focus workspace, `p` pin, `D` close workspace, `N` new workspace, `F` font options).
No TUI: `--dump-snapshot`, `--launch-decision`, `--ensure`, `--toggle`.

## Install

Requires Herdr 0.9+. A Nerd Font lights up the icons (see Fonts); plain
glyphs otherwise.

```sh
herdr plugin install <owner>/herdr-project-sidebar
```

From a checkout instead:

```sh
herdr plugin link /path/to/herdr-project-sidebar
herdr plugin action invoke herdr-project-sidebar.open-projects
```

## Keys

One binding covers open, focus, and close. Paste into Herdr's `config.toml`
(`prefix` is `ctrl+b` by default, so it never collides with pane contents):

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "herdr-project-sidebar.toggle-projects"
description = "Projects: toggle sidebar"
```

The `open-projects` action (`--ensure`) is the same door without the close
swing: run it from the action list when you only ever want to open/focus.

## Fonts

The sidebar uses four Nerd Font glyphs, each with an ASCII fallback, so it
reads fine without one:

| Where | Nerd Font | Fallback |
| --- | --- | --- |
| project head | repo glyph | `#` |
| worktree row | branch mark | `└─` |
| pinned project | pin mark | `*` |
| agent states | spinner, tick, `?` | same (already plain) |

First run without a detected Nerd Font shows a one-line notice. Press `F`
for options: rescan after installing one (e.g. JetBrainsMono Nerd Font in
the terminal), keep ASCII forever, or assume a font is present. The choice
persists in `state.json`; `HERDR_SIDEBAR_FONT=0/1` overrides everything.

## Marketplace

Listing is automatic: a public GitHub repo with the `herdr-plugin` topic and
this manifest on the default branch is indexed within ~30 minutes. Keep
`id`, `name`, `version`, `platforms`, and `min_herdr_version` parseable;
forks and archived repos are excluded.

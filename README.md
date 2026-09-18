# herdr-project-sidebar

Orca-style project sidebar for Herdr. Collapsible `project -> worktree -> agent` tree, git-aware, event-driven.

Status: skeleton verified (`cargo build --release`, linked, `open-projects` action renders + collapses in a right split). Live Herdr sync (`events.subscribe`, `agent.list`, `workspace.list`) plugs into `snapshot()` next.

Run: `cargo run --release` (`q` quits, `j/k` moves, `enter` collapses).

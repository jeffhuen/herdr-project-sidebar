# herdr-project-sidebar agent contract

Rust + Ratatui docked pane for Herdr: collapsible `project -> worktree -> agent` tree, git-aware, event-driven.

## Build and checks

```sh
cargo build --release
cargo test --release
```

Link the checkout for live verification (no install step, `plugin link` runs no build):

```sh
herdr plugin link /home/jeffhuen/projects/herdr-project-sidebar
herdr plugin action invoke open-projects --plugin herdr-project-sidebar
```

## Constraints

- Perf budget: deadline-driven tick (150ms only while a visible agent works, 1s idle sleep), signature-skipped draws, windowed render. No per-event process spawns.
- Live Herdr sync plugs into `snapshot()`; keep its return shape stable so render does not change.
- Never subscribe to `pane.updated`: the echo pushes write latency from ~1ms to ~110ms (measured in herdr-radar recon).

## Shared procedures

Use [pi-beads-companion](https://github.com/jeffhuen/pi-beads-companion) for the Beads workflow and `skill://herdr-workflow` for delegation, worktrees, and review. Companion setup owns the short Beads block in this file and `.beads/PRIME.md`; keep project rules outside those managed sections.

Choose branch isolation separately from issue tracking. Preserve other writers' work and use only authorized Git and remote operations. Herdr manages resources it creates, not the harness's internal isolation.

## Code discovery

No symbol or concept graphs are configured for this repository yet. For exact text, scripts, configuration, and source, use direct search and source reads.

<!-- BEGIN PI-BEADS-COMPANION -->
## Beads companion

Use native harness plans, todos, memory, and subagents for execution. Beads holds durable outcomes, acceptance criteria, high-level plans, and recovery checkpoints, not every execution step. Small bounded work needs no new bead unless project rules require one. Choose worktrees separately for writer or branch isolation. Read the project workflow with `bd prime`. One coordinator owns bead updates and closure unless ownership transfers explicitly. For assigned bead-scoped work, verify acceptance, record final evidence, and close the bead within your authority before reporting it complete. Completed todos alone do not prove acceptance. Helpers report back; lifecycle events do not close issues. Keep official Herdr integrations separate.
<!-- END PI-BEADS-COMPANION -->

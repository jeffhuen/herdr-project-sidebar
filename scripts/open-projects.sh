#!/usr/bin/env bash
# Open-or-focus the Projects pane in the invoking tab (idempotent).
set -euo pipefail
exec ./target/release/herdr-project-sidebar --ensure

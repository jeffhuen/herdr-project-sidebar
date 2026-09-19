#!/usr/bin/env bash
# Open-or-focus the Projects pane in the invoking tab (idempotent).
set -euo pipefail
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec "$dir/target/release/herdr-project-sidebar" --ensure

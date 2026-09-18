#!/usr/bin/env bash
# Open-or-focus the Projects pane in the invoking tab.
set -euo pipefail
HERDR="${HERDR_BIN_PATH:-herdr}"
exec "$HERDR" plugin pane open --plugin herdr-project-sidebar --entrypoint projects --placement split --direction right --focus

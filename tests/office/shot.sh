#!/usr/bin/env bash
# Headless Chrome screenshot of the Office view at a given hour of day.
#
# Usage: OFFICE_URL=http://127.0.0.1:18420 tests/office/shot.sh <hour 0-23> <out.png> [hash]
# `hash` defaults to "h=<hour>"; pass e.g. "h=21&focus=busy-loop" to override it.
# SHOT_TIMEOUT (ms, default 8000) is how long the page runs before the capture;
# SHOT_FLAGS adds Chrome flags (e.g. "--force-prefers-reduced-motion").
# Start the server first with tests/office/sandbox.sh.
set -euo pipefail

: "${OFFICE_URL:?set OFFICE_URL to the sandbox base URL}"
HOUR="${1:?usage: shot.sh <hour> <out.png> [hash]}"
OUT="${2:?usage: shot.sh <hour> <out.png> [hash]}"
HASH="${3:-h=$HOUR}"
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

PROFILE="$(mktemp -d)"
trap 'rm -rf "$PROFILE"' EXIT
# A throwaway profile keeps this from attaching to (and hanging on) a running Chrome.
# --timeout, not --virtual-time-budget: the page keeps a request open (the state
# stream), which a virtual-time budget waits on forever.
# shellcheck disable=SC2086
"$CHROME" --headless=new --user-data-dir="$PROFILE" --enable-unsafe-swiftshader --use-gl=angle --use-angle=swiftshader \
  --window-size=1500,900 --timeout="${SHOT_TIMEOUT:-8000}" ${SHOT_FLAGS:-} --screenshot="$OUT" \
  "$OFFICE_URL/loops?view=office#$HASH"
echo "wrote $OUT"

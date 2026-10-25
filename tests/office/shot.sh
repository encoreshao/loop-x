#!/usr/bin/env bash
# Headless Chrome screenshot of the Office view at a given hour of day.
#
# Usage: OFFICE_URL=http://127.0.0.1:18420 tests/office/shot.sh <hour 0-23> <out.png> [hash]
# `hash` defaults to "h=<hour>"; pass e.g. "h=21&focus=busy-loop" to override it.
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
"$CHROME" --headless=new --user-data-dir="$PROFILE" --enable-unsafe-swiftshader --use-gl=angle --use-angle=swiftshader \
  --window-size=1500,900 --virtual-time-budget=15000 --screenshot="$OUT" \
  "$OFFICE_URL/loops?view=office#$HASH"
echo "wrote $OUT"

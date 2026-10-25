#!/usr/bin/env bash
# Disposable dev server for the Office view: every state directory is a fresh temp dir
# (never the real ~/.loopx, Keychain or launchd), seeded with one loop per office state
# plus a pending approval on gitlab-loop.
#
# Usage: tests/office/sandbox.sh [port]     (default port 18420)
# Prints "OFFICE_URL=http://127.0.0.1:<port>", plus SANDBOX_HOME (the config dir, with
# loops.json) and SANDBOX_ROOT (outputs/status/ holds the status files) so a check can
# edit them. Ctrl-C stops the server and removes the temp dirs. SIGUSR1 stops just the
# server and SIGUSR2 starts it again on the same port (the staleness check uses both). For screenshots, in another shell:
#   OFFICE_URL=http://127.0.0.1:18420 tests/office/shot.sh 14 /tmp/day.png
set -euo pipefail

PORT="${1:-18420}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

# Build with the real toolchain dirs, then swap HOME for the sandbox below.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
(cd "$REPO" && cargo build -q -p loopx)
BIN="$(cd "$REPO" && cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/loopx"

SANDBOX="$(mktemp -d)"
export LOOP_ENGINEERING_HOME="$SANDBOX/home"
export HOME="$SANDBOX/user"
export LOOPX_ROOT="$SANDBOX/root"
export LOOP_UPDATE_DIR="$SANDBOX/update"
export LOOP_EVENTS_DIR="$SANDBOX/events"
mkdir -p "$LOOP_ENGINEERING_HOME" "$HOME" "$LOOP_UPDATE_DIR" "$LOOP_EVENTS_DIR" \
  "$LOOPX_ROOT/outputs/status" "$LOOPX_ROOT/outputs/approvals"

SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$SANDBOX"
}
trap cleanup EXIT
trap 'exit 0' INT TERM

# off-loop: disabled; busy-loop: running (this script's own pid is alive);
# broken-loop: failed; quiet-loop: idle; gitlab-loop: idle + one pending approval -> waiting.
cat > "$LOOP_ENGINEERING_HOME/loops.json" <<JSON
[
  {"name": "gitlab-loop", "enabled": true},
  {"name": "busy-loop", "enabled": true},
  {"name": "broken-loop", "enabled": true},
  {"name": "quiet-loop", "enabled": true},
  {"name": "off-loop", "enabled": false}
]
JSON
NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
LATER="$(date -u -v+1d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+1 day' +%Y-%m-%dT%H:%M:%SZ)"
echo "{\"state\":\"idle\",\"updated_at\":\"$NOW\"}" > "$LOOPX_ROOT/outputs/status.json"
echo "{\"state\":\"running\",\"pid\":$$,\"updated_at\":\"$NOW\"}" > "$LOOPX_ROOT/outputs/status/busy-loop.json"
echo "{\"state\":\"failed\",\"updated_at\":\"$NOW\"}" > "$LOOPX_ROOT/outputs/status/broken-loop.json"
echo "{\"state\":\"idle\",\"updated_at\":\"$NOW\"}" > "$LOOPX_ROOT/outputs/status/quiet-loop.json"
echo "{\"state\":\"running\",\"pid\":$$,\"updated_at\":\"$NOW\"}" > "$LOOPX_ROOT/outputs/status/off-loop.json"
cat > "$LOOPX_ROOT/outputs/approvals/apr_sandbox0001.json" <<JSON
{"id":"apr_sandbox0001","kind":"merge_request","run_id":"run-1","project":"demo","issue_iid":1,
 "title":"Demo merge request","branch":"fix/demo","target_branch":"main","summary":"Sandbox approval",
 "status":"pending","created_at":"$NOW","expires_at":"$LATER","decided_at":null,"decided_by":null}
JSON

cd "$REPO"
start_server() {
  [ -z "$SERVER_PID" ] || return 0
  "$BIN" serve --port "$PORT" &
  SERVER_PID=$!
}
stop_server() {
  [ -z "$SERVER_PID" ] || { kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; }
  SERVER_PID=""
}
trap stop_server USR1
trap start_server USR2
start_server
UP=""
for _ in $(seq 1 60); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/loops" && UP=1 && break
  sleep 0.5
done
[ -n "$UP" ] || { echo "server did not come up on port $PORT" >&2; exit 1; }
echo "SANDBOX_HOME=$LOOP_ENGINEERING_HOME"
echo "SANDBOX_ROOT=$LOOPX_ROOT"
echo "OFFICE_URL=http://127.0.0.1:$PORT"
# Short sleeps (not one long wait) so USR1/USR2 are handled promptly.
while :; do sleep 0.2; done

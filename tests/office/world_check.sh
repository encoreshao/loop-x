#!/usr/bin/env bash
# Checks the Office world renders: screenshots at three hours and from the left
# camera must exist, be > 200 KB (the empty container alone is ~110 KB) and
# differ from each other; with WebGL off the page must show the List with the
# note; under reduced motion two captures taken at different moments must be
# identical (nothing ambient moves).
#
# Usage: tests/office/world_check.sh [out-dir] [port]
# Screenshots are kept in out-dir (default: a fresh temp dir, printed at the end).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$(mktemp -d)}"
PORT="${2:-18431}"
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
mkdir -p "$OUT"

LOG="$OUT/sandbox.log"
"$REPO/tests/office/sandbox.sh" "$PORT" > "$LOG" 2>&1 &
SANDBOX_PID=$!
cleanup() {
  kill "$SANDBOX_PID" 2>/dev/null || true
  wait "$SANDBOX_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 600); do
  grep -q '^OFFICE_URL=' "$LOG" 2>/dev/null && break
  kill -0 "$SANDBOX_PID" 2>/dev/null || { cat "$LOG" >&2; echo "FAIL: sandbox exited" >&2; exit 1; }
  sleep 0.5
done
OFFICE_URL="$(grep '^OFFICE_URL=' "$LOG" | head -1 | cut -d= -f2-)"
[ -n "$OFFICE_URL" ] || { cat "$LOG" >&2; echo "FAIL: sandbox did not start" >&2; exit 1; }
export OFFICE_URL

# A capture under ~200 KB is the empty dark container (the scene had not drawn
# yet); a loaded office is 300+ KB. Retry such a capture twice before failing on it.
shoot() {
  local out="$2"
  for attempt in 1 2 3; do
    "$REPO/tests/office/shot.sh" "$@"
    [ "$(wc -c < "$out")" -gt 200000 ] && return 0
    echo "note: $out came out blank (attempt $attempt)" >&2
  done
}
shoot 13 "$OUT/day.png"
shoot 17.9 "$OUT/dusk.png"
shoot 23 "$OUT/night.png"
shoot 13 "$OUT/left.png" "left&h=13"
shoot 23 "$OUT/right-night.png" "right&h=23"
shoot 13 "$OUT/inside.png" "in&h=13"
SHOT_FLAGS="--force-prefers-reduced-motion" SHOT_TIMEOUT=6000 shoot 13 "$OUT/reduced-a.png"
SHOT_FLAGS="--force-prefers-reduced-motion" SHOT_TIMEOUT=9000 shoot 13 "$OUT/reduced-b.png"

# No WebGL: the DOM after load must show the List and the note, and log no errors.
PROFILE="$(mktemp -d)"
"$CHROME" --headless=new --user-data-dir="$PROFILE" --disable-gpu --disable-software-rasterizer \
  --enable-logging=stderr --v=0 --window-size=1500,900 --timeout=6000 --dump-dom \
  "$OFFICE_URL/loops?view=office" > "$OUT/nowebgl.html" 2> "$OUT/nowebgl.log" || true
rm -rf "$PROFILE"

python3 -I - "$OUT" <<'PY'
import hashlib, os, re, sys

out = sys.argv[1]
fails = []

def digest(name):
    path = os.path.join(out, name)
    if not os.path.exists(path):
        fails.append(f"{name}: missing")
        return None
    size = os.path.getsize(path)
    if size <= 200_000:
        fails.append(f"{name}: only {size} bytes (blank?)")
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()

shots = ["day.png", "dusk.png", "night.png", "left.png", "right-night.png", "inside.png"]
hashes = {name: digest(name) for name in shots}
seen = {}
for name, h in hashes.items():
    if h is None:
        continue
    if h in seen:
        fails.append(f"{name} is identical to {seen[h]}")
    seen[h] = name

a, b = digest("reduced-a.png"), digest("reduced-b.png")
if a and b and a != b:
    fails.append("reduced motion: captures 3 s apart differ (something still moves)")

dom = open(os.path.join(out, "nowebgl.html"), encoding="utf-8", errors="replace").read()
office = re.search(r"<section id=\"office\"[^>]*>", dom)
listing = re.search(r"<div id=\"office-list\"[^>]*>", dom)
if not office or "hidden" not in office.group(0):
    fails.append("no WebGL: #office is not hidden")
if not listing or "hidden" in listing.group(0):
    fails.append("no WebGL: the List is not shown")
if "office-note" not in dom:
    fails.append("no WebGL: the note is missing")
if 'class="card loop-section"' not in dom:
    fails.append("no WebGL: the List sections are missing")
log = open(os.path.join(out, "nowebgl.log"), encoding="utf-8", errors="replace").read()
errors = [l for l in log.splitlines() if "Uncaught" in l]
if errors:
    fails.append("no WebGL: console errors: " + " | ".join(errors[:3]))

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: " + ", ".join(f"{n} {os.path.getsize(os.path.join(out, n)) // 1024} KB" for n in shots))
PY
echo "screenshots in $OUT"

#!/usr/bin/env bash
# Checks the Office people: everyone is placed in the zone their state calls for,
# state changes walk them there (or teleport under reduced motion), a change made
# mid-walk ends in the final zone, 40 loops draw 24 with "and 16 more", zero loops
# is an empty room, and a hostile loop name stays canvas text. No console errors.
#
# Drives headless Chrome over the DevTools pipe (tests/office/cdp.py, standard
# library only): it loads the sandbox's office at 13h, reads
# `window.__office.debug()` and feeds states with `window.__office.setLoops()`
# rather than rewriting status files, so transitions are deterministic.
#
# Usage: tests/office/agents_check.sh [out-dir] [port]
# Screenshots are kept in out-dir (default: a fresh temp dir, printed at the end).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$(mktemp -d)}"
PORT="${2:-18432}"
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

python3 -I - "$REPO/tests/office" "$OFFICE_URL" "$OUT" <<'PY'
import json, sys, time
sys.path.insert(0, sys.argv[1])
from cdp import Chrome

url, out = sys.argv[2], sys.argv[3]
fails = []
SEAT_ROWS = (-4.5, -1.5, 2.5, 5.5)

def zone_ok(a):
    s, x, z = a["state"], a["x"], a["z"]
    if s in ("working", "failed"):
        return a["seated"] and x < -2 and any(abs(z - r) < 0.15 for r in SEAT_ROWS)
    if s == "idle":
        return x > 3
    if s == "waiting":
        return abs(z - 9.9) < 0.8 and -1.5 < x < 6
    if s == "disabled":
        return x < -13
    return False

def loop(name, state):
    return {"name": name, "title": name, "enabled": state != "disabled", "state": state, "reason": "",
            "last_run": None, "next_run": "", "history": [], "run_now_url": "", "toggle_url": "",
            "pending_approvals": 1 if state == "waiting" else 0}

def debug(c):
    return c.evaluate("window.__office.debug()")

def set_loops(c, loops):
    c.evaluate(f"window.__office.setLoops({json.dumps(loops)})")

def settle(c, label, timeout=150):
    """Waits until nobody walks and everyone is in their zone; records a failure otherwise."""
    try:
        c.wait_for("(() => { const d = window.__office.debug(); return d.every(a => !a.walking) && d; })()"
                   " && window.__office.debug().every(a => " + ZONE_JS + ")", timeout=timeout, interval=0.5)
    except TimeoutError:
        bad = [a for a in debug(c) if a["walking"] or not zone_ok(a)]
        fails.append(f"{label}: not settled in zone after {timeout}s: {bad[:5]}")

# The same zone rule in JS, for polling inside the page.
ZONE_JS = ("((a) => { const rows = [-4.5, -1.5, 2.5, 5.5];"
           " if (a.state === 'working' || a.state === 'failed') return a.seated && a.x < -2 && rows.some(r => Math.abs(a.z - r) < 0.15);"
           " if (a.state === 'idle') return a.x > 3;"
           " if (a.state === 'waiting') return Math.abs(a.z - 9.9) < 0.8 && a.x > -1.5 && a.x < 6;"
           " return a.x < -13; })(a)")

def load(c, hash_="h=13"):
    c.goto("about:blank")  # a hash-only change would not reload the page
    c.goto(f"{url}/loops?view=office#{hash_}")
    c.wait_for("window.__office && window.__office.ready && window.__office.syncs >= 1", timeout=60)

FIXTURE = {"gitlab-loop": "waiting", "busy-loop": "working", "broken-loop": "failed",
           "quiet-loop": "idle", "off-loop": "disabled"}

with Chrome() as c:
    try:
        load(c)
    except TimeoutError as e:
        print(f"FAIL\n  - page never became ready: {e}; console: {c.errors[:3]}")
        sys.exit(1)

    # 1. The sandbox's real /office/state: one person per loop, placed without walking.
    d = debug(c)
    names = {a["name"]: a for a in d}
    for name, state in FIXTURE.items():
        a = names.get(name)
        if not a:
            fails.append(f"fixture: no person for {name}")
            continue
        if a["state"] != state:
            fails.append(f"fixture: {name} is {a['state']}, expected {state}")
        if a["walking"]:
            fails.append(f"fixture: {name} walks on first load")
    settle(c, "fixture", timeout=20)
    c.pump(3)
    c.screenshot(f"{out}/fixture-13h.png")

    # 2. Transitions walk (not teleport) and end in the new zones.
    # Start from the server's own entries, so any other registered loop keeps its place.
    base = {l["name"]: l for l in c.evaluate("fetch('/office/state').then(r => r.json()).then(d => d.loops)")}
    after = dict(base)
    after.update({"busy-loop": loop("busy-loop", "failed"), "quiet-loop": loop("quiet-loop", "disabled"),
                  "gitlab-loop": loop("gitlab-loop", "working"), "broken-loop": loop("broken-loop", "idle"),
                  "off-loop": loop("off-loop", "waiting")})
    set_loops(c, list(after.values()))
    try:
        c.wait_for("window.__office.debug().some(a => a.name === 'quiet-loop' && a.walking)", timeout=5, interval=0.1)
    except TimeoutError:
        fails.append("transition: quiet-loop did not start walking (teleported?)")
    settle(c, "transition")
    c.pump(2)
    c.screenshot(f"{out}/after-13h.png")

    # 3. Rapid double change: a second change mid-walk, and two changes before any frame.
    set_loops(c, list({**after, "quiet-loop": loop("quiet-loop", "idle")}.values()))
    c.pump(0.6)
    set_loops(c, list({**after, "quiet-loop": loop("quiet-loop", "failed")}.values()))
    c.evaluate("window.__office.setLoops(" + json.dumps(list({**after, "quiet-loop": loop("quiet-loop", "failed"),
               "busy-loop": loop("busy-loop", "waiting")}.values())) + "); window.__office.setLoops(" +
               json.dumps(list({**after, "quiet-loop": loop("quiet-loop", "failed"),
               "busy-loop": loop("busy-loop", "disabled")}.values())) + ")")
    settle(c, "double change")
    final = {a["name"]: a for a in debug(c)}
    if final.get("quiet-loop", {}).get("state") != "failed" or final.get("busy-loop", {}).get("state") != "disabled":
        fails.append(f"double change: wrong final states {final}")

    # 4. 40 loops: 24 drawn, overflow 16 and the note says so.
    pattern = ["working", "idle", "failed", "disabled", "idle", "working", "idle", "working"]
    forty = [loop(f"loop-{i:02d}", "waiting" if i in (1, 9) else pattern[i % len(pattern)]) for i in range(40)]
    set_loops(c, forty)
    d = debug(c)
    if len(d) != 24:
        fails.append(f"40 loops: {len(d)} people drawn, expected 24")
    if c.evaluate("window.__office.overflow") != 16:
        fails.append(f"40 loops: overflow {c.evaluate('window.__office.overflow')}, expected 16")
    note = c.evaluate("(() => { const n = document.querySelector('#office .office-more'); return n && !n.hidden ? n.textContent : ''; })()")
    if "16" not in (note or ""):
        fails.append(f"40 loops: no 'and 16 more' note (got {note!r})")
    settle(c, "40 loops", timeout=30)
    c.pump(2)
    c.screenshot(f"{out}/forty-13h.png")

    # 4b. More loops than people: disabled ones are left out, so the 5 failed at the end show.
    many = [loop(f"dis-{i:02d}", "disabled") for i in range(30)] + [loop(f"bad-{i}", "failed") for i in range(5)] + \
           [loop(f"idle-{i}", "idle") for i in range(5)]
    set_loops(c, many)
    drawn = {a["name"] for a in debug(c)}
    missing = [f"bad-{i}" for i in range(5) if f"bad-{i}" not in drawn] + [f"idle-{i}" for i in range(5) if f"idle-{i}" not in drawn]
    if missing or len(drawn) != 24:
        fails.append(f"overflow: active loops hidden or wrong count ({len(drawn)} drawn, missing {missing})")
    if c.evaluate("window.__office.overflow") != 16:
        fails.append("overflow: wrong overflow count with 40 loops")

    # 5. Zero loops: an empty room, the note hidden.
    set_loops(c, [])
    c.pump(1)
    if debug(c) != []:
        fails.append("zero loops: people still drawn")
    if c.evaluate("(() => { const n = document.querySelector('#office .office-more'); return !!(n && !n.hidden); })()"):
        fails.append("zero loops: the overflow note is still shown")
    c.screenshot(f"{out}/empty-13h.png")

    # 6. A hostile name is canvas text, not HTML.
    before = c.evaluate("document.querySelectorAll('b').length")
    evil = '<b>"x"/é'
    set_loops(c, [loop(evil, "idle")])
    c.pump(1)
    d = debug(c)
    if [a["name"] for a in d] != [evil]:
        fails.append(f"hostile name: debug shows {[a['name'] for a in d]}")
    if c.evaluate("document.querySelectorAll('b').length") != before:
        fails.append("hostile name: a <b> element appeared in the DOM")

    if c.errors:
        fails.append("console errors: " + " | ".join(c.errors[:3]))

    # Inside camera, back to the sandbox fixture, for a closer look.
    load(c, "in&h=13")
    c.pump(4)
    c.screenshot(f"{out}/inside-13h.png")

# 8. A started run sends its person to the desk, even one too short to be seen as
# "working", and a finished one relaxes at the coffee machine or on a sofa.
SEATED_JS = ("((n) => { const a = window.__office.debug().find(a => a.name === n);"
             " return !!a && !a.walking && a.seated && a.x < -2; })")
with Chrome() as c:
    load(c)
    settle(c, "run start fixture", timeout=20)
    # 8a. Run now clicked while the loop still reads idle: walk to the desk at once.
    c.evaluate("window.__office.runRequested('quiet-loop')")
    try:
        c.wait_for("window.__office.debug().some(a => a.name === 'quiet-loop' && a.walking)", timeout=5, interval=0.1)
        c.wait_for(SEATED_JS + "('quiet-loop')", timeout=60, interval=0.5)
    except TimeoutError:
        fails.append(f"run requested: quiet-loop did not walk to its desk: {[a for a in debug(c) if a['name'] == 'quiet-loop']}")
    # 8b. The run flips to working and back to idle before the person arrives (chat run):
    # still walks to the desk, works a moment, then goes to coffee or a sofa.
    load(c)
    settle(c, "short run fixture", timeout=20)
    base = {l["name"]: l for l in c.evaluate("fetch('/office/state').then(r => r.json()).then(d => d.loops)")}
    set_loops(c, list({**base, "quiet-loop": loop("quiet-loop", "working")}.values()))
    c.pump(0.3)
    set_loops(c, list({**base, "quiet-loop": loop("quiet-loop", "idle")}.values()))
    try:
        c.wait_for(SEATED_JS + "('quiet-loop')", timeout=60, interval=0.5)
        c.wait_for("(() => { const a = window.__office.debug().find(a => a.name === 'quiet-loop');"
                   " return !a.walking && (a.at === 'coffee' || a.at === 'sofa'); })()", timeout=90, interval=0.5)
    except TimeoutError:
        fails.append(f"short run: quiet-loop did not sit at its desk and then relax: {[a for a in debug(c) if a['name'] == 'quiet-loop']}")
    if c.errors:
        fails.append("run start console errors: " + " | ".join(c.errors[:3]))

# 7. Reduced motion: a state change teleports, nobody ever walks.
with Chrome(reduced_motion=True) as c:
    load(c)
    set_loops(c, [loop(n, "working" if n == "quiet-loop" else s) for n, s in FIXTURE.items()])
    walked = False
    end = time.time() + 1.5
    while time.time() < end:
        walked = walked or any(a["walking"] for a in debug(c))
        c.pump(0.1)
    q = next((a for a in debug(c) if a["name"] == "quiet-loop"), None)
    if walked:
        fails.append("reduced motion: someone walked")
    if not q or not zone_ok(q):
        fails.append(f"reduced motion: quiet-loop not at a seat after the change: {q}")
    if c.errors:
        fails.append("reduced motion console errors: " + " | ".join(c.errors[:3]))

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: fixture zones, walking transitions, run start and relax, double change, 40 -> 24 + 16 more, empty room, hostile name, reduced motion")
PY
echo "screenshots in $OUT"

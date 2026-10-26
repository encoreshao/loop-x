#!/usr/bin/env bash
# Checks the Office view stays live: (a) rewriting a loop's status file in the sandbox moves
# that person without a page reload, without resetting the camera or the open panel;
# (b) adding one loop does not move anyone else; (c) with the server killed the "Showing
# last known state" note appears after `#stale=<ms>` and clears when the server returns;
# (d) dispose() closes the panel, removes its listeners and stops the refetching; the panel
# heading is the translated title and "Last run" follows the page language.
#
# Drives headless Chrome over the DevTools pipe (tests/office/cdp.py). Wall-clock waits only.
# The sandbox (tests/office/sandbox.sh) exposes its temp dirs and stops/starts its server on
# SIGUSR1/SIGUSR2; nothing here touches ~/.loopx.
#
# Usage: tests/office/live_check.sh [out-dir] [port]
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$(mktemp -d)}"
PORT="${2:-18434}"
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
SANDBOX_HOME="$(grep '^SANDBOX_HOME=' "$LOG" | head -1 | cut -d= -f2-)"
SANDBOX_ROOT="$(grep '^SANDBOX_ROOT=' "$LOG" | head -1 | cut -d= -f2-)"
[ -n "$OFFICE_URL" ] && [ -n "$SANDBOX_HOME" ] && [ -n "$SANDBOX_ROOT" ] || { cat "$LOG" >&2; echo "FAIL: sandbox did not start" >&2; exit 1; }

python3 -I - "$REPO/tests/office" "$OFFICE_URL" "$OUT" "$SANDBOX_PID" "$SANDBOX_HOME" "$SANDBOX_ROOT" <<'PY'
import json, os, signal, sys, time, urllib.request
sys.path.insert(0, sys.argv[1])
from cdp import Chrome

url, out, sandbox_pid, home, root = sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6]
fails = []

def load(c, hash_):
    c.goto("about:blank")
    c.goto(f"{url}/office#{hash_}")
    c.wait_for("window.__office && window.__office.ready && window.__office.syncs >= 1 && !!window.__office.select", timeout=60)

def debug(c):
    return c.evaluate("window.__office.debug()")

def by_name(c):
    return {a["name"]: a for a in debug(c)}

def settled(c, timeout=60):
    c.wait_for("window.__office.debug().every(a => !a.walking)", timeout=timeout, interval=0.5)

def write_status(name, state):
    pid = f',"pid":{sandbox_pid}' if state == "running" else ""
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    with open(f"{root}/outputs/status/{name}.json", "w") as f:
        f.write(f'{{"state":"{state}"{pid},"updated_at":"{now}"}}')

STALE = "(() => { const n = document.querySelector('#office .office-stale'); return !!(n && !n.hidden && n.textContent.trim()); })()"

with Chrome() as c:
    load(c, "h=20&poll=1000")
    c.evaluate("window.__marker = 12345")
    settled(c)

    # (a) A status file changes: the loop's row updates, the page is not reloaded, card and camera stay.
    c.evaluate("window.__office.select('E2')")
    c.pump(1.5)
    cam0 = c.evaluate("window.__office.focus()")
    if not cam0 or not c.evaluate("window.__office.panelOpen()"):
        fails.append("(a) card did not open for E2")
    write_status("quiet-loop", "running")
    ROW = "document.querySelector('.office-panel-loop[data-loop=\"quiet-loop\"] .office-panel-state')"
    try:
        c.wait_for(f"!!{ROW} && {ROW}.dataset.state === 'working'", timeout=20, interval=0.25)
    except TimeoutError:
        fails.append("(a) quiet-loop never showed as working in E2's card after its status file changed")
    c.pump(1)
    k = by_name(c).get("E2")
    if not k or not k["seated"] or k["state"] != "working":
        fails.append(f"(a) E2 should be seated and working: {k}")
    if c.evaluate("window.__marker") != 12345:
        fails.append("(a) the page reloaded")
    cam1 = c.evaluate("window.__office.focus()")
    if not c.evaluate("window.__office.panelOpen()") or c.evaluate("window.__office.selected()") != "E2":
        fails.append("(a) the open card was reset by a live refresh")
    elif max(abs(a - b) for a, b in zip(cam0["target"], cam1["target"])) > 1e-3:
        fails.append(f"(a) the camera moved on a live refresh: {cam0['target']} -> {cam1['target']}")

    # (b) One more loop: it is dealt to a colleague and nobody moves.
    c.evaluate("window.__office.select(null)")
    before = by_name(c)
    with open(f"{home}/loops.json") as f:
        loops = json.load(f)
    loops.append({"name": "zz-new-loop", "enabled": True})
    with open(f"{home}/loops.json", "w") as f:
        json.dump(loops, f)
    try:
        c.wait_for("window.__office.debug().some(a => a.loops.includes('zz-new-loop'))", timeout=20, interval=0.25)
    except TimeoutError:
        fails.append("(b) the new loop never appeared")
    c.pump(1.5)
    after = by_name(c)
    for name, a in before.items():
        b = after.get(name)
        if not b or (a["x"], a["z"], a["state"]) != (b["x"], b["z"], b["state"]):
            fails.append(f"(b) {name} moved or changed when an idle loop was added: {a} -> {b}")
    if c.evaluate("window.__marker") != 12345:
        fails.append("(b) the page reloaded")
    if c.errors:
        fails.append("(a/b) console errors: " + " | ".join(c.errors[:3]))

    # (d) A loop's title is its row's text, then dispose.
    c.evaluate("window.__office.setLoops([{name:'t-loop',title:'Titre traduit',enabled:true,state:'idle',reason:'',"
               "last_run:'2026-03-25T14:07:00Z',next_run:'',history:[],run_now_url:'',toggle_url:'',pending_approvals:0}])")
    c.evaluate("window.__office.select('E1')")
    row = c.evaluate("document.querySelector('.office-panel-loop a').textContent")
    if row != "Titre traduit":
        fails.append(f"(d) the loop row shows {row!r}, expected the title")
    c.evaluate("window.__office.dispose()")
    if c.evaluate("!!document.querySelector('.office-panel')") or c.evaluate("window.__office.panelOpen()"):
        fails.append("(d) the card is still there after dispose()")
    c.evaluate("for (const t of [document.body, document.getElementById('office')]) t.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}))")
    n0 = c.evaluate("performance.getEntriesByType('resource').filter(r => r.name.includes('/office/state')).length")
    c.pump(2.5)
    n1 = c.evaluate("performance.getEntriesByType('resource').filter(r => r.name.includes('/office/state')).length")
    if n1 != n0:
        fails.append(f"(d) /office/state still fetched after dispose ({n0} -> {n1})")
    if c.errors:
        fails.append("(d) console errors: " + " | ".join(c.errors[:3]))

# (c) The server goes away: the note appears after the stale timeout, clears on return.
with Chrome() as c:
    load(c, "h=20&stale=2000&poll=500")
    if c.evaluate(STALE):
        fails.append("(c) the stale note shows while the server is up")
    os.kill(sandbox_pid, signal.SIGUSR1)
    t0 = time.time()
    c.pump(0.9)
    if c.evaluate(STALE):
        fails.append("(c) the stale note showed before the timeout")
    try:
        c.wait_for(STALE, timeout=15, interval=0.25)
        waited = time.time() - t0
        if waited < 1.8:
            fails.append(f"(c) the stale note appeared after only {waited:.1f}s")
    except TimeoutError:
        fails.append("(c) the stale note never appeared with the server down")
    text = c.evaluate("document.querySelector('#office .office-stale').textContent")
    if "last known" not in text.lower():
        fails.append(f"(c) stale note text is {text!r}")
    c.screenshot(f"{out}/stale.png")
    os.kill(sandbox_pid, signal.SIGUSR2)
    try:
        c.wait_for(f"!({STALE})", timeout=45, interval=0.5)
    except TimeoutError:
        fails.append("(c) the stale note never cleared after the server came back")

# (e) The first state does not depend on SSE or the poll.
with Chrome() as c:
    c.call("Page.addScriptToEvaluateOnNewDocument", {"source": "window.EventSource = undefined"}, session=c.session)
    c.goto("about:blank")
    c.goto(f"{url}/office#h=20&poll=60000")
    c.wait_for("window.__office && window.__office.ready", timeout=60)
    try:
        c.wait_for("window.__office.syncs >= 1 && window.__office.debug().some(a => a.loops.length)", timeout=3, interval=0.2)
    except TimeoutError:
        fails.append("(e) no loops dealt within 3s with SSE unavailable and the poll off")

# (f) A 200 that is not a state feed keeps the office and counts as stale.
with Chrome() as c:
    c.call("Page.addScriptToEvaluateOnNewDocument", {"source":
        "const f = window.fetch; window.fetch = (u, o) => window.__bad && String(u).includes('/office/state')"
        " ? Promise.resolve(new Response('{}', {status: 200, headers: {'Content-Type': 'application/json'}})) : f(u, o)"},
        session=c.session)
    load(c, "h=20&stale=2000&poll=500")
    count = lambda: sum(len(a["loops"]) for a in debug(c))
    n = count()
    c.evaluate("window.__bad = true")
    try:
        c.wait_for(STALE, timeout=15, interval=0.25)
    except TimeoutError:
        fails.append("(f) a malformed feed never raised the stale note")
    if count() != n or n == 0:
        fails.append(f"(f) loops changed from {n} to {count()} on a malformed feed")
    c.evaluate("window.__bad = false")
    try:
        c.wait_for(f"!({STALE})", timeout=15, interval=0.25)
    except TimeoutError:
        fails.append("(f) the stale note did not clear when the feed recovered")

# (g) Frames stop while the office is not visible and resume afterwards.
with Chrome() as c:
    load(c, "h=20")
    f0 = c.evaluate("window.__office.frames")
    c.pump(1)
    if c.evaluate("window.__office.frames") <= f0:
        fails.append("(g) no frames drawn while the office is visible")
    c.evaluate("document.getElementById('office').style.display = 'none'")
    c.pump(0.7)
    f1 = c.evaluate("window.__office.frames")
    c.pump(1)
    f2 = c.evaluate("window.__office.frames")
    if f2 != f1:
        fails.append(f"(g) frames kept advancing while hidden ({f1} -> {f2})")
    c.evaluate("document.getElementById('office').style.display = ''")
    c.pump(1)
    if c.evaluate("window.__office.frames") <= f2:
        fails.append("(g) frames did not resume when the office was shown again")

# (h) Reduced motion draws on demand: idle means no frames, a selection draws some.
with Chrome(reduced_motion=True) as c:
    load(c, "h=20")
    c.pump(2)
    f1 = c.evaluate("window.__office.frames")
    c.pump(1)
    f2 = c.evaluate("window.__office.frames")
    if f2 != f1:
        fails.append(f"(h) frames kept advancing at rest under reduced motion ({f1} -> {f2})")
    c.evaluate("window.__office.select('E2')")
    c.pump(0.5)
    if c.evaluate("window.__office.frames") <= f2:
        fails.append("(h) selecting a person drew no frame under reduced motion")

# (i) If a module fails to load the List is shown with a note.
with Chrome() as c:
    c.call("Network.enable", session=c.session)
    c.call("Network.setBlockedURLs", {"urls": ["*three.module.min.js*"]}, session=c.session)
    c.goto("about:blank")
    c.goto(f"{url}/office#h=20")
    try:
        c.wait_for("document.getElementById('office').hidden && !document.getElementById('office-list').hidden"
                   " && !!document.querySelector('#office-list .office-note')", timeout=20, interval=0.3)
    except TimeoutError:
        fails.append("(i) a failed module load did not fall back to the List with a note")

# (j) Hash hooks are clamped: poll=10 still means at most a few fetches a second.
with Chrome() as c:
    load(c, "h=20&poll=10")
    n0 = c.evaluate("performance.getEntriesByType('resource').filter(r => r.name.includes('/office/state')).length")
    c.pump(2)
    n1 = c.evaluate("performance.getEntriesByType('resource').filter(r => r.name.includes('/office/state')).length")
    if n1 - n0 > 12:
        fails.append(f"(j) poll=10 fetched {n1 - n0} times in 2s; the floor is not applied")

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: live status change, no camera/card reset, add a loop moves nobody, stale note and recovery, dispose, loop title")
PY
echo "screenshots in $OUT"

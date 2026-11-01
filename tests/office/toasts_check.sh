#!/usr/bin/env bash
# Checks the Office view's bottom-left notifications: (a) a loop's status file going running
# (with a step), moving to another step, then idle shows one card for that loop at a time,
# in the bottom-left corner, ending as "finished"; (b) a card fades in; (c) at most ten are
# shown, the oldest leaving first; (d) the cards follow the theme: light and dark mode give
# different surfaces, and the accent colours a run's card; (e) finished cards fade away after
# their time, failures do not; (f) a click opens the loop's owner, the close button
# dismisses; (g) French text in French.
#
# Drives headless Chrome over the DevTools pipe (tests/office/cdp.py) against the disposable
# sandbox (tests/office/sandbox.sh); nothing here touches ~/.loopx.
#
# Usage: tests/office/toasts_check.sh [out-dir] [port]
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$(mktemp -d)}"
PORT="${2:-18436}"
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
SANDBOX_ROOT="$(grep '^SANDBOX_ROOT=' "$LOG" | head -1 | cut -d= -f2-)"
[ -n "$OFFICE_URL" ] && [ -n "$SANDBOX_ROOT" ] || { cat "$LOG" >&2; echo "FAIL: sandbox did not start" >&2; exit 1; }

python3 -I - "$REPO/tests/office" "$OFFICE_URL" "$OUT" "$SANDBOX_PID" "$SANDBOX_ROOT" <<'PY'
import json, sys, time
sys.path.insert(0, sys.argv[1])
from cdp import Chrome

url, out, sandbox_pid, root = sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
fails = []

def load(c, hash_, lang=None):
    c.goto("about:blank")
    if lang:
        c.call("Network.enable", session=c.session)
        c.call("Network.setExtraHTTPHeaders", {"headers": {"Accept-Language": lang}}, session=c.session)
    c.goto(f"{url}/office#{hash_}")
    c.wait_for("window.__office && window.__office.ready && window.__office.syncs >= 1 && !!window.__office.toasts", timeout=60)

def write_status(name, state, step=None):
    doc = {"state": state, "updated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    if state == "running":
        doc["pid"] = sandbox_pid
    if step:
        doc["current_step"] = step
    with open(f"{root}/outputs/status/{name}.json", "w") as f:
        json.dump(doc, f)

toasts = lambda c: c.evaluate("window.__office.toasts()")
mine = lambda c, loop: [t for t in toasts(c) if t["loop"] == loop]

def wait_toast(c, cond, what, timeout=20):
    try:
        c.wait_for(f"window.__office.toasts().some(t => {cond})", timeout=timeout, interval=0.25)
    except TimeoutError:
        fails.append(f"{what}: {toasts(c)}")

# Loops the page did not know (state S), as /office/state would send them.
MANY = "Array.from({length: 14}, (_, i) => ({name: 'm' + i, title: 'Loop ' + i, enabled: true, state: S, reason: '', " \
       "last_run: null, next_run: '', history: [], run_now_url: '', toggle_url: '', pending_approvals: 0, step: null}))"

with Chrome() as c:
    load(c, "h=13&poll=500&toast=60000")
    if toasts(c):
        fails.append(f"(a) the first state raised notifications: {toasts(c)}")

    # (a) running with a step, another step, then idle: one card for the loop at each point.
    write_status("quiet-loop", "running", "analyzing")
    wait_toast(c, "t.loop === 'quiet-loop' && t.kind === 'step' && t.text.includes('analyzing')", "(a) no step card for quiet-loop")
    # (b) It fades in: the first keyframe is transparent.
    first = c.evaluate("(() => { const a = document.querySelector('.app-toast[data-key=\"quiet-loop\"]').getAnimations()[0];"
                       " return a ? {name: a.animationName, from: a.effect.getKeyframes()[0].opacity} : null })()")
    if not first or first["name"] != "app-toast-in" or str(first["from"]) != "0":
        fails.append(f"(b) the card does not fade in: {first}")
    box = c.evaluate("(() => { const r = document.querySelector('.app-toast').getBoundingClientRect();"
                     " return {left: r.left, bottom: innerHeight - r.bottom} })()")
    if not (0 <= box["left"] <= 24 and 0 <= box["bottom"] <= 24):
        fails.append(f"(a) the card is not in the bottom-left corner: {box}")
    write_status("quiet-loop", "running", "testing")
    wait_toast(c, "t.loop === 'quiet-loop' && t.text.includes('testing')", "(a) the step update never showed")
    if len(mine(c, "quiet-loop")) != 1:
        fails.append(f"(a) a step update stacked a second card: {mine(c, 'quiet-loop')}")
    write_status("quiet-loop", "idle")
    wait_toast(c, "t.loop === 'quiet-loop' && t.kind === 'done'", "(a) no finished card")
    if len(mine(c, "quiet-loop")) != 1:
        fails.append(f"(a) the finished card did not replace the run's card: {mine(c, 'quiet-loop')}")
    c.screenshot(f"{out}/dark.png")

    # (d) Theme: light and dark surfaces differ; the accent tints a run's card.
    bg = "getComputedStyle(document.querySelector('.app-toast')).backgroundColor"
    c.evaluate("document.documentElement.setAttribute('data-color-mode', 'dark')")
    dark = c.evaluate(bg)
    c.evaluate("document.documentElement.setAttribute('data-color-mode', 'light')")
    light = c.evaluate(bg)
    if dark == light:
        fails.append(f"(d) light and dark mode give the same card surface: {dark}")
    c.screenshot(f"{out}/light.png")
    write_status("quiet-loop", "running")
    wait_toast(c, "t.loop === 'quiet-loop' && t.kind === 'started'", "(d) no started card")
    tint = "getComputedStyle(document.querySelector('.app-toast[data-kind=\"started\"]')).borderLeftColor"
    c.evaluate("document.documentElement.setAttribute('data-accent', 'indigo')")
    indigo = c.evaluate(tint)
    c.evaluate("document.documentElement.setAttribute('data-accent', 'green')")
    green = c.evaluate(tint)
    if indigo == green:
        fails.append(f"(d) the accent does not colour the card: {indigo}")

    # (f) A click opens the loop's owner; the close button dismisses.
    c.evaluate("document.querySelector('.app-toast[data-key=\"quiet-loop\"]').click()")
    c.pump(0.5)
    owner = c.evaluate("window.__office.debug().find(a => a.loops.includes('quiet-loop'))?.name")
    if not c.evaluate("window.__office.panelOpen()") or c.evaluate("window.__office.selected()") != owner:
        fails.append(f"(f) the click did not open {owner}'s card")
    c.evaluate("window.__office.select(null)")
    if len(mine(c, "quiet-loop")) != 1:
        fails.append(f"(d) the new run's card did not replace the finished one: {mine(c, 'quiet-loop')}")
    c.evaluate("document.querySelector('.app-toast[data-key=\"quiet-loop\"] .app-toast-close').click()")
    c.pump(0.6)
    if mine(c, "quiet-loop") or c.evaluate("!!document.querySelector('.app-toast[data-key=\"quiet-loop\"]')"):
        fails.append("(f) the close button did not dismiss the card")

    # (c) Fourteen runs start at once: ten cards, the newest ones.
    c.evaluate(f"(() => {{ let S = 'idle'; window.__office.setLoops({MANY}); }})()")
    c.evaluate(f"(() => {{ let S = 'working'; window.__office.setLoops({MANY}); }})()")
    c.pump(0.6)
    shown = toasts(c)
    in_dom = c.evaluate("document.querySelectorAll('.app-toast').length")
    if len(shown) != 10 or in_dom != 10:
        fails.append(f"(c) expected 10 cards, got {len(shown)} ({in_dom} in the page)")
    elif shown[0]["loop"] != "m4" or shown[-1]["loop"] != "m13":
        fails.append(f"(c) the oldest did not leave first: {[t['loop'] for t in shown]}")
    c.screenshot(f"{out}/ten.png")
    if c.errors:
        fails.append("console errors: " + " | ".join(c.errors[:3]))

# (e) Finished cards fade away after their time; a failure stays until dismissed.
with Chrome() as c:
    load(c, "h=13&toast=1500")
    c.evaluate(f"(() => {{ let S = 'working'; window.__office.setLoops({MANY}.slice(0, 2)); }})()")
    c.evaluate(f"(() => {{ let S = 'working'; window.__office.setLoops({MANY}.slice(0, 2)); }})()")
    c.evaluate(f"(() => {{ let S = 'idle'; const l = {MANY}.slice(0, 2); l[1].state = 'failed'; window.__office.setLoops(l); }})()")
    c.pump(0.5)
    kinds = sorted(t["kind"] for t in toasts(c))
    if kinds != ["done", "failed"]:
        fails.append(f"(e) expected a done and a failed card: {kinds}")
    c.pump(2.5)
    left = [t["kind"] for t in toasts(c)]
    if left != ["failed"]:
        fails.append(f"(e) after the time only the failure should remain: {left}")

# (g) French.
with Chrome() as c:
    load(c, "h=13&poll=500", lang="fr")
    c.evaluate(f"(() => {{ let S = 'idle'; window.__office.setLoops({MANY}.slice(0, 1)); }})()")
    c.evaluate(f"(() => {{ let S = 'idle'; const l = {MANY}.slice(0, 1); l[0].state = 'waiting'; window.__office.setLoops(l); }})()")
    c.pump(0.4)
    t = toasts(c)
    if not t or t[0]["text"] != "Loop 0 attend votre approbation":
        fails.append(f"(g) French text expected: {t}")
    label = c.evaluate("document.querySelector('.app-toast-close')?.getAttribute('aria-label')")
    if label != "Ignorer":
        fails.append(f"(g) the dismiss label is {label!r}")

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: bottom-left cards per run, fade-in, ten at most, theme and light/dark, expiry, click and dismiss, French")
PY
echo "screenshots in $OUT"

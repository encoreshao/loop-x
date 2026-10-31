#!/usr/bin/env bash
# Checks the Office colleagues: always exactly three (E1, E2, E3) seated 120 degrees
# apart around the round desk whatever the loop count, every loop dealt to exactly one of
# them, their state following their loops (working > waiting > failed > idle), a chat
# question putting one randomly picked colleague to work until it is answered, idle
# colleagues talking to each other (and stopping when one gets work), the details card
# listing a colleague's loops, a hostile loop name staying canvas text, and nobody talking
# under reduced motion; during the workday (09-17h) one idle colleague at a time walks to the
# coffee shop or the bookstore, stays and comes back, and returns at once when given work, while
# nobody goes out after hours. No console errors.
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
import json, math, sys, time
sys.path.insert(0, sys.argv[1])
from cdp import Chrome

url, out = sys.argv[2], sys.argv[3]
fails = []
NAMES = ["E1", "E2", "E3"]
DESK = (-8.0, 1.0, 3.6)

def loop(name, state):
    return {"name": name, "title": name, "enabled": state != "disabled", "state": state, "reason": "",
            "last_run": None, "next_run": "", "history": [], "run_now_url": "", "toggle_url": "",
            "pending_approvals": 1 if state == "waiting" else 0}

def debug(c):
    return c.evaluate("window.__office.debug()")

def set_loops(c, loops):
    c.evaluate(f"window.__office.setLoops({json.dumps(loops)})")

def load(c, hash_="h=20"):
    c.goto("about:blank")  # a hash-only change would not reload the page
    c.goto(f"{url}/office#{hash_}")
    c.wait_for("window.__office && window.__office.ready && window.__office.syncs >= 1", timeout=60)

def check_three(d, label):
    if [a["name"] for a in d] != NAMES:
        fails.append(f"{label}: people are {[a['name'] for a in d]}, expected {NAMES}")
        return
    for a in d:
        r = math.hypot(a["x"] - DESK[0], a["z"] - DESK[1])
        if abs(r - DESK[2]) > 0.05 or not a["seated"] or a["walking"]:
            fails.append(f"{label}: {a['name']} is not seated at the desk: {a}")
    angles = sorted(math.degrees(math.atan2(a["z"] - DESK[1], a["x"] - DESK[0])) % 360 for a in d)
    gaps = [(angles[(i + 1) % 3] - angles[i]) % 360 for i in range(3)]
    if any(abs(g - 120) > 1 for g in gaps):
        fails.append(f"{label}: seats are not spaced evenly around the desk: gaps {gaps}")

def expected_state(loops):
    states = [l["state"] for l in loops]
    for s in ("working", "waiting", "failed"):
        if s in states:
            return s
    return "idle"

FIXTURE = {"gitlab-loop": "waiting", "busy-loop": "working", "broken-loop": "failed",
           "quiet-loop": "idle", "off-loop": "disabled"}

with Chrome() as c:
    try:
        load(c)
    except TimeoutError as e:
        print(f"FAIL\n  - page never became ready: {e}; console: {c.errors[:3]}")
        sys.exit(1)

    # 1. The sandbox's real /office/state: three people, every loop with exactly one of them.
    d = debug(c)
    check_three(d, "fixture")
    dealt = sorted(n for a in d for n in a["loops"])
    if dealt != sorted(FIXTURE):
        fails.append(f"fixture: loops dealt {dealt}, expected {sorted(FIXTURE)}")
    state_of = {}
    for a in d:
        mine = [dict(name=n, state=FIXTURE[n]) for n in a["loops"]]
        if a["state"] != expected_state(mine):
            fails.append(f"fixture: {a['name']} is {a['state']}, expected {expected_state(mine)} for {a['loops']}")
    c.pump(3)
    c.screenshot(f"{out}/fixture-13h.png")

    # 2. Any loop count keeps three people; loops are dealt out evenly.
    set_loops(c, [])
    c.pump(0.5)
    d = debug(c)
    check_three(d, "zero loops")
    if any(a["state"] != "idle" or a["loops"] for a in d):
        fails.append(f"zero loops: expected three idle people with no loops, got {d}")
    forty = [loop(f"loop-{i:02d}", "working" if i == 7 else "idle") for i in range(40)]
    set_loops(c, forty)
    d = debug(c)
    check_three(d, "40 loops")
    sizes = sorted(len(a["loops"]) for a in d)
    if sizes != [13, 13, 14]:
        fails.append(f"40 loops: loops dealt {sizes}, expected [13, 13, 14]")
    if c.evaluate("window.__office.overflow") != 0:
        fails.append("40 loops: nothing should overflow")
    owner = next(a for a in d if "loop-07" in a["loops"])
    if owner["state"] != "working" or [a["state"] for a in d].count("working") != 1:
        fails.append(f"40 loops: only the owner of the running loop should work: {d}")

    # 3. A chat question puts one random colleague to work; the answer ends it.
    def all_idle(label):
        try:
            # Someone who just answered a question keeps typing a few seconds.
            c.wait_for("window.__office.debug().every(a => a.state === 'idle')", timeout=10, interval=0.25)
        except TimeoutError:
            fails.append(f"{label}: colleagues never went back to idle: {debug(c)}")

    def asked():
        return [a["name"] for a in debug(c) if a["chat"]]

    set_loops(c, [loop("a-loop", "idle"), loop("b-loop", "idle"), loop("c-loop", "idle")])
    all_idle("chat")
    seen = set()
    for _ in range(40):
        c.evaluate("document.dispatchEvent(new CustomEvent('loopx:chat', {detail: {busy: true}}))")
        who = asked()
        if len(who) != 1 or next(a for a in debug(c) if a["name"] == who[0])["state"] != "working":
            fails.append(f"chat: expected exactly one person working on the question, got {who}: {debug(c)}")
            break
        seen.add(who[0])
        c.evaluate("document.dispatchEvent(new CustomEvent('loopx:chat', {detail: {busy: false}}))")
        if asked():
            fails.append("chat: the question is still assigned after the answer")
            break
    if len(seen) < 2:
        fails.append(f"chat: the picked colleague is not random: always {seen}")
    all_idle("chat answered")
    c.evaluate("document.dispatchEvent(new CustomEvent('loopx:chat', {detail: {busy: true}}))")
    first = asked()
    c.evaluate("document.dispatchEvent(new CustomEvent('loopx:tool', {detail: {name: 'gitlab.issue'}}))")
    if asked() != first or [a["state"] for a in debug(c)].count("working") != 1:
        fails.append("chat: a tool call must not put a second colleague to work")
    c.evaluate("document.dispatchEvent(new CustomEvent('loopx:chat', {detail: {busy: false}}))")
    all_idle("chat tool call")
    # The question goes to a free colleague, not the one already running a loop (E1 owns a-loop).
    set_loops(c, [loop("a-loop", "working"), loop("b-loop", "idle"), loop("c-loop", "idle")])
    for _ in range(12):
        c.evaluate("window.__office.chatBusy(true)")
        if asked() == ["E1"] or len(asked()) != 1:
            fails.append(f"chat: the question should go to a free colleague, not E1 who runs a loop: {asked()}")
            break
        c.evaluate("window.__office.chatBusy(false)")
    set_loops(c, [loop("a-loop", "idle"), loop("b-loop", "idle"), loop("c-loop", "idle")])
    all_idle("chat free colleague")

    # 4. Idle colleagues talk to each other, and stop when one gets work.
    set_loops(c, [loop("a-loop", "idle"), loop("b-loop", "idle"), loop("c-loop", "idle")])
    if not c.evaluate("window.__office.talkNow()"):
        fails.append("talk: two idle colleagues did not start talking")
    c.pump(1)
    talkers = [a for a in debug(c) if a["talking"]]
    if len(talkers) != 2 or not any(a["said"] for a in talkers):
        fails.append(f"talk: expected two talking and one speech bubble, got {talkers}")
    c.screenshot(f"{out}/talking-13h.png")
    names = [a["name"] for a in talkers] or ["E1"]
    busy = [l for l in ("a-loop", "b-loop", "c-loop")]
    owner = next(a for a in debug(c) if a["name"] == names[0])["loops"]
    set_loops(c, [loop(n, "working" if n in owner else "idle") for n in busy])
    c.pump(0.5)
    if any(a["talking"] for a in debug(c)):
        fails.append("talk: the conversation went on after a participant got work")
    for a in debug(c):
        if a["state"] != "idle" and a["said"]:
            fails.append(f"talk: {a['name']} still shows a speech bubble while working")

    # 5. The details card: the colleague's name, state and a row per loop.
    set_loops(c, [loop("a-loop", "idle"), loop("b-loop", "failed"), loop("c-loop", "waiting"), loop("d-loop", "idle")])
    c.evaluate("window.__office.select('E1')")
    c.pump(0.5)
    card = c.evaluate("(() => { const p = document.querySelector('.office-panel'); return p && { name: p.querySelector('.office-panel-name').textContent,"
                      " rows: [...p.querySelectorAll('.office-panel-loop')].map(r => r.dataset.loop) }; })()")
    mine = next(a for a in debug(c) if a["name"] == "E1")["loops"]
    if not card or card["name"] != "E1" or card["rows"] != mine:
        fails.append(f"card: expected E1 with loops {mine}, got {card}")
    c.evaluate("window.__office.select(null)")

    # 6. A hostile name is canvas text, not HTML.
    before = c.evaluate("document.querySelectorAll('b').length")
    evil = '<b>"x"/\u00e9'
    set_loops(c, [loop(evil, "idle")])
    c.pump(1)
    if sum(len(a["loops"]) for a in debug(c)) != 1 or c.evaluate("document.querySelectorAll('b').length") != before:
        fails.append("hostile name: a <b> element appeared in the DOM or the loop was lost")

    if c.errors:
        fails.append("console errors: " + " | ".join(c.errors[:3]))

    load(c, "in&h=20")
    c.pump(4)
    c.screenshot(f"{out}/inside-13h.png")

# 6b. Trips to the shops: only in the workday, one at a time, and back at once for work.
SHOP = {"coffee": (-78.0, -70.0), "books": (-66.0, -58.0)}
def idle_loops():
    return [loop("a-loop", "idle"), loop("b-loop", "idle"), loop("c-loop", "idle")]

with Chrome() as c:
    load(c, "h=10")
    set_loops(c, idle_loops())
    # Nobody has gone out yet, and the schedule sends someone by itself (first trip ~12 s).
    if any(a["away"] for a in debug(c)):
        fails.append("trips: someone was away at the start")
    try:
        c.wait_for("window.__office.debug().some(a => a.away)", timeout=40, interval=0.5)
    except TimeoutError:
        fails.append("trips: nobody went out by themselves during the workday")
    away = [a for a in debug(c) if a["away"]]
    if len(away) != 1:
        fails.append(f"trips: expected exactly one colleague away, got {away}")
    if c.evaluate("window.__office.visitNow('books')"):
        fails.append("trips: a second colleague was sent out while one is away")
    who = away[0]["name"] if away else None
    place = away[0]["away"] if away else "coffee"
    lo, hi = SHOP[place]
    # They stand inside the shop, idle, with the label saying so.
    try:
        c.wait_for(f"window.__office.debug().some(a => a.name === {json.dumps(who)} && a.at === {json.dumps(place)})", timeout=60, interval=0.5)
    except TimeoutError:
        fails.append(f"trips: {who} never reached the {place} shop: {debug(c)}")
    me = next((a for a in debug(c) if a["name"] == who), None)
    if me and not (lo < me["x"] < hi and 5.5 < me["z"] < 12.5 and me["state"] == "idle" and not me["seated"]):
        fails.append(f"trips: {who} is not standing idle inside the {place} shop: {me}")
    c.evaluate(f"window.__office.select({json.dumps(who)})")
    card = c.evaluate("(() => { const n = document.querySelector('.office-panel-note'); return n && !n.hidden ? n.textContent : ''; })()")
    want = "At the coffee shop" if place == "coffee" else "At the bookstore"
    if card != want:
        fails.append(f"trips: the card note is {card!r}, expected {want!r}")
    c.evaluate("window.__office.select(null)")
    c.screenshot(f"{out}/shop-visit-10h.png")
    # Work calls them back at once: they walk home and sit down, and the trip is over.
    owner = me["loops"] if me else []
    set_loops(c, [loop(n, "working" if n in owner else "idle") for n in ("a-loop", "b-loop", "c-loop")])
    try:
        c.wait_for(f"window.__office.debug().some(a => a.name === {json.dumps(who)} && a.walking)", timeout=5, interval=0.2)
        c.wait_for(f"window.__office.debug().some(a => a.name === {json.dumps(who)} && a.seated && !a.walking && !a.away)", timeout=90, interval=0.5)
    except TimeoutError:
        fails.append(f"trips: {who} did not walk back to the desk when given work: {debug(c)}")
    set_loops(c, idle_loops())
    # A bookstore visit ends by itself (stay 12-20 s) and the colleague sits back down.
    try:
        # visitNow only sends someone who is free (not out strolling), so retry until it does.
        c.wait_for("window.__office.visitNow('books')", timeout=60, interval=0.5)
        c.wait_for("window.__office.debug().some(a => a.at === 'books')", timeout=60, interval=0.5)
        c.screenshot(f"{out}/bookstore-10h.png")
        c.wait_for("window.__office.debug().every(a => !a.away)", timeout=120, interval=0.5)
    except TimeoutError:
        fails.append(f"trips: a bookstore visit did not start and end: {debug(c)}")
    if c.errors:
        fails.append("trips console errors: " + " | ".join(c.errors[:3]))

with Chrome() as c:
    # After hours nobody goes out by themselves.
    load(c, "h=20")
    set_loops(c, idle_loops())
    c.pump(20)
    if any(a["away"] for a in debug(c)):
        fails.append("trips: someone went out after hours")

# 6c. The user assigns colleagues from the card, at any hour (here after hours).
with Chrome() as c:
    load(c, "h=20")
    set_loops(c, idle_loops())
    def press(label):
        return c.evaluate("(() => { const b = [...document.querySelectorAll('.office-panel-assign button')].find(x => x.textContent === %s); if (!b) return false; b.click(); return true; })()" % json.dumps(label))
    c.evaluate("window.__office.select('E1')")
    if not press("Coffee shop"):
        fails.append("assign: the card has no Coffee shop button")
    try:
        c.wait_for("window.__office.debug().find(a => a.name === 'E1').at === 'coffee'", timeout=60, interval=0.5)
    except TimeoutError:
        fails.append(f"assign: E1 did not walk to the coffee shop after hours: {debug(c)}")
    if not press("Call back to desk"):
        fails.append("assign: no Call back button while E1 is away")
    try:
        c.wait_for("window.__office.debug().some(a => a.name === 'E1' && !a.away && a.seated && !a.walking)", timeout=90, interval=0.5)
    except TimeoutError:
        fails.append(f"assign: E1 did not come back when called: {debug(c)}")
    # A conversation about a typed topic, in both directions' lines.
    c.evaluate("window.__office.select('E1')")
    c.evaluate("(() => { const i = document.querySelector('.office-panel-topic'); i.value = 'quantum gardening'; })()")
    if not press("Chat with E2"):
        fails.append("assign: the card has no Chat with E2 button")
    try:
        c.wait_for("window.__office.debug().some(a => a.talking && (a.said || '').includes('quantum gardening'))", timeout=10, interval=0.25)
    except TimeoutError:
        fails.append(f"assign: nobody spoke about the typed topic: {debug(c)}")
    if not [a["name"] for a in debug(c) if a["talking"]] == ["E1", "E2"]:
        fails.append(f"assign: the talkers are not E1 and E2: {debug(c)}")
    c.screenshot(f"{out}/assigned-talk.png")
    # A busy colleague cannot be sent out: the card says so and nobody moves.
    set_loops(c, [loop("a-loop", "working"), loop("b-loop", "idle"), loop("c-loop", "idle")])
    owner = next(a["name"] for a in debug(c) if "a-loop" in a["loops"])
    c.evaluate(f"window.__office.select({json.dumps(owner)})")
    press("Bookstore")
    msg = c.evaluate("(() => { const m = document.querySelector('.office-panel-assign-msg'); return m && !m.hidden ? m.textContent : ''; })()")
    if "busy" not in msg or any(a["away"] for a in debug(c)):
        fails.append(f"assign: a busy colleague was sent out or no notice shown: {msg!r}")
    if c.errors:
        fails.append("assign console errors: " + " | ".join(c.errors[:3]))

# 6d. Ctrl+V flips between the Office and the Dashboard, but not while typing.
with Chrome() as c:
    load(c, "h=20")
    ctrl_v = "document.dispatchEvent(new KeyboardEvent('keydown', {key: 'v', ctrlKey: true, bubbles: true, cancelable: true}))"
    c.evaluate("(() => { const t = document.querySelector('#ai-panel textarea, #ai-panel input[name=text]'); t.addEventListener('keydown', e => e.stopPropagation(), false); })()")
    c.evaluate("document.querySelector('#ai-panel [name=text]').dispatchEvent(new KeyboardEvent('keydown', {key: 'v', ctrlKey: true, bubbles: true, cancelable: true}))")
    c.pump(1)
    if "/office" not in c.evaluate("location.pathname"):
        fails.append("ctrl+v: typing in the chat box left the Office")
    c.evaluate(ctrl_v)
    try:
        c.wait_for("location.search.includes('mode=dashboard')", timeout=15, interval=0.25)
    except TimeoutError:
        fails.append("ctrl+v: did not go from the Office to the Dashboard")
    else:
        c.wait_for("document.readyState === 'complete' && !!document.getElementById('ai-panel')", timeout=30)
        c.pump(1)
        c.evaluate(ctrl_v)
        try:
            c.wait_for("location.pathname === '/office'", timeout=15, interval=0.25)
        except TimeoutError:
            fails.append("ctrl+v: did not go from the Dashboard back to the Office")

# 6d. With no work the idle colleagues stroll round the office (any hour, never leaving it,
# never walking through the desk), several of them at once and at varying speeds, and work calls
# them back to the desk.
with Chrome() as c:
    load(c, "h=20")
    set_loops(c, idle_loops())
    try:
        c.wait_for("window.__office.debug().some(a => a.at === 'roam')", timeout=70, interval=0.5)
    except TimeoutError:
        fails.append(f"roam: nobody strolled round the office with no work: {debug(c)}")
    else:
        pts, speeds, most, last = [], [], 0, {}
        for _ in range(240):
            c.pump(0.5)
            d = debug(c)
            most = max(most, sum(1 for a in d if a["at"] == "roam"))
            for a in d:
                if a["at"] == "roam" or a["walking"]:
                    pts.append((a["x"], a["z"]))
                    if a["name"] in last and a["walking"]:
                        speeds.append(math.hypot(a["x"] - last[a["name"]][0], a["z"] - last[a["name"]][1]) / 0.5)
                last[a["name"]] = (a["x"], a["z"])
        out = [p for p in pts if not (-23.4 < p[0] < 23.4 and -9.2 < p[1] < 12.4)]
        if out:
            fails.append(f"roam: someone left the office: {out[:3]}")
        if any(math.hypot(x - DESK[0], z - DESK[1]) < 3.3 for x, z in pts if abs(math.hypot(x - DESK[0], z - DESK[1]) - DESK[2]) > 0.3 and math.hypot(x - DESK[0], z - DESK[1]) < 4.3):
            fails.append("roam: someone walked through the desk")
        if most < 2:
            fails.append("roam: never more than one colleague strolled at a time")
        if speeds and max(speeds) - min(s for s in speeds if s > 0.2) < 1.0:
            fails.append(f"roam: the walking pace never varied: {min(speeds):.2f}..{max(speeds):.2f} m/s")
        who = next((a for a in debug(c) if a["at"] == "roam"), None)
        if who:
            if who["away"]:
                fails.append(f"roam: a stroll should not count as a shop trip: {who}")
            set_loops(c, [loop(n, "working" if n in who["loops"] else "idle") for n in ("a-loop", "b-loop", "c-loop")])
            try:
                c.wait_for(f"window.__office.debug().some(a => a.name === {json.dumps(who['name'])} && a.seated && !a.walking && a.at === 'desk')", timeout=60, interval=0.5)
            except TimeoutError:
                fails.append(f"roam: {who['name']} did not go back to the desk when given work: {debug(c)}")
    if c.errors:
        fails.append("roam console errors: " + " | ".join(c.errors[:3]))

# 7. Reduced motion: nobody talks, a state change shows at once, no errors.
with Chrome(reduced_motion=True) as c:
    load(c)
    check_three(debug(c), "reduced motion")
    set_loops(c, [loop("a-loop", "idle"), loop("b-loop", "idle"), loop("c-loop", "idle")])
    c.pump(6)
    if any(a["talking"] for a in debug(c)):
        fails.append("reduced motion: someone talked")
    if c.evaluate("window.__office.visitNow('coffee')"):
        fails.append("reduced motion: someone was sent to a shop")
    c.evaluate("window.__office.chatBusy(true, 'E2')")
    c.pump(0.3)
    if next(a for a in debug(c) if a["name"] == "E2")["state"] != "working":
        fails.append("reduced motion: the chat did not put E2 to work")
    if c.errors:
        fails.append("reduced motion console errors: " + " | ".join(c.errors[:3]))

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: three seats around the round desk, loops dealt out, states from loops, random chat worker, talking, details card, hostile name, shop trips in the workday only, reduced motion")
PY
echo "screenshots in $OUT"

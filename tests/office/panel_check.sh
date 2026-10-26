#!/usr/bin/env bash
# Checks the Office details card of a colleague: selecting E1, E2 or E3 opens a card
# with their name, translated state and one row per loop (link, state, Run now,
# Enable/Disable); Escape and a click on empty space close it and return focus; a real
# click on a person (projected to screen) selects them and the camera eases to them;
# hover turns the cursor to a pointer; French shows French labels; a hostile loop name
# never becomes HTML and its link is percent-encoded; Enable/Disable really toggle the
# sandbox loop and the card follows; Run now POSTs to run_now_url with a CSRF token
# (recorded, never sent, so no real run launches) and puts the loop's owner to work; a
# failed action shows "Action failed".
#
# Drives headless Chrome over the DevTools pipe (tests/office/cdp.py). Screenshots
# are kept in out-dir (default: a fresh temp dir, printed at the end).
#
# Usage: tests/office/panel_check.sh [out-dir] [port]
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$(mktemp -d)}"
PORT="${2:-18433}"
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
import json, sys
sys.path.insert(0, sys.argv[1])
from cdp import Chrome

url, out = sys.argv[2], sys.argv[3]
fails = []

def load(c, lang=None):
    c.goto("about:blank")
    if lang:
        c.goto(f"{url}/office#h=20")
        c.wait_for("document.readyState === 'complete'", timeout=30)
        c.evaluate(f"document.cookie = 'loop_lang={lang}; path=/'")
        c.goto("about:blank")
    c.goto(f"{url}/office#h=20")
    c.wait_for("window.__office && window.__office.ready && window.__office.syncs >= 1 && !!window.__office.select", timeout=60)

def js(c, expr):
    return c.evaluate(expr)

PANEL = "document.querySelector('#office .office-panel')"
def row(name):
    return f"{PANEL}.querySelector('.office-panel-loop[data-loop=' + {json.dumps(json.dumps(name))} + ']')"

def panel_open(c):
    return js(c, "window.__office.panelOpen()")

def read_card(c):
    return js(c, f"""(() => {{ const p = {PANEL}; if (!p) return null;
      const q = (s) => p.querySelector(s);
      return {{name: q('.office-panel-name').textContent, chip: q('.office-panel-state').textContent,
        note: !q('.office-panel-note').hidden, empty: !q('.office-panel-empty').hidden,
        rows: [...p.querySelectorAll('.office-panel-loop')].map(r => ({{
          loop: r.dataset.loop, href: r.querySelector('a').getAttribute('href'), text: r.querySelector('a').textContent,
          chip: r.querySelector('.office-panel-state').textContent,
          runDisabled: r.querySelector('[data-act=run]').getAttribute('aria-disabled') === 'true',
          runText: r.querySelector('[data-act=run]').textContent, toggleText: r.querySelector('[data-act=toggle]').textContent}})),
        focusInside: p.contains(document.activeElement), error: !q('.office-panel-error').hidden,
        errorText: q('.office-panel-error').textContent, html: p.innerHTML}}; }})()""")

def wait_open(c, label):
    try:
        c.wait_for("window.__office.panelOpen()", timeout=5, interval=0.1)
    except TimeoutError:
        fails.append(f"{label}: card did not open")
        return False
    return True

def escape(c):
    for t in ("keyDown", "keyUp"):
        c.call("Input.dispatchKeyEvent", {"type": t, "key": "Escape", "code": "Escape", "windowsVirtualKeyCode": 27}, session=c.session)

def mouse(c, typ, x, y, **kw):
    c.call("Input.dispatchMouseEvent", {"type": typ, "x": x, "y": y, "button": "left", "buttons": 1 if typ == "mousePressed" else 0,
                                        "clickCount": 1, **kw}, session=c.session)

def click(c, x, y):
    c.call("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": y}, session=c.session)
    mouse(c, "mousePressed", x, y)
    mouse(c, "mouseReleased", x, y)

# The sandbox's five loops, dealt out in name order to E1, E2 and E3.
FIXTURE = {"gitlab-loop": "waiting", "busy-loop": "working", "broken-loop": "failed",
           "quiet-loop": "idle", "off-loop": "disabled"}
DEALT = {"E1": ["broken-loop", "off-loop"], "E2": ["busy-loop", "quiet-loop"], "E3": ["gitlab-loop"]}
STATE = {"E1": "failed", "E2": "working", "E3": "waiting"}

with Chrome() as c:
    try:
        load(c)
    except TimeoutError as e:
        print(f"FAIL\n  - page never became ready (is window.__office.select exposed?): {e}; console: {c.errors[:3]}")
        sys.exit(1)
    i18n = json.loads(js(c, "document.getElementById('office-i18n').textContent"))
    csrf = js(c, "document.getElementById('office-csrf').value")
    if not csrf:
        fails.append("no csrf token in the page")

    # 1. Selecting each colleague shows their card with a row per loop.
    for who, loops in DEALT.items():
        js(c, f"window.__office.select({json.dumps(who)})")
        if not wait_open(c, who):
            continue
        p = read_card(c)
        if p["name"] != who:
            fails.append(f"{who}: card heading {p['name']!r}")
        if p["chip"].strip() != i18n["state_" + STATE[who]]:
            fails.append(f"{who}: chip {p['chip']!r}, expected {i18n['state_' + STATE[who]]!r}")
        if [r["loop"] for r in p["rows"]] != loops:
            fails.append(f"{who}: rows {[r['loop'] for r in p['rows']]}, expected {loops}")
        for r in p["rows"]:
            state = FIXTURE[r["loop"]]
            if r["chip"].strip() != i18n["state_" + state]:
                fails.append(f"{who}/{r['loop']}: chip {r['chip']!r}")
            if r["runDisabled"] != (state in ("working", "disabled")):
                fails.append(f"{who}/{r['loop']}: Run now disabled={r['runDisabled']} for a {state} loop")
            if r["runText"] != i18n["run_now"] or r["toggleText"] != i18n["enable" if state == "disabled" else "disable"]:
                fails.append(f"{who}/{r['loop']}: button texts {r['runText']!r} / {r['toggleText']!r}")
            if r["href"] != f"/loops/{r['loop']}":
                fails.append(f"{who}/{r['loop']}: link {r['href']!r}")
        if not p["focusInside"]:
            fails.append(f"{who}: focus did not move into the card")
        if p["error"] or p["note"] or p["empty"]:
            fails.append(f"{who}: unexpected error/note/empty line: {p['error'], p['note'], p['empty']}")
    c.screenshot(f"{out}/card-en.png")

    # 1b. A colleague with no loops, and one answering a question.
    js(c, "window.__office.setLoops([])")
    js(c, "window.__office.select('E3')")
    wait_open(c, "no loops")
    p = read_card(c)
    if not p or not p["empty"] or p["rows"]:
        fails.append(f"no loops: card should say so: {p and (p['empty'], p['rows'])}")
    js(c, "window.__office.chatBusy(true, 'E3')")
    p = read_card(c)
    if not p or not p["note"] or p["chip"].strip() != i18n["state_working"]:
        fails.append(f"chat: the card should show E3 answering: {p and (p['note'], p['chip'])}")
    js(c, "window.__office.chatBusy(false)")
    js(c, "window.__office.select(null)")
    load(c)

    # 2. Escape closes and returns focus to the canvas container; so does the close button.
    js(c, "window.__office.select('E2')")
    wait_open(c, "escape")
    escape(c)
    c.pump(0.3)
    if panel_open(c):
        fails.append("escape: card still open")
    if js(c, "document.activeElement && document.activeElement.id") != "office":
        fails.append(f"escape: focus is {js(c, 'document.activeElement && (document.activeElement.id || document.activeElement.tagName)')}, not #office")
    js(c, "window.__office.select('E2')")
    wait_open(c, "close button")
    js(c, f"{PANEL}.querySelector('.office-panel-close').click()")
    if panel_open(c):
        fails.append("close button: card still open")

    # 3. A real click on a person selects them; hover shows a pointer; camera eases to them.
    pt = js(c, "window.__office.screenOf('E2')")
    if not pt:
        fails.append("screenOf(E2) gave nothing")
    else:
        c.call("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": pt["x"], "y": pt["y"]}, session=c.session)
        c.pump(0.4)
        if js(c, "window.__office.hover()") != "E2":
            fails.append(f"hover: {js(c, 'window.__office.hover()')!r} at {pt}")
        if js(c, "document.querySelector('#office canvas').style.cursor") != "pointer":
            fails.append("hover: cursor is not a pointer")
        click(c, pt["x"], pt["y"])
        if wait_open(c, "click person"):
            if js(c, "window.__office.selected()") != "E2":
                fails.append(f"click person selected {js(c, 'window.__office.selected()')!r}")
            c.pump(1.2)
            f = js(c, "window.__office.focus()")
            d = sum((a - b) ** 2 for a, b in zip(f["target"], f["person"])) ** 0.5
            if d > 2.5:
                fails.append(f"camera target is {d:.1f} from the person after the ease")
        # 4. A click on empty space closes it.
        js(c, "window.__office.select('E2')")
        wait_open(c, "empty click")
        r = js(c, "(() => { const b = document.querySelector('#office canvas').getBoundingClientRect(); return {x: b.left + 12, y: b.top + 12}; })()")
        click(c, r["x"], r["y"])
        c.pump(0.3)
        if panel_open(c):
            fails.append("empty click: card still open")
        if js(c, "window.__office.selected()") is not None:
            fails.append("empty click: still selected")

    # 5. A hostile loop name stays text and its link is percent-encoded.
    evil = 'we/ird"<b>\u00e9'
    state = {"name": evil, "title": evil, "enabled": True, "state": "idle", "reason": "r<i>", "last_run": None, "next_run": "",
             "history": [], "run_now_url": "/loops/x/run-now", "toggle_url": "/daemons/loops/x/disable", "pending_approvals": 0}
    js(c, f"window.__office.setLoops([{json.dumps(state)}]); window.__office.select('E1')")
    if wait_open(c, "hostile"):
        p = read_card(c)
        if [r["text"] for r in p["rows"]] != [evil] or p["rows"][0]["href"] != "/loops/we%2Fird%22%3Cb%3E%C3%A9":
            fails.append(f"hostile: rows {p['rows']}")
        if js(c, f"{PANEL}.querySelectorAll('b, i').length") != 0:
            fails.append("hostile: loop data became an element in the card")
    js(c, "window.__office.select(null)")

    # 6. Enable / Disable really toggle the sandbox loop; the card and the colleague follow.
    load(c)
    js(c, "window.__office.select('E2')")
    wait_open(c, "toggle")
    js(c, f"{row('quiet-loop')}.querySelector('[data-act=toggle]').click()")
    try:
        c.wait_for(f"{row('quiet-loop')}.querySelector('[data-act=toggle]').textContent === {json.dumps(i18n['enable'])}", timeout=10, interval=0.2)
    except TimeoutError:
        fails.append(f"disable: the card did not update: {read_card(c)}")
    st = js(c, "fetch('/office/state').then(r => r.json()).then(d => d.loops.find(l => l.name === 'quiet-loop').state)")
    if st != "disabled":
        fails.append(f"disable: server state is {st}")
    p = read_card(c)
    if p and p["error"]:
        fails.append("disable: error line shown after a successful toggle")
    js(c, f"{row('quiet-loop')}.querySelector('[data-act=toggle]').click()")
    try:
        c.wait_for(f"{row('quiet-loop')}.querySelector('[data-act=toggle]').textContent === {json.dumps(i18n['disable'])}", timeout=10, interval=0.2)
    except TimeoutError:
        fails.append("enable: the card did not return to Disable")

    # 6b. A refused action shows the server's own flash text.
    base = js(c, "fetch('/office/state').then(r => r.json()).then(d => d.loops)")
    ghost = {"name": "ghost-loop", "title": "Ghost", "enabled": True, "state": "idle", "reason": "", "last_run": None,
             "next_run": "", "history": [], "run_now_url": "", "pending_approvals": 0,
             "toggle_url": "/daemons/loops/ghost-loop/disable"}
    js(c, f"window.__office.setLoops({json.dumps(base + [ghost])})")
    who = next(a["name"] for a in js(c, "window.__office.debug()") if "ghost-loop" in a["loops"])
    js(c, f"window.__office.select({json.dumps(who)})")
    wait_open(c, "ghost-loop")
    js(c, f"{row('ghost-loop')}.querySelector('[data-act=toggle]').click()")
    try:
        c.wait_for(f"(() => {{ const e = {PANEL}.querySelector('.office-panel-error'); return !e.hidden && e.textContent.includes('ghost-loop'); }})()", timeout=10, interval=0.2)
    except TimeoutError:
        fails.append(f"refused toggle: card error is {read_card(c) and read_card(c)['errorText']!r}, expected the server's flash text")
    js(c, "window.__office.select(null)")

    # 7. Run now: record the POST, never send it; the loop's owner goes to work.
    load(c)
    js(c, """(() => { window.__posts = []; const real = window.fetch.bind(window);
      window.fetch = (u, o) => { if (o && o.method === 'POST') { window.__posts.push([String(u), String(o.body)]);
        return Promise.resolve(new Response(null, {status: window.__fakeStatus || 200})); } return real(u, o); }; })()""")
    expected = js(c, "fetch('/office/state').then(r => r.json()).then(d => d.loops.find(l => l.name === 'broken-loop').run_now_url)")
    js(c, "window.__office.select('E1')")
    wait_open(c, "run now")
    js(c, f"{row('broken-loop')}.querySelector('[data-act=run]').click()")
    c.pump(0.8)
    posts = js(c, "window.__posts")
    if len(posts) != 1 or posts[0][0] != expected or "csrf_token=" + csrf not in posts[0][1]:
        fails.append(f"run now: recorded POSTs {posts}, expected one to {expected} with csrf_token={csrf[:4]}...")
    if read_card(c)["error"]:
        fails.append("run now: error shown for an ok response")
    # 8. A failed response shows the translated line.
    js(c, "window.__fakeStatus = 500; window.__posts = []")
    js(c, f"{row('broken-loop')}.querySelector('[data-act=run]').click()")
    c.pump(0.8)
    if not read_card(c)["error"]:
        fails.append("failed action: no 'Action failed' line")
    # A disabled Run now never posts.
    js(c, "window.__fakeStatus = 200; window.__posts = []; window.__office.select('E2')")
    wait_open(c, "run disabled")
    js(c, f"{row('busy-loop')}.querySelector('[data-act=run]').click()")
    c.pump(0.4)
    if js(c, "window.__posts.length") != 0:
        fails.append("run now posted while the loop is working")

    if c.errors:
        fails.append("console errors: " + " | ".join(c.errors[:3]))

# 9. French.
with Chrome() as c:
    load(c, "fr")
    i18n = json.loads(js(c, "document.getElementById('office-i18n').textContent"))
    if i18n["run_now"] != "Ex\u00e9cuter maintenant":
        fails.append(f"french: i18n run_now {i18n['run_now']!r}")
    js(c, "window.__office.select('E2')")
    if wait_open(c, "french"):
        p = read_card(c)
        if p["rows"][0]["runText"] != i18n["run_now"] or p["chip"].strip() != i18n["state_working"] or p["chip"].strip() == "Working":
            fails.append(f"french: card text {p['rows'][0]['runText']!r} / {p['chip']!r}")
        c.pump(1.2)
        c.screenshot(f"{out}/card-working-fr.png")
    if c.errors:
        fails.append("french console errors: " + " | ".join(c.errors[:3]))

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: card per colleague, Escape/close/empty click, real click + hover + camera, French, encoded links, hostile name, toggle, run now, failure line")
PY
echo "screenshots in $OUT"

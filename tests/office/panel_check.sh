#!/usr/bin/env bash
# Checks the Office details panel: selecting a person opens a panel with the name,
# the translated state and the right buttons per state; Escape and a click on empty
# space close it and return focus; a real click on a person (projected to screen)
# selects them and the camera eases to them; hover turns the cursor to a pointer;
# French shows French labels; the history link is percent-encoded and a hostile
# loop name never becomes HTML; Enable/Disable really toggle the sandbox loop and
# move the person; Run now POSTs to run_now_url with a CSRF token (recorded, never
# sent, so no real run launches); a failed action shows "Action failed".
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
        c.goto(f"{url}/loops?view=office#h=13")
        c.wait_for("document.readyState === 'complete'", timeout=30)
        c.evaluate(f"document.cookie = 'loop_lang={lang}; path=/'")
        c.goto("about:blank")
    c.goto(f"{url}/loops?view=office#h=13")
    c.wait_for("window.__office && window.__office.ready && window.__office.syncs >= 1 && !!window.__office.select", timeout=60)

def js(c, expr):
    return c.evaluate(expr)

PANEL = "document.querySelector('#office .office-panel')"
def panel_open(c):
    return js(c, "window.__office.panelOpen()")

def read_panel(c):
    return js(c, f"""(() => {{ const p = {PANEL}; if (!p) return null;
      const q = (s) => p.querySelector(s);
      const dis = (s) => {{ const e = q(s); return e ? (e.disabled || e.getAttribute('aria-disabled') === 'true') : null; }};
      const ap = q('[data-act=approvals]');
      return {{name: q('.office-panel-name').textContent, chip: q('.office-panel-state').textContent,
        why: q('.office-panel-why').textContent,
        runDisabled: dis('[data-act=run]'), runText: q('[data-act=run]').textContent,
        toggleText: q('[data-act=toggle]').textContent,
        open: q('[data-act=open]').getAttribute('href'),
        approvals: ap && !ap.closest('[hidden]') && !ap.hidden ? [ap.textContent, ap.getAttribute('href')] : null,
        focusInside: p.contains(document.activeElement),
        error: !q('.office-panel-error').hidden,
        html: p.innerHTML}}; }})()""")

def wait_open(c, label):
    try:
        c.wait_for("window.__office.panelOpen()", timeout=5, interval=0.1)
    except TimeoutError:
        fails.append(f"{label}: panel did not open")
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

FIXTURE = {  # name -> (state, run disabled, toggle label key)
    "gitlab-loop": ("waiting", False, "Disable"),
    "busy-loop": ("working", True, "Disable"),
    "broken-loop": ("failed", False, "Disable"),
    "quiet-loop": ("idle", False, "Disable"),
    "off-loop": ("disabled", True, "Enable"),
}

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

    # 1. Selecting each fixture loop shows its panel.
    for name, (state, run_dis, toggle) in FIXTURE.items():
        js(c, f"window.__office.select({json.dumps(name)})")
        if not wait_open(c, name):
            continue
        p = read_panel(c)
        if p["name"] != name:
            fails.append(f"{name}: panel name {p['name']!r}")
        if p["chip"].strip() != i18n["state_" + state]:
            fails.append(f"{name}: chip {p['chip']!r}, expected {i18n['state_' + state]!r}")
        if p["runDisabled"] != run_dis:
            fails.append(f"{name}: Run now disabled={p['runDisabled']}, expected {run_dis}")
        if p["runText"] != i18n["run_now"]:
            fails.append(f"{name}: run button text {p['runText']!r}")
        if p["toggleText"] != i18n[toggle.lower()]:
            fails.append(f"{name}: toggle text {p['toggleText']!r}, expected {i18n[toggle.lower()]!r}")
        if p["open"] != f"/loops/{name}":
            fails.append(f"{name}: open link {p['open']!r}")
        if not p["why"].strip():
            fails.append(f"{name}: empty Why")
        if not p["focusInside"]:
            fails.append(f"{name}: focus did not move into the panel")
        if state == "waiting":
            if not p["approvals"] or "1" not in p["approvals"][0] or i18n["pending_approvals"] not in p["approvals"][0] \
                    or p["approvals"][1] != "/?view=activity":
                fails.append(f"{name}: approvals link {p['approvals']}")
        elif p["approvals"]:
            fails.append(f"{name}: approvals link shown for a {state} loop")
        if p["error"]:
            fails.append(f"{name}: error line visible")

    # 2. Escape closes and returns focus to the canvas container.
    js(c, "window.__office.select('busy-loop')")
    wait_open(c, "escape")
    escape(c)
    c.pump(0.3)
    if panel_open(c):
        fails.append("escape: panel still open")
    if js(c, "document.activeElement && document.activeElement.id") != "office":
        fails.append(f"escape: focus is {js(c, 'document.activeElement && (document.activeElement.id || document.activeElement.tagName)')}, not #office")
    # The close button closes too.
    js(c, "window.__office.select('busy-loop')")
    wait_open(c, "close button")
    js(c, f"{PANEL}.querySelector('.office-panel-close').click()")
    if panel_open(c):
        fails.append("close button: panel still open")

    # 3. A real click on a person selects them; hover shows a pointer; camera eases to them.
    pt = js(c, "window.__office.screenOf('quiet-loop')")
    if not pt:
        fails.append("screenOf(quiet-loop) gave nothing")
    else:
        c.call("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": pt["x"], "y": pt["y"]}, session=c.session)
        c.pump(0.4)
        if js(c, "window.__office.hover()") != "quiet-loop":
            fails.append(f"hover: {js(c, 'window.__office.hover()')!r} at {pt}")
        if js(c, "document.querySelector('#office canvas').style.cursor") != "pointer":
            fails.append("hover: cursor is not a pointer")
        click(c, pt["x"], pt["y"])
        if wait_open(c, "click person"):
            if js(c, "window.__office.selected()") != "quiet-loop":
                fails.append(f"click person selected {js(c, 'window.__office.selected()')!r}")
            c.pump(1.2)
            f = js(c, "window.__office.focus()")
            d = sum((a - b) ** 2 for a, b in zip(f["target"], f["person"])) ** 0.5
            if d > 2.5:
                fails.append(f"camera target is {d:.1f} from the person after the ease")
            c.screenshot(f"{out}/panel-focus-en.png")
        # 4. A click on empty space closes it.
        js(c, "window.__office.select('quiet-loop')")
        wait_open(c, "empty click")
        r = js(c, "(() => { const b = document.querySelector('#office canvas').getBoundingClientRect(); return {x: b.left + 12, y: b.top + 12}; })()")
        click(c, r["x"], r["y"])
        c.pump(0.3)
        if panel_open(c):
            fails.append("empty click: panel still open")
        if js(c, "window.__office.selected()") is not None:
            fails.append("empty click: still selected")

    # 5. History link encoding and a hostile name.
    evil = 'we/ird"<b>é'
    state = {"name": evil, "title": evil, "enabled": True, "state": "idle", "reason": "r<i>", "last_run": None, "next_run": "",
             "history": [{"file": "a b/é.md", "name": "a b/é"}], "run_now_url": "/loops/x/run-now",
             "toggle_url": "/daemons/loops/x/disable", "pending_approvals": 0}
    js(c, f"window.__office.setLoops([{json.dumps(state)}]); window.__office.select({json.dumps(evil)})")
    if wait_open(c, "hostile"):
        p = read_panel(c)
        if p["name"] != evil:
            fails.append(f"hostile: name {p['name']!r}")
        links = js(c, f"[...{PANEL}.querySelectorAll('.office-panel-history a')].map(a => a.getAttribute('href'))")
        want = "/loops/we%2Fird%22%3Cb%3E%C3%A9/history/a%20b%2F%C3%A9.md"
        if links != [want]:
            fails.append(f"hostile: history links {links}, expected [{want!r}]")
        if p["open"] != "/loops/we%2Fird%22%3Cb%3E%C3%A9":
            fails.append(f"hostile: open link {p['open']!r}")
        if js(c, f"{PANEL}.querySelectorAll('b, i').length") != 0:
            fails.append("hostile: loop data became an element in the panel")
    js(c, "window.__office.select(null)")

    # 6. Enable / Disable really toggle the sandbox loop; the person moves.
    load(c)
    js(c, "window.__office.select('quiet-loop')")
    wait_open(c, "toggle")
    js(c, f"{PANEL}.querySelector('[data-act=toggle]').click()")
    try:
        c.wait_for("window.__office.debug().find(a => a.name === 'quiet-loop').state === 'disabled'", timeout=10, interval=0.2)
        c.wait_for("(() => { const a = window.__office.debug().find(a => a.name === 'quiet-loop'); return !a.walking && a.x < -13; })()", timeout=120, interval=0.5)
    except TimeoutError:
        fails.append(f"disable: quiet-loop did not become disabled and move: {js(c, 'window.__office.debug()')}")
    st = js(c, "fetch('/office/state').then(r => r.json()).then(d => d.loops.find(l => l.name === 'quiet-loop').state)")
    if st != "disabled":
        fails.append(f"disable: server state is {st}")
    p = read_panel(c)
    if not p or p["toggleText"] != i18n["enable"] or not p["runDisabled"]:
        fails.append(f"disable: panel did not update: {p and (p['toggleText'], p['runDisabled'])}")
    if p and p["error"]:
        fails.append("disable: error line shown after a successful toggle")
    js(c, f"{PANEL}.querySelector('[data-act=toggle]').click()")
    try:
        c.wait_for("window.__office.debug().find(a => a.name === 'quiet-loop').state === 'idle'", timeout=10, interval=0.2)
    except TimeoutError:
        fails.append("enable: quiet-loop did not return to idle")

    # 7. Run now: record the POST, never send it.
    js(c, """(() => { window.__posts = []; const real = window.fetch.bind(window);
      window.fetch = (u, o) => { if (o && o.method === 'POST') { window.__posts.push([String(u), String(o.body)]);
        return Promise.resolve(new Response(null, {status: window.__fakeStatus || 200})); } return real(u, o); }; })()""")
    expected = js(c, "fetch('/office/state').then(r => r.json()).then(d => d.loops.find(l => l.name === 'broken-loop').run_now_url)")
    js(c, "window.__office.select('broken-loop')")
    wait_open(c, "run now")
    js(c, f"{PANEL}.querySelector('[data-act=run]').click()")
    c.pump(0.8)
    posts = js(c, "window.__posts")
    if len(posts) != 1 or posts[0][0] != expected or "csrf_token=" + csrf not in posts[0][1]:
        fails.append(f"run now: recorded POSTs {posts}, expected one to {expected} with csrf_token={csrf[:4]}...")
    if read_panel(c)["error"]:
        fails.append("run now: error shown for an ok response")
    # 8. A failed response shows the translated line.
    js(c, "window.__fakeStatus = 500; window.__posts = []")
    js(c, f"{PANEL}.querySelector('[data-act=run]').click()")
    c.pump(0.8)
    p = read_panel(c)
    if not p["error"]:
        fails.append("failed action: no 'Action failed' line")
    # A disabled Run now never posts.
    js(c, "window.__fakeStatus = 200; window.__posts = []; window.__office.select('busy-loop')")
    wait_open(c, "run disabled")
    js(c, f"{PANEL}.querySelector('[data-act=run]').click()")
    c.pump(0.4)
    if js(c, "window.__posts.length") != 0:
        fails.append("run now posted while the loop is working")

    if c.errors:
        fails.append("console errors: " + " | ".join(c.errors[:3]))

# 9. French.
with Chrome() as c:
    load(c, "fr")
    i18n = json.loads(js(c, "document.getElementById('office-i18n').textContent"))
    if i18n["run_now"] != "Exécuter maintenant":
        fails.append(f"french: i18n run_now {i18n['run_now']!r}")
    js(c, "window.__office.select('busy-loop')")
    if wait_open(c, "french"):
        p = read_panel(c)
        if p["runText"] != "Exécuter maintenant" or p["chip"].strip() != i18n["state_working"] or p["chip"].strip() == "Working":
            fails.append(f"french: panel text {p['runText']!r} / {p['chip']!r}")
        if p["toggleText"] != i18n["disable"]:
            fails.append(f"french: toggle {p['toggleText']!r}")
        c.pump(1.2)
        c.screenshot(f"{out}/panel-working-fr.png")
    js(c, "window.__office.select('gitlab-loop')")
    c.pump(1.2)
    c.screenshot(f"{out}/panel-waiting-fr.png")
    if c.errors:
        fails.append("french console errors: " + " | ".join(c.errors[:3]))

if fails:
    print("FAIL")
    for f in fails:
        print("  - " + f)
    sys.exit(1)
print("PASS: panel per state, Escape/close/empty click, real click + hover + camera, French, encoded links, hostile name, toggle, run now, failure line")
PY
echo "screenshots in $OUT"

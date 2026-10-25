"""A minimal Chrome DevTools Protocol driver over --remote-debugging-pipe.

Standard library only: Chrome reads NUL-terminated JSON commands on fd 3 and
writes replies and events on fd 4, so no WebSocket client is needed. Used by the
office checks to drive the page through `window.__office` test hooks.

    with Chrome(width=1500, height=900) as c:
        c.goto(url)
        c.wait_for("window.__office && window.__office.ready")
        c.evaluate("window.__office.debug()")
        c.screenshot("/tmp/shot.png")
        c.errors   # uncaught exceptions and console.error calls seen so far
"""

import base64
import json
import os
import select
import shutil
import subprocess
import tempfile
import time

CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


class Chrome:
    def __init__(self, width=1500, height=900, flags=(), reduced_motion=False):
        self.profile = tempfile.mkdtemp()
        to_chrome_r, self._out = os.pipe()
        self._in, from_chrome_w = os.pipe()

        def wire():
            # Move both ends clear of 3 and 4 first: either may already sit there.
            r, w = os.dup(to_chrome_r), os.dup(from_chrome_w)
            r, w = os.dup2(r, 100), os.dup2(w, 101)
            os.dup2(r, 3)
            os.dup2(w, 4)
            os.close(r)
            os.close(w)

        # A throwaway profile keeps this from attaching to a running Chrome.
        self.proc = subprocess.Popen(
            [CHROME, "--headless=new", "--remote-debugging-pipe", f"--user-data-dir={self.profile}",
             "--enable-unsafe-swiftshader", "--use-gl=angle", "--use-angle=swiftshader",
             "--no-first-run", "--no-default-browser-check", f"--window-size={width},{height}",
             *flags, "about:blank"],
            preexec_fn=wire, close_fds=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.close(to_chrome_r)
        os.close(from_chrome_w)
        self._buf = b""
        self._id = 0
        self.errors = []
        page = next(t for t in self.call("Target.getTargets")["targetInfos"] if t["type"] == "page")
        self.session = self.call("Target.attachToTarget", {"targetId": page["targetId"], "flatten": True})["sessionId"]
        self.call("Runtime.enable", session=self.session)
        self.call("Page.enable", session=self.session)
        # Pin prefers-reduced-motion: headless Chrome otherwise follows the host's setting.
        self.call("Emulation.setEmulatedMedia", {"features": [
            {"name": "prefers-reduced-motion", "value": "reduce" if reduced_motion else "no-preference"}]},
            session=self.session)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def close(self):
        try:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()
        for fd in (self._in, self._out):
            try:
                os.close(fd)
            except OSError:
                pass
        shutil.rmtree(self.profile, ignore_errors=True)

    def _read(self, timeout):
        while b"\0" not in self._buf:
            ready, _, _ = select.select([self._in], [], [], timeout)
            if not ready:
                raise TimeoutError("no reply from Chrome")
            chunk = os.read(self._in, 1 << 20)
            if not chunk:
                raise RuntimeError("Chrome closed the pipe")
            self._buf += chunk
        msg, self._buf = self._buf.split(b"\0", 1)
        return json.loads(msg)

    def _event(self, msg):
        method, params = msg.get("method"), msg.get("params", {})
        if method == "Runtime.exceptionThrown":
            d = params.get("exceptionDetails", {})
            self.errors.append((d.get("exception") or {}).get("description") or d.get("text", "exception"))
        elif method == "Runtime.consoleAPICalled" and params.get("type") == "error":
            self.errors.append(" ".join(str(a.get("value", a.get("description", ""))) for a in params.get("args", [])))

    def call(self, method, params=None, session=None, timeout=60):
        self._id += 1
        msg = {"id": self._id, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        os.write(self._out, json.dumps(msg).encode() + b"\0")
        while True:
            reply = self._read(timeout)
            if reply.get("id") == self._id:
                if "error" in reply:
                    raise RuntimeError(f"{method}: {reply['error']}")
                return reply.get("result", {})
            self._event(reply)

    def pump(self, seconds):
        """Waits `seconds` while still collecting console events."""
        end = time.time() + seconds
        while time.time() < end:
            try:
                self._event(self._read(max(0.01, end - time.time())))
            except TimeoutError:
                pass

    def goto(self, url):
        self.call("Page.navigate", {"url": url}, session=self.session)

    def evaluate(self, expression):
        r = self.call("Runtime.evaluate", {"expression": expression, "returnByValue": True, "awaitPromise": True},
                      session=self.session)
        if "exceptionDetails" in r:
            raise RuntimeError(f"evaluate({expression!r}): {r['exceptionDetails'].get('text')}")
        return r.get("result", {}).get("value")

    def wait_for(self, expression, timeout=60, interval=0.25):
        """Polls until `expression` is truthy; returns its value or raises TimeoutError."""
        end = time.time() + timeout
        while time.time() < end:
            try:
                v = self.evaluate(expression)
            except RuntimeError:
                v = None
            if v:
                return v
            self.pump(interval)
        raise TimeoutError(f"timed out waiting for {expression}")

    def screenshot(self, path):
        data = self.call("Page.captureScreenshot", {"format": "png"}, session=self.session)["data"]
        with open(path, "wb") as f:
            f.write(base64.b64decode(data))

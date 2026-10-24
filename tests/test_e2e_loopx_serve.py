"""End-to-end `loopx serve`: the lock file, /api/version, the SSE event stream,
approvals over HTTP, and the server-rendered pages. Rust-only
(skipped without LOOPX_BIN)."""
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from cli_runner import loopx_bin

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")

REPO = Path(__file__).resolve().parent.parent


def start_server(tmp_path, *flags, legacy=False):
    le = tmp_path / "le"
    home = tmp_path / "home"
    le.mkdir()
    home.mkdir()
    env = {
        "PATH": f"{Path(sys.executable).parent}:/usr/bin:/bin:{Path(loopx_bin()).parent}",
        "HOME": str(home), "LOOP_ENGINEERING_HOME": str(le), "LOOPX_ROOT": str(le),
        "LOOP_EVENTS_DIR": str(le / "outputs" / "events"), "LOOP_UPDATE_DIR": str(tmp_path / "upd"),
        "LOOPX_LEGACY_ROOT": str(REPO),
    }
    args = [loopx_bin(), "serve", "--port", "0", *([] if legacy else ["--no-legacy"]), *flags]
    proc = subprocess.Popen(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    line = proc.stdout.readline()
    m = re.search(r"http://127\.0\.0\.1:(\d+)/", line)
    assert m, f"no ready line: {line!r} {proc.poll()}"
    return proc, int(m.group(1)), le, env


def stop(proc):
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()


def get(port, path, headers=None):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=headers or {})
    with urllib.request.urlopen(req, timeout=15) as r:
        return r.status, r.read().decode()


def test_lock_file_version_and_clean_shutdown(tmp_path):
    proc, port, le, _ = start_server(tmp_path)
    try:
        lock = json.loads((le / "loopx.lock").read_text())
        assert lock["pid"] == proc.pid and lock["port"] == port and len(lock["token"]) == 64
        assert oct((le / "loopx.lock").stat().st_mode & 0o777) == "0o600"
        status, body = get(port, "/api/version")
        v = json.loads(body)
        assert status == 200 and v["name"] == "loopx" and v["pid"] == proc.pid
        assert v["root"] == str(le)
        assert get(port, "/healthz")[1] == "ok"
    finally:
        stop(proc)
    assert proc.returncode == 0
    assert not (le / "loopx.lock").exists(), "the lock goes away on a clean exit"


def test_a_second_backend_is_refused_while_the_first_lives(tmp_path):
    proc, port, le, env = start_server(tmp_path)
    try:
        second = subprocess.run([loopx_bin(), "serve", "--port", "0", "--no-legacy"], env=env,
                                capture_output=True, text=True, timeout=20)
        assert second.returncode == 1
        assert "already running" in second.stderr and str(port) in second.stderr
    finally:
        stop(proc)


def test_a_stale_lock_does_not_block_startup(tmp_path):
    le = tmp_path / "le"
    le.mkdir()
    (le / "loopx.lock").write_text(json.dumps(
        {"pid": 4194300, "port": 1, "version": "0.0.1", "started_at": "x", "token": "old"}))
    env = {"PATH": "/usr/bin:/bin", "HOME": str(tmp_path), "LOOP_ENGINEERING_HOME": str(le),
           "LOOPX_ROOT": str(le)}
    p = subprocess.Popen([loopx_bin(), "serve", "--port", "0", "--no-legacy"], env=env,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        assert "serving" in p.stdout.readline()
        assert json.loads((le / "loopx.lock").read_text())["pid"] == p.pid
    finally:
        stop(p)


def test_foreign_host_headers_are_rejected(tmp_path):
    proc, port, _, _ = start_server(tmp_path)
    try:
        with pytest.raises(urllib.error.HTTPError) as e:
            get(port, "/api/snapshot", {"Host": "evil.example.com"})
        assert e.value.code == 403
    finally:
        stop(proc)


def test_event_stream_pushes_status_changes_and_approvals(tmp_path):
    proc, port, le, env = start_server(tmp_path)
    got = []
    ready = threading.Event()

    def reader():
        req = urllib.request.Request(f"http://127.0.0.1:{port}/api/events")
        with urllib.request.urlopen(req, timeout=30) as r:
            for raw in r:
                line = raw.decode().rstrip("\n")
                if line.startswith("data: "):
                    got.append(json.loads(line[6:]))
                    ready.set()
                if len(got) >= 3:
                    return

    t = threading.Thread(target=reader, daemon=True)
    t.start()
    try:
        assert ready.wait(10), "no initial snapshot"
        assert got[0]["app_state"] == "idle"
        subprocess.run([loopx_bin(), "status", "write", "running", "--pid", str(proc.pid)], env=env, check=True)
        deadline = time.time() + 15
        while time.time() < deadline and not any(s["app_state"] == "running" for s in got):
            time.sleep(0.1)
        assert any(s["app_state"] == "running" for s in got)
        # an approval request flips it to awaiting_approval
        appr = le / "outputs" / "approvals"
        appr.mkdir(parents=True)
        (appr / "apr_x.json").write_text(json.dumps({
            "id": "apr_x", "kind": "open_mr", "status": "pending", "title": "Fix #1",
            "created_at": "2026-10-22T10:00:00.000Z", "expires_at": "2099-01-01T00:00:00.000Z"}))
        t.join(timeout=15)
        assert got[-1]["app_state"] == "awaiting_approval"
        assert got[-1]["approvals"]["pending"][0]["id"] == "apr_x"
    finally:
        stop(proc)


def test_approvals_can_be_answered_over_http_with_the_lock_token(tmp_path):
    proc, port, le, env = start_server(tmp_path)
    try:
        appr = le / "outputs" / "approvals"
        appr.mkdir(parents=True)
        (appr / "apr_y.json").write_text(json.dumps({
            "id": "apr_y", "kind": "open_mr", "status": "pending", "title": "t",
            "created_at": "2026-10-22T10:00:00.000Z", "expires_at": "2099-01-01T00:00:00.000Z"}))
        assert json.loads(get(port, "/api/approvals")[1])["pending"][0]["id"] == "apr_y"
        body = json.dumps({"decision": "approve", "by": "mac-app"}).encode()
        req = urllib.request.Request(f"http://127.0.0.1:{port}/api/approvals/apr_y", data=body,
                                     headers={"Content-Type": "application/json"}, method="POST")
        with pytest.raises(urllib.error.HTTPError) as e:
            urllib.request.urlopen(req, timeout=10)
        assert e.value.code == 403, "no token, no write"
        token = json.loads((le / "loopx.lock").read_text())["token"]
        req.add_header("X-LoopX-Token", token)
        with urllib.request.urlopen(req, timeout=10) as r:
            assert json.loads(r.read())["status"] == "approved"
        assert json.loads((appr / "apr_y.json").read_text())["decided_by"] == "mac-app"
    finally:
        stop(proc)


def test_the_dashboard_home_page_is_served_by_loopx(tmp_path):
    proc, port, le, _ = start_server(tmp_path)
    try:
        status, html = get(port, "/")
        assert status == 200 and "<html" in html.lower()
        assert "LOOP" in html.upper()
    finally:
        stop(proc)

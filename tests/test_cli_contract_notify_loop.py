"""Black-box contract for `notify.py` / `loopx notify-loop`, against a fake Slack
webhook with scratch HOME/LOOP_ENGINEERING_HOME. Run against Rust with LOOPX_BIN=target/debug/loopx."""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from cli_runner import run_cli


def start_hook(status=200):
    posts = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            posts.append(json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0)))))
            self.send_response(status)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, posts


@pytest.fixture
def setup(tmp_path):
    def make(status=200, loops=None):
        server, posts = start_hook(status)
        (tmp_path / ".slack").mkdir(exist_ok=True)
        (tmp_path / ".slack" / "config.json").write_text(
            json.dumps({"webhook_url": f"http://127.0.0.1:{server.server_address[1]}/hook"}))
        home = tmp_path / "le"
        home.mkdir(exist_ok=True)
        (home / "loops.json").write_text(json.dumps(loops if loops is not None else [{"name": "x"}]))
        return {"HOME": str(tmp_path), "LOOP_ENGINEERING_HOME": str(home)}, posts, server
    return make


def test_default_route_posts_to_slack(setup):
    env, posts, server = setup()
    r = run_cli("notify.py", "x", "hello", env=env)
    server.shutdown()
    assert (r.returncode, r.stdout) == (0, "slack-default: ok: sent\n")
    assert posts == [{"text": "hello"}]


def test_unknown_loop_and_non_list_notify_still_use_default(setup):
    env, posts, server = setup(loops=[{"name": "y", "notify": "oops"}])
    for name in ("ghost", "y"):
        r = run_cli("notify.py", name, "hi", env=env)
        assert (r.returncode, r.stdout) == (0, "slack-default: ok: sent\n")
    server.shutdown()
    assert len(posts) == 2


def test_default_failure_reports_class_and_exit_1(setup):
    env, posts, server = setup(status=500)
    r = run_cli("notify.py", "x", "hello", env=env)
    server.shutdown()
    assert (r.returncode, r.stdout) == (1, "slack-default: FAILED: HTTPError\n")


def test_missing_slack_config_reports_filenotfound(tmp_path):
    (tmp_path / "le").mkdir()
    (tmp_path / "le" / "loops.json").write_text("[]")
    r = run_cli("notify.py", "x", "hello", env={"HOME": str(tmp_path), "LOOP_ENGINEERING_HOME": str(tmp_path / "le")})
    assert (r.returncode, r.stdout) == (1, "slack-default: FAILED: FileNotFoundError\n")


def test_usage(tmp_path):
    env = {"HOME": str(tmp_path), "LOOP_ENGINEERING_HOME": str(tmp_path)}
    for args in ([], ["only-one"], ["a", "b", "c"]):
        r = run_cli("notify.py", *args, env=env)
        assert r.returncode == 1 and "Usage" in r.stderr

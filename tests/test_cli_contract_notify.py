"""Black-box contract for `slack_notify.py` / `loopx notify`, against a fake
incoming-webhook server and a scratch HOME. Run against Rust with
LOOPX_BIN=target/debug/loopx.
"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from cli_runner import run_cli


class Webhook:
    def __init__(self, status=200):
        self.posts = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                outer.posts.append({
                    "path": self.path,
                    "content_type": self.headers.get("Content-Type"),
                    "body": json.loads(self.rfile.read(length)),
                })
                self.send_response(status)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


@pytest.fixture
def hook():
    h = Webhook()
    yield h
    h.close()


@pytest.fixture
def env(tmp_path, hook):
    (tmp_path / ".slack").mkdir()
    (tmp_path / ".slack" / "config.json").write_text(json.dumps({
        "webhook_url": hook.url + "/default",
        "bundle_webhooks": {"vip": hook.url + "/vip", "empty": ""},
    }))
    return {"HOME": str(tmp_path)}


def notify(env, *args):
    return run_cli("slack_notify.py", *args, env=env)


def test_posts_text_to_default_webhook(env, hook):
    r = notify(env, "hello é")
    assert r.returncode == 0, r.stderr
    assert r.stdout == "Slack response status: 200\n"
    assert hook.posts == [{"path": "/default", "content_type": "application/json", "body": {"text": "hello é"}}]


def test_bundle_selects_its_webhook(env, hook):
    notify(env, "--bundle=vip", "hi")
    assert hook.posts[0]["path"] == "/vip"


def test_unknown_or_blank_bundle_falls_back_to_default(env, hook):
    notify(env, "--bundle=nope", "a")
    notify(env, "--bundle=empty", "b")
    assert [p["path"] for p in hook.posts] == ["/default", "/default"]


def test_blocks_are_included_when_truthy(env, hook):
    blocks = [{"type": "section", "text": {"type": "mrkdwn", "text": "x"}}]
    notify(env, "--blocks=" + json.dumps(blocks), "hi")
    assert hook.posts[0]["body"] == {"text": "hi", "blocks": blocks}


def test_empty_blocks_are_omitted(env, hook):
    notify(env, "--blocks=[]", "hi")
    assert hook.posts[0]["body"] == {"text": "hi"}


def test_invalid_blocks_json_exits_1_without_posting(env, hook):
    r = notify(env, "--blocks=not-json", "hi")
    assert r.returncode == 1
    assert "Invalid --blocks JSON" in r.stderr
    assert hook.posts == []


def test_only_first_positional_is_the_message(env, hook):
    notify(env, "one", "two")
    assert hook.posts[0]["body"] == {"text": "one"}


@pytest.mark.parametrize("flag", ["-h", "--help"])
def test_help_prints_usage_to_stdout_and_exits_0(env, hook, flag):
    r = notify(env, flag)
    assert r.returncode == 0
    assert r.stdout.startswith("Usage:") and "--bundle=" in r.stdout
    assert hook.posts == []


def test_no_message_is_a_usage_error(env):
    r = notify(env)
    assert r.returncode == 1
    assert "Usage:" in r.stderr


def test_missing_config_fails_nonzero(tmp_path):
    r = notify({"HOME": str(tmp_path)}, "hi")
    assert r.returncode != 0


def test_http_error_fails_nonzero(tmp_path):
    bad = Webhook(status=500)
    try:
        (tmp_path / ".slack").mkdir()
        (tmp_path / ".slack" / "config.json").write_text(json.dumps({"webhook_url": bad.url}))
        assert notify({"HOME": str(tmp_path)}, "hi").returncode != 0
    finally:
        bad.close()

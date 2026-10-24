"""End-to-end LoopKit plugin runs through `loopx` with a fake GitLab, a fake RSS
server, a fake `claude` and a fake Slack webhook, in a scratch HOME and
LOOP_ENGINEERING_HOME. Rust-only (skipped without LOOPX_BIN)."""
import json
import shutil
import stat
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from cli_runner import loopx_bin
from fake_gitlab import FakeGitLab
from fake_webhook import Webhook

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")

REPO = Path(__file__).resolve().parent.parent

RSS = b"""<rss><channel>
<item><title>Rails 9 released</title><link>https://feed.example/rails9</link><pubDate>Mon, 06 Oct 2026 09:00:00 GMT</pubDate><description>big</description></item>
<item><title>Cat pictures</title><link>https://feed.example/cats</link></item>
</channel></rss>"""

ANSWER = {"highlights": [{"title": "Rails 9 released", "link": "https://feed.example/rails9", "source": "Feed",
                          "summary": "Major release", "do_next": "Read the notes", "kill_if": "Not on Rails", "score": 5},
                         {"title": "Evil", "link": "https://evil.example/x", "score": 5}]}

FAKE_CLAUDE = """#!/bin/sh
cat > "$FAKE_LOG/stdin-$$.txt"
printf '%s\\n' "$@" > "$FAKE_LOG/args-$$.txt"
cat "$FAKE_ANSWER"
"""


class Feed:
    def __init__(self):
        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Length", str(len(RSS)))
                self.end_headers()
                self.wfile.write(RSS)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/feed.xml"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


@pytest.fixture
def world(tmp_path):
    hook, feed = Webhook(), Feed()
    home, le = tmp_path / "home", tmp_path / "le"
    (home / ".slack").mkdir(parents=True)
    (home / ".slack" / "config.json").write_text(json.dumps({"webhook_url": hook.url + "/h"}))
    for d in ("stale-sweeper", "rss-watch"):
        (le / "loops" / d).mkdir(parents=True)
        for f in (REPO / "loops" / d).iterdir():
            shutil.copy(f, le / "loops" / d / f.name)
    (le / "loops.json").write_text(json.dumps([
        {"name": "stale-sweeper-loop", "entry_point": "bin.loop_plugins.stale_sweeper", "timeout_seconds": 120,
         "log_suffix": "-stale", "emit_run_events": False, "enabled": True,
         "schedule": {"frequency": "daily", "hour": 1, "minute": 0}, "settings": {"stale_days": "7"}},
        {"name": "rss-watch-loop", "entry_point": "bin.loop_plugins.rss_watch", "timeout_seconds": 120,
         "log_suffix": "-rss", "emit_run_events": False, "enabled": True,
         "schedule": {"frequency": "daily", "hour": 1, "minute": 0}, "settings": {"interests": "Rails"}}]))
    (le / "connectors.json").write_text(json.dumps([
        {"id": "feed", "type": "rss", "label": "Feed", "enabled": True, "settings": {"feeds": feed.url}}]))
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    claude = bin_dir / "claude"
    claude.write_text(FAKE_CLAUDE)
    claude.chmod(claude.stat().st_mode | stat.S_IEXEC)
    log = tmp_path / "log"
    log.mkdir()
    answer = tmp_path / "answer.json"
    answer.write_text(json.dumps({"result": json.dumps(ANSWER), "total_cost_usd": 0.01}))
    env = {"PATH": f"{bin_dir}:/usr/bin:/bin", "HOME": str(home), "LOOP_ENGINEERING_HOME": str(le),
           "LOOPX_ROOT": str(le), "LOOP_EVENTS_DIR": str(le / "events"), "LOOPX_NO_LOGIN_SHELL": "1",
           "LOOPX_KEYCHAIN": f"file:{tmp_path / 'kc'}", "FAKE_LOG": str(log), "FAKE_ANSWER": str(answer)}
    yield {"env": env, "le": le, "hook": hook, "home": home, "log": log, "answer": answer}
    hook.close()
    feed.close()


def loopx(world, *args):
    return subprocess.run([loopx_bin(), *args], env=world["env"], capture_output=True, text=True, timeout=120)


def test_loopkit_lists_the_plugins(world):
    r = loopx(world, "loopkit", "list")
    assert r.returncode == 0
    assert "rss_watch\trss-watch-loop" in r.stdout and "stale_sweeper\tstale-sweeper-loop" in r.stdout


def test_loopkit_usage_and_unknown_plugin(world):
    assert loopx(world, "loopkit").returncode == 2
    r = loopx(world, "loopkit", "nope", "run_1")
    assert r.returncode == 2 and "unknown plugin" in r.stderr
    assert loopx(world, "loopkit", "rss-watch-loop").returncode == 2  # missing run id


def test_stale_sweeper_runs_natively_through_loopx_run(world):
    with FakeGitLab({
        "/api/v4/user": {"username": "me"},
        "/api/v4/issues": [{"iid": 3, "title": "Old issue", "web_url": "https://gl.example/g/p/-/issues/3",
                            "updated_at": "2020-01-01T00:00:00Z", "references": {"full": "g/p#3"}}],
        "/api/v4/merge_requests": [],
    }) as gl:
        (world["home"] / ".gitlab").mkdir()
        (world["home"] / ".gitlab" / "config.json").write_text(json.dumps(
            {"instances": {"work": {"url": gl.url, "token": "glpat"}}}))
        r = loopx(world, "run", "stale-sweeper-loop")
        assert r.returncode == 0, r.stderr
        assert {q["token"] for q in gl.requests} == {"glpat"}
        issue_q = next(q for q in gl.requests if q["path"] == "/api/v4/issues")["query"]
        assert issue_q["scope"] == "assigned_to_me" and issue_q["state"] == "opened"
    texts = world["hook"].texts
    assert any(t.startswith("Stale work - ") and "g/p#3 Old issue (idle " in t for t in texts), texts
    last = json.loads((world["le"] / "outputs" / "loops" / "stale-sweeper-loop" / "last-run.json").read_text())
    assert last["counts"] == {"done": 1, "skipped": 0, "failed": 0} and last["notified"] is True
    assert last["notify_targets"] == [{"id": "slack-default", "ok": True}]
    # a second run has nothing new (the day's item is seen)
    n = len(world["hook"].posts)
    with FakeGitLab({"/api/v4/user": {"username": "me"}, "/api/v4/issues": [
            {"iid": 3, "title": "Old issue", "updated_at": "2020-01-01T00:00:00Z"}],
            "/api/v4/merge_requests": []}) as gl:
        (world["home"] / ".gitlab" / "config.json").write_text(json.dumps(
            {"instances": {"work": {"url": gl.url, "token": "glpat"}}}))
        assert loopx(world, "run", "stale-sweeper-loop").returncode == 0
    assert len(world["hook"].posts) == n


def test_rss_watch_with_a_fake_claude_sends_a_filtered_digest(world):
    r = loopx(world, "loopkit", "rss-watch-loop", "run_e2e")
    assert r.returncode == 0, r.stderr
    digest = next(t for t in world["hook"].texts if t.startswith("RSS Watch Brief"))
    assert "Rails 9 released" in digest and "https://feed.example/rails9" in digest
    assert "evil.example" not in digest, "links the model was never offered are dropped"
    assert "Do next: Read the notes" in digest and ":one:" in digest
    prompts = [p.read_text() for p in world["log"].glob("stdin-*.txt")]
    assert len(prompts) == 1 and "Cat pictures" in prompts[0] and "Rails" in prompts[0]
    args = next(world["log"].glob("args-*.txt")).read_text()
    assert "--strict-mcp-config" in args and "--no-session-persistence" in args
    results = list((world["le"] / "outputs" / "loop-runs").glob("run_e2e_*/result.json"))
    assert len(results) == 1 and json.loads(results[0].read_text())["final_state"] == "completed"
    # entries were marked seen: the next run has no new items and calls no model
    for p in world["log"].glob("*"):
        p.unlink()
    assert loopx(world, "loopkit", "rss-watch-loop", "run_e2e2").returncode == 0
    assert not list(world["log"].glob("stdin-*.txt"))
    content = json.loads((world["le"] / "outputs" / "loops" / "rss-watch-loop" / "content.json").read_text())
    assert content[0]["data"]["highlights"][0]["link"] == "https://feed.example/rails9"


def test_a_malformed_answer_is_retried_then_fails_the_item(world):
    world["answer"].write_text(json.dumps({"result": "not json at all", "total_cost_usd": 0.01}))
    r = loopx(world, "loopkit", "rss-watch-loop", "run_bad")
    assert r.returncode == 1, r.stderr
    last = json.loads((world["le"] / "outputs" / "loops" / "rss-watch-loop" / "last-run.json").read_text())
    assert last["counts"]["failed"] == 1
    assert len(list(world["log"].glob("stdin-*.txt"))) == 2
    second = sorted(world["log"].glob("stdin-*.txt"), key=lambda p: p.stat().st_mtime)[-1].read_text()
    assert "violated the output contract" in second
    assert any("1 of 1 item(s) failed" in t for t in world["hook"].texts)


def test_notify_loop_routes_to_connector_accounts(world):
    loops = json.loads((world["le"] / "loops.json").read_text())
    loops[0]["notify"] = ["slack-default", "feed", "ghost"]
    (world["le"] / "loops.json").write_text(json.dumps(loops))
    r = loopx(world, "notify-loop", "stale-sweeper-loop", "hello team")
    assert r.returncode == 0, r.stderr
    assert r.stdout.splitlines() == [
        "slack-default: ok: sent", "feed: FAILED: not a notify connector", "ghost: FAILED: unknown connector"]
    assert world["hook"].texts == ["hello team"]

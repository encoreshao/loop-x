"""`loopx chat-tool` (the command the dashboard chat assistant runs) and the
chat eval's offline `_chat-eval-stub`, as black-box subprocesses. Rust-only
(skipped without LOOPX_BIN)."""
import json
import os
import subprocess

import pytest

from cli_runner import loopx_bin

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")


@pytest.fixture
def env(tmp_path):
    root, home, user = tmp_path / "root", tmp_path / "home", tmp_path / "user"
    for d in (root, home, user):
        d.mkdir()
    return {
        **os.environ,
        "LOOPX_ROOT": str(root),
        "LOOP_ENGINEERING_HOME": str(home),
        "LOOP_EVENTS_DIR": str(tmp_path / "events"),
        "HOME": str(user),
    }


def run(env, *args):
    return subprocess.run([loopx_bin(), *args], env=env, capture_output=True, text=True, timeout=30)


def test_topic_save_then_list_round_trips(env):
    saved = run(env, "chat-tool", "topic-save", "name=ai", "label=AI", "brief=news")
    assert saved.returncode == 0 and json.loads(saved.stdout)["ok"] is True
    listed = json.loads(run(env, "chat-tool", "topic-list").stdout)
    assert [t["name"] for t in listed["topics"]] == ["ai"]


def test_project_save_refuses_command_fields(env):
    out = run(env, "chat-tool", "project-save", "alias=web", "project_id=g/web", "lint_cmd=boom")
    assert json.loads(out.stdout)["ok"] is False


def test_unknown_action_exits_nonzero(env):
    out = run(env, "chat-tool", "nope")
    assert out.returncode == 1 and "Unknown chat-tool action" in out.stderr


def test_stub_runs_the_registry_offline_and_logs_each_call(env, tmp_path):
    fixture = tmp_path / "fixture.json"
    fixture.write_text(json.dumps({
        "issue": {"iid": 7, "title": "T", "web_url": "https://gitlab.example.com/g/p/-/issues/7", "description": "d"},
        "notes": [], "auto_approve": [],
    }))
    log = tmp_path / "calls.jsonl"
    stub_env = {**env, "LOOP_CHAT_EVAL_DIR": str(tmp_path), "LOOP_CHAT_EVAL_FIXTURE": str(fixture),
                "LOOP_CHAT_EVAL_LOG": str(log)}
    url = "https://gitlab.example.com/g/p/-/issues/7"
    read = run(stub_env, "_chat-eval-stub", "call", "gitlab.read_issue", json.dumps({"url": url}))
    assert json.loads(read.stdout)["result"]["iid"] == 7
    write = run(stub_env, "_chat-eval-stub", "call", "gitlab.comment", json.dumps({"url": url, "body": "hi"}))
    assert json.loads(write.stdout)["status"] == "pending_approval"
    lines = [json.loads(line) for line in log.read_text().splitlines()]
    assert [(c["tool"], c["status"], c["effects"]) for c in lines] == [
        ("gitlab.read_issue", "ok", []),
        ("gitlab.comment", "pending_approval", []),
    ]

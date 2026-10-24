"""End-to-end `loopx run topic-loop` with a fake `claude` and a fake Slack
webhook. Rust-only (skipped without LOOPX_BIN)."""
import json
import shutil
import stat
import subprocess
from pathlib import Path

import pytest

from cli_runner import loopx_bin
from fake_webhook import Webhook

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")

REPO = Path(__file__).resolve().parent.parent

OK_CLAUDE = """#!/bin/sh
for last; do :; done
printf '%s\\n' "$@" > "$FAKE_LOG/args-$(date +%s%N)-$$.txt"
printf '%s' '{"result":"briefing text","total_cost_usd":0.1,"usage":{"input_tokens":1,"output_tokens":1}}'
"""


@pytest.fixture
def world(tmp_path):
    hook = Webhook()
    home, le = tmp_path / "home", tmp_path / "le"
    (home / ".slack").mkdir(parents=True)
    (home / ".slack" / "config.json").write_text(json.dumps({"webhook_url": hook.url + "/h"}))
    (le / "loops" / "topic-monitor").mkdir(parents=True)
    shutil.copy(REPO / "loops" / "topic-monitor" / "loop.yaml", le / "loops" / "topic-monitor" / "loop.yaml")
    (le / "topics.json").write_text(json.dumps([
        {"name": "ai", "label": "AI", "brief": "b", "slack_bundle": None, "enabled": True},
        {"name": "off", "label": "Off", "brief": "b", "slack_bundle": None, "enabled": False},
        {"name": "rust", "label": "Rust", "brief": "b", "slack_bundle": None},
    ]))
    (le / "loops.json").write_text(json.dumps([{
        "name": "topic-loop", "entry_point": "bin.topic_monitor_runner", "timeout_seconds": 60,
        "log_suffix": "-topic-loop", "emit_run_events": False, "enabled": True,
        "schedule": {"frequency": "daily", "hour": 1, "minute": 0}}]))
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    claude = fake_bin / "claude"
    claude.write_text(OK_CLAUDE)
    claude.chmod(claude.stat().st_mode | stat.S_IEXEC)
    log = tmp_path / "log"
    log.mkdir()
    env = {"PATH": f"{fake_bin}:/usr/bin:/bin", "HOME": str(home), "LOOP_ENGINEERING_HOME": str(le),
           "LOOPX_ROOT": str(le), "LOOP_EVENTS_DIR": str(le / "events"),
           "LOOPX_NO_LOGIN_SHELL": "1", "FAKE_LOG": str(log)}
    yield {"env": env, "le": le, "hook": hook, "log": log, "claude": claude}
    hook.close()


def run(world):
    return subprocess.run([loopx_bin(), "topic", "run"], env=world["env"], capture_output=True,
                          text=True, timeout=120)


def calls(world):
    return ["\n".join(p.read_text().splitlines()) for p in sorted(world["log"].glob("args-*.txt"))]


def test_runs_one_session_per_enabled_topic_and_skips_disabled(world):
    r = run(world)
    assert r.returncode == 0, r.stderr
    got = calls(world)
    assert len(got) == 2
    assert "Process exactly one topic: 'ai'" in got[0] and "Process exactly one topic: 'rust'" in got[1]
    assert not any("'off'" in c for c in got)
    assert "--max-budget-usd" in got[0] and "--add-dir" not in got[0]
    assert "Bash(loopx topic seen*)" in got[0]
    results = sorted((world["le"] / "outputs" / "loop-runs").glob("*/result.json"))
    assert len(results) == 2
    assert all(json.loads(p.read_text())["final_state"] == "completed" for p in results)
    assert world["hook"].posts == [], "no alerts when everything completed"
    status = json.loads((world["le"] / "outputs" / "status" / "topic-loop.json").read_text())
    assert status["state"] == "idle"
    assert "briefing text" in (world["le"] / "outputs" / "history").glob("*-topic-loop.log").__next__().read_text()


def test_a_failed_topic_is_marked_failed_and_alerted_but_the_run_still_exits_zero(world):
    world["claude"].write_text("#!/bin/sh\necho 'rate limited' >&2\nexit 1\n")
    r = run(world)
    assert r.returncode == 0, r.stderr
    doc = json.loads((world["le"] / "outputs" / "topic-monitor" / "status.json").read_text())
    assert doc["topics"]["ai"]["state"] == "failed" and doc["topics"]["rust"]["state"] == "failed"
    assert any("Topic monitor loop" in t and "did not complete" in t for t in world["hook"].texts)
    assert "rate limited" in (world["le"] / "logs" / "loop-engineering.log").read_text()


def test_wrong_arity_to_the_entry_is_a_usage_error(world):
    r = subprocess.run([loopx_bin(), "_entry", "bin.topic_monitor_runner", "run_1", "extra"],
                       env=world["env"], capture_output=True, text=True)
    assert r.returncode == 2

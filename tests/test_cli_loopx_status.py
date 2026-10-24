"""`loopx status` (replaces dashboard_server.py write-status / write-topic-status).

Rust-only: the Python writers resolve their paths from the checkout and can't
be sandboxed, so this exercises `loopx` alone and skips without LOOPX_BIN.
"""
import json
import subprocess

import pytest

from cli_runner import loopx_bin

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")


def loopx(tmp_path, *args):
    return subprocess.run(
        [loopx_bin(), "status", *args], capture_output=True, text=True,
        env={"PATH": "/usr/bin:/bin", "LOOP_ENGINEERING_HOME": str(tmp_path)},
    )


def test_write_defaults_to_the_gitlab_loop_file(tmp_path):
    r = loopx(tmp_path, "write", "running", "--pid", "42", "--current-issue", "web #7")
    assert r.returncode == 0, r.stderr
    doc = json.loads((tmp_path / "outputs" / "status.json").read_text())
    assert list(doc) == ["state", "updated_at", "pid", "current_issue"]
    assert doc["pid"] == 42 and doc["state"] == "running"


def test_other_loops_get_their_own_file_and_exit_code_key(tmp_path):
    loopx(tmp_path, "write", "failed", "--loop", "topic-loop", "--exit-code", "3")
    doc = json.loads((tmp_path / "outputs" / "status" / "topic-loop.json").read_text())
    assert doc["last_exit_code"] == 3


def test_each_write_replaces_the_document(tmp_path):
    loopx(tmp_path, "write", "running", "--pid", "1")
    loopx(tmp_path, "write", "idle")
    assert "pid" not in json.loads((tmp_path / "outputs" / "status.json").read_text())


def test_topic_status_updates_one_entry(tmp_path):
    loopx(tmp_path, "topic", "ai", "running", "--current-step", "searching")
    loopx(tmp_path, "topic", "rust", "idle")
    doc = json.loads((tmp_path / "outputs" / "topic-monitor" / "status.json").read_text())
    assert doc["topics"]["ai"]["current_step"] == "searching"
    assert doc["topics"]["rust"]["state"] == "idle"


def test_usage_errors_exit_1(tmp_path):
    assert loopx(tmp_path).returncode == 1
    assert loopx(tmp_path, "write").returncode == 1
    assert loopx(tmp_path, "write", "idle", "--pid", "x").returncode == 1
    assert loopx(tmp_path, "topic", "only-name").returncode == 1

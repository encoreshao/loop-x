"""Black-box contract for `dashboard_server.py read-messages|add-message` /
`loopx messages read|add`, against a scratch checkout (a copy of bin/ for Python,
LOOPX_ROOT for Rust). Run against Rust with LOOPX_BIN=target/debug/loopx."""
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent


@pytest.fixture
def checkout(tmp_path):
    root = tmp_path / "checkout"
    root.mkdir()
    if not os.environ.get("LOOPX_BIN"):
        shutil.copytree(ROOT / "bin", root / "bin", ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    return root


def cli(checkout, what, *args):
    env = {**os.environ, "LOOPX_ROOT": str(checkout), "LOOP_ENGINEERING_HOME": str(checkout / "home")}
    binary = os.environ.get("LOOPX_BIN")
    if binary:
        cmd = [binary, "messages", {"read-messages": "read", "add-message": "add"}[what], *args]
    else:
        cmd = [sys.executable, str(checkout / "bin" / "web" / "dashboard_server.py"), what, *args]
    return subprocess.run(cmd, env=env, capture_output=True, text=True)


def messages(checkout):
    return json.loads((checkout / "outputs" / "messages.json").read_text())


def test_read_on_an_empty_checkout_prints_an_empty_array(checkout):
    r = cli(checkout, "read-messages")
    assert r.returncode == 0 and r.stdout == "[]\n"


def test_add_opens_a_session_and_read_pops_user_messages_once(checkout):
    assert cli(checkout, "add-message", "user", "hello café").returncode == 0
    assert cli(checkout, "add-message", "loop", "hi back").returncode == 0
    msgs = messages(checkout)
    assert [m["from"] for m in msgs] == ["user", "loop"]
    assert list(msgs[0]) == ["from", "text", "timestamp", "seen_by_loop", "session"]
    assert list(msgs[1]) == ["from", "text", "timestamp", "session"]
    assert re.fullmatch(r"[0-9a-f]{12}", msgs[0]["session"]) and msgs[0]["session"] == msgs[1]["session"]
    assert msgs[0]["seen_by_loop"] is False
    sessions = json.loads((checkout / "outputs" / "chat-sessions.json").read_text())
    assert sessions["current"] == msgs[0]["session"]
    assert sessions["sessions"][0]["title"] == "hello café"
    assert sessions["sessions"][0]["title_source"] == "auto"

    first = cli(checkout, "read-messages")
    popped = json.loads(first.stdout)
    assert [m["text"] for m in popped] == ["hello café"]
    assert "\\u00e9" in first.stdout and first.stdout.startswith("[\n  {\n    \"from\": \"user\"")
    assert cli(checkout, "read-messages").stdout == "[]\n"
    assert messages(checkout)[0]["seen_by_loop"] is True


def test_add_validates_its_arguments(checkout):
    assert cli(checkout, "add-message", "user").returncode == 1
    assert cli(checkout, "add-message", "robot", "x").returncode == 1
    assert not (checkout / "outputs" / "messages.json").exists()

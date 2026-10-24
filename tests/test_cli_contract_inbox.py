"""Contract for `inbox_triage_runner.py <run_id>` (Rust: `loopx inbox run <run_id>`),
run against both the Python script and `loopx` (set LOOPX_BIN). Only the paths
that need no mail provider: usage, missing config, and a config with nothing
enabled. The full run against a fake provider is tests/test_e2e_loopx_inbox.py."""
import json
import shutil
from pathlib import Path

from cli_runner import run_cli

REPO = Path(__file__).resolve().parent.parent


def sandbox(tmp_path, inboxes=None):
    le = tmp_path / "le"
    (le / "loops" / "inbox-triage").mkdir(parents=True)
    shutil.copy(REPO / "loops" / "inbox-triage" / "loop.yaml", le / "loops" / "inbox-triage" / "loop.yaml")
    if inboxes is not None:
        (le / "inboxes.json").write_text(json.dumps({"inboxes": inboxes}))
    env = {"HOME": str(tmp_path / "home"), "LOOP_ENGINEERING_HOME": str(le), "LOOPX_ROOT": str(le),
           "LOOP_EVENTS_DIR": str(le / "events"), "LOOPX_KEYCHAIN": f"file:{tmp_path / 'kc'}"}
    return env


def test_usage_without_a_run_id_exits_2(tmp_path):
    r = run_cli("inbox_triage_runner.py", env=sandbox(tmp_path))
    assert r.returncode == 2
    assert "Usage" in r.stderr


def test_too_many_arguments_exit_2(tmp_path):
    assert run_cli("inbox_triage_runner.py", "a", "b", env=sandbox(tmp_path)).returncode == 2


def test_missing_inbox_config_fails(tmp_path):
    r = run_cli("inbox_triage_runner.py", "run_1", env=sandbox(tmp_path))
    assert r.returncode == 1
    assert "inboxes.json" in r.stderr


def test_nothing_enabled_is_a_quiet_success(tmp_path):
    inbox = {"name": "off", "label": "Off", "provider": "gmail", "account": "me@example.com", "enabled": False}
    env = sandbox(tmp_path, [inbox])
    r = run_cli("inbox_triage_runner.py", "run_1", env=env)
    assert r.returncode == 0, r.stderr
    assert not (tmp_path / "le" / "events").exists() or not list((tmp_path / "le" / "events").glob("*.jsonl"))

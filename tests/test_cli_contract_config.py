"""Black-box contract for `loop_config.py` / `loopx config`.

Runs against the Python script by default and against Rust with
`LOOPX_BIN=target/debug/loopx python3 -m pytest tests/test_cli_contract_config.py`.
"""
import json

import pytest

from cli_runner import run_cli

CONFIG = {
    "gitlab_instance": "gitlab.example.com",
    "assignee_username": "dev",
    "worktree_root": "/tmp/worktrees",
    "projects": {
        "alpha": {"local_path": "/src/alpha", "repo": "g/alpha"},
        "beta": {"local_path": "/src/beta", "repo": "g/beta", "instance": "other.example.com"},
    },
}


@pytest.fixture
def home(tmp_path):
    (tmp_path / "projects.json").write_text(json.dumps(CONFIG))
    return {"LOOP_ENGINEERING_HOME": str(tmp_path)}


def test_aliases(home):
    r = run_cli("loop_config.py", "aliases", env=home)
    assert r.returncode == 0
    assert r.stdout == "alpha\nbeta\n"


def test_local_paths(home):
    r = run_cli("loop_config.py", "local-paths", env=home)
    assert r.stdout == "/src/alpha\n/src/beta\n"


@pytest.mark.parametrize("command,expected", [
    ("worktree-root", "/tmp/worktrees"),
    ("assignee", "dev"),
    ("instance", "gitlab.example.com"),
])
def test_scalar_commands(home, command, expected):
    r = run_cli("loop_config.py", command, env=home)
    assert r.returncode == 0
    assert r.stdout == expected + "\n"


def test_project_resolves_default_instance(home):
    r = run_cli("loop_config.py", "project", "alpha", env=home)
    assert r.returncode == 0
    assert json.loads(r.stdout) == {**CONFIG["projects"]["alpha"], "instance": "gitlab.example.com"}


def test_project_keeps_own_instance(home):
    r = run_cli("loop_config.py", "project", "beta", env=home)
    assert json.loads(r.stdout)["instance"] == "other.example.com"


def test_project_output_is_indented_two_spaces(home):
    r = run_cli("loop_config.py", "project", "alpha", env=home)
    assert r.stdout.startswith('{\n  "local_path"')


def test_no_args_prints_usage_and_exits_1(home):
    r = run_cli("loop_config.py", env=home)
    assert r.returncode == 1
    assert "Usage" in r.stderr


def test_unknown_command_exits_1(home):
    r = run_cli("loop_config.py", "bogus", env=home)
    assert r.returncode == 1
    assert "Unknown command: bogus" in r.stderr


def test_project_without_alias_exits_1(home):
    r = run_cli("loop_config.py", "project", env=home)
    assert r.returncode == 1


def test_missing_config_fails_nonzero(tmp_path):
    r = run_cli("loop_config.py", "aliases", env={"LOOP_ENGINEERING_HOME": str(tmp_path)})
    assert r.returncode != 0
    assert "No project config" in r.stderr

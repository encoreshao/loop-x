"""Black-box contract for `loops_config.py` / `loopx loops` (the CLI
run-loop-now.sh uses) and `ai_cli_config.py` / `loopx ai-cli`. Run against Rust
with LOOPX_BIN=target/debug/loopx.
"""
import json

import pytest

from cli_runner import run_cli

LOOPS = [
    {"name": "gitlab-loop", "entry_point": "bin.gitlab_loop_runner", "timeout_seconds": 43200,
     "log_suffix": "", "emit_run_events": True, "enabled": True},
    {"name": "topic-loop", "entry_point": "bin/topic runner.py", "timeout_seconds": 3600,
     "log_suffix": "-topic's"},
    {"name": "no-entry", "timeout_seconds": 10},
]


@pytest.fixture
def le(tmp_path):
    (tmp_path / "loops.json").write_text(json.dumps(LOOPS))
    # An empty template so the checkout's real one never backfills loops.
    (tmp_path / "template.json").write_text("[]")
    return {"LOOP_ENGINEERING_HOME": str(tmp_path), "LOOP_LOOPS_TEMPLATE": str(tmp_path / "template.json")}


def loops(env, *args):
    return run_cli("loops_config.py", *args, env=env)


def test_names_lists_in_order(le):
    r = loops(le, "names")
    assert r.returncode == 0
    assert r.stdout == "gitlab-loop\ntopic-loop\nno-entry\n"


def test_template_loops_are_appended(le, tmp_path):
    (tmp_path / "template.json").write_text(json.dumps([{"name": "gitlab-loop"}, {"name": "extra-loop"}]))
    assert loops(le, "names").stdout == "gitlab-loop\ntopic-loop\nno-entry\nextra-loop\n"


def test_single_field_commands(le):
    assert loops(le, "entry-point", "gitlab-loop").stdout == "bin.gitlab_loop_runner\n"
    assert loops(le, "timeout-seconds", "gitlab-loop").stdout == "43200\n"
    assert loops(le, "log-suffix", "topic-loop").stdout == "-topic's\n"
    assert loops(le, "log-suffix", "gitlab-loop").stdout == "\n"
    assert loops(le, "emit-run-events", "gitlab-loop").stdout == "true\n"
    assert loops(le, "emit-run-events", "topic-loop").stdout == "false\n"


def test_bash_env_is_shell_quoted(le):
    r = loops(le, "bash-env", "topic-loop")
    assert r.returncode == 0
    assert r.stdout.splitlines() == [
        "ENTRY_POINT='bin/topic runner.py'",
        "TIMEOUT_SECONDS=3600",
        "LOG_SUFFIX='-topic'\"'\"'s'",
        "EMIT_RUN_EVENTS=false",
    ]
    r = loops(le, "bash-env", "gitlab-loop")
    assert r.stdout.splitlines() == [
        "ENTRY_POINT=bin.gitlab_loop_runner", "TIMEOUT_SECONDS=43200", "LOG_SUFFIX=''", "EMIT_RUN_EVENTS=true",
    ]


def test_bash_env_reports_missing_registry_field(le):
    r = loops(le, "bash-env", "no-entry")
    assert r.returncode == 1
    assert "missing required registry field 'entry_point'" in r.stderr


def test_unknown_loop_exits_1_with_keyerror_text(le):
    r = loops(le, "entry-point", "nope")
    assert r.returncode == 1
    assert r.stderr.strip() == "\"No loop named 'nope' in the loops registry\""


def test_invalid_loop_name_exits_1(le):
    r = loops(le, "entry-point", "../etc")
    assert r.returncode == 1
    assert "Invalid loop name" in r.stderr


def test_missing_registry_exits_1(tmp_path):
    r = loops({"LOOP_ENGINEERING_HOME": str(tmp_path / "nothing")}, "names")
    assert r.returncode == 1
    assert "No loops config at" in r.stderr or "FileNotFoundError" in r.stderr


def test_usage_errors_exit_1(le):
    assert loops(le).returncode == 1
    r = loops(le, "entry-point")
    assert r.returncode == 1 and "Usage" in r.stderr
    r = loops(le, "frobnicate", "gitlab-loop")
    assert r.returncode == 1 and "Unknown command: frobnicate" in r.stderr


def ai(env, *args):
    return run_cli("ai_cli_config.py", *args, env=env)


def test_ai_cli_defaults_to_claude_and_set_get_round_trip(tmp_path):
    env = {"LOOP_ENGINEERING_HOME": str(tmp_path)}
    assert ai(env, "get").stdout == "claude\n"
    r = ai(env, "set", "codex")
    assert r.returncode == 0 and r.stdout == "Switched to codex\n"
    assert json.loads((tmp_path / "ai_cli.json").read_text()) == {"cli": "codex"}
    assert ai(env, "get").stdout == "codex\n"


def test_ai_cli_rejects_unknown_and_garbage_falls_back(tmp_path):
    env = {"LOOP_ENGINEERING_HOME": str(tmp_path)}
    r = ai(env, "set", "gpt5")
    assert r.returncode == 1 and r.stderr == "Unknown AI CLI: gpt5\n"
    assert not (tmp_path / "ai_cli.json").exists()
    (tmp_path / "ai_cli.json").write_text("{nope")
    assert ai(env, "get").stdout == "claude\n"


def test_ai_cli_usage(tmp_path):
    env = {"LOOP_ENGINEERING_HOME": str(tmp_path)}
    assert ai(env).returncode == 1
    r = ai(env, "set")
    assert r.returncode == 1 and "Usage" in r.stderr
    r = ai(env, "bogus")
    assert r.returncode == 1 and "Unknown command: bogus" in r.stderr

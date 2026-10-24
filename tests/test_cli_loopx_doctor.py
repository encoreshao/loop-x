"""`loopx doctor [--json]`: first-launch checks for the Mac app and the terminal.

Rust-only (new command, no Python counterpart): skips without LOOPX_BIN. Runs
against a scratch HOME and a PATH holding only fake `claude`/`codex` scripts,
so it never sees this machine's real CLIs or credentials.
"""
import json
import os
import subprocess

import pytest

from cli_runner import loopx_bin

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")


def fake_cli(bin_dir, name, body):
    bin_dir.mkdir(exist_ok=True)
    p = bin_dir / name
    p.write_text(f"#!/bin/sh\n{body}\n")
    p.chmod(0o755)


@pytest.fixture
def machine(tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    return {"home": home, "bin": tmp_path / "bin", "tmp": tmp_path}


def doctor(machine, *args, extra_env=None):
    env = {"PATH": str(machine["bin"]), "HOME": str(machine["home"])}
    env.update(extra_env or {})
    return subprocess.run(
        [loopx_bin(), "doctor", *args], capture_output=True, text=True, env=env, timeout=60
    )


def report(machine, **kw):
    r = doctor(machine, "--json", **kw)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


def test_empty_machine_reports_everything_missing_with_next_steps(machine):
    r = report(machine)
    assert r["ready"] is False
    assert r["ai_clis"]["claude"]["found"] is False
    assert r["ai_clis"]["codex"]["found"] is False
    assert r["gitlab_skill"]["installed"] is False
    assert r["gitlab_config"]["exists"] is False
    assert r["backend"]["running"] is False
    assert any("Install an AI CLI" in s for s in r["next_steps"])


def test_human_output_has_hints_and_exits_zero(machine):
    r = doctor(machine)
    assert r.returncode == 0
    assert "Not ready" in r.stdout and "Next steps:" in r.stdout
    assert "claude: not found on PATH" in r.stdout


def test_finds_fake_clis_with_versions(machine):
    fake_cli(machine["bin"], "claude", "echo '9.9.9 (Claude Code)'")
    fake_cli(machine["bin"], "codex", "echo 'codex-cli 1.2.3'")
    r = report(machine)
    assert r["ready"] is True
    assert r["ai_clis"]["claude"]["version"] == "9.9.9 (Claude Code)"
    assert r["ai_clis"]["codex"]["version"] == "codex-cli 1.2.3"
    assert r["selected_cli"] == "claude"


def test_hanging_cli_does_not_hang_doctor(machine):
    fake_cli(machine["bin"], "claude", "exec sleep 60")
    r = report(machine)
    assert r["ai_clis"]["claude"]["found"] is True
    assert r["ai_clis"]["claude"]["version"] is None


def test_selected_cli_comes_from_ai_cli_json_under_loop_engineering_home(machine):
    fake_cli(machine["bin"], "claude", "echo 1")
    home = machine["tmp"] / "loophome"
    home.mkdir()
    (home / "ai_cli.json").write_text('{"cli": "codex"}')
    r = report(machine, extra_env={"LOOP_ENGINEERING_HOME": str(home)})
    assert r["selected_cli"] == "codex"
    assert r["ready"] is False
    assert r["loop_home"] == str(home)


def test_credentials_are_checked_but_tokens_never_printed(machine):
    (machine["home"] / ".gitlab").mkdir()
    (machine["home"] / ".gitlab" / "config.json").write_text('{"token": "glpat-TOPSECRET"}')
    (machine["home"] / ".slack").mkdir()
    (machine["home"] / ".slack" / "config.json").write_text("{broken")
    for args in (["--json"], []):
        out = doctor(machine, *args).stdout
        assert "glpat-TOPSECRET" not in out
    r = report(machine)
    assert r["gitlab_config"] == {
        "path": str(machine["home"] / ".gitlab" / "config.json"), "exists": True, "valid": True,
    }
    assert r["slack_config"]["valid"] is False


def test_skill_dir_and_loop_config_files(machine):
    (machine["home"] / ".encore-skills" / "skills" / "gitlab-config").mkdir(parents=True)
    cfg = machine["home"] / ".loopx"
    cfg.mkdir()
    for n in ("projects.json", "loops.json", "topics.json"):
        (cfg / n).write_text("{}")
    r = report(machine)
    assert r["gitlab_skill"]["installed"] is True
    assert all(r["config"][k]["exists"] and r["config"][k]["valid"] for k in ("projects", "loops", "topics"))


def test_live_and_stale_lock(machine):
    cfg = machine["home"] / ".loopx"
    cfg.mkdir()
    lock = cfg / "loopx.lock"
    body = lambda pid: json.dumps(
        {"pid": pid, "port": 48421, "version": "0.4.0", "started_at": "t", "token": "tok-SECRET"}
    )
    lock.write_text(body(os.getpid()))
    r = report(machine)
    assert r["backend"]["running"] is True and r["backend"]["port"] == 48421
    assert "tok-SECRET" not in json.dumps(r)
    lock.write_text(body(4_000_000_000))
    r = report(machine)
    assert r["backend"]["running"] is False and r["backend"]["stale_lock"] is True


def test_rejects_unknown_arguments(machine):
    assert doctor(machine, "--nope").returncode == 2

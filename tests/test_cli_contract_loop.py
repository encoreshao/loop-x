"""Black-box contract for `loop_cli.py` / `loopx loop`: init, validate, audit,
doctor, run, status, inspect, replay, cost, eval. Runs on Python by default and
on Rust with LOOPX_BIN. Never runs the paid --golden/--chat suites."""
import json
import os
import re
from pathlib import Path

import pytest
import yaml

from cli_runner import run_cli

REPO = Path(__file__).resolve().parent.parent
TEMPLATES = sorted(p.parent.name for p in (REPO / "templates").glob("*/loop.yaml"))
LOOPS = sorted(str(p) for p in (REPO / "loops").glob("*/loop.yaml"))


def write_def(path, **over):
    data = {
        "name": "cli-test-loop", "version": 1, "trigger": {"type": "manual"},
        "goal": {"type": "self_check"}, "actions": ["run_tests"],
        "verification": {"required": ["tests"]},
        "verifiers": [{"name": "tests", "type": "command", "command": "true"}],
        "stop_conditions": {"max_iterations": 1, "max_runtime_minutes": 5, "max_cost_usd": 1,
                            "no_progress_iterations": 1},
        "retry": {"enabled": False, "max_attempts": 1}, "human_gates": [],
    }
    data.update(over)
    path.write_text(yaml.safe_dump(data))
    return path


@pytest.fixture
def env(tmp_path):
    return {"LOOP_ENGINEERING_HOME": str(tmp_path / "home"), "LOOP_EVENTS_DIR": str(tmp_path / "ev"),
            "LOOP_EVALS_DIR": str(tmp_path / "evals"), "LOOP_UPDATE_DIR": str(tmp_path / "upd")}


def cli(*args, env, **kw):
    return run_cli("loop_cli.py", *args, env=env, **kw)


def test_usage_exit_2(env):
    assert cli(env=env).returncode == 2
    assert cli("nope", env=env).returncode == 2


def test_init_default_and_refuse_and_force(tmp_path, env):
    r = cli("init", "--dir", str(tmp_path), env=env)
    assert r.returncode == 0
    assert r.stdout.strip() == f"Created {tmp_path}/.loop/loop.yaml"
    assert yaml.safe_load((tmp_path / ".loop/loop.yaml").read_text())["name"]
    r = cli("init", "--dir", str(tmp_path), env=env)
    assert r.returncode == 1 and "already exists" in r.stderr
    assert cli("init", "--dir", str(tmp_path), "--force", env=env).returncode == 0


def test_init_unknown_template_lists_choices(tmp_path, env):
    r = cli("init", "--dir", str(tmp_path), "--template", "nope", env=env)
    assert r.returncode == 1
    assert "unknown template 'nope'" in r.stderr and "generic" in r.stderr


@pytest.mark.parametrize("name", TEMPLATES)
def test_every_template_inits_validates_and_audits(tmp_path, env, name):
    assert cli("init", "--dir", str(tmp_path), "--template", name, env=env).returncode == 0
    f = str(tmp_path / ".loop/loop.yaml")
    assert (tmp_path / ".loop/loop.yaml").read_text() == (REPO / "templates" / name / "loop.yaml").read_text()
    assert cli("validate", f, env=env).returncode == 0
    r = cli("audit", f, env=env)
    assert r.returncode == 0 and "FAIL" not in r.stdout


def test_validate_output(tmp_path, env):
    r = cli("validate", str(write_def(tmp_path / "l.yaml")), env=env)
    assert r.returncode == 0
    assert r.stdout.splitlines()[0] == "✓ trigger configured"
    assert "✗ no human gates configured" in r.stdout
    assert r.stdout.rstrip().endswith("Loop configuration valid.")
    bad = tmp_path / "bad.yaml"
    bad.write_text(yaml.safe_dump({"name": "x", "version": 1}))
    assert cli("validate", str(bad), env=env).returncode == 1
    assert cli("validate", env=env).returncode == 2


def test_audit_output_and_exit_codes(tmp_path, env):
    f = str(write_def(tmp_path / "l.yaml"))
    r = cli("audit", f, env=env)
    assert r.returncode == 0
    assert r.stdout.splitlines()[0] == "PASS  goal                   goal.type='self_check'"
    assert re.search(r"Loop Ready Score: \d+(\.\d)? / 100\n$", r.stdout)
    assert cli("audit", f, "--min-score", "101", env=env).returncode == 1
    assert cli("audit", env=env).returncode == 2
    two = cli("audit", f, f, env=env)
    assert two.stdout.count("== ") == 2 and two.stdout.count("Loop Ready Score") == 2


@pytest.mark.parametrize("path", LOOPS)
def test_real_loops_meet_the_ci_gate(env, path):
    r = cli("audit", path, "--min-score", "70", env=env)
    assert r.returncode == 0, r.stdout


def test_doctor_surfaces_fail_first(tmp_path, env):
    f = write_def(tmp_path / "l.yaml", permissions={"credentials_read": True})
    r = cli("doctor", str(f), env=env)
    assert r.returncode == 0 and r.stdout.startswith("Loop Health: ")
    assert any("credential_boundary" in l and "[FAIL]" in l for l in r.stdout.splitlines()[-3:])


def run_id_of(r):
    return [l for l in r.stdout.splitlines() if l.startswith("run_id:")][0].split()[-1]


def test_run_status_inspect_replay_cost(tmp_path, env):
    f = str(write_def(tmp_path / "l.yaml"))
    results = tmp_path / "results"
    r = cli("run", f, "--results-dir", str(results), env=env)
    assert r.returncode == 0, r.stderr
    assert "final_state: completed" in r.stdout and "stop_reason: completed" in r.stdout
    rid = run_id_of(r)
    data = json.loads((results / rid / "result.json").read_text())
    assert data["status"] == "finished" and data["prompt"] is None
    assert data["definition_path"] == str(Path(f).resolve())
    s = cli("status", "--results-dir", str(results), env=env)
    assert s.stdout.splitlines()[0] == "cli-test-loop" and "Status: COMPLETED" in s.stdout
    assert f"Run: {rid}" in s.stdout and "Iterations: 1" in s.stdout
    i = cli("inspect", rid, "--results-dir", str(results), env=env)
    assert i.stdout.splitlines()[-1] == "    [PASS] tests"
    assert cli("inspect", "nope", "--results-dir", str(results), env=env).returncode == 1
    rp = cli("replay", rid, "--results-dir", str(results), env=env)
    assert rp.returncode == 0 and "no prompt recorded" in rp.stdout
    # events were written under LOOP_EVENTS_DIR and feed the cost report
    c = cli("cost", "--events-dir", env["LOOP_EVENTS_DIR"], env=env)
    assert c.returncode == 0 and re.search(r"Runs\s+1\n", c.stdout)
    assert cli("cost", "--results-dir", str(results), env=env).returncode == 2


def test_run_failing_verifier_exits_1(tmp_path, env):
    f = write_def(tmp_path / "l.yaml", verifiers=[{"name": "tests", "type": "command", "command": "false"}])
    r = cli("run", str(f), "--results-dir", str(tmp_path / "r"), env=env)
    assert r.returncode == 1 and "final_state: escalated" in r.stdout


def test_run_policy_violation_refuses(tmp_path, env):
    f = write_def(tmp_path / "l.yaml", actions=["merge"])
    r = cli("run", str(f), "--results-dir", str(tmp_path / "r"), env=env)
    assert r.returncode == 1 and "policy violation" in r.stderr


def fake_claude(tmp_path, body):
    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    p = bin_dir / "claude"
    p.write_text("#!/bin/sh\n" + body + "\n")
    p.chmod(0o755)
    return {"PATH": f"{bin_dir}:{os.environ['PATH']}"}


OK_CLAUDE = """echo '{"result": "done", "total_cost_usd": 0.05, "usage": {}, "modelUsage": {}}'"""


def test_run_with_prompt_invokes_agent_and_replay_reruns(tmp_path, env):
    env = {**env, **fake_claude(tmp_path, OK_CLAUDE)}
    f = write_def(tmp_path / "l.yaml")
    results = tmp_path / "results"
    r = cli("run", str(f), "--results-dir", str(results), "--prompt", "fix it", env=env)
    assert r.returncode == 0, r.stderr
    rid = run_id_of(r)
    data = json.loads((results / rid / "result.json").read_text())
    assert data["prompt"] == "fix it"
    rp = cli("replay", rid, "--results-dir", str(results), env=env)
    assert rp.returncode == 0 and "replayed run_id: " + rid in rp.stdout
    assert len(list(results.glob("*/result.json"))) == 2


def test_empty_prompt_is_still_an_agent_run(tmp_path, env):
    env = {**env, **fake_claude(tmp_path, OK_CLAUDE)}
    f = write_def(tmp_path / "l.yaml")
    results = tmp_path / "results"
    rid = run_id_of(cli("run", str(f), "--results-dir", str(results), "--prompt", "", env=env))
    rp = cli("replay", rid, "--results-dir", str(results), env=env)
    assert "no prompt recorded" not in rp.stdout and rp.returncode == 0


def test_prompt_file_and_agent_failure(tmp_path, env):
    env = {**env, **fake_claude(tmp_path, "echo boom; exit 1")}
    f = write_def(tmp_path / "l.yaml")
    pf = tmp_path / "p.txt"
    pf.write_text("x")
    r = cli("run", str(f), "--results-dir", str(tmp_path / "r"), "--prompt-file", str(pf), env=env)
    assert r.returncode == 1 and "agent invocation" in r.stderr
    r = cli("run", str(f), "--prompt-file", str(tmp_path / "missing"), env=env)
    assert r.returncode == 1 and "could not read --prompt-file" in r.stderr


def test_status_no_runs_and_running_snapshot(tmp_path, env):
    r = cli("status", "--results-dir", str(tmp_path / "none"), env=env)
    assert r.stdout.strip() == "No runs yet."
    d = tmp_path / "results" / "run_x"
    d.mkdir(parents=True)
    (d / "result.json").write_text(json.dumps({
        "run_id": "run_x", "definition_name": "live", "final_state": "running", "stop_reason": "running",
        "status": "running", "iterations": [{"iteration": 2, "state": "evaluating", "verification_results": [],
        "budget": {"iterations": {"used": 2, "limit": 3}, "cost": {"used_usd": 0.73}}, "progressed": True}]}))
    r = cli("status", "--results-dir", str(tmp_path / "results"), env=env)
    assert "Status: RUNNING" in r.stdout and "Iteration: 2/3" in r.stdout
    assert "Cost: $0.73" in r.stdout and "██████░░░░ 67%" in r.stdout


def test_scripted_eval_default_records_last_json(tmp_path, env):
    r = cli("eval", env=env)
    assert r.returncode == 0 and "8/8 cases passed" in r.stdout and "FAIL" not in r.stdout
    last = json.loads((tmp_path / "evals" / "last.json").read_text())
    assert len(last["results"]) == 8 and all(x["passed"] for x in last["results"])


def test_eval_custom_dir_fails_and_does_not_record(tmp_path, env):
    d = tmp_path / "cases"
    d.mkdir()
    (d / "wrong.yaml").write_text(yaml.safe_dump({
        "name": "wrong", "description": "d",
        "definition": {"name": "e", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "fix"}},
        "script": {"agent": [{"changed": True, "cost_usd": 0.0}], "verifiers": {"tests": [True]}},
        "expect": {"final_state": "escalated", "stop_reason": "no_progress"}}))
    r = cli("eval", str(d), env=env)
    assert r.returncode == 1 and "FAIL  wrong" in r.stdout and "0/1 cases passed" in r.stdout
    assert not (tmp_path / "evals").exists()


def test_eval_dir_errors(tmp_path, env):
    r = cli("eval", "/no/such/dir", env=env)
    assert r.returncode == 2 and "no such directory" in r.stderr
    (tmp_path / "empty").mkdir()
    r = cli("eval", str(tmp_path / "empty"), env=env)
    assert r.returncode == 2 and "no case files found" in r.stderr


def test_paid_suite_argument_errors_never_launch_a_run(tmp_path, env):
    assert cli("eval", "--golden", "--budget-usd", "0", env=env).returncode == 2
    assert cli("eval", "--golden", "--budget-usd", "lots", env=env).returncode == 2
    assert cli("eval", "--golden", "--case", "no-such-case", env=env).returncode == 2
    assert cli("eval", "--chat", "--case", "no-such-case", env=env).returncode == 2

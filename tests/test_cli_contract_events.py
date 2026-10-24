"""Black-box contract for `events.py` / `loopx events` and the
`loop_cli.py ledger backfill` / `loopx ledger backfill` subcommand. Run against
Rust with LOOPX_BIN=target/debug/loopx.
"""
import json
import re

import pytest

from cli_runner import loopx_bin, run_cli


@pytest.fixture
def ev(tmp_path):
    d = tmp_path / "events"
    return d, {"LOOP_EVENTS_DIR": str(d)}


def files(d):
    return sorted(d.glob("*.jsonl"))


def lines(d):
    return [l for f in files(d) for l in f.read_text().splitlines()]


def test_no_args_and_unknown_command_exit_1(ev):
    d, env = ev
    assert run_cli("events.py", env=env).returncode == 1
    r = run_cli("events.py", "bogus", env=env)
    assert r.returncode == 1 and "unknown command 'bogus'" in r.stderr


def test_emit_requires_type_and_run_id(ev):
    d, env = ev
    r = run_cli("events.py", "emit", "--run-id", "r", env=env)
    assert r.returncode == 1 and "--type is required" in r.stderr
    r = run_cli("events.py", "emit", "--type", "t", env=env)
    assert r.returncode == 1 and "--run-id is required" in r.stderr
    r = run_cli("events.py", "emit", "--type", "t", "--run-id", "r", "--data", "{nope", env=env)
    assert r.returncode == 1 and "not valid JSON" in r.stderr
    assert files(d) == []


def test_emit_writes_one_python_style_line(ev):
    d, env = ev
    r = run_cli("events.py", "emit", "--type", "run.started", "--run-id", "run_1", env=env)
    assert r.returncode == 0 and r.stdout == ""
    [line] = lines(d)
    event = json.loads(line)
    assert list(event) == ["schema_version", "event_id", "timestamp", "event_type", "run_id",
                           "issue_run_id", "project", "issue_iid", "data"]
    assert event["schema_version"] == 1 and event["data"] == {}
    assert re.fullmatch(r"evt_[0-9a-f]{32}", event["event_id"])
    assert re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z", event["timestamp"])
    assert files(d)[0].name == event["timestamp"][:10] + ".jsonl"
    # json.dumps default separators
    assert line.startswith('{"schema_version": 1, "event_id": "evt_')
    assert '"issue_run_id": null, "project": null, "issue_iid": null, "data": {}}' in line


def test_emit_optional_fields_and_ascii_escaping(ev):
    d, env = ev
    r = run_cli("events.py", "emit", "--type", "issue.completed", "--run-id", "r1",
                "--issue-run-id", "r1_p_1", "--project", "kurrant", "--issue-iid", "123",
                "--data", '{"note": "é", "n": [1, 2.5, null]}', env=env)
    assert r.returncode == 0
    [line] = lines(d)
    assert '"data": {"note": "\\u00e9", "n": [1, 2.5, null]}}' in line
    event = json.loads(line)
    assert (event["issue_run_id"], event["project"], event["issue_iid"]) == ("r1_p_1", "kurrant", 123)


def test_emit_data_null_becomes_empty_object_and_appends(ev):
    d, env = ev
    run_cli("events.py", "emit", "--type", "a", "--run-id", "r", "--data", "null", env=env, check=True)
    run_cli("events.py", "emit", "--type", "b", "--run-id", "r", env=env, check=True)
    events = [json.loads(l) for l in lines(d)]
    assert [e["event_type"] for e in events] == ["a", "b"] and events[0]["data"] == {}
    assert events[0]["event_id"] != events[1]["event_id"]


def test_emit_events_dir_flag_overrides_env(ev, tmp_path):
    d, env = ev
    other = tmp_path / "other"
    run_cli("events.py", "emit", "--type", "a", "--run-id", "r", "--events-dir", str(other), env=env, check=True)
    assert files(d) == [] and len(lines(other)) == 1


def seed(d):
    d.mkdir(parents=True)
    (d / "2026-10-01.jsonl").write_text(
        '{"run_id": "a", "event_type": "x", "n": 1}\n\nnot json\n{"run_id": "b", "event_type": "y", "s": "é"}\n')
    (d / "2026-10-02.jsonl").write_text('{"run_id": "a", "event_type": "z"}\n')


def test_list_requires_date_or_run_id(ev):
    d, env = ev
    r = run_cli("events.py", "list", env=env)
    assert r.returncode == 1 and "either --date or --run-id" in r.stderr


def test_list_by_run_id_across_files_skips_malformed(ev):
    d, env = ev
    seed(d)
    r = run_cli("events.py", "list", "--run-id", "a", env=env)
    assert r.returncode == 0
    assert r.stdout.splitlines() == ['{"run_id": "a", "event_type": "x", "n": 1}',
                                     '{"run_id": "a", "event_type": "z"}']
    assert "skipping malformed line" in r.stderr


def test_list_by_date_and_both_and_missing(ev):
    d, env = ev
    seed(d)
    r = run_cli("events.py", "list", "--date", "2026-10-01", env=env)
    assert r.stdout.splitlines() == ['{"run_id": "a", "event_type": "x", "n": 1}',
                                     '{"run_id": "b", "event_type": "y", "s": "\\u00e9"}']
    r = run_cli("events.py", "list", "--date", "2026-10-01", "--run-id", "b", env=env)
    assert r.stdout.splitlines() == ['{"run_id": "b", "event_type": "y", "s": "\\u00e9"}']
    r = run_cli("events.py", "list", "--date", "1999-01-01", env=env)
    assert r.returncode == 0 and r.stdout == ""
    r = run_cli("events.py", "list", "--run-id", "a", "--events-dir", str(d / "nope"), env=env)
    assert r.returncode == 0 and r.stdout == ""


# ---- ledger backfill (loop_cli.py ledger backfill / loopx ledger backfill) ----

def ledger(*args, env):
    return run_cli("loop_cli.py", "ledger", "backfill", *args, env=env)


def write_result(results, run_id, **over):
    result = {
        "loop_id": "l_" + run_id, "run_id": run_id, "definition_name": "test-loop",
        "final_state": "completed",
        "iterations": [{"iteration": 1, "state": "completed", "verification_results": [
            {"name": "tests", "passed": True, "exit_code": 0, "duration_ms": 1, "output": "x", "evidence": {}}],
            "budget": {"overall": "ok", "cost": {"used_usd": 1.5}, "runtime": {"used_seconds": 60.0}},
            "progressed": True}],
        "stop_reason": "completed", "prompt": None, "definition_path": None, "status": "finished",
    }
    result.update(over)
    (results / run_id).mkdir(parents=True)
    (results / run_id / "result.json").write_text(json.dumps(result, indent=2))


def test_ledger_backfill_appends_once(tmp_path, ev):
    d, env = ev
    results = tmp_path / "results"
    write_result(results, "run_20260901_100000_a")
    write_result(results, "run_20260902_100000_run", status="running")
    r = ledger("--results-dir", str(results), "--events-dir", str(d), env=env)
    assert r.returncode == 0 and r.stdout == "Backfilled 1 run(s) into the ledger\n"
    [line] = (d / "backfill-loop-result.jsonl").read_text().splitlines()
    event = json.loads(line)
    assert event["event_type"] == "loop.result" and event["timestamp"] == "2026-09-01T10:01:00.000Z"
    assert event["data"]["started_at"] == "2026-09-01T10:00:00.000Z"
    assert event["data"]["finished_at"] == "2026-09-01T10:01:00.000Z"
    assert event["data"]["duration_ms"] == 60000 and event["data"]["total_cost_usd"] == 1.5
    assert event["data"]["iterations"] == [{"n": 1, "state": "completed", "verifiers_passed": True, "cost_usd": 1.5}]
    assert event["data"]["verified_success"] is True and event["data"]["budget_overall"] == "ok"
    assert list(event["data"]) == ["run_id", "loop_id", "definition", "final_state", "stop_reason", "started_at",
                                   "finished_at", "duration_ms", "total_cost_usd", "iterations",
                                   "verified_success", "budget_overall"]
    r = ledger("--results-dir", str(results), "--events-dir", str(d), env=env)
    assert r.stdout == "Backfilled 0 run(s) into the ledger\n"
    assert len((d / "backfill-loop-result.jsonl").read_text().splitlines()) == 1


def test_ledger_backfill_zero_cost_is_unknown(tmp_path, ev):
    d, env = ev
    results = tmp_path / "results"
    write_result(results, "run_20260901_100000_z", iterations=[
        {"iteration": 1, "state": "completed", "verification_results": [],
         "budget": {"overall": "ok", "cost": {"used_usd": 0.0}, "runtime": {"used_seconds": 0.0}},
         "progressed": True}])
    ledger("--results-dir", str(results), "--events-dir", str(d), env=env)
    event = json.loads((d / "backfill-loop-result.jsonl").read_text())
    assert event["data"]["total_cost_usd"] is None
    assert event["data"]["iterations"][0]["cost_usd"] is None


def test_ledger_usage_exit_2(ev):
    d, env = ev
    r = run_cli("loop_cli.py", "ledger", env=env)
    assert r.returncode == 2

"""Black-box contract for `cost.py` / `loopx cost`. Run against Rust with LOOPX_BIN=target/debug/loopx."""
import json
from datetime import datetime, timedelta, timezone

from cli_runner import run_cli

CLAUDE = {
    "total_cost_usd": 0.14211259999999998,
    "usage": {"input_tokens": 2, "cache_creation_input_tokens": 34536,
              "cache_read_input_tokens": 19123, "output_tokens": 14},
    "modelUsage": {"claude-sonnet-5": {}},
    "result": "Hi! What can I help you with today?",
    "duration_ms": 3109,
}


def out_file(tmp_path, text):
    p = tmp_path / "out.json"
    p.write_text(text)
    return str(p)


def test_extract_result_text(tmp_path):
    r = run_cli("cost.py", "extract-result-text", "--cli-output-file", out_file(tmp_path, json.dumps(CLAUDE)))
    assert (r.returncode, r.stdout) == (0, "Hi! What can I help you with today?\n")
    r = run_cli("cost.py", "extract-result-text", "--cli-output-file", out_file(tmp_path, '{"x": 1}'))
    assert r.stdout == "(no result text in CLI output)\n"


def test_extract_result_text_passes_non_json_through(tmp_path):
    r = run_cli("cost.py", "extract-result-text", "--cli-output-file", out_file(tmp_path, "line1\nline2"))
    assert (r.returncode, r.stdout) == (0, "line1\nline2\n")
    r = run_cli("cost.py", "extract-result-text", "--cli-output-file", out_file(tmp_path, "[1, 2]"))
    assert r.stdout == "[1, 2]\n"
    r = run_cli("cost.py", "extract-result-text", "--cli-output-file", str(tmp_path / "missing"))
    assert (r.returncode, r.stdout) == (0, "\n")


def test_usage_json(tmp_path):
    r = run_cli("cost.py", "usage-json", "--cli-output-file", out_file(tmp_path, json.dumps(CLAUDE)))
    assert r.returncode == 0
    assert r.stdout == (
        '{"provider": "anthropic", "model": "claude-sonnet-5", "input_tokens": 2, "output_tokens": 14, '
        '"cache_read_tokens": 19123, "cache_write_tokens": 34536, "duration_ms": 3109, '
        '"cost_usd": 0.14211259999999998}\n'
    )


def test_usage_json_skips_with_stderr_note(tmp_path):
    for text, note in (('{"type": "thread.started"}', "missing total_cost_usd/usage"), ("not json", "not valid Claude JSON")):
        r = run_cli("cost.py", "usage-json", "--cli-output-file", out_file(tmp_path, text))
        assert (r.returncode, r.stdout) == (0, "")
        assert note in r.stderr
    r = run_cli("cost.py", "usage-json", "--cli-output-file", str(tmp_path / "missing"))
    assert (r.returncode, r.stdout) == (0, "")


def test_usage_errors():
    for sub in ("extract-result-text", "usage-json"):
        r = run_cli("cost.py", sub)
        assert r.returncode == 1 and "--cli-output-file is required" in r.stderr
    assert run_cli("cost.py").returncode == 1
    r = run_cli("cost.py", "bogus")
    assert r.returncode == 1 and "unknown command 'bogus'" in r.stderr


def write_events(tmp_path):
    d = tmp_path / "events"
    d.mkdir()
    evs = [
        {"event_type": "run.completed", "run_id": "r1", "data": {
            "cost_usd": 1234.5, "input_tokens": 1000, "output_tokens": 1,
            "cache_read_tokens": 3000, "cache_write_tokens": 0}},
        {"event_type": "issue.started", "run_id": "r1", "issue_run_id": "i1", "timestamp": "2026-09-04T10:00:00.000Z"},
        {"event_type": "issue.completed", "run_id": "r1", "issue_run_id": "i1", "timestamp": "2026-09-04T10:05:00.000Z"},
    ]
    (d / "2026-09-04.jsonl").write_text("\n".join(json.dumps(e) for e in evs) + "\n")
    return d


def test_report_all_time(tmp_path):
    d = write_events(tmp_path)
    r = run_cli("cost.py", "report", "--events-dir", str(d))
    assert r.returncode == 0
    lines = r.stdout.splitlines()
    assert lines[0] == "Loop Cost (all time)"
    assert "Total AI cost         $1,234.50" in lines
    assert "Total tokens          4,001" in lines
    assert "Cache hit rate        75.0%" in lines
    assert "Cost per issue         $1,234.50" in lines
    assert lines[-1].startswith("Wasted cost            N/A (no retry tracking")


def test_report_days_window_and_empty_dir(tmp_path):
    d = write_events(tmp_path)
    r = run_cli("cost.py", "report", "--days", "7", "--events-dir", str(d))
    today = datetime.now(timezone.utc).date()
    assert r.stdout.splitlines()[0] == f"Loop Cost ({(today - timedelta(days=6)).isoformat()} to {today.isoformat()})"
    assert "Total AI cost         $0.00" in r.stdout
    assert "Cache hit rate        N/A" in r.stdout


def test_report_errors(tmp_path):
    r = run_cli("cost.py", "report", "--days", "abc")
    assert r.returncode == 1 and "--days must be an integer, got 'abc'" in r.stderr
    r = run_cli("cost.py", "report", "--events-dir", str(tmp_path / "nope"))
    assert r.returncode == 1 and "does not exist" in r.stderr

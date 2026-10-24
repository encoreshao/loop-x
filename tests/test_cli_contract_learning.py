"""Black-box contract for `learning.py` / `loopx learning`. Run against Rust with LOOPX_BIN=target/debug/loopx."""
import json
from datetime import datetime, timedelta, timezone

from cli_runner import run_cli


def events_dir(tmp_path):
    d = tmp_path / "events"
    d.mkdir()
    ts = "2026-09-04T10:00:00.000Z"
    evs = [
        {"event_type": "memory.created", "project": "k", "timestamp": ts, "data": {"lesson_id": "lesson_1", "category": "testing"}},
        {"event_type": "issue.started", "issue_run_id": "a", "project": "k", "timestamp": ts},
        {"event_type": "issue.started", "issue_run_id": "b", "project": "k", "timestamp": ts},
        {"event_type": "memory.reused", "issue_run_id": "a", "project": "k", "timestamp": ts, "data": {"lesson_id": "lesson_1"}},
        {"event_type": "issue.completed", "issue_run_id": "a", "project": "k", "timestamp": ts},
        {"event_type": "memory.reused", "issue_run_id": "b", "project": "other", "timestamp": ts, "data": {"lesson_id": "lesson_2"}},
        {"event_type": "issue.escalated", "issue_run_id": "b", "project": "k", "timestamp": ts},
    ]
    (d / "2026-09-04.jsonl").write_text("\n".join(json.dumps(e) for e in evs) + "\n")
    return str(d)


def test_report_all_time(tmp_path):
    r = run_cli("learning.py", "--events-dir", events_dir(tmp_path))
    assert r.returncode == 0
    lines = r.stdout.splitlines()
    assert lines[:2] == ["Loop Learning (all time)", ""]
    assert "  Lessons created       1" in lines
    assert "  Total reuses          2" in lines
    assert "  Memory reuse rate     100.0%" in lines
    assert "  Memory success rate   50.0%" in lines
    assert "  lesson_1  (testing)  reused=1 effectiveness=100.0%" in lines
    assert "  lesson_2  (uncategorized)  reused=1 effectiveness=0.0%" in lines


def test_report_project_filter(tmp_path):
    r = run_cli("learning.py", "--project", "k", "--events-dir", events_dir(tmp_path))
    assert r.stdout.splitlines()[0] == "Loop Learning (all time) \u2014 project: k"
    assert "  Total reuses          1" in r.stdout
    assert "lesson_2" not in r.stdout


def test_days_window_and_empty(tmp_path):
    (tmp_path / "e").mkdir()
    r = run_cli("learning.py", "--days", "7", "--events-dir", str(tmp_path / "e"))
    today = datetime.now(timezone.utc).date()
    assert r.stdout.splitlines()[0] == f"Loop Learning ({(today - timedelta(days=6)).isoformat()} to {today.isoformat()})"
    assert r.stdout.splitlines()[-1] == "  N/A (no lessons reused in this window)"
    assert "Memory reuse rate     N/A" in r.stdout


def test_errors(tmp_path):
    r = run_cli("learning.py", "--days", "x")
    assert r.returncode == 1 and "--days must be an integer, got 'x'" in r.stderr
    r = run_cli("learning.py", "--events-dir", str(tmp_path / "nope"))
    assert r.returncode == 1 and "does not exist" in r.stderr

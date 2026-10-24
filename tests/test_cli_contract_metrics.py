"""`metrics.py` / `loopx metrics`."""
import json
import re

import pytest

from cli_runner import run_cli


def ev(t, **kw):
    base = {"schema_version": 1, "event_id": "e", "event_type": t, "run_id": None, "issue_run_id": None,
            "project": None, "issue_iid": None, "data": {}}
    base.update(kw)
    return json.dumps(base)


@pytest.fixture
def d(tmp_path):
    from datetime import datetime, timezone
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    lines = [
        ev("run.started", run_id="r1", timestamp=f"{today}T10:00:00.000Z"),
        ev("run.completed", run_id="r1", timestamp=f"{today}T10:00:02.000Z"),
        ev("issue.started", issue_run_id="i1", project="alpha", timestamp=f"{today}T10:00:00.000Z"),
        ev("issue.escalated", issue_run_id="i1", project="alpha", timestamp=f"{today}T10:00:10.000Z",
           data={"reason": "needs_clarification"}),
        ev("verification.started", issue_run_id="i2", timestamp=f"{today}T10:00:00.000Z"),
        ev("verification.passed", issue_run_id="i2", timestamp=f"{today}T10:00:03.000Z"),
    ]
    (tmp_path / f"{today}.jsonl").write_text("\n".join(lines) + "\n")
    (tmp_path / "2000-01-01.jsonl").write_text(ev("run.started", run_id="old", timestamp="2000-01-01T00:00:00.000Z") + "\n")
    return tmp_path


def metrics(*a):
    return run_cli("metrics.py", *a)


def test_full_report_layout(d):
    r = metrics("--events-dir", str(d))
    assert r.returncode == 0
    out = r.stdout
    assert out.startswith("Loop Metrics (all time)\n\nRuns\n  Total          2\n")
    assert "  Avg duration   2.0s" in out
    assert "  Verification pass rate  100.0%" in out
    assert "  requirement     100.0%" in out
    assert out.rstrip().endswith("Avg verification duration   3.0s")


def test_days_window_and_project(d):
    r = metrics("--days", "1", "--events-dir", str(d))
    assert re.search(r"Total\s+1\b", r.stdout) and re.match(r"Loop Metrics \(\d{4}-\d\d-\d\d to \d{4}-\d\d-\d\d\)", r.stdout)
    r = metrics("--project", "alpha", "--events-dir", str(d))
    assert "— project: alpha" in r.stdout and "N/A (a run is not scoped to a single project)" in r.stdout
    assert re.search(r"Processed\s+1\b", r.stdout)


def test_errors(tmp_path):
    r = metrics("--days", "x", "--events-dir", str(tmp_path))
    assert r.returncode == 1 and "days" in r.stderr
    r = metrics("--events-dir", str(tmp_path / "nope"))
    assert r.returncode == 1 and "does not exist" in r.stderr and r.stdout == ""

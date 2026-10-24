"""Black-box contract for `risk.py` / `loopx risk`. Run against Rust with LOOPX_BIN=target/debug/loopx."""
import json

from cli_runner import run_cli


def test_score_prints_python_style_json():
    r = run_cli("risk.py", "score", "--title", "Add authentication", "--description", "for the login flow")
    assert r.returncode == 0
    assert r.stdout == '{"score": 30, "level": "MEDIUM", "matched_keywords": ["authentication"]}\n'


def test_score_sums_clamps_and_orders_keywords():
    r = run_cli("risk.py", "score", "--title", "Payment", "--description", "authentication database migration")
    assert json.loads(r.stdout) == {
        "score": 100, "level": "CRITICAL",
        "matched_keywords": ["database migration", "authentication", "payment"],
    }
    r = run_cli("risk.py", "score", "--title", "documentation", "--description", "test-only change")
    assert json.loads(r.stdout) == {"score": 0, "level": "LOW", "matched_keywords": ["test-only change", "documentation"]}


def test_missing_flags_and_usage_exit_1():
    r = run_cli("risk.py", "score", "--description", "x")
    assert r.returncode == 1 and "--title is required" in r.stderr
    r = run_cli("risk.py", "score", "--title", "x")
    assert r.returncode == 1 and "--description is required" in r.stderr
    for args in ([], ["bogus"]):
        r = run_cli("risk.py", *args)
        assert r.returncode == 1 and "Usage" in r.stderr

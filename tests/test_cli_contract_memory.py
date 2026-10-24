"""Black-box contract for `project_memory.py` / `loopx memory`.

Learnings live in the gitlab-config skill's project cache entry
($HOME/.gitlab/cache/<instance>/projects/<project>/project.json, `_memory.fix_learnings`).
Run against Rust with LOOPX_BIN=target/debug/loopx.
"""
import json
from pathlib import Path

import pytest

from cli_runner import loopx_bin, run_cli

REAL_SKILLS = Path.home() / ".encore-skills"


@pytest.fixture
def home(tmp_path):
    if not loopx_bin() and not REAL_SKILLS.exists():
        pytest.skip("gitlab-config skill (~/.encore-skills) not installed")
    h = tmp_path / "home"
    h.mkdir()
    if REAL_SKILLS.exists():
        (h / ".encore-skills").symlink_to(REAL_SKILLS)
    return h


def project_file(home, instance="inst", project="grp/proj"):
    return home / ".gitlab" / "cache" / instance / "projects" / project.replace("/", "__") / "project.json"


def run(home, *args):
    return run_cli("project_memory.py", *args, env={"HOME": str(home)})


def test_get_with_nothing_recorded_is_empty_list(home):
    r = run(home, "get", "inst", "grp/proj")
    assert r.returncode == 0
    assert r.stdout == "[]\n"


def test_add_then_get_round_trips(home):
    r = run(home, "add", "inst", "grp/proj", "Run migrations before specs", "42", "rails,db")
    assert r.returncode == 0
    assert r.stdout == "OK\n"
    got = json.loads(run(home, "get", "inst", "grp/proj").stdout)
    assert got == [{"lesson": "Run migrations before specs", "issue_iid": 42, "tags": ["rails", "db"]}]


def test_add_appends_in_order(home):
    run(home, "add", "inst", "grp/proj", "first")
    run(home, "add", "inst", "grp/proj", "second")
    got = json.loads(run(home, "get", "inst", "grp/proj").stdout)
    assert [e["lesson"] for e in got] == ["first", "second"]


def test_optional_fields_are_omitted_when_empty(home):
    run(home, "add", "inst", "grp/proj", "bare", "", "")
    assert json.loads(run(home, "get", "inst", "grp/proj").stdout) == [{"lesson": "bare"}]


def test_tags_are_split_on_comma_without_trimming(home):
    run(home, "add", "inst", "grp/proj", "x", "", "a, b")
    assert json.loads(run(home, "get", "inst", "grp/proj").stdout)[0]["tags"] == ["a", " b"]


def test_add_preserves_other_project_cache_data(home):
    p = project_file(home)
    p.parent.mkdir(parents=True)
    p.write_text(json.dumps({"name": "proj", "members": [{"id": 1}], "_memory": {"other": 1}}))
    run(home, "add", "inst", "grp/proj", "lesson")
    saved = json.loads(p.read_text())
    assert saved["name"] == "proj" and saved["members"] == [{"id": 1}]
    assert saved["_memory"]["other"] == 1
    assert saved["_memory"]["fix_learnings"] == [{"lesson": "lesson"}]


def test_project_cache_is_written_sorted_indent_2_ascii(home):
    run(home, "add", "inst", "grp/proj", "café", "7", "t")
    text = project_file(home).read_text()
    assert text == json.dumps(json.loads(text), indent=2, sort_keys=True)
    assert "caf\\u00e9" in text


def test_non_integer_issue_iid_fails(home):
    r = run(home, "add", "inst", "grp/proj", "x", "not-a-number")
    assert r.returncode != 0
    assert not project_file(home).exists()


def test_no_args_prints_usage_and_exits_1(home):
    r = run(home)
    assert r.returncode == 1
    assert "Usage" in r.stderr


def test_unknown_command_exits_1(home):
    r = run(home, "bogus")
    assert r.returncode == 1
    assert "Unknown command: bogus" in r.stderr


def test_missing_args_fail_nonzero(home):
    assert run(home, "get", "inst").returncode != 0
    assert run(home, "add", "inst", "grp/proj").returncode != 0

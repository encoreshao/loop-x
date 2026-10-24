"""Black-box contract for `list_assigned_issues.py` / `loopx issues list`.

The Python script shells out to the external gitlab-config skill
(~/.encore-skills), so those tests link it into the scratch HOME and skip when
it is not installed. Run against Rust with LOOPX_BIN=target/debug/loopx.
"""
import json
from pathlib import Path

import pytest

from cli_runner import loopx_bin, run_cli
from fake_gitlab import FakeGitLab

REAL_SKILLS = Path.home() / ".encore-skills"


def issue(iid, *assignees):
    return {"iid": iid, "title": f"Issue é{iid}", "assignees": [{"username": a} for a in assignees]}


@pytest.fixture
def gitlab():
    routes = {
        "/api/v4/projects/grp/alpha/issues": [issue(1, "dev"), issue(2, "other"), issue(3, "dev", "x")],
        "/api/v4/projects/grp/beta/issues": [issue(9, "other")],
        "/api/v4/projects/grp/big/issues": [issue(i, "dev") for i in range(1, 151)],
    }
    with FakeGitLab(routes) as g:
        yield g


@pytest.fixture
def env(tmp_path, gitlab):
    if not loopx_bin() and not REAL_SKILLS.exists():
        pytest.skip("gitlab-config skill (~/.encore-skills) not installed")
    home = tmp_path / "home"
    (home / ".gitlab").mkdir(parents=True)
    (home / ".gitlab" / "config.json").write_text(json.dumps({
        "default": "fake",
        "instances": {"fake": {"url": gitlab.url, "token": "tok-123"}},
        "projects": {a: {"project_id": f"grp/{a}"} for a in ("alpha", "beta", "big")},
    }))
    if REAL_SKILLS.exists():
        (home / ".encore-skills").symlink_to(REAL_SKILLS)
    le = tmp_path / "le"
    le.mkdir()
    (le / "projects.json").write_text(json.dumps({
        "gitlab_instance": "fake", "assignee_username": "dev", "worktree_root": "/w",
        "projects": {"alpha": {"local_path": "/a"}, "beta": {"local_path": "/b"}},
    }))
    return {"HOME": str(home), "LOOP_ENGINEERING_HOME": str(le)}


def test_explicit_user_and_aliases_filters_by_assignee(env):
    r = run_cli("list_assigned_issues.py", "dev", "alpha", "beta", env=env)
    assert r.returncode == 0, r.stderr
    out = json.loads(r.stdout)
    assert list(out) == ["alpha", "beta"]
    assert [i["iid"] for i in out["alpha"]] == [1, 3]
    assert out["beta"] == []


def test_defaults_come_from_projects_json(env):
    r = run_cli("list_assigned_issues.py", env=env)
    assert r.returncode == 0, r.stderr
    assert list(json.loads(r.stdout)) == ["alpha", "beta"]


def test_output_is_python_json_indent_2_ascii(env):
    r = run_cli("list_assigned_issues.py", "dev", "alpha", env=env)
    assert r.stdout.startswith('{\n  "alpha": [\n    {\n      "iid": 1,')
    assert "\\u00e9" in r.stdout


def test_paginates_past_first_page(env):
    r = run_cli("list_assigned_issues.py", "dev", "big", env=env)
    assert len(json.loads(r.stdout)["big"]) == 150


def test_requests_open_issues_with_token(env, gitlab):
    run_cli("list_assigned_issues.py", "dev", "alpha", env=env)
    first = gitlab.requests[0]
    assert first["path"] == "/api/v4/projects/grp/alpha/issues"
    assert first["query"]["state"] == "opened"
    assert first["token"] == "tok-123"


def test_one_argument_is_a_usage_error(env):
    r = run_cli("list_assigned_issues.py", "dev", env=env)
    assert r.returncode == 1
    assert "Usage" in r.stderr


def test_api_failure_exits_nonzero(env):
    r = run_cli("list_assigned_issues.py", "dev", "no-such-project", env=env)
    assert r.returncode != 0

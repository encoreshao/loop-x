"""Black-box contract for the gitlab-config skill's `gitlab_api.py` / `loopx gitlab`,
against a fake GitLab and a scratch HOME. The Python side needs the skill
(~/.encore-skills) and is skipped without it. Run against Rust with
LOOPX_BIN=target/debug/loopx.
"""
import json

import pytest

from cli_runner import run_cli
from fake_gitlab import FakeGitLab


def issue(iid, state="opened", labels=(), updated="2026-01-01T00:00:00Z", assignee="dev"):
    return {"iid": iid, "state": state, "labels": list(labels), "updated_at": updated,
            "title": f"Issue é{iid}", "assignees": [{"username": assignee}]}


@pytest.fixture
def gitlab():
    routes = {
        "/api/v4/user": {"id": 1, "username": "dev"},
        "/api/v4/projects/grp/alpha/issues": [
            issue(1, labels=["bug"], updated="2026-01-03T00:00:00Z"),
            issue(2, "closed", ["bug", "ui"], "2026-01-02T00:00:00Z", "other"),
            issue(3, labels=["ui"], updated="2026-01-05T00:00:00Z"),
        ],
        "/api/v4/projects/grp/alpha/issues/7": {"iid": 7, "title": "Seven", "author": {"id": 5, "username": "a", "name": "A", "extra": 1}},
        "/api/v4/projects/grp/alpha/issues/7/notes": [
            {"id": 11, "created_at": "2026-01-02T00:00:00Z", "author": {"id": 6, "username": "b"}, "body": "second"},
            {"id": 10, "created_at": "2026-01-01T00:00:00Z", "author": {"id": 5, "username": "a"}, "body": "first"},
        ],
        "/api/v4/projects/grp/alpha/merge_requests": [{"iid": 4, "source_branch": "loop/issue-7"}],
        "/api/v4/projects/grp/alpha/merge_requests/4": {"iid": 4},
        "/api/v4/projects/grp/alpha/merge_requests/4/changes": {"changes": [
            {"old_path": "a.rb", "new_path": "a.rb", "diff": "@@ -1 +1 @@\n-x\n+y"}]},
        "/api/v4/projects/grp/alpha/merge_requests/4/notes": [],
        "/api/v4/projects/grp/alpha": {"id": 3, "namespace": {"kind": "user", "full_path": "grp"}},
        "/api/v4/projects/grp/alpha/members/all": [{"id": 5, "username": "a", "access_level": 30}],
    }
    with FakeGitLab(routes) as g:
        yield g


@pytest.fixture
def env(tmp_path, gitlab):
    home = tmp_path / "home"
    (home / ".gitlab").mkdir(parents=True)
    (home / ".gitlab" / "config.json").write_text(json.dumps({
        "default": "fake",
        "instances": {
            "fake": {"url": gitlab.url, "token": "tok-default", "description": "Fake"},
            "other": {"url": gitlab.url, "token": "tok-other"},
        },
        "bundles": {"vip": {"instance": "fake", "token": "tok-vip"}},
        "projects": {
            "alpha": {"project_id": "grp/alpha", "bundle": "vip", "description": "Alpha"},
            "beta": {"project_id": "grp/beta", "instance": "other"},
        },
    }))
    cwd = tmp_path / "cwd"
    cwd.mkdir()
    return {"HOME": str(home), "_cwd": str(cwd)}


def gl(env, *args):
    e = {k: v for k, v in env.items() if not k.startswith("_")}
    return run_cli("gitlab_api.py", *args, env=e, cwd=env["_cwd"])


def cache_file(env, *rel):
    from pathlib import Path
    return Path(env["HOME"], ".gitlab", "cache", *rel)


def test_whoami_uses_alias_free_default_instance(env, gitlab):
    r = gl(env, "whoami")
    assert r.returncode == 0, r.stderr
    assert json.loads(r.stdout) == {"id": 1, "username": "dev"}
    assert gitlab.requests[0]["token"] == "tok-default"


def test_alias_resolves_project_id_and_bundle_token(env, gitlab):
    r = gl(env, "list-issues", "alpha")
    assert r.returncode == 0, r.stderr
    assert [i["iid"] for i in json.loads(r.stdout)] == [1, 2, 3]
    req = gitlab.requests[0]
    assert req["path"] == "/api/v4/projects/grp/alpha/issues"
    assert req["token"] == "tok-vip"
    assert req["query"]["state"] == "opened" and req["query"]["per_page"] == "100"


def test_explicit_instance_ignores_alias_bundle(env, gitlab):
    gl(env, "--instance=other", "list-issues", "alpha")
    assert gitlab.requests[0]["token"] == "tok-other"


def test_list_issues_state_labels_and_filters(env, gitlab):
    gl(env, "list-issues", "alpha", "closed", "bug", "ui", "--assignee=dev", "--author=me")
    q = gitlab.requests[0]["query"]
    assert q["state"] == "closed" and q["labels"] == "bug,ui"
    assert q["assignee_username"] == "dev" and q["author_username"] == "me"


def test_project_info(env):
    r = gl(env, "project-info", "alpha")
    assert json.loads(r.stdout) == {"project_id": "grp/alpha", "instance": None, "bundle": "vip"}
    assert r.stdout.startswith('{\n  "project_id"')


def test_project_info_unknown_alias_fails(env):
    r = gl(env, "project-info", "nope")
    assert r.returncode == 1 and "Unknown project alias 'nope'" in r.stderr


def test_get_issue_includes_notes(env):
    r = gl(env, "get-issue", "alpha", "7")
    got = json.loads(r.stdout)
    assert got["title"] == "Seven"
    assert [n["id"] for n in got["notes"]] == [11, 10]


def test_post_issue_comment_sends_json_body(env, gitlab):
    r = gl(env, "post-issue-comment", "alpha", "7", "hello é")
    assert r.returncode == 0, r.stderr
    post = next(q for q in gitlab.requests if q.get("method") == "POST")
    assert post["path"] == "/api/v4/projects/grp/alpha/issues/7/notes"
    assert post["body"] == {"body": "hello é"}
    assert json.loads(r.stdout)["body"] == "hello é"


def test_list_mrs_and_get_diff(env):
    assert json.loads(gl(env, "list-mrs", "alpha").stdout)[0]["source_branch"] == "loop/issue-7"
    r = gl(env, "get-diff", "alpha", "4")
    assert r.stdout == "File: a.rb -> a.rb\n@@ -1 +1 @@\n-x\n+y\n\n\n"


def test_aggregate_issues(env):
    got = json.loads(gl(env, "aggregate-issues", "alpha").stdout)
    assert got["total"] == 3 and got["opened"] == 2 and got["closed"] == 1
    assert got["by_label"] == {"bug": 2, "ui": 2}
    assert [i["iid"] for i in got["recent"]] == [3, 1, 2]


def test_sync_issue_writes_sorted_cache_and_users(env):
    r = gl(env, "sync-issue", "alpha", "7")
    assert r.returncode == 0, r.stderr
    path = cache_file(env, "fake", "projects", "grp__alpha", "issues", "7.json")
    text = path.read_text()
    saved = json.loads(text)
    assert text == json.dumps(saved, indent=2, sort_keys=True)
    assert [n["id"] for n in saved["notes"]] == [10, 11]  # sorted by created_at
    users = json.loads(cache_file(env, "fake", "users.json").read_text())
    assert users["5"] == {"id": 5, "username": "a", "name": "A"}  # only the known fields
    assert set(users) == {"5", "6"}


def test_sync_issue_keeps_annotations_and_old_notes(env):
    path = cache_file(env, "fake", "projects", "grp__alpha", "issues", "7.json")
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps({"_notes": {"loop_last_seen_note_id": "9"},
                                "notes": [{"id": 9, "created_at": "2025-12-31T00:00:00Z", "author": {"id": 7}}]}))
    gl(env, "sync-issue", "alpha", "7")
    saved = json.loads(path.read_text())
    assert saved["_notes"] == {"loop_last_seen_note_id": "9"}
    assert [n["id"] for n in saved["notes"]] == [9, 10, 11]


def test_cached_issue_reads_cache_and_fails_when_absent(env):
    r = gl(env, "cached-issue", "alpha", "7")
    assert r.returncode == 1 and "run sync-issue first" in r.stderr
    gl(env, "sync-issue", "alpha", "7")
    assert json.loads(gl(env, "cached-issue", "alpha", "7").stdout)["title"] == "Seven"


def test_sync_project_merges_members(env):
    r = gl(env, "sync-project", "alpha")
    assert r.returncode == 0, r.stderr
    saved = json.loads(cache_file(env, "fake", "projects", "grp__alpha", "project.json").read_text())
    assert saved["members"][0]["username"] == "a"


def test_http_error_exits_1_with_response(env):
    r = gl(env, "get-issue", "alpha", "404")
    assert r.returncode == 1
    assert "HTTP Error" in r.stderr and "404" in r.stderr


def test_unknown_command_and_missing_args(env):
    r = gl(env, "bogus", "alpha")
    assert r.returncode == 1 and "Unknown command: bogus" in r.stderr
    r = gl(env, "get-issue", "alpha")
    assert r.returncode == 1 and "Invalid arguments" in r.stderr


def test_list_projects_and_instances_mention_configuration(env):
    out = gl(env, "list-projects").stdout
    assert "- alpha" in out and "Project ID: grp/alpha" in out and "- beta" in out
    out = gl(env, "list-instances").stdout
    assert "Default instance: fake" in out and "- fake (default)" in out

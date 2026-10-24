"""Black-box contract for `memory_store.py` / `loopx memory-store`, under a scratch
LOOP_ENGINEERING_HOME. Run against Rust with LOOPX_BIN=target/debug/loopx."""
import json
import re

import pytest

from cli_runner import run_cli


@pytest.fixture
def env(tmp_path):
    return {"LOOP_ENGINEERING_HOME": str(tmp_path)}


def add(env, *args):
    return run_cli("memory_store.py", "add", *args, env=env)


def test_add_prints_created_then_updated(env, tmp_path):
    r = add(env, "harbor", "142", "RSpec db-cleaner leaves stale rows", "flaky-test", "testing")
    assert r.returncode == 0
    payload = json.loads(r.stdout)
    assert list(payload) == ["action", "lesson_id", "path"]
    assert payload["action"] == "created" and re.fullmatch(r"lesson_[0-9a-f]{32}", payload["lesson_id"])
    assert payload["path"] == str(tmp_path / "memory" / "harbor" / "142-rspec-db-cleaner-leaves-stale-rows.md")
    r2 = add(env, "harbor", "142", "second lesson")
    assert json.loads(r2.stdout)["action"] == "updated"
    assert json.loads(r2.stdout)["lesson_id"] == payload["lesson_id"]


def test_files_on_disk(env, tmp_path):
    lid = json.loads(add(env, "web", "7", "Use bundle exec rubocop .", "b,a", "lint").stdout)["lesson_id"]
    text = (tmp_path / "memory" / "web" / "7-use-bundle-exec-rubocop.md").read_text()
    assert text.startswith(
        "---\nname: 7-use-bundle-exec-rubocop\ndescription: Use bundle exec rubocop .\nmetadata:\n"
        f"  type: project\n  issue_iid: 7\n  lesson_id: {lid}\n  category: lint\n  tags: [b, a]\n  created_at: "
    )
    assert re.search(r"\n  modified: \d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ\n  score: 0\n  flag: \n---\n\nRecorded \d{4}-\d\d-\d\d: Use bundle exec rubocop \.\n$", text)
    index = (tmp_path / "memory" / "web" / "MEMORY.md").read_text()
    assert index == "- [#7 use bundle exec rubocop](7-use-bundle-exec-rubocop.md) \u2014 Use bundle exec rubocop .\n"
    add(env, "web", "7", "more", "c")
    text = (tmp_path / "memory" / "web" / "7-use-bundle-exec-rubocop.md").read_text()
    assert "tags: [a, b, c]" in text and "category: lint" in text
    assert (tmp_path / "memory" / "web" / "MEMORY.md").read_text().count("\n") == 1


def test_get_and_list(env, tmp_path):
    assert run_cli("memory_store.py", "get", "harbor", "1", env=env).stdout == "null\n"
    assert run_cli("memory_store.py", "list", "harbor", env=env).stdout == "[]\n"
    add(env, "harbor", "1", "caf\u00e9 lesson", "x", "cat")
    got = json.loads(run_cli("memory_store.py", "get", "harbor", "1", env=env).stdout)
    assert list(got) == ["name", "description", "issue_iid", "tags", "lesson_id", "category",
                         "created_at", "modified", "score", "flag", "body", "path"]
    assert got["issue_iid"] == 1 and got["tags"] == ["x"] and got["category"] == "cat" and got["score"] == 0
    raw = run_cli("memory_store.py", "get", "harbor", "1", env=env).stdout
    assert raw.startswith('{\n  "name": "1-caf-lesson",') and "caf\\u00e9 lesson" in raw
    listed = json.loads(run_cli("memory_store.py", "list", "harbor", env=env).stdout)
    assert [e["issue_iid"] for e in listed] == [1]


def test_usage_errors(env):
    assert run_cli("memory_store.py", env=env).returncode == 1
    r = run_cli("memory_store.py", "bogus", env=env)
    assert r.returncode == 1 and "Unknown command: bogus" in r.stderr
    assert run_cli("memory_store.py", "add", "a", env=env).returncode == 1
    assert run_cli("memory_store.py", "add", "a", "notint", "x", env=env).returncode == 1

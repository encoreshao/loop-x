"""Black-box contract for `topic_config.py` / `loopx topic config` and
`topic_seen.py` / `loopx topic seen`. Run against Rust with
LOOPX_BIN=target/debug/loopx.
"""
import json
from datetime import datetime, timedelta, timezone

import pytest

from cli_runner import run_cli

TOPICS = [
    {"name": "ai", "label": "AI", "brief": "Model news é", "slack_bundle": None, "enabled": True},
    {"name": "rust", "label": "Rust", "brief": "Rust news", "slack_bundle": "b", "enabled": False},
]


@pytest.fixture
def le(tmp_path):
    (tmp_path / "topics.json").write_text(json.dumps(TOPICS))
    return {"LOOP_ENGINEERING_HOME": str(tmp_path)}


def config(env, *args):
    return run_cli("topic_config.py", *args, env=env)


def test_names_lists_topics_in_order(le):
    r = config(le, "names")
    assert r.returncode == 0
    assert r.stdout == "ai\nrust\n"


def test_topic_prints_the_entry_indent_2_ascii(le):
    r = config(le, "topic", "ai")
    assert r.returncode == 0
    assert json.loads(r.stdout) == TOPICS[0]
    assert r.stdout.startswith('{\n  "name": "ai",')
    assert "\\u00e9" in r.stdout


def test_unknown_topic_exits_1(le):
    r = config(le, "topic", "nope")
    assert r.returncode == 1
    assert "No topic named" in r.stderr


def test_topic_without_name_exits_1(le):
    r = config(le, "topic")
    assert r.returncode == 1 and "Usage" in r.stderr


def test_config_no_args_and_unknown_command(le):
    assert config(le).returncode == 1
    r = config(le, "bogus")
    assert r.returncode == 1 and "Unknown command: bogus" in r.stderr


def test_missing_topics_file_fails(tmp_path):
    r = config({"LOOP_ENGINEERING_HOME": str(tmp_path)}, "names")
    assert r.returncode != 0
    assert "No topic config at" in r.stderr


def test_topics_file_must_be_an_array(tmp_path):
    (tmp_path / "topics.json").write_text('{"a": 1}')
    r = config({"LOOP_ENGINEERING_HOME": str(tmp_path)}, "names")
    assert r.returncode != 0
    assert "must contain a JSON array" in r.stderr


# --- topic seen -------------------------------------------------------------

@pytest.fixture
def state(tmp_path):
    d = tmp_path / "state"
    return {"LOOP_TOPIC_STATE_DIR": str(d), "LOOP_ENGINEERING_HOME": str(tmp_path / "le")}, d


def seen(env, *args):
    return run_cli("topic_seen.py", *args, env=env)


def iso(delta_days):
    return (datetime.now(timezone.utc) - timedelta(days=delta_days)).isoformat()


def test_get_with_no_state_is_empty_list(state):
    env, _ = state
    r = seen(env, "get", "ai")
    assert r.returncode == 0
    assert r.stdout == "[]\n"


def test_add_then_get(state):
    env, d = state
    assert seen(env, "add", "ai", "https://x/1", "Title é").stdout == "OK\n"
    got = json.loads(seen(env, "get", "ai").stdout)
    assert got == [{"url": "https://x/1", "title": "Title é"}]
    saved = json.loads((d / "ai.json").read_text())
    assert set(saved[0]) == {"url", "title", "seen_at"}
    assert datetime.fromisoformat(saved[0]["seen_at"]).utcoffset() == timedelta(0)


def test_get_prunes_entries_older_than_seven_days(state):
    env, d = state
    d.mkdir(parents=True)
    (d / "ai.json").write_text(json.dumps([
        {"url": "old", "title": "o", "seen_at": iso(8)},
        {"url": "new", "title": "n", "seen_at": iso(1)},
        {"url": "bad", "title": "b", "seen_at": "garbage"},
        {"url": "none", "title": "n"},
    ]))
    assert [e["url"] for e in json.loads(seen(env, "get", "ai").stdout)] == ["new"]
    assert [e["url"] for e in json.loads((d / "ai.json").read_text())] == ["new"]


def test_add_prunes_and_appends(state):
    env, d = state
    d.mkdir(parents=True)
    (d / "ai.json").write_text(json.dumps([{"url": "old", "title": "o", "seen_at": iso(9)}]))
    seen(env, "add", "ai", "u", "t")
    assert [e["url"] for e in json.loads((d / "ai.json").read_text())] == ["u"]


def test_state_file_is_indent_2(state):
    env, d = state
    seen(env, "add", "ai", "u", "café")
    text = (d / "ai.json").read_text()
    assert text == json.dumps(json.loads(text), indent=2)
    assert "caf\\u00e9" in text


def test_corrupt_state_reads_as_empty(state):
    env, d = state
    d.mkdir(parents=True)
    (d / "ai.json").write_text("{not json")
    assert seen(env, "get", "ai").stdout == "[]\n"


def test_topic_name_cannot_escape_state_dir(state):
    env, d = state
    seen(env, "add", "../escape", "u", "t")
    assert (d / "escape.json").exists()
    assert not (d.parent / "escape.json").exists()


def test_seen_usage_errors(state):
    env, _ = state
    assert seen(env).returncode == 1
    assert seen(env, "get").returncode == 1
    r = seen(env, "add", "ai", "u")
    assert r.returncode == 1 and "Usage" in r.stderr
    r = seen(env, "bogus", "ai")
    assert r.returncode == 1 and "Unknown command: bogus" in r.stderr

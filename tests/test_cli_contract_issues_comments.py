"""Black-box contract for `track_new_comments.py` / `loopx issues comments`.

State lives in the gitlab-config skill's cache layout under $HOME/.gitlab/cache.
Python tests link the external skill into the scratch HOME (skipped if absent).
Run against Rust with LOOPX_BIN=target/debug/loopx.
"""
import json
from pathlib import Path

import pytest

from cli_runner import loopx_bin, run_cli

REAL_SKILLS = Path.home() / ".encore-skills"


def note(nid, author, body="hi"):
    return {"id": nid, "author": {"username": author}, "body": body}


@pytest.fixture
def home(tmp_path):
    if not loopx_bin() and not REAL_SKILLS.exists():
        pytest.skip("gitlab-config skill (~/.encore-skills) not installed")
    h = tmp_path / "home"
    h.mkdir()
    if REAL_SKILLS.exists():
        (h / ".encore-skills").symlink_to(REAL_SKILLS)
    return h


def cache_file(home, instance="inst", project="grp/proj", iid=7):
    return home / ".gitlab" / "cache" / instance / "projects" / project.replace("/", "__") / "issues" / f"{iid}.json"


def write_cache(home, data, **kw):
    p = cache_file(home, **kw)
    p.parent.mkdir(parents=True)
    p.write_text(json.dumps(data))
    return p


def run(home, *args):
    return run_cli("track_new_comments.py", *args, env={"HOME": str(home)})


def test_no_cache_means_no_new_notes(home):
    r = run(home, "inst", "grp/proj", "7")
    assert r.returncode == 0
    assert json.loads(r.stdout) == []


def test_without_watermark_every_note_is_new(home):
    write_cache(home, {"notes": [note(1, "a"), note(2, "b")]})
    r = run(home, "inst", "grp/proj", "7")
    assert [n["id"] for n in json.loads(r.stdout)] == [1, 2]


def test_only_notes_past_the_watermark_are_new(home):
    write_cache(home, {"notes": [note(1, "a"), note(2, "b"), note(3, "c")],
                       "_notes": {"loop_last_seen_note_id": "2"}})
    r = run(home, "inst", "grp/proj", "7")
    assert [n["id"] for n in json.loads(r.stdout)] == [3]


def test_exclude_author_drops_their_notes(home):
    write_cache(home, {"notes": [note(1, "bot"), note(2, "human")]})
    r = run(home, "inst", "grp/proj", "7", "bot")
    assert [n["id"] for n in json.loads(r.stdout)] == [2]


def test_output_is_indent_2_ascii(home):
    write_cache(home, {"notes": [note(1, "a", "café")]})
    r = run(home, "inst", "grp/proj", "7")
    assert r.stdout.startswith('[\n  {\n    "id": 1,')
    assert "caf\\u00e9" in r.stdout


def test_mark_seen_advances_watermark_past_all_notes_including_excluded(home):
    p = write_cache(home, {"title": "t", "notes": [note(4, "bot"), note(9, "human")]})
    r = run(home, "mark-seen", "inst", "grp/proj", "7")
    assert r.returncode == 0
    assert r.stdout == "OK\n"
    saved = json.loads(p.read_text())
    assert saved["_notes"]["loop_last_seen_note_id"] == "9"
    assert saved["title"] == "t"
    assert run(home, "inst", "grp/proj", "7").stdout.strip() == "[]"


def test_mark_seen_is_noop_but_ok_when_nothing_new(home):
    p = write_cache(home, {"notes": [note(1, "a")], "_notes": {"loop_last_seen_note_id": "1"}})
    before = p.read_text()
    r = run(home, "mark-seen", "inst", "grp/proj", "7")
    assert r.stdout == "OK\n"
    assert p.read_text() == before


def test_cache_is_rewritten_with_sorted_keys_and_indent_2(home):
    p = write_cache(home, {"z": 1, "notes": [note(1, "a")], "a": {"y": 1, "b": 2}})
    run(home, "mark-seen", "inst", "grp/proj", "7")
    text = p.read_text()
    assert text == json.dumps(json.loads(text), indent=2, sort_keys=True)
    assert text.index('"_notes"') < text.index('"a"') < text.index('"notes"') < text.index('"z"')


def test_too_few_args_is_usage_error(home):
    r = run(home, "inst", "grp/proj")
    assert r.returncode == 1
    assert "Usage" in r.stderr


def test_mark_seen_too_few_args_is_usage_error(home):
    r = run(home, "mark-seen", "inst", "grp/proj")
    assert r.returncode == 1
    assert "mark-seen" in r.stderr

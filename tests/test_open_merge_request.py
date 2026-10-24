"""Contract for `open_merge_request.sh` / `loopx mr open`.
Run against Rust with LOOPX_BIN=target/debug/loopx.
"""
from cli_runner import run_cli


def test_dry_run_prints_expected_git_push_command():
    result = run_cli(
        "open_merge_request.sh", "/tmp/some-repo", "loop/issue-42", "staging", "Fix #42: sample title",
        env={"DRY_RUN": "1"}, check=True,
    )

    assert result.stdout.splitlines() == [
        "git", "-C", "/tmp/some-repo", "push", "origin", "loop/issue-42",
        "-o", "merge_request.create",
        "-o", "merge_request.target=staging",
        "-o", "merge_request.title=Fix #42: sample title",
        "-o", "merge_request.remove_source_branch",
    ]


def test_missing_arguments_exits_nonzero():
    result = run_cli("open_merge_request.sh", "/tmp/some-repo")

    assert result.returncode != 0


def test_refuses_to_push_non_loop_branch():
    result = run_cli("open_merge_request.sh", "/tmp/some-repo", "staging", "staging", "not a loop branch")

    assert result.returncode != 0
    assert "refusing" in result.stderr.lower()


def test_rejects_loop_prefix_lookalikes():
    for branch in ("loop/issues-1", "xloop/issue-1", "loop-issue-1"):
        result = run_cli("open_merge_request.sh", "/tmp/some-repo", branch, "staging", "t")
        assert result.returncode != 0, branch


def test_title_with_spaces_and_unicode_stays_one_argument():
    result = run_cli(
        "open_merge_request.sh", "/r", "loop/issue-5", "main", "My \u00e9 title",
        env={"DRY_RUN": "1"}, check=True,
    )
    assert "merge_request.title=My \u00e9 title" in result.stdout.splitlines()


def test_usage_on_wrong_argument_count_mentions_usage():
    result = run_cli("open_merge_request.sh", "a", "b", "c", "d", "e")
    assert result.returncode == 1
    assert "Usage" in result.stderr

"""Run a loop CLI as a black-box subprocess: the `loopx` subcommand that
replaced each legacy `bin/*.py` script (the legacy name is kept as the key, so
the contract tests read the same as when they ran both sides).

`LOOPX_BIN` is the path of the built binary; without it, tests that need it skip.
"""
import os
import subprocess

import pytest

# legacy script -> loopx subcommand words. Grow this as scripts migrate.
SUBCOMMANDS = {
    "events.py": ["events"],
    "loops_config.py": ["loops"],
    "ai_cli_config.py": ["ai-cli"],
    "loop_cli.py": ["loop"],  # `loopx loop <sub>`; `ledger` is also top-level `loopx ledger`
    "metrics.py": ["metrics"],
    "loop_config.py": ["config"],
    "list_assigned_issues.py": ["issues", "list"],
    "track_new_comments.py": ["issues", "comments"],
    "project_memory.py": ["memory"],
    "topic_config.py": ["topic", "config"],
    "topic_seen.py": ["topic", "seen"],
    "slack_notify.py": ["notify"],
    "gitlab_api.py": ["gitlab"],
    "new_worktree.sh": ["worktree", "new"],
    "open_merge_request.sh": ["mr", "open"],
    "risk.py": ["risk"],
    "cost.py": ["cost"],
    "learning.py": ["learning"],
    "memory_store.py": ["memory-store"],
    "notify.py": ["notify-loop"],
    "inbox_triage_runner.py": ["inbox", "run"],
}


def loopx_bin():
    """The binary under test, or None when `LOOPX_BIN` is unset."""
    return os.environ.get("LOOPX_BIN") or None


def command_for(script, *args):
    binary = loopx_bin()
    if not binary:
        pytest.skip("needs LOOPX_BIN")
    return [binary, *SUBCOMMANDS[script], *args]


def run_cli(script, *args, env=None, input=None, cwd=None, check=False):
    """Run `script` (legacy filename) with args; returns CompletedProcess (text mode).

    `check=True` raises CalledProcessError on a non-zero exit, like subprocess.run."""
    full_env = {**os.environ, **(env or {})}
    return subprocess.run(
        command_for(script, *args),
        env=full_env, input=input, cwd=cwd, check=check,
        capture_output=True, text=True,
    )

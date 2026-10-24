"""`loopx scheduler` allows one poller at a time (the app's scheduler and a launchd
one must never both start the same due loop). Rust-only (skipped without LOOPX_BIN)."""
import fcntl
import os
import subprocess

import pytest

from cli_runner import loopx_bin

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")


def poll(home):
    env = {**os.environ, "LOOP_ENGINEERING_HOME": str(home), "LOOPX_ROOT": str(home)}
    return subprocess.run([loopx_bin(), "scheduler"], env=env, capture_output=True, text=True, timeout=30)


def test_a_poll_is_skipped_while_another_scheduler_holds_the_lock(tmp_path):
    with open(tmp_path / "scheduler.lock", "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        out = poll(tmp_path)
    assert out.returncode == 0
    assert "another scheduler is already polling" in out.stdout


def test_the_lock_is_free_once_the_holder_exits(tmp_path):
    out = poll(tmp_path)
    assert out.returncode == 0
    assert "already polling" not in out.stdout
    assert "already polling" not in poll(tmp_path).stdout

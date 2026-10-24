"""The per-machine directory moved from ~/.loop-engineering to ~/.loopx: the first
loopx run migrates an existing one and leaves a symlink. Rust-only (skipped without
LOOPX_BIN)."""
import json
import os
import subprocess

import pytest

from cli_runner import loopx_bin

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")


def aliases(home):
    env = {k: v for k, v in os.environ.items() if k != "LOOP_ENGINEERING_HOME"}
    env["HOME"] = str(home)
    return subprocess.run([loopx_bin(), "config", "aliases"], env=env, capture_output=True, text=True, timeout=30)


def test_the_legacy_directory_is_moved_and_still_reachable_by_its_old_name(tmp_path):
    legacy = tmp_path / ".loop-engineering"
    legacy.mkdir()
    (legacy / "projects.json").write_text(json.dumps({
        "gitlab_instance": "x", "projects": {"web": {"project_id": "g/web"}},
    }))
    out = aliases(tmp_path)
    assert out.returncode == 0 and "web" in out.stdout
    assert (tmp_path / ".loopx" / "projects.json").is_file()
    assert legacy.is_symlink() and (legacy / "projects.json").is_file()


def test_a_fresh_machine_uses_loopx_and_creates_no_legacy_link(tmp_path):
    aliases(tmp_path)
    assert not (tmp_path / ".loop-engineering").exists()

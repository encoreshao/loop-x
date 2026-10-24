"""Shared fixtures for the black-box suite that drives the `loopx` binary.

Every test runs against scratch directories for the per-machine config, the
event ledger, the eval results and the update state, so nothing here can read
or write the real `~/.loopx`/`~/.loop-engineering` or this checkout's `outputs/`.
The binary under test comes from `LOOPX_BIN` (see cli_runner.py).
"""
import pytest


@pytest.fixture(autouse=True)
def _scratch_state_dirs(monkeypatch, tmp_path_factory):
    """Point every per-machine and per-checkout state directory at a scratch dir
    (inherited by the subprocesses a test spawns)."""
    for var, name in (
        ("LOOP_ENGINEERING_HOME", "home"),
        ("LOOP_EVENTS_DIR", "events"),
        ("LOOP_EVALS_DIR", "evals"),
        ("LOOP_UPDATE_DIR", "update"),
    ):
        monkeypatch.setenv(var, str(tmp_path_factory.mktemp(name)))

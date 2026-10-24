"""`loopx secrets`: migrating plaintext tokens out of ~/.gitlab and ~/.slack into the
secret store, and `loopx gitlab` / `loopx notify` still working off the references.

Rust-only; skipped without LOOPX_BIN. LOOPX_KEYCHAIN=file:<tmp> keeps every run off the
real Keychain.
"""
import json
import stat
import subprocess

import pytest

from cli_runner import loopx_bin
from fake_gitlab import FakeGitLab
from fake_webhook import Webhook

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")

TOKEN = "glpat-PLAINTEXT-TOKEN"
VIP = "glpat-PLAINTEXT-VIP"


@pytest.fixture
def setup(tmp_path):
    hook = Webhook()
    with FakeGitLab({"/api/v4/user": {"id": 1, "username": "dev"}}) as gl:
        (tmp_path / ".gitlab").mkdir()
        (tmp_path / ".gitlab" / "config.json").write_text(json.dumps({
            "default": "fake",
            "instances": {"fake": {"url": gl.url, "token": TOKEN}},
            "bundles": {"vip": {"instance": "fake", "token": VIP}},
        }))
        (tmp_path / ".slack").mkdir()
        (tmp_path / ".slack" / "config.json").write_text(json.dumps({
            "webhook_url": hook.url + "/default",
            "bundle_webhooks": {"vip": hook.url + "/vip"},
        }))
        env = {
            "PATH": "/usr/bin:/bin", "HOME": str(tmp_path),
            "LOOP_ENGINEERING_HOME": str(tmp_path / "home"),
            "LOOPX_KEYCHAIN": f"file:{tmp_path / 'keychain'}",
        }
        yield env, gl, hook, tmp_path
    hook.close()


def run(env, *args, input=None):
    return subprocess.run([loopx_bin(), *args], env=env, capture_output=True, text=True, input=input)


def configs(tmp_path):
    return ((tmp_path / ".gitlab" / "config.json").read_text(),
            (tmp_path / ".slack" / "config.json").read_text())


def test_refuses_without_yes_and_changes_nothing(setup):
    env, _, _, tmp = setup
    before = configs(tmp)
    r = run(env, "secrets", "migrate")
    assert r.returncode == 1 and "--yes" in r.stderr and "gitlab-config" in r.stderr
    assert configs(tmp) == before
    assert not (tmp / "keychain").exists()


def test_dry_run_lists_fields_without_values_and_writes_nothing(setup):
    env, _, _, tmp = setup
    before = configs(tmp)
    r = run(env, "secrets", "migrate", "--dry-run")
    assert r.returncode == 0, r.stderr
    assert "instances.fake.token" in r.stdout and "would migrate" in r.stdout
    assert TOKEN not in r.stdout + r.stderr and VIP not in r.stdout + r.stderr
    assert "http://" not in r.stdout
    assert configs(tmp) == before and not (tmp / "keychain").exists()
    assert not list((tmp / ".gitlab").glob("*.bak-*"))


def test_migrate_moves_secrets_backs_up_and_is_idempotent(setup):
    env, _, _, tmp = setup
    r = run(env, "secrets", "migrate", "--yes")
    assert r.returncode == 0, r.stderr
    assert TOKEN not in r.stdout + r.stderr
    gitlab_text, slack_text = configs(tmp)
    assert TOKEN not in gitlab_text and VIP not in gitlab_text and "127.0.0.1" not in slack_text
    doc = json.loads(gitlab_text)
    assert doc["instances"]["fake"]["token"] == "keychain:loop-engineering.config/gitlab/instances/fake/token"
    assert doc["instances"]["fake"]["url"].startswith("http://127.0.0.1")  # non-secrets untouched
    (backup,) = (tmp / ".gitlab").glob("config.json.bak-*")
    assert TOKEN in backup.read_text() and stat.S_IMODE(backup.stat().st_mode) == 0o600
    assert stat.S_IMODE((tmp / ".gitlab" / "config.json").stat().st_mode) == 0o600

    again = run(env, "secrets", "migrate", "--yes")
    assert again.returncode == 0 and "nothing to migrate" in again.stdout
    assert configs(tmp) == (gitlab_text, slack_text)
    assert len(list((tmp / ".gitlab").glob("config.json.bak-*"))) == 1

    listing = run(env, "secrets", "list").stdout
    assert "gitlab/bundles/vip/token" in listing and "reference (resolves)" in listing
    assert TOKEN not in listing and "PLAINTEXT" not in listing


def test_gitlab_and_notify_resolve_references_transparently(setup):
    env, gl, hook, tmp = setup
    run(env, "secrets", "migrate", "--yes")
    r = run(env, "gitlab", "whoami")
    assert r.returncode == 0, r.stderr
    assert gl.requests[-1]["token"] == TOKEN
    r = run(env, "notify", "--bundle=vip", "hello")
    assert r.returncode == 0, r.stderr
    assert [p["path"] for p in hook.posts] == ["/vip"]


def test_plain_values_still_work_when_nothing_was_migrated(setup):
    env, gl, hook, _ = setup
    assert run(env, "gitlab", "whoami").returncode == 0
    assert gl.requests[-1]["token"] == TOKEN
    assert run(env, "notify", "hi").returncode == 0 and hook.posts[0]["path"] == "/default"


def test_a_dangling_reference_is_a_clear_error_not_a_blank_token(setup):
    env, _, _, tmp = setup
    run(env, "secrets", "migrate", "--yes")
    run(env, "secrets", "delete", "gitlab/instances/fake/token")
    r = run(env, "gitlab", "whoami")
    assert r.returncode != 0 and "secret store" in r.stderr and TOKEN not in r.stderr
    assert "MISSING" in run(env, "secrets", "list").stdout


def test_set_reads_the_value_from_stdin_and_never_echoes_it(setup):
    env, _, _, tmp = setup
    r = run(env, "secrets", "set", "custom/thing", input="s3cr3t-value\n")
    assert r.returncode == 0 and "s3cr3t-value" not in r.stdout + r.stderr
    assert "custom/thing" in run(env, "secrets", "list").stdout
    assert run(env, "secrets", "delete", "custom/thing").returncode == 0
    assert "custom/thing" not in run(env, "secrets", "list").stdout

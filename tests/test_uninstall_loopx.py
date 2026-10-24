"""bin/scripts/uninstall-loopx.sh: removes a loopx install (Homebrew/web or Mac app).

launchctl and `security` are injected as fakes (LOOPX_LAUNCHCTL_CMD /
LOOPX_SECURITY_CMD) that only log - these tests never reach the real launchd,
LaunchAgents or Keychain, and always pass an explicit --project-dir.
"""
import hashlib
import json
import os
import signal
import subprocess
import time
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parent.parent / "bin" / "scripts" / "uninstall-loopx.sh"

CONFIG_FILES = ("projects.json", "topics.json", "loops.json", "ai_cli.json")


def make_fake_launchctl(tmp_path):
    log = tmp_path / "launchctl.log"
    log.touch()
    exe = tmp_path / "fake-launchctl"
    exe.write_text(f'#!/usr/bin/env bash\necho "$@" >> {str(log)!r}\nexit 0\n')
    exe.chmod(0o755)
    return exe, log


def make_fake_security(tmp_path, services):
    """`security dump-keychain` lists the services; each delete-generic-password
    -s <svc> removes one item from <svc>'s count and exits 44 once none are left."""
    state = tmp_path / "keychain.json"
    state.write_text(json.dumps(services))
    log = tmp_path / "security.log"
    log.touch()
    exe = tmp_path / "fake-security"
    exe.write_text(f"""#!/usr/bin/env python3
import json, sys
state_path, log_path = {str(state)!r}, {str(log)!r}
state = json.load(open(state_path))
args = sys.argv[1:]
open(log_path, "a").write(" ".join(args) + "\\n")
if args[:1] == ["dump-keychain"]:
    for svc, n in state.items():
        if n > 0:
            print('    "svce"<blob>="%s"' % svc)
    sys.exit(0)
if args[:1] == ["delete-generic-password"]:
    svc = args[args.index("-s") + 1]
    if state.get(svc, 0) <= 0:
        sys.exit(44)
    state[svc] -= 1
    json.dump(state, open(state_path, "w"))
    sys.exit(0)
sys.exit(1)
""")
    exe.chmod(0o755)
    return exe, state, log


class Box:
    def __init__(self, tmp_path, keychain=None):
        self.tmp = tmp_path
        self.project = tmp_path / "project"
        self.agents = tmp_path / "LaunchAgents"
        self.agents.mkdir()
        self.launchctl, self.launchctl_log = make_fake_launchctl(tmp_path)
        self.security, self.keychain_state, self.security_log = make_fake_security(tmp_path, keychain or {})
        self.home = tmp_path / "home"
        self.home.mkdir()

    def populate(self, with_config=True):
        self.project.mkdir(exist_ok=True)
        (self.project / "bin").mkdir(exist_ok=True)
        (self.project / "bin" / "loopx").write_text("#!/bin/sh\n")
        (self.project / "outputs").mkdir(exist_ok=True)
        (self.project / "outputs" / "run.log").write_text("x")
        (self.project / "instructions.md").write_text("mine")
        (self.project / "connectors.json").write_text("{}")
        if with_config:
            for n in CONFIG_FILES:
                (self.project / n).write_text(f'{{"keep": "{n}"}}')

    def agent(self, name):
        (self.agents / name).write_text("<plist/>")
        return self.agents / name

    def env(self, **extra):
        env = {**os.environ, "HOME": str(self.home),
               "LOOPX_LAUNCHCTL_CMD": str(self.launchctl), "LOOPX_SECURITY_CMD": str(self.security)}
        env.pop("LOOP_ENGINEERING_HOME", None)
        env.update(extra)
        return env

    def run(self, *args, check=True, env=None, project=True):
        cmd = ["bash", str(SCRIPT), "--launch-agents-dir", str(self.agents)]
        if project:
            cmd += ["--project-dir", str(self.project)]
        return subprocess.run([*cmd, *args], check=check, capture_output=True, text=True, env=env or self.env())

    def launchctl_calls(self):
        return self.launchctl_log.read_text().splitlines()

    def keychain(self):
        return json.loads(self.keychain_state.read_text())


@pytest.fixture
def box(tmp_path):
    return Box(tmp_path)


def test_removes_the_loopx_launch_agents_with_bootout(box):
    box.populate()
    for n in ("com.hermes.loop-engineering.plist", "com.hermes.loop-engineering-dashboard.plist",
              "com.hermes.loop-engineering-topic-monitor.plist", "homebrew.mxcl.loopx.plist"):
        box.agent(n)
    unrelated = box.agent("com.example.other.plist")
    r = box.run()
    assert [p.name for p in box.agents.iterdir()] == ["com.example.other.plist"] and unrelated.exists()
    calls = box.launchctl_calls()
    uid = str(os.getuid())
    for label in ("com.hermes.loop-engineering", "com.hermes.loop-engineering-dashboard",
                  "com.hermes.loop-engineering-topic-monitor", "homebrew.mxcl.loopx"):
        assert f"bootout gui/{uid}/{label}" in calls
    assert not any("example.other" in c for c in calls)
    assert "Removed com.hermes.loop-engineering.plist" in r.stdout


def test_no_agents_installed_is_fine_and_idempotent(box):
    box.populate()
    box.run()
    r = box.run()
    assert "No loopx launchd agents" in r.stdout
    assert "does not exist" in r.stdout


def test_removes_the_project_dir_including_config_by_default(box):
    box.populate()
    box.run()
    assert not box.project.exists()


def test_keep_config_keeps_exactly_the_four_config_files(box):
    box.populate()
    (box.project / ".hidden").write_text("x")
    (box.project / "launchd").mkdir()
    (box.project / "loopx.lock").write_text("{}")
    r = box.run("--keep-config")
    assert sorted(p.name for p in box.project.iterdir()) == sorted(CONFIG_FILES)
    assert json.loads((box.project / "projects.json").read_text()) == {"keep": "projects.json"}
    assert "--keep-config" in r.stdout


def test_keep_config_still_removes_agents_and_keychain(tmp_path):
    box = Box(tmp_path, keychain={"loop-engineering.oauth": 2})
    box.populate()
    box.agent("com.hermes.loop-engineering.plist")
    box.run("--keep-config")
    assert not list(box.agents.iterdir())
    assert box.keychain()["loop-engineering.oauth"] == 0


def test_stale_lock_is_removed(box):
    box.populate()
    (box.project / "loopx.lock").write_text('{"pid": 4000000000, "port": 1}')
    r = box.run("--keep-config")
    assert not (box.project / "loopx.lock").exists()
    assert "Removed" in r.stdout


def test_live_loopx_backend_is_stopped_but_a_foreign_pid_is_not(box, tmp_path):
    box.populate()
    # a process whose executable is named `loopx`
    fake = tmp_path / "bin" / "loopx"
    fake.parent.mkdir()
    fake.symlink_to("/bin/sleep")
    victim = subprocess.Popen([str(fake), "60"])
    other = subprocess.Popen(["/bin/sleep", "60"])
    try:
        time.sleep(0.3)
        (box.project / "loopx.lock").write_text(json.dumps({"pid": victim.pid, "port": 1}))
        r = box.run("--keep-config")
        assert victim.wait(timeout=10) == -signal.SIGTERM
        assert f"Stopped loopx backend (pid {victim.pid})" in r.stdout

        (box.project / "loopx.lock").write_text(json.dumps({"pid": other.pid, "port": 1}))
        r = box.run("--keep-config")
        assert other.poll() is None
        assert "not a loopx process" in r.stdout
        assert not (box.project / "loopx.lock").exists()
    finally:
        for p in (victim, other):
            if p.poll() is None:
                p.kill()


def test_keychain_deletes_every_item_of_the_real_services_only(tmp_path):
    box = Box(tmp_path, keychain={
        "loop-engineering.oauth": 3, "loop-engineering.config": 1, "loop-engineering.mail": 2,
        "loop-engineering.connectors": 1, "loop-engineering.oauth.sandbox-abc1234567": 4,
        "com.other.app": 5,
    })
    box.populate()
    box.run()
    kc = box.keychain()
    assert [kc[s] for s in ("loop-engineering.oauth", "loop-engineering.config",
                            "loop-engineering.mail", "loop-engineering.connectors")] == [0, 0, 0, 0]
    assert kc["loop-engineering.oauth.sandbox-abc1234567"] == 4  # not sandboxed -> untouched
    assert kc["com.other.app"] == 5
    assert not any("com.other.app" in c for c in box.security_log.read_text().splitlines())


def test_sandboxed_run_deletes_only_its_own_sandbox_services(tmp_path):
    sandbox_home = tmp_path / "sandbox-home"
    sandbox_home.mkdir()
    digest = hashlib.sha256(os.path.realpath(sandbox_home).encode()).hexdigest()[:10]
    mine = f"loop-engineering.oauth.sandbox-{digest}"
    box = Box(tmp_path, keychain={
        mine: 2, "loop-engineering.oauth.sandbox-0000000000": 2, "loop-engineering.oauth": 2,
    })
    box.populate()
    box.run(env=box.env(LOOP_ENGINEERING_HOME=str(sandbox_home)))
    kc = box.keychain()
    assert kc[mine] == 0
    assert kc["loop-engineering.oauth.sandbox-0000000000"] == 2
    assert kc["loop-engineering.oauth"] == 2


def test_keep_keychain_leaves_entries(tmp_path):
    box = Box(tmp_path, keychain={"loop-engineering.oauth": 2})
    box.populate()
    box.run("--keep-keychain")
    assert box.keychain()["loop-engineering.oauth"] == 2
    assert "dump-keychain" not in box.security_log.read_text()


def test_scratch_project_dir_never_reaches_the_real_keychain_or_launchctl(tmp_path):
    # No injected commands: the real `security`/`launchctl` must not be used.
    box = Box(tmp_path)
    box.populate()
    box.agent("com.hermes.loop-engineering.plist")
    env = {k: v for k, v in box.env().items() if k not in ("LOOPX_LAUNCHCTL_CMD", "LOOPX_SECURITY_CMD")}
    fake_bin = tmp_path / "pathbin"
    fake_bin.mkdir()
    for name in ("security", "launchctl"):
        (fake_bin / name).write_text(f"#!/bin/sh\necho {name} >> {tmp_path / 'real_calls.log'}\n")
        (fake_bin / name).chmod(0o755)
    env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
    r = box.run(env=env)
    assert not (tmp_path / "real_calls.log").exists()
    assert "not calling launchctl" in r.stdout and "not touching the real Keychain" in r.stdout
    assert not list(box.agents.iterdir())  # plist file still removed
    assert not box.project.exists()


def test_refuses_to_run_from_another_checkout_without_project_dir(tmp_path):
    box = Box(tmp_path)
    # SCRIPT lives in the repo checkout, not in $HOME/.loop-engineering
    r = box.run(check=False, project=False)
    assert r.returncode == 1 and "Refusing to uninstall" in r.stderr
    assert box.launchctl_calls() == []


def test_remove_app_deletes_the_bundle_from_the_given_apps_dir(box):
    box.populate()
    apps = box.tmp / "Applications"
    (apps / "LoopX.app" / "Contents").mkdir(parents=True)
    (apps / "Other.app").mkdir()
    r = box.run("--remove-app", "--apps-dir", str(apps))
    assert not (apps / "LoopX.app").exists() and (apps / "Other.app").exists()
    assert "Removed" in r.stdout


def test_app_is_kept_without_remove_app(box):
    box.populate()
    apps = box.tmp / "Applications"
    (apps / "LoopX.app").mkdir(parents=True)
    box.run("--apps-dir", str(apps))
    assert (apps / "LoopX.app").exists()


def test_unknown_flag_prints_usage():
    r = subprocess.run(["bash", str(SCRIPT), "--bogus"], capture_output=True, text=True)
    assert r.returncode == 1 and "Usage: uninstall-loopx.sh" in r.stderr


def test_works_piped_through_bash_with_no_local_clone(box):
    box.populate()
    r = subprocess.run(
        ["bash", "-s", "--", "--launch-agents-dir", str(box.agents), "--project-dir", str(box.project)],
        input=SCRIPT.read_text(), capture_output=True, text=True, env=box.env(), check=True,
    )
    assert not box.project.exists() and "Uninstall done" in r.stdout


def test_an_unmigrated_legacy_install_is_found_and_removed_by_default(box):
    legacy = box.home / ".loop-engineering"
    legacy.mkdir()
    (legacy / "projects.json").write_text("{}")
    r = subprocess.run(
        ["bash", "-s", "--", "--launch-agents-dir", str(box.agents)],
        input=SCRIPT.read_text(), capture_output=True, text=True, env=box.env(), check=True,
    )
    assert not legacy.exists() and "Uninstall done" in r.stdout


def test_the_migration_symlink_is_removed_with_the_new_default_dir(box):
    new = box.home / ".loopx"
    new.mkdir()
    (new / "projects.json").write_text("{}")
    (box.home / ".loop-engineering").symlink_to(new)
    subprocess.run(
        ["bash", "-s", "--", "--launch-agents-dir", str(box.agents)],
        input=SCRIPT.read_text(), capture_output=True, text=True, env=box.env(), check=True,
    )
    assert not new.exists() and not (box.home / ".loop-engineering").is_symlink()

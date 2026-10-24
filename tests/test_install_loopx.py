"""bin/scripts/install-loopx.sh: the clone-free installer for the loopx binary.

Everything runs against a fake release laid out in a tmp dir (served over
file://, and once over a local HTTP server), a scratch HOME, a scratch --dir and
--launch-agents-dir, and a fake `launchctl` first on PATH - never the real
~/.loopx, ~/Library/LaunchAgents or launchd (see CLAUDE.md).
"""
import hashlib
import http.server
import json
import os
import plistlib
import shutil
import subprocess
import tarfile
import threading
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "bin" / "scripts" / "install-loopx.sh"

DASHBOARD = "com.hermes.loop-engineering-dashboard"
SCHEDULER = "com.hermes.loop-engineering"

FAKE_LOOPX = """#!/usr/bin/env bash
if [ "$1" = "--version" ]; then echo "loopx {version}"; exit 0; fi
if [ "$1" = "ledger" ] && [ "$2" = "backfill" ]; then
  echo "$LOOP_ENGINEERING_HOME" > "$LOOP_ENGINEERING_HOME/ledger_backfill_ran.txt"
  exit "${{FAKE_BACKFILL_EXIT:-0}}"
fi
exit 0
"""

LEGACY_DASHBOARD_PLIST = """<plist><dict>
  <key>Label</key><string>com.hermes.loop-engineering-dashboard</string>
  <key>ProgramArguments</key><array>
    <string>/usr/bin/python3</string>
    <string>/old/bin/web/dashboard_server.py</string>
    <string>48500</string>
  </array></dict></plist>
"""
LEGACY_SCHEDULER_PLIST = """<plist><dict>
  <key>Label</key><string>com.hermes.loop-engineering</string>
  <key>ProgramArguments</key><array>
    <string>/usr/bin/python3</string>
    <string>/old/bin/loop_scheduler.py</string>
  </array></dict></plist>
"""


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def make_release(root, version="9.9.9", arches=("universal",), binary=None, corrupt=()):
    """A GitHub-Release-shaped dir: per-arch tarballs, loopx-support.tar.gz, SHA256SUMS."""
    root = Path(root)
    root.mkdir(parents=True, exist_ok=True)
    stage = root.parent / (root.name + "-stage")
    shutil.rmtree(stage, ignore_errors=True)
    stage.mkdir(parents=True)
    sums = []
    for arch in arches:
        bin_dir = stage / f"bin-{arch}"
        bin_dir.mkdir()
        exe = bin_dir / "loopx"
        if binary:
            shutil.copy(binary, exe)
        else:
            exe.write_text(FAKE_LOOPX.format(version=f"{version} ({arch})"))
        exe.chmod(0o755)
        asset = root / f"loopx-darwin-{arch}.tar.gz"
        with tarfile.open(asset, "w:gz") as t:
            t.add(exe, arcname="loopx")
        sums.append((sha256(asset), asset.name))
    support = root / "loopx-support.tar.gz"
    with tarfile.open(support, "w:gz") as t:
        for f in sorted((REPO / "config").glob("*.template")):
            t.add(f, arcname=f"config/{f.name}")
        for f in sorted((REPO / "launchd" / "loopx").glob("*.plist.template")):
            t.add(f, arcname=f"launchd/loopx/{f.name}")
    sums.append((sha256(support), support.name))
    (root / "SHA256SUMS").write_text("".join(f"{h}  {n}\n" for h, n in sums))
    for name in corrupt:
        (root / name).write_bytes((root / name).read_bytes() + b"tampered")
    return root


def make_fake_launchctl(tmp_path, loaded=()):
    fake_bin = tmp_path / "fake_bin"
    fake_bin.mkdir(exist_ok=True)
    log = tmp_path / "launchctl_calls.txt"
    log.touch()
    script = fake_bin / "launchctl"
    script.write_text(f"""#!/usr/bin/env bash
echo "$@" >> {str(log)!r}
if [ "$1" = "list" ]; then
  case " {' '.join(loaded)} " in *" $2 "*) exit 0 ;; esac
  exit 1
fi
exit 0
""")
    script.chmod(0o755)
    return fake_bin, log


class Box:
    """One scratch machine: HOME, install dir, LaunchAgents dir, release, fake launchctl."""

    def __init__(self, tmp_path, loaded=(), **release_kw):
        self.tmp = tmp_path
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.dir = tmp_path / "install"
        self.agents = tmp_path / "LaunchAgents"
        self.release = make_release(tmp_path / "release", **release_kw)
        self.fake_bin, self.log = make_fake_launchctl(tmp_path, loaded)

    def env(self, **extra):
        env = {
            **os.environ,
            "HOME": str(self.home),
            "PATH": f"{self.fake_bin}{os.pathsep}{os.environ['PATH']}",
            "LOOPX_RELEASE_BASE": self.release.as_uri(),
        }
        env.pop("LOOP_ENGINEERING_HOME", None)
        env.update(extra)
        return env

    def run(self, *args, check=True, env=None):
        cmd = ["bash", str(SCRIPT), "--dir", str(self.dir), "--launch-agents-dir", str(self.agents)]
        return subprocess.run(
            [*cmd, *args], check=check, capture_output=True, text=True, env=env or self.env(),
        )

    def calls(self):
        return [line for line in self.log.read_text().splitlines() if line]

    def plist(self, label):
        return (self.dir / "launchd" / f"{label}.plist").read_text()


@pytest.fixture
def box(tmp_path):
    return Box(tmp_path)


def test_fresh_install_puts_the_verified_binary_in_dir_bin(box):
    r = box.run("--skip-service")
    exe = box.dir / "bin" / "loopx"
    assert exe.exists() and os.access(exe, os.X_OK)
    assert "loopx 9.9.9" in subprocess.run([exe, "--version"], capture_output=True, text=True).stdout
    assert "Verified" in r.stdout
    assert not (box.dir / ".git").exists()


def test_install_scaffolds_config_with_home_substituted(box):
    box.run("--skip-service")
    for name in ("projects.json", "topics.json", "loops.json", "ai_cli.json", "inboxes.json", "connectors.json"):
        assert (box.dir / name).is_file(), name
    projects = (box.dir / "projects.json").read_text()
    assert "{{HOME}}" not in projects
    assert json.loads(projects)["worktree_root"] == f"{box.home}/.loopx/worktrees"


def test_install_seeds_scheduler_state_for_every_loop_with_todays_date(box):
    box.run("--skip-service")
    names = [l["name"] for l in json.loads((box.dir / "loops.json").read_text())]
    state = json.loads((box.dir / "loop_scheduler_state.json").read_text())
    assert list(state) == names and len(names) > 5
    dates = {v["last_attempted_date"] for v in state.values()}
    assert len(dates) == 1 and len(next(iter(dates))) == 10


def test_install_leaves_existing_config_and_state_alone(box):
    box.dir.mkdir()
    for name in ("projects.json", "topics.json", "loops.json", "ai_cli.json", "loop_scheduler_state.json"):
        (box.dir / name).write_text('{"mine": true}')
    box.run("--skip-service")
    for name in ("projects.json", "topics.json", "loops.json", "ai_cli.json", "loop_scheduler_state.json"):
        assert (box.dir / name).read_text() == '{"mine": true}', name


def test_config_path_flags_redirect_the_scaffolds(box):
    custom = box.tmp / "cfg"
    box.run("--skip-service", "--config-path", str(custom / "p.json"), "--loops-config-path", str(custom / "l.json"),
            "--state-path", str(custom / "s.json"))
    assert (custom / "p.json").exists() and (custom / "l.json").exists()
    assert json.loads((custom / "s.json").read_text())
    assert not (box.dir / "projects.json").exists()


def test_install_renders_loopx_plists_for_this_machine(box):
    box.run("--skip-service", "--port", "48433")
    dash = box.plist(DASHBOARD)
    sched = box.plist(SCHEDULER)
    exe = str(box.dir / "bin" / "loopx")
    assert f"<string>{exe}</string>" in dash and "<string>serve</string>" in dash
    assert "<string>48433</string>" in dash
    assert "<string>--no-legacy</string>" in dash
    assert f"<string>{exe}</string>" in sched and "<string>scheduler</string>" in sched
    assert "<integer>900</integer>" in sched
    for text in (dash, sched):
        assert "{{" not in text
        env = plistlib.loads(text.encode())["EnvironmentVariables"]
        assert env["LOOP_ENGINEERING_HOME"] == str(box.dir)
        assert str(box.home) in env["PATH"]
    assert (box.dir / "outputs" / "history").is_dir()


def test_default_port_is_in_the_documented_range(box):
    box.run("--skip-service")
    port = int(box.plist(DASHBOARD).split("<string>serve</string>")[1].split("<string>")[2].split("<")[0])
    assert 48420 <= port <= 48620


def test_legacy_python_dashboard_present_drops_no_legacy_flag(box):
    (box.dir / "bin" / "web").mkdir(parents=True)
    (box.dir / "bin" / "web" / "dashboard_server.py").write_text("")
    box.run("--skip-service")
    assert "--no-legacy" not in box.plist(DASHBOARD)


def test_skip_service_never_calls_launchctl_and_copies_no_plist(box):
    box.run("--skip-service")
    assert box.calls() == []
    assert not box.agents.exists()


def test_fresh_install_does_not_load_anything_unless_asked(box):
    box.run()
    assert not any(c.startswith(("load", "kickstart", "unload")) for c in box.calls())
    assert not (box.agents / f"{DASHBOARD}.plist").exists()


def test_load_agents_loads_only_the_dashboard(box):
    box.run("--load-agents")
    assert f"load -w {box.agents}/{DASHBOARD}.plist" in box.calls()
    assert (box.agents / f"{DASHBOARD}.plist").exists()
    assert not (box.agents / f"{SCHEDULER}.plist").exists()
    assert not any(f"/{SCHEDULER}.plist" in c for c in box.calls())


def test_load_failure_warns_and_continues(tmp_path):
    box = Box(tmp_path)
    (box.fake_bin / "launchctl").write_text('#!/usr/bin/env bash\nif [ "$1" = list ]; then exit 1; fi\nexit 5\n')
    r = box.run("--load-agents")
    assert "launchctl load failed" in r.stderr
    assert not (box.agents / f"{DASHBOARD}.plist").exists()


def test_checksum_mismatch_installs_nothing(tmp_path):
    box = Box(tmp_path, corrupt=("loopx-darwin-universal.tar.gz",))
    r = box.run("--skip-service", check=False)
    assert r.returncode != 0 and "SHA-256 mismatch" in r.stderr
    assert not (box.dir / "bin" / "loopx").exists()
    assert not (box.dir / "projects.json").exists()


def test_tampered_support_bundle_is_rejected_too(tmp_path):
    box = Box(tmp_path, corrupt=("loopx-support.tar.gz",))
    r = box.run("--skip-service", check=False)
    assert r.returncode != 0 and "SHA-256 mismatch" in r.stderr
    assert not (box.dir / "bin" / "loopx").exists()


def test_asset_missing_from_sha256sums_is_refused(tmp_path):
    box = Box(tmp_path)
    lines = [l for l in (box.release / "SHA256SUMS").read_text().splitlines() if "support" not in l]
    (box.release / "SHA256SUMS").write_text("\n".join(lines) + "\n")
    r = box.run("--skip-service", check=False)
    assert r.returncode != 0 and "not listed in SHA256SUMS" in r.stderr
    assert not (box.dir / "bin" / "loopx").exists()


def test_missing_release_fails_clearly(tmp_path):
    box = Box(tmp_path)
    r = box.run("--skip-service", check=False, env=box.env(LOOPX_RELEASE_BASE=(tmp_path / "nope").as_uri()))
    assert r.returncode != 0 and "SHA256SUMS" in r.stderr


def test_prefers_the_native_arch_build_over_universal(tmp_path):
    box = Box(tmp_path, arches=("arm64", "universal"))
    box.run("--skip-service", "--arch", "arm64")
    assert "(arm64)" in subprocess.run([box.dir / "bin" / "loopx", "--version"], capture_output=True, text=True).stdout


def test_falls_back_to_universal_when_no_native_build(tmp_path):
    box = Box(tmp_path, arches=("arm64", "universal"))
    box.run("--skip-service", "--arch", "x86_64")
    assert "(universal)" in subprocess.run([box.dir / "bin" / "loopx", "--version"], capture_output=True, text=True).stdout


def test_arch_aliases_and_unsupported_arch(tmp_path):
    box = Box(tmp_path, arches=("arm64",))
    box.run("--skip-service", "--arch", "aarch64")
    assert (box.dir / "bin" / "loopx").exists()
    r = box.run("--skip-service", "--arch", "riscv64", check=False)
    assert r.returncode != 0 and "Unsupported architecture" in r.stderr


def test_no_build_for_this_arch_is_an_error(tmp_path):
    box = Box(tmp_path, arches=("arm64",))
    r = box.run("--skip-service", "--arch", "x86_64", check=False)
    assert r.returncode != 0 and "no loopx build for x86_64" in r.stderr


def test_a_binary_that_does_not_run_is_not_installed(tmp_path):
    box = Box(tmp_path)
    exe = tmp_path / "bad"
    exe.write_text("#!/bin/sh\nexit 3\n")
    shutil.rmtree(box.release)
    make_release(box.release, binary=exe)
    r = box.run("--skip-service", check=False)
    assert r.returncode != 0 and "does not run" in r.stderr
    assert not (box.dir / "bin" / "loopx").exists()


def test_unknown_flag_prints_usage():
    r = subprocess.run(["bash", str(SCRIPT), "--bogus"], capture_output=True, text=True)
    assert r.returncode == 1 and "Usage: install-loopx.sh" in r.stderr


def test_default_dir_is_dot_loop_engineering_under_home(tmp_path):
    box = Box(tmp_path)
    subprocess.run(
        ["bash", str(SCRIPT), "--skip-service"], check=True, capture_output=True, text=True, env=box.env(),
    )
    assert (box.home / ".loopx" / "bin" / "loopx").exists()


def test_install_over_http_with_a_local_server(tmp_path):
    box = Box(tmp_path)

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=str(box.release), **kw)

        def log_message(self, *a):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Quiet)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        box.run("--skip-service", "--release-base", f"http://127.0.0.1:{server.server_port}/")
    finally:
        server.shutdown()
    assert (box.dir / "bin" / "loopx").exists()


# ----------------------------------------------------------------- --upgrade

def test_upgrade_fails_fast_when_nothing_is_installed(box):
    r = box.run("--upgrade", "--skip-service", check=False)
    assert r.returncode != 0
    assert "nothing is installed" in r.stderr and "without --upgrade" in r.stderr
    assert not box.dir.exists()


def test_upgrade_replaces_only_the_program_and_keeps_config(tmp_path):
    box = Box(tmp_path, version="1.0.0")
    box.run("--skip-service")
    (box.dir / "projects.json").write_text('{"edited": true}')
    (box.dir / "instructions.md").write_text("my instructions")
    saved_state = (box.dir / "loop_scheduler_state.json").read_text()
    make_release(box.release, version="2.0.0")
    box.run("--upgrade", "--skip-service")
    assert "loopx 2.0.0" in subprocess.run([box.dir / "bin" / "loopx", "--version"], capture_output=True, text=True).stdout
    assert (box.dir / "projects.json").read_text() == '{"edited": true}'
    assert (box.dir / "instructions.md").read_text() == "my instructions"
    assert (box.dir / "loop_scheduler_state.json").read_text() == saved_state


def test_upgrade_does_not_clobber_a_rendered_plist_or_change_its_port(box):
    box.run("--skip-service", "--port", "48444")
    plist = box.dir / "launchd" / f"{SCHEDULER}.plist"
    plist.write_text(plist.read_text().replace("<integer>900</integer>", "<integer>1800</integer>"))
    box.run("--upgrade", "--skip-service", "--port", "48555")
    assert "<integer>1800</integer>" in plist.read_text()
    assert "<string>48444</string>" in box.plist(DASHBOARD)


def test_upgrade_runs_ledger_backfill_and_survives_its_failure(box):
    box.run("--skip-service")
    box.run("--upgrade", "--skip-service")
    assert (box.dir / "ledger_backfill_ran.txt").read_text().strip() == str(box.dir)
    r = box.run("--upgrade", "--skip-service", env=box.env(FAKE_BACKFILL_EXIT="1"))
    assert r.returncode == 0 and "ledger backfill failed" in r.stderr


def test_plain_install_does_not_run_ledger_backfill(box):
    box.run("--skip-service")
    assert not (box.dir / "ledger_backfill_ran.txt").exists()


def test_upgrade_restarts_a_loaded_dashboard_with_kickstart(tmp_path):
    box = Box(tmp_path, loaded=(DASHBOARD,))
    box.run("--skip-service")
    box.run("--upgrade")
    calls = box.calls()
    assert any(c.startswith("kickstart -k") and c.endswith(f"/{DASHBOARD}") for c in calls)
    assert (box.agents / f"{DASHBOARD}.plist").exists()
    assert not any(c.startswith("load") for c in calls)


def test_upgrade_reregisters_a_loaded_scheduler_but_never_kickstarts_it(tmp_path):
    box = Box(tmp_path, loaded=(SCHEDULER,))
    box.run("--skip-service")
    box.run("--upgrade")
    calls = box.calls()
    assert f"unload {box.agents}/{SCHEDULER}.plist" in calls
    assert f"load -w {box.agents}/{SCHEDULER}.plist" in calls
    assert not any("kickstart" in c and c.endswith(f"/{SCHEDULER}") for c in calls)


def test_upgrade_leaves_agents_that_are_not_loaded_alone(box):
    box.run("--skip-service")
    box.run("--upgrade")
    assert not any(c.startswith(("load", "unload", "kickstart")) for c in box.calls())
    assert not box.agents.exists()


def test_upgrade_migrates_legacy_python_plists_keeping_the_port_and_reloading(tmp_path):
    box = Box(tmp_path, loaded=(DASHBOARD, SCHEDULER))
    (box.dir / "launchd").mkdir(parents=True)
    (box.dir / "bin").mkdir()
    (box.dir / "bin" / "loopx").write_text("#!/bin/sh\necho old\n")  # an install exists
    (box.dir / "bin" / "loopx").chmod(0o755)
    (box.dir / "launchd" / f"{DASHBOARD}.plist").write_text(LEGACY_DASHBOARD_PLIST)
    (box.dir / "launchd" / f"{SCHEDULER}.plist").write_text(LEGACY_SCHEDULER_PLIST)
    box.run("--upgrade")
    dash = box.plist(DASHBOARD)
    assert "dashboard_server.py" not in dash and "<string>serve</string>" in dash
    assert "<string>48500</string>" in dash
    assert "loop_scheduler.py" not in box.plist(SCHEDULER)
    calls = box.calls()
    # a changed program needs a real reload, not just kickstart
    assert f"unload {box.agents}/{DASHBOARD}.plist" in calls
    assert f"load -w {box.agents}/{DASHBOARD}.plist" in calls
    assert not any(c.startswith("kickstart") for c in calls)


def test_upgrade_accepts_a_legacy_install_sh_clone_as_installed(box):
    (box.dir / ".git").mkdir(parents=True)
    (box.dir / "projects.json").write_text('{"mine": 1}')
    box.run("--upgrade", "--skip-service")
    assert (box.dir / "bin" / "loopx").exists()
    assert (box.dir / "projects.json").read_text() == '{"mine": 1}'


def test_upgrade_removes_the_orphaned_topic_monitor_daemon(box):
    box.run("--skip-service")
    orphan = "com.hermes.loop-engineering-topic-monitor"
    box.agents.mkdir()
    (box.agents / f"{orphan}.plist").write_text("<plist/>")
    (box.dir / "launchd" / f"{orphan}.plist").write_text("<plist/>")
    box.run("--upgrade")
    assert not (box.agents / f"{orphan}.plist").exists()
    assert not (box.dir / "launchd" / f"{orphan}.plist").exists()
    assert any(c.startswith("unload") and orphan in c for c in box.calls())


@pytest.mark.skipif(not os.environ.get("LOOPX_BIN"), reason="needs LOOPX_BIN")
def test_installs_the_real_loopx_binary(tmp_path):
    box = Box(tmp_path, binary=os.environ["LOOPX_BIN"])
    box.run("--skip-service")
    out = subprocess.run([box.dir / "bin" / "loopx", "--version"], capture_output=True, text=True)
    assert out.returncode == 0 and out.stdout.startswith("loopx ")
    doctor = subprocess.run(
        [box.dir / "bin" / "loopx", "doctor", "--json"], capture_output=True, text=True,
        env={"HOME": str(box.home), "PATH": "/usr/bin:/bin", "LOOP_ENGINEERING_HOME": str(box.dir)},
    )
    report = json.loads(doctor.stdout)
    assert all(report["config"][k]["valid"] for k in ("projects", "loops", "topics"))


def test_a_pre_rename_install_dir_is_moved_and_leaves_a_symlink(box):
    legacy = box.home / ".loop-engineering"
    legacy.mkdir()
    (legacy / "projects.json").write_text('{"mine": true}')
    cmd = ["bash", str(SCRIPT), "--launch-agents-dir", str(box.agents)]  # default --dir
    subprocess.run(cmd, check=True, capture_output=True, text=True, env=box.env())
    new = box.home / ".loopx"
    assert json.loads((new / "projects.json").read_text()) == {"mine": True}
    assert legacy.is_symlink() and legacy.resolve() == new.resolve()
    assert (new / "bin" / "loopx").exists()

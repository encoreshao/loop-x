"""End-to-end `loopx run gitlab-loop` against a fake GitLab, a fake Slack
webhook, a bare "origin" and a fake `claude` script on PATH. Rust-only (skipped
without LOOPX_BIN); nothing here can reach a real GitLab, Slack or agent CLI.
"""
import json
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest

from cli_runner import loopx_bin
from fake_gitlab import FakeGitLab
from fake_webhook import Webhook

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")

REPO = Path(__file__).resolve().parent.parent

FAKE_CLAUDE = r"""#!/bin/sh
# Records its argv, acts like the agent for the "one issue" prompt, prints a Claude JSON envelope.
for last; do :; done
printf '%s\n' "$@" > "$FAKE_LOG/args-$(date +%s%N)-$$.txt"
case "$last" in
  *"ONE issue inside"*)
    if [ -n "$LOOP_HANDOFF_PATH" ]; then
      wt=$(loopx worktree new "$FAKE_REPO" main 1 "$FAKE_WT_ROOT") || exit 9
      cd "$wt" && echo fix > fix.txt && git add fix.txt \
        && git -c user.email=a@b.c -c user.name=n commit -qm "Fix #1" || exit 9
      mkdir -p "$(dirname "$LOOP_HANDOFF_PATH")"
      printf '%s' '{"action":"fix","branch":"loop/issue-1","target_branch":"main","title":"Fix #1: x","summary":"did it"}' > "$LOOP_HANDOFF_PATH"
    fi;;
esac
printf '%s' '{"result":"all done","total_cost_usd":0.25,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":2,"cache_creation_input_tokens":1},"duration_ms":5}'
"""


def git(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True,
                          env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@x",
                               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@x"})


@pytest.fixture
def world(tmp_path):
    """Everything a run touches, under tmp_path."""
    issues = [{"iid": 1, "title": "Broken é thing", "web_url": "http://x/1",
               "assignees": [{"username": "dev"}]}]
    gl = FakeGitLab({"/api/v4/projects/grp/alpha/issues": issues}).__enter__()
    hook = Webhook()

    home = tmp_path / "home"
    le = tmp_path / "le"
    (home / ".gitlab").mkdir(parents=True)
    (home / ".slack").mkdir()
    (home / ".gitlab" / "config.json").write_text(json.dumps({
        "default": "fake",
        "instances": {"fake": {"url": gl.url, "token": "tok"}},
        "projects": {"alpha": {"project_id": "grp/alpha"}},
    }))
    (home / ".slack" / "config.json").write_text(json.dumps({"webhook_url": hook.url + "/hook"}))

    # A real repo with a bare origin that accepts push options.
    origin = tmp_path / "origin.git"
    git("init", "--bare", "-b", "main", str(origin))
    git("config", "receive.advertisePushOptions", "true", cwd=origin)
    repo = tmp_path / "alpha"
    git("clone", str(origin), str(repo))
    (repo / "README.md").write_text("hi\n")
    git("add", "README.md", cwd=repo)
    git("commit", "-m", "init", cwd=repo)
    git("push", "-u", "origin", "main", cwd=repo)

    wt_root = tmp_path / "worktrees"
    (le / "loops" / "gitlab-issue").mkdir(parents=True)
    (le / "projects.json").write_text(json.dumps({
        "gitlab_instance": "fake", "assignee_username": "dev", "worktree_root": str(wt_root),
        "projects": {"alpha": {
            "project_id": "grp/alpha", "local_path": str(repo), "target_branch": "main",
            "install_cmd": "true", "lint_cmd": "true", "test_cmd": "true", "instance": "fake"}},
    }))
    (le / "loops.json").write_text(json.dumps([{
        "name": "gitlab-loop", "entry_point": "bin.gitlab_loop_runner", "timeout_seconds": 120,
        "log_suffix": "", "emit_run_events": True, "enabled": True,
        "schedule": {"frequency": "daily", "hour": 1, "minute": 0}}]))

    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    claude = fake_bin / "claude"
    claude.write_text(FAKE_CLAUDE)
    claude.chmod(claude.stat().st_mode | stat.S_IEXEC)
    log = tmp_path / "fake-log"
    log.mkdir()

    env = {
        "PATH": f"{fake_bin}:/usr/bin:/bin",
        "HOME": str(home), "LOOP_ENGINEERING_HOME": str(le), "LOOPX_ROOT": str(le),
        "LOOP_EVENTS_DIR": str(le / "events"), "LOOPX_NO_LOGIN_SHELL": "1",
        "FAKE_LOG": str(log), "FAKE_REPO": str(repo), "FAKE_WT_ROOT": str(wt_root),
    }
    yield {"env": env, "le": le, "gl": gl, "hook": hook, "origin": origin, "log": log, "root": tmp_path}
    gl.__exit__(None, None, None)
    hook.close()


def set_mode(world, mode):
    shutil.copy(REPO / "loops" / "gitlab-issue" / "loop.yaml", world["le"] / "loops" / "gitlab-issue" / "loop.yaml")
    path = world["le"] / "loops" / "gitlab-issue" / "loop.yaml"
    path.write_text(path.read_text().replace("mode: observe", f"mode: {mode}"))


def run(world, *extra):
    return subprocess.run([loopx_bin(), "run", "gitlab-loop", *extra], env=world["env"],
                          capture_output=True, text=True, timeout=120)


def events(world):
    out = []
    for f in sorted((world["le"] / "events").glob("*.jsonl")):
        out += [json.loads(line) for line in f.read_text().splitlines() if line.strip()]
    return out


class Call(list):
    """One recorded agent argv (one line per arg; the prompt spans many lines)."""

    @property
    def prompt(self):
        return "\n".join(self[self.index("json") + 1:])

    def arg_after(self, flag):
        return self[self.index(flag) + 1]


def agent_calls(world):
    return [Call(p.read_text().splitlines()) for p in sorted(world["log"].glob("args-*.txt"))]


def test_observe_mode_runs_every_assigned_issue_then_the_wrap_up(world):
    set_mode(world, "observe")
    r = run(world)
    assert r.returncode == 0, r.stderr

    calls = agent_calls(world)
    assert len(calls) == 2, "one issue session plus the end-of-run wrap-up"
    assert "project alias 'alpha', issue IID 1" in calls[0].prompt
    assert "'End of run' section, covering the whole batch" in calls[1].prompt
    allowed = calls[0].arg_after("--allowedTools")
    assert "Bash(loopx events*)" in allowed and "Bash(git push origin loop/issue-*)" in allowed
    assert "--max-budget-usd" in calls[0] and "--max-budget-usd" not in calls[1]

    types = [e["event_type"] for e in events(world)]
    assert types[0] == "run.started"
    assert "issue.queued" in types and "run.completed" in types
    completed = next(e for e in events(world) if e["event_type"] == "run.completed")
    assert completed["data"]["issues"] == 1 and completed["data"]["cost_usd"] == 0.25
    assert completed["data"]["input_tokens"] == 10 and completed["data"]["cache_write_tokens"] == 1
    queued = next(e for e in events(world) if e["event_type"] == "issue.queued")
    assert queued["data"] == {"title": "Broken é thing", "web_url": "http://x/1"}

    results = list((world["le"] / "outputs" / "loop-runs").glob("*/result.json"))
    assert len(results) == 1
    assert json.loads(results[0].read_text())["final_state"] == "completed"
    assert any("GitLab loop started" in t and "1 issue to check" in t for t in world["hook"].texts)
    status = json.loads((world["le"] / "outputs" / "status.json").read_text())
    assert status["state"] == "idle" and status["last_exit_code"] == 0


def test_zero_assigned_issues_still_runs_the_wrap_up(world):
    set_mode(world, "observe")
    world["gl"].routes["/api/v4/projects/grp/alpha/issues"] = []
    assert run(world).returncode == 0
    calls = agent_calls(world)
    assert len(calls) == 1 and "End of run" in calls[0].prompt
    assert any("0 issues to check" in t for t in world["hook"].texts)


def test_a_failing_agent_is_contained_and_alerted(world):
    set_mode(world, "observe")
    fake = Path(world["env"]["PATH"].split(":")[0]) / "claude"
    fake.write_text("#!/bin/sh\necho boom >&2\nexit 7\n")
    r = run(world)
    assert r.returncode == 0, "a contained per-issue failure is not a failed run"
    result = json.loads(next((world["le"] / "outputs" / "loop-runs").glob("*/result.json")).read_text())
    assert result["final_state"] != "completed"
    assert any("did not complete" in t for t in world["hook"].texts)
    assert any(e["event_type"] == "issue.agent_failed" for e in events(world))
    assert "boom" in (world["le"] / "logs" / "loop-engineering.log").read_text()


def test_gate_mode_opens_the_mr_only_after_the_loops_own_checks_pass(world):
    set_mode(world, "gate")
    r = run(world)
    assert r.returncode == 0, r.stderr

    first = agent_calls(world)[0]
    assert "Harness gate is ON" in first.prompt
    allowed = first.arg_after("--allowedTools")
    assert "Bash(loopx mr*)" not in allowed and "Bash(git push origin" not in allowed
    disallowed = first.arg_after("--disallowedTools")
    assert "Bash(loopx mr open*)" in disallowed

    assert "loop/issue-1" in git("branch", cwd=world["origin"]).stdout
    posts = [q for q in world["gl"].requests if q.get("method") == "POST"]
    assert posts and "Opened merge request for this fix" in posts[0]["body"]["body"]
    assert posts[0]["path"] == "/api/v4/projects/grp/alpha/issues/1/notes"
    completed = [e for e in events(world) if e["event_type"] == "issue.completed"]
    assert completed[0]["data"]["outcome"] == "mr_opened" and completed[0]["data"]["gated"] is True
    assert any("*Finished*" in t and "MR opened" in t for t in world["hook"].texts)
    ext = [e for e in events(world) if e["event_type"] == "verification.external_completed"]
    assert ext and ext[0]["data"]["mode"] == "gate" and ext[0]["data"]["passed"] is True


def test_gate_mode_escalates_when_the_checks_fail(world):
    set_mode(world, "gate")
    projects = world["le"] / "projects.json"
    cfg = json.loads(projects.read_text())
    cfg["projects"]["alpha"]["test_cmd"] = "sh -c 'echo failing-test-output; exit 1'"
    projects.write_text(json.dumps(cfg))
    r = run(world)
    assert r.returncode == 0, r.stderr
    assert "loop/issue-1" not in git("branch", cwd=world["origin"]).stdout, "nothing may be pushed"
    posts = [q for q in world["gl"].requests if q.get("method") == "POST"]
    assert "failing-test-output" in posts[0]["body"]["body"]
    puts = [q for q in world["gl"].requests if q.get("method") == "PUT"]
    assert puts[0]["body"] == {"add_labels": "loop:needs-human"}
    esc = [e for e in events(world) if e["event_type"] == "issue.escalated"]
    assert esc[0]["data"]["reason"] == "verification_failed"
    result = json.loads(next((world["le"] / "outputs" / "loop-runs").glob("*/result.json")).read_text())
    assert result["final_state"] in ("escalated", "failed", "stopped")
    # the retry got the failure as feedback
    assert any("Previous attempt failed external verification" in c.prompt for c in agent_calls(world))


def test_scoped_single_issue_run_uses_the_two_arg_prompt(world):
    set_mode(world, "observe")
    assert run(world, "alpha", "1").returncode == 0
    calls = agent_calls(world)
    assert len(calls) == 1 and "on-demand single-issue run" in calls[0].prompt
    started = next(e for e in events(world) if e["event_type"] == "run.started")
    assert started["data"] == {"trigger": "dashboard"}


def test_unknown_loop_is_logged_and_alerted(world):
    r = subprocess.run([loopx_bin(), "run", "nope"], env=world["env"], capture_output=True, text=True)
    assert r.returncode == 1
    assert "FAILED to resolve its loop registry entry" in (world["le"] / "logs" / "loop-engineering.log").read_text()
    assert any("could not resolve its registry entry" in t for t in world["hook"].texts)


# --- approvals: a human's OK before the loop opens the merge request ----------

def require_approval(world, timeout=None):
    projects = world["le"] / "projects.json"
    cfg = json.loads(projects.read_text())
    cfg["require_approval"] = True
    if timeout:
        cfg["approval_timeout_seconds"] = timeout
    projects.write_text(json.dumps(cfg))


def approvals(world, *args):
    r = subprocess.run([loopx_bin(), "approvals", *args], env=world["env"], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


def start(world):
    return subprocess.Popen([loopx_bin(), "run", "gitlab-loop"], env=world["env"],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def wait_for_pending(world, proc):
    import time
    for _ in range(100):
        pending = approvals(world, "list")
        if pending:
            return pending[0]
        assert proc.poll() is None, "run ended before asking for approval"
        time.sleep(0.2)
    raise AssertionError("no approval was requested")


def test_the_mr_waits_for_approval_and_opens_once_approved(world):
    set_mode(world, "gate")
    require_approval(world)
    proc = start(world)
    pending = wait_for_pending(world, proc)
    assert pending["kind"] == "open_mr" and pending["branch"] == "loop/issue-1"
    assert pending["project"] == "alpha" and pending["title"] == "Fix #1: x"
    assert "loop/issue-1" not in git("branch", cwd=world["origin"]).stdout, "nothing is pushed while waiting"
    assert any("*Approval needed*" in t for t in world["hook"].texts)
    done = approvals(world, "approve", pending["id"], "--by", "mac-app")
    assert done["status"] == "approved" and done["decided_by"] == "mac-app"
    assert proc.wait(timeout=60) == 0
    assert "loop/issue-1" in git("branch", cwd=world["origin"]).stdout
    types = [e["event_type"] for e in events(world)]
    assert types.index("approval.requested") < types.index("approval.decided") < types.index("issue.completed")
    assert approvals(world, "list") == []


def test_a_denied_approval_escalates_without_pushing(world):
    set_mode(world, "gate")
    require_approval(world)
    proc = start(world)
    pending = wait_for_pending(world, proc)
    approvals(world, "deny", pending["id"])
    assert proc.wait(timeout=60) == 0
    assert "loop/issue-1" not in git("branch", cwd=world["origin"]).stdout
    esc = [e for e in events(world) if e["event_type"] == "issue.escalated"]
    assert esc[0]["data"]["reason"] == "approval_denied"
    posts = [q for q in world["gl"].requests if q.get("method") == "POST"]
    assert "needs a human's approval" in posts[0]["body"]["body"] and "was denied" in posts[0]["body"]["body"]
    assert [q["body"] for q in world["gl"].requests if q.get("method") == "PUT"] == [{"add_labels": "loop:needs-human"}]


def test_an_unanswered_approval_times_out_and_escalates(world):
    set_mode(world, "gate")
    require_approval(world, timeout=2)
    proc = start(world)
    wait_for_pending(world, proc)
    assert proc.wait(timeout=60) == 0
    assert "loop/issue-1" not in git("branch", cwd=world["origin"]).stdout
    esc = [e for e in events(world) if e["event_type"] == "issue.escalated"]
    assert esc[0]["data"]["reason"] == "approval_timeout"
    assert approvals(world, "list", "--all")[0]["status"] == "timeout"


def test_a_late_approval_cannot_revive_a_timed_out_request(world):
    set_mode(world, "gate")
    require_approval(world, timeout=2)
    proc = start(world)
    pending = wait_for_pending(world, proc)
    proc.wait(timeout=60)
    r = subprocess.run([loopx_bin(), "approvals", "approve", pending["id"]], env=world["env"],
                       capture_output=True, text=True)
    assert r.returncode == 1 and "already timeout" in r.stderr

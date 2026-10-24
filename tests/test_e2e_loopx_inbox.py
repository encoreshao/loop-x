"""End-to-end `loopx run inbox-triage-loop` / `loopx inbox run` against a fake
Gmail + token endpoint, a fake Slack webhook and a fake `claude` on PATH.
Rust-only (skipped without LOOPX_BIN). Refresh tokens come from a file-backed
secret store (LOOPX_KEYCHAIN=file:...), so no real Keychain, mailbox, Slack or
Claude is ever reached."""
import base64
import hashlib
import json
import os
import shutil
import signal
import stat
import subprocess
import time
from pathlib import Path

import pytest

from cli_runner import loopx_bin
from fake_webhook import Webhook
from mail_stub import StubServer

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")

REPO = Path(__file__).resolve().parent.parent
U = "/gmail/v1/users/me"

FAKE_CLAUDE = r"""#!/bin/sh
# Records argv and stdin, then answers the triage prompt for messages m1 and m2.
n=$(ls "$FAKE_LOG" | wc -l | tr -d ' ')
printf '%s\n' "$@" > "$FAKE_LOG/args-$n.txt"
cat > "$FAKE_LOG/stdin-$n.txt"
if [ -f "$FAKE_LOG/hang" ]; then
  [ -f "$FAKE_LOG/term" ] && exit 1
  sleep 30; exit 1
fi
printf '%s' '{"result":"[{\"id\":\"m1\",\"category\":\"urgent\",\"reason\":\"prod is down\",\"draft_body\":\"On it - fixing now.\"},{\"id\":\"m2\",\"category\":\"newsletters\",\"reason\":\"digest\",\"draft_body\":null}]","total_cost_usd":0.25}'
"""


def b64(text):
    return base64.urlsafe_b64encode(text.encode()).decode().rstrip("=")


def raw(msg_id, sender, subject, body, ms, thread):
    hdrs = [{"name": "From", "value": sender}, {"name": "To", "value": "me@example.com"},
            {"name": "Subject", "value": subject}, {"name": "Message-ID", "value": f"<{msg_id}@x.com>"}]
    return {"id": msg_id, "threadId": thread, "internalDate": str(ms),
            "payload": {"mimeType": "text/plain", "headers": hdrs, "body": {"data": b64(body)}}}


def sandbox_service(home):
    digest = hashlib.sha256(os.path.realpath(home).encode()).hexdigest()[:10]
    return f"loop-engineering.mail.sandbox-{digest}"


def seed_token(kc, le, account, token):
    d = kc / sandbox_service(le)
    d.mkdir(parents=True)
    (d / account.encode().hex()).write_text(token)


@pytest.fixture
def world(tmp_path):
    stub = StubServer().__enter__()
    hook = Webhook()
    home, le, kc = tmp_path / "home", tmp_path / "le", tmp_path / "kc"
    (home / ".slack").mkdir(parents=True)
    (home / ".slack" / "config.json").write_text(json.dumps({"webhook_url": hook.url + "/hook"}))
    (le / "loops" / "inbox-triage").mkdir(parents=True)
    shutil.copy(REPO / "loops" / "inbox-triage" / "loop.yaml", le / "loops" / "inbox-triage" / "loop.yaml")
    (le / "instructions" / "loopx").mkdir(parents=True)
    shutil.copy(REPO / "instructions" / "loopx" / "inbox-triage.md", le / "instructions" / "loopx" / "inbox-triage.md")
    inbox = {"name": "work", "label": "Work", "provider": "gmail", "account": "me@example.com", "enabled": True,
             "urgent_brief": "", "vip_senders": [], "exclude_senders": ["@spam.com"], "categories": None,
             "slack_bundle": None}
    (le / "inboxes.json").write_text(json.dumps({"inboxes": [inbox]}))
    (le / "mail_oauth.json").write_text(json.dumps({"google": {"client_id": "gid", "client_secret": "gsecret"}}))
    seed_token(kc, le, "work", "refresh-1")
    (le / "loops.json").write_text(json.dumps([{
        "name": "inbox-triage-loop", "entry_point": "bin.inbox_triage_runner", "timeout_seconds": 120,
        "log_suffix": "-inbox-triage-loop", "emit_run_events": False, "enabled": True,
        "schedule": {"frequency": "daily", "hour": 1, "minute": 0}}]))

    stub.add("POST", "/token", body={"access_token": "at-1"})
    stub.add("GET", f"{U}/profile", body={"emailAddress": "Me@Example.com"})
    stub.add("GET", f"{U}/messages", body={"messages": [{"id": "m2"}, {"id": "m1"}, {"id": "bot"}]})
    m1 = raw("m1", "Alice <alice@x.com>", "Prod is down", "SECRET BODY one", 1790000000000, "t1")
    m2 = raw("m2", "News <news@x.com>", "Weekly digest", "SECRET BODY two", 1790000500000, "t2")
    bot = raw("bot", "bot@spam.com", "Spam", "SECRET BODY spam", 1790000100000, "t3")
    for m in (m1, m2, bot):
        stub.add("GET", f"{U}/messages/{m['id']}", body=m)
        stub.add("GET", f"{U}/threads/{m['threadId']}", body={"messages": [m]})
    stub.add("GET", f"{U}/labels", body={"labels": [{"id": "L1", "name": "Loop/Urgent"}]})
    stub.add("POST", f"{U}/labels", body={"id": "L2", "name": "Loop/Newsletters"})
    stub.add("POST", f"{U}/messages/m1/modify", body={})
    stub.add("POST", f"{U}/messages/m2/modify", body={})
    stub.add("POST", f"{U}/drafts", body={"id": "d1", "message": {"id": "dm1", "threadId": "t1"}})

    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    claude = fake_bin / "claude"
    claude.write_text(FAKE_CLAUDE)
    claude.chmod(claude.stat().st_mode | stat.S_IEXEC)
    log = tmp_path / "fake-log"
    log.mkdir()
    env = {"PATH": f"{fake_bin}:/usr/bin:/bin", "HOME": str(home), "LOOP_ENGINEERING_HOME": str(le),
           "LOOPX_ROOT": str(le), "LOOP_EVENTS_DIR": str(le / "events"), "LOOPX_NO_LOGIN_SHELL": "1",
           "LOOPX_KEYCHAIN": f"file:{kc}", "LOOPX_GMAIL_BASE_URL": stub.base_url + "/gmail/v1",
           "LOOPX_MAIL_GOOGLE_TOKEN_URL": stub.base_url + "/token", "FAKE_LOG": str(log)}
    yield {"env": env, "le": le, "stub": stub, "hook": hook, "log": log, "kc": kc, "tmp": tmp_path}
    stub.__exit__(None, None, None)
    hook.close()


def run(world, *args, entry=("run", "inbox-triage-loop")):
    return subprocess.run([loopx_bin(), *entry, *args], env={**os.environ, **world["env"]},
                          capture_output=True, text=True, timeout=120)


def out_dir(world):
    return world["le"] / "outputs" / "inbox-triage"


def events(world):
    out = []
    for f in sorted((world["le"] / "events").glob("*.jsonl")):
        out += [json.loads(line) for line in f.read_text().splitlines() if line.strip()]
    return out


def paths(world, method, path):
    return [r for r in world["stub"].requests if r["method"] == method and r["path"] == path]


def test_a_full_run_triages_labels_drafts_and_records(world):
    r = run(world)
    assert r.returncode == 0, r.stderr
    stub = world["stub"]

    # Token refresh, then bearer auth on every mail call.
    token_req = paths(world, "POST", "/token")[0]
    assert "refresh_token=refresh-1" in token_req["body"] and "client_id=gid" in token_req["body"]
    assert stub.requests[1]["headers"]["Authorization"] == "Bearer at-1"
    # The excluded sender is dropped (never labelled, never shown to the model);
    # labels are created only when missing.
    assert not stub.bodies("POST", f"{U}/messages/bot/modify")
    assert [json.loads(b)["name"] for b in stub.bodies("POST", f"{U}/labels")] == ["Loop/Newsletters"]
    assert json.loads(stub.bodies("POST", f"{U}/messages/m1/modify")[0]) == {"addLabelIds": ["L1"]}
    assert json.loads(stub.bodies("POST", f"{U}/messages/m2/modify")[0]) == {"addLabelIds": ["L2"]}
    # The reply is a threaded draft, never a send.
    draft = json.loads(stub.bodies("POST", f"{U}/drafts")[0])
    assert draft["message"]["threadId"] == "t1"
    mime = base64.urlsafe_b64decode(draft["message"]["raw"] + "==").decode()
    assert "Subject: Re: Prod is down" in mime and "On it - fixing now." in mime
    assert not any("/send" in req["path"] for req in stub.requests)

    # The model call is sealed and the mail text only travels on stdin.
    args = (world["log"] / "args-0.txt").read_text().splitlines()
    assert args[args.index("--tools") + 1] == "" and "--strict-mcp-config" in args
    assert "--max-budget-usd" in args
    assert not any("SECRET BODY" in a for a in args)
    stdin = (world["log"] / "stdin-0.txt").read_text()
    assert "SECRET BODY one" in stdin and "SECRET BODY two" in stdin and "SECRET BODY spam" not in stdin

    # Seen ids, status (no bodies), history, drafts archive, events, result.
    state = json.loads((out_dir(world) / "state" / "work.json").read_text())
    assert set(state["seen"]) == {"m1", "m2"} and state["high_water"].startswith("2026-09-21T")
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "idle" and status["counts"] == {"urgent": 1, "newsletters": 1}
    assert status["urgent"][0]["draft_link"].startswith("https://mail.google.com/")
    assert "SECRET BODY" not in (out_dir(world) / "status.json").read_text()
    history = next((out_dir(world) / "history").glob("*-work.md")).read_text()
    assert "| urgent | prod is down |" in history and "SECRET BODY" not in history
    drafts = json.loads((out_dir(world) / "drafts.json").read_text())
    assert drafts[0]["body"] == "On it - fixing now." and drafts[0]["inbox"] == "work"
    types = [e["event_type"] for e in events(world)]
    assert "loop.result" in types
    result = json.loads(next((world["le"] / "outputs" / "loop-runs").glob("*work/result.json")).read_text())
    assert result["final_state"] == "completed"
    # One digest in Slack.
    assert len(world["hook"].texts) == 1
    assert "*Work*: 1 urgent (drafts ready), 0 action, 1 other" in world["hook"].texts[0]


def test_a_second_run_does_not_redo_already_triaged_mail(world):
    assert run(world).returncode == 0
    assert run(world).returncode == 0
    assert len(list(world["log"].glob("args-*.txt"))) == 1, "the second run is quiet: no AI call"
    assert len(world["stub"].bodies("POST", f"{U}/drafts")) == 1
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "idle"
    assert any("no new mail" in t for t in world["hook"].texts)


def test_an_inbox_that_was_never_connected_needs_reauth(world):
    shutil.rmtree(world["kc"])
    assert run(world).returncode == 0
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "needs_reauth" and "not connected" in status["error"]
    assert not paths(world, "GET", f"{U}/profile") and not list(world["log"].iterdir())
    assert any("needs re-auth" in t for t in world["hook"].texts)


def test_a_rejected_refresh_token_needs_reauth(world):
    world["stub"].routes[("POST", "/token")] = [(400, {"error": "invalid_grant"}, {})]
    assert run(world).returncode == 0
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "needs_reauth" and "invalid_grant" in status["error"]


def test_the_wrong_signed_in_account_fails_before_fetching(world):
    world["stub"].routes[("GET", f"{U}/profile")] = [(200, {"emailAddress": "other@example.com"}, {})]
    assert run(world).returncode == 0
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "failed" and "Signed in as other@example.com" in status["error"]
    assert not paths(world, "GET", f"{U}/messages")


def test_codex_is_refused_before_any_mail_is_read(world):
    (world["le"] / "ai_cli.json").write_text(json.dumps({"cli": "codex"}))
    assert run(world).returncode == 0
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "failed" and "requires the Claude CLI" in status["error"]
    assert not world["stub"].requests and not list(world["log"].iterdir())


def test_a_failing_model_call_labels_nothing_and_is_reported(world):
    (Path(world["env"]["PATH"].split(":")[0]) / "claude").write_text("#!/bin/sh\ncat >/dev/null\necho boom >&2\nexit 7\n")
    assert run(world).returncode == 0
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "failed" and "AI call failed: CalledProcessError" in status["error"]
    assert not world["stub"].bodies("POST", f"{U}/messages/m1/modify")
    assert not (out_dir(world) / "state" / "work.json").exists()
    assert any("failed" in t for t in world["hook"].texts)


def test_a_draft_failure_still_labels_and_reports_manual_reply(world):
    world["stub"].routes[("POST", f"{U}/drafts")] = [(400, {"error": "bad"}, {})]
    assert run(world).returncode == 0
    assert world["stub"].bodies("POST", f"{U}/messages/m1/modify")
    status = json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]
    assert status["state"] == "idle" and status["urgent"][0]["draft_failed"] is True
    assert any("draft failed - reply manually" in t for t in world["hook"].texts)


def test_a_401_mid_run_refreshes_the_token_once(world):
    world["stub"].routes[("GET", f"{U}/labels")] = [(401, {}, {}), (200, {"labels": []}, {})]
    world["stub"].routes[("POST", "/token")] = [(200, {"access_token": "at-1"}, {}), (200, {"access_token": "at-2"}, {})]
    assert run(world).returncode == 0
    assert len(paths(world, "POST", "/token")) == 2
    assert paths(world, "GET", f"{U}/labels")[-1]["headers"]["Authorization"] == "Bearer at-2"
    assert json.loads((out_dir(world) / "status.json").read_text())["inboxes"]["work"]["state"] == "idle"


def test_the_cli_entry_matches_the_registered_loop(world):
    r = run(world, "run_cli", entry=("inbox", "run"))
    assert r.returncode == 0, r.stderr
    assert paths(world, "POST", f"{U}/drafts")
    assert next((world["le"] / "outputs" / "loop-runs").glob("run_cli_work/result.json"))


def test_loopx_run_records_the_run_status(world):
    assert run(world).returncode == 0
    status = json.loads((world["le"] / "outputs" / "status.json").read_text()) \
        if (world["le"] / "outputs" / "status.json").exists() else {}
    log = (world["le"] / "logs" / "loop-engineering.log").read_text()
    assert "inbox-triage-loop ---- run finished (exit 0)" in log
    assert "inbox-triage: claude triage call ok" in log
    assert "SECRET BODY" not in log
    assert status.get("state", "idle") in ("idle",)


def test_sigterm_marks_the_running_inbox_failed(world):
    (world["log"] / "hang").write_text("")
    proc = subprocess.Popen([loopx_bin(), "inbox", "run", "run_term"], env={**os.environ, **world["env"]},
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
    status_path = out_dir(world) / "status.json"
    for _ in range(100):
        if list(world["log"].glob("args-*.txt")):
            break
        time.sleep(0.1)
    assert json.loads(status_path.read_text())["inboxes"]["work"]["state"] == "running"
    (world["log"] / "term").write_text("")
    os.killpg(proc.pid, signal.SIGTERM)
    proc.communicate(timeout=60)
    assert proc.returncode == 128 + signal.SIGTERM
    status = json.loads(status_path.read_text())["inboxes"]["work"]
    assert status["state"] == "failed" and "interrupted" in status["error"]
    assert not world["hook"].posts, "an interrupted run sends no digest"

# Pipeline Auto-fix Loop

## Goal

When Pipeline Doctor diagnoses a failed pipeline on a merge request the issue
loop opened, and the failure is a test failure or a lint error, start one Fix
now run on it, so a red pipeline on a loop MR gets fixed without waiting for
a person. A second failure, a failed fix or a cap goes to Needs you and Slack
instead: the loop never fixes in a loop.

## How it runs

- Runner: `crates/core/src/mr_autofix.rs` (native, entry point
  `bin.mr_autofix`); definition `loops/mr-autofix/loop.yaml`.
- Trigger: `trigger.type: event` on `pipeline_doctor.diagnosed` where
  `mr_owned_by_loop` is true and `category` is `test_failure` or `lint`. The
  registry entry has no `schedule`; its `enabled` flag stops new attempts (a
  Fix now run already started keeps going until it ends or is stopped). It
  ships disabled and needs Pipeline Doctor enabled too (Pipeline Doctor is
  hourly, so a fix starts up to about an hour after the pipeline fails).
- The one-minute `loopx scheduler` tick reads yesterday's and today's event
  files, adds new matching events to the loop's inbox
  (`<loopx home>/event_triggers/mr-autofix-loop.json`) and queues the loop
  with reason `event`. The first tick that sees the subscription arms it:
  older events never match, so enabling the loop does not replay history. While
  the loop is switched off the tick deletes its inbox, so switching it back on
  arms it afresh.
  Each event is matched once (`event_triggers/mr-autofix-loop-events/seen.json`,
  30 days); the inbox holds at most 20 pending events. The quota pause and
  the run queue's limits apply as for any scheduled run.
- A run first refuses to do anything while the attempt ledger exists but
  cannot be read (it is never overwritten; reported once as
  `ledger_corrupt`). It then escalates, once each, any attempt still
  `started` more than 50 minutes ago whose Fix now job is no longer running
  (its run was stopped or killed) as `fix_failed`. Then it takes **one** due
  event from the inbox; the others wait for the next run (the tick queues it
  again while due events remain). For that event it:
  1. Re-reads the merge request from GitLab. A definite refusal drops the
     event (one log line, nothing posted or spent) unless the MR is open and
     not a draft, same-project (not a fork), on a loop branch, authored by the
     token's user, at the diagnosed `sha`, and its head pipeline is the
     diagnosed pipeline with status `failed`. If GitLab cannot be asked (a
     network error, a missing token), the event goes back to the inbox and is
     retried after 5, 10, 20 and 40 minutes; after the fifth failure, or once
     the event is a day old, it is escalated once as `verify_failed`.
     The merge request is then named by GitLab's own ids (instance, target
     project id, iid), however the event spelled the project, so one merge
     request never gets two ledger entries.
  2. Escalates `low_confidence` below `settings.min_confidence` (default 0.7).
  3. Escalates `already_attempted` if the MR already had its one attempt
     (for the MR's lifetime; a later push does not re-arm it).
  4. Escalates `daily_cap` at `settings.max_attempts_per_day` attempts in the
     last 24 hours (default 3; each Fix now run is capped at $10).
  5. Escalates `job_busy` if a Fix now run is already in progress on the MR.
  6. Records the attempt in `<loopx home>/mr-autofix/attempts.json`, then
     starts a CI-mode Fix now run (see [mr-fix-now.md](mr-fix-now.md)) and
     waits up to 40 minutes for it. On timeout, or when the autofix run is
     stopped (SIGTERM, the Stop button), it stops the Fix now job first.
  7. A pushed fix emits `mr_autofix.completed`; a run that changed nothing
     escalates `fix_nothing`; a failed or timed-out run escalates
     `fix_failed`.
- With nothing pending (or the loop switched off) the run ends as skipped.

## Escalation

`mr_autofix.escalated {mr_ref, mr_web_url, sha, pipeline_id, reason}`, one
Slack line, and a Needs you item (`mr_autofix:<mr_ref>:<sha>`, dismissable)
with a link to the merge request. Escalations are de-duplicated per
`(mr_ref, sha)`. The item stays until it is dismissed, a newer automatic fix
of the MR completes, or it is 7 days old.

## Safety boundary

- The event log is untrusted: agents can write it with `loopx events emit`.
  The `where` filter is only a pre-filter; the GitLab re-check above decides.
- One attempt per MR, recorded before the job is spawned, so a crash never
  gives a second one.
- A CI-mode Fix now run fixes only the CI failure, never skips or loosens a
  test or lint rule (its prompt says so), and ends as `nothing`, before the
  worktree is prepared, if the merge request is a draft, if its head is not
  the diagnosed sha, or if an earlier `ci:` fix is already recorded as handled
  on it. Every Fix now guard (settings-path refusal, no
  force-push, re-run checks, ignored files, start-note handshake) applies.
- The paid `loopx loop eval --golden` before and after the `mr-fix.md`
  prompt change has not been run; it is the owner's to run.

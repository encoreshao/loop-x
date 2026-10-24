# Daily GitLab Issue Loop

## Goal

Every weekday, check GitLab issues already assigned to the configured user on the configured GitLab instance, pick up anything new (a fresh issue or a new comment on one already being tracked), and then take exactly one of four actions per issue: implement a fix, verify it, and open a merge request when the ask is clear, scoped, and genuinely needs a code change; answer directly with a GitLab comment when the ask is clear but needs no code at all (a question, a status check, a request for information); post a GitLab comment asking for clarification when the ask is ambiguous; or, when the issue has already been handed off to someone else to review, wait — no GitLab comment at all, just one Slack reminder after a week if nobody has followed up. Failing verification also gets a GitLab comment instead of a guess. Every comment the loop does post is short, goal-based, and structured markdown (a one-line summary, then bold-labelled sections with one fact or question per bullet): it states a decided next action grounded in the issue's full history, or shares a progress update — never one dense, hard-to-parse paragraph. The loop also gets more capable over time: reusable lessons learned per project (fix patterns, gotchas, root-cause categories) are recorded per issue as markdown task-memory files and read back on later runs — see `loopx memory-store` (entries recorded before this format existed are still read via `loopx memory`).

## Setup

New to this loop? Run `bin/scripts/setup.sh` once (or click install on the dashboard's Settings → Skills page) — it installs the `gitlab-config` skill this loop depends on (from [encore-skills](https://github.com/encoreshao/encore-skills)). `~/.loopx/projects.json` is scaffolded from the template by `install-loopx.sh`, or you save a project from the dashboard. The dashboard's Settings → Skills view (`/settings?view=skills`) shows a live view of what's installed.

## Scope

Which GitLab projects to track, their local checkout paths, target branches, GitLab username, worktree scratch directory, and per-project install/lint/test commands are **not** hardcoded in this repo — they live in `~/.loopx/projects.json`, since every team member running this loop has their own local checkout paths. See `config/projects.json.template` for the exact format, and `crates/core/src/config.rs` for how the loop reads it.

The loop never self-assigns issues — it only tracks issues already assigned to the configured `assignee_username`, on the projects listed in that config file.

## Expected output

Each run produces or updates:
- `outputs/daily-review.md` (and an archived copy under `outputs/history/`)
- `PROGRESS.md`
- Slack messages via the webhook at `~/.slack/config.json` (per-issue start/finish + one end-of-run digest, sent every run — including mornings with no assigned issues at all)
- Zero or more GitLab comments and zero or more opened merge requests

## Safety boundary (fixed — does not loosen with time or repeated success)

- **Never merge a merge request.** The loop's job ends at "MR opened, verification passing." Merging is always a manual step for the human.
- Every code change happens inside an isolated git worktree, under the configured `worktree_root`, on a branch named `loop/issue-<iid>`, never on the checkout's target branch.
- An MR is only opened if the project's own configured `test_cmd`/`lint_cmd` pass, and the diff only touches files relevant to the issue.
- Only the command allow-list in `instructions/loopx/gitlab-issue.md` may run — no arbitrary shell, no dependency upgrades, no reading `.env`/credentials/SSH keys.
- Issues are processed one at a time, sequentially — never multiple worktrees/fixes in parallel in the same run.
- In `observe` mode (the default) the same verification failure on the same issue is not retried within a run — it escalates via a GitLab comment instead. In `gate` mode the loop retries at most `stop_conditions.max_iterations` times (see below); it never opens an MR whose tests/lint fail.
- The loop only touches: issues/comments/MRs on the projects listed in `~/.loopx/projects.json`, its own git worktrees (under the configured `worktree_root`), and its own state files (`PROGRESS.md`, `outputs/`).

## Verification gate (observe vs gate)

After the agent's work on an issue, the runner independently re-runs the project's own `test_cmd`/`lint_cmd` (from `projects.json`) inside the worktree the agent used, via `ProjectCommandsVerifier` in `crates/core/src/loop_verifiers.rs`, inside `LoopRuntime`. No worktree or no configured commands (the agent made no code change) is a vacuous pass, not a failure. `verification.mode` in `loops/gitlab-issue/loop.yaml` selects what the result does:

- **`observe` (default).** The real result is recorded (`observed_passed` in the iteration's evidence and the `verification.external_completed` event) but the iteration always passes, so outcomes are unchanged. The agent still pushes and opens the MR itself.
- **`gate`.** A failing check fails the iteration. `LoopRuntime` retries up to `stop_conditions.max_iterations` (2) with the failing command's output tail as feedback (`format_feedback`, bounded). The agent's allowlist drops `loopx mr open` and `git push` (`allowed_tools(.., gate = true, ..)` in `crates/core/src/gitlab_runner.rs`), a `GATE_OVERRIDE` prompt section tells it to write a handoff JSON to `outputs/handoffs/<run_id>/<alias>-<iid>.json` (also in `$LOOP_HANDOFF_PATH`) instead, and the runner (`finalize_gated_issue`) reads it (`read_handoff`) and runs `loopx mr open` itself only after verification passed. In gate mode the external check runs only for an attempt whose handoff says `"action": "fix"`; an `answer`, `escalate` or `wait_for_review` handoff (or none) is a vacuous pass, so the agent is never re-run on an issue it already answered, escalated or left waiting. The runner deletes the handoff file before every attempt, so a retry that crashes can never be finalized with an earlier attempt's handoff. Of step 10 the agent still annotates `loop_last_action` (`fix_handed_off`), marks the notes seen and records a learning; the runner sends the "Finished" Slack message (to the project's Slack bundle webhook from `~/.gitlab/config.json`, like the agent's own messages, with links to the issue and the MR) and emits the `issue.completed` fix event after opening the MR. Handoff actions: `fix`, `answer`, `escalate`, `wait_for_review` (Wait for reviewer, or nothing to act on this run). An agent that writes no handoff at all - even one that deliberately skipped the issue - is escalated as `handoff_invalid`, so a silently skipped issue is always surfaced to a human.

Gate-mode outcomes: `mr_opened` (an `issue.completed` event with `gated: true`), `answered` / `waiting_for_review` / `escalated:agent` (the agent's own handoff action), or `escalated:<reason>` with reason `verification_failed` (worded by the runtime's stop reason, e.g. the same failure on consecutive attempts; also used when a retry crashed or hit the budget after an earlier attempt's checks failed, quoting that attempt's failure tail), `handoff_invalid` (handoff missing or malformed - never an MR), `run_incomplete` (the agent session crashed/timed out or the budget ran out before any attempt was verified; `data.stop_reason` says which), `mr_open_failed`, or `project_config_error`. Every `escalated:<reason>` outcome except `project_config_error` posts a GitLab comment and adds the `loop:needs-human` label (`escalated:agent` is the agent's own escalation, commented by the agent itself); the unpushed `loop/issue-<iid>` branch stays in its worktree for a human to inspect, and exactly one Slack notification is sent (the end-of-batch "did not complete" alert skips issues the gate already escalated). `project_config_error` has no project to address, so it only logs, emits the `issue.escalated` event and notifies. An `escalated:<reason>` on a run the runtime had marked completed is written to `result.json` as `final_state: escalated` with `stop_reason: gate:<reason>`, so Loop Runs shows it as an escalation.

The Harness -> Gates view (`/harness?view=gates`) shows the observe-mode agreement rate (of the issues the agent completed with `action: fix`, the share the external check also passed; answers, waits and escalations are left out, since a verdict on them usually comes from a stale worktree; verifier config errors excluded) and what the gate did (blocked, retried then passed, escalated), per project, plus each loop's current mode. CI audits every `loops/*/loop.yaml` with `loopx loop audit --min-score` (see `.github/workflows/rust.yml`).

### Rollout checklist

1. Ship with `mode: observe`.
2. After at least 5 weekday runs, open Harness -> Gates. If agreement is 90% or higher and no project shows a systematic false failure (flaky suite, lint baseline - see CLAUDE.md's rubocop notes), set `mode: gate` and raise CI `--min-score` to 80 in the same commit.
3. If a project is systematically red for reasons unrelated to the fix, fix its `test_cmd`/`lint_cmd` in `projects.json` rather than weakening the gate.
4. Gate mode multiplies the per-issue worst case to `max_iterations x (agent + test + lint)`, up to 6x `max_runtime_minutes`. `config/loops.json.template` now sets `gitlab-loop`'s `timeout_seconds` to 43200, but that only applies to new installs/backfill: an existing `~/.loopx/loops.json` keeps its old 21600. The dashboard does not expose this field, so raise it by hand in that file before flipping to gate.

### Limitations

- Gate enforcement on Codex is prompt-only: `codex exec` has no per-command allowlist, so "do not push / do not open the MR" is an instruction, not a harness-enforced denial as it is on Claude.
- A `project_config_error` escalation cannot comment or label (see above).
- `loopx gitlab*` stays on the agent's allowlist in gate mode (it needs it to read issues, notes and MRs). Its MR-creating calls are not blocked, but they cannot produce a usable MR in gate mode: the agent cannot push, so the `loop/issue-<iid>` branch does not exist on the remote.

# Architecture

This is the consolidated V2 architecture reference for this repo — the
pieces described in `~/Downloads/LOOP-ENGINEERING-V2-TECH-PLAN.md` and how
they actually landed in code, in one place. The ~30 design docs under
`docs/superpowers/specs/` remain the source of record for *why* each piece
was built the way it was (constraints, rejected alternatives, review
findings); this file is the map, not a replacement for them — each section
below links to its own spec rather than re-explaining it.

Read [`README.md`](../README.md) first for what this project *does*
(schedule, dashboard, safety boundaries). Read
[`instructions/loopx/gitlab-issue.md`](../instructions/loopx/gitlab-issue.md) for the GitLab issue
loop's own step-by-step spec — this file is about the runtime underneath
both scheduled loops, not either loop's own decision logic.

## 1. Vision

Loop Engineering is not another agent framework. It's the layer around
one: it doesn't decide *how* an agent solves a task, it decides how that
agent's work is triggered, bounded, verified, stopped, retried, escalated,
and measured.

```text
Trigger → Goal → Context → Agent → Actions → Verification → Evaluation
                                                                  │
                                              ┌───────────────────┼───────────────────┐
                                              ↓                   ↓                   ↓
                                           SUCCESS              RETRY             ESCALATE
                                              ↓                   ↓                   ↓
                                          COMPLETE          next iteration          HUMAN

                                   Memory + Metrics accumulate alongside every run
```

## 2. Ledger

The run history is the **ledger** (`crates/core/src/ledger.rs`). `ledger.iter_runs`
reads only the append-only event log (`outputs/events/*.jsonl`, written by
`crates/core/src/events.rs`) and builds one `RunRecord` per run from its `loop.result`
event (last one wins). It does not open `result.json`; that per-run file
(`outputs/loop-runs/<run_id>/result.json`, written by `crates/core/src/loop_serialize.rs`)
reaches the ledger only through the `loop.result` event `write_result` emits,
or through backfill. A run with a legacy terminal event (`loop.completed`/
`loop.failed`/`loop.stopped`) but no `loop.result` is a complete record with
`has_result` False; a run with only `loop.started` is reported as incomplete.
`iter_runs(days=N)` is a rolling window on event timestamps;
`iter_runs(since_date=..., until_date=...)` is the calendar-day window
(UTC dates, inclusive) the Analytics page uses for the Health score, matching
metrics/cost/learning.

- **The `loop.result` event** — `loop_serialize.write_result` emits it when a
  run's status is `finished`, into the caller's `events_dir`. Its payload is
  small (at most 8 KB: ids, states, counts, cost, duration, per-iteration
  `{state, passed, cost_usd}`; no verifier output, no prompts), so the event
  log alone is enough to rebuild a run's outcome. `RunRecord.has_result` is
  True only for records built from a `loop.result` event; retry rate and cost
  efficiency count only those runs.
- **Cost** — `total_cost_usd` is the sum of what agent calls reported,
  including a failed call's spend (e.g. a `--max-budget-usd` stop, carried as
  `exc.cost_usd`). It is `None` (unknown), never `0`, when no call reported a
  cost: every Codex run (topic-monitor runs before 2026-10-07 also have no
  cost figure; that loop now runs the CLI with JSON output). Health's cost efficiency
  leaves `None`-cost runs out of the numerator and the denominator and counts
  only `gitlab-issue-loop` runs; the `summarize_*`/Budget rollups add `None`
  as `0`, as before. Backfilled legacy runs cannot tell unknown from `$0`: a
  recorded `$0` on a run with at least one iteration is booked as `None`
  (no run that called a model really costs `$0`); any other figure is kept.
- **Backfill** — `loop ledger backfill [--results-dir DIR] [--events-dir DIR]`
  appends a `loop.result` for each pre-existing finished `result.json` that has
  none. It is idempotent (keyed by `run_id`), skips a corrupt `result.json`
  rather than aborting, and never rewrites existing JSONL: it writes only
  `outputs/events/backfill-loop-result.jsonl`. It runs automatically,
  best-effort, once per process start of the dashboard and of the scheduler
  (`ledger.run_startup_backfill`), as well as from `install-loopx.sh --upgrade`.
- **Readers** — `health.rs` (its retry-rate and cost-efficiency components)
  and `budget.rs`/`loop_serialize`'s `summarize_*` (Runs → Loop Runs,
  Insights → Budget, Harness → Audit) read through the ledger. `metrics.rs`,
  `cost.rs`, `learning.rs` and `risk.rs` (Insights Analytics/Cost/Memory, and
  Health's other five components) read the event log directly with
  `events.iter_events`, not through `iter_runs`. `LOOP_EVENTS_DIR` overrides
  the events directory (`events.default_events_dir()`), e.g. for tests and
  golden sandboxes.

The GitLab issue loop still emits its own domain events
(`issue.started`/`issue.completed`/`verification.*`/`memory.*`) into the same
log, and the topic monitor loop emits `loop.result` only.

### Golden evals

`loop eval` (scripted agent, §8) tests the runtime; `loop eval --golden`
tests the real agent. `crates/core/src/golden_eval.rs` builds a synthetic fixture repo per
case from `evals/golden/<case>/case.yaml` (generated repos and invented issue
text only, never real issue content), runs the issue-loop path against it in a
sandboxed `LOOP_ENGINEERING_HOME`/events dir, and checks the project's checks
afterwards. It is paid, so it requires a budget: `--budget-usd N` (default 10);
each agent call is capped by `--max-budget-usd` set to what is left of the
suite budget (minus what earlier attempts of the same case spent), not by the
case's own `stop_conditions`; no new case starts once spend reaches the budget
(unstarted cases are listed as `not_run`), and a case that reports no cost is
booked at the remaining budget. `--case NAME` selects cases.
Results go to `outputs/evals/golden-last.json`; the scripted run writes
`outputs/evals/last.json`. Both show on **Harness → Evals**.

## 3. Module map

Every runtime module under `crates/core/src/` (the CLI is in `crates/cli/src/`,
the dashboard in `crates/server/src/`) — one line each, pulled from its own
module docs so this stays honest as the code changes. Flat modules,
not the plan's original `bin/runtime/`, `bin/verifiers/`, etc.
subdirectories — see §9. Bare file names in the table below are in
`crates/core/src/`.

| Module | Responsibility |
|---|---|
| `loop_definition.rs` | `LoopDefinition` structs + YAML loader/validator |
| `loop_state.rs` | `LoopState` enum + its explicit transition table (raises on an invalid transition, not silent) |
| `loop_runtime.rs` | `LoopRuntime` — the orchestrator; owns iteration, budget, retry, policy, events |
| `loop_verifiers.rs` | `Verifier` interface + `CommandVerifier`/`DiffVerifier` |
| `budget.rs` | `BudgetController` (per-iteration OK/WARNING/EXCEEDED) + the Budget dashboard's rollup functions |
| `policy.rs` | `RiskLevel` (L0–L3) classification + `PolicyEngine` — which *action types* need a human gate |
| `loop_progress.rs` | `ProgressDetector` — same failing-verifier signature twice in a row? |
| `risk.rs` | Deterministic keyword risk scorer for *issue text* (advisory only) — distinct from `policy.rs`'s action-type risk |
| `loop_serialize.rs` | `LoopResult` JSON persistence (`result.json`) + `summarize_results()` |
| `loop_audit.rs` | `loop audit` — scores a `LoopDefinition`'s readiness (PASS/WARN/FAIL per check + weighted score) |
| `loop_eval.rs` | Evaluation dataset harness — drives a real `LoopRuntime` with a scripted agent/verifiers |
| `loop_cmd.rs` | The `loop` CLI: `init`/`validate`/`run`/`status`/`inspect`/`audit`/`cost`/`doctor`/`replay`/`eval` |
| `crates/core/src/agents/mod.rs`, `crates/core/src/agents/claude.rs`, `crates/core/src/agents/codex.rs` | `Agent` trait + `AgentResult` + provider adapters |
| `events.rs` | Append-only JSONL event log (read through the ledger, §2) |
| `metrics.rs`, `cost.rs`, `health.rs`, `learning.rs` | Pure report builders over the event log (read through the ledger, §2) |
| `gitlab_runner.rs` | Wires the real GitLab issue loop through `LoopRuntime`, one issue at a time |
| `topic_runner.rs` | Wires the real Topic Monitor loop through `LoopRuntime`, one topic at a time |
| `loop_scheduler.rs` | The single launchd-scheduled poll loop: reads `loops_config.list_loops()` and runs whichever registered loop(s) are due, via `loopx run` |
| `loops_config.rs` | Loads the loops registry (`~/.loopx/loops.json`) — name, schedule, entry point, and per-loop knobs for `loopx run`/`loop_scheduler.rs` |

## 4. `LoopDefinition`

A `LoopDefinition` (`crates/core/src/loop_definition.rs`) is loaded from a `loop.yaml`
(see `loops/gitlab-issue/loop.yaml`, `loops/topic-monitor/loop.yaml`, or
any of the 8 generic templates under `templates/`):

```text
LoopDefinition
├── trigger: {type, schedule}
├── goal: {type}
├── agent: {provider, model}
├── context: {sources: [...]}
├── actions: [...]
├── verification: {required: [...]}
├── stop_conditions: {max_iterations, max_runtime_minutes, max_cost_usd, no_progress_iterations}
├── human_gates: [...]
├── retry: {enabled, max_attempts}
├── verifiers: [...]                    # loop_verifiers.build_verifiers() input
├── permissions: {credentials_read}
└── observability_enabled: bool
```

Every field has a validated default via `_STOP_CONDITION_DEFAULTS`/
`_RETRY_DEFAULTS` — there is no such thing as an unbounded loop by
omission. See
[`2026-09-06-loop-runtime-foundation-design.md`](superpowers/specs/2026-09-06-loop-runtime-foundation-design.md).

### Risk levels (`policy.rs`)

Every *action type* a loop can take (not the loop definition's own
config — the actual actions like `modify_code`, `create_merge_request`)
gets classified:

| Level | Meaning | Example actions |
|---|---|---|
| L0 | Read-only | `inspect_issue`, `read_issue` |
| L1 | Local mutation | `modify_code`, `run_tests` |
| L2 | External change | `create_merge_request`, `post_public_comment` |
| L3 | Irreversible | `merge`, `production_deploy` |

An action not in the table defaults to **L3** — fail-safe, not
fail-open. `PolicyEngine` refuses any L3 action whose type isn't
explicitly listed in the definition's `human_gates`. See
[`2026-09-07-policy-engine-design.md`](superpowers/specs/2026-09-07-policy-engine-design.md).

## 5. `LoopState`

```text
PENDING → DISCOVERING → PLANNING → EXECUTING → VERIFYING → EVALUATING
                                        ↑                        │
                                        └────── RETRY ───────────┤
                                                                  ├→ COMPLETED
                                                                  ├→ BLOCKED → ESCALATED
                                                                  ├→ ESCALATED
                                                                  ├→ STOPPED
                                                                  └→ FAILED
```

`transition(current, target)` raises `InvalidTransition` for any pair not
in `VALID_TRANSITIONS` — "every loop must have explicit exits" (plan
§4.3) is enforced by this table, not left as a convention `LoopRuntime`
is merely trusted to follow.

## 6. `LoopRuntime`

The actual public surface is smaller than the plan's original mockup
(`start`/`execute`/`transition`/`stop`) — it collapsed to one entry point:

```text
# sketch of the surface — see `loop_runtime.rs` for the exact Rust signatures
runtime = LoopRuntime(agent_fn, verifiers, progress_detector=None,
                       policy_engine=None, events_dir=None, on_iteration=None)
result = runtime.start(definition, run_id, loop_id=None)  # -> LoopResult
```

`agent_fn` and `verifiers` are injected, not imported — `loop_runtime.rs`
itself has no GitLab/Claude/Codex/Slack dependency. Each iteration:
executes the agent, runs every configured verifier, checks budget
(`BudgetController` in `budget.rs`), checks progress
(`ProgressDetector` in `loop_progress.rs`), checks policy
(`PolicyEngine` in `policy.rs`) for any action needing a human gate, and
either transitions to `COMPLETED`, retries (`EXECUTING` again, subject to
`retry.max_attempts`), or exits to `BLOCKED`/`ESCALATED`/`STOPPED`/`FAILED`.
Every step emits through `crates/core/src/events.rs`, best-effort (an event-log write
failure never crashes the runtime — same `|| true` philosophy as
`loopx run`). See
[`2026-09-06-loop-runtime-foundation-design.md`](superpowers/specs/2026-09-06-loop-runtime-foundation-design.md).

## 7. Observability

- **Events** — `outputs/events/<UTC date>.jsonl`, one JSON object per
  line: `{event_id, run_id, iteration, type, timestamp, data}`. See §2 for
  how the ledger joins the event log with persisted results.
- **Metrics** (`metrics.rs`) — issue/verification/classification/failure-
  taxonomy/quality-and-autonomy metrics computed from the event log.
- **Cost** (`cost.rs`) — Claude-only usage/cost extraction (`claude -p
  --output-format json`'s `total_cost_usd`); Codex cost is still
  unverified and reported as `None` (its success-path usage schema has not
  been confirmed against a real sample). Failed budget stops are booked at
  the per-call cap (`--max-budget-usd`, from `stop_conditions.max_cost_usd`);
  other unknown costs stay `None`.
- **Health** (`health.rs`) — a 7-component Loop Health score (weights:
  resolution 30, autonomy 25, verification 15, cost efficiency 10, retry rate
  10, escalation 5, learning effectiveness 5). Retry rate and cost efficiency
  come from the ledger's per-run records; learning effectiveness from memory
  reuse outcomes. A component with no data yet is reported as such rather
  than guessed, and the score is flagged partial in that case.
- **Audit** (`loop_audit.rs`) — `loop audit <loop.yaml>... [--min-score N]` scores a
  `LoopDefinition`'s readiness: goal/trigger/verification/stop_conditions/
  retry/budget/no_progress_detection/human_gates/context_strategy/
  memory_strategy/credential_boundary/observability, each PASS/WARN/FAIL,
  weighted into a 0–100 score. Same honest-degradation pattern as
  `health.rs`.
- **Loop Efficiency Score** (`loop_serialize.summarize_results`) — the
  plan's experimental §17 metric: verified-successful runs / (total
  cost_usd × total duration_hours × total iterations), summed across all
  persisted `LoopRuntime` runs. Shown as one tile among several on the
  Runs → Loop Runs view, deliberately not a standalone KPI.
- **External GitLab issue verification** (`loop_verifiers.ProjectCommandsVerifier`,
  run inside `LoopRuntime` by `gitlab_loop_runner._run_one_issue`)
  — independently re-runs a project's real `test_cmd`/`lint_cmd` against
  the worktree an issue's agent call actually used. In `verification.mode:
  observe` (the default) the result is only recorded (`observed_passed`);
  in `gate` mode a failure fails the iteration and the runner retries with
  the failing output as feedback (`format_feedback`); the runner then opens the
  MR itself from the agent's handoff file only if the checks passed (see
  [`docs/tasks/gitlab-issue-loop.md`](tasks/gitlab-issue-loop.md) for the gate
  outcomes and rollout). The Harness -> Gates view reports the observe-mode
  agreement rate. See
  [`2026-09-13-gitlab-issue-external-verification-design.md`](superpowers/specs/2026-09-13-gitlab-issue-external-verification-design.md).

## 8. CLI (`crates/cli/src/loop_cmd.rs`)

`loopx loop <command>` (subcommand dispatch in `crates/cli/src/loop_cmd.rs`):

| Command | Does |
|---|---|
| `init --template <name>` | Scaffold `.loop/loop.yaml` from one of the 10 shipped templates |
| `validate <loop.yaml>` | Check trigger/goal/verifiers/budget/human-gates are configured |
| `run <loop.yaml>` | Real `LoopRuntime.start()` invocation, via `--prompt`/`--prompt-file` (no-op agent otherwise — L0/observe, plan §32) |
| `status <run_id>` / `inspect <run_id>` | Read back a persisted `result.json` |
| `audit <loop.yaml>` | See §7 |
| `cost` | Cost report across persisted runs |
| `doctor` | Top issues, human-readable |
| `replay <run_id>` | Re-invoke a real agent using the prompt/definition path recorded by the original `run` |
| `eval [cases_dir]` | Run the evaluation harness (§ below) against `evals/cases/*.yaml` |

See [`2026-09-07-loop-cli-design.md`](superpowers/specs/2026-09-07-loop-cli-design.md).

### Evaluation harness (`loop_eval.rs`, `evals/cases/`)

Drives a real, unmodified `LoopRuntime` with a `ScriptedAgent` and
`ScriptedVerifiers`, so a passing case is evidence about the runtime's
stop/verify/escalate/cost behavior, not about the harness's own logic.
Six shipped cases: `success`, `retry`, `no-progress`, `budget`,
`unsafe-action`, `ambiguous-task` — testing whether the *loop* stops,
verifies, and escalates correctly, not whether an agent can write code
(plan §29). See
[`2026-09-09-loop-eval-harness-design.md`](superpowers/specs/2026-09-09-loop-eval-harness-design.md).

## 9. How the two production loops actually plug in

```text
loop_scheduler.rs (launchd, StartInterval poll)     dashboard "Run now" / chat tool
              │                                              │
              └──────────────────┬───────────────────────────┘
                                  ↓
                    loopx run <loop_name>
                 (entry point/timeout/etc. looked up
                  from loops.json via loops_config.rs)
                                  │
                  ┌───────────────┴───────────────┐
                  ↓                                ↓
        gitlab_runner.rs              topic_runner.rs
         (one issue at a time)               (one topic at a time)
                  │                                ↓
                  └───────────────┬────────────────┘
                                  ↓
LoopRuntime(agent_fn, verifiers).start(definition, run_id=issue_run_id)
```

Each issue/topic gets its own `LoopRuntime.start()` call — the runtime
tracks and bounds that single invocation, it does not span the whole
scheduled batch. `loopx run` is the one CLI entry point both loops
go through (replacing the old separate `run-loop.sh`/
`run-topic-monitor-loop.sh`); `crates/core/src/loop_scheduler.rs` is the actual
`launchd`-scheduled trigger now — a single `com.hermes.loop-engineering`
job on a fixed `StartInterval` poll, replacing the old per-loop
`StartCalendarInterval` plists, which invokes `loopx run <loop_name>`
for whichever registered loop(s) are due. The dashboard's Run now button
and its chat tool's "paste an issue link" flow invoke `loopx run`
directly, on demand, bypassing the scheduler. Nothing here requires
`loop run gitlab-issue` as the plan originally imagined (plan §25/§26's
"final state" was never reached — the shell script remains the real
trigger, calling into `LoopRuntime` one level down rather than being
replaced by it).

## 10. Deliberately not (yet) built

Documented gaps, not oversights:

- **No polymorphic `Trigger` interface** (plan §8's `bin/triggers/*.py`).
  Schedule is `launchd`, manual is the dashboard's "Run now" button or a
  pasted GitLab issue link, and neither goes through a shared `Trigger.fire()`
  abstraction.
- **Audit threshold is a floor, not a target** — CI blocks
  (`.github/workflows/ci.yml`) when any shipped `loops/*/loop.yaml` audits
  below `--min-score 70`. The threshold stays at 70 while `gitlab-issue`
  runs `verification.mode: observe` (its `project_commands` verification
  check WARNs, "recorded but not enforced"); raise it to 80 in the commit
  that flips that loop to `gate`.
- **The Loop Efficiency Score is unvalidated** — no production data has
  been run through it yet to know if the formula in §7 is actually a
  useful signal over time.

## Connectors

Connectors are the accounts loops talk to (GitLab, GitHub, Slack/chat
webhooks, RSS, Jira, Linear, mailboxes). Each connector *type* declares
capabilities (`issues`, `merge_requests`, `pipelines`, `notify`, `feed`,
`mail`); loops declare `requires: [capabilities]` rather than a product.

| Module | Responsibility |
|---|---|
| `crates/core/src/connectors/mod.rs` | `CONNECTOR_TYPES` registry + `get_type(name)` |
| `crates/core/src/connectors/mod.rs` | `Field`, `Connector` base class, capability constants, `ConnectorError` |
| `connectors/{gitlab,github,slack,webhook,rss,jira,linear,mailbox}.rs` | One module per type: its settings fields, capabilities, and `test()` (plus `send()` for `notify` types) |
| `connectors_config.rs` | The account registry: `list_accounts`/`get_account`/`accounts_with_capability`, `upsert_account`/`delete_account`, `load_connector` |
| `secrets.rs` | macOS Keychain wrapper for connector secrets (service `loop-engineering.connectors`, sandbox-suffixed under `LOOP_ENGINEERING_HOME`), built on `mail_auth`'s Keychain calls |
| `loop_notify.rs` | Per-loop notification routing (below) |

**Native vs external accounts.** *Native* accounts are the ones added on the
dashboard's `/connectors` page: non-secret settings live in
`~/.loopx/connectors.json`, secrets only in the Keychain. *External*
accounts are read through, never copied or migrated, from the files that
already own them: `~/.gitlab/config.json` (GitLab instances, id = alias),
`~/.slack/config.json` (`slack-default`, `slack-<bundle>`), and `inboxes.json`
(mailboxes, id = inbox name). They carry a `managed_by` marker so the page can
show a "Managed on …" link instead of edit/delete controls. Last Test results
are recorded in `outputs/connectors/test-results.json`.

**Notify routing.** `loops_config` accepts an optional `notify: [connector ids]`
per loop (written by `set_notify`, and kept in step by `replace_notify_id` when a
connector is deleted or renamed). `notify.notify(loop_name, text)`
sends to each listed connector that has the `notify` capability and returns one
`(connector_id, ok, message)` per target without raising; a loop with no
`notify` goes to the default Slack webhook via `slack.rs`, as before.
`loopx notify-loop <loop> <text>` is the CLI form. Routing applies only to
loops whose registry entry declares `"routes_notifications": true` (their runner
calls `notify`); only those get the dashboard's "Notify via" control. The
built-in GitLab, Topic and Inbox loops don't declare it and still post to the
Slack webhook directly; an existing `notify` list on such a loop is shown
read-only with a Clear button. In the dashboard, the
Loops catalog blocks enabling a loop (UI and server) until a connector with each
capability in its `requires` exists, and the AI panel exposes a read-only
`connector-list` chat tool.

## LoopKit

`crates/core/src/loopkit.rs` is a small plugin runner for "discover items, ask the model
about each, act on the answer" loops. The seven plugin loops (Daily Digest, MR
Review, Pipeline Doctor, RSS Watch, Meeting Prep, Release Notes, Stale Work
Sweeper) are modules under `crates/core/src/loop_plugins/`; each
one's `loops.json` entry points its `entry_point` at that module, whose
`__main__` block hands a `LoopPlugin` subclass instance to `loopkit.main`. Each plugin also has
a loop definition and prompt under `loops/<definition_dir>/` (`loop.yaml`,
`prompt.md`) and a spec under `docs/tasks/`.

Design notes for the next batch of plugin loops: [`docs/loops-wave2-notes.md`](loops-wave2-notes.md).

**Plugin contract.** A `LoopPlugin` sets `loop_name`, `definition_dir`,
`max_items_per_run`, `output_keys` (the JSON keys the answer must contain) and
optionally `settings_fields` (connector `Field`s the dashboard renders as the
loop's Settings tab; values land in the entry's `settings`). It implements
`discover(ctx)` returning `WorkItem`s (`key`, `title`, `url`, `payload`) and
`after_item(item, answer, ctx)` returning an `Outcome` (`done`, `skipped` or
`failed`); `digest(outcomes, ctx)` optionally returns the run's notification
text. `build_prompt` fills `{{item_json}}`/`{{settings_json}}` into the
definition's `prompt.md`, and `call_model` defaults to the sealed call.

**Run algorithm (`run_plugin`).**

1. Take a per-loop exclusive `flock` (`outputs/loops/<name>/run.lock`); a
   second concurrent run returns immediately. The kernel drops the lock when
   the process exits, so a killed run never leaves it stuck.
2. Load the `LoopDefinition` and the loop's `settings`, then call `discover`.
   A discovery crash notifies `<loop> FAILED during discovery` and re-raises.
3. Drop items already in the seen store (unless `--force`) and cap the list at
   `max_items_per_run`.
4. Run each item through `LoopRuntime` with an output-contract verifier (plus
   any verifiers in the definition): an answer that is not valid JSON with the
   required keys is retried with the violation fed back. Only a COMPLETED run
   reaches `after_item`.
5. Write a history markdown file and `last-run.json`, call `digest`, notify,
   and send a second notification if any item failed.

**Sealed model calls.** `crates/core/src/agents/sealed.rs` is the one hardened Claude call
every plugin uses on untrusted content: no tools, no MCP servers, prompt on
stdin, no session persistence, hooks disabled. It is Claude-only; codex is
refused because it always gives the model a shell and records the prompt.

**`chat_*` helpers.** `chat_text`, `chat_url` and `chat_link` sanitize
untrusted text before it goes into any chat message (control characters
stripped, whitespace collapsed, `<`/`>` replaced so no Slack control sequence
can form, only plain http(s) URLs kept, links rendered as `label (url)`).

**Seen store and persistence.** `crates/core/src/seen_store.rs` keeps
`outputs/loops/<name>/seen.json` (`{key: iso timestamp}`, entries older than 30
days dropped on save). An item is marked seen only when its outcome is `done`
or `skipped`, and the store is saved right after each such item, so a crash or
SIGTERM mid-run never re-runs work already acted on, while `failed` items
retry next run.

**Per-item isolation.** A model failure, verification failure, `after_item`
exception or any other crash in one item becomes a `failed` outcome (with the
exception class and message in the summary) and the loop moves on to the next
item.

**`routes_notifications` and `notify`.** The four plugin entries declare
`"routes_notifications": true` and a `requires` capability. `run_plugin` sends
through `notify.notify(loop_name, text)`, so the dashboard's **Notify via**
selection (`notify: [connector ids]`) decides where digests and failure notices
go, defaulting to the Slack webhook when unset.

## Where to look next

Each module above links to its own design spec above; browse all of them
under `docs/superpowers/specs/` for full rationale, rejected alternatives,
and review findings. `docs/tasks/gitlab-issue-loop.md` and
`docs/tasks/topic-monitor-loop.md` are the two loops' own behavioral
specs — not covered here, since this file is about the runtime
underneath them.

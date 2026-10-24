# CLI and script contract (Python baseline for the Rust `loopx` migration)

Recorded from the source at branch `rust-migration` (base `e7b3117`). This is the observable
contract a Rust port must preserve byte-for-byte where agents or shell scripts parse it.
Every sample below was produced by running the script against a scratch
`LOOP_ENGINEERING_HOME`/`HOME`; nothing touched the real `~/.loopx`.

## 0. Renames and things that no longer exist

| Name in the migration table | Status |
|---|---|
| `run-loop.sh` | Removed. Replaced by `run-loop-now.sh <loop_name> [extra_args...]` (repo root). `launchd/run-loop-now.sh` is not a separate file; only the root copy exists. |
| `run-topic-monitor-loop.sh` | Removed. Same replacement: `run-loop-now.sh topic-loop`. |
| `list_assigned_issues.py`, `track_new_comments.py` | Still exist, but the runner (`bin/gitlab_loop_runner.py`) no longer shells out to them for the main path; they remain agent-callable helpers (see allowlist in `_allowed_tools()`). |
| `build_topic_prompt.sh` | Exists, but "nothing calls it any more" (its own header); manual escape hatch only. |
| `launchd/*.plist.template` | Two templates: `com.hermes.loop-engineering.plist.template` (scheduler) and `com.hermes.loop-engineering-dashboard.plist.template`. No per-loop plists. |

Conventions common to the Python helpers (none use argparse except where noted):
hand-rolled `sys.argv` dispatch; usage text goes to stderr with exit 1; success output goes to stdout
with exit 0; unhandled exceptions (missing config file, unknown project alias) produce a Python
traceback on stderr and exit 1 (an unhandled `KeyError` or `FileNotFoundError`). A Rust port should
exit 1 with a stderr message; the traceback text itself is not contract.

Path resolution: `LOOP_ENGINEERING_HOME` (default `~/.loopx`) is read **at import time**
into module constants in `loop_config.py`, `topic_config.py`, `ai_cli_config.py`, `memory_store.py`,
`loops_config.py`, `connectors_config.py`, `loop_scheduler.py`.

---

## 1. Priority CLIs (full detail)

### 1.1 `bin/loop_config.py`

Purpose: read `projects.json`, the per-machine project registry. Called by agents, `run-loop`-era
shell, `build_run_prompt.sh` text ("Look up its config via `loop_config.py project <alias>`"), and the dashboard.

Invocation: `loop_config.py <command> [arg]`, argv-positional, no flags.

| Command | stdout | exit |
|---|---|---|
| `aliases` | project aliases, one per line, in JSON insertion order | 0 |
| `local-paths` | each project's `local_path`, one per line, same order | 0 |
| `worktree-root` | `worktree_root` string | 0 |
| `assignee` | `assignee_username` string | 0 |
| `instance` | top-level `gitlab_instance` | 0 |
| `project <alias>` | the project object, `json.dumps(indent=2)`, with `instance` always injected (project's own `instance`, else top-level `gitlab_instance`; an own `instance` is kept as-is) | 0 |
| (no args) | usage (stderr): `Usage: loop_config.py <aliases\|local-paths\|worktree-root\|assignee\|instance>` newline `       loop_config.py project <alias>` | 1 |
| unknown command | `Unknown command: <cmd>` (stderr) | 1 |
| `project` without alias | `Usage: loop_config.py project <alias>` (stderr) | 1 |
| missing config / unknown alias | traceback, `FileNotFoundError: No project config at <path>. Copy config/projects.json.template there ...` or `KeyError: '<alias>'` | 1 |

Config shape: `{"gitlab_instance", "assignee_username", "worktree_root", "projects": {alias: {"local_path", "default_branch", ... optional "instance"}}}` (template: `config/projects.json.template`).

Sample (scratch config):

```
$ loop_config.py aliases
web
api
$ loop_config.py project web
{
  "local_path": "/src/web",
  "default_branch": "main",
  "instance": "gitlab.example.com"
}
$ loop_config.py project api        # own instance wins
{
  "local_path": "/src/api",
  "instance": "other"
}
```

Env: `LOOP_ENGINEERING_HOME`. Files read: `$LOOP_ENGINEERING_HOME/projects.json`. Writes: none.
Note: JSON key order is preserved (no sorting); `indent=2`, no trailing newline difference beyond `print`'s `\n`.
Tests: `tests/test_loop_config.py`.

### 1.2 `bin/list_assigned_issues.py`

Purpose: list open GitLab issues assigned to a user, grouped by project alias.

Invocation:
- `list_assigned_issues.py` (no args): username from `loop_config.get_assignee_username()`, aliases from `loop_config.list_aliases()`.
- `list_assigned_issues.py <username> <alias> [alias...]` (needs at least 3 argv entries).
- Any other arity (exactly 1 or 2 args) prints `Usage: list_assigned_issues.py [username] [project_alias...] (with no arguments, both default from ~/.loopx/projects.json)` to stderr, exit 1.

Behavior: for each alias runs `python3 ~/.encore-skills/skills/gitlab-config/scripts/gitlab_api.py list-issues <alias> opened` (`subprocess.run(check=True)`, stdout parsed as a JSON array), keeps issues whose `assignees[].username == username`.
stdout: `json.dumps(result, indent=2)`, shape `{"<alias>": [<full GitLab issue object>, ...], ...}` (aliases in input order; empty list when none). A failing `gitlab_api.py` raises `CalledProcessError` (traceback, exit 1).

Sample:

```
{
  "web": [
    {"id": 1, "iid": 42, "title": "...", "assignees": [{"username": "alice"}], "...": "..."}
  ],
  "api": []
}
```
(actual output is pretty-printed with indent=2 at every level)

External dependency: the `gitlab-config` skill's `gitlab_api.py` (path hard-coded, `GITLAB_API`; tests monkeypatch `fetch_open_issues`). Env: `LOOP_ENGINEERING_HOME` (indirectly, via loop_config); `HOME`. Files: reads projects.json, GitLab network via gitlab_api.py. Tests: `tests/test_list_assigned_issues.py`.

### 1.3 `bin/track_new_comments.py`

Purpose: watermark-based "which notes on this issue are new since the loop last looked", stored as an annotation in the gitlab-config skill's local cache.

Invocation:
- `track_new_comments.py <instance> <project_id> <issue_iid> [exclude_author]`: print new notes.
- `track_new_comments.py mark-seen <instance> <project_id> <issue_iid>`: advance watermark.
- Fewer than 3 positional args (`len(argv) < 4`) prints two-line usage to stderr, exit 1; `mark-seen` with fewer than 3 args prints `Usage: track_new_comments.py mark-seen <instance> <project_id> <issue_iid>`, exit 1. Note the `mark-seen` check happens first on `argv[1]`, so an instance literally named `mark-seen` is unreachable.

Semantics:
- Read: cache = `gitlab_cache.get_issue_cache(instance, project_id, iid)`; no cache gives `[]`. `last_seen = int(cache["_notes"]["loop_last_seen_note_id"] or 0)`. New notes = `notes` with `int(id) > last_seen`; if `exclude_author`, drop notes whose `author.username == exclude_author`. stdout `json.dumps(new_notes, indent=2)` (full note objects, original key order). Empty result prints `[]`.
- `mark-seen`: takes **all** unseen notes (no author filter, so the loop's own notes still consume ids), writes `loop_last_seen_note_id = str(max_id)` via `gitlab_cache.annotate_issue`, prints `OK`. No-op (still prints `OK`) when nothing is new.

Files: reads/writes `~/.gitlab/cache/<instance>/projects/<project>/issues/<iid>.json` (owned by gitlab_cache.py; `CACHE_ROOT = ~/.gitlab/cache`). Hard dependency: `import gitlab_cache` from `~/.encore-skills/skills/gitlab-config/scripts` (ImportError at import time if missing; unlike project_memory.py there is no fallback). Env: `HOME`. Tests: `tests/test_track_new_comments.py` (monkeypatches `gitlab_cache.CACHE_ROOT`).

### 1.4 `bin/project_memory.py`

Purpose: durable cross-run "fix learnings" per GitLab project, stored in the project cache entry under `_memory.fix_learnings`.

Invocation:
- `project_memory.py get <instance> <project_id>`: stdout `json.dumps(list, indent=2)`; list of `{"lesson": str, "issue_iid"?: int, "tags"?: [str]}`, oldest first; `[]` if none or the skill is not installed.
- `project_memory.py add <instance> <project_id> <lesson> [issue_iid] [tags_csv]`: appends one entry; `issue_iid` is `int(...)` when non-empty (non-integer raises, exit 1), `tags` is `split(",")` when non-empty (no trimming); prints `OK`. When the gitlab-config skill is missing: `RuntimeError("gitlab-config skill not installed (no gitlab_cache in <path>)")`, traceback, exit 1.
- no args: two-line usage on stderr, exit 1. Unknown command: `Unknown command: <cmd>`, exit 1. Missing positional args for get/add raise `IndexError` (traceback, exit 1).

Sample: `[ { "lesson": "Run migrations before specs", "issue_iid": 42, "tags": ["rails","db"] } ]` (indent=2 formatting).

Files: `~/.gitlab/cache/<instance>/projects/<project>/project.json` (via `gitlab_cache.annotate_project`). Env: `HOME`. Tests: `tests/test_project_memory.py`. Related, newer: `bin/memory_store.py` (see section 2), keyed by alias and under `LOOP_ENGINEERING_HOME`.

### 1.5 `bin/topic_config.py`

Purpose: topic registry (`topics.json`, a JSON **array**) for the topic monitor loop.

CLI (only two commands; the mutators below are library-only, used by the dashboard):
- `topic_config.py names`: topic names, one per line, file order. Exit 0.
- `topic_config.py topic <name>`: `json.dumps(topic, indent=2)`; unknown name prints `"No topic named 'zz' in <path>"` (note: `str(KeyError)` so the message is wrapped in double quotes) to stderr, exit 1.
- no args: `Usage: topic_config.py names` / `       topic_config.py topic <name>` on stderr, exit 1; unknown command `Unknown command: <cmd>`, exit 1; `topic` without name prints `Usage: topic_config.py topic <name>`, exit 1.
- missing `topics.json`: `FileNotFoundError: No topic config at <path>. Copy config/topics.json.template there ...` traceback, exit 1. Non-array: `ValueError`.

Topic shape: `{"name", "label", "brief", "slack_bundle": str|null, "enabled": bool}`.

Sample:

```
$ topic_config.py topic ai
{
  "name": "ai",
  "label": "AI",
  "brief": "b",
  "slack_bundle": null,
  "enabled": true
}
```

Library-only functions to port with the dashboard, not the CLI: `upsert_topic` (strips; requires name/label/brief; edit preserves `enabled`, new topic is enabled; blank bundle stored as `null`), `rename_topic`, `set_enabled`, `delete_topic`; writes are atomic (`.topics.json.tmp` then replace), `indent=2`.
Env: `LOOP_ENGINEERING_HOME`. Files: `$LOOP_ENGINEERING_HOME/topics.json`. Tests: `tests/test_topic_config.py`.

### 1.6 `bin/topic_seen.py`

Purpose: rolling 7-day de-dup state so the daily briefing does not repeat stories.

CLI:
- `topic_seen.py get <topic_name>`: prunes entries older than `SEEN_WINDOW_DAYS = 7` (and entries with a missing/unparseable `seen_at`), **rewrites the state file as a side effect**, prints `json.dumps([{"url","title"}...], indent=2)` (`seen_at` is not exposed; `[]` when empty).
- `topic_seen.py add <topic_name> <url> <title>`: prune, append `{"url","title","seen_at": <UTC now isoformat with +00:00>}`, write, print `OK`. Fewer than 5 argv prints `Usage: topic_seen.py add <topic_name> <url> <title>`, exit 1.
- fewer than 3 argv: two-line usage, exit 1. Unknown command: `Unknown command: <cmd>`, exit 1.

State: `<repo_root>/outputs/topic-monitor/state/<basename(topic_name)>.json` (`DEFAULT_STATE_DIR` is derived from `__file__`, so it is per-checkout and **not** affected by `LOOP_ENGINEERING_HOME`; `Path(topic_name).name` strips path traversal). A corrupt or non-list file reads as `[]`. File format: JSON array, `indent=2`, entries `{"url","title","seen_at"}`.
Called by: `instructions/topic-monitor.md` (agent). Tests: `tests/test_topic_seen.py`.

### 1.7 `bin/slack_notify.py`

Purpose: post to the configured Slack incoming webhook.

Invocation: `slack_notify.py [--bundle=<name>] [--blocks=<json array>] <message text>`. Flags use the `--key=value` form only (no space-separated value). Only the first positional is used as text; extras are ignored. `-h`/`--help` anywhere prints usage to **stdout**, exit 0.
- No positional: usage to stderr, exit 1. Invalid `--blocks` JSON: `Invalid --blocks JSON: <error>` to stderr, exit 1.
- Success: POST to webhook, prints `Slack response status: 200`, exit 0. Network/HTTP failure raises (traceback, exit 1; the shell wrappers use `|| true`).
- Request: `Content-Type: application/json`, body `{"text": <text>}` plus `"blocks": [...]` only when blocks is truthy.
- Webhook resolution: `~/.slack/config.json`; with `--bundle=X`, `bundle_webhooks[X]` if present, else `webhook_url`.
- Library-only: `resolve_blocks(notification_key, message)` substitutes every literal `{{message}}` inside the bound template's blocks (from `block_templates[*].notification_key`); never raises, `None` on any problem. Used by `notify.py`.

Env/Files: `HOME` (`~/.slack/config.json`, read-only). Tests: `tests/test_slack_notify.py`, plus `tests/test_notify.py`, `tests/test_notification_key_consistency.py`.
Note: `LOOP_ENGINEERING_HOME` does not sandbox this; the safety mechanism is a scratch `HOME`.

### 1.8 `bin/scripts/new_worktree.sh`

Usage: `new_worktree.sh <repo_path> <default_branch> <issue_iid> <worktree_root>` (exactly 4 args, else usage on stderr, exit 1). Bash, `set -euo pipefail`.

Behavior, in order:
1. Branch = `loop/issue-<iid>`; worktree path = `<worktree_root>/<basename(repo_path)>-issue-<iid>`; `mkdir -p worktree_root`.
2. `git -C repo fetch origin <default_branch>` (always, stdout redirected to stderr).
3. If the branch already exists: add the worktree if the directory is missing; **refuse** (exit 1, `Worktree has uncommitted changes, refusing to sync: <path>`) if `git status --porcelain` is non-empty; then `git merge --no-edit origin/<default_branch>`; on conflict `git merge --abort` and exit 1 with `Merge conflict pulling in <default_branch>, aborted: <path>`.
   Otherwise: `git worktree add -b loop/issue-<iid> <path> origin/<default_branch>`.
4. Copy `.ruby-version` and `config/database.yml` from repo to worktree if they exist (every run).
5. `git submodule update --init --recursive` in the worktree.
6. stdout: **only** the worktree path plus newline. All git chatter goes to stderr (callers capture stdout as the path).

Exit codes: 0 success; 1 on usage/dirty/conflict; git's own non-zero (via `set -e`) otherwise. Env: none read. Files: creates/updates worktrees and branches in `repo_path`'s git dir; writes the two copied files. Tests: `tests/test_new_worktree.py` (real local git repos).

Sample: `/Users/me/.loop-engineering/worktrees/kurrant.web-issue-42`

### 1.9 `bin/scripts/open_merge_request.sh`

Usage: `open_merge_request.sh <repo_path> <branch> <target_branch> <title>` (exactly 4 args, else usage on stderr, exit 1).
- Hard guard: `branch` must match `loop/issue-*`, else `Refusing to push non-loop branch: <branch>` on stderr, exit 1.
- Executes `git -C <repo> push origin <branch> -o merge_request.create -o merge_request.target=<target> -o merge_request.title=<title> -o merge_request.remove_source_branch` (GitLab push options; GitLab's response on the push goes through git's stderr).
- `DRY_RUN=1`: prints the command argv, **one argument per line**, instead of running. Sample (title `My title`):

```
git
-C
/r
push
origin
loop/issue-5
-o
merge_request.create
-o
merge_request.target=main
-o
merge_request.title=My title
-o
merge_request.remove_source_branch
```

Env: `DRY_RUN`. Exit: git push's status (0 on success). Tests: `tests/test_open_merge_request.py`.

### 1.10 `run-loop-now.sh` (replaces `run-loop.sh` and `run-topic-monitor-loop.sh`)

Usage: `run-loop-now.sh <loop_name> [extra_args...]`. No args: `Usage: run-loop-now.sh <loop_name> [extra_args...]` on stderr, **exit 2**. Callers: `bin/loop_scheduler.py` (env `LOOP_NOW_SH` can override the script path), dashboard "run now" buttons, dashboard chat "paste an issue link" tool (passes `<alias> <issue_iid>`, which makes `RUN_TRIGGER=dashboard`).

Sequence:
1. `LOOP_DIR` = directory of the script. `LOG_DIR=outputs/history`, unified log `logs/loop-engineering.log`.
2. `python3 bin/loops_config.py bash-env <loop_name>` (stderr merged). On failure: append `[ts] ---- <loop> ---- FAILED to resolve its loop registry entry: <msg> ----` to the unified log, send Slack via `slack_notify.py "*<loop> loop FAILED* - could not resolve its registry entry: <msg>" || true`, exit 1. On success, `eval` its output:

```
ENTRY_POINT=bin.a
TIMEOUT_SECONDS=5
LOG_SUFFIX=''
EMIT_RUN_EVENTS=false
```
   (values `shlex.quote`d; `EMIT_RUN_EVENTS` is `true`/`false`). Entry script = `$LOOP_DIR/<ENTRY_POINT with . as />.py`.
3. `RUN_ID=run_<UTC %Y%m%d_%H%M%S>`, exported as `LOOP_RUN_ID`.
4. `mkdir -p` outputs/history, logs, outputs/topic-monitor. From here all stdout/stderr append to `outputs/history/<YYYY-MM-DD><LOG_SUFFIX>.log`; unified log gets `[ts] ---- <loop> ---- run started ----`.
5. ERR trap: unified-log line `run FAILED (exit N)`, Slack `*<loop> loop FAILED* (exit N) — see outputs/history/<date><suffix>.log`, `python3 bin/web/dashboard_server.py write-status failed --loop <loop> --exit-code N`, and if `EMIT_RUN_EVENTS=true` `python3 bin/events.py emit --type run.failed --run-id <id> --data '{"exit_code": N}'`; each guarded with `|| true`.
6. If `EMIT_RUN_EVENTS=true`: `events.py emit --type run.started --run-id <id> --data '{"trigger": "dashboard"|"scheduled"}'` (dashboard iff exactly 2 extra args).
7. `dashboard_server.py write-status running --loop <loop> --pid $$` (the pid doubles as the process-group id for the Stop button; callers launch with `start_new_session=True`).
8. `zsh -i -l -c "timeout <TIMEOUT_SECONDS> python3 <entry_script> <run_id> [extra_args...]"` (args shell-quoted with `printf %q`; interactive login zsh so the user's PATH/claude auth is available).
9. On success: `write-status idle --loop <loop> --exit-code 0`; unified log `run finished (exit 0)`.

Exit: 0, the runner's/timeout's status via `set -e` (124 on timeout), 1 on registry failure, 2 on usage. Env read: none by the script directly, but it inherits `LOOP_ENGINEERING_HOME`, `LOOP_EVENTS_DIR` for children; it **writes** `LOOP_RUN_ID`. Files: `outputs/history/*.log`, `logs/loop-engineering.log`, `outputs/topic-monitor/`, status via dashboard_server. Tests: `tests/test_dashboard_server.py`, `tests/test_loop_scheduler.py`, `tests/test_loops_config.py` (no dedicated `run-loop-now` test file).

Registry `loops.json` entries (`config/loops.json.template`): `gitlab-loop` -> `bin.gitlab_loop_runner`, timeout 43200, suffix `""`, `emit_run_events: true`; `topic-loop` -> `bin.topic_monitor_runner`, 21600, `-topic-loop`, false; `inbox-triage-loop` -> `bin.inbox_triage_runner`, 3600, `-inbox-triage-loop`, false (further entries exist in the template).

#### `loops_config.py` (used by the above)
`loops_config.py <names|entry-point|timeout-seconds|log-suffix|emit-run-events|bash-env> [name]`. `names` prints one per line; the rest need `<name>`; missing name: `Usage: loops_config.py <cmd> <name>`, exit 1; `KeyError`/`ValueError`/`FileNotFoundError`/`JSONDecodeError` while resolving a named loop prints the message to stderr, exit 1 (but `names` with a missing file is an unhandled traceback). `bash-env` requires `entry_point` and `timeout_seconds` (`Loop '<n>' is missing required registry field 'x'`, exit 1). Tests: `tests/test_loops_config.py`.

---

## 2. Remaining Python CLIs

All are hand-rolled `sys.argv` except where noted. "Env" lists env vars the file reads; `HOME` is implied wherever a `~/` path appears.

| Script | Purpose / invocation | Notes (output, exit, env, files) | Tests |
|---|---|---|---|
| `ai_cli_config.py` | `get` / `set <claude\|codex>`: which AI CLI the loop shells out to | usage exit 1; env `LOOP_ENGINEERING_HOME`; file `ai_cli.json` | `test_ai_cli_config.py` |
| `loops_config.py` | see 1.10 | env `LOOP_ENGINEERING_HOME`; file `loops.json` | `test_loops_config.py` |
| `memory_store.py` | `add <alias> <issue_iid> <lesson> [tags_csv] [category]`, `get`, `list`: alias-keyed learnings under `LOOP_ENGINEERING_HOME` | exit 1 on bad usage; env `LOOP_ENGINEERING_HOME` | `test_memory_store.py` |
| `events.py` | `emit --type T --run-id R [--data JSON ...]`, `list (--date D \| --run-id R) [...]`: per-checkout event ledger | usage/unknown command exit 1; env `LOOP_EVENTS_DIR` (else `<repo>/outputs/events/`) | `test_events.py` |
| `cost.py` | `extract-result-text --cli-output-file F`, `usage-json --cli-output-file F`, `report [--days N] [--events-dir PATH]` | usage exit 1; `usage-json` prints one JSON object or skips with stderr note | `test_cost.py` |
| `loop_cli.py` | `validate`, `audit`, `run <loop.yaml> [--cwd] [--results-dir] ...`, `inspect <run_id>`, `replay <run_id>`, `doctor`, `eval [--golden\|--chat] [--budget-usd N] [--case NAME]`, `ledger backfill [--results-dir] [--events-dir]` | the Rust migration's primary subcommand tree; env `LOOP_EVALS_DIR`, `LOOP_EVENTS_DIR`; exits per subcommand | `test_loop_cli.py`, `test_loop_eval.py`, `test_loop_runtime*.py` |
| `loop_audit.py` | `loop_audit.py <loop.yaml>` | usage exit 1 | `test_loop_audit.py` |
| `loop_scheduler.py` | no args: one poll of due loops (launchd every 900 s) and spawn `run-loop-now.sh` | env `LOOP_NOW_SH`, `LOOP_DIR`, `LOOP_ENGINEERING_HOME`; state file under `LOOP_ENGINEERING_HOME` | `test_loop_scheduler.py` |
| `gitlab_loop_runner.py` | `<run_id> [alias issue_iid]`: the GitLab loop entry point | usage exit 1; env `LOOP_RUN_ID`, `LOOP_HANDOFF_PATH`; writes handoffs/results under `outputs/` | `test_gitlab_loop_runner.py`, `test_gitlab_issue_loop_definition.py` |
| `topic_monitor_runner.py` | `<run_id>`: topic loop entry point, one agent call per enabled topic | env `LOOP_DIR`, `LOOP_ENGINEERING_HOME` | `test_topic_monitor_runner.py`, `test_topic_monitor_loop_definition.py` |
| `inbox_triage_runner.py` | `<run_id>`: inbox triage loop entry point | usage exit 1 | `test_inbox_triage_runner.py` |
| `notify.py` | `notify.py <loop_name> <text>`: Slack notify routed by notification key | usage exit 1 | `test_notify.py` |
| `risk.py` | `score --title T --description D` | exit 1 on bad usage | `test_risk.py` |
| `learning.py`, `metrics.py` | subcommand CLIs for outcome learnings / run metrics | exit 1 on bad usage | `test_learning.py`, `test_metrics.py` |
| `golden_eval.py` | only `agent --issue-file F --run-id R --result-file F --repo-root D [--max-budget-usd N] [--feedback-file F] [--log F] [--timeout 900]` (**argparse**); the suite itself runs via `loop_cli.py eval --golden` | env `LOOP_ENGINEERING_HOME`, `LOOP_EVALS_DIR`, `LOOP_EVENTS_DIR`, `LOOP_RUN_ID`; costs real money | `test_golden_eval.py` |
| `chat_eval.py` | driven by `loop_cli.py eval --chat` | | `test_chat_eval.py` |
| `update_runner.py` | `run [--repo R] [--state-dir D] [--install-target T]` (**argparse**, positional `command` choices `["run"]`) | env `LOOP_UPDATE_DIR` (tests must set) | `test_update_runner.py` |
| `update_check.py` | library plus `__main__`-less helper used by the dashboard | state under `LOOP_UPDATE_DIR` or `<repo>/outputs/update/` | `test_update_check.py` |
| `desktop_app.py` | `[--port N] [--title T]` (**argparse**): macOS app shell attaching to the dashboard | env `LOOP_DIR` | `test_desktop_app.py` |
| `linkedin_login.py` | `<account_id>`: interactive browser login | usage exit 1 | `test_linkedin_login.py` |
| `bin/web/dashboard_server.py` | `[port]` (default `DEFAULT_PORT`, 8420) runs the server. Subcommands: `write-status <state> --loop L [--pid N] [--exit-code N] [--current-step S]`, `write-topic-status <topic> <state> [--current-step S]`, `write-skills-install-status <state> [--status-path P]`, `read-messages`, `add-message <user\|loop> <text>`, `chat-tool <name> [args]` | env `LOOP_ENGINEERING_HOME`, `LOOP_EVALS_DIR`, `LOOP_DIR`, `LOOP_NOW_SH`, `LOOP_CHAT_SESSION`; `outputs/messages.json` is **not** sandboxed; every POST route needs CSRF | `test_dashboard_server.py`, `test_hub.py`, `test_inbox_pages.py`, `test_update_pages.py` |
| `bin/loop_plugins/*.py` | `calendar_prep`, `daily_digest`, `linkedin_autopilot`, `mr_review`, `pipeline_doctor`, `release_notes`, `rss_watch`, `stale_sweeper`: LoopKit plugins with a `__main__` guard, run through `loopkit.py` and `seen_store.py` | each reads its own connector config; pattern `bin/loop_plugins/*.py` is in the runner's `_allowed_tools()` | `test_loop_plugin_<name>.py`, `test_loopkit.py`, `test_seen_store.py` |
| `bin/chat_tools/*.py` | `calendar`, `documents`, `github`, `gitlab`, `mail`, `pending`, `registry`, `rss`, `settings`: invoked only in-process or via `dashboard_server.py chat-tool <name>`; **no standalone CLI** | write tools stop at the Approve card | `test_chat_tools_*.py` |
| Library-only modules (no CLI) | `loop_runtime`, `loop_state`, `loop_policy`, `loop_budget`, `loop_progress`, `loop_result`, `loop_verifiers`, `loop_definition`, `loop_serialize`, `ledger`, `instructions`, `health`, `auth_health`, `content_archive`, `connectors_config`, `inbox_config`, `inbox_seen`, `inbox_status`, `inbox_triage`, `issue_tracking_config`, `mail_*`, `secret_store`, `i18n`, `linkedin_{browser,extract,humanize,people,session}`, `connectors/`, `agents/`, `mail_providers/` | | matching `tests/test_*.py` |

Per-file argparse detail beyond the above is intentionally shortened; before porting `loop_cli.py`, `events.py`, `cost.py` or `metrics.py`, read their `main` and `_cmd_*` functions directly (they were not exhaustively captured here).

---

## 3. Shell scripts

| Script | Usage / flags | Purpose and notes | Tests |
|---|---|---|---|
| `bin/scripts/new_worktree.sh` | see 1.8 | | `test_new_worktree.py` |
| `bin/scripts/open_merge_request.sh` | see 1.9 | | `test_open_merge_request.py` |
| `run-loop-now.sh` | see 1.10 | | indirect |
| `bin/scripts/build_run_prompt.sh` | `[alias issue_iid \| --batch-issue alias issue_iid \| --batch-end-of-run \| --issue-file alias issue_iid issue.json]`; bad arity prints usage to stderr, exit 1 | prints the agent prompt to stdout. 0 args: `Follow instructions/gitlab-issue.md in <LOOP_DIR> exactly. This is a scheduled headless run ...`; 2 args: single-issue dashboard variant | `test_build_run_prompt.py` |
| `bin/scripts/build_topic_prompt.sh` | `[topic_name]`; 0 args legacy all-topics prompt, 1 arg per-topic prompt, more: `Usage: build_topic_prompt.sh [topic_name]`, exit 1 | stdout only; **unused by the runtime now** | `test_build_topic_prompt.py` |
| `bin/scripts/setup.sh` | `[--skip-skills-install] [--config-path P] [--topics-config-path P] [--connectors-config-path P] [--ai-cli-config-path P] [--loops-config-path P] [--state-path P]` | scaffolds config files from `config/*.template`; ignores `LOOP_ENGINEERING_HOME`; tests run it under a temp `HOME` | `test_setup.py` |
| `bin/scripts/install.sh` | `[--dir PATH] [--branch N] [--repo-url U] [--launch-agents-dir P] [--upgrade] [--skip-nginx] [--skip-launchd-daemons] [--port N] [--skip-skills-install] [--config-path P] [--topics-config-path P] [--ai-cli-config-path P] [--loops-config-path P] [--state-path P]` | clone/upgrade into `$HOME/.loop-engineering`, render plist templates, load launchd. **Never run bare in dev**; always pass `--dir` | `test_install.py` |
| `bin/scripts/uninstall.sh` | `[--launchd-dir P] [--launch-agents-dir P] [--nginx-domain N] [--nginx-servers-dir P] [--hosts-file P] [--project-dir P] [--skip-nginx] [--skip-hosts] [--skip-service] [--keep-config]` | removes agents, nginx entry, and `rm -rf` of the project dir; refuses the default dir when run from a different checkout without `--project-dir`. **Never run bare** | `test_uninstall.py` |
| `bin/scripts/setup-nginx.sh` | `[--domain N] [--port N] [--servers-dir P] [--hosts-file P] [--skip-brew-install] [--skip-hosts] [--skip-service]` | local nginx reverse proxy for the dashboard | `test_setup_nginx.py` |
| `bin/scripts/restart-daemons.sh` | `[--launchd-dir P] [--with-scheduler]` | `launchctl kickstart -k` for loaded agents; dashboard only by default | `test_restart_daemons.py` |
| `bin/scripts/build_macos_app.sh` | `[--output-dir D] [--python P] [--skip-venv] [--skip-icon] [--dmg]` | builds the `.app` bundle | `test_desktop_app.py` (partial) |
| `bin/scripts/rebuild_macos_app.sh` | `[--no-pull] [--no-launch] [--output-dir D] [build_macos_app.sh options...]` | pull + rebuild + relaunch | none |
| `launchd/com.hermes.loop-engineering.plist.template` | placeholders `{{PYTHON3}}`, `{{LOOP_DIR}}` | label `com.hermes.loop-engineering`; runs `{{PYTHON3}} {{LOOP_DIR}}/bin/loop_scheduler.py`; `RunAtLoad` false; `StartInterval` 900; stdout/stderr `{{LOOP_DIR}}/outputs/history/launchd.{out,err}.log` | `test_install.py` |
| `launchd/com.hermes.loop-engineering-dashboard.plist.template` | `{{PYTHON3}}`, `{{LOOP_DIR}}`, `{{PORT}}` | label `com.hermes.loop-engineering-dashboard`; runs `dashboard_server.py {{PORT}}`; `RunAtLoad` and `KeepAlive` true; logs `dashboard.{out,err}.log` | `test_install.py` |

## 4. Porting notes for the priority set

- stdout is parsed by agents, so keep exact `indent=2` JSON (Python separators `", "`/`": "` with newline-per-element; the key order of the source object is preserved) and the exact one-per-line formats above.
- `track_new_comments.py`, `project_memory.py`, and `list_assigned_issues.py` depend on the external gitlab-config skill under `~/.encore-skills/skills/gitlab-config/scripts/` (cache layout `~/.gitlab/cache/<instance>/projects/<project>/{project.json,issues/<iid>.json}`, annotation keys `_notes.loop_last_seen_note_id` and `_memory.fix_learnings`). A Rust port either reimplements that cache format or shells out to the same Python.
- `topic_seen.py` state dir is repo-relative, not `LOOP_ENGINEERING_HOME`-relative; `topic_config.py` is the reverse.
- Error exits are all 1 except `run-loop-now.sh` usage (2) and timeouts (124 from `timeout`).

## Rust replacements added for `loop_cli.py`, `metrics.py`, update

- `loop_cli.py <sub>` is `loopx loop <sub>` (`init validate audit run status inspect replay cost doctor eval ledger`;
  `ledger backfill` is also `loopx ledger backfill`). `eval --golden|--chat` stay paid and opt-in. Hidden
  `loopx loop eval-agent` is the golden suite's per-attempt child.
- `metrics.py [--days N] [--project P] [--events-dir D]` is `loopx metrics`.
- `update_runner.py run [--repo] [--state-dir] [--install-target]` is `loopx update run`; new: `update check|status|start|notification`.
- `health.py` and `auth_health.py` have no CLI; they are `loopx_core::health` and `loopx_core::auth_health`.

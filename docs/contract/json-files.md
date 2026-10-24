# On-disk JSON contracts

The Rust `loopx` binary must read and write these files so that the Python
implementation (still shipping during the migration) and the Rust one can
share the same state directory interchangeably. All examples use synthetic
values; never paste real tokens, URLs or usernames into this document.

Source of truth is the Python code named under each file. Where the Python
behavior is lenient or quirky, the quirk is part of the contract and is
called out.

## Conventions shared by every file

### Locations

| Variable | Effect |
|---|---|
| `LOOP_ENGINEERING_HOME` | Overrides the base dir for files under `~/.loopx/`. Default `~/.loopx`. Read once at process start. |
| `HOME` | `~/.gitlab/config.json` and `~/.slack/config.json` are resolved from `Path.home()` and are **not** affected by `LOOP_ENGINEERING_HOME`. |
| (repo checkout) | `outputs/status.json`, `outputs/messages.json` live under `<repo_root>/outputs/` (per-checkout run state), where `repo_root` is the checkout containing `bin/web/dashboard_server.py`. `LOOP_ENGINEERING_HOME` does not move them. |

### Serialization

- UTF-8 JSON, written with Python `json.dump(..., indent=2)`: two-space
  indent, `", "`/`": "` separators with newlines, keys in insertion order
  (never sorted), non-ASCII **escaped** as `\uXXXX` (`ensure_ascii=True`
  default), and **no trailing newline**. Rust should emit the same to stay
  byte-stable (serde_json `PrettyFormatter` with two spaces plus an
  ASCII-escaping writer, preserve key order via `preserve_order`). Readers
  must accept any valid JSON.
- Timestamps are `datetime.now(timezone.utc).isoformat()`, e.g.
  `2026-10-22T08:15:03.412907+00:00` (microseconds present unless exactly 0,
  `+00:00` offset, never `Z`).

### Write strategies (three exist; do not unify silently)

1. **Atomic + 0600** (`_atomic_write_json` in `bin/web/dashboard_server.py`):
   `NamedTemporaryFile` in the same directory named `.<file>.<random>.tmp`,
   `json.dump`, `chmod 0600`, `os.replace` over the target. Parent dirs are
   created. On error the temp file is unlinked. Used for `projects.json`
   (dashboard writes), `~/.gitlab/config.json`, `~/.slack/config.json`,
   `messages.json`, `chat-sessions.json`.
2. **Atomic, default mode** (`topic_config._write_topics`): writes
   `.<file>.tmp` in the same dir (fixed name, not random), `Path.replace`.
   Used for `topics.json`.
3. **Plain truncate-and-write** (`open(path, "w")`, not atomic): `status.json`
   (`write_status`), `topic-monitor/status.json`, `ai_cli.json`. A crash
   mid-write can leave a corrupt file; readers therefore treat parse errors
   as "unknown/default" (see per file). Rust may improve on this by writing
   atomically; that is compatible, since readers cannot observe the
   difference.

### Locking

Only `messages.json` (and `chat-sessions.json`) use locking: an exclusive
`fcntl.flock(LOCK_EX)` on a **sibling** lock file (`messages.json.lock`,
`chat-sessions.json.lock`, opened `a+`, never deleted), held across the whole
read-modify-write. The lock file is not the data file, so the atomic
`os.replace` does not invalidate it. Rust must take the same `flock` on the
same `.lock` path so Python and Rust writers exclude each other. No other
file in this document is locked; they are last-writer-wins.

### Read leniency

Dashboard-side readers are best-effort (missing/corrupt returns a default);
loop-side loaders (`loop_config`, `topic_config`) fail fast. Both behaviors
are listed per file.

---

## 1. `projects.json`

- **Path:** `$LOOP_ENGINEERING_HOME/projects.json` (default
  `~/.loopx/projects.json`). Template: `config/projects.json.template`
  (`{{HOME}}` in `worktree_root` is substituted by the installer).
- **Readers:** `bin/loop_config.py` (strict; `load_config`, `get_project`,
  `get_instance`, `get_assignee_username`, `get_worktree_root`, `list_aliases`,
  CLI `loop_config.py aliases|local-paths|worktree-root|assignee|instance|project <alias>`),
  `bin/gitlab_loop_runner.py`, `bin/loop_verifiers.py`,
  `bin/web/dashboard_server.py` (`read_loop_projects_config`, lenient: `{}`
  on missing/corrupt), `gitlab_issue_url_prefixes`.
- **Writers:** dashboard Settings page only
  (`update_loop_project_settings`, `upsert_tracked_project`,
  `delete_tracked_project`) via strategy 1 (atomic, 0600). The installer
  copies the template once.

### Schema

Top level: JSON **object**.

| Field | Type | Required | Notes |
|---|---|---|---|
| `gitlab_instance` | string | yes (loop side) | Default instance key; must be a key of `instances` in `~/.gitlab/config.json` (the dashboard enforces this when saving; `loop_config` does not). |
| `assignee_username` | string | yes | GitLab username whose assigned issues are tracked. Non-empty on dashboard save. |
| `worktree_root` | string | yes | Absolute path; per-issue worktrees at `<worktree_root>/<repo>-issue-<iid>`. Non-empty on dashboard save. |
| `projects` | object alias -> project | yes (loop side) | Alias is the identity key (history, dedup, worktree names); treat as fixed. May be `{}`. |

Project object:

| Field | Type | Required | Notes |
|---|---|---|---|
| `project_id` | string | yes | GitLab path (`group/sub/project`) or numeric id as string. A URL is only derivable when it contains `/`. Non-empty on dashboard save. |
| `local_path` | string | yes | Absolute checkout path. Dashboard saves it stripped, may be `""`. |
| `target_branch` | string | yes | Branch worktrees are cut from / MRs target. May be `""` when saved from the UI. |
| `install_cmd` | string | yes | Shell command; may be `""`. |
| `lint_cmd` | string | yes | Shell command; may be `""`. See `CLAUDE.md` on pass-an-explicit-path (`rubocop .`) and `--cache false`. |
| `test_cmd` | string | yes | Shell command; may be `""`. |
| `instance` | string | optional | Per-project GitLab instance override. **Omitted entirely** (never `""`) when unset. |

### Semantics and quirks

- `get_project(alias)` returns a **copy** of the project with `instance`
  always populated: `setdefault("instance", config["gitlab_instance"])`. The
  default is never written back to disk.
- Unknown alias raises `KeyError`; missing file raises `FileNotFoundError`
  with a message telling the user to copy the template. Invalid JSON raises
  `json.JSONDecodeError`. There is no schema validation on load: a missing
  key surfaces as `KeyError` at the point of use.
- Unknown extra keys (top level or per project) must be **preserved** on
  rewrite: the dashboard read-modify-writes the whole document
  (`config.setdefault("projects", {})`, edits one entry). Rust must do a
  round-trip on a generic JSON value, not a fixed struct that drops unknown
  fields.
- `upsert_tracked_project` rebuilds the project entry from exactly the six
  string fields (+ optional `instance`), so any extra per-project keys an
  operator added are dropped when that project is edited from the UI. A
  rename (`original_alias` != `alias`) deletes the old key and inserts the
  new one at the **end** of the object (alias order changes).
- Dashboard save validation: alias and project_id non-empty after strip;
  rename target must not exist; unknown `original_alias` rejected.

### Example

```json
{
  "gitlab_instance": "work",
  "assignee_username": "jdoe",
  "worktree_root": "/Users/jdoe/.loop-engineering/worktrees",
  "projects": {
    "acme.web": {
      "project_id": "acme/platform/acme.web",
      "local_path": "/Users/jdoe/code/acme.web",
      "target_branch": "staging",
      "install_cmd": "bundle check || bundle install",
      "lint_cmd": "bundle exec rubocop .",
      "test_cmd": "RAILS_ENV=test bundle exec rspec"
    },
    "harbor": {
      "project_id": "other-org/harbor",
      "local_path": "/Users/jdoe/code/harbor",
      "target_branch": "main",
      "install_cmd": "npm ci",
      "lint_cmd": "npm run lint",
      "test_cmd": "npm run test",
      "instance": "partner"
    }
  }
}
```

---

## 2. `topics.json`

- **Path:** `$LOOP_ENGINEERING_HOME/topics.json`. Template:
  `config/topics.json.template`.
- **Readers:** `bin/topic_config.py` (`load_config`, `list_names`,
  `get_topic`; CLI `topic_config.py names|topic <name>`),
  `bin/topic_monitor_runner.py`, dashboard.
- **Writers:** `topic_config.upsert_topic`, `rename_topic`, `set_enabled`,
  `delete_topic` (all strategy 2: atomic via fixed `.topics.json.tmp`,
  default file mode, no lock).

### Schema

Top level: JSON **array** (a non-array raises `ValueError`
`"... must contain a JSON array of topics, got <type>"`; missing file raises
`FileNotFoundError` on load, but mutators treat a missing file as `[]`).

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Identity key (slug, e.g. `ai-news`). Used in history filenames `<date>...<name>.md`, `outputs/topic-monitor/status.json` keys and `topic_seen` state file `<name>.json`. Unique; `rename_topic` rejects collisions. |
| `label` | string | yes | Display name. Non-empty on save. |
| `brief` | string | yes | What counts as notable; non-empty on save. |
| `slack_bundle` | string or `null` | yes on write | Bundle key into `~/.slack/config.json` `bundle_webhooks`. `null` means default webhook. A blank string is stored as `null`. The shipped template omits `enabled`. |
| `enabled` | bool | optional | **Absent means enabled** (`t.get("enabled", True)`). Written explicitly (`true`/`false`) by every mutator. |

### Semantics and quirks

- `upsert_topic` strips all inputs, rebuilds the entry as exactly
  `{name,label,brief,slack_bundle,enabled}` in that key order (extra keys on
  an existing entry are dropped), preserves the existing `enabled` on edit,
  new topics get `enabled: true` and are appended at the end; edits replace
  in place (order preserved).
- `rename_topic` and `set_enabled` mutate in place and preserve other keys.
  `set_enabled` coerces with `bool()`.
- `rename_topic` only touches this file; the caller migrates history
  files, status entry and seen-state (`_migrate_topic_rename` in the
  dashboard). Rust must do the same three migrations if it takes over rename.
- `get_topic` on an unknown name raises `KeyError`.

### Example

```json
[
  {
    "name": "ai-news",
    "label": "AI News",
    "brief": "Major model releases and funding rounds from the last 24 hours. Skip developer-tooling releases.",
    "slack_bundle": null,
    "enabled": true
  },
  {
    "name": "ai-policy-safety",
    "label": "AI Policy & Safety",
    "brief": "Regulation, policy proposals and publicized safety incidents from the last 24 hours.",
    "slack_bundle": "ops",
    "enabled": false
  }
]
```

---

## 3. `ai_cli.json`

- **Path:** `$LOOP_ENGINEERING_HOME/ai_cli.json`. Template:
  `config/ai_cli.json.template` (`{"cli": "claude"}`).
- **Readers:** `bin/ai_cli_config.py` (`get_selected_cli`; CLI
  `ai_cli_config.py get`), `bin/gitlab_loop_runner.py`, run scripts.
- **Writers:** `ai_cli_config.set_selected_cli` (CLI `set <claude|codex>`),
  strategy 3 (plain write, **not** atomic, parent dirs created).

### Schema

Top level object; the only key consumed is:

| Field | Type | Required | Notes |
|---|---|---|---|
| `cli` | string, one of `"claude"`, `"codex"` | optional on read | Anything else, missing key, missing file, invalid JSON or a non-object file all resolve to `"claude"` (fail-safe: claude's guardrails are harness-enforced). |

### Semantics and quirks

- `load_config` returns `{}` for missing file, `JSONDecodeError`, or a
  non-dict document: it never raises.
- `set_selected_cli` **overwrites the whole file** with `{"cli": "<value>"}`
  (any other keys are discarded); an invalid value returns
  `(False, "Unknown AI CLI: <v>")` without writing.
- Output of `get` is the bare word plus newline on stdout.

### Example

```json
{
  "cli": "codex"
}
```

---

## 4. `outputs/status.json` (GitLab loop run status)

- **Path:** `<repo_root>/outputs/status.json` (`STATUS_PATH`). Not affected by
  `LOOP_ENGINEERING_HOME`. **Other loops** use
  `<repo_root>/outputs/status/<loop_name>.json` (same schema;
  `status_path_for_loop`), so `gitlab-loop` is the only loop with the legacy
  top-level path.
- **Readers:** dashboard (`read_status`, Run/Stop/Overview pages, run-now
  refusal logic, `_process_alive` pid check), `run-loop-now.sh`.
- **Writers:** `python3 bin/web/dashboard_server.py write-status <state>
  [--loop NAME] [--exit-code N] [--current-issue TEXT] [--current-step TEXT]
  [--pid N]` (invoked by `run-loop-now.sh` and, with `running`, by the agent
  per `instructions/gitlab-issue.md`), and the dashboard's own Stop handler.
  Strategy 3: plain `open("w")`, creating parent dirs, no lock.
  **Each write replaces the whole document**: fields from the previous write
  are not carried over; a writer must resupply any field it wants kept.

### Schema

Top level object (a non-object file reads as `{"state": "unknown"}`).

| Field | Type | Required | Notes |
|---|---|---|---|
| `state` | string | yes | Observed values: `running`, `idle`, `failed`, `stopped`. Synthetic read values (never written): `never_run` (file missing), `unknown` (corrupt/non-object). Consumers branch on `== "running"`. |
| `updated_at` | string (ISO 8601 UTC) | yes (written) | Set by `write_status` on every write. |
| `pid` | integer | optional | `--pid`; the runner's `$$`. With `state == "running"` the Stop/refuse logic checks the pid is alive; if the pid is missing or dead the run is treated as stale. |
| `last_exit_code` | integer | optional | `--exit-code N`. Key name differs from the flag. |
| `current_issue` | string | optional | Free text, `"<alias> #<iid>"`. |
| `current_step` | string | optional | e.g. `analyzing`, `implementing`, `verifying`, `opening_mr`. |

Extra keys are tolerated by readers. Key order as written: `state`,
`updated_at`, then extras in call order (pid, last_exit_code, current_issue,
current_step as parsed by the CLI: pid, exit code, issue, step).

### Related per-topic file (same writer module, different path)

`<repo_root>/outputs/topic-monitor/status.json` has the shape
`{"topics": {"<name>": {"state": "...", "updated_at": "...", "current_step": "..."}}}`
(`write-topic-status <topic> <state> [--current-step T]`). It is
read-modify-written whole, so concurrent writers race; the missing/corrupt
default is `{"topics": {}}`. Out of scope for the main contract but must be
handled by the same code path. `outputs/inbox-triage/status.json` and
`outputs/update/status.json` are different, unrelated documents.

### Example

```json
{
  "state": "running",
  "updated_at": "2026-10-22T08:15:03.412907+00:00",
  "pid": 48213,
  "current_issue": "acme.web #1042",
  "current_step": "implementing"
}
```

After a clean run:

```json
{
  "state": "idle",
  "updated_at": "2026-10-22T08:41:57.120334+00:00",
  "last_exit_code": 0
}
```

---

## 5. `outputs/messages.json` (dashboard chat / loop inbox)

- **Path:** `<repo_root>/outputs/messages.json` (`MESSAGES_PATH`), plus the
  siblings `messages.json.lock`, `chat-sessions.json`,
  `chat-sessions.json.lock` in the same directory. Per-checkout; ignores
  `LOOP_ENGINEERING_HOME`.
- **Readers/writers:** `bin/web/dashboard_server.py`: `read_messages`,
  `append_message`, `pop_unseen_user_messages`; CLI `dashboard_server.py
  read-messages` (the GitLab loop's inbox: prints and marks-seen the unseen
  user messages as a JSON array, indent 2) and `add-message <from> <text>`.
  Writers are two different OS processes (dashboard chat threads and the
  scheduled loop), hence the flock.
- **Write:** strategy 1 (atomic, 0600) under the flock described in
  "Locking". Whole file rewritten on every append/pop.

### Schema

Top level: JSON **array** of message objects, oldest first (flat, sessions
only group for display). On read, a non-array or unreadable file yields `[]`
and non-object elements are silently dropped (and so vanish on the next
write).

| Field | Type | Required | Notes |
|---|---|---|---|
| `from` | string | yes | `"user"` or `"loop"`. |
| `text` | string | yes | Message body (markdown for assistant replies). |
| `timestamp` | string (ISO 8601 UTC) | yes | As in Conventions. |
| `seen_by_loop` | bool | only when `from == "user"` | Written `false`; flipped to `true` in the same atomic write by `pop_unseen_user_messages`. A message is "unseen" iff `from == "user"` **and** `seen_by_loop is False` (absent does not count). Loop messages never carry it. |
| `session` | string | optional | Chat session id (12 hex chars, `secrets.token_hex(6)`). **Omitted** for messages in the legacy session (`LEGACY_CHAT_SESSION_ID = "earlier"`); a message with no/empty `session` belongs to `"earlier"`. |
| `tools` | array of objects | optional | Only on assistant (`loop`) replies, only if non-empty. Each item: `{"name": string, "summary": string, "ok": bool\|null, "pending"?: object}` in call order; `ok` is `null` until a result; `pending` (optional object) holds an awaiting-approval card payload. |

Key order as written: `from`, `text`, `timestamp`, `seen_by_loop`?,
`session`?, `tools`?.

Behavioral rules Rust must match:
- `append_message` with `session=None` (the loop's `post-message` path) uses
  the current session, or opens a new one if "New chat" cleared it
  (`current_chat_session_id`). Before `chat-sessions.json` exists, untagged
  messages count as the current `"earlier"` session.
- A failed assistant run appends **nothing** (no error message is persisted).
- The file is never truncated or pruned by the dashboard other than session
  deletion (`delete_chat_session`).

### `chat-sessions.json` (sibling)

```json
{
  "current": "3f9a1c07b2de",
  "sessions": [
    {"id": "3f9a1c07b2de", "started_at": "2026-10-22T08:10:00.000000+00:00",
     "title": "Which issues are blocked on review", "title_source": "auto"}
  ]
}
```

`current` is a session id or `null` (after "New chat"). Missing or malformed
file reads as `{"current": null, "sessions": []}` with `exists: false`.
`title_source` is `"auto"` for the heuristic title (first non-blank line of
the first message, markdown punctuation stripped, max 48 chars cut on a word
boundary, `…` appended; `"New chat"` if blank); an AI-generated title
replaces it with a different source value. Same flock/atomic discipline on
`chat-sessions.json.lock`.

### Example

```json
[
  {
    "from": "user",
    "text": "What is the loop working on right now?",
    "timestamp": "2026-10-22T08:10:00.250113+00:00",
    "seen_by_loop": true,
    "session": "3f9a1c07b2de"
  },
  {
    "from": "loop",
    "text": "Currently implementing **acme.web #1042**.",
    "timestamp": "2026-10-22T08:10:09.871420+00:00",
    "session": "3f9a1c07b2de",
    "tools": [
      {"name": "status", "summary": "Read loop status", "ok": true}
    ]
  }
]
```

---

## 6. `~/.gitlab/config.json`

- **Path:** `~/.gitlab/config.json` (`GITLAB_CONFIG_PATH`; **not** under
  `LOOP_ENGINEERING_HOME`; tests/sandboxes must set `HOME`). The file is
  shared with the external `gitlab-config` skill (`gitlab_api.py`, installed
  under `~/.encore-skills/`), so this repo is **not** the only writer and
  extra keys must survive.
- **Contains secrets** (`token`). Written 0600; Rust must keep the 0600 mode
  and must never log, echo or copy token values into any other file,
  dashboard HTML or error text (all connector views redact them).
- **Readers:** `dashboard_server.read_gitlab_config` (lenient: `{}` on
  missing/corrupt; note a valid non-object JSON would be returned as-is),
  `gitlab_issue_url_prefixes` (reads only `instances.*.url`),
  `gitlab_loop_runner._project_slack_target` (reads `projects.<alias>.bundle`
  and `instances.<name>.url`), `connectors_config` (lists `instances` as
  read-only external accounts, id = instance name, `settings.url` only),
  `connectors/gitlab.py`, `gitlab_api.py` (external).
- **Writers:** dashboard Settings (`upsert_gitlab_instance`,
  `delete_gitlab_instance`, `set_default_gitlab_instance`,
  `upsert_access_bundle`, `delete_access_bundle`) via strategy 1.

### Schema

Top-level object. Only the fields below are known to this repo; treat the
rest as opaque and preserve them.

| Field | Type | Required | Notes |
|---|---|---|---|
| `default` | string | optional | Default instance name; must be a key of `instances` (`set_default_gitlab_instance` rejects unknown). The default instance cannot be deleted. |
| `instances` | object name -> instance | yes for any use | |
| `instances.<name>.url` | string | yes | Base URL, e.g. `https://gitlab.example.com`. Trailing slash tolerated (consumers `rstrip('/')`). Non-empty on save. |
| `instances.<name>.token` | string | yes (secret) | Personal access token. On edit, a blank submitted token means "keep the existing one"; required (non-blank) for a new instance. |
| `bundles` | object name -> bundle | optional | An "access bundle": an alternate identity (token) on an instance. |
| `bundles.<name>.instance` | string | yes | Must be a key of `instances`. Cannot be changed while a project references the bundle. |
| `bundles.<name>.token` | string | yes (secret) | Same blank-means-keep rule on edit. |
| `projects` | object alias -> `{ "bundle"?: string, "instance"?: string }` | optional | Per-alias access mapping, keyed by the same alias as `projects.json`. Dashboard blocks deleting an instance/bundle that any project/bundle still references. This repo only reads `bundle` (and `instance` for referential checks). |

Delete rules (referential integrity, enforced by the dashboard, not by any
reader): instance deletion fails if it is `default`, or any `projects.*.instance`
or `bundles.*.instance` points at it; bundle deletion fails if any
`projects.*.bundle` points at it and, on success, also removes
`bundle_webhooks.<name>` from the Slack config (two-file write, GitLab file
first).

Consumers must tolerate: missing file, non-dict, missing `instances`,
`instances.<name>` not a dict (skipped), missing `url`.

### Example (placeholder token strings)

```json
{
  "default": "work",
  "instances": {
    "work": {"url": "https://gitlab.example.com", "token": "glpat-REDACTED-EXAMPLE"},
    "partner": {"url": "https://git.partner.example", "token": "glpat-REDACTED-EXAMPLE-2"}
  },
  "bundles": {
    "ops": {"instance": "work", "token": "glpat-REDACTED-EXAMPLE-3"}
  },
  "projects": {
    "acme.web": {"bundle": "ops"}
  }
}
```

---

## 7. `~/.slack/config.json`

- **Path:** `~/.slack/config.json` (`SLACK_CONFIG_PATH`;
  `slack_notify.DEFAULT_CONFIG_PATH`). Not under `LOOP_ENGINEERING_HOME`.
- **Contains secrets** (webhook URLs are bearer credentials). 0600 on
  dashboard writes. Never log or surface them (the Connectors page lists
  Slack accounts with empty `settings`).
- **Readers:** `bin/slack_notify.py` (**strict** for the webhook:
  `load_webhook_url` raises `KeyError('webhook_url')` / `FileNotFoundError` /
  `JSONDecodeError`; **lenient** for blocks: `resolve_blocks` returns `None`
  on any error), `dashboard_server.read_slack_config` (`{}` on
  missing/corrupt), `connectors_config` (lists `slack-default` iff
  `webhook_url` is truthy and `slack-<bundle>` for each key in
  `bundle_webhooks`; secrets resolved from the same file).
- **Writers:** dashboard Settings (`upsert_access_bundle` with a webhook,
  `delete_access_bundle`, `clear_bundle_webhook`, `upsert_block_template`,
  `delete_block_template`, default-webhook save) via strategy 1.

### Schema

| Field | Type | Required | Notes |
|---|---|---|---|
| `webhook_url` | string | yes for posting | Default Slack incoming webhook. |
| `bundle_webhooks` | object bundle-name -> webhook url string | optional | Per-bundle override. `slack_notify --bundle=<name>` uses `bundle_webhooks[name]` **if truthy**, else falls back to `webhook_url`. The bundle name is shared with `~/.gitlab/config.json` `bundles` and `topics.json` `slack_bundle`; the two files are joined only by that name. |
| `block_templates` | object name -> template | optional | Block Kit templates. |
| `block_templates.<name>.blocks` | array | yes | Slack Block Kit blocks. Any string anywhere inside may contain the literal `{{message}}`, replaced recursively (`substitute_message`) by the alert text. Must be a JSON list on save. |
| `block_templates.<name>.notification_key` | string or `null` | yes on write | Binds the template to an alert. Allowed values: `gitlab_wrapup_failed`, `gitlab_issues_incomplete`, `topic_monitor_incomplete`, `inbox_triage_digest`. Each key binds to at most one template: saving a template with a key sets `notification_key` to `null` on whichever other template held it. |

Message POST contract (`post_message`): HTTP POST to the webhook with header
`Content-Type: application/json` and body `{"text": "<text>"}` plus
`"blocks": [...]` only when blocks are non-empty (key omitted otherwise);
returns the HTTP status. Template lookup (`resolve_blocks`): the **first**
template (dict order) whose `notification_key` equals the requested key;
none -> `None` (plain text only).

Shipped default templates live in `docs/slack-templates/<name>.json`
(`{"blocks": [...], "notification_key": ...}`), are read-only, and are
shadowed by a saved template of the same name; they are never written to
this file unless the user saves them.

### Example (placeholder webhook URLs)

```json
{
  "webhook_url": "https://hooks.slack.example/services/T000/B000/REDACTEDEXAMPLE",
  "bundle_webhooks": {
    "ops": "https://hooks.slack.example/services/T000/B001/REDACTEDEXAMPLE2"
  },
  "block_templates": {
    "wrapup-failed": {
      "notification_key": "gitlab_wrapup_failed",
      "blocks": [
        {"type": "section", "text": {"type": "mrkdwn", "text": "{{message}}"}}
      ]
    }
  }
}
```

---

## Compatibility checklist for the Rust port

1. Round-trip unknown keys in every file (generic JSON value, ordered maps).
2. Match write strategy per file (atomic+0600 vs. plain) or be strictly
   safer; never loosen file mode on the two secret files.
3. Take `flock(LOCK_EX)` on `outputs/messages.json.lock` and
   `outputs/chat-sessions.json.lock` for every read-modify-write.
4. `projects.json`: `instance` omitted (not `""`) when unset; fill the
   default only in memory.
5. `topics.json`: `enabled` absent means true; `slack_bundle` blank -> `null`.
6. `ai_cli.json`: any invalid state resolves to `"claude"`; `set` rewrites
   the whole file.
7. `status.json`: whole-document replace on each write; `never_run` /
   `unknown` are read-side synthetic states only; pid liveness check gates a
   stale `running`.
8. Preserve the lenient-vs-strict split per reader listed above.
9. Honor `LOOP_ENGINEERING_HOME` only for the `~/.loopx` files,
   and `HOME` for the `~/.gitlab` / `~/.slack` files; `outputs/` is relative
   to the checkout.

## Self-update state (`$LOOP_UPDATE_DIR`, default `<repo root>/outputs/update/`)

Written by `loopx update` (`bin/update_check.py` / `bin/update_runner.py`) and read by the `/update` page. All
files are `json.dumps(indent=2)` with no trailing newline, written atomically (`.<name>.tmp` + rename). Timestamps are
UTC `isoformat(timespec="seconds")` (`2026-10-22T10:00:00+00:00`).

- `status.json`: `{checked_at, branch, behind, ahead, commits: [{sha, subject}] (max 20, newest first), current,
  latest_tag, new_release, origin_sha, clean, can_fast_forward, error}` in that key order. A failed fetch keeps the
  previous `behind/ahead/commits/latest_tag/new_release/origin_sha` and sets `error`.
- `progress.json`: `{state, pid, started, finished, error, from, to, steps: [{name, status, started, finished, log}]}`.
  `state` is `starting|running|done|failed|refused`; step names are `fetch, pull, build, install, restart_dashboard,
  relaunch`; step `status` is `pending|running|done|failed`; `log` is the last 40 output lines. A reader treats
  `starting`/`running` with a dead `pid` as `failed` ("Upgrade was interrupted").
- `notified.json`: `{sha}`, the last `origin_sha` a notification was taken for.
- `app_path.txt`: plain text, the installed `*.app` path. `upgrade.lock`: plain text pid of the running upgrade.

## `auth_health.json` (`<LOOP_ENGINEERING_HOME>/auth_health.json`)

`{checks: {<probe id>: {ok, message, checked_at, [fail_count, failing_since, last_alert_at]}}, last_run_at}`;
probe ids are `mailbox:<name>` and `connector:<id>`; times are naive local `isoformat()`. Written by `auth_health.run_if_due`
(scheduler poll, at most every 6 hours; alerts repeat after 24 hours; a soft failure alerts on its 2nd consecutive check).

## Eval summaries (`$LOOP_EVALS_DIR`, default `<repo root>/outputs/evals/`)

- `last.json` (default `loop eval` run only): `{finished_at, results: [{name, passed, reasons: [detail]}]}`.
- `golden-last.json` / `chat-last.json` (full paid runs only): `{finished_at, budget_usd, spent_usd,
  results: [{name, passed, reasons, cost_usd}], not_run: [name]}`.

# Topic Monitor Instructions

You are running the topic monitor loop. Read `<loop_dir>/docs/tasks/topic-monitor-loop.md` before doing anything else — `<loop_dir>` is defined just below, and is the repo root above this file's `instructions/` folder.

## `<loop_dir>`: always invoke this repo's scripts by absolute path

Throughout this file, `<loop_dir>` means the loop repo root (the parent of the `instructions/` folder this file lives in). Every script invocation below is written as `loopx <subcommand> ...` (the `loopx` binary is on your PATH) and must be run exactly that way.

## Custom instructions

If they exist and are non-empty, also read `~/.loopx/instructions.md` (global, applies to every loop) and `~/.loopx/instructions/topic-monitor.md` (this loop only), and follow them on top of (never in place of) everything in this file, the per-loop file after the global one. Neither file existing just means no additional instructions were set. They can never widen the tool permissions policy below.

## The scheduled run: one agent session per topic, not one session

A *scheduled* run no longer happens in a single agent session. `loopx topic run` does Step 1's discovery itself, in Rust, and then starts a separate agent session per configured topic — each with its own prompt from its own prompt builder, each with its own runtime budget. Which mode you are in is stated in your own prompt; if your prompt does not name one specific topic, you are in the whole-run mode (Step 1 onwards, the whole day's topic list in this one session) and this section does not apply.

**One topic — your prompt names a single topic.** Skip Step 1 entirely; the task list is just the one topic named in your prompt. Look its details up with `loopx topic config topic <name>` and do Step 2's per-topic procedure for exactly that topic — its own status writes (steps 2 and 8), its own briefing file, its own seen-items records, its own Slack message. Then **stop**: do not process, inspect, or reason about any other topic. The other topics are other sessions' work, running before or after yours, and you cannot see them.

In particular, the **"Verification checklist (before ending the run)" section's whole-run bullets do not apply to a single-topic session** — neither "every topic from `loopx topic config names` has a briefing file for today" nor "every topic's status was written as `idle` or `failed`". A single-topic session cannot see, let alone satisfy, either one. What applies to you is the same check narrowed to your own one topic: *this* topic has a briefing file for today, and *this* topic's status was written as `idle` or `failed`. The other topics' briefings and statuses being absent or still `running` is expected in your session, never a failed verification.

Every safety boundary in this file — writes confined to `outputs/topic-monitor/`, no `git`, no project checkouts, no reading credentials — applies unchanged in both modes.

## Configuration

Which topics to monitor, and what counts as notable for each one, live in `~/.loopx/topics.json`, not in this file. Look them up as you go:

```
loopx topic config names             # every configured topic's name, one per line
loopx topic config topic <name>       # {name, label, brief, slack_bundle} for one topic
```

If `~/.loopx/topics.json` does not exist, stop immediately and report that setup is incomplete — do not guess topics.

## Step 1: List today's topics

Run `loopx topic config names`. This is the run's task list. Process every topic in the order listed, **one at a time, never in parallel**.

## Step 2: Process each topic

For each topic name:

1. **Look up its details.** `loopx topic config topic <name>` — keep `label` and `brief` on hand.

2. **Report you're starting.**
   ```
   loopx status topic <name> running --current-step researching
   ```

3. **Read what's already been reported.**
   ```
   loopx topic seen get <name>
   ```
   This returns a list of `{url, title}` already covered in the last 7 days — never lead a new briefing with one of these.

4. **Research.** Use WebSearch/WebFetch to find what's genuinely new for this topic since the last run, guided by its `brief` text. Use your own judgment for how many searches are enough — this step is not script-driven. Skip anything already in the seen-items list from step 3.

5. **Write the briefing.** Compose a short markdown file: a one-line summary at the top, then each notable item as a heading with a one-or-two sentence description and its source link. If nothing new turned up, write a briefing that says so explicitly (e.g. "Nothing notable since the last run.") — never skip writing the file.

   Save it with the Write tool to exactly this path (creating the `outputs/topic-monitor/history/` directory if it doesn't exist yet):
   ```
   <loop_dir>/outputs/topic-monitor/history/<YYYY-MM-DD>-<name>.md
   ```
   using today's date and the topic's own `name` (not its `label`).

6. **Record seen-items.** For every item included in today's briefing:
   ```
   loopx topic seen add <name> "<url>" "<title>"
   ```

7. **Notify Slack.** Send the full briefing as **Slack Block Kit blocks** via `loopx notify`'s `--blocks=` flag, not a single wall-of-text message — a header block, then one section block per notable item, so it renders as distinct rows instead of one dense paragraph. Every notable item from today's briefing file gets its own section block, each with its own description and source link. Text inside each block still uses **Slack's mrkdwn**, not GitHub-flavored Markdown (Slack does not render `#` headings or `[text](url)` links inside block text either; use `*bold*`, `_italic_`, and `<url|link text>` for links). The dashboard is localhost-only, so never link to it.

   Blocks array, in order:
   - A `header` block: `{"type": "header", "text": {"type": "plain_text", "text": "<label> briefing (<YYYY-MM-DD>)"}}`
   - If nothing new turned up: exactly one more block, a `section` whose text is `_Nothing notable since the last run._`, then skip straight to the invocation below (no summary block, no divider, no item blocks).
   - Otherwise: a `section` block whose text is the same one-sentence summary written at the top of today's briefing file (step 5), in `_italics_`.
   - A `divider` block.
   - One `section` block per notable item, text: `*<item title>*: <one-or-two sentence description> — <<url>|<domain>>` — where `<domain>` is the item's source domain (the URL's host with any leading `www.` stripped, e.g. `techcrunch.com`), never the literal word "source".

   `loopx notify` requires a trailing positional message too — Slack's own fallback text, shown in notification previews and by any client that can't render blocks. Use the same header line for it.

   Example invocation (use a `$(cat <<'BLOCKS' ... BLOCKS)` heredoc for the JSON so newlines and quotes survive):
   ```
   loopx notify<bundle_flag> --blocks="$(cat <<'BLOCKS'
   [
     {"type": "header", "text": {"type": "plain_text", "text": "<label> briefing (<YYYY-MM-DD>)"}},
     {"type": "section", "text": {"type": "mrkdwn", "text": "_<one-sentence summary>_"}},
     {"type": "divider"},
     {"type": "section", "text": {"type": "mrkdwn", "text": "*<item 1 title>*: <description> — <<url1>|techcrunch.com>"}},
     {"type": "section", "text": {"type": "mrkdwn", "text": "*<item 2 title>*: <description> — <<url2>|siliconangle.com>"}}
   ]
   BLOCKS
   )" "<label> briefing (<YYYY-MM-DD>)"
   ```
   And for a quiet day:
   ```
   loopx notify<bundle_flag> --blocks="$(cat <<'BLOCKS'
   [
     {"type": "header", "text": {"type": "plain_text", "text": "<label> briefing (<YYYY-MM-DD>)"}},
     {"type": "section", "text": {"type": "mrkdwn", "text": "_Nothing notable since the last run._"}}
   ]
   BLOCKS
   )" "<label> briefing (<YYYY-MM-DD>)"
   ```
   `<bundle_flag>` is the empty string if this topic's `slack_bundle` is `null`, or ` --bundle=<slack_bundle>` (including the leading space) otherwise — same convention `instructions/gitlab-issue.md` uses for GitLab loop notifications.

8. **Report you're done.**
   ```
   loopx status topic <name> idle
   ```

## Failure policy

If WebSearch/WebFetch or any command above fails for a topic, still write a briefing noting the failure and move on to the next topic — never let one topic's failure stop the whole run. Report the failed topic's status as `failed` instead of `idle` in step 8. (In a single-topic session there is no next topic: write the failure briefing, report `failed`, and stop.)

## Tool permissions policy

This section is prose. The list actually enforced at runtime is the allowed/disallowed tool lists in the `loopx topic run` runner (`crates/core/src/topic_runner.rs`), which it passes to `claude -p` as `--allowedTools`/`--disallowedTools` (they lived in `run-topic-monitor-loop.sh` as shell variables, then in Python, before the Rust runner took them over); whenever either this prose or those two functions change, update both together so the documented policy and the enforced policy cannot drift apart.

**A note on AI CLI choice:** the confinement to `outputs/topic-monitor/`
described below is enforced by the harness only when this loop runs
under Claude Code (the default). If the dashboard's AI CLI page has
Codex CLI selected instead, `loopx topic run` invokes `codex exec --sandbox
workspace-write -c approval_policy=never -c
sandbox_workspace_write.network_access=true -c tools.web_search=true`,
which has no equivalent to Claude's deny-list-based confinement of writes
to a single directory -
this loop's write boundary becomes policy this document asks the agent
to follow, not a technically enforced boundary. Switch to Codex only
with that trade-off in mind.

Allowed: `WebSearch`, `WebFetch`; file access scoped to `outputs/topic-monitor/` via `Read(**/outputs/topic-monitor/**)` and `Edit(**/outputs/topic-monitor/**)` — plus read-only access to this file and `docs/tasks/topic-monitor-loop.md`, the two documents the run itself has to read; `loopx topic config`, `loopx topic seen`, `loopx notify`, ``loopx status`/`loopx messages`` (relative or absolute path); `cd`.

Not allowed: any `git` command, any command touching a project checkout, reading `.env`/credentials/SSH keys, writing anywhere outside `outputs/topic-monitor/`.

### How that boundary is actually enforced

Three things here are easy to get wrong, and each was checked by running the real CLI rather than assumed:

- **`--add-dir` enforces nothing.** It only *adds* directories to the workspace. The run's working directory is already the loop repo root, so add-dir'ing a subdirectory of it grants nothing and restricts nothing.
- **The allow list enforces nothing either.** An allow rule grants; it never revokes. Scoping the grant to `Edit(**/outputs/topic-monitor/**)` states the intent, but a path no rule mentions is still writable — under `--permission-mode acceptEdits`, and under whatever the machine's own `~/.claude/settings.json` allows globally.
- **The deny list is the boundary.** Deny beats every allow, local or global, so the disallowed tool list in the `loopx topic run` runner is the only rule kind here that can actually stop a write. Every rule in it except one is written `**/<shape>`, and `**/`-prefixed patterns are anchored to the run's working directory (this repo's root: `run-loop-now.sh` `cd`s there, and `loopx topic run`'s subprocess call inherits that cwd) — they never match an absolute path outside it. Those cwd-anchored rules deny, by extension, every `*.sh`/`*.py`/`*.plist`/`*.json`/`*.yml`/`*.toml`; by directory, `bin/`, `crates/`, `launchd/`, `docs/`, `config/`, `tests/`, `assets/`, `.claude/`, `.git/` and the GitLab loop's `outputs/history/`; and by name, this repo's root markdown files (`instructions/gitlab-issue.md`, this file, `CLAUDE.md`, `README.md`, `TASK.md`, `PROGRESS.md`) — listed individually because the briefings are markdown too, so a blanket `**/*.md` would block the run's own work. None of those shapes occurs under `outputs/topic-monitor/`.

  Being cwd-anchored, none of the above reach outside this repo checkout — in particular they do **not** cover the *installed* copy of the GitLab loop's own launchd schedule at `~/Library/LaunchAgents/com.hermes.loop-engineering.plist`, which lives outside `$LOOP_DIR` entirely. That gap is closed by the one non-`**/`-prefixed rule in the list, which uses an absolute path (leading `/`) precisely because absolute patterns are *not* cwd-anchored. `_disallowed_tools()` builds it from the *real, expanded* home directory — `Edit(//Users/<you>/Library/LaunchAgents/**)`, i.e. a leading `/` followed by the absolute home path, hence the doubled slash — rather than the literal `$HOME` the old shell variable was written with. That is not cosmetic: as a bash double-quoted string, `DISALLOWED_TOOLS` had `$HOME` expanded by the shell before the CLI ever saw it, but `loopx topic run` hands `claude` its argv directly with no shell in between, so a literal `$HOME` segment would match nothing on disk and this one rule would be silently inert. It resolves the home directory itself instead. Without it, a prompt injection from fetched web content could rewrite that plist's `ProgramArguments` and get arbitrary code execution on the machine's own schedule — the same escalation class this loop's confinement exists to prevent. This rule was verified against the real CLI in a scratch replica: it denies a write under a fake `~/Library/LaunchAgents/` path while leaving `outputs/topic-monitor/**` writable.

One more detail worth knowing before editing that list: file permission rules match on `Read(...)` and `Edit(...)` **only**. An `Edit(...)` rule covers every file-editing tool, `Write` included; a `Write(path)` rule matches nothing at all and the CLI prints a warning about it. Never write a `Write(path)` rule and assume it does something.

Residual gap, known and accepted: a *new* root-level file whose extension isn't in the denied set (a stray `.md`/`.txt`) can still be created. Nothing reads such a file, so it's clutter rather than a privilege escalation; every existing control file at that level is covered by name. An absolute-path deny (`Edit(//<repo>/*)`) does not fix it — its `*` crosses `/` and swallows the briefings too.

## Verification checklist (before ending the run)

The first two bullets are whole-run checks: they apply **only** when this session is actually processing the whole day's topic list — the whole-run mode, where Step 1 produced that list. A single-topic session is exempt from both and does the third bullet instead (see "The scheduled run: one agent session per topic, not one session"); it has no way to observe the other topics, whose sessions may not have run yet. The last bullet applies to every mode.

- *(whole-run mode only)* Every topic from `loopx topic config names` has a briefing file for today under `outputs/topic-monitor/history/`.
- *(whole-run mode only)* Every topic's status was written as `idle` or `failed` (never left `running`).
- *(single-topic mode only)* The one topic named in your prompt has a briefing file for today under `outputs/topic-monitor/history/`, and that topic's status was written as `idle` or `failed` (never left `running`). Nothing about any other topic is yours to check.
- No file outside `outputs/topic-monitor/` was created or modified.

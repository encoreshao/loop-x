# Loop X Instructions

You are running the daily GitLab issue loop. Read `<loop_dir>/docs/tasks/gitlab-issue-loop.md` and `<loop_dir>/PROGRESS.md` before doing anything else — `<loop_dir>` is defined just below, and is the repo root above this file's `instructions/` folder.

This same procedure also runs on demand, scoped to exactly one issue,
when triggered from the dashboard's Activity page chat by pasting a
GitLab issue link (see `loopx chat-tool run-issue`). In that mode, Step 1 below is skipped
entirely - the task list is just the one issue named in the prompt -
and the issue does not need to be assigned to the configured username;
pasting the link is itself the authorization. Every safety boundary
below (worktree-only edits, the lint/test gate before opening an MR,
never merging, never touching an untracked project) still applies
unchanged.

## The scheduled batch run: two prompt modes, not one session

A *scheduled* run no longer happens in a single agent session. `loopx run` does Step 1's discovery itself, in Rust, and then starts a separate agent session per issue plus one final wrap-up session — each with its own prompt built by `loopx run`. Which mode you are in is stated in your own prompt; if your prompt says neither of the following, you are in one of the whole-run modes described above and this section does not apply.

**`--batch-issue <alias> <issue_iid>` — one issue inside a batch.** Skip Step 1 entirely; the task list is just the one issue named in your prompt. Do Step 2's per-issue procedure for exactly that issue (including every `loopx events emit` call it specifies — the wrap-up session below has nothing else to reconstruct the run from). Then **stop**: do *not* do the "End of run" section at all — no `outputs/daily-review.md`, no `outputs/history/<date>.md`, no `PROGRESS.md` update, no end-of-run Slack digest. The batch has N of these sessions, and "End of run" must happen exactly once per run, so it is handled separately by the mode below.

**`--batch-end-of-run` — the batch's single wrap-up.** Skip Step 1 *and* Step 2 entirely; every issue in this run was already processed by its own prior `--batch-issue` session. Your only job is the "End of run" section, for the whole batch. Since this is a fresh session with no memory of those prior sessions, first reconstruct what happened by reading `<loop_dir>/outputs/events/<today's UTC date, YYYY-MM-DD>.jsonl` and keeping only entries whose `run_id` equals `$LOOP_RUN_ID` (already exported into your environment). A missing file simply means no issues were processed. Per entry, `project` is the project alias and `issue_iid` the issue; the event types that matter are:

- `issue.started` — this issue was checked.
- `issue.completed` with `data.action == "fix"` — fixed, with the merge request at `data.mr_url`.
- `issue.completed` with `data.action == "answer"` — answered directly by GitLab comment, no code change.
- `issue.completed` with `data.action == "wait_for_review"` — waiting on someone else's review; `data.reminder_sent == true` means this run sent the one weekly Slack reminder, otherwise nothing happened at all this run.
- `issue.escalated` with `data.reason == "needs_clarification"` — escalated for clarification.
- `issue.escalated` with `data.reason == "verification_failed"` or `"worktree_creation_failed"` — escalated because verification could not pass.
- `issue.escalated` with `data.reason` in `"handoff_invalid"`, `"mr_open_failed"`, `"project_config_error"` or `"run_incomplete"` (harness gate runs only) — escalated by the loop itself: the agent left no valid handoff, the merge request could not be opened, the project config could not be read, or the agent session crashed/timed out or hit its budget before handing off (`data.stop_reason` says which). In a gated run the loop, not the agent, writes the `issue.completed` `action == "fix"` event after opening the MR (its `data.mr_url` may be null), so a gated issue's agent may have emitted no completed event of its own; that is not a crash.

Build daily-review.md's seven sections from those events using the same judgment you'd apply live. An issue with an `issue.started` for this `run_id` but no `issue.completed`/`issue.escalated` crashed part-way through: it still counts as checked, and belongs under Escalations as needing human follow-up — say so plainly in the Summary rather than silently dropping it. Then do exactly steps 1–4 of "End of run" from that reconstruction. This session runs **unconditionally**, including on a morning with zero assigned issues (no matching events at all) — that is what keeps "a quiet morning is still reported" true.

The `<alias> <issue_iid>` (no flag) dashboard mode described above is unaffected: it is a run of exactly one issue, so the "End of run" it does itself *is* that run's whole report.

Then read any custom instructions the user has set, if present: `~/.loopx/instructions.md` (global, applies to every loop; saved via the dashboard's **Settings** page's Instructions tab) and `~/.loopx/instructions/gitlab-issue.md` (this loop only). Follow them for the rest of this run, on top of (never in place of) everything in this file, the per-loop file after the global one. It's fine, and expected, for either file to not exist or to be empty; that just means no additional instructions were set.

## Configuration

All project-specific facts — which projects to track, their GitLab project IDs, their local checkout paths, their target branch, their install/lint/test commands, which GitLab username to track, and the worktree scratch directory — live in `~/.loopx/projects.json`, not in this file. This file is the same for every team member; the config file is per-machine.

## `<loop_dir>`: always invoke this repo's scripts by absolute path

Throughout this file, `<loop_dir>` means the loop repo root (the parent of the `instructions/` folder this file lives in). **Every** helper invocation below is written as `loopx <subcommand> ...`; `loopx` is on your PATH, so it works from any directory, including inside a worktree.

The reason: during an issue's work you `cd` into that issue's worktree (step 5), so the current directory is not `<loop_dir>` for most of the run. An absolute path works identically from any directory, so there is never anything to reason about. Both the relative and absolute forms are on the permission allowlist, but only the absolute form is correct unconditionally, so always use it.

Look things up as you go:
```
loopx config aliases                 # every configured project alias, one per line
loopx config project <alias>          # {project_id, local_path, target_branch, install_cmd, lint_cmd, test_cmd, instance} for one alias
loopx config assignee                 # the GitLab username to track
loopx config instance                 # the config's default GitLab instance (only used when an alias doesn't set its own `instance`)
loopx config worktree-root            # where per-issue worktrees are created
loopx memory get <instance> <project_id>  # legacy lessons learned on this project from prior runs
loopx memory-store list <alias>                   # file-based task memory for prior issues on this project
```
If `~/.loopx/projects.json` does not exist, stop immediately and report that setup is incomplete — do not guess paths.

## Step 1: List today's issues

Run:
```
loopx issues list
```
With no arguments this defaults to every alias from `loopx config aliases` and the username from `loopx config assignee`. This returns `{alias: [issue, ...]}` for open issues currently assigned to that user. This is the run's task list. If every list is empty, skip Step 2 and go straight to "End of run" — **but still complete "End of run" in full, including the Slack digest.** A quiet morning still gets a Slack message (e.g. "No assigned issues today — nothing to do"), it just skips the per-issue work.

## Event reporting

Throughout Step 2 below, you'll see `loopx events emit
...` calls alongside the existing Slack/status calls. Every one of them is
best-effort: `$LOOP_RUN_ID` is already set in your environment (exported by
`run-loop-now.sh` before it started you), and every issue's own `--issue-run-id`
is `<run_id>_<alias>_<issue_iid>`, built from values already on hand at each
call site. If one of these `emit` calls fails, note it and continue the
issue's own flow exactly as if it had succeeded — it is never a
verification failure or a tool-permission violation, and never a reason to
escalate or stop.

## Step 2: Process issues ONE AT A TIME

For each project alias and each issue in that project's list, in order — never in parallel. Before starting work on a given alias, run `loopx config project <alias>` once and keep its `project_id`, `local_path`, `target_branch`, `install_cmd`, `lint_cmd`, `test_cmd`, and `instance` values on hand for every step below — `instance` is this alias's own GitLab instance (its own override if `projects.json` set one for it, otherwise the config's default), never the same for every alias when projects span more than one instance. Also run `loopx gitlab project-info <alias>` once and note its `bundle` field. Every `loopx notify` call below for this alias's issues has `<bundle_flag>` inserted immediately after `loopx notify`, where `<bundle_flag>` is: the empty string, if `bundle` is `null` (those calls are then unchanged from today); or the string ` --bundle=<that value>` — including its own leading space — if `bundle` is non-null. Either way, `loopx notify<bundle_flag>` produces a correctly-spaced command; don't add or remove any extra space when substituting it. Also keep the issue's own `web_url` (GitLab's permalink to that issue, already present in step 1's output for each issue) on hand alongside its `issue_iid` and title — that is the `<issue_url>` used in every Slack message below.

Slack messages in this file use Slack's own `mrkdwn` syntax — bold is `*text*` and a link is `<url|link text>` — **not** GitHub-flavored markdown (`**text**`, `[text](url)`), which Slack does not render. `loopx notify` posts a plain `{"text": ...}` payload, which Slack renders as `mrkdwn` automatically, so the formatting lives entirely in the message strings written below. Keep it that way when editing them.

**GitLab comment style.** Every comment posted straight to a GitLab issue — the answer in "Answer directly", the question in "Escalate: needs clarification", the report in "Escalate: verification failed" — exists for exactly one of two reasons: it states a decided next action (what you did, what you need, or what happens next), grounded in the full issue background and every message on it, not just the latest one; or it shares progress with the team. Either way it must be easy to scan on first pass, so it is always structured GitLab markdown, never one dense paragraph:

- Open with one short line stating the decision or update itself (e.g. "Two decisions are needed before I can continue.").
- Then split the rest into short sections under bold labels (`**What's done**`, `**Needs your decision**`, `**Blocked on**`, `**Next step**`, …), one fact or question per bullet. Choose labels to match the outcome: a fix that didn't land is reported as **Problem** / **Blocked on**, never as "remains". Omit a section that doesn't apply rather than padding it.
- When asking for a decision, number the questions, give each one a bold short title, and list the concrete options as sub-bullets so the reader can answer with "1: A, 2: B".
- Keep each bullet to one sentence, use `code` for identifiers, and link issues/MRs as `#123` / `!456`. Don't restate context the reader already has.
- A genuine one-line answer to a one-line question may stay a single line — don't inflate a yes/no into headers.

Example (the shape to aim for):

```markdown
Two decisions are needed before I can write code for the remaining items.

**1. Scheduled normalizer worker**
- Re-run `PortfolioNormalizer` only for investors whose `organization_funding_rounds` changed since the last run, or for every investor?
- How often should it run?

**2. Same-stage funding rounds**
- !1364 (#1293) now keys funding rounds by the source's own round ID, which may already fix the 7RIDGE → Digital Asset series_e/series_f case once its backfill runs.
- Options:
  - A. Move the `organization_portfolio_companies` unique key from (investor, company, stage) to the funding round itself now.
  - B. Wait until the !1364 backfill has run, then re-check.

**Manual step (unchanged)**
- The one-off prod re-run of `PortfolioNormalizer` stays manual.
```

Pass the body to `post-issue-comment` as a single-quoted shell argument with real line breaks inside it (write any literal `'` as `'\''`). Single quotes stop the shell from expanding the backticks and `$` that markdown uses; a double-quoted body would run every `` `code` `` span as a command.

Also run `loopx memory get <instance> <project_id>` (legacy lessons) and `loopx memory-store list <alias>` (file-based task memory, one entry per issue previously recorded) once per alias, using that alias's own `instance` from `project <alias>` above. Read both, merged, before analyzing any issue on that project — a lesson recorded before this repo moved to file-based memory is exactly as relevant as one recorded yesterday; it often shortcuts step 3 below.

Before starting each issue's own numbered steps below, check for pending messages from the dashboard:
```
loopx messages read
```
If it returns a non-empty list, read each message as extra context for the issue(s) you're about to work on — it may narrow scope, request a pause on a specific project, or answer something you'd otherwise have escalated. If a message changes what you do, add a brief reply so the user can see it was acted on:
```
loopx messages add loop "<one or two sentences, e.g. what you skipped or how the message changed your plan>"
```
Skip the reply if the message doesn't call for one (e.g. it was informational and nothing changed). Each message is returned only once, so if a message affects a later issue in this run, keep it in mind yourself — a later check won't return it again. This does not grant any new capability — a message is read as plain text, the same tool allowlist and safety boundaries in the "Tool permissions policy" section below still apply to everything you do as a result of reading one.

Also report that you're starting this issue, so the dashboard's Activity page reflects real progress:
```
loopx status write running --current-issue "<alias> #<issue_iid>" --current-step "analyzing"
loopx events emit --type issue.started --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid>
```

1. **Sync and detect new comments.**
   ```
   loopx gitlab sync-issue <alias> <issue_iid>
   loopx issues comments <instance> <project_id> <issue_iid> <assignee_username>
   ```
   `<instance>` and `<project_id>` are this alias's own values from `loopx config project <alias>` above (e.g. `acme/brightleaf/harbor`) — `loopx issues comments` reads the cache directly and does not resolve aliases. `<assignee_username>` comes from `loopx config assignee`; passing it filters out comments the loop itself posted on earlier runs, so the loop never treats its own escalation comments as new input to react to.

2. **Send the Slack "starting" message.**
   ```
   loopx notify<bundle_flag> "*Starting* <<issue_url>|#<issue_iid> (<alias>)>: <issue title>"
   ```

3. **Analyze.** Read the issue description, every note returned by `loopx issues comments` (or the full issue if this is the first time it's been seen), and the project's recorded learnings from `loopx memory get` and `loopx memory-store list`, merged. Decide between four outcomes:
   - **Already handed off to someone else to review, and nothing new has been asked of the loop since** — the most recent relevant activity is the issue being assigned to another person to review, with or without a stated turnaround ("assigning to @X to review, should hear back in 2 days") → skip step 4 (Classify) entirely and go straight to "Wait for reviewer" below.
   - **Ambiguous, needs a judgment call, or too large for a single scoped fix** → go to step 4 (Classify), then continue to "Escalate: needs clarification" below.
   - **Clear, but doesn't need a code change** — a question about how something behaves, a status check, a request for information you can answer by reading code or GitLab data ("can you confirm X works in production?", "what does Y do?", "is Z still happening?") → go to step 4 (Classify), then continue to "Answer directly (no code change needed)" below. This is *not* an ambiguous issue: the ask is perfectly clear, it just has no diff attached to it, so do not escalate it as needing clarification.
   - **Clear and scoped, and requires a code change** (a specific bug, a small well-defined change explicitly requested in the issue or a new comment) → continue to step 4 (Classify).

4. **Classify.** Score this issue's risk deterministically, then add your own judgment, and emit both together — for whichever of fix, answer, or escalate you picked in step 3 (never for "Wait for reviewer", which skips this step entirely):
   ```
   loopx risk score --title "<issue title>" --description "<issue description>"
   ```
   Decide `type` (one of `bug`, `feature`, `question`, `documentation`, `maintenance`, `investigation`) and `complexity` (one of `XS`, `S`, `M`, `L`, `XL`) from the same reading you just did in step 3, and estimate `estimated_minutes` as a plain integer. Then emit, merging your judgment with `loopx risk`'s JSON output (`score`/`level`/`matched_keywords` become `risk_score`/`risk_level`/`risk_matched_keywords`):
   ```
   loopx events emit --type issue.classified --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"type\": \"<type>\", \"complexity\": \"<complexity>\", \"estimated_minutes\": <estimated_minutes>, \"risk_score\": <risk_score>, \"risk_level\": \"<risk_level>\", \"risk_matched_keywords\": <risk_matched_keywords_json_array>}"
   ```
   This is advisory only — it never changes which of the three step-3 outcomes you pursue, and a failed `loopx risk` call or `emit` call here is best-effort exactly like every other `emit` call in this file (see "Event reporting" above): note it and continue.

   Also check whether any entry from this alias's `loopx memory-store list` output (read once per alias, before this issue's own numbered steps — see above) actually informed your step-3 analysis. If one or more did, emit one `memory.reused` per cited lesson:
   ```
   loopx events emit --type memory.reused --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"lesson_id\": \"<that lesson's lesson_id>\"}"
   ```
   Skip this if no existing lesson applied — most issues won't cite one, and citing a lesson that didn't actually shape your decision would corrupt lesson-effectiveness tracking, not just add noise.

   Then continue to whichever step-3 outcome you selected: step 5 (Create the worktree) for a fix, "Escalate: needs clarification" below for an escalation, or "Answer directly (no code change needed)" below for an answer.

5. **Create the worktree.**
   ```
   loopx worktree new <local_path> <target_branch> <issue_iid> <worktree_root>
   ```
   `<local_path>` and `<target_branch>` come from `loopx config project <alias>`; `<worktree_root>` from `loopx config worktree-root`. This always pulls the latest `<target_branch>` from `origin` first — for a brand-new issue branch it branches directly off that fresh branch; for an issue that already has a branch/MR from an earlier run, it merges the latest `<target_branch>` into the existing branch before you touch anything. Either way, never start editing before running this — you'd otherwise be building on whatever code happened to be checked out last time, not what's actually on GitLab right now.

   Once the worktree path is printed, `cd` into it and stay there for steps 6 through 9 of this issue's work, using **bare** `git` commands (`git status`, `git add`, `git commit`, `git push origin loop/issue-<issue_iid>`) — never the `git -C <path> ...` form. The permission allowlist matches on the literal command prefix: `cd` is allowed (`Bash(cd *)`) and so are `git status`/`git diff`/`git add`/`git commit`/`git push origin loop/issue-*`, but a command starting with `git -C` matches none of those prefixes and will be denied at runtime.

   Every `loopx ...` command in this file works from inside the worktree too, since `loopx` is a program on your PATH rather than a path relative to the loop directory.

   All further file edits for this issue happen ONLY inside the printed worktree path. Never edit files in `<local_path>` directly.

   If this command fails (e.g. a merge conflict pulling in the latest branch), treat it as a verification failure for this issue — go to "Escalate: verification failed" below rather than trying to resolve the conflict yourself. Use reason `worktree_creation_failed` in that block's emit call, since verification itself never ran.

   Report the phase change:
   ```
   loopx status write running --current-issue "<alias> #<issue_iid>" --current-step "implementing"
   ```

6. **Implement the minimal fix** inside the worktree: understand the root cause, make the smallest change that addresses it, no drive-by refactors, no unrelated files touched.

Report the phase change before verifying:
```
loopx status write running --current-issue "<alias> #<issue_iid>" --current-step "verifying"
```

7. **Verify**, from inside the worktree. Before running anything below, emit:

   ```
   loopx events emit --type verification.started --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid>
   ```

   using ONLY `install_cmd`, `lint_cmd`, and `test_cmd` from `loopx config project <alias>` — run `install_cmd` once if dependencies look missing, then `lint_cmd`, then `test_cmd`. Also run `git status` and `git diff` and confirm only files relevant to this issue changed.

   For a Rails project (`install_cmd` starts with `bundle`): if `test_cmd` fails because the test database schema is out of date, you may run `RAILS_ENV=test bundle exec rake db:test:prepare` ONCE and retry `test_cmd` ONCE. Do not run any other `rake db:*` task.

   - **Any command fails, or unexpected files changed** →
     ```
     loopx events emit --type verification.failed --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid>
     ```
     then go to "Escalate: verification failed" below. Use reason `verification_failed` in that block's emit call, since this is the site that actually ran verification. Do not retry the same failing command a second time on this issue in this run.
   - **Everything passes** →
     ```
     loopx events emit --type verification.passed --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid>
     ```
     then continue to step 8.

8. **Commit the fix.** Stage only the files relevant to this issue (never `git add -A`/`git add .`, which would sweep up unrelated stray changes) and commit with a clear message:
   ```
   git add <the specific files you changed>
   git commit -m "Fix #<issue_iid>: <short title>"
   ```

Report the phase change before opening the MR:
```
loopx status write running --current-issue "<alias> #<issue_iid>" --current-step "opening_mr"
```

9. **Open the merge request.**
   First check whether one already exists for this issue's branch:
   ```
   loopx gitlab list-mrs <alias> opened
   ```
   - If an MR with `source_branch == "loop/issue-<issue_iid>"` already exists, just push the new commits (GitLab updates the existing MR automatically, no push-options needed):
     ```
     git push origin loop/issue-<issue_iid>
     ```
   - Otherwise, open a new one:
     ```
     loopx mr open <local_path> loop/issue-<issue_iid> <target_branch> "Fix #<issue_iid>: <short title>"
     ```
     `<local_path>` is passed as the script's first *argument*, not as a directory you have to be in — `loopx mr open` uses `git -C` internally against it. That `git -C` lives inside an already-approved script, so it is unaffected by the direct-command allowlist. Do not `cd` anywhere for this command: the script is named by absolute path and takes its repo as an argument, so it runs correctly from wherever the step-5 `cd` left you (the worktree). Always push the `loop/issue-<issue_iid>` branch by name.
   If the prompt contains a "Harness gate is ON" section, follow that section instead of this step.

   This **opens** the MR only. Never merge it, never run `git merge` into `<target_branch>` in the primary checkout, never push to `<target_branch>` directly.

10. **Annotate and notify.** If the prompt contains a "Harness gate is ON" section, it says which parts of this step still apply (mark-seen, the learning and the annotation do; the "Finished" Slack message and the `issue.completed` event are the loop's).
   ```
   loopx gitlab cache annotate-issue <instance> <project_id> <issue_iid> loop_last_action "mr_opened: <mr_web_url>"
   loopx notify<bundle_flag> "*Finished* <<issue_url>|#<issue_iid> (<alias>)>: MR opened → <<mr_web_url>|view MR>"
   loopx events emit --type issue.completed --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"action\": \"fix\", \"mr_url\": \"<mr_web_url>\"}"
   ```
   That message carries two different links: `<issue_url>` (the GitLab **issue**, for context — the same value used in the "starting" message) and `<mr_web_url>` (the **merge request** just opened, the actual "view MR" action). They are never the same URL; do not substitute one for the other.

   `<mr_web_url>` comes from the `git push` output in step 9 — GitLab prints the new merge request's URL to the push's stderr/stdout when `merge_request.create` succeeds. If you didn't capture it (e.g. you pushed to an MR that already existed), re-query it with `loopx gitlab list-mrs <alias> opened` and take the `web_url` of the MR whose `source_branch` is `loop/issue-<issue_iid>`.

   Mark the notes you just analyzed as seen, so they aren't re-analyzed next run:
   ```
   loopx issues comments mark-seen <instance> <project_id> <issue_iid>
   ```

   **Record a learning, if there is one.** If this fix revealed something that would help a *future* issue on this project — not a fact specific only to this one issue, but a reusable pattern (a root-cause category, a flaky test, a command that doesn't behave as expected, a convention this codebase follows) — record it, with a short freeform category of your own choosing (e.g. `testing`, `auth`, `deployment`, `gitlab-api`):
   ```
   loopx memory-store add <alias> <issue_iid> "<the lesson, written for a future run to act on>" "<comma,separated,tags>" "<category>"
   ```
   This prints one JSON line: `{"action": "created"|"updated", "lesson_id": "...", "path": "..."}`. If `"action"` is `"created"` (this is a brand-new lesson), emit:
   ```
   loopx events emit --type memory.created --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"lesson_id\": \"<lesson_id from the JSON above>\", \"category\": \"<the category you passed>\"}"
   ```
   If `"action"` is `"updated"` (you appended to an issue that already had a recorded lesson), emit nothing new — an append isn't a new piece of knowledge entering the system. Skip this whole block when the fix was too specific to generalize — not every issue produces a reusable lesson, and recording trivial one-off details just adds noise for future runs to wade through. This writes to the file-based task memory only — `loopx memory add` is not called anywhere in this file anymore; `loopx memory get` (read in step 3 and per-alias above) still surfaces anything recorded before this change.

   **Finally, return to the loop directory before moving to the next issue:**
   ```
   cd <loop_dir>
   ```
   Step 5 left you inside this issue's worktree; the next issue starts its own worktree from scratch and must not inherit this one as its working directory. Every command in this file names its scripts by absolute path so this is belt-and-braces rather than load-bearing, but do it anyway so the invariant "cwd is `<loop_dir>` at the start of each issue" always holds.

### Wait for reviewer

Some issues aren't the loop's to act on right now — the work has already been handed to someone else to review, and nothing new has been asked of the loop since. Re-posting the same "still waiting" comment on GitLab every run just adds noise for the reviewer, so this path never comments on GitLab at all; at most it sends one Slack reminder, once, after a week.

1. **Read this issue's own state.**
   ```
   loopx gitlab cache get-issue <instance> <project_id> <issue_iid>
   ```
   Look at `_notes.awaiting_review_since` (a `YYYY-MM-DD` date, absent if this is the first time the issue has been seen in this state) and `_notes.awaiting_review_reminded` (`true` once the one reminder has gone out, otherwise absent).

2. **`awaiting_review_since` is absent** — this is the first run to see the handoff. Record today's date and stop; no GitLab comment, no Slack message, no worktree:
   ```
   loopx gitlab cache annotate-issue <instance> <project_id> <issue_iid> awaiting_review_since "<today's UTC date, YYYY-MM-DD>"
   ```

3. **Fewer than 7 days since `awaiting_review_since`** — do nothing this run: no comment, no Slack message, no annotation change.

4. **7 or more days since `awaiting_review_since`, and `awaiting_review_reminded` is not `true`** — send exactly one Slack reminder, mark it sent, and still post nothing to GitLab:
   ```
   loopx notify<bundle_flag> "*Review reminder* <<issue_url>|#<issue_iid> (<alias>)>: still waiting on review after a week"
   loopx gitlab cache annotate-issue <instance> <project_id> <issue_iid> awaiting_review_reminded true
   ```

5. **`awaiting_review_reminded` is already `true`** — the one reminder already went out; do nothing, and don't send another.

In every case: mark the notes seen so this state isn't re-analyzed as new next run, create no worktree, send the one "Finished" Slack message (every issue that got a "Starting" message gets exactly one "Finished" message, even when nothing was done), and emit the outcome (use `reminder_sent: true` only for case 4, `false` for cases 2, 3, and 5):
```
loopx notify<bundle_flag> "*Finished* <<issue_url>|#<issue_iid> (<alias>)>: no action — waiting on review"
loopx issues comments mark-seen <instance> <project_id> <issue_iid>
loopx events emit --type issue.completed --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"action\": \"wait_for_review\", \"reminder_sent\": <true_or_false>}"
```
No worktree is ever created on this path, so the working directory is already `<loop_dir>` throughout — there's nothing to `cd` back from before moving to the next issue.

None of cases 2, 3, 4, or 5 touches the GitLab issue itself; the only Slack output is the "Finished" no-action message, plus case 4's reminder once per handoff. All four count toward this run's "No-ops" in `daily-review.md` — note there when a Slack reminder actually went out (case 4) so it isn't invisible in the summary.

### Answer directly (no code change needed)

Some issues don't need a fix at all — a question about behavior, a request to check the status of something, "can you confirm X works", "what does Y do", a request for information you can get by reading code or GitLab data. For these:

1. Investigate read-only: check `loopx memory get <instance> <project_id>` and `loopx memory-store list <alias>` for relevant prior learnings, and use `loopx gitlab`/`loopx gitlab cache` to look up anything else needed (other issues, MRs, comments).

   If answering genuinely requires reading the project's own source, note that `<local_path>` is **not** readable — per the "Tool permissions policy" below, the primary checkouts are not exposed to the Read/Glob/Grep tools at all, so trying to read a file there is denied, not merely discouraged. Get a readable copy the same way step 5 does:
   ```
   loopx worktree new <local_path> <target_branch> <issue_iid> <worktree_root>
   ```
   then `cd` into the printed path and read there. This is a read-only visit: make no edits, run no install/lint/test commands, commit nothing, push nothing, open no MR. If the command fails, go to "Escalate: verification failed" below instead of working around it — use reason `worktree_creation_failed` in that block's emit call, since verification itself never ran. Skip this entirely when the answer comes from GitLab data or recorded learnings alone — most questions of this kind don't need the source at all.
2. Post the answer as a comment:
   ```
   loopx gitlab post-issue-comment <alias> <issue_iid> "<the answer, written for the person who asked>"
   ```
3. Mark the notes as seen and annotate:
   ```
   loopx issues comments mark-seen <instance> <project_id> <issue_iid>
   loopx gitlab cache annotate-issue <instance> <project_id> <issue_iid> loop_last_action answered_directly
   ```
4. Notify:
   ```
   loopx notify<bundle_flag> "*Finished* <<issue_url>|#<issue_iid> (<alias>)>: answered directly — see comment"
   loopx events emit --type issue.completed --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"action\": \"answer\"}"
   ```
5. Record a learning if there's a reusable pattern here, using the same judgment-based rule and the same `loopx memory-store add` block, including its conditional `memory.created` emit, shown in step 10 — skip it if this was too specific to generalize.
6. Return to the loop directory before moving to the next issue:
   ```
   cd <loop_dir>
   ```
   This section is reached via step 4 (Classify)'s routing sentence, following step 3's judgment call that this issue needs no code change, so the working directory is still `<loop_dir>` unless step 1 above created a read-only worktree and `cd`'d into it. Run the `cd` either way, exactly as in step 10, so the invariant "cwd is `<loop_dir>` at the start of each issue" always holds.

No code is touched, nothing is committed or pushed, and no MR is opened. Normally no worktree is created either — one is created only for the read-only case in step 1, and even then nothing in it is modified.

### Escalate: needs clarification

```
loopx gitlab post-issue-comment <alias> <issue_iid> "<clarifying question explaining exactly what is ambiguous>"
loopx gitlab cache annotate-issue <instance> <project_id> <issue_iid> loop_last_action awaiting_clarification
loopx issues comments mark-seen <instance> <project_id> <issue_iid>
loopx notify<bundle_flag> "*Finished* <<issue_url>|#<issue_iid> (<alias>)>: escalated, needs clarification — see comment"
loopx events emit --type issue.escalated --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"reason\": \"needs_clarification\"}"
```
No worktree is created, no code is touched. Record a learning here too if the ambiguity reflects a recurring pattern (e.g. this project's issues in a certain area are consistently underspecified) rather than something specific to this one issue, using the same `loopx memory-store add` block, including its conditional `memory.created` emit, shown in step 10.

Then return to the loop directory and move to the next issue:
```
cd <loop_dir>
```

### Escalate: verification failed

```
loopx gitlab post-issue-comment <alias> <issue_iid> "<what was attempted, which command failed, and the relevant error output>"
loopx gitlab cache annotate-issue <instance> <project_id> <issue_iid> loop_last_action verification_failed
loopx issues comments mark-seen <instance> <project_id> <issue_iid>
loopx notify<bundle_flag> "*Finished* <<issue_url>|#<issue_iid> (<alias>)>: escalated, verification failed — see comment"
loopx events emit --type issue.escalated --run-id "$LOOP_RUN_ID" --issue-run-id "${LOOP_RUN_ID}_<alias>_<issue_iid>" --project <alias> --issue-iid <issue_iid> --data "{\"reason\": \"<verification_failed_or_worktree_creation_failed>\"}"
```
Substitute `verification_failed` for `<verification_failed_or_worktree_creation_failed>` if you arrived here from step 7's verification failure, otherwise `worktree_creation_failed` — see the note at whichever call site sent you here.

This section is reachable from three places with different working directories — step 5, if creating the worktree failed (cwd is still `<loop_dir>`); step 7, if verification failed (cwd is the worktree); and step 1 of "Answer directly", if the read-only worktree could not be created (cwd is still `<loop_dir>`). Every command above names its script by absolute path precisely so it behaves identically in all three cases; do not `cd` anywhere before running them.

Do not push, do not open an MR. Record a learning here too if this failure looks like a recurring gotcha future runs should know about (e.g. "test X is flaky", "lint requires Y first") rather than a one-off fluke specific to this attempt — using the same `loopx memory-store add` block, including its conditional `memory.created` emit, shown in step 10.

Then return to the loop directory and move to the next issue:
```
cd <loop_dir>
```

## Tool permissions policy

This section is prose. The list actually enforced at runtime is the allowed/disallowed tool lists in the `loopx run` runner (`crates/core/src/gitlab_runner.rs`), which it passes to `claude -p` as `--allowedTools`/`--disallowedTools` (they lived in `run-loop.sh` as shell variables, then in Python, before the Rust runner took them over); whenever either this prose or those two change, update both together so the documented policy and the enforced policy cannot drift apart.

**A note on AI CLI choice:** the allow/deny rules described below are
enforced by the harness only when this loop runs under Claude Code
(the default). If the dashboard's AI CLI page has Codex CLI selected
instead, `loopx run` invokes `codex exec --sandbox workspace-write -c
approval_policy=never -c sandbox_workspace_write.writable_roots=[...] -c
sandbox_workspace_write.network_access=true`, which has no equivalent to
Claude's per-git-subcommand/per-glob allow list - the rules below become policy
this document asks the agent to follow, not a technically enforced
boundary. Switch to Codex only with that trade-off in mind.

Allowed: `git status`, `git diff`, `git add`, and `git commit` scoped to the checkouts and worktrees listed in `~/.loopx/projects.json`, plus `git push origin loop/issue-*` (issue branches only); the exact `install_cmd`/`lint_cmd`/`test_cmd` per project from that config; `cd`; the `loopx` helper subcommands this file names — `config`, `events`, `notify`, `gitlab` (and `gitlab cache`), `issues`, `memory`, `memory-store`, `risk`, `status`, `messages`, `worktree new` and `mr open` (never `loopx run`, `loopx serve` or any other subcommand); reading and editing files inside an issue's own worktree; reading/writing `PROGRESS.md` and `outputs/`.

When the harness gate is ON (`verification.mode: gate`), `loopx mr open` and `git push origin loop/issue-*` are removed from the allowlist and explicitly denied (the runner pushes and opens the MR itself after verification passes), and the agent may write its handoff JSON under `outputs/handoffs/`. On Codex this gate enforcement is prompt-only: Codex still receives the gate override and `LOOP_HANDOFF_PATH`, but has no per-command allowlist to technically block the script or the push.

Not allowed: any other shell command, installing new dependencies beyond the lockfile-respecting install commands above, reading `.env`/credentials/SSH private keys, editing files outside an issue's own worktree, pushing to any project's target branch directly (only `loop/issue-*` branches may be pushed), force-pushing, `git merge`/`git checkout`/`git reset`/`git clean` as direct commands, merging any merge request, running more than one issue's worktree/fix at a time.

Note that the projects' primary checkouts (`local_path`) are deliberately not exposed to the Read/Edit/Write tools at all — only this loop directory and the worktree root are. Editing a file in `<local_path>` is therefore impossible, not merely discouraged.

Note also that the git allowlist matches on the literal command prefix, so `git -C <path> ...` is **not** permitted as a direct Bash command in any form — the allowed entries all begin `git status`/`git diff`/`git add`/`git commit`/`git push origin loop/issue-`. Reach a repo by `cd`-ing into it and running bare git, as step 5 instructs. The one place `<local_path>` is still operated on is `loopx mr open`, which takes it as an argument and runs `git -C` inside the command — that is allowed because the approved unit is the `loopx` invocation, not the git command it happens to build.

## Failure policy

- If the same verification command fails twice for the same issue in one run, stop retrying — escalate.
- If a tool call is outside the permissions policy above, stop and add a note under "Needs Human Review" in `<loop_dir>/PROGRESS.md` instead of improvising.

## End of run

This section always runs, even if Step 1 found zero assigned issues — a quiet morning is still reported, not silently skipped. The one exception is `--batch-issue` mode, which skips this section entirely; the run's single `--batch-end-of-run` session is what makes "always" true there (see "The scheduled batch run: two prompt modes, not one session").

All paths here are written absolutely for the same reason as the script paths: this section runs after the last issue's per-issue work, so the working directory may still be a leftover worktree.

1. Write `<loop_dir>/outputs/daily-review.md` with sections: Summary, Issues checked, New comments found, MRs opened, Answered directly, Escalations, No-ops. "Answered directly" lists issues closed out with a GitLab comment and no code change; "No-ops" means nothing new since the last run and no action taken — an answered question is never a no-op. An issue handled via "Wait for reviewer" always belongs under No-ops (it never gets a GitLab comment); if that run happened to be the one that sent the weekly Slack reminder (`data.reminder_sent == true`), say so in the No-ops line for that issue so the reminder isn't invisible in the summary. If nothing was actionable, this can be short (e.g. "No assigned issues today"), but it must still exist and say so explicitly.
2. Copy it to `<loop_dir>/outputs/history/<YYYY-MM-DD>.md`.
3. Update `<loop_dir>/PROGRESS.md`: last run date, issues touched, MR links opened today, open escalations.
4. Send the end-of-run Slack digest — unconditionally, even when every count is zero:
   ```
   loopx notify "*Daily GitLab loop:* N issues checked, M MRs opened, J answered directly, K escalated"
   ```
   e.g. `"*Daily GitLab loop:* no assigned issues today. 0 checked, 0 MRs opened, 0 answered directly, 0 escalated."` when the task list was empty. This digest is an aggregate summary rather than a report on one issue, so it carries no `<issue_url>` link.

## Verification checklist (before ending the run)

The first three bullets are the "End of run" artifacts, so they apply only to the modes that actually produce them: the whole-run mode (no flag, Step 1 onwards), the dashboard's `<alias> <issue_iid>` single-issue mode, and `--batch-end-of-run`. **A `--batch-issue` session is exempt from those three** — it must not create, update, or even touch any of them, because the run's separate `--batch-end-of-run` session owns them (see "The scheduled batch run: two prompt modes, not one session"). Their absence is expected in that mode, never a failed verification. Everything from "No merge request was merged" down applies to every mode.

- `<loop_dir>/outputs/daily-review.md` exists and has all seven required sections.
- `<loop_dir>/outputs/history/<today>.md` exists.
- `<loop_dir>/PROGRESS.md` was updated.
- No merge request was merged.
- No file outside an issue's own worktree, `PROGRESS.md`, or `outputs/` was modified.
- Every issue this session was responsible for was either fixed-and-MR'd, answered directly, escalated for clarification, or escalated for verification failure — none were silently skipped. That means today's whole task list in the whole-run mode, and in `--batch-end-of-run` the whole run as reconstructed from the event log (an `issue.started` with no `issue.completed`/`issue.escalated` is the crashed-part-way case, which belongs under Escalations, not dropped); in `--batch-issue` and the dashboard single-issue mode it means the one issue named in your prompt.

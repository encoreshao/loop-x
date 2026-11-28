# MR Review Loop

## Goal

Every two hours, find open, non-draft merge requests assigned to you (as a
reviewer or an assignee, and not authored by you), have the model pre-review
each diff and leave the findings as GitLab **draft** review notes. You open
the MR, check the drafts under Review, and submit to publish them (or discard
them).

## The review

The model reviews against the MR's goal first, then correctness, security,
performance, data/migrations, reliability/compatibility and tests. Its answer
is structured (`summary`, `verdict`, `overview`, `risks`, `findings`), and the
loop renders it as:

- one inline draft per finding: severity and area up front
  (`🟠 **Major** · Data & migrations — title`), then the problem and an
  optional **Suggested fix**. Then comes a collapsed **🤖 Prompt for AI agents**:
  one self-contained instruction a coding agent can follow. Last is a hidden
  `<!-- loopx-finding {...} -->` marker holding the finding's stable id
  (path, area and normalised title, so it survives line shifts), severity,
  area, path, line and that instruction. In the marker, `-`, `>` and `@` are
  JSON-escaped, so it can't close the comment early or mention anyone. When
  the draft is published, Fix now reads the marker for the thread's severity
  instead of parsing the heading;
- one overall draft: the verdict (looks good / mergeable after comments /
  changes requested), a "What this MR does" list, a risk table by area and
  level, and an index of the findings.

## Verification pass

Before anything is drafted, a second sealed, tool-less call re-checks every
candidate finding against the diff. It runs on the cheaper `sonnet` by default
(loop setting `verify_model`; `off` disables it) with a $0.25 cap. For each
finding it answers:

- **keep**: the problem is real;
- **drop**: wrong, already handled, speculative, or not in the change;
- **downgrade**: real but less serious.

A downgrade below the severity floor counts as a drop. Secret-scan findings are
never sent to it.

It fails open: an unusable reply, an error or an exhausted budget keeps every
finding. The summary counts dropped findings under **Review scope**, and the run
log records kept, dropped, downgraded and the cost. The verdict still comes from
the main review.

`loopx loop eval --review` runs the same pass, and `--no-verify` measures
without it.

## Automated checks

Every reviewed MR also passes the same model-free gates LoopX's own fixes pass
before their merge request opens (`crates/core/src/mr_gates.rs`):

- **Secret scan** (`security_scan::scan`, unchanged): runs at discovery on the
  MR's API diff, rebuilt with `diff --git` separators so each hit names its own
  file. A blocking hit (a key, token, private key, generic secret outside test
  paths, an added `.env`) becomes an inline 🔴 Blocker · Security draft on its
  line, at most 5 per MR; the summary counts all of them. Files without a text
  diff (binary, collapsed, too large) and a truncated diff are named in the
  line, never called clean.
- **Dependency audit** (`dependency_audit::audit`, unchanged), GitLab only for
  now: for each changed lockfile (at most 3 per MR, at most 10 MB) the loop
  reads the file at the MR's base and head commits through the files API, plus
  a sibling `package.json` at head for npm/Yarn/pnpm, into a temp dir, and
  audits the packages the MR added or upgraded with the installed tool. At
  most 5 minutes of audit time per run; later MRs say it was skipped.
- Both appear under **Automated checks** in the overall draft, after the
  verdict: an optional `⛔ Would block a LoopX fix: …` line, then the same
  secret-scan and dependency lines LoopX writes into its own MR descriptions.
  They are advisory: the model's verdict is never changed, nothing is blocked,
  approved or published, and the model's prompt and input are unchanged (the
  results live in the item's `meta`, which the model never sees).
- Each reviewed MR commit emits `mr_review.gates_completed`
  (`data.source = "human_mr"`; rule and advisory ids only, no path or text),
  counted on Harness → Gates → **Human merge requests** (MRs checked, and how
  many would have been blocked; the latest commit of each MR wins). GitHub
  pull requests get the secret scan only.

## How it runs

- Plugin: `crates/core/src/loop_plugins/mr_review.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/mr-review/loop.yaml`, prompt `loops/mr-review/prompt.md`.
- Schedule: every 2 hours from its `loops.json` entry (disabled by default;
  `requires: ["merge_requests"]`). Only `gitlab` accounts are used.
- One item per MR **and head commit** (`mr:<account>:<project>!<iid>@<sha>`),
  so a new push is reviewed again and an unchanged MR never is. At most 8 MRs
  per run. Setting `min_severity` (default `minor`) drops lower findings.
- Drafts are never reviewed: the list asks for `draft=no`, and a `draft` /
  `work_in_progress` flag or a `Draft:`/`WIP:` title prefix is skipped too
  (older GitLab ignores `draft=no`).

## Repository configuration (`.loopx/review.yaml`)

A project can commit `.loopx/review.yaml` (at most 64 KB) to tune its reviews.
The file is read through the files API at the MR's **base** commit, never its
head. So an MR cannot change the rules that review it, and a config change
applies to MRs opened after it merges. GitLab only for now: GitHub pull requests
use the defaults.

```yaml
version: 1
profile: chill          # quiet (major+, at most 8 findings) | chill (minor+, 15) | assertive (nit+, 25)
path_filters:           # globs: * and ? within a segment, ** across segments,
  - "!docs/**"          #   no "/" = any depth; "!" excludes
path_instructions:      # guidance for matching files, sent with the diff
  - path: "db/migrate/**"
    instructions: "Every migration sets lock_timeout; backfills run in batches."
auto_review:
  enabled: true         # false: only "Review now" reviews
  ignore_title_keywords: [WIP]
  labels: [review-me, "!no-review"]   # any plain label = required; "!label" excludes
  ignore_authors: [renovate-bot]
  base_branches: [main, "release/.*"] # regexes matched against the whole target branch
```

**Layers.** Each layer below overrides the ones before it:

1. built-in defaults;
2. the loop's `min_severity` setting;
3. the repo file;
4. the matching project's `review` block in `projects.json` (same keys,
   matched by `project_id`).

Your Mac has the last word. `path_filters` from the repo file and
`projects.json` are combined.

**Errors.** Unknown keys, a bad profile, an unsupported `version` or an invalid
branch regex make that layer ignored. The review still runs, and its summary
says why under **Review scope**.

**Path filters.**
- A built-in deny-list is always on: lockfiles, `node_modules/`, `vendor/`,
  `dist/`, generated folders, minified files, source maps, snapshots, protobuf
  output, images, fonts and archives.
- A plain pattern brings back a file the deny-list would drop.
- Any plain pattern turns the list into an allowlist.
- A `!` exclude always wins.
- Filtered files leave the model's diff only. The secret scan, the dependency
  audit and `changed_paths` still see every file.
- An MR whose files are all filtered out is not reviewed. The summary counts the
  filtered files.

**Auto review.** The rules are checked right after the MR detail is read, before
any diff is fetched. **Review now** bypasses them.

**Repository guidance.** The model gets a `repo_guidance` list next to the diff.
Each entry has a `source`, the files it `applies_to` and a `text`, most specific
first:

1. `path_instructions` (from both layers) whose glob matches a reviewed file;
2. `CLAUDE.md` / `AGENTS.md` in the five deepest folders holding reviewed files;
3. root `CLAUDE.md`, `AGENTS.md`, `.cursorrules`,
   `.github/copilot-instructions.md`;
4. the project's `custom_instructions` from `projects.json`.

All files are read at the base commit. Each entry is capped at 8,000 characters
and the list at 40,000.

The prompt treats guidance as the team's conventions: a risky violation is a
finding, and something the guidance allows is not. Guidance is untrusted data,
like the diff. It never changes the answer format, severities or verdict rules.
Without guidance the key is absent and the prompt is unchanged. The summary
counts the sources used under **Review scope**.

**Checking a file.** `loopx review config [--file <path>] [--project <alias>]`
validates a file and prints the effective configuration with the source of each
value. It reads local files only.

## The Merge requests tab

The loop page lists your open MRs live from GitLab: those assigned to you
(with their review state) and those you created (never reviewed). Per MR you
can turn **Auto review** off (skip it) or press **Review now**, which queues
it and starts a run that reviews it even if its current commit was already
reviewed. Both choices live in
`outputs/loops/mr-review-loop/mr-prefs.json` (`skip`, `requested`); a run
clears the requests it read.

**Fix now** on an MR is a separate manual run (see
[`mr-fix-now.md`](mr-fix-now.md)); this review loop stays draft-note-only.

## Safety boundary

- Writes only `POST /projects/:id/merge_requests/:iid/draft_notes`. It never
  publishes (`bulk_publish`), approves, or posts a normal note.
- MR title, description and diff are untrusted; the model runs sealed and the
  prompt says so. Draft text is built from validated findings only: the path
  must be in the diff, the line an integer, the severity known, the body
  trimmed to 2000 chars, at most 15 findings. Word-start `@` is defused so a
  model-echoed `@all` cannot ping anyone.
- The diff sent to the model is capped at 150 KB with a visible truncation
  marker.
- If GitLab rejects a line position (400/422) the finding is posted as a
  general draft note prefixed with `path:line`.
- Fetch errors log only the exception class name; one failing account or MR
  does not stop the others.
- Reads lockfiles through the files API into a temp dir and runs only installed
  audit tools there (never an install, never the MR's tool config: no `.npmrc`,
  `.yarnrc*`, `.pnpmfile.cjs` or `.cargo/` is fetched, and `package.json` is
  reduced to its name, version, workspaces and dependency lists, so its audit
  ignore lists never apply and are reported as a note; a path with `..`, a
  leading `/` or `-`, or a control character is refused).
- Gate text is model-free; paths are flattened to one line and a line-start
  `/` is neutralised (in the model's text too), so no draft line can run a
  GitLab quick action once published.

## Re-runs, partial failures and read-only tokens

- Before posting, the loop lists the MR's draft notes and deletes only those
  carrying the "LoopX pre-review" signature (earlier partial runs, older
  SHAs); your own drafts are never touched. Draft POST/DELETE calls are never
  auto-retried (`max_attempts=1`).
- If a write fails after at least one draft was posted, the item is `done`
  ("N draft notes (partial: <ExcClass>)") and marked seen, so it is not
  re-posted as a duplicate set. If nothing was posted, the item fails and
  retries next run.
- A 401/403 on a draft write (token lacks the `api` scope) marks the item
  `skipped` ("token needs the api scope to write draft notes"), which is
  recorded as seen for that MR SHA, so the same SHA is not re-reviewed (and
  re-billed) every run. Other MRs/new SHAs still reach the model once each
  until the token is fixed.
- Markdown images in finding bodies are neutralised (`![` becomes `!` + a
  zero-width space + `[`).

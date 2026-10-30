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
  optional **Suggested fix**;
- one overall draft: the verdict (looks good / mergeable after comments /
  changes requested), a "What this MR does" list, a risk table by area and
  level, and an index of the findings.

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

## The Merge requests tab

The loop page lists your open MRs live from GitLab: those assigned to you
(with their review state) and those you created (never reviewed). Per MR you
can turn **Auto review** off (skip it) or press **Review now**, which queues
it and starts a run that reviews it even if its current commit was already
reviewed. Both choices live in
`outputs/loops/mr-review-loop/mr-prefs.json` (`skip`, `requested`); a run
clears the requests it read.

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

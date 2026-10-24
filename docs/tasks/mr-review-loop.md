# MR Review Loop

## Goal

Every two hours, find open, non-draft merge requests where you are a reviewer
(and not the author), have the model pre-review each diff and leave the
findings as GitLab **draft** review notes. You open the MR, check the drafts
under Review, and submit to publish them (or discard them).

## How it runs

- Plugin: `crates/core/src/loop_plugins/mr_review.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/mr-review/loop.yaml`, prompt `loops/mr-review/prompt.md`.
- Schedule: every 2 hours from its `loops.json` entry (disabled by default;
  `requires: ["merge_requests"]`). Only `gitlab` accounts are used.
- One item per MR **and head commit** (`mr:<account>:<project>!<iid>@<sha>`),
  so a new push is reviewed again and an unchanged MR never is. At most 8 MRs
  per run. Setting `min_severity` (default `minor`) drops lower findings.

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
  carrying the "Loop X pre-review" signature (earlier partial runs, older
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

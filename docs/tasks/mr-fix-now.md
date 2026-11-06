# MR Fix now

## Goal

On the MR Review page, press **Fix now** on any non-draft merge request the
page lists (assigned to you as assignee or reviewer, or created by you, whoever its author is, so a reviewer-only MR pushes to its author's branch; same
project only, not a fork) to have the agent address its review feedback and
push the fixes to the MR's source branch. It is a manual, one-MR run. It is separate from the
MR Review loop, which stays draft-note-only (see
[`mr-review-loop.md`](mr-review-loop.md)).

## The manual flow

1. The button starts a detached `loopx mr-fix run <ref>` job (the dashboard
   records it as a job and shows its progress).
2. Pre-check: eligibility and the MR's changed files are checked before
   anything is posted.
3. A worktree for the MR branch is prepared.
4. A start note is posted on the MR.
5. One agent session fixes the feedback (it announces its plan through
   `loopx mr-fix announce` before editing).
6. The runner, not the agent, re-runs the project's checks.
7. The runner pushes to the MR's source branch (a plain push, never forced).
8. A finish note is posted.

## What it posts, and as whom

Everything is posted with the user's own GitLab token, so notes carry the
user's name. Every note opens with `🤖 LoopX agent`.

- Start note.
- Finish note: a checkbox list of the feedback items (fixed or not), plus a
  hidden handled marker (`<!-- loopx-mr-fix handled=... -->`) that records
  which feedback was handled.
- Optional replies in the review threads it addressed.

When nothing needs a code change (questions, approvals, items it skips), the
agent does not announce: the runner posts only the finish note, marking those
threads handled, and pushes nothing.

## Eligibility

- Listed on the MR Review page (assigned to you as assignee or reviewer, or created by you), any author. No role check is made: pressing it on an MR you only review pushes to its author's branch.
- Same-project merge request (not from a fork; an MR whose source project is
  unknown is treated as a fork).
- The project is configured in `projects.json`.
- Not a draft (a draft shows the button disabled, with the reason).

## What counts as feedback

All unresolved threads and human notes on the MR. LoopX review findings that
you have submitted count; drafts that are still pending do not.

Severity policy: Major and Medium findings are all addressed; Low is the
agent's call, and it gives a reason when it skips one.

## Safety boundary

- Never merges, approves or resolves threads.
- MR text (titles, descriptions, notes, diffs) is untrusted data and is
  never obeyed as instructions.
- Refuses an MR that changes `.claude/`, `CLAUDE.md`, `CLAUDE.local.md` or
  `.mcp.json` (matched case-insensitively), since the worktree is the MR's
  branch and those files would configure the agent.
- The project's checks are re-run by the runner after the agent finishes.
- Never force-pushes; refuses a push that rewrites history.
- Refuses to push per-developer or git-ignored files (for example
  `config/database.yml`, `.ruby-version`). Ignore status comes from the
  `.gitignore` files of the commit the run started from, so removing a rule
  in the same push does not help.
- Items the agent reports as fixed are only counted, checked off and marked
  handled when a commit was pushed; otherwise they show as "reported fixed,
  but nothing was committed".
- A leftover unpushed fix from an earlier run blocks the next run, with the
  worktree path in the message; push or discard it by hand. If the author
  rebased or force-pushed and nothing local is unpushed, the worktree follows
  the new branch.
- The agent runs under Claude permission rules (an allow/deny fence), not a
  sandbox: `git push`, the GitLab, Slack and dashboard helpers and credential
  files are denied, but the lint, test and install commands it may run
  execute the repository's own code. The runner's push and gate ignore repo
  hooks, but worktree setup (checkout, merge, submodules) runs plain git, so a
  hook a project tracks (for example husky) can run then. Neither the agent nor
  the checks see `GITLAB_TOKEN`/`GITLAB_URL`.

## Cleanup

The worktree is removed only by `loopx mr-fix run` on a merged or closed MR.
The dashboard does not trigger it.

## Open items

- `--restricted`: the Claude CLI flag was not adopted, because its effect on
  the allowlist fence is unknown without a paid call. The owner should run
  the golden MR-fix case once as is and once with `--restricted` for the fix
  session, and check that git add/commit and the allowed check commands
  still run, file edits work, every deny rule still blocks, a hostile
  `.claude/settings.json` has no effect, and whether `CLAUDE.md` is read.
- Known gaps: a commit message read from a file through combined short flags
  is not checked; GitLab truncating the changed-files list on very large MRs
  could hide a settings file; LoopX severity labels in human notes can be
  spoofed.
- Jobs have no log view or Stop button in the UI yet.
- Before merge: the golden eval for the new prompt (`instructions/loopx/mr-fix.md`) and
  tool allowlist has not been run; a paid `loopx loop eval --golden` before
  and after, plus a new MR-fix case, is the owner's to run.

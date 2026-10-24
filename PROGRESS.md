# Loop Progress

## Current State
- Status: OK — GitLab API token is working again (confirmed this run)
- Main objective: Track assigned issues on the projects configured in ~/.loop-engineering/projects.json; fix clear, scoped requests; escalate the rest
- Current focus: Nothing actionable right now — all 4 assigned issues are no-ops; 2 remain awaiting a human reply to prior clarification requests
- Last updated: 2026-08-21 (run 3)

## Last Run
- Date: 2026-08-21 (third scheduled run today)
- Trigger: Scheduled headless run
- Issues checked: 4 — `brightleaf.web` #12, #13, #14; `orchard` #21. `harbor` had none assigned. The GitLab PAT that was revoked in the prior run (afternoon) is working again — `list_assigned_issues.py` and all subsequent API calls succeeded.
- New comments found: none on any issue.
- MRs opened: 0
- Answered directly: 0
- Escalations filed this run: 0 new. 2 pre-existing escalations remain open (see Needs Human Review).
- No-ops: 4 — see `outputs/daily-review.md` for detail on each.

## Open Items
- Follow up on 2 open escalations (see Needs Human Review) — still no human response as of 2026-08-21.
- `brightleaf.web` #14 and `orchard` #21 both look fully resolved via manual work already deployed to prod; a human should consider closing them (outside the loop's scope to do itself).

## Blockers
- None currently. The GitLab PAT revocation noted in the prior run's entry is resolved.

## Needs Human Review
- brightleaf.web #12: needs a human to decide who provisions the GitLab service account/PAT and whether any of this maps to a repo change (posted 2026-08-09, still open as of 2026-08-21). Only new activity since is a side conversation between two humans about an unrelated account, not an answer to the clarification.
- brightleaf.web #13: needs confirmation of the validation algorithm, related-record cleanup scope, and report delivery format before an irreversible bulk-delete migration is written (posted 2026-08-09, still open as of 2026-08-21, no response yet).

## Next Run Should
- Read this file and `docs/tasks/gitlab-issue-loop.md` first.
- List open issues assigned to the configured user across the configured projects (`python3 <loop_dir>/bin/list_assigned_issues.py`).
- Process issues one at a time per `LOOPX_INSTRUCTIONS.md`.
- Re-check whether #12/#13 have received a human reply yet before re-escalating.
- Update this file before stopping.

## Decisions Made
- The loop never merges its own MRs.
- The loop never self-assigns issues.
- Project scope lives in `~/.loop-engineering/projects.json`, not hardcoded in this repo.

## Do Not Repeat
- Do not retry the same failing verification command twice on the same issue in one run.
- Do not modify files outside an issue's own git worktree.
- Do not push directly to a project's target branch.

## Loop Review Notes
-

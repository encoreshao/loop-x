# Stale Work Sweeper Loop

## Goal

Once a week, list the GitLab work that has gone quiet so nothing is silently
forgotten: open issues assigned to you and open MRs you opened that nobody has
touched for a while, and open MRs waiting on your review.

## How it runs

- Plugin: `crates/core/src/loop_plugins/stale_sweeper.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/stale-sweeper/loop.yaml` (no prompt - see below).
- Schedule: Mondays 09:00 from its `loops.json` entry, disabled by default;
  `requires: ["issues"]`, so a GitLab connector must exist.
- No model call: `build_prompt` renders the list and `call_model` returns it
  unchanged with `cost_usd: 0`. The run still goes through LoopRuntime, so it
  has history, a ledger entry and the usual failure reporting.
- Per GitLab account (`GET /user` for your username), three queries, each
  `state=opened`, oldest first, at most 20 rows:

  | Section | Query | Idle for |
  |---|---|---|
  | Assigned issues | `/issues?scope=assigned_to_me` | `stale_days` |
  | My merge requests | `/merge_requests?scope=created_by_me` | `stale_days` |
  | Reviews waiting on you | `/merge_requests?scope=all&reviewer_username=<you>` | `review_days` |

  "Idle" is GitLab's `updated_before`, so any activity (comment, push, label)
  resets it.
- One item per account with anything stale, key `stale:<account>:<date>`, so
  the same work is listed again next week until it is touched. Nothing stale
  anywhere means no message.
- Rows: `- g/web#12 Title (idle 21d) <url>`; titles go through `chat_text`,
  URLs through `chat_url`. The notification starts with
  `Stale work - <date>`.

## Settings

| key | default | meaning |
|---|---|---|
| `stale_days` | 14 | your issues/MRs idle this many days are listed (1-365) |
| `review_days` | 3 | reviews waiting on you this many days are listed (1-365) |

## Safety boundary

- Read-only: it never comments, labels or nudges anyone.
- Errors log only the exception class name; a failing query is shown as
  "unavailable" and never hides the other sections or accounts.

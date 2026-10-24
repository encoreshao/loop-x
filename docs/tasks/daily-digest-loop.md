# Daily Digest Loop

## Goal

Every weekday morning, send one short brief: pending todos, issues assigned to
you, merge requests awaiting your review (and your own open ones), today's
calendar meetings, what Loop X completed or escalated yesterday, urgent inbox
counts and the latest topic headlines. It is read-only: it gathers data from
every enabled `issues` connector account (GitLab, GitHub) and `calendar`
account, asks the model to sort it into sections, and posts the result through
the loop's notification routing.

## How it runs

- Plugin: `crates/core/src/loop_plugins/daily_digest.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/daily-digest/loop.yaml`, prompt `loops/daily-digest/prompt.md`.
- Schedule: weekdays 09:30 from its `loops.json` entry (disabled by default;
  `requires: ["issues"]`).
- One item per day (`digest:<date>`), so a second run the same day is a no-op;
  the dashboard's "Run now" passes `--force` to run again.

## Safety boundary

- Read-only against connectors; the only side effect is the notification.
- A failing account or calendar adds an entry with only the exception class
  name (never the message) and never fails the digest.
- The data sent to the model is capped at 60 KB (`"truncated": true` when cut)
  and is treated as untrusted content in the prompt.
- Any URL in the model's answer that is not present in the gathered data is
  dropped, so the digest never links to something invented.

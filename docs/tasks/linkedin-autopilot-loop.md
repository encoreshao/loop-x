# LinkedIn Autopilot Loop

## Goal

Once a day, browse your own signed-in LinkedIn session at a human pace, read the
feed, notifications and messages, and leave a short overview of what is worth
your attention in the Content tab and Slack. Stage 1 is read-only; liking posts
and sending connection requests come in Stage 2.

## How it runs

- Plugin: `crates/core/src/loop_plugins/linkedin_autopilot.rs` on LoopKit; definition
  `loops/linkedin-autopilot/loop.yaml`, prompt `loops/linkedin-autopilot/prompt.md`.
- Schedule: daily 09:00 from its `loops.json` entry (disabled by default;
  `requires: ["social"]`, so add a LinkedIn connector and press Connect LinkedIn first).
- Browsing: `crates/core/src/linkedin_browser.rs` through `crates/core/src/linkedin_humanize.rs` - variable
  dwell time, uneven scrolling, curved cursor movement, one to three extra pages
  (My Network, Jobs, your profile), a random start time inside the active-hours
  window, some days skipped, and at least 2.5 s between page loads.
- Discovery: one item per account per day (`linkedin:<account>:<date>`) with up to
  30 notifications, 30 feed posts and 20 messages not seen before
  (`outputs/loops/linkedin-autopilot-loop-items/seen.json`, gitignored).
- Answer: `{summary, topics, worth_reading: [{id, why}], people: [{id, why}]}`;
  ids that were not offered are dropped.
- Problems (expired session, security check, no Playwright/Chrome) become a failed
  outcome and a "LinkedIn needs attention" message; the model is not called.

## Settings

`headless` (default off: a visible Chrome window), `active_hours` (default `9-21`),
`topics`. The session lives in `~/.loopx/linkedin/<account>/profile/`
(mode 0700; no password stored; Disconnect wipes it).

## Safety boundary

- Read-only in Stage 1. Messages are summarized, never answered.
- LinkedIn content is untrusted; the model runs sealed; text goes through the chat sanitizers.
- LinkedIn does not offer an API for this and its terms prohibit automation: the
  account can be restricted. Pacing, the active-hours window, skipped days and the
  stop-on-security-check rule reduce that risk; they do not remove it.

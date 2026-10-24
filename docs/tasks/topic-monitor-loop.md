# Topic Monitor Loop

## Goal

Every day, for each topic configured in `~/.loopx/topics.json`, research what's new since the last run (using live web search — this loop reads the wider internet, not GitLab), write a short briefing of the notable items, and deliver it as a saved history entry plus a Slack message. Nothing already reported in the last 7 days is repeated. A quiet day — nothing new to report — still produces a briefing and a Slack message saying so, exactly as the GitLab issue loop still sends its end-of-run digest on a morning with zero assigned issues.

This is a separate, independent loop from the [daily GitLab issue loop](gitlab-issue-loop.md) — its own entry script, instructions doc, and config file. Nothing about the GitLab loop's files is shared or changed by this task. Both loops are, however, triggered by the same daemon: the unified scheduler (`crates/core/src/loop_scheduler.rs`), the single launchd job `com.hermes.loop-engineering` polling on a `StartInterval`, checks `~/.loopx/loops.json` (the loop registry) and runs whichever registered loop is due — there is no longer a dedicated topic-monitor plist/daemon.

## Setup

Copy `config/topics.json.template` to `~/.loopx/topics.json` and list the topics to monitor (see Scope below for the format). There is no dashboard form for managing topics — like `projects.json`, this file is hand-edited per machine.

This loop's schedule lives as its own entry in `~/.loopx/loops.json` (seeded from `config/loops.json.template` by `install-loopx.sh`, or by the app on first run), which defaults to every day at 10:00 AM. Like `projects.json`/`topics.json`, that entry is hand-edited per machine, not driven from a dashboard form — the Daemons page shows this loop's daemon (the unified scheduler, `com.hermes.loop-engineering`) with its own on/off switch and polling interval, but changing which days/time this specific loop runs on means editing its entry in `loops.json` directly.

## Scope

Which topics to monitor, and what "notable" means for each one, are **not** hardcoded in this repo — they live in `~/.loopx/topics.json`, a JSON array of:

```json
{ "name": "ai-news", "label": "AI news", "brief": "Major model releases, funding rounds, notable research papers, and product launches in AI, from the last 24 hours.", "slack_bundle": null }
```

- `name` — a stable slug, used in file paths and as the identity key for seen-item dedup. Must not change once a topic has run.
- `label` — human-readable name shown on the dashboard and in Slack messages.
- `brief` — free text describing what counts as notable for this topic; the loop's research step is guided by this text, not a fixed keyword list.
- `slack_bundle` — optional; `null` uses the default Slack webhook, or the name of an access bundle from `~/.gitlab/config.json`'s Slack-webhook-override mechanism (the same bundle concept the GitLab loop uses) to route this topic's notifications elsewhere.

`crates/core/src/topics.rs` reads this file — same dependency-injection style and same fail-fast-if-missing behavior as `crates/core/src/config.rs` reading `projects.json`.

Topics are processed one at a time, in the order listed, never in parallel — same discipline as the GitLab loop's issue processing.

## Expected output

Each run produces, per configured topic:
- `outputs/topic-monitor/history/<date>-<topic-name>.md` — that day's briefing
- A status update the dashboard's Loops → Topic Monitor page reads (idle/running/last-run-time per topic)
- One Slack message via the webhook at `~/.slack/config.json` (or the topic's `slack_bundle` override), containing the briefing text directly — the dashboard is localhost-only, so messages never link to it

And, across the whole run:
- `outputs/topic-monitor/state/<topic-name>.json` — a rolling 7-day window of already-reported item URLs/titles, so the same story isn't repeated day over day

## Safety boundary (fixed — does not loosen with time or repeated success)

- Only reads the public web via `WebSearch`/`WebFetch` — never touches GitLab, any project checkout, or git in any way.
- Writes are confined to `outputs/topic-monitor/` — no other path in this repo, or on the machine, is ever written to.
- Topics are processed one at a time, sequentially — never multiple research passes in parallel in the same run.
- A quiet result (nothing notable since the last run) still produces a briefing and a Slack message saying so — never silently skipped.
- Only the command allow-list in `instructions/loopx/topic-monitor.md` may run — no arbitrary shell, no reading `.env`/credentials/SSH keys.
- The loop only touches: the topics listed in `~/.loopx/topics.json`, and its own state under `outputs/topic-monitor/`.

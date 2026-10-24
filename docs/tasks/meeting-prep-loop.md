# Meeting Prep Loop

## Goal

Shortly before each meeting, send a short prep brief: what the meeting is for,
the agenda, what is still open, what to raise and what was left over from last
time. Daily Digest lists the day's meetings each morning; Meeting Prep is the
per-meeting complement.

## How it runs

- Plugin: `crates/core/src/loop_plugins/calendar_prep.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/calendar-prep/loop.yaml`, prompt
  `loops/calendar-prep/prompt.md`.
- Schedule: every 15 minutes (`{"frequency": "hourly", "interval_minutes": 15}`)
  from its `loops.json` entry, disabled by default; `requires: ["calendar"]`,
  so add a Google Calendar connector first.
- Discovery: for each calendar account (or only `settings.calendar_account`),
  events starting within the next `lead_minutes` (default 45). All-day events,
  events already started, events you declined and events with no other
  attendee (unless `include_solo` is `yes`) are skipped. One item per
  occurrence, key `cal:<account>:<event id>:<start>`, so each occurrence gets
  one brief and a moved meeting gets a fresh one.
- Each item carries the invite (title, time, organizer, attendee names,
  location, join link, description trimmed to 4000 characters) plus:
  - **Linked GitLab work**: issue/MR URLs in the description that belong to a
    configured GitLab account, at most 5, fetched for title, state, last
    update, assignees/reviewers and MR merge status.
  - **Recent mail**: `search_recent` on every connected mailbox for the
    organizer and attendees (never you), last `mail_days` days (default 30),
    at most 10 rows of subject/sender/date/snippet - never bodies. Only when
    the selected AI CLI is Claude; `mail_days: 0` turns it off.
  - **Last time**: the previous brief's summary and follow-ups for the same
    recurring series, from `outputs/loops/meeting-prep-loop/series.json`
    (gitignored, written atomically after each brief).
- Answer: `{summary, agenda, open_items: [{text, link}], talking_points,
  follow_ups}`. Every string is sanitised and capped (summary 400 characters,
  5 entries of 200 per list); an `open_items` link survives only if it was one
  of the offered GitLab URLs.
- Quality filter: after the model answers, list markers are stripped and
  empty, placeholder ("N/A", "None", "TBD", "No agenda provided"), too-short
  and duplicate entries are dropped, so a brief only holds content the model
  could ground in this meeting's data. The prompt requires that grounding.
- Delivery: one Slack mrkdwn block per brief through the loop's Notify via
  routing: bold title, then **Date & Time** (weekday, date, range, timezone,
  minutes until start), **Attendees** (up to 8 names, "+N more") and
  **Meeting Link**, then the quoted summary, agenda, open items, talking points
  and follow-ups. Empty sections are omitted.

## Settings

| key | default | meaning |
|---|---|---|
| `calendar_account` | empty = all | connector id of the calendar to watch |
| `lead_minutes` | 45 | how far ahead to look, 15-120 |
| `mail_days` | 30 | mail lookback in days; 0 turns mail off |
| `include_solo` | `no` | `yes` also briefs events with no other attendees |

Invalid numbers fall back to the default and are clamped to the range.

## Safety boundary

- Read-only: reads calendar, GitLab and mail; writes only the notification
  and its own `series.json`.
- Invite text, GitLab titles and mail snippets are untrusted; the prompt says
  so, the model runs sealed, and notification text goes through the chat
  sanitizers.
- Attendee addresses that are not plain email addresses never reach a mail
  search query.
- Errors log only the exception class name; a failing calendar, GitLab link or
  mailbox never stops the other sources or the brief.
- A failed model call leaves the occurrence unseen, so the next run retries it
  while the meeting is still upcoming.

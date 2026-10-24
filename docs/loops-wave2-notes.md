# Wave-2 loop design notes

Decisions already made for the next batch of LoopKit loops, so the next plan
starts from them. Docs only; nothing here is implemented yet.

All wave-2 loops are `LoopPlugin` subclasses (`crates/core/src/loopkit.rs`) living under
`bin/loop_plugins/`, with `loops/<definition_dir>/{loop.yaml,prompt.md}`, a
`loops.json` entry that sets `routes_notifications: true` (so notifications go
through `crates/core/src/loop_notify.rs` and the "Notify via" control appears), and a
module-level `SETTINGS_FIELDS` tuple of connector `Field`s that the dashboard
reads (`_plugin_settings_fields`) and the plugin exposes as `settings_fields`.
Untrusted text in any chat message goes through `chat_text`, `chat_url` or
`chat_link`. The plugin contract is `discover(ctx)` -> `WorkItem`s,
`after_item(item, answer, ctx)` -> `Outcome`, optional `digest(outcomes, ctx)`,
and `output_keys` for the required answer JSON keys.

## Release Notes

- **Shipped** (2026-10-07): `crates/core/src/loop_plugins/release_notes.rs`, see
  `docs/tasks/release-notes-loop.md`. It triggers on a new tag (the first tag
  seen per project is only a baseline) rather than listing everything since
  the latest tag, and links come from the offered MRs, never from the model.
  The notes below are the original plan.
- **discover**: merged MRs on the default branch since the latest tag.
  `GET /projects/:id/repository/tags?per_page=1` for the tag date, then
  `GET /merge_requests?state=merged&target_branch=<default>&updated_after=<tag date>`
  using the existing GitLab connector (`crates/core/src/connectors/gitlab.rs`), as
  Pipeline Doctor and MR Review do.
- **Answer**: `output_keys = ("highlights", "fixes", "internal")`.
- **Output**: saved to `outputs/loops/release-notes-loop/<project>-<date>.md`,
  then a `digest()` notification.
- **Out of v1**: creating the GitLab release. That would be an L2 action and is
  gated behind P4.

## Stale Work Sweeper

- **Shipped** (2026-10-07): `crates/core/src/loop_plugins/stale_sweeper.rs`, see
  `docs/tasks/stale-sweeper-loop.md`. `build_prompt` renders the list and
  `call_model` returns it unchanged at $0 (LoopKit calls `call_model` without
  the item, so the rendering lives in `build_prompt`). The notes below are
  the original plan.
- Proves `call_model` can be overridden to be deterministic, with no model
  call: `output_keys = ()` and `call_model` returns
  `{"text": rendered_list, "cost_usd": 0}`.
- Discovery lists stale issues/MRs; the rendered list is the answer and goes
  out via `digest()`.

## GitHub / Jira / Linear issue loops

- The GitHub, Jira and Linear connectors already exist
  (`crates/core/src/connectors/{github,jira,linear}.rs`); what is missing is the loop side.
- Requires extracting an `IssueSource` interface out of
  `crates/core/src/gitlab_runner.rs`: `list_assigned()`, `get(issue_ref)`,
  `comment(issue_ref, body)`, `open_change_request(branch, title, body)`.
- Do this **after** P4, so the gate and retry logic is shared rather than
  duplicated per provider.

## Meeting Prep

- **Shipped** (2026-10-07): `crates/core/src/loop_plugins/calendar_prep.rs`, see
  `docs/tasks/meeting-prep-loop.md`. It also needed `list_events` to return
  descriptions, attendees and join links, a read-only `search_recent` on the
  mail providers, and 15-minute schedules (`interval_minutes`). The notes
  below are the original plan.
- The Google Calendar connector now exists:
  `crates/core/src/connectors/google_calendar.rs` (`GoogleCalendarConnector`, capability
  `CALENDAR`, `list_events(time_min_iso, time_max_iso, max_results=50)`).
  No connector or re-consent work is needed.
- It only needs a LoopKit plugin: `discover` calls `list_events` for the
  upcoming window and yields one `WorkItem` per event, the model drafts a prep
  brief, and `after_item`/`digest` deliver it. Settings (calendar account,
  lookahead hours) go in `SETTINGS_FIELDS`.
- Attendee names, titles and descriptions are untrusted: pass them through
  `chat_text` before they reach any notification.

## Wave 3 one-liners

- **Dependency Updater**: template already exists in `templates/dependency-updater`.
- **Security Advisories**: an RSS Watch preset pointed at a GHSA feed.
- **Uptime check**: no model; a `CommandVerifier`-style HTTP probe.
- **Slack mention triage**: needs a `slack_bot` connector with the
  `channels:history` scope.

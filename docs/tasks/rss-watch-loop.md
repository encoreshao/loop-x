# RSS Watch Loop

## Goal

Once a day, collect new entries from your RSS/Atom feeds, have the model rank
them against your interests and send a short digest of the best ones.

## How it runs

- Plugin: `crates/core/src/loop_plugins/rss_watch.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/rss-watch/loop.yaml`, prompt `loops/rss-watch/prompt.md`.
- Schedule: daily 08:00 from its `loops.json` entry (disabled by default;
  `requires: ["feed"]`, so add an RSS connector first).
- Discovery: one item per RSS account per run (`rss:<account>:<date>`),
  carrying up to 40 entries not seen before (title trimmed to 200 characters;
  entries whose link is not a plain http(s) URL are dropped). Accounts with no
  new entries produce no item.
- Answer: `{"highlights": [{title, link, why, score}]}`, at most 10, sorted by
  score. Highlights whose link was not among the offered entries are dropped.
- Every offered entry is marked seen after a successful item (highlighted or
  not), in `outputs/loops/rss-watch-loop-entries/seen.json` (gitignored).

## Settings

The loop's page has a Settings tab with an `interests` textarea
(comma-separated topics, up to 1000 characters), stored in the loop's
`loops.json` entry as `settings.interests`. Plugins declare such fields with
`settings_fields` (and a module-level `SETTINGS_FIELDS`, which the dashboard
reads without running the loop); `POST /loops/<name>/settings` saves only
declared keys.

## Safety boundary

- Read-only: the loop only fetches feeds and notifies.
- Feed content is untrusted; the prompt says so, the model runs sealed, and
  notification text goes through the chat sanitizers.
- Errors log only the exception class name; one failing feed or account does
  not stop the others.

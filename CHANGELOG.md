# Changelog

All notable changes to Loop X Engineering are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is `0.x`, a minor bump may include breaking changes; those are called
out under **Changed** or **Removed**.

Every user-visible change adds a line under **Unreleased** in the same commit
or PR. To cut a release, see "Changelog and releases" in `CLAUDE.md`.

## [Unreleased]

### Added

- `loopx chat-tool <action>`: the command the dashboard chat assistant runs (status, history, daemons, run-now, topic/project saves, enable/disable, `call <tool>`), ported from the Python dashboard so the chat no longer needs it.

### Fixed

- Saving a tracked GitLab project or the worktree root now checks that the loop can actually create worktrees: the target branch and an absolute local checkout path (`~` is expanded) are required, the checkout must be a git repository with an `origin` remote, and the worktree root must be absolute, outside every tracked checkout, and is created if missing. `loopx worktree new` reports the same misconfigurations by name instead of a bare git failure.
- An installed `loopx` (DMG) now seeds every built-in loop definition, prompt and agent instruction into `~/.loopx`, so scheduled loops (topic, daily digest, ...) no longer fail with `cannot read ~/.loopx/loops/<loop>/loop.yaml`.
- Seeding the bundled loop definitions no longer overwrites files already in `~/.loopx` (for example a `loop.yaml` you edited to `mode: gate`) the first time it runs; only an upgrade refreshes them.
- The Runs page's empty-state hint now says `loopx loop run <loop.yaml>` instead of the removed `bin/loop_cli.py`.

### Changed

- The per-machine directory is now `~/.loopx` (was `~/.loop-engineering`). The first `loopx` run, `install-loopx.sh` or the Mac app's backend moves an existing one and leaves a symlink at the old name, so existing configs, worktree paths and launchd plists keep working; `uninstall-loopx.sh` finds either. `LOOP_ENGINEERING_HOME` still overrides it, and the Keychain service names are unchanged.
- The Mac app no longer checks for a legacy `com.hermes.loop-engineering` launchd scheduler before running scheduled loops, and Settings no longer warns about one.
- The Update page and banner only report that origin is ahead (and let you check again); upgrading is done by Sparkle (the app) or Homebrew / `install-loopx.sh --upgrade` (the binary).
- `loopx serve --no-legacy` is accepted and ignored; the legacy Python dashboard no longer exists.
- The Mac app's dashboard window now opens filling the screen's visible area (menu bar and Dock stay visible) the first time it is shown in a session; Settings > Display > "Dashboard window opens" switches it to native full screen or the standard window.

### Fixed

- A first launch of the Mac app (or `loopx serve`) on a fresh machine now seeds the scheduler state along with `loops.json`, as `install-loopx.sh` does, so turning on "Run scheduled loops" no longer fires the enabled loops straight away.

### Removed

- All Python code: `bin/*.py`, the Python dashboard, scripts and their tests. Everything runs in the `loopx` binary; `loopx run` no longer falls back to `python3` for an entry point it does not know.
- The legacy `install.sh`/`uninstall.sh`/`setup-nginx.sh` flow, `run-loop-now.sh`, the Python-bundled app build (`build_macos_app.sh`) and the legacy `launchd/com.hermes.loop-engineering*` templates.
- `loopx update run` and `loopx update start`, and the Update page's "Upgrade now" / "Retry" buttons: the git-pull self-update is replaced by Sparkle and Homebrew.

### Fixed

- `loopx scheduler` takes a lock (`scheduler.lock`) for the whole poll, so the LoopX app's built-in scheduler and a launchd scheduler running together can no longer start the same due loop twice.
- Loops → Topic Monitor now works in the Mac app (Live and Topics views are served by loopx itself); it showed "Not found" because the page had not been ported and the legacy dashboard is off.
- Loops → Inbox Triage now works in the Mac app (Live, Drafts and Setup views, saving inboxes and OAuth clients, pause, delete, disconnect, test connection). Connect (the Google and Microsoft sign-in) is not available there yet and says so.
- Loops → GitLab → Live shows the same empty view as Topic Monitor (message plus a Set up a project button) when no project is configured and nothing has run, instead of a disabled Run now over empty cards.
- Settings → General opens instantly: AI CLI availability is read from a saved flag (`cli_status.json`) and refreshed in the background instead of starting a login shell on every page view.
- Settings → Daemons: a fresh install now seeds `loops.json` from the template, so the registered loops list is no longer empty.
- The Mac app has a main menu, so ⌘Q quits LoopX and ⌘W, ⌘, ⌘H and the copy/paste shortcuts work.
- Settings → Daemons explains that the LoopX app runs scheduled loops itself when no launchd agents are installed, instead of showing an empty list.
- A running `loopx serve` rewrites `loopx.lock` within a few seconds if it disappears (for example when the data directory is removed under it), so the Mac app finds the backend instead of retrying forever against a port that is already in use.
- The Mac app's waiting screen shows the LoopX logo and name with a spinner and the backend's status, or the failure reason with a Try again link, and follows the system appearance.
- `loopx serve` keeps running when the legacy Python dashboard cannot start (no checkout, a missing module) and serves the ported pages, instead of exiting and leaving the Mac app without a backend. The app's dashboard window shows "LoopX" with the backend's status and opens by itself once it is ready.
- The Mac app starts the bundled `loopx serve` itself when launchd refuses to run its bundled agent (an ad-hoc signed build), instead of showing nothing.

### Added

- `build_loopx_app.sh --install [DIR]` copies the built LoopX.app to `/Applications` (or DIR) after quitting a running copy.
- LoopX.app has an icon (`assets/app-icon.png`, turned into `AppIcon.icns` by `build_loopx_app.sh`).
- The Mac app can run the scheduler itself: Settings > "Run scheduled loops while LoopX is open" (off by default) runs `loopx scheduler` on launch and every 15 minutes, and stands down when the installed `com.hermes.loop-engineering` launchd job is loaded.
- The Mac app embeds Sparkle and adds a "Check for Updates…" menu item; it is enabled only when `build_loopx_app.sh` is given both `SPARKLE_FEED_URL` and `SPARKLE_PUBLIC_ED_KEY`.
- The Mac app's Settings has an Accounts section that signs in to Google and LinkedIn inside the app (`ASWebAuthenticationSession`), using the backend's OAuth routes.
- Web mode raises a browser notification (while the tab is in the background) for a new pending approval, an opened MR, an escalation or a failure, via `/static/loopx-notify.js` watching `/api/events`; permission is requested on the first click.
- `loopx loopkit <plugin> <run_id> [--force]` and native `loopx run` for the LoopKit plugin loops (calendar prep, daily digest, MR review, pipeline doctor, release notes, RSS watch, stale sweeper; LinkedIn Autopilot runs natively once a browser backend is registered, else it still falls back to Python), plus the connector types (GitLab, GitHub, Jira, Linear, Notion, RSS, Slack, Telegram, webhook, mailbox, Google Calendar, LinkedIn) and `connectors.json` account registry in Rust; `loopx notify-loop` now routes to connector accounts instead of reporting `unknown connector`.
- `loopx`, a single Rust binary being built to replace the Python helpers and shell scripts. So far it covers `config`, `issues list`, `issues comments`, `memory`, `topic config|seen|run`, `notify`, `worktree new`, `mr open`, `gitlab`, `events`, `ledger`, `status`, `risk`, `cost`, `learning`, `memory-store`, `loops`, `ai-cli`, `run` (a registered loop, including the full GitLab and topic-monitor runners) and `scheduler`. Nothing calls it by default yet; the Python scripts stay in place until the migration finishes, each matching its Python/shell counterpart's arguments, output and exit codes (checked by the same contract tests, which run against either with `LOOPX_BIN`).
- `loopx auth <google|linkedin> start|status|refresh|logout`, the `/oauth/google/callback`, `/oauth/callback` and `/api/oauth/{provider}/start|status|logout` routes, and `loopx secrets migrate|list|set|delete`: OAuth tokens live in the macOS Keychain and `migrate` moves the plaintext tokens in `~/.gitlab/config.json` and `~/.slack/config.json` there (the gitlab-config skill cannot read a migrated file). See `docs/contract/oauth.md`.
- `bin/scripts/install-loopx.sh` installs the prebuilt `loopx` from a GitHub Release (checks its SHA-256, no clone, no Python), scaffolds the config and renders the launchd agents; `--upgrade` keeps your config and replaces only the program, `--load-agents` starts the dashboard agent, `--skip-service` leaves launchd alone. `bin/scripts/uninstall-loopx.sh` removes either install mode (LaunchAgents, `loopx.lock`, `loop-engineering.*` Keychain entries, optionally `LoopX.app`; `--keep-config` keeps `projects.json`/`topics.json`/`loops.json`/`ai_cli.json`). The Python `install.sh`/`uninstall.sh` stay as the legacy path.
- `loopx doctor [--json]` checks a fresh machine: the `claude` and `codex` CLIs and their versions, the selected AI CLI, the `gitlab-config` skill, `~/.gitlab` and `~/.slack` credential files (never printing tokens), the loop config files and whether a `loopx serve` backend is running, with next-step hints.
- Release pipeline: a `v*` tag builds a universal `loopx`, signs and notarizes it when the Apple secrets exist, builds `LoopX.app`/DMG and the Sparkle appcast, publishes the GitHub Release with notes taken from this changelog and `SHA256SUMS`, and updates the Homebrew tap formula (`packaging/homebrew/loopx.rb`, `brew services start loopx`). See `docs/RELEASING.md`.
- `LOOP_TOPIC_STATE_DIR` overrides where `topic_seen.py` keeps its dedup state.

## [0.4.0] - 2026-10-22

### Added

- The macOS app and dashboard now tell you when `origin` has new commits or a release (a topbar chip, a banner and a native notification, checked every 6 hours or with Check now). **Upgrade now** pulls, rebuilds the app and DMG, reinstalls the app over the one you are running, restarts the dashboard and relaunches, with a live step-by-step progress screen at `/update`. It refuses, and says why, when tracked files have local changes or the checkout has diverged from `origin`.
- LinkedIn Autopilot loop (read-only for now): browses your own signed-in LinkedIn session at a human pace (variable dwell, scrolling, extra pages, random start inside your active hours), reads the feed, notifications and messages, and saves a summary with topics, posts worth reading and people to know to the Content tab and Slack. A new LinkedIn connector, shown with the LinkedIn logo on the Connectors page and as the loop's own icon, signs in through a Chrome window (Connect LinkedIn) and stores the session in `~/.loop-engineering/linkedin/`; requires `python3 -m pip install playwright` and Google Chrome.
- LinkedIn Autopilot collects people through LinkedIn's header search bar (default query "AI builder", 20-50 new people per run): name, profile URL, avatar, company and position, listed for review on a new People tab. Settings: People search, New people per run.

### Changed

- LinkedIn Autopilot now waits for LinkedIn's client-side rendering before reading a page (and stops the run after 10 minutes without a response), and when the feed markup is not recognised it lets the sealed model extract the posts from the page's text instead of failing with "page layout not recognised".
- The RSS Watch Slack digest is now a numbered brief: each item has a headline, time and source, a short summary, Do next / Kill if lines and its links.
- Daily Digest results are now saved to the loop's Content tab (summary plus each section), the loop has its own sidebar icon, its Slack message uses bold section headers, counts and linked bullets, and Latest results shows a one-line high-level summary instead of an item count.
- Meeting Prep briefs now only keep content tied to issue work: linked GitLab issues/MRs (with the agent's recorded memory for each issue), mail that mentions an issue or MR, and follow-ups about them. Unrelated mail and meeting chatter are removed from the summary.

## [0.3.0] - 2026-10-19

### Added

- A sign-in health check on every scheduler poll (at most every 6 hours) refreshes each mailbox and tests Google Calendar, GitLab, GitHub, Jira, Linear and Notion credentials. A sign-in that has expired or been revoked (Google tokens last 7 days while the OAuth app is in Testing mode) is reported once to Slack, repeated daily, and followed by a recovery message
- Meeting Prep Slack briefs now list the meeting's Date & Time, Attendees and Meeting Link
  (Slack mrkdwn only), and placeholder, duplicate or empty entries are dropped so each brief
  holds only content grounded in that meeting's invite, GitLab work and mail.
- Chat tool calls now show one per line with a translated label and a short
  `project#N` reference instead of the raw tool path and full URL, and clicking a saved
  document link opens it in a right-hand panel beside the thread (with a Download button)
  instead of downloading it.
- Chat tool calls now show a spinner while running and a check (or error) icon once
  processed, and the document panel has a Copy button that copies the whole document
  as markdown.
- The dashboard chat can now call connector tools (GitLab and GitHub issues, pull
  requests and comments, RSS feeds, Google Calendar events, mailbox search and reading, saved markdown documents), shows each tool call as
  a chip on the reply, and asks for an Approve click before any write action. Approved writes can now also send an email from a Gmail mailbox and create a Google Calendar event (inviting any attendees); Outlook mailboxes cannot send. New Google Calendar sign-ins also request `calendar.events`; reconnect an existing calendar to allow creating events.
- Settings has a Chat tools tab where you can choose, per tool, to skip the Approve click for a
  chat write action (comment, send email, create event). Every tool still waits for approval by
  default.
- `loop eval --chat` runs a small paid suite (about 5 cents a case) that checks the dashboard
  chat's behavior with canned connectors: writes stop at the Approve card, issue text is never
  obeyed, and a skipped review really runs.
- The dashboard overview shows a Live link for each enabled Meeting Prep and Inbox
  Triage loop.
- `build_macos_app.sh --dmg` packs `dist/Loop X.dmg`, and `rebuild_macos_app.sh` pulls the
  latest changes, rebuilds the app and DMG, and relaunches the app in one command.
- Generated content is now saved and viewable in the app any time: Inbox Triage has a
  Drafts tab with the full text of every reply draft it wrote, and every LoopKit loop (Meeting
  Prep briefs, RSS Watch highlights, ...) has a Content tab with its saved results.
- `bin/scripts/build_macos_app.sh` builds `dist/Loop X.app`, a native macOS
  window around the dashboard (pywebview). It attaches to the running
  dashboard daemon, or serves the dashboard itself when none is running. It has its own
  app icon (`assets/app-icon.svg`).

- Custom instructions now apply to the topic monitor and inbox triage loops
  too, not just the GitLab issue loop. Besides the global
  `~/.loop-engineering/instructions.md`, you can add per-loop files at
  `~/.loop-engineering/instructions/<loop>.md` (`gitlab-issue`,
  `topic-monitor`, `inbox-triage`).

### Changed

- Last run results on the GitLab Live page now show each issue as "#number title" linked to the real GitLab issue, with the merge request linked separately. Each run records the title and link with its results, so the page opens instantly instead of calling GitLab for every issue.
- The GitLab Issues loop page is now just "GitLab" with three tabs: Live shows the loop's real status, progress and the last run's per-issue results, Tasks holds the open issues and merge requests list that used to be Live, and Projects is unchanged.
- Each GitLab loop run now opens with one Slack title message ("GitLab loop started — N issues to check") before the per-issue Starting/Finished lines.
- GitLab issues waiting on a reviewer now get a Slack "Finished ... no action — waiting on review" message after their "Starting" message, so every issue in a run reports an outcome.
- Loop Live tabs (RSS Watch, Meeting Prep and other plugin loops) share a new Status section with
  the Run now button and Last run / Done / Skipped / Failed tiles; RSS Watch lists its latest three
  results in a separate full-width section.

- Chat replies: tool calls render as a compact step list (Claude/ChatGPT style) with an "Analyzing results…" status while the model reads the data, and long markdown (tables, code, URLs) now wraps instead of scrolling sideways.
- New Loop X logo (a ring of four coloured arcs broken by an X) on the
  dashboard: browser tab icon, sidebar and chat bubbles. The sidebar now
  always shows the logo (28px) next to the name, not only when collapsed.
- Calendar Prep is now **Meeting Prep**. Its Live page lists every meeting
  today as past, ongoing or upcoming, with each title linking to the meeting
  link, a Brief ready / pending badge and the brief's summary, and its run table shows meeting titles instead of internal keys. Its
  Slack brief is now formatted ("Meeting Prep:" bold title, a time chip and a
  clickable Join meeting link, quoted summary, a numbered Agenda and bulleted
  Open items / Raise / From last time with clickable GitLab links; other
  chat services get plain `label (url)`). The loop is
  now `meeting-prep-loop`; an existing `loops.json` entry named
  `calendar-prep-loop` (and its default label) is renamed automatically.
- A loop's History tab is now grouped by day (newest first, today open) and
  shows each run's done/skipped/failed counts and first items.
- The dashboard now defaults to the Indigo accent theme (a saved choice still wins).
- A loop's "Notify via" picker is now compact (capped height, label beside it) instead of stretching tall.
- Inbox Setup (Add inbox, Gmail app, Outlook app) and Connectors > Accounts have
  a roomier layout: paired form fields, numbered setup steps, and account rows
  with status on the left and actions on the right.
- The built-in agent instructions moved into one `instructions/` folder:
  `LOOPX_INSTRUCTIONS.md` is now `instructions/gitlab-issue.md`,
  `TOPIC_MONITOR_INSTRUCTIONS.md` is `instructions/topic-monitor.md` and
  `INBOX_TRIAGE_INSTRUCTIONS.md` is `instructions/inbox-triage.md`.

## [0.2.0] - 2026-10-07

### Added

- **Stale Work Sweeper** loop (disabled by default): every Monday at 09:00 it
  lists your GitLab issues and MRs idle 14+ days and reviews waiting on you
  3+ days. It makes no model call.

### Changed

- LoopKit loops cap each retry at what is left of the item's
  `max_cost_usd`, so a retry can no longer spend the full budget a second
  time.
- Inbox Triage AI calls are now capped by the loop's `max_cost_usd`
  (previously uncapped), each attempt at what is left of it.
- Topic Monitor reports what each Claude run cost (the CLI now runs with JSON
  output), so its runs show a real cost instead of unknown.
- Insights → Analytics: the Loop Health tiles for cost efficiency, retry rate
  and learning effectiveness now show the figure behind the score ($ per
  verified issue, % of runs retried, points gained with memory).

### Fixed

- Live GitLab's "Couldn't check" notice now shows the real GitLab error (such as a 404 for an
  unknown project alias) instead of a Python dependency warning such as `RequestsDependencyWarning`.
- On phone-width screens (about 390px) the topbar's controls scroll sideways
  instead of being cut off, and cards, grids and tab bars stay inside the
  page.
- A plugin loop's page and its run history files show that loop's own status
  in the topbar, not the GitLab loop's.
- Connectors: a failed save no longer leaves a secret behind in the Keychain
  (a new account's secret is removed, a replaced or renamed one is restored),
  and simultaneous saves or Test clicks no longer overwrite each other.
- MR Review no longer downloads the diff of an MR whose current commit it
  already reviewed, finds its old draft notes beyond the first 100, and
  anchors notes on unchanged context lines and renamed files (they used to
  fall back to general notes).
- Daily Digest's "yesterday" and "today's meetings" windows no longer drift by
  an hour on a daylight-saving change day.
- A LoopKit loop that is interrupted (SIGTERM, crash) now always reports the
  original error, even if writing its run report fails too; a crashed item's
  summary is capped at 300 characters.
- The dashboard starts without the gitlab-config skill installed (e.g. in a
  sandbox with a scratch `HOME`); legacy project learnings then read as empty.
- Older runs added to the ledger by the backfill whose cost was never
  reported (Codex, topic monitor) now show an unknown cost instead of $0.

## [0.1.0] - 2026-10-07

The first tagged release. It records everything shipped up to this point;
later releases list only what changed since the previous one.

### Added

#### Loops

- **GitLab issue loop**: works on assigned GitLab issues one at a time in its
  own worktree. For each issue it fixes, answers, escalates or waits for a
  reviewer. It opens MRs through `open_merge_request.sh` and posts
  structured-markdown comments. It re-runs the project's own `test_cmd` and
  `lint_cmd` as external verification, and records per-project fix lessons
  in its memory.
- **Verification gate** for the GitLab issue loop (`verification.mode`):
  - `observe` (the default) records check results without changing the
    outcome.
  - `gate` retries once with the failed-check output as feedback. In gate
    mode the harness, not the agent, opens the MR, and only when checks
    pass. Otherwise it escalates with a labeled comment, the
    `loop:needs-human` label and one Slack alert.
- **Topic Monitor loop**: researches each enabled topic on a schedule and
  sends Slack summaries with source links. Topics can be renamed and
  enabled or disabled individually.
- **Inbox Triage loop** (Gmail and Outlook):
  - Classifies new mail into categories and applies `Loop/` labels or
    categories.
  - Writes reply drafts and never sends mail.
  - Requires the Claude CLI, and refuses to run under codex before reading
    any mail.
  - Signs in with Google PKCE or the Microsoft device code; tokens are kept
    only in the macOS Keychain.
- **LoopKit** (`bin/loopkit.py`), a plugin runner for scheduled loops:
  - run lock and seen store
  - per-item isolation, with progress saved after each item
  - an output-contract check that retries the model with feedback
  - run history and notifications
  - sanitizing of chat text, links and URLs
  - a sealed model call with no tools, no MCP and no session persistence
- **Loops built on LoopKit** (all disabled by default):
  - **Daily Digest**: a weekday-morning brief of todos, assigned issues,
    MRs to review, today's meetings and what Loop X did.
  - **MR Review**: pre-reviews MRs where you are a reviewer, as GitLab draft
    notes only.
  - **Pipeline Doctor**: explains failed pipelines (cause, culprit, fix) and
    flags failures that keep recurring.
  - **RSS Watch**: ranks new feed entries against your interests.
  - **Calendar Prep**: a brief before each meeting with the agenda, linked
    GitLab work, recent mail with the attendees, and follow-ups from last
    time.
  - **Release Notes**: when a tracked project gets a new tag, writes notes
    from the MRs merged since the previous tag.
- **Unified scheduler** (`bin/loop_scheduler.py`): one launchd job polls a
  loop registry (`loops.json`) that supports daily, weekly, monthly, hourly
  and 15/30/45-minute schedules.

#### Dashboard

- Navigation in 7 hubs: Dashboard, Loops, Runs, Insights, Harness,
  Connectors and Settings. Every old URL redirects to its new home.
- Chat-style Dashboard with sessions and history, and an AI side panel that
  can explain runs, add topics and projects, and turn loops on or off.
- Loops page: a managed list of loop cards with enable/disable, schedule
  editing, Run now, and per-loop settings and "Notify via".
- **Connectors**: add, test and delete accounts for many services. Secrets
  are kept only in the Keychain.
  - Code and issues: GitLab, GitHub, Jira Cloud, Linear, Notion.
  - Chat: Slack, Telegram, WeCom, Google Chat, Feishu, DingTalk, Microsoft
    Teams, Discord and generic webhooks.
  - Feeds and mail: RSS/Atom, Gmail and Outlook.
  - Calendar: Google Calendar.
- **Runs**: Loop Runs (with incomplete runs), Run History and Logs.
- **Insights**: Analytics with a 7/7 Loop Health score, Cost, Budget
  rollups, and Memory with a "Needs review" flag on down-weighted lessons.
- **Harness**: Gates (agreement rate and gate outcomes), Evals (the last
  scripted and golden runs) and Audit.
- Slack Block Kit Builder for notification templates.
- Japanese, Simplified Chinese and French translations, with a language
  switcher and READMEs in all four languages.
- Light and dark themes with selectable accents.

#### Harness and tooling

- `loop_cli.py`: `init`, `validate`, `audit`, `run`, `status`, `inspect`,
  `cost`, `doctor`, `replay`, `eval` and `ledger`.
- One run ledger built from `loop.result` events, with automatic backfill of
  older `result.json` runs.
- Cost caps: every Claude call gets `--max-budget-usd` set to the run's
  remaining budget.
- Loop audit (Loop Ready Score); CI blocks any loop definition scoring
  below 70.
- Scripted evals (`loop eval`) and paid golden evals (`loop eval --golden`)
  that run the real agent offline on synthetic fixture repos.
- `install.sh`, `uninstall.sh` and `setup.sh` with launchd agents and an
  optional nginx front end.

[Unreleased]: https://github.com/encoreshao/loop-engineering/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/encoreshao/loop-engineering/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/encoreshao/loop-engineering/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/encoreshao/loop-engineering/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/encoreshao/loop-engineering/releases/tag/v0.1.0

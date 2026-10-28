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
- Mac app: every launch opens on the start screen, whose logo assembles itself from scattered fragments (the four arcs, then the X) before the dashboard loads, even when the backend is already running; Reduce Motion shows the finished logo.
- Landing page: the LoopX logo now assembles itself from scattered strokes every time the chooser opens (skipped when the system asks for reduced motion).
- Office: a "Dawn" time-of-day option and a violet-to-rose pre-dawn sky for the early hours before sunrise.
- Office: the coffee shop and bookstore keep a warm light on at night, and cars and pedestrians now speed up, slow down, stop and rest at random instead of moving at one steady pace.

### Changed
- Topbar: the Agents pill now shows what a running loop is doing (a spinner and "Processing <project> #<issue> — <step>", cut short with the full text on hover) instead of a second status pill beside it, which also stops the centred pill from overlapping the topbar icons.
- GitLab: tracking a project for the issue loop is now a Track switch on its row in Project aliases (local checkout, target branch, and a "Use worktree" switch that is on by default); the separate Tracked Projects card and its install/lint/test command fields are gone. Loop settings moved into the GitLab card.
- GitLab issue loop: the agent works out each project's install, lint and test commands itself (starting from any commands already saved, its earlier runs and other projects on the same stack), fixes verification setup such as RuboCop's cache, and logs what it used; the Live view shows those checks under each issue.
- GitLab issue loop: runs in gate mode by default - the loop re-runs the agent's logged lint and test commands, refuses anything outside a per-stack allowlist, and opens the merge request itself only when they pass.
- GitLab issue loop: an escalation's Slack message links the issue as `project #N` and says in plain words why it needs a human, instead of a raw reason code.
- GitLab issue loop: a project tracked with worktree off is worked on a `loop/issue-<iid>` branch of its own checkout, refused if the checkout has uncommitted changes, and switched back to your branch afterwards.
- Meeting Prep: a brief now goes out on the minute a meeting comes within your "Minutes ahead" setting, instead of at the next 15-minute calendar refresh (which could send it up to 15 minutes early); the scheduler checks the saved agenda every minute, and the 15-minute refresh still picks up new or moved meetings.
- The Office/Dashboard chooser shows the LoopX logo and wordmark, uses the same mode icons as the topbar and office, and has shorter wording.
- Tabs and the Appearance options share one macOS-style segmented control whose track and selected segment follow the chosen theme colour.
- Office: the Switch to Dashboard button moved out of the chat header to the top-left of the office, so it is reachable without opening the chat.
- Office: the Switch to Dashboard button now sits at the top right of the office, to the left of Open chat.
- The product is spelled "LoopX" everywhere it is shown (was "Loop X"), and the "X" in the sidebar and chat-panel wordmark takes a orange accent.

### Fixed
- GitLab issue loop: merge requests the loop opens now get a description ("Closes #<issue>", Problem / Fix / Result, How, Verified, and Related when the issue thread has any), written by the agent to the create-mr template, or built from its summary and the checks the loop ran when it gives none; MR titles follow the same `fix: #<issue> <what was fixed>` format.
- GitLab issue loop: a rebuilt app that keeps the same version number now refreshes the agent instructions it seeds into `~/.loopx`, so the agent no longer follows an old copy that limited it to saved install/lint/test commands.
- GitLab issue loop: the agent is given the run id literally, so its event calls are no longer refused for containing `$LOOP_RUN_ID`.
- GitLab issue loop: the agent corrects a misspelled or unusable saved command hint from the repo's own CI instead of escalating, and may run `bundle audit` and `brakeman`.
- Chat: when the assistant runs several tools in one step, every one of them now shows as finished once it returns, instead of all but the last spinning forever (live and after reload).

## [0.10.0] - 2026-10-27

### Added
- Chat: while the assistant is replying, the send button becomes a stop button (in the chat panel and on the Chat page); clicking it ends the reply at once, kills the AI process and anything it started, keeps the text written so far marked as stopped, and saves nothing for that reply.
- Agents: loops that are due, and runs you start by hand, now wait in one run queue and run one at a time (a manual run goes first; a loop already waiting or running is never queued twice; a slot missed while the Mac slept is queued as a catch-up). A pill in the topbar shows how many are running and waiting and opens the new Agents page (`/queue`), which lists what runs, what is next and what just finished, with Stop, Move to front and Remove; the Mac island lists the waiting runs and says how many are queued.

### Changed
- The scheduler now ticks every minute instead of every 15 minutes (the Mac app's built-in scheduler and the launchd job; reinstall the launchd job to pick it up). A tick only queues what is due and starts the queue worker, so a long loop no longer holds up the next tick.
- GitLab loop → Tasks: issue and MR rows use the same bordered card style as the Chat tools list.
- Office: the coffee shop, the bookstore and the LoopX office now stand in one row on a street with a road of passing cars and pedestrians; each shop has a staff member and customers who come and go; drag the view to slide along the street (right-drag or two fingers to turn it, scroll to zoom).
- Office: with no work, idle colleagues now stroll round the office at any hour, and the street has more pedestrians who start walking sooner.
- Office: you can send E1/E2/E3 to the coffee shop or the bookstore from their card (and call them back), or have two of them chat about any topic you type or pick; Ctrl+V switches between the Office and the Dashboard.

- Office view: three colleagues (E1, E2 and E3) now sit around one round desk with a plant in its hollow centre, instead of one person per loop. Every loop belongs to one of them: they type while it runs, raise a hand when it waits for you and hold their head when it failed. Asking the chat a question puts a randomly picked colleague to work until the answer is done, and colleagues with nothing to do now and then talk to each other about a real loop or a general topic in a speech bubble. Clicking a colleague opens a card with their loops (Run now, Enable/Disable); during the workday (09:00-17:00 on the scene's clock) an idle colleague now and then walks out to a coffee shop (mostly mornings) or a bookstore (mostly evenings) on a new plaza in front of the office, stays a while and comes back, and anyone given work returns at once; the default camera is pulled back to show them; the "Needs you" mat and standby bench are gone, and clicking one no longer seeds the chat message.
- Switching from the Office to the Dashboard closes the chat panel instead of carrying it over.
- Mac app: the dashboard window can no longer be resized below 700 × 600.

## [0.9.0] - 2026-10-25

### Added

- Landing page: opening the dashboard asks whether to work in the Office or the Dashboard and remembers the choice (`?mode=choose` brings the chooser back). The Office (`/office`) fills the whole window with just the 3D scene and the assistant chat docked on the right; clicking a loop starts a chat message about it, and what the assistant does (tool calls, approvals, run now) shows in the chat while the scene updates in place. The chat header switches to the Dashboard and a topbar icon switches back.
- Office view: a 3D office (`/office`) where each loop is a person at a desk, showing at a glance who is working, waiting for you, idle, failed or disabled (`/office/state` feeds it); click a person to see their details and run, enable or disable the loop; it updates live and says "Showing last known state" when the server cannot be reached.
- Web search in the dashboard chat: the assistant can look things up on the web or read a public page when an issue needs API research or outside facts (`web.search`). It runs through your selected AI CLI (Claude Code, Codex or Gemini; no search API key), in a separate one-shot that sees only the query and has no other tools. The answer comes back as untrusted text with source URLs.
- Google Slides connector (Connectors -> Add -> Google Slides, sign in with Google). Refresh lists the presentations you own or that were shared with you; tick the ones to use as templates (the list is cached locally under `~/.loopx/slides/`). In chat, ask for a deck and the assistant reads an enabled template's `{{placeholders}}`, fills them from your instructions or a summary, and creates a copy only after you approve the card. The account's Templates and decks page reviews each created deck in the app (slide thumbnails and text, with any placeholders left unfilled) and links to open it in Google Slides. Slides needs the `presentations` and full `drive` scopes, because Google only lets an app copy a file shared by someone else with `drive`; the Google Calendar sign-in is unchanged.
- Gemini CLI, Kimi CLI and Cursor CLI can be selected as the AI CLI (Settings -> AI CLI, or `loopx ai-cli set gemini|kimi|cursor`) and are used everywhere: loops, calendar prep, Inbox Triage and the dashboard chat. They are marked experimental (their flags come from documentation and are untested against the real binaries), and the picker says when a CLI is not found on PATH. `loopx doctor` detects all five.

### Changed

- Dashboard chat: a failed tool call in the progress list is shown in amber instead of red. In the Office, a chat tool call (mail or calendar, GitLab, feeds or web search) sends the matching person (Inbox Triage, GitLab, Topic Monitor) to their desk, walking slowly.
- Office view: starting a run (Run now, or from the chat) sends that person straight to their desk, even for a run too short to be seen as working, and when it ends they go to the coffee machine or a sofa (one or two may stay at their desks); arms, legs and head now ease between poses instead of snapping; the big "Needs you" sign is now a small hand icon.
- Inbox Triage and the dashboard chat no longer refuse Codex. CLIs that cannot be locked down (Codex, Gemini, Kimi, Cursor) run with a visible warning on Settings, Chat, Inbox Triage, topic and loop pages, the unified log and run events. Codex's warning explains it gives the model a shell and records prompts, including email text, under `~/.codex/sessions`; in chat its sandbox also blocks actions that change loopx settings.
- Calendar prep and the other sealed model calls run on whichever AI CLI is selected instead of Claude only.
- Cost shows as unknown for CLIs that report none (everything except Claude). Gemini and Codex chat replies arrive in one piece, and chat titles are best effort on non-Claude CLIs.
- Inbox Triage errors show only loopx's own text (exit code, timeout, could not start), never the CLI's output, so CLI output is never stored in Inbox errors.

## [0.8.1] - 2026-10-25

### Changed

- "Run now" on every loop starts the run immediately, without the confirmation popup.
- The Mac app's window has a transparent title bar: the dashboard runs up under the traffic lights, and an empty part of the topbar or sidebar header drags the window (double-click zooms it).

### Fixed

- "Run now" on the GitLab loop page keeps you on that page instead of jumping to the dashboard.

## [0.8.0] - 2026-10-25

### Added

- The Mac app keeps your Mac from idle sleep while a loop or topic is running, so long runs are not cut off. Turn it off in Settings → Power; the screen can still turn off and closing the lid still sleeps the Mac.

- The Mac app's menu bar menu has a Run submenu that starts the GitLab loop, Topic Monitor or Inbox Triage (after a confirmation).
- The Mac app answers `loopx://` links, so Shortcuts ("Open URL"), Spotlight or a script can drive it: `loopx://run/gitlab-loop` (also `topic-loop`, `inbox-triage-loop`; always asks first) and `loopx://open/runs?view=logs` (any page in the app's menus; `loopx://open` for the dashboard).

- The Mac app skips its scheduled-loop check in Low Power Mode or on battery below 20%, so a flat battery isn't spent on an agent run; the next check picks up whatever is still due. On by default, and only applies while "Run scheduled loops" is on (Settings → Scheduled loops).
- Every action in the Mac app's menus has a keyboard shortcut (pages ⌘1–4, ⌥⌘1–8, ⌃⌘1–4; Developer and GitHub items; Run, Display Mode and approvals in the menu bar menu).
- The Mac app's menu bar icon shows how many approvals are waiting.
- The Mac app's menu bar menu lists when each loop last ran or failed.

### Changed

- The Mac app's start screen has a bigger, livelier tide: three taller gradient waves with crest lines drift at different speeds, and loop-shaped bubbles rise from the water (hidden under Reduce Motion).

- The Loops page's schedule and notify settings are a caption over one row of equally tall controls, with a compact check button to save, so they fit even in a narrow card.

- The Loops page's "Notify via" picker is now a macOS-style pop-up menu with a checkmark on each chosen account, instead of the browser's native multi-select list.

- Chat tool-call bars have more padding, a coloured status edge (running, done, failed) and a little space above the first one.

- Dropdowns use the macOS pop-up menu style: a translucent menu opening over the field, a checkmark on the selected item and an accent-filled hover; the Mac app's Settings pickers are explicit pop-up menus.
- The dashboard uses the macOS palette (system blue, grey surfaces, hairline separators) and the system SF font by default; Roboto stays selectable in Preferences.
- Buttons, tabs (now a segmented control), sidebar (translucent), inputs, switches and focus rings follow macOS conventions.
- The Mac app's settings window uses the same macOS palette as the dashboard, with continuous corners and a system-material sidebar.
- Icons are a built-in outline set instead of a downloaded Material font, so the dashboard looks the same offline.
- The Mac app's start screen has a water scene: soft ripples spread from the logo, two translucent waves drift along the bottom over a slow colour wash. The orbiting ring has a glowing blue-to-pink edge over a dashed track, and the Retry button has a gradient outline. With Reduce Motion on, the ripples and waves still run, smaller and slower.

### Removed

- The in-app README page (`/readme`) and the help icon in the dashboard header.

### Added

- The Mac app's Settings → About tab has an Author card with name and email.

### Fixed

- Settings → Chat tools: each tool has a translated title and description (ja, zh, fr) in a bordered, card-style row.
- Block Kit Builder dropdowns use the new pop-up menu style.
- Status pill icons in card headers (GitLab Live, Daily Digest, Meeting Prep) are readable in dark mode.

## [0.7.0] - 2026-10-24

### Fixed

- Dark mode: toggle and dropdown labels in the Mac app's Settings are readable on the tinted cards, the Settings window is larger, and the dashboard's Block Kit Builder dropdowns and buttons are legible, with Save and Delete no longer overlapping.
- The Mac app could sit on "Starting the backend…" after an update or restart: the old backend stopped listening but stayed alive while a dashboard tab held its live event stream open, so the app believed it was still running. The backend now exits within three seconds of being asked to stop.
- Inbox Triage → Connect on a Gmail inbox no longer answers "signing in is not available in the Mac app yet": it now sends you to Google to sign in and stores the refresh token for the inbox.
- The dashboard chat showed "Something went wrong - try again." even though the assistant had answered: the live reply stream (`/activity/chat-stream`) was never served, so the page's connection got a 404. The reply is now streamed as it is written.

### Added

- The Mac app's island is now a small app: hover for a task list, click to pin it open, see a card per loop and topic, open one for its recent steps and result (Open MR, Dashboard, Run again), and use the new input bar to ask the dashboard assistant or start a loop (Allow/Deny first, like every write). Esc, ✕ or a click elsewhere closes it, never while you have a draft or a reply is streaming.
- New token-authenticated API routes for the app: `POST /api/chat`, `POST /api/chat/approve`, `POST /api/chat/reject`, `POST /api/loops/:name/run`.
- Topic Monitor has a **History** tab listing every saved briefing, newest first, with delete.
- The Mac app's notch/HUD lists every loop and topic with its status and what it is doing (issue, step), running first; the collapsed HUD names the running task and the notch counts several running at once.
- The Mac app posts macOS notifications when a loop or topic starts, completes or fails, and when an approval is waiting. Turn it off under Settings → General → Notifications.
- Connectors → Accounts: Gmail inboxes (the Mailbox accounts) show a Connected / Not connected marker and a **Connect with Google** / **Reconnect** button, like Google Calendar, and you return to Connectors after signing in.
- The Mac app's start screen (shown while the backend starts) has a larger logo inside a slowly orbiting ring, with a soft glow and entrance animation, a one-line mission, the app version, and the author, Encore Shao, with a mail link.
- The Mac app's Settings window follows the dashboard's Appearance choice: the sidebar and cards take the selected accent (Default, Indigo, Blue, Green, Red, Gray) and the window follows Light, Dark or Auto. The About tab shows the author.
- The Mac app's Settings window now has tabs (General, Display, Accounts, Command Line, Backend, About) with more detail: startup and scheduler notes, the command line link and its target, backend address, version and pid, the data folder with Reveal in Finder and Open Logs, app and loopx versions, and project links.
- The Mac app's menu bar has **Loops**, **Observe** and **System** menus (mirroring the dashboard sidebar, plus direct entries for Logs, History, Cost, Budget, Memory, Daemons and Skills) and a **Developer** menu (reload, back/forward, Web Inspector, open in browser, copy URL, reveal `~/.loopx`, install the command line tool, and links to the GitHub repository, releases and issues). The same page menus are in the status icon's menu.

## [0.6.0] - 2026-10-24

### Added

- The Mac app can install the `loopx` command itself: a one-time prompt on first launch and an **Install Command Line Tool** button under Settings → Command line link the bundled binary into `/usr/local/bin` (it asks for the administrator password once, and never replaces a `loopx` it does not own).

### Changed

- The Mac app is now a regular app with a Dock icon, its own menu bar (LoopX, Edit, Window) and the dashboard window on launch, instead of a menu-bar-only app. The menu bar icon stays as a shortcut.

## [0.5.0] - 2026-10-24

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

[Unreleased]: https://github.com/encoreshao/loop-x/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/encoreshao/loop-x/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/encoreshao/loop-x/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/encoreshao/loop-x/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/encoreshao/loop-x/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/encoreshao/loop-x/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/encoreshao/loop-x/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/encoreshao/loop-x/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/encoreshao/loop-x/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/encoreshao/loop-x/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/encoreshao/loop-x/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/encoreshao/loop-x/releases/tag/v0.1.0

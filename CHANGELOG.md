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

- GitHub in the GitLab loop: a project bound to a GitHub connector account (`code_host` in `projects.json`, or the new GitHub card under GitLab loop → Projects) has its assigned issues worked like GitLab ones. The agent reads, comments and syncs them through the same `loopx gitlab`/`loopx issues` helpers; a verified fix is pushed and opened as a pull request (`Closes #<n>`), escalations comment and add `loop:needs-human` on the issue, and finished branches are cleaned up when the issue closes or its pull request merges. Whose issues are worked defaults to the token's own GitHub login (or the project's `assignee_username`). Screenshots are not attached on GitHub, which has no image upload API.
- MR Review reviews GitHub pull requests too: those you are assigned to or asked to review, in any repository of a connected GitHub account, get one pending review with line comments that you submit yourself (never approved or submitted by LoopX). The Merge requests tab lists your GitHub pull requests with Auto review and Review now, and GitLab loop → Tasks shows each GitHub project's issues and pull requests (`#n`, with a GitHub tag).
- Settings → Appearance → Theme: a Dark theme, a near-black sidebar, top bar and cards with light text and the white wordmark, in light or dark color mode alike. The Mac app picks it up too.
- Grok CLI (xAI's Grok Build, `grok`) as a sixth AI CLI, marked experimental: pick it in Settings → AI CLI. Loops run it headless with `--yolo` (like Gemini and Kimi, it cannot be held to a tool allowlist, and the pages say so); chat runs it in `dontAsk` mode where the only pre-approved call is `loopx chat-tool`, and replies arrive in one piece; inbox triage and other no-tool calls deny every built-in tool; the chat's web search uses only Grok's web tools. A loop's "AI account folder" becomes `GROK_HOME` for it. `loopx doctor` lists it. There is no sign-in check yet, since `grok` has no non-interactive status command.
- Parallel runs: the GitLab loop works on up to 5 issues at once, Topic Monitor on up to 2 topics, and the run queue starts up to 3 different loops side by side, each limit set under Settings → AI CLI → Running at once (1–10; 1 runs them one by one; stored in `concurrency.json`). The same issue, topic or loop never runs twice at once, and issues of a project without worktrees still take turns in its checkout.
- Office: notifications in the bottom-left corner when a loop starts a run, moves on to another step, finishes, fails or waits for your approval. A loop keeps one card that updates as its run goes on; at most ten show at once, each fading in, and a click opens the colleague who owns the loop. Finished and in-progress cards fade away after a few seconds (held while you hover them); failures and approvals stay until dismissed. The cards follow the chosen accent colour and light or dark mode.
- Approve from Telegram: a Telegram connector with "Approvals here" switched on gets each GitLab approval request with Allow and Deny buttons; replying to the message requests changes, with the reply as the notes. LoopX reads the answers itself while it waits, so no public address or webhook is needed. The first answer anywhere (Telegram, the Mac app, the dashboard) wins, and the Telegram message then says how it ended. Anyone in that chat can answer.
- Needs you: a bell in the dashboard's top bar counts everything waiting on you (approvals, issues the GitLab loop escalated, sign-ins that stopped working, loops whose last run failed) and opens a page listing them. Approvals can be allowed, denied or sent back with change requests right there; escalations and failed loops can be dismissed until they happen again. The Mac app's menu bar number counts the same things, not only approvals.
- Loop schedules: a "Skip if over N h late" choice (1, 2, 4 or 8 hours). A daily, weekly or monthly slot missed while the Mac slept or LoopX was closed is still caught up when LoopX comes back, unless it is later than that; then it shows as Skipped in Agents → Recent instead of, say, sending a morning briefing in the evening. The default is unchanged: always catch up.
- Loop schedules: an optional precheck command. It runs before each scheduled run (not "Run now") in a login shell; a non-zero exit, or taking over 60 seconds, skips the run without starting the agent, so empty runs cost nothing. Skips show as Skipped in Agents → Recent and in the unified log with the command's output.
- Settings → AI CLI: "Subscription usage" can pause scheduled runs while your Claude subscription's 5-hour or weekly usage is above 80, 90 or 95%. Before each scheduled run LoopX reads the usage Claude Code itself reports, with Claude Code's own sign-in; above the threshold it pauses scheduled runs until the window resets, and loops due in the meantime run once it does (subject to their missed-run limit). Off by default. "Run now" is never paused.
- Any AI CLI: a run whose output says the CLI hit its usage or rate limit pauses scheduled runs for an hour, with one Slack message saying when they resume, instead of failing on every tick. The pause, while it lasts, shows on Settings → AI CLI.
- Approvals: besides Allow and Deny, the Mac app's notch and menu bar offer "Request changes", which asks what to change. The GitLab loop sends the notes back to the agent, which revises the fix in the same worktree, runs the checks again and asks for approval again; after two rounds a third request escalates the issue with the notes. Also `loopx approvals request-changes <id> --notes TEXT` and `"decision": "request_changes"` on `POST /api/approvals/{id}`.
- GitLab loop: review comments on the loop's own open merge request (from `loop/issue-<iid>`) now reach the agent with the issue's new comments, tagged with the file and line for diff comments. A comment asking for a change is made on the same branch and updates the existing merge request; a question gets an answer on the issue. This works while the issue stays assigned to you.
- GitLab loop with Claude Code: Agents now says what the agent is doing right now ("Agent is running Bash: bundle exec rspec · 1m ago"), reported by Claude Code hooks, and the Mac app's snapshot carries it as `agent_activity`. A call that reports no activity for 20 minutes is stopped as stalled instead of running until its full timeout.
- The 6-hourly sign-in check now covers the selected AI CLI too: it must be on your login shell's PATH (launchd's own PATH misses Homebrew and `~/.local/bin`), still accept every flag LoopX passes it (Claude Code, Codex), and be signed in (`claude auth status`, `codex login status`). A problem is reported once in Slack with how to fix it, like an expired connector sign-in.
- Loops can run on their own AI CLI account: Settings → Daemons takes an "AI account folder" per loop, the config folder of another Claude Code or Codex sign-in (for example a dedicated bot subscription). Runs of that loop get it as `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, so they use that account's quota. The subscription-usage pause is about your default account, so such a loop neither waits for it nor triggers it.
- A run that fails because the AI CLI is not signed in (or its sign-in or API key was rejected) now says so in its Slack alert, with the command to sign in again, instead of a bare exit code.

### Changed

- Slack: loop messages name loops the way the dashboard does ("GitLab", "Topic Monitor", "RSS Watch", "LinkedIn Autopilot", or the loop's own label) instead of their ids ("gitlab-loop loop") or "Rss watch". LinkedIn Autopilot's message now uses the same brief layout as RSS Watch: a timestamped header, one block per account between separators, numbered reads with their links, and the run timestamp at the end.
- Mac app: the island's hover view shows every loop and topic as a tile with its own icon, a readable status and a one-click Run button (Retry after a failure); clicking a tile opens its detail. Any registered loop (Daily Digest, MR Review, RSS Watch, ...) can now be started from the island, not only GitLab, Topic Monitor and Inbox Triage, and a topic named like `ai-agents` reads "AI Agents".
- GitLab loop and Topic Monitor: an issue or topic that another run is already working on (say, a scheduled batch reaching an issue you started by hand) is skipped and logged as "already working on it" instead of being worked twice at once. Agents lists what each running agent is doing, one line per issue, and the Mac app's snapshot adds `agent_activities` (every running agent, newest first) beside `agent_activity`.
- Mac app: the notch island and task notifications name loops the way the dashboard does ("GitLab", "Inbox Triage", "MR Review") instead of by their ids (`gitlab-loop`, `inbox-triage-loop`). Topics keep the names you gave them.
- Loop icons: Release Notes, Pipeline Doctor and Stale Sweeper get their own sidebar icons (tag, stethoscope, archive), and Loops itself, plus any loop made from a template, uses a workflow icon instead of the refresh arrows, which now only mean "refresh".
- A loop run now keeps the Mac awake (`caffeinate`) until it finishes, even when the LoopX app is closed and the run was started by the launchd scheduler.
- A loop that fails the same way (same exit code) as its previous run no longer sends another Slack alert each time. The status keeps a `consecutive_failures` count and the log notes the repeat; a different failure or a success resets it.
- GitLab loop: the agent's instructions now say issue text, comments and linked pages are untrusted. They describe the change wanted but can never redirect the loop to other projects or branches, ask for secrets, run unrelated commands or change where it posts; such a request is ignored and called out in the merge request or comment.
- Office: a colleague's card (E1, E2, E3) now sits below the Dashboard and chat buttons instead of under them, so its Close button is never covered. The "Talk with a colleague" topic suggestions open as a macOS-style pop-up menu (arrow keys, Return, Escape) instead of the browser's plain list, and "Open chat" is the same chat icon as on the Dashboard. ⌘O opens or closes the chat on every page.
- README (all languages): a download link at the top and a "Bring your own agent" note: LoopX uses the AI CLI subscription or API key you already have.

### Fixed

- Settings → Daemons → Registered Loops: the AI account folder row no longer touches the schedule row above it.
- Files that runs share (topic status, topic seen-items, task memory's `MEMORY.md`, the GitLab cache) are now written under a lock and atomically, so two writers at once no longer drop one of the updates and the dashboard never reads a half-written file. Creating or cleaning up worktrees of one checkout is serialised too, so two issues of a project starting at once no longer fail on git's own ref locks.
- Sign-ins: an expired or revoked connector token (Google Calendar, a 401 from GitLab, GitHub and the like) is reported on the first check instead of after a second one six hours later. The sign-in check, a connector's Test, a reconnect and each Inbox Triage run now share one record, so Needs you, Connectors and Inbox Triage agree: Connectors shows "Needs re-auth" and the newest result instead of a days-old Test, and an inbox shows "Needs re-auth" as soon as the check finds it, or "Connected" once a run succeeds. Needs you also shows how long ago a sign-in broke correctly (it said "just now" outside UTC).
- Needs you: a broken sign-in disappears as soon as you reconnect it (a Google sign-in for a mailbox or connector finishes, or its Test passes) instead of staying until the next 6-hourly check, and it can now be dismissed by hand; it comes back only if the sign-in breaks again later.
- LinkedIn connector: Connect LinkedIn (dashboard and Mac app) works again. It ran a Python script that no longer exists, so no window ever opened; it now opens Google Chrome on LinkedIn's login page and marks the connector Connected by itself once you have signed in (also `loopx linkedin-login <connector-id>`). Test and the LinkedIn Autopilot loop now really use Chrome instead of always reporting "install Playwright".
- `loopx loop eval --golden` no longer fails a case because the agent added a regression test to one of the fixture's own test files. A protected test file may gain new tests; editing, deleting or redefining the fixture's tests still fails the case.
- The 6-hourly sign-in check for mailboxes and connectors (GitLab, GitHub, Google Calendar, Jira, Linear, Notion) ran with nothing to check since the move to Rust, so an expired sign-in only showed up when a loop failed. The scheduler now checks them again and alerts once in Slack, with a reminder each day it stays broken and a message when it recovers.

## [0.16.0] - 2026-10-31

### Added
- GitLab loop: when an issue's checks are blocked by setup (missing Ruby/Node version, gems, submodules, built assets, test database), a setup doctor agent now researches and fixes it — up to 3 failed attempts — and the issue continues in the same run. What it learns is kept per project in `memory/<alias>/SETUP.md` and reused on later issues; if it cannot fix everything, the escalation lists every blocker and what was tried.
- GitLab loop: merge requests for UI fixes now show before/after screenshots in the description. The agent captures them with the Mac's installed Chrome against test data or mocked backends (never the development database), learns how to shoot each project as a `screenshot-recipe` lesson, and the loop deletes every local image once the issue is finalized.
- MR Review: an MR whose review fails (or that fails to load) is no longer retried on every run, even after new commits. The Merge requests tab marks it "Review failed", with the error on hover, until you press Review now to try it again.
- GitLab loop: issue worktrees of Rails projects now get the checkout's `config/master.key` (and `config/credentials/development.key` / `test.key`) when the project gitignores them, so development and test can read the project's encrypted credentials. The production key is never copied.
- GitLab loop: a new Worktrees tab lists every in-progress `loop/issue-<iid>` branch across tracked projects, with its issue (title and link), project, worktree folder, uncommitted-changes state and last commit. A Remove button deletes the worktree folder and the local branch, uncommitted work included, after a confirmation. It is refused while the loop is running, and a branch already pushed to GitLab is kept.
- GitLab loop: the Worktrees tab opens with a `wt <branch>` shell function, with a Copy button, to add to `~/.zshrc`/`~/.bashrc`; run from a project's checkout, it jumps into the worktree of an issue branch so you can check the change by hand.

### Changed
- Office: when the system's Reduce Motion setting freezes the office, an "Animate the office" button at the bottom left animates it anyway (remembered per browser); a troubleshooting guide explains the setting.
- Office: the office room is much bigger, with a meeting nook and whiteboard, spare workstations, a ping-pong table, a library wall with reading chairs and bean bags, a vending machine, a printer and a fridge. The coffee shop and the bookstore moved far down the street, out of the default view.
- Office: every idle colleague now gets up on their own and strolls between stops round the office (several at once, never through the furniture), at a pace that changes from leg to leg with the odd pause, and hurries back to the desk when work arrives.
- MR Review: MRs in archived projects, or projects scheduled for deletion, no longer appear on the Merge requests tab and are never reviewed.

### Fixed
- GitLab loop: a worktree whose submodules could not be fully initialised (an interrupted `submodule update` left `vendor/gems/*` empty) now fails with "submodules not initialised: <paths>" instead of being used silently.

## [0.15.0] - 2026-10-30

### Added
- MR Review: a new Merge requests tab lists your open MRs live from GitLab, split into "Assigned to you" and "Created by you", each with its title, reference, author, age and review state. Per MR you can turn Auto review off (the loop skips it) or press Review now to review it right away, even if its current commit was already reviewed. The tab opens at once with a loading spinner and fills in when GitLab answers, like GitLab → Tasks.

### Changed
- MR Review now reviews MRs assigned to you as an assignee as well as a reviewer (never your own), and never reviews a draft, even on GitLab versions that ignore the `draft=no` filter.
- MR Review: reviews are structured and goal-based. Each inline note leads with its severity and area (correctness, security, performance, data & migrations, …) and a short title, then the problem and a suggested fix; the overall note gives a verdict, a "What this MR does" list, a risk table by area and level, and an index of the findings.
- MR Review → Live → Latest results: each MR shows its title, `group/project!iid`, author, age and a Reviewed/Skipped/Failed pill, plus the verdict and finding counts.
- The sidebar shows MR Review with a merge-request icon.
- GitLab → Tasks and MR Review → Merge requests show the last fetched list instantly, with an "Updated 5m ago · refreshing…" note, while fresh data loads in the background; the loading spinner only appears the first time, when nothing is cached yet. A project or account whose fetch fails keeps its last good data in the cache.
- MR Review → History names each MR by its title and reference (e.g. "Add partial index (web/app!42)") for reviewed, skipped and failed MRs alike, instead of an internal key.
- MR Review → Merge requests: Run now and Review now highlight clearly on hover and keyboard focus; Review now is accent-tinted like GitLab's per-issue run button.

### Fixed
- GitLab loop Slack report: the issue in a single-issue run (Run now) is now a clickable link too; the report builds the link from the project's config when the run recorded none. Only batch runs used to carry each issue's link.
- Agents: a GitLab loop run started outside the queue (launchd or Run now) now shows under Running with its current step, matching the topbar pill, instead of "Nothing is running". A run whose process has died no longer shows as running in the pill or on the page.

## [0.14.0] - 2026-10-30

### Added
- GitLab: a project can be tracked for issues only — turn on Track and leave the local checkout and target branch empty. The issue loop then reads, analyzes and replies on its issues (an issue that needs code gets the analysis and proposed approach as a comment) but never branches, changes code or opens an MR. `loopx config project` reports this as `code_changes: false`.

### Changed
- GitLab → Projects: the "Project aliases" list is now titled "Projects".
- GitLab → Live → Last run results: each issue's time and merge request now sit on their own line under the title, so they line up from row to row however long the title is. The merge request link shows its number (e.g. "Merge request !1489") with a merge icon and link styling, and each row's edge is colored by its outcome.
- `loopx scheduler status [--json]` shows whether the scheduler is actually ticking (every tick now records a heartbeat), what is queued or running (flagging a run whose process is gone), and each loop's schedule, next due time and last run; it exits 1 when the scheduler is not running correctly. `loopx doctor` gets a matching Scheduler section.
- The topbar's Agents, Office and Chat icons now take the theme colour picked in Settings → Appearance instead of always using the default accent.

## [0.13.0] - 2026-10-29

### Changed
- The topbar's chat button is now a chat-bubble line icon, in the same style as the Office icon, instead of the LoopX logo.

### Fixed
- Auto-refreshing pages no longer reload while the chat side panel is open, so a message being typed there is not wiped.
- Slack: the links in a plugin loop's message (Daily digest, Calendar prep) are clickable again instead of showing as raw `<url|text>`.

## [0.12.0] - 2026-10-29

### Added
- GitLab → Tasks: every tracked issue in My Queue gets a "Run now" button that starts the GitLab loop on just that issue (hidden while its tracking switch is off).
- GitLab loop: each run first deletes the local `loop/issue-<iid>` branch, and its worktree, of every issue that is closed or whose merge request is merged. A worktree with uncommitted changes, or a branch the checkout itself is on, is kept and logged.

### Changed
- The sidebar's logo sits 25px lower, clear of the top edge.
- The app icon, favicon, dashboard logo and every brand image are now fully transparent: the mark and the "LoopX" name are unmixed from the mockup's dark background with no tile, gray rim or halo, re-cut at higher resolution, and small icon sizes are rendered smoothly. The `*-on-dark` brand images are removed.
- The Mac app icon puts the mark on a rounded dark tile again, with a subtle vertical gradient (`#31354f` to `#212336`) sampled from the app's own background, and every dashboard page uses the same icon: the favicon, the sidebar and chat mark, the landing chooser and the Office wall.

### Fixed
- The sidebar, header and AI chat no longer keep showing the previous logo after an update: the dashboard's `/brand/*.png` images carry a version of their content, so the browser and the Mac app load the new images instead of a cached copy.

## [0.11.0] - 2026-10-29

### Added
- Brand assets in `assets/brand/`, cut from the brand mockup (`mockup.png`): the 1024px app icon, the "LoopX" name image for dark and light backgrounds, and the name + tagline ("Dynamic Orchestration Workspace") and icon + name + tagline images, with and without their dark background. `bin/scripts/render_brand_assets.sh` renders the full macOS iconset (16-1024px), the favicon and the dashboard's `/brand/*.png` images from them.
- Mac app: every launch opens on the start screen, whose logo flies in and settles before the dashboard loads, even when the backend is already running; Reduce Motion shows the finished logo.
- Landing page: the LoopX logo now zooms in every time the chooser opens (skipped when the system asks for reduced motion).
- Office: a "Dawn" time-of-day option and a violet-to-rose pre-dawn sky for the early hours before sunrise.
- Office: the coffee shop and bookstore keep a warm light on at night, and cars and pedestrians now speed up, slow down, stop and rest at random instead of moving at one steady pace.
- Settings → General → Slack: a "Message template" picker for loop run reports - **Report** (a status line, headed lists, a table and a "Needs you" callout, in Block Kit) or **Classic** (the same content as plain text) - and a "Send a sample report" button.

### Changed
- New LoopX logo and brand from the brand mockup: a glowing infinity loop crossed into an X on a dark tile. It is the Mac app icon, the favicon, the sidebar and chat mark, and the logo on the landing page, the Mac start screen and the Office wall; the sidebar, landing page and start screen show the "LoopX" name as the brand image (white lettering in dark mode, ink lettering in light mode), and the X in "LoopX" elsewhere in the text carries the brand gradient.
- GitLab issue loop: a scheduled batch now posts one run report per Slack channel when it finishes (MRs opened, answered, waiting on review, a by-project table, and what needs you) instead of a start line and a Finished/Escalated line per issue, a separate digest and a separate "issues incomplete" alert. An approval request is still sent the moment it is needed.
- Topic Monitor: the agent saves its briefing as data (`loopx topic report`) and the loop posts it once the topic finishes, with the run time and item count; a topic that fails now gets a "did not finish" report instead of a separate alert.
- Inbox Triage, plugin loops (Daily Digest, RSS Watch, MR Review and the rest), sign-in health warnings, "loop failed" alerts and the dashboard's single-issue GitLab run now send their Slack messages as run reports in the picked message template; a plugin loop sends one report with its digest and any failed items instead of two messages. Email and other non-Slack connectors still get plain text.
- Dashboard → Activity: a "Latest progress" card now heads the page (what a running loop is doing, its newest steps from the event ledger - started, classified, verifying, MR opened, escalated - and its Stop button, or "All loops idle"), followed by one status card per loop (GitLab, Topic Monitor, Inbox Triage and every enabled plugin loop) and then the run reviews.
- Topic Monitor → Live: the Run now button sits at the top right of the Topics card like the other loop pages, and becomes Stop while a topic is running.
- Topbar: the Agents pill now shows what a running loop is doing (a spinner and "Processing <project> #<issue> — <step>", cut short with the full text on hover) instead of a second status pill beside it, which also stops the centred pill from overlapping the topbar icons.
- GitLab: tracking a project for the issue loop is now a Track switch on its row in Project aliases (local checkout, target branch, and a "Use worktree" switch that is on by default); the separate Tracked Projects card and its install/lint/test command fields are gone. Loop settings moved into the GitLab card.
- GitLab issue loop: the agent works out each project's install, lint and test commands itself (starting from any commands already saved, its earlier runs and other projects on the same stack), fixes verification setup such as RuboCop's cache, and logs what it used; the Live view shows those checks under each issue.
- GitLab issue loop: runs in gate mode by default - the loop re-runs the agent's logged lint and test commands, refuses anything outside a per-stack allowlist, and opens the merge request itself only when they pass.
- GitLab issue loop: the comment it posts on an issue after opening a merge request is now goal-based - the MR link, a one-line **Goal**, and bullets of key results plus the number of checks passed; the commands it ran and how it made the change live only in the MR description.
- GitLab issue loop: an escalation's Slack message links the issue as `project #N` and says in plain words why it needs a human, instead of a raw reason code.
- GitLab issue loop: a project tracked with worktree off is worked on a `loop/issue-<iid>` branch of its own checkout, refused if the checkout has uncommitted changes, and switched back to your branch afterwards.
- Meeting Prep: a brief now goes out on the minute a meeting comes within your "Minutes ahead" setting, instead of at the next 15-minute calendar refresh (which could send it up to 15 minutes early); the scheduler checks the saved agenda every minute, and the 15-minute refresh still picks up new or moved meetings.
- The Office/Dashboard chooser shows the LoopX logo and wordmark, uses the same mode icons as the topbar and office, and has shorter wording.
- Tabs and the Appearance options share one macOS-style segmented control whose track and selected segment follow the chosen theme colour.
- Office: the Switch to Dashboard button moved out of the chat header to the top-left of the office, so it is reachable without opening the chat.
- Office: the Switch to Dashboard button now sits at the top right of the office, to the left of Open chat.
- The product is spelled "LoopX" everywhere it is shown (was "Loop X"), and the "X" in the sidebar and chat-panel wordmark takes a orange accent.

### Fixed
- GitLab issue loop: Node projects now install, lint and test under the Node version they need (a `.nvmrc`/`.node-version`/`.tool-versions` pin, else the `engines` of `package.json` and its lockfile) picked from the versions nvm, fnm, mise, asdf or Volta have installed, instead of whichever `node` is first on PATH. Before, an older default Node made `npm ci` skip native bindings and `npm run lint`/`npm test` fail with "Cannot find native binding".
- GitLab issue loop: merge requests the loop opens now get a description ("Closes #<issue>", Problem / Fix / Result, How, Verified, and Related when the issue thread has any), written by the agent to the create-mr template, or built from its summary and the checks the loop ran when it gives none; MR titles follow the same `fix: #<issue> <what was fixed>` format.
- GitLab issue loop: a rebuilt app that keeps the same version number now refreshes the agent instructions it seeds into `~/.loopx`, so the agent no longer follows an old copy that limited it to saved install/lint/test commands.
- GitLab issue loop: the agent is given the run id literally, so its event calls are no longer refused for containing `$LOOP_RUN_ID`.
- GitLab issue loop: the agent corrects a misspelled or unusable saved command hint from the repo's own CI instead of escalating, and may run `bundle audit` and `brakeman`.
- Chat: when the assistant runs several tools in one step, every one of them now shows as finished once it returns, instead of all but the last spinning forever (live and after reload).

### Removed
- Settings → Block Kit Builder: the "GitLab loop: end-of-run digest failed", "GitLab loop: issues incomplete" and "Topic monitor: topics incomplete" bindings and their example templates, since those alerts are now part of the run reports. A template bound to "Inbox Triage: run digest" still replaces that report.

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

[Unreleased]: https://github.com/encoreshao/loop-x/compare/v0.16.0...HEAD
[0.16.0]: https://github.com/encoreshao/loop-x/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/encoreshao/loop-x/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/encoreshao/loop-x/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/encoreshao/loop-x/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/encoreshao/loop-x/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/encoreshao/loop-x/compare/v0.10.0...v0.11.0
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

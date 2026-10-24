<p align="right">

**English** | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [Français](README.fr.md)

</p>

<p align="center">
  <img src="assets/app-icon.svg" alt="Loop X logo" width="256" height="256">
</p>

# Loop X Engineering

![CI](https://github.com/encoreshao/loop-engineering/actions/workflows/rust.yml/badge.svg)
![License](https://img.shields.io/github/license/encoreshao/loop-engineering)
![Platform](https://img.shields.io/badge/platform-macOS-lightgrey)
![Shell](https://img.shields.io/badge/shell-bash-4EAA25)

Loop X Engineering's mission is to give you back the time issue triage
eats: a standing, unattended teammate that works your GitLab queue every
weekday so nothing assigned to you sits untouched — shipping fixes,
answering questions, or flagging what genuinely needs your judgment — and
your attention goes only where it actually matters. A local web dashboard
lets you watch it work, review everything it's done, and configure it all
by hand — no editing JSON.

It's built to be safe to leave running unattended: it never merges its own
merge requests, never assigns itself new issues, and only ever touches the
projects you've explicitly told it about.

## Table of contents

- [How it works](#how-it-works)
- [Loops](#loops)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Directory layout](#directory-layout)
- [Configuration](#configuration)
- [Running it](#running-it)
- [The dashboard](#the-dashboard)
- [Connectors](#connectors)
- [Scripts reference](#scripts-reference)
- [Safety boundaries](#safety-boundaries)
- [Testing](#testing)
- [Project docs](#project-docs)
- [License](#license)



## How it works

Each scheduled run (`loopx run gitlab-loop`):

1. Lists every open GitLab issue assigned to your configured username, across every project alias in your config.
2. Processes them **one at a time, never in parallel**, following the step-by-step decision procedure in [`instructions/loopx/gitlab-issue.md`](https://github.com/encoreshao/loop-engineering/blob/main/instructions/loopx/gitlab-issue.md).
3. For each issue, does exactly one of:
  - **Fix it** — in an isolated git worktree, on a `loop/issue-<iid>` branch, only opening a merge request once the project's own lint/test commands pass.
  - **Answer it** — post a GitLab comment when the ask needs no code change (a question, a status check).
  - **Escalate it** — post a GitLab comment asking for clarification when the ask is ambiguous, or when verification fails.
4. Sends a Slack message per issue plus one end-of-run digest (every run, even mornings with nothing assigned).
5. Updates [`PROGRESS.md`](https://github.com/encoreshao/loop-engineering/blob/main/PROGRESS.md) and `outputs/daily-review.md` so the next run — and you — know what happened.

Reusable, cross-run lessons (fix patterns, gotchas) get recorded per issue as markdown task-memory files via `loopx memory-store` (entries recorded before this format existed are still read via `loopx memory`), so later runs start smarter than the last.

A second, independent loop (`loopx run topic-loop`) watches arbitrary topics on the wider web instead of GitLab — see [`docs/tasks/topic-monitor-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/topic-monitor-loop.md).

A third loop (`loopx run inbox-triage-loop`) triages Gmail and Outlook inboxes: it categorises each new unread message into a `Loop/*` label, drafts (never sends) a threaded reply to anything urgent, and reports via a Slack digest and the dashboard's **Loops → Inbox Triage** page — see [`docs/tasks/inbox-triage-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/inbox-triage-loop.md).

## Loops

Seven loops ship in `config/loops.json.template`; each has its own page under **Loops** in the dashboard and its own spec under `docs/tasks/`. A loop's schedule is editable on that page; the defaults below come from the template.

| Loop | What it does | Needs | Default schedule | Writes outside Loop X |
| --- | --- | --- | --- | --- |
| GitLab issues | Works your assigned issues: fixes, answers or escalates each one | GitLab config (`~/.gitlab/config.json`) | Weekdays 10:00 | Branches and merge requests (never merged), issue comments, Slack |
| Topic monitor | Researches your topics on the web and sends a daily briefing | Topics (`topics.json`) | Daily 10:00 | Slack digest |
| Inbox triage | Labels new mail and drafts replies to urgent messages (disabled by default) | A mailbox (`mail` capability) | Weekdays 09:00 | Mail labels and drafts (never sends) |
| Daily Digest | One morning brief: todos, assigned issues, MRs awaiting your review, today's meetings and what Loop X did yesterday (disabled by default) | `issues` connector (calendar optional) | Weekdays 09:30 | Notifications only |
| MR Review | Pre-reviews merge requests where you are a reviewer (disabled by default) | `merge_requests` connector (GitLab) | Every 2 hours | GitLab draft notes only; never publishes, approves or posts a normal note |
| Pipeline Doctor | Diagnoses failed CI pipelines on tracked projects and your open MRs, and flags recurring failures (disabled by default) | `pipelines` connector (GitLab) | Hourly | Notifications only |
| RSS Watch | Ranks new feed entries against your interests and sends a short digest (disabled by default) | `feed` connector (RSS) | Daily 08:00 | Notifications only |
| LinkedIn Autopilot | Browses your own signed-in LinkedIn session at a human pace and summarizes what is worth your attention: posts, people, messages (read-only; disabled by default). Needs Google Chrome. LinkedIn offers no API for this and its terms prohibit automation, so the account can be restricted | `social` connector (LinkedIn) | Daily 09:00 | Content tab + notifications only |
| Meeting Prep | A prep brief before each meeting: agenda, linked GitLab work, recent mail with the attendees, follow-ups from last time (disabled by default) | `calendar` connector (Google Calendar); optional GitLab and mailbox | Every 15 minutes | Notifications only |
| Release Notes | When a tracked project gets a new tag, writes release notes from the MRs merged since the previous tag (disabled by default) | `merge_requests` connector (GitLab) | Hourly | Local markdown file + notification |
| Stale Work Sweeper | Every Monday, lists your GitLab issues and MRs that have gone quiet and reviews waiting on you; no model call (disabled by default) | `issues` connector (GitLab) | Mondays 09:00 | Notifications only |

The last four are LoopKit plugins (`crates/core/src/loopkit.rs`, `crates/core/src/loop_plugins/`): they run the model sealed (no tools, no MCP servers), isolate each item so one failure never stops the run, and send through the loop's **Notify via** connectors. See [`docs/architecture.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/architecture.md#loopkit).

## Requirements

- macOS (the schedule and the dashboard both run as `launchd` agents)
- `git` 2.42+ (worktrees, push-options)
- A GitLab account + personal access token for the projects you want tracked
- (optional) A Slack incoming webhook, for run notifications
- The `[gitlab-config](https://github.com/encoreshao/encore-skills/tree/main/skills/gitlab-config)` skill from `[encore-skills](https://github.com/encoreshao/encore-skills)` — this loop's one external dependency, deployed to `~/.encore-skills` by `setup.sh`. Check it's actually present any time from the dashboard's **Settings → Skills** page.
- (build from source only) the Rust toolchain pinned in `rust-toolchain.toml`



## Quick start

**New: install the `loopx` binary (no clone).** Downloads the prebuilt `loopx` for your Mac from the latest GitHub Release, verifies its SHA-256, installs it to `~/.loopx/bin/loopx` and scaffolds `projects.json`/`topics.json`/`loops.json` from the templates:

```bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash -s -- --upgrade   # later
```

Add `--upgrade` to update later: it keeps your config, replaces only the program, and restarts the launchd agents that are already loaded (dashboard restarted, scheduler re-registered, never kicked). Pass `--load-agents` to also start the dashboard agent on a fresh install. Prefer Homebrew? `brew install encoreshao/tap/loopx` (then `brew services start loopx`), or install the signed `LoopX.app` from the release's DMG. First-launch check of the AI CLIs, credentials and config: `loopx doctor` (`--json` for the app). Uninstall either mode: `bin/scripts/uninstall-loopx.sh --project-dir ~/.loopx` (`--keep-config`, `--remove-app`). Release procedure: [`docs/RELEASING.md`](docs/RELEASING.md).

**Working in Claude Code already?** Paste this instead of running the commands yourself:

> Set up [https://github.com/encoreshao/loop-engineering](https://github.com/encoreshao/loop-engineering) for me: run its online installer
> (`curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash`),
> then help me fill in `~/.loopx/projects.json` with my own GitLab project(s), and `~/.gitlab/config.json` with my GitLab token.

The `gitlab-config` skill (see Requirements) is installed from the dashboard's **Settings → Skills** page, which runs `bin/scripts/setup.sh`; you can run that script by hand instead.

### Install the Mac app (DMG)

Download `LoopX-<version>.dmg` from the latest GitHub Release, open it and drag **LoopX** onto **Applications**, then launch it. There is no installer step; everything is set up on first launch:

- the app registers its bundled `loopx` as a background agent (`com.hermes.loopx.backend`); macOS may ask you to allow it under **System Settings → General → Login Items & Extensions**;
- the backend scaffolds `~/.loopx/loops.json` and seeds `loop_scheduler_state.json` with today's date for every loop, so turning on **Run scheduled loops** never fires an immediate run;
- the first-launch guide checks for the `claude` / `codex` CLIs and the `gitlab-config` skill. It cannot fill in your GitLab projects and token: add them under **Loops → GitLab Issues → Projects** and in `~/.gitlab/config.json`.

Until a release is Developer ID signed and notarized, macOS Gatekeeper warns about the downloaded app (right-click → Open). To use the CLI from a shell, link the bundled binary: `sudo ln -sf /Applications/LoopX.app/Contents/Helpers/loopx /usr/local/bin/loopx`. The app only updates itself in Sparkle-enabled builds; otherwise download the new DMG and replace the app.

### Build from source

```bash
cargo build --release -p loopx                        # -> target/release/loopx (toolchain pinned in rust-toolchain.toml)
bin/scripts/build_loopx_app.sh target/release/loopx   # -> dist/LoopX.app, the Swift app around that binary (--install copies it to /Applications)
```

`LoopX.app` (sources in [`macos/LoopX`](https://github.com/encoreshao/loop-engineering/tree/main/macos/LoopX)) embeds the `loopx` binary and registers it as a background agent (`com.hermes.loopx.backend`) so the backend outlives the app; if a `loopx serve` is already running (see `loopx.lock`), the app attaches to it instead. Build details: [`docs/building.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/building.md).

**Updates.** The app updates itself through Sparkle (only in builds that were given a Sparkle feed URL and public key); the binary updates through `brew upgrade loopx` or `install-loopx.sh --upgrade`. The dashboard still shows an *Update available* notice when this checkout is behind `origin`, but it no longer upgrades itself — pull and rebuild by hand.

### Uninstalling

```bash
bin/scripts/uninstall-loopx.sh --keep-config   # omit --keep-config to remove everything
```

Removes the install's `launchd` agents, stops the running backend (`loopx.lock`), and deletes the `loop-engineering.*` Keychain items and the install directory (`~/.loopx` by default; pass `--project-dir` for another). `--keep-config` keeps `projects.json`, `topics.json`, `loops.json` and `ai_cli.json`; `--remove-app` also deletes `LoopX.app`; `--keep-keychain` leaves the Keychain alone. Safe to re-run; finish a Homebrew install with `brew uninstall loopx`.


## Directory layout

Using the default install path, everything lands under one folder:

```
~/.loopx/            # the install directory (loopx binary, config, run state)
├── bin/loopx                   # the loopx binary (install-loopx.sh)
├── projects.json                # your config: GitLab projects to track  ┐
├── topics.json                  # your config: topics to monitor         │
├── loops.json                   # your config: scheduled-loop registry   ├─ yours
├── instructions.md              # your free-text instructions            │
├── connectors.json              # your config: connector accounts        │
├── ai_cli.json                  # your config: Claude Code vs Codex CLI   ┘
├── loop_scheduler_state.json    # managed automatically, not hand-edited
├── PROGRESS.md                  # live run state, updated every run
├── outputs/                     # ← generated docs & run history live here (gitignored)
│   ├── daily-review.md          #   latest GitLab-issue-loop report
│   ├── connectors/test-results.json  #   last Test result per connector account
│   ├── messages.json             #   Dashboard → Activity message thread
│   ├── status.json               #   GitLab loop's current/last run status
│   ├── status/<loop_name>.json   #   every other registered loop's current/last run status
│   └── history/<date>.{md,log}   #   every past run's report + log
└── worktrees/                    # ← per-issue git worktrees for tracked projects (gitignored)
    └── <project>-issue-<iid>/    #   that project's own checkout, on branch loop/issue-<iid>
```

`projects.json`, `topics.json`, `loops.json`, `instructions.md`, and `ai_cli.json` always resolve to `~/.loopx/…` regardless of where this repo is checked out. `projects.json`'s scaffolded `worktree_root` defaults to `~/.loopx/worktrees` too.

Two more config files live outside this tree entirely, editable from the dashboard's **Loops → GitLab Issues → Projects** and **Settings → Notifications** pages instead of by hand: `~/.gitlab/config.json` and `~/.slack/config.json`.

## Configuration


| File                                  | Holds                                                                                                                                                                                                                   | Managed via                                                                                                                                                                   |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `~/.loopx/projects.json`   | Which projects to track, their local checkout paths, target branch, install/lint/test commands, your GitLab username, and the worktree scratch directory (`worktree_root`, defaults to `~/.loopx/worktrees`) | Dashboard **Loops → GitLab Issues → Projects** page's "Tracked Projects" section, or copy [`config/projects.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/projects.json.template) by hand, or let `install-loopx.sh` do it |
| ↳ per-project `instance` (optional)   | Overrides the top-level `gitlab_instance` for one project — set this when your projects span more than one GitLab instance. Falls back to `gitlab_instance` when omitted.                                               | Same file, per project entry — see the template's `harbor` example                                                                                                            |
| `~/.loopx/topics.json`     | Which topics to monitor and what counts as notable for each one (topic monitor loop only)                                                                                                                               | Copy [`config/topics.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/topics.json.template) by hand, or let `install-loopx.sh` do it                                                                |
| `~/.loopx/inboxes.json`    | Which mailboxes to triage (provider, account, categories, VIP/excluded senders, Slack bundle) and the shared default category set (Inbox Triage loop only)                                                              | Dashboard's **Loops → Inbox Triage → Setup** (`/loops/inbox-triage-loop?view=setup`), or let `install-loopx.sh` scaffold it from [`config/inboxes.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/inboxes.json.template)               |
| `~/.loopx/mail_oauth.json` | The Gmail/Outlook OAuth app's own client ID (and, for Google, client secret) — a one-time app-registration step, not a per-mailbox credential                                                                           | Dashboard's **Loops → Inbox Triage → Setup** page                                                                                                                                                     |
| `~/.loopx/loops.json`      | The registry of scheduled loops: each entry's name, schedule (weekdays/hour/minute), entry point module, timeout, and per-loop knobs — read by `loopx loops`, polled by `loopx scheduler`                 | Copy [`config/loops.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/loops.json.template) by hand, or let `install-loopx.sh` do it                                                                  |
| `~/.loopx/loop_scheduler_state.json` | Per-loop last-attempted date, so the scheduler never runs the same loop twice in a day — not something you hand-edit                                                                                            | Written automatically by `loopx scheduler`; seeded with today's date for every registered loop by `install-loopx.sh` and on the first launch of the Mac app / `loopx serve`, so enabling the scheduler doesn't fire an immediate run |
| `~/.loopx/instructions.md` (+ `instructions/<loop>.md`) | Your own free-text instructions, global and per loop, read at the start of every run                                                                                                                                             | Dashboard **Settings** page's Instructions tab                                                                                                                               |
| `~/.loopx/ai_cli.json`     | Which AI CLI (Claude Code or Codex CLI) `loopx run` invokes for every registered loop; defaults to `claude`                                                                                                        | Dashboard **Settings** page's AI CLI tab, or let `install-loopx.sh` do it                                                                                                                |
| `~/.gitlab/config.json`               | GitLab instance URLs, tokens, and project-alias → project-ID mappings (read by the `gitlab-config` skill)                                                                                                               | Dashboard **Loops → GitLab Issues → Projects** page                                                                                                                                                     |
| `~/.slack/config.json`                | Your Slack incoming webhook URL (and any per-bundle overrides)                                                                                                                                                          | Dashboard **Settings** page's Notifications tab (the default webhook) / **Loops → GitLab Issues → Projects** page's Access bundles section (per-bundle overrides)                                                              |


`loopx config` is the only code that reads `projects.json` — use it to sanity-check your config from a terminal:

```bash
loopx config aliases                # every configured project alias
loopx config project <alias>        # that alias's full config, incl. resolved GitLab instance
loopx config assignee               # the GitLab username being tracked
loopx config worktree-root          # where per-issue worktrees get created
```

If `~/.loopx/projects.json` doesn't exist yet, every command that needs it fails fast with a message saying so — nothing silently guesses paths. Save a project from the dashboard's **Loops → GitLab Issues → Projects** page, or copy `config/projects.json.template`.

Note: the directory used to be `~/.loop-engineering`. The first `loopx` run (or `install-loopx.sh`) moves an existing one to `~/.loopx` and leaves a symlink at the old name, so paths already written into your configs and launchd plists keep working. Set `LOOP_ENGINEERING_HOME` to make loopx use a different directory.

**Access bundles** — per-project token/webhook overrides

Most projects just use their GitLab instance's default token. An **access bundle** is a named override — its own `{instance, token}` pair, plus an optional Slack webhook — for the rare project whose default instance token doesn't have the access that project needs.

Manage bundles from the dashboard's **Loops → GitLab Issues → Projects** page, in their own "Access bundles" section:

- **Add a bundle**: name it, pick which GitLab instance it authenticates against, paste its token, and optionally a Slack webhook URL.
- **Assign a bundle to a project**: edit the project alias's row and pick the bundle from the **Bundle** dropdown — defaults to "(use instance default)".
- A bundle can't be deleted, and its instance can't be changed, while any project alias still points at it.
- Deleting a bundle also clears its Slack webhook override, if it had one.

Bundles live in `~/.gitlab/config.json`'s `bundles` key and, if a webhook override is set, `~/.slack/config.json`'s `bundle_webhooks` key — joined only by the bundle's name.

## Running it

**Manually**, once, to see it work before trusting it with a schedule:

```bash
loopx run gitlab-loop   # the daily GitLab issue loop
loopx run topic-loop    # the topic monitor loop
```

Both log to `outputs/history/`, and both also append every `claude` CLI invocation's output to `logs/loop-engineering.log` (viewable on the dashboard's **Runs → Logs** page); you can also trigger the GitLab loop from the dashboard's **Run now** button (Dashboard → Overview) without a terminal.

**On a schedule** — three ways:

- the LoopX app's built-in scheduler: while the app is open and **Settings → Run scheduled loops while LoopX is open** is on, it runs `loopx scheduler` every 15 minutes;
- launchd agents: `install-loopx.sh` renders the templates under [`launchd/loopx/`](https://github.com/encoreshao/loop-engineering/tree/main/launchd/loopx) into `<install dir>/launchd/` once, and you load them with a click each from the dashboard's **Settings → Daemons** page (which also shows whether each is currently loaded and its PID), or by hand with `launchctl load -w`;
- or run `loopx scheduler` from any scheduler of your own.

| Agent                                   | Runs                                                                                                                                             |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------|
| `com.hermes.loop-engineering`           | `loopx scheduler`, every 15 minutes (`StartInterval`) — runs whichever loop(s) registered in `~/.loopx/loops.json` are due |
| `com.hermes.loop-engineering-dashboard` | The web dashboard (`loopx serve`), always-on (`RunAtLoad` + `KeepAlive`)                                                                          |
| `com.hermes.loopx.backend`              | The backend bundled in `LoopX.app`, registered by the app itself (not a template in this repo)                                                    |

Which loops run and on what schedule is config, not code — edit `~/.loopx/loops.json` (see [`config/loops.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/loops.json.template)) to add a loop or change when it's due; adding a loop needs a new `loops.json` entry, not a new plist. The **Settings → Daemons** page's per-agent schedule editor only applies to a plist's own `StartCalendarInterval`, which `com.hermes.loop-engineering` does not have (it polls every 15 minutes on a fixed `StartInterval` and defers to `loops.json` for which loop is actually due) — editing a loop's own schedule is done in the **Loops** pages or by hand in `loops.json`.


## The dashboard

A localhost-only web UI (server-rendered pages, no JS framework) served by `loopx serve` — one Rust binary, also the API the Mac app talks to. `loopx serve --port N` picks the port (default `8420`; `--port 0` picks a free one). `install-loopx.sh` picks a random port in `48420`-`48620` the first time it renders the always-on `launchd` agent (overridable with `--port`, and never re-picked on a later `--upgrade`) — check `launchd/com.hermes.loop-engineering-dashboard.plist` in the install directory for the port an existing install is actually running on.


| Sidebar entry | Shows |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Dashboard** (`/`) | Views: Overview — Current/last run status, a live progress indicator, and the Run now button; Activity — A message thread with the loop, plus its own live progress indicator. Paste a GitLab issue link here to have the loop work on that one issue immediately, regardless of who it's assigned to. |
| **Loops** (`/loops`) | A catalog splitting active loops from available ones; each loop that's visible also appears as a child link under **Loops** in the sidebar (Inbox Triage stays hidden there while it's disabled and has never run) |
| — **GitLab Issues** (`/loops/gitlab-loop`) | Views: Live — Your currently assigned issues and open MRs, fetched live; Projects — Manage `~/.gitlab/config.json` (instances, project aliases, access bundles) and `~/.loopx/projects.json` (tracked projects, loop settings) without hand-editing JSON |
| — **Topic Monitor** (`/loops/topic-loop`) | Views: Live — Status and saved briefings for every configured topic; Topics — Add, edit, and delete monitored topics — a separate view on the same page, so configuration doesn't clutter the Live status view |
| — **Inbox Triage** (`/loops/inbox-triage-loop`) | Views: Inbox Triage's own live status (**Live**) plus mailbox connection and categories (**Setup**: connect Gmail/Outlook, categories, VIP/excluded senders, Slack bundle) |
| **Runs** (`/runs`) | Views: Loop Runs — Every run recorded under `outputs/loop-runs/` (one per issue or topic processed), most recent first — read-only; an overview strip shows total runs, success/escalation rate, average cost, and the experimental Loop Efficiency Score; History — Every past run's review report, newest first; Logs — The tail of `logs/loop-engineering.log` - every `claude` CLI invocation's output, across the GitLab loop, the topic monitor loop, and this dashboard's own chat assistant |
| **Insights** (`/insights`) | Views: Analytics — The loop's performance over a selectable day window: a Loop Health score, outcomes, quality, risk & classification, failure breakdown, and learning trends; Cost — AI usage cost — the GitLab issue loop's own windowed cost, and total cost across every run under `outputs/loop-runs/`; Budget — Each recorded run's last-known budget status, plus rollups by loop definition and by day/week/month; Memory — Cross-run lessons recorded per project, one markdown file per GitLab issue, plus anything recorded before this format existed (shown under "Legacy learnings") |
| **Harness** (`/harness`) | Views: Audit — A score and pass/fail checks for each loop definition |
| **Connectors** (`/connectors`) | Views: Accounts — Every connector account grouped by type, with capability chips, a Test button (**Send test message** for notification targets) showing its last result, and a "Managed on …" badge linking to the owner page for external accounts; Add — Pick a type and fill its form. Secrets go in a password field and are never shown again (leave blank to keep the stored one when editing) |
| **Settings** (`/settings`) | Views: General — Notifications (manage `~/.slack/config.json`'s default webhook), AI CLI (choose Claude Code or Codex CLI, with a live installed/not-found check for each), Appearance (color mode, accent theme, auto-refresh interval — saved to this browser's `localStorage`), and Instructions (your own free-text instructions, read by the loop at the start of every run) — clustered as tabs on one page (the **General** view is split into Notifications / AI CLI / Appearance / Instructions tabs (`?tab=`)); Daemons — Load state, an editable schedule, and enable/disable for every `launchd` agent, plus a Registered Loops breakdown of every loop the unified scheduler runs (its own schedule and last-run status, read from `loops.json`); Skills — Every external skill this loop depends on, and whether it's actually installed |
| **README** (`/readme`) | Moved to the topbar's help icon (`/readme`): this file, rendered in-app with a jump-to-section quicknav |

Every older URL (`/activity`, `/gitlab`, `/topic-monitor`, `/inbox`, `/loop-runs`, `/history`, `/logs`, `/analytics`, `/cost`, `/budget`, `/memory`, `/audit`, `/settings/general`, `/daemons`, `/skills`, and so on) permanently redirects (301) to its new home, query string kept, so bookmarks keep working.


## Connectors

A connector is an account the loops can talk to: a GitLab or GitHub instance, a Slack, Telegram or chat webhook, a Notion workspace, an RSS feed list, a Jira or Linear workspace, a mailbox, a Google Calendar. Manage them on the dashboard's **System → Connectors** page (`/connectors`). Each type declares *capabilities* (`issues`, `merge_requests`, `pipelines`, `notify`, `feed`, `mail`, `docs`, `calendar`), and a loop can require a capability instead of a specific product.

| Type | Capabilities | What you enter | Secret |
| --- | --- | --- | --- |
| GitLab | `issues`, `merge_requests`, `pipelines` | URL | personal access token |
| GitHub | `issues`, `merge_requests`, `pipelines` | API URL (default `https://api.github.com`), username | token |
| Slack webhook | `notify` | — | webhook URL |
| Chat webhook | `notify` | pick a preset: Feishu, DingTalk, WeCom 企业微信 (group bot; personal WeChat has no bot API), Microsoft Teams, Discord, Google Chat, or Generic webhook | webhook URL |
| Telegram bot | `notify` | chat ID | bot token |
| RSS / Atom feeds | `feed` | feed URLs, one per line | — |
| Notion | `docs` (shown as Documents) | — | integration token |
| Jira Cloud | `issues` | site URL, email | API token |
| Linear | `issues` | — | API key |
| Mailbox | `mail` | external — managed on Inbox Triage setup | — |
| Google Calendar | `calendar` (shown as Calendar) | calendar ID (default `primary`) | Google sign-in (`calendar.readonly` plus `calendar.events`, so the chat can create events after you approve); the refresh token goes in the Keychain |

**Gallery and form.**

- **Add** opens a gallery of connector types, grouped into Google (first: Gmail, Google Calendar and Google Chat), Code hosting, Chat & notifications, Work tracking, Knowledge, Feeds and Mail (Outlook lives here), with a search box to filter them. Cards follow the accent selected in **Settings → Appearance** and each service's brand color, and each chat webhook preset has its own description.
- Each tile and each account row carries the service's brand logo (inline Simple Icons marks; services without one — Feishu, DingTalk, the generic webhook — get a lettermark).
- The chat webhook tile expands into the presets above, each with a one-line hint (for example, Teams Workflows webhooks may require Adaptive Cards) and a **Where do I get this?** link to that service's own docs.
- The add/edit form has an **Account** section (**Label** and **Connector id**; the id is suggested from the label until you edit it), a **Connection** section (the type's settings) and a **Credentials** section (the secret).
- **Google Calendar** has no pasted secret: click **Connect with Google** (**Reconnect** once connected) to sign in. It reuses the Google OAuth client you already set up for Gmail on Inbox Triage's setup page (Gmail tab; if it is missing, the form links you there), so Google Cloud needs the same redirect URI, `http://127.0.0.1:<port>/oauth/google/callback`. The account row shows a **Connected** / **Not connected** marker, and **Test** reads the calendar. It requests `calendar.readonly` and `calendar.events`; a calendar connected before the second scope existed keeps working read-only until you click **Reconnect**.
- Required fields are marked `*`, the others say "(optional)", and fields have example placeholders. The secret field has a **Show**/**Hide** toggle.
- Buttons: **Save**, **Save and test** (saves, then runs the probe) and **Cancel**. If a save fails, the form is shown again with your non-secret values kept.

**Where things live.** Accounts you add on the page are *native*: their non-secret settings go in `~/.loopx/connectors.json`, and their secrets go in the macOS Keychain under the service `loop-engineering.connectors` (suffixed `.sandbox-<hash>` whenever `LOOP_ENGINEERING_HOME` is set, so a sandboxed run never touches the real ones). Secrets are never written to `connectors.json` and never shown again after saving.

**External accounts** are read through from the files that already own them, with nothing migrated: GitLab instances from `~/.gitlab/config.json` (id = the instance alias), Slack webhooks from `~/.slack/config.json` (`slack-default`, plus `slack-<bundle>` per bundle webhook), and mailboxes from `inboxes.json` (id = the inbox name). They show a "Managed on …" badge linking to the page where you edit them; you can still Test them here.

**Test buttons.** Every account has a **Test** button (**Send test message** for Slack, Telegram and chat webhooks); the last result is kept in `outputs/connectors/test-results.json`.

**Loops and notifications.** On **Loops**, a loop that needs a capability shows "Needs: …" chips, and it cannot be enabled (in the UI or by the server) until a connector with that capability exists. A loop whose `loops.json` entry declares `"routes_notifications": true` (its runner sends through `loopx notify-loop`) also gets a **Notify via** selection, stored as `notify: [connector ids]`. `loopx notify-loop` routes such a loop's notification to those connectors; with no `notify` set it posts to the default Slack webhook exactly as before. The built-in GitLab, Topic and Inbox loops don't declare it yet and still post to the Slack webhook directly; if one of them already has a `notify` list, Loops shows it read-only with a **Clear** button. You can try it from the CLI with `loopx notify-loop <loop> "<text>"`. The dashboard's AI panel can also list your connectors (chat tool `connector-list`). The chat can also call connector tools (GitLab and GitHub issues, pull requests and comments, RSS feeds, saved markdown documents), shows each call as a chip on the reply, and asks for an Approve click before any write.

## Scripts reference

Expand for the full list


| Command / script | Purpose                                                                                                                                                                                                                              |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `loopx run <loop>` | Runs one registered loop now (looked up from `~/.loopx/loops.json`) — logs to `outputs/history/`, records events, notifies Slack on failure; `loopx run gitlab-loop <alias> <issue_iid>` works a single issue. Invoked by `loopx scheduler` (on schedule) or the dashboard (on demand) |
| `loopx scheduler` | The poll loop launchd (or the app) runs every 15 minutes: reads `~/.loopx/loops.json` and starts whichever registered loop(s) are due |
| `loopx loops`, `loopx ai-cli` | The loops registry (`loops.json`: name, schedule, entry point) and which AI CLI (`claude` or `codex`) every loop invokes (`ai_cli.json`) |
| `loopx config` | Reads `~/.loopx/projects.json` (aliases, assignee, worktree root, per-project config) |
| `loopx issues list`, `loopx issues comments` | Lists open GitLab issues assigned to the configured user across configured projects; detects which notes on a cached issue are new since the loop last looked |
| `loopx memory`, `loopx memory-store` | Reads (legacy) per-project lessons stored in the GitLab cache; reads/records per-issue task memory as markdown files (one per issue, plus a per-project MEMORY.md index) |
| `loopx topic` | Topic registry (`topics.json`), the rolling 7-day dedup window per topic, and the topic monitor run (`loopx topic run`) |
| `loopx inbox run`, `loopx loopkit` | The Inbox Triage runner; runs a LoopKit plugin |
| `loopx notify`, `loopx notify-loop` | Posts a message to the configured Slack incoming webhook; routes a loop's notification to its connectors |
| `loopx worktree new`, `loopx mr open` | Creates (or reuses) an isolated git worktree on a `loop/issue-<iid>` branch; pushes an issue branch and opens its MR — refuses anything not named `loop/issue-*` |
| `loopx gitlab`, `loopx secrets`, `loopx auth` | GitLab REST helpers and cache; connector secrets in the Keychain; Google/LinkedIn OAuth sign-ins |
| `loopx serve`, `loopx chat-tool` | The dashboard backend and the Mac app's API; the one command the dashboard chat assistant may run |
| `loopx loop`, `loopx events`, `loopx ledger`, `loopx metrics`, `loopx cost`, `loopx risk`, `loopx learning` | Loop definitions (init, validate, audit, eval, ...), the event ledger and the reports built from it |
| `loopx doctor`, `loopx update`, `loopx status`, `loopx messages`, `loopx approvals` | First-launch checks; the update check; run status files; the dashboard chat inbox; pending approvals |
| `bin/scripts/install-loopx.sh` | Clone-free installer for the `loopx` binary: downloads the release build for this Mac, verifies it against `SHA256SUMS`, scaffolds the config, renders `launchd/loopx/*.plist.template`; `--upgrade` keeps config and replaces only the program; `--load-agents`, `--skip-service`, `--release-base` |
| `bin/scripts/uninstall-loopx.sh` | Removes a loopx install of either mode (LaunchAgents, `loopx.lock`, Keychain entries, `--remove-app`, `--keep-config`) |
| `bin/scripts/build_loopx_app.sh` | Builds the universal `LoopX.app` (Swift, `macos/LoopX`) around a given `loopx` binary |
| `bin/scripts/setup.sh` | Installs the `gitlab-config` skill (the Settings → Skills install button runs it) |
| `bin/scripts/release_notes.sh` | Prints a version's section of `CHANGELOG.md` for the release notes |




## Safety boundaries

Fixed, and does not loosen with time or repeated success (see [`docs/tasks/gitlab-issue-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/gitlab-issue-loop.md)):

- **Never merges a merge request.** The loop's job ends at "MR opened, verification passing" — merging is always a manual human step.
- Every code change happens in its own git worktree, on a `loop/issue-<iid>` branch, never on the target branch directly.
- An MR only opens if the project's own configured `test_cmd`/`lint_cmd` pass, and the diff only touches files relevant to the issue.
- No arbitrary shell, no dependency upgrades, no reading `.env`/credentials/SSH keys — only the command allow-list in `instructions/loopx/gitlab-issue.md`.
- Issues are processed one at a time, sequentially, never in parallel.
- A verification failure on the same issue is never retried within a run — it escalates via a GitLab comment instead. (With `verification.mode: gate` the loop itself re-runs the project's checks and allows one bounded retry with the failing output as feedback; it never opens an MR whose tests/lint fail, and escalates with the `loop:needs-human` label instead.)

The Inbox Triage loop has its own fixed safety boundary (see [`docs/tasks/inbox-triage-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/inbox-triage-loop.md)):

- **Never sends mail.** Neither mail provider module contains a send function, and the Outlook token it obtains is scoped without `Mail.Send` — sending is impossible at the token level, not just the code level.
- **Never archives, deletes, moves, or changes read state.** The only mailbox writes are creating `Loop/*` labels/categories, applying them, and creating reply drafts left in the mailbox's own Drafts folder.
- **Only `Loop/*` labels are ever applied.** Every category label, default or custom, must start with `Loop/` — `inboxes.json` is rejected on load otherwise, so a hand-edited system label like `TRASH` or `UNREAD` can never be applied to real mail.
- **Message bodies never persist.** They exist only in memory and in the prompt of the `claude -p` call per inbox (plus at most one retry), which keeps no session transcript — never written to `outputs/`, logs, status, or the Slack digest, which only ever get sender, subject, category, the AI's short reason, and a draft link. The AI-written reply draft is saved only in the mailbox's own Drafts folder.
- **Inbox Triage requires the Claude CLI.** Codex always gives the model a shell and records the prompt under `~/.codex/sessions/`, so with Codex selected every inbox fails up front — before any mail is read — until the AI CLI is switched back to Claude in **Settings**.
- **Refresh tokens live only in the macOS Keychain**, written via `security -i` with the token on stdin, never on disk in plain text or in a process's argv.



## Testing

```bash
cargo test --workspace
cargo build -p loopx && LOOPX_BIN=target/debug/loopx python3 -m pytest tests/
```

Rust unit tests live next to the code. The `pytest` suite only drives the built `loopx` binary as a black box (CLI contracts, end-to-end runs, installers, packaging), against real subprocesses/tmp dirs rather than mocks wherever practical; `tests/golden_ui/` holds the rendered HTML of every dashboard page in four languages, which `cargo test` compares byte for byte.

`loop eval` runs the scripted evaluation cases (free) and writes `outputs/evals/last.json`. `loop eval --golden [--budget-usd N] [--case NAME]` runs the real agent on synthetic fixture repos (paid: the budget defaults to 10 USD and no new case starts once it is spent) and writes `outputs/evals/golden-last.json`. Both results show under Harness → Evals. `loop ledger backfill` rebuilds ledger records from older `result.json` files.

## Project docs


| Doc                                                                    | What it's for                                                                                          |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| [`docs/architecture.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/architecture.md)                         | The V2 runtime architecture: `LoopDefinition`/`LoopState`/`LoopRuntime`, verification/budget/policy, observability, and the CLI — the map, not either loop's own spec |
| [`docs/building.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/building.md)                                 | Build and run the Rust `loopx` backend, Web mode, and the Mac app, including the sandboxed dev setup |
| [`TASK.md`](https://github.com/encoreshao/loop-engineering/blob/main/TASK.md)                                                   | Index of every scheduled task this repo runs, each pointing at its own spec under `docs/tasks/`        |
| [`docs/tasks/gitlab-issue-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/gitlab-issue-loop.md)   | The GitLab issue loop's human-facing spec: goal, scope, safety boundaries                              |
| [`docs/tasks/topic-monitor-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/topic-monitor-loop.md) | The topic monitor loop's human-facing spec: goal, scope, safety boundaries                             |
| [`instructions/loopx/gitlab-issue.md`](https://github.com/encoreshao/loop-engineering/blob/main/instructions/loopx/gitlab-issue.md)                         | The step-by-step procedure the GitLab issue loop itself follows each run                               |
| [`instructions/loopx/topic-monitor.md`](https://github.com/encoreshao/loop-engineering/blob/main/instructions/loopx/topic-monitor.md)       | The step-by-step procedure the topic monitor loop itself follows each run                              |
| [`PROGRESS.md`](https://github.com/encoreshao/loop-engineering/blob/main/PROGRESS.md)                                           | Live state the loop reads and updates every run — last run's summary, open escalations, decisions made |
| [`docs/troubleshooting/crash-looping-launchd-agent.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/troubleshooting/crash-looping-launchd-agent.md) | Diagnose and fix a `com.hermes.loop-engineering*` launchd agent stuck crash-looping and flooding its log |




## License

[MIT](https://github.com/encoreshao/loop-engineering/blob/main/LICENSE) — see the [`LICENSE`](https://github.com/encoreshao/loop-engineering/blob/main/LICENSE) file.
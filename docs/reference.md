# LoopX reference

The detailed reference for LoopX: where files live, configuration, scheduling, the dashboard, connectors, CLI commands and safety rules. For the short overview, see the [README](../README.md).

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
├── ai_cli.json                  # your config: which AI CLI   ┘
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
| `~/.loopx/projects.json`   | Which projects to track, their local checkout paths, target branch, whether each uses worktrees, optional install/lint/test command hints, your GitLab username, and the worktree scratch directory (`worktree_root`, defaults to `~/.loopx/worktrees`) | Dashboard **Loops → GitLab Issues → Projects** page: the Track switch on each Project aliases row, or copy [`config/projects.json.template`](https://github.com/encoreshao/loop-x/blob/main/config/projects.json.template) by hand, or let `install-loopx.sh` do it |
| ↳ per-project `instance` (optional)   | Overrides the top-level `gitlab_instance` for one project — set this when your projects span more than one GitLab instance. Falls back to `gitlab_instance` when omitted.                                               | Same file, per project entry — see the template's `harbor` example                                                                                                            |
| `~/.loopx/topics.json`     | Which topics to monitor and what counts as notable for each one (topic monitor loop only)                                                                                                                               | Copy [`config/topics.json.template`](https://github.com/encoreshao/loop-x/blob/main/config/topics.json.template) by hand, or let `install-loopx.sh` do it                                                                |
| `~/.loopx/inboxes.json`    | Which mailboxes to triage (provider, account, categories, VIP/excluded senders, Slack bundle) and the shared default category set (Inbox Triage loop only)                                                              | Dashboard's **Loops → Inbox Triage → Setup** (`/loops/inbox-triage-loop?view=setup`), or let `install-loopx.sh` scaffold it from [`config/inboxes.json.template`](https://github.com/encoreshao/loop-x/blob/main/config/inboxes.json.template)               |
| `~/.loopx/mail_oauth.json` | The Gmail/Outlook OAuth app's own client ID (and, for Google, client secret) — a one-time app-registration step, not a per-mailbox credential                                                                           | Dashboard's **Loops → Inbox Triage → Setup** page                                                                                                                                                     |
| `~/.loopx/loops.json`      | The registry of scheduled loops: each entry's name, schedule (weekdays/hour/minute), entry point module, timeout, and per-loop knobs — read by `loopx loops`, polled by `loopx scheduler`                 | Copy [`config/loops.json.template`](https://github.com/encoreshao/loop-x/blob/main/config/loops.json.template) by hand, or let `install-loopx.sh` do it                                                                  |
| `~/.loopx/loop_scheduler_state.json` | Per-loop last-attempted date, so the scheduler never runs the same loop twice in a day — not something you hand-edit                                                                                            | Written automatically by `loopx scheduler`; seeded with today's date for every registered loop by `install-loopx.sh` and on the first launch of the Mac app / `loopx serve`, so enabling the scheduler doesn't fire an immediate run |
| `~/.loopx/instructions.md` (+ `instructions/<loop>.md`) | Your own free-text instructions, global and per loop, read at the start of every run                                                                                                                                             | Dashboard **Settings** page's Instructions tab                                                                                                                               |
| `~/.loopx/ai_cli.json`     | Which AI CLI (Claude Code, Codex, Gemini, Kimi or Cursor) `loopx run` invokes for every registered loop; defaults to `claude`                                                                                                        | Dashboard **Settings** page's AI CLI tab, or let `install-loopx.sh` do it                                                                                                                |
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

Both log to `outputs/history/`, and both also append every AI CLI invocation's output to `logs/loop-engineering.log` (viewable on the dashboard's **Runs → Logs** page); you can also trigger the GitLab loop from the dashboard's **Run now** button (Dashboard → Overview) without a terminal.

**On a schedule** — three ways:

- the LoopX app's built-in scheduler: while the app is open and **Settings → Run scheduled loops while LoopX is open** is on, it runs `loopx scheduler` every minute;
- launchd agents: `install-loopx.sh` renders the templates under [`launchd/loopx/`](https://github.com/encoreshao/loop-x/tree/main/launchd/loopx) into `<install dir>/launchd/` once, and you load them with a click each from the dashboard's **Settings → Daemons** page (which also shows whether each is currently loaded and its PID), or by hand with `launchctl load -w`;
- or run `loopx scheduler` from any scheduler of your own.

To check it is working, run `loopx scheduler status` (add `--json` for scripts): it shows when the last tick ran (each tick writes `~/.loopx/scheduler_heartbeat.json`; more than 3 minutes ago counts as stalled), what is queued or running, and each loop's next due time and last run, and exits 1 when something is wrong. `loopx doctor` includes the same tick check.

| Agent                                   | Runs                                                                                                                                             |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------|
| `com.hermes.loop-engineering`           | `loopx scheduler`, every minute (`StartInterval`) — queues what is due; a worker runs the queue one loop at a time — runs whichever loop(s) registered in `~/.loopx/loops.json` are due |
| `com.hermes.loop-engineering-dashboard` | The web dashboard (`loopx serve`), always-on (`RunAtLoad` + `KeepAlive`)                                                                          |
| `com.hermes.loopx.backend`              | The backend bundled in `LoopX.app`, registered by the app itself (not a template in this repo)                                                    |

Which loops run and on what schedule is config, not code — edit `~/.loopx/loops.json` (see [`config/loops.json.template`](https://github.com/encoreshao/loop-x/blob/main/config/loops.json.template)) to add a loop or change when it's due; adding a loop needs a new `loops.json` entry, not a new plist. The **Settings → Daemons** page's per-agent schedule editor only applies to a plist's own `StartCalendarInterval`, which `com.hermes.loop-engineering` does not have (it ticks every minute on a fixed `StartInterval` and defers to `loops.json` for which loop is actually due) — editing a loop's own schedule is done in the **Loops** pages or by hand in `loops.json`.


## The dashboard

A localhost-only web UI (server-rendered pages, no JS framework) served by `loopx serve` — one Rust binary, also the API the Mac app talks to. `loopx serve --port N` picks the port (default `8420`; `--port 0` picks a free one). `install-loopx.sh` picks a random port in `48420`-`48620` the first time it renders the always-on `launchd` agent (overridable with `--port`, and never re-picked on a later `--upgrade`) — check `launchd/com.hermes.loop-engineering-dashboard.plist` in the install directory for the port an existing install is actually running on.


| Sidebar entry | Shows |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Dashboard** (`/`) | Views: Overview — Current/last run status, a live progress indicator, and the Run now button; Activity — A message thread with the loop, plus its own live progress indicator. Paste a GitLab issue link here to have the loop work on that one issue immediately, regardless of who it's assigned to. |
| **Loops** (`/loops`) | A catalog splitting active loops from available ones; each loop that's visible also appears as a child link under **Loops** in the sidebar (Inbox Triage stays hidden there while it's disabled and has never run) |
| — **GitLab Issues** (`/loops/gitlab-loop`) | Views: Live — Your currently assigned issues and open MRs, fetched live; Projects — Manage `~/.gitlab/config.json` (instances, project aliases, access bundles) and `~/.loopx/projects.json` (tracked projects, loop settings) without hand-editing JSON |
| — **Topic Monitor** (`/loops/topic-loop`) | Views: Live — Status and saved briefings for every configured topic; Topics — Add, edit, and delete monitored topics — a separate view on the same page, so configuration doesn't clutter the Live status view |
| — **Inbox Triage** (`/loops/inbox-triage-loop`) | Views: Inbox Triage's own live status (**Live**) plus mailbox connection and categories (**Setup**: connect Gmail/Outlook, categories, VIP/excluded senders, Slack bundle) |
| **Runs** (`/runs`) | Views: Loop Runs — Every run recorded under `outputs/loop-runs/` (one per issue or topic processed), most recent first — read-only; an overview strip shows total runs, success/escalation rate, average cost, and the experimental Loop Efficiency Score; History — Every past run's review report, newest first; Logs — The tail of `logs/loop-engineering.log` - every AI CLI invocation's output, across the GitLab loop, the topic monitor loop, and this dashboard's own chat assistant |
| **Insights** (`/insights`) | Views: Analytics — The loop's performance over a selectable day window: a Loop Health score, outcomes, quality, risk & classification, failure breakdown, and learning trends; Cost — AI usage cost — the GitLab issue loop's own windowed cost, and total cost across every run under `outputs/loop-runs/`; Budget — Each recorded run's last-known budget status, plus rollups by loop definition and by day/week/month; Memory — Cross-run lessons recorded per project, one markdown file per GitLab issue, plus anything recorded before this format existed (shown under "Legacy learnings") |
| **Harness** (`/harness`) | Views: Audit — A score and pass/fail checks for each loop definition |
| **Connectors** (`/connectors`) | Views: Accounts — Every connector account grouped by type, with capability chips, a Test button (**Send test message** for notification targets) showing its last result, and a "Managed on …" badge linking to the owner page for external accounts; Add — Pick a type and fill its form. Secrets go in a password field and are never shown again (leave blank to keep the stored one when editing) |
| **Settings** (`/settings`) | Views: General — Notifications (manage `~/.slack/config.json`'s default webhook), AI CLI (choose Claude Code, Codex, Gemini, Kimi or Cursor, with a live installed/not-found check for each; Gemini, Kimi and Cursor are marked experimental), Appearance (color mode, accent theme, auto-refresh interval — saved to this browser's `localStorage`), and Instructions (your own free-text instructions, read by the loop at the start of every run) — clustered as tabs on one page (the **General** view is split into Notifications / AI CLI / Appearance / Instructions tabs (`?tab=`)); Daemons — Load state, an editable schedule, and enable/disable for every `launchd` agent, plus a Registered Loops breakdown of every loop the unified scheduler runs (its own schedule and last-run status, read from `loops.json`); Skills — Every external skill this loop depends on, and whether it's actually installed |

Every older URL (`/activity`, `/gitlab`, `/topic-monitor`, `/inbox`, `/loop-runs`, `/history`, `/logs`, `/analytics`, `/cost`, `/budget`, `/memory`, `/audit`, `/settings/general`, `/daemons`, `/skills`, and so on) permanently redirects (301) to its new home, query string kept, so bookmarks keep working.


## Connectors

A connector is an account the loops can talk to: a GitLab or GitHub instance, a Slack, Telegram or chat webhook, a Notion workspace, an RSS feed list, a Jira or Linear workspace, a mailbox, a Google Calendar, Google Slides. Manage them on the dashboard's **System → Connectors** page (`/connectors`). Each type declares *capabilities* (`issues`, `merge_requests`, `pipelines`, `notify`, `feed`, `mail`, `docs`, `calendar`, `slides`), and a loop can require a capability instead of a specific product.

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
| Google Slides | `slides` (shown as Slides) | none | Google sign-in (`presentations` plus `drive`: copying a template someone else shared needs full Drive access); the refresh token goes in the Keychain |

**Gallery and form.**

- **Add** opens a gallery of connector types, grouped into Google (first: Gmail, Google Calendar and Google Chat), Code hosting, Chat & notifications, Work tracking, Knowledge, Feeds and Mail (Outlook lives here), with a search box to filter them. Cards follow the accent selected in **Settings → Appearance** and each service's brand color, and each chat webhook preset has its own description.
- Each tile and each account row carries the service's brand logo (inline Simple Icons marks; services without one — Feishu, DingTalk, the generic webhook — get a lettermark).
- The chat webhook tile expands into the presets above, each with a one-line hint (for example, Teams Workflows webhooks may require Adaptive Cards) and a **Where do I get this?** link to that service's own docs.
- The add/edit form has an **Account** section (**Label** and **Connector id**; the id is suggested from the label until you edit it), a **Connection** section (the type's settings) and a **Credentials** section (the secret).
- Sign-in guide for Gmail and Calendar: [`authentication.md`](authentication.md).
- **Google Calendar** has no pasted secret: click **Connect with Google** (**Reconnect** once connected) to sign in. It reuses the Google OAuth client you already set up for Gmail on Inbox Triage's setup page (Gmail tab; if it is missing, the form links you there), so Google Cloud needs the same redirect URI, `http://127.0.0.1:<port>/oauth/google/callback`. The account row shows a **Connected** / **Not connected** marker, and **Test** reads the calendar. It requests `calendar.readonly` and `calendar.events`; a calendar connected before the second scope existed keeps working read-only until you click **Reconnect**.
- **Google Slides** also signs in with **Connect with Google** and reuses the same OAuth client (enable the **Google Slides API** and **Google Drive API** in Google Cloud). **Templates and decks** on its row opens `/connectors/slides`: **Refresh from Drive** caches the presentations you own or that were shared with you (`~/.loopx/slides/<id>.json`, file ids, names and links only), and the ticked ones are the only templates the chat can copy. Put `{{placeholders}}` in a template's text; the chat tools `slides.list_templates`, `slides.template_info`, `slides.create_deck` (a write: it waits for your approval) and `slides.review_deck` fill them in. Created decks are listed on the same page, where **Review** shows each slide's thumbnail and text next to an **Open in Google Slides** link. A connector signed in before this existed does not need to change.
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
| `loopx scheduler` | The tick launchd (or the app) runs every minute: reads `~/.loopx/loops.json`, adds whichever registered loop(s) are due to the run queue (`~/.loopx/run_queue.json`) and starts the queue worker, which runs them one at a time |
| `loopx loops`, `loopx ai-cli` | The loops registry (`loops.json`: name, schedule, entry point) and which AI CLI (`claude`, `codex`, `gemini`, `kimi` or `cursor`) every loop invokes (`ai_cli.json`) |
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

Fixed, and does not loosen with time or repeated success (see [`docs/tasks/gitlab-issue-loop.md`](https://github.com/encoreshao/loop-x/blob/main/docs/tasks/gitlab-issue-loop.md)):

- **Never merges a merge request.** The loop's job ends at "MR opened, verification passing" — merging is always a manual human step.
- Every code change happens on a `loop/issue-<iid>` branch, never on the target branch directly: in its own git worktree, or in the checkout itself for a project tracked with worktree off.
- The agent works out each project's install/lint/test commands and logs them; an MR only opens after the loop itself re-runs that lint and test command and they pass, and the diff only touches files relevant to the issue.
- No arbitrary shell, no dependency upgrades, no reading `.env`/credentials/SSH keys — only the command allow-list in `instructions/loopx/gitlab-issue.md`.
- Issues are processed one at a time, sequentially, never in parallel.
- A verification failure on the same issue is never retried within a run — it escalates via a GitLab comment instead. (With `verification.mode: gate` the loop itself re-runs the project's checks and allows one bounded retry with the failing output as feedback; it never opens an MR whose tests/lint fail, and escalates with the `loop:needs-human` label instead.)

The Inbox Triage loop has its own fixed safety boundary (see [`docs/tasks/inbox-triage-loop.md`](https://github.com/encoreshao/loop-x/blob/main/docs/tasks/inbox-triage-loop.md)):

- **Never sends mail.** Neither mail provider module contains a send function, and the Outlook token it obtains is scoped without `Mail.Send` — sending is impossible at the token level, not just the code level.
- **Never archives, deletes, moves, or changes read state.** The only mailbox writes are creating `Loop/*` labels/categories, applying them, and creating reply drafts left in the mailbox's own Drafts folder.
- **Only `Loop/*` labels are ever applied.** Every category label, default or custom, must start with `Loop/` — `inboxes.json` is rejected on load otherwise, so a hand-edited system label like `TRASH` or `UNREAD` can never be applied to real mail.
- **Message bodies never persist.** They exist only in memory and in the prompt of the AI CLI call per inbox (plus at most one retry), which keeps no session transcript — never written to `outputs/`, logs, status, or the Slack digest, which only ever get sender, subject, category, the AI's short reason, and a draft link. The AI-written reply draft is saved only in the mailbox's own Drafts folder.
- **Inbox Triage runs on the selected AI CLI.** CLIs that cannot be locked down (Codex, Gemini, Kimi, Cursor) run with a visible warning; Codex gives the model a shell and records the prompt, including email text, under `~/.codex/sessions/`. Claude is the one CLI run with no tools. Error text shown for an inbox is only loopx's own (exit code, timeout, could not start), never the CLI's output.
- **Refresh tokens live only in the macOS Keychain**, written via `security -i` with the token on stdin, never on disk in plain text or in a process's argv.

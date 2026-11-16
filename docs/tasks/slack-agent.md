# Slack Agent

## Goal

Mention `@LoopX` in Slack (a channel, a thread or a direct message) from a phone
or any Slack client. Loop X on your Mac understands the request, runs a loop or
an ad-hoc agent task, and replies in the same thread with progress and a result.
It is the same chat agent and tool registry as the dashboard's Office view, with
Slack as the front door. It is not a hosted service: nothing is public, and
nothing runs when your Mac is asleep, off or logged out.

## How it runs

- Code: `crates/core/src/slack_agent/*` (the listener and everything it needs),
  `crates/core/src/connectors/slack_bot.rs` (the `slack_bot` connector type),
  `crates/core/src/chat_tools/private_ledger.rs` (the private-call ledger),
  `crates/cli/src/slack_cmd.rs` (the `loopx slack` subcommands),
  `crates/server/src/ui/pages/slack_agent.rs` (the Settings card).
- Transport: Slack Socket Mode. The Mac opens an outbound `wss://` connection
  with an app-level token. No public URL, tunnel or relay.
- Process: `loopx slack listen`, run by its own launchd agent
  `com.hermes.loopx-slack`, so Slack works while the Mac is awake even with the
  Loop X app closed. The agent is installed only on an explicit button press.
- Flow of one message: Slack acks within 3 seconds, before any work; intake
  drops anything that is not an `app_mention` or a DM, is from a bot, is not
  from an allowlisted user, or was already seen (recent event ids are kept in
  `outputs/slack/seen.json`); the mention is stripped and the text becomes a user
  message in the thread's chat session; the bot reacts with eyes and posts
  "On it…"; the chat agent runs (it may enqueue loops through its tools); the
  progress message is edited as tools start; the final text replaces it (split if
  long) and the eyes reaction becomes a check, or an X on failure.
- A Slack thread is a chat session. `thread_ts` maps to a session id
  (`outputs/slack/threads.json`), so follow-ups keep their context and the
  conversation also shows up in the dashboard's chat sessions.
- A Slack turn is stored as already seen by loops: it belongs to its chat
  session and is never picked up as a new message by another loop.
- State directory: `outputs/slack/` under the Loop X root holds `seen.json`,
  `threads.json`, `status.json` and `listener.lock`. Logs are
  `slack-listener.out.log` and `slack-listener.err.log` in the launchd log
  directory.

## Setup

1. **Create the Slack app from the manifest.** In Settings, Connectors, add a
   **Slack agent** and press **Copy Slack app manifest** (or run
   `loopx slack manifest`). In Slack, go to api.slack.com/apps, Create New App,
   From a manifest, paste it, and install the app to your workspace. The manifest
   turns on Socket Mode, the `app_mention` and `message.im` events, interactivity
   and these bot scopes: `app_mentions:read`, `chat:write`, `reactions:write`,
   `im:history`, `im:write`. Offline catch-up (below) is optional and needs
   seven more, listed as commented-out lines in the manifest: `channels:read`,
   `groups:read`, `im:read`, `mpim:read` (to list the conversations) and
   `channels:history`, `groups:history`, `mpim:history` (to read them).
2. **Paste the tokens.** The bot token (`xoxb-`, OAuth and Permissions) and an
   app-level token (`xapp-`, Basic Information, App-Level Tokens, with the
   `connections:write` scope). Both go to the macOS Keychain and are never shown
   again.
3. **Set the allowlist.** The Slack user IDs (`U01ABC2DEF`; Slack profile, More,
   Copy member ID) allowed to use the agent, one per line or comma-separated.
   An empty allowlist makes the listener refuse to start. Everyone on the list
   uses your agent as you: they get replies with your private data (mail,
   calendar) by DM and can answer your approvals. List only yourself unless
   you mean that.
4. **Save, then Test connection.** Save the connector first (Test connection
   checks a saved account). It checks both tokens against Slack and starts
   nothing. `loopx slack test` does the same from a terminal.
5. **Install background listener.** A separate, explicit opt-in: saving the
   connector or the tokens never starts anything. The confirmation states four
   things: it writes `~/Library/LaunchAgents/com.hermes.loopx-slack.plist` and
   loads it with launchd; it runs in the background while you are logged in;
   it can start agent work from Slack messages from the allowlisted users; and
   everyone on the allowlist can use your agent as you, getting replies with
   your mail and calendar data by DM and answering your approvals.
   `loopx slack install` shows the same text and needs `--yes` when not run from
   a terminal.
6. **Send a message.** Mention `@LoopX` in a channel the app was invited to, or
   open a DM with the app, and ask something.

Optional settings on the connector: **Approvals here** (`approvals`, off by
default) and **Requests per hour** (`rate_per_hour`, default 10).

The listener reads the connector once when it starts. Saving the connector in
the dashboard while the listener is installed restarts it, so a changed
allowlist, on/off switch or rate takes effect at once (the message after Save
says so). If the dashboard cannot reach launchd (a development sandbox) or the
settings change another way, run `loopx slack restart`.

No real Slack workspace test was done for this feature. It was exercised only
against a local fake Socket Mode server and a fake Slack web API; expect rough
edges on first contact with a real workspace.

## Commands

`loopx slack listen [--account ID]` runs the listener in the foreground;
`status`, `test`, `install`, `uninstall`, `restart` and `manifest` manage it.
`install`, `uninstall`, `restart` and `status` accept `--launch-agents-dir`,
`--launchd-dir` and `--real-launchd` for sandboxed runs (see
`docs/dev-sandbox.md`). `restart` does a `launchctl kickstart`.

## Safety model

- **Allowlist.** Only Slack user IDs on the allowlist can start work. Anyone else
  gets one fixed reply ("You're not authorized to use LoopX"), at most once per
  ten minutes per user, with no machine or setup detail.
- **Everyone on the allowlist acts as you.** There are no per-user permissions:
  each allowlisted person uses your full Loop X agent, gets replies built from
  your mail and calendar by DM, and can press approval and catch-up buttons.
- **Opt-in.** Off by default; an upgrade adds no agent and opens no socket.
- **Approvals through Slack.** With **Approvals here** on, an approval request
  is sent as a DM to the first allowed user with Allow and Deny buttons. Any
  allowlisted user can answer; the button is bound to that pending approval id,
  and a decided or expired approval cannot be decided again. The shared approvals
  store takes a lock on decisions so two answers (Slack, dashboard, Mac app,
  Telegram) cannot both win. Merging, sending, deleting and posting already
  require approval whatever the channel.
- **Slack text is untrusted.** It is user content, never part of the system
  prompt. The agent keeps the chat lock-down: tools only through the `chat-tool`
  registry, no free shell, no file read or write, hooks disabled.
- **Private data stays out of channels.** If the agent touches mail or calendar
  (see below), the answer goes to the asker by DM, and the channel gets a short
  notice. The asker is whoever wrote the message, so an allowlisted colleague
  gets your mail or calendar answer in their own DM.
- **Replies are escaped.** `&`, `<` and `>` in everything the agent posts are
  escaped, so a reply cannot ping `@channel`, `@here` or a user, or show a link
  under a different label. Markdown in replies is shown as written.
- **Rate limit.** At most 10 requests per user per hour by default (set it with
  **Requests per hour**); one active task per thread, a second message in a busy
  thread gets "Still working on your last message in this thread."
- **Tokens** live only in the Keychain. Errors, logs, the status file and the
  plist carry none; the plist holds only paths.

## Behaviors worth knowing

- **Google files linked in Slack are not granted to the agent**, unlike in the
  dashboard chat. A Drive link in a Slack message is just text.
- **Private-call ledger.** Before any mail or calendar tool runs, `loopx chat-tool`
  appends a line to `outputs/chat-private/<session>.jsonl`. The executor reads it
  to decide where a reply goes. A thread that ever touched mail or calendar is
  DM-only from then on (the ledger is sticky per thread, and an unreadable ledger
  counts as private). The ledger is never pruned. In a channel, the progress
  message does not name private tools.
- **Offline catch-up.** On the first connection after start and after every
  reconnect, the listener looks back at most 10 minutes for @mentions and DMs
  from allowlisted users that got no reply, and posts "I was offline when you
  wrote this. Run it now?" with Run and Ignore buttons. It never runs old
  messages by itself. Mentions inside threads are not caught up (Slack's history
  call returns top-level messages only). Catch-up needs the optional
  `channels:read`, `groups:read`, `im:read` and `mpim:read` scopes (to list the
  conversations with `users.conversations`) and `channels:history`,
  `groups:history`, `im:history` and `mpim:history` (to read them); the
  manifest lists the optional ones as commented-out lines instead of
  requesting them. Without the read scopes the status notes "catch-up
  unavailable (missing scope)" until a later pass succeeds; a conversation
  whose history cannot be read is skipped and the rest, DMs included, are
  still scanned. A failed offer post is not retried, and one reconnect scans at
  most 50 channels and posts at most 10 offers.
- **Asleep or off means no delivery.** Slack does not queue Socket Mode messages
  for a Mac that is asleep, off or logged out; those messages are lost, apart
  from what catch-up can offer within its 10-minute window.
- **Approvals via Slack buttons only work while the listener runs.** If it is
  stopped, the dashboard, the Mac app and Telegram still answer approvals, and
  approvals still expire.
- **Tokens saved in the dashboard live in the dashboard's Keychain service.**
  When the dashboard runs with a pinned `LOOP_ENGINEERING_HOME`, the terminal
  CLI uses a different service. Saving tokens in the dashboard and then running
  `loopx slack install` in a terminal fails preflight with "The Slack bot token
  (xoxb-) is not set". Install from the dashboard, or save the tokens from the
  same environment. The plist pins `LOOP_ENGINEERING_HOME`, `LOOPX_ROOT` and
  `LOOPX_KEYCHAIN` from the installer's environment so the listener reads the
  same Keychain service.
- **The plist is protected.** It cannot be toggled from the Daemons page (which
  shows "Managed by Slack settings") or from the chat tool. Use Settings,
  Connectors, Slack, or `loopx slack uninstall`.
- **Deleting the Slack agent connector uninstalls the listener** when it is
  the account the listener runs as (the first enabled Slack agent); deleting
  another Slack agent leaves it installed. `uninstall-loopx.sh` removes it too.
- **A rejected or revoked token stops the listener.** Status becomes "Needs
  attention" and a Needs you entry appears in the dashboard and Mac app. The
  same happens for swapped tokens (Slack's `not_allowed_token_type`) and for an
  app-level token without `connections:write`, which retrying cannot fix. A
  native macOS notification banner is not implemented yet (a follow-up).
- **A one-off Slack API or network error** shows as the last error only until
  a later Slack call succeeds; a missing scope stays until a restart.

## Exit codes and launchd

The agent runs with `KeepAlive` `{SuccessfulExit: false}` and a 30-second
`ThrottleInterval`. Exit codes of `loopx slack listen`:

| Code | Meaning | Relaunched by launchd |
|---|---|---|
| 0 | clean stop, or Slack rejected a token | no |
| 78 | misconfigured (no account, missing token, empty allowlist) | yes, throttled |
| 1 | another listener already holds the lock (after retrying for about 2 seconds) | yes, throttled |

Any other non-zero exit, such as a crash, is relaunched too. A rejected token
exits 0 on purpose: relaunching would only hit Slack again with the same bad
token. While the listener is installed but misconfigured (exit 78 every 30
seconds), the dashboard card shows the reason instead of the login hint.

## Status

The dashboard card (Settings, Connectors, Slack) shows **Not installed**,
**Installed, not running**, **Offline** (running, not connected; it reconnects
by itself with backoff), **Connected** (since, last event), **Needs attention**
(with a token-free reason), or **Not running** when the status file says
connected or offline but nothing holds the listener lock (it stopped
unexpectedly). A listener started by hand gets the note "Running by hand".
`loopx slack status` prints the same states: `not installed`,
`installed, stopped`, `installed, offline since ...`,
`installed, connected since ...`, `installed, needs attention: ...` and
`installed, not running (it stopped unexpectedly; ...)`, with
`running by hand` in place of `installed`.

## Troubleshooting

- **Token rejected, status "Needs attention".** Paste a new token in Settings,
  Connectors, Slack and Save, then press **Restart** (or `loopx slack restart`).
  The listener exited 0, so launchd does not relaunch it by itself.
- **Missing scope.** Add the scope in the Slack app (OAuth and Permissions),
  reinstall the app to the workspace, then Restart. For catch-up, add
  `channels:read`, `groups:read`, `im:read`, `mpim:read`, `channels:history`,
  `groups:history` and `mpim:history` (`im:history` is already requested).
- **Swapped tokens.** "Slack refused the bot token: it is not a bot token" (or
  the app-level one): the `xoxb-` and `xapp-` tokens are in each other's field.
  Paste them in the right fields, Save, then Restart.
- **"Another Slack listener is already running" / exit 1.** Another
  `loopx slack listen` (often one started by hand) holds `outputs/slack/listener.lock`.
  Stop it, or uninstall the background listener if you prefer the hand-run one.
- **Messages sent while the Mac was asleep are missing.** Expected; see above.
  Only the last 10 minutes of top-level messages can be offered after reconnect.
- **Not running.** The status file says it should be running but it is not. Run
  `loopx slack restart` and read `slack-listener.err.log`.
- **Approval buttons do nothing.** The listener is not running; start it with
  Restart. Approve from the dashboard, Mac app or Telegram meanwhile.
- **Install says "missing token" after saving the tokens.** See the Keychain
  service note above.

## Not in scope

Multiple workspaces or users per install, per-user permissions, slash commands,
App Home, file uploads into the agent, status and help commands handled outside
the agent, and running Slack-triggered work while the Mac is asleep.

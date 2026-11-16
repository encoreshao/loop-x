# Slack Agent

Setup, configuration, testing and troubleshooting for the Slack agent in one
place. The first half is for someone setting it up; the second half is the
reference (how it runs, safety, behaviors, exit codes).

## Goal

Mention `@LoopX` in Slack (a channel, a thread or a direct message) from a phone
or any Slack client. Loop X on your Mac understands the request, runs a loop or
an ad-hoc agent task, and replies in the same thread with progress and a result.
It is the same chat agent and tool registry as the dashboard's Office view, with
Slack as the front door. It is not a hosted service: nothing is public, and
nothing runs when your Mac is asleep, off or logged out.

No real Slack workspace test was done for this feature beyond a first
connection: it was exercised against a local fake Socket Mode server and a fake
Slack web API, so expect rough edges on first contact with a real workspace.

## Quick start

Before you start, check whether your workspace **already has a Slack app named
LoopX** (for example one you made earlier for webhook notifications). The
manifest below names the new app and its bot `LoopX` too. Two apps with the same
name are easy to mix up: you can paste the tokens of one and `@mention` the bot
of the other, and then nothing happens. If one exists, change both names in the
manifest (`display_information.name` and `features.bot_user.display_name`) to
something distinct, such as `LoopX Agent`, before you paste it. Do not paste the
manifest over an existing app: it replaces that app's settings and can break
its incoming webhook.

1. **Create the Slack app from the manifest.** In the dashboard open Connectors,
   Add, **Slack agent**, and press **Copy Slack app manifest** (or run
   `loopx slack manifest`). In Slack go to https://api.slack.com/apps, Create New
   App, **From an app manifest**, pick your workspace, paste it, Create, then
   **Install to Workspace** (a workspace admin may have to approve it). The app
   is a normal single-workspace bot app with Socket Mode on; it is not a legacy
   "classic" app and is not published to the Slack Marketplace.
2. **Get the two tokens.** The **bot token** (`xoxb-`) is on OAuth and
   Permissions after the install. The **app-level token** (`xapp-`) is on Basic
   Information, App-Level Tokens, Generate Token and Scopes, with the
   `connections:write` scope. Both go to the macOS Keychain and are never shown
   again.
3. **Add the Slack agent in Loop X.** Paste both tokens. Set **Allowed users** to
   your Slack member ID (your Slack profile, More, **Copy member ID**; it looks
   like `U01ABC2DEF`; names and `@handles` do not work). An empty list makes the
   listener refuse to start. Everyone on the list uses your agent as you, so list
   only yourself unless you mean otherwise (see the safety model). **Save** the
   connector.
4. **Test connection.** Press it on the saved connector. It checks both tokens
   against Slack and starts nothing: `Bot token: OK (<your workspace>)` and
   `App-level token: OK`. `loopx slack test` does the same from a terminal.
5. **Install background listener.** A separate, explicit opt-in: saving the
   connector or the tokens never starts anything. The confirmation sheet states
   what it will do (see "How it runs"). Install from the dashboard, not from a
   terminal (see the Keychain note under "Behaviors worth knowing").
6. **Invite the bot and send a message.** In a channel, run `/invite @LoopX` (or
   your renamed bot), then mention it. Or open the app in Slack's sidebar and use
   its **Messages** tab for a DM. See "Check that it works" for what you should
   see.

### Is it the right Slack app?

If the bot does not answer, confirm that the tokens, the invited bot and the app
you configured are all the same app. In the Slack app settings (api.slack.com/apps)
for the app whose tokens you pasted:

- **Event Subscriptions** lists the bot events `app_mention` and `message.im`.
- **App Home** has the **Messages** tab on, with "Allow users to send Slash
  commands and messages from the messages tab" ticked. (The manifest turns the
  Home tab off and the Messages tab on; an app showing **Home** and **About**
  but no **Messages** tab is not one made from this manifest.) Slack's labels
  can differ slightly from these.
- The app's description reads "Talk to LoopX from Slack" unless you changed it.

## Configuration reference

### In the Slack app (the manifest sets all of it)

| Setting | Value |
|---|---|
| Socket Mode | on (no public URL needed) |
| Bot events | `app_mention`, `message.im` |
| Interactivity | on (Allow/Deny and Run/Ignore buttons over the socket) |
| App Home | Messages tab on, Home tab off |
| Token rotation | off |
| Bot scopes | `app_mentions:read`, `chat:write`, `reactions:write`, `im:history`, `im:write` |
| Optional scopes (offline catch-up) | `channels:read`, `groups:read`, `im:read`, `mpim:read` (list conversations), `channels:history`, `groups:history`, `mpim:history` (read them); commented out in the manifest, uncomment all seven to turn catch-up on |
| App-level token | `connections:write` (you create it; the manifest cannot) |

After adding a scope, reinstall the app to the workspace, check whether the bot
token on OAuth and Permissions changed (paste it again if it did), and restart
the listener.

### In Loop X (the connector fields)

| Field | Key | Default | Meaning |
|---|---|---|---|
| Bot token | secret | none | the `xoxb-` token, kept in the Keychain |
| App-level token (xapp-) | secret | none | kept in the Keychain; shown only as "Saved" or "Not set" |
| Allowed users | `allowed_users` | none (required) | Slack member IDs, one per line or comma-separated; only these can start work |
| Approvals here | `approvals` | off | send approval requests as a DM with Allow and Deny buttons |
| Requests per hour | `rate_per_hour` | 10 | most requests one person can start per hour |
| enabled | account switch | on | the listener runs as the first enabled Slack agent |

The listener reads the connector once when it starts. Saving the connector in
the dashboard while the listener is installed restarts it, so a changed
allowlist, on/off switch or rate takes effect at once (the message after Save
says so). If the dashboard cannot reach launchd (a development sandbox) or the
settings change another way, run `loopx slack restart`.

### Where things live

Under the Loop X root (normally `~/.loopx`):

| Path | What |
|---|---|
| `outputs/slack/status.json` | the listener's state (what `loopx slack status` and the dashboard card read) |
| `outputs/slack/seen.json` | recent event and message ids, so a redelivery runs once (created when the first message is accepted) |
| `outputs/slack/threads.json` | Slack thread to chat session map (created with the first run) |
| `outputs/slack/listener.lock` | held by the running listener |
| `outputs/chat-private/<session>.jsonl` | the private-call ledger (see below) |
| `logs/slack-listener.err.log`, `logs/slack-listener.out.log` | the listener's log |
| `~/Library/LaunchAgents/com.hermes.loopx-slack.plist` | the launchd agent, present only after Install |

### Commands

`loopx slack listen [--account ID]` runs the listener in the foreground;
`status`, `test`, `install`, `uninstall`, `restart` and `manifest` manage it.
`install`, `uninstall`, `restart` and `status` accept `--launch-agents-dir`,
`--launchd-dir` and `--real-launchd` for sandboxed runs (see
`docs/dev-sandbox.md`). `restart` does a `launchctl kickstart`.

## Check that it works

Keep the first messages read-only ("hi, what can you do?", "show my recent
runs"). Free text goes to the same agent as the Office chat, which can start real
loops, and every request uses your selected AI CLI (Settings, AI CLI), so it
costs tokens. Merging, sending, deleting and posting still need your approval.

1. **Before installing.** Test connection shows both tokens OK. The card says
   **Not installed**.
2. **After Install.** The card, and `loopx slack status`, should show
   `installed, connected since <time>` within a few seconds.
3. **A DM or a mention.** You should see an eyes reaction on your message, an
   `On it…` reply in a thread, then the answer replacing it, with the eyes
   becoming a check mark (an X if it failed). A follow-up in the same thread
   keeps the context. `loopx slack status` now shows a fresh **Last event**.
4. **Someone not on the allowlist** who mentions the bot gets one reply, "You're
   not authorized to use LoopX", at most once per ten minutes, and nothing runs.
   A line `unauthorized Slack user ... (answered)` goes to the log.
5. **A fast sender.** More than **Requests per hour** requests an hour gets
   "You're going a bit fast; try again in a bit."; a second message in a busy
   thread gets "Still working on your last message in this thread."
6. **Private data.** If a mail or calendar connector is set up and the agent
   uses it, the answer arrives by DM and the thread says "I sent the answer to
   you in a DM because it includes private data."
7. **Offline.** Turn the network off for a minute and back on: the card goes
   **Offline**, then **Connected** again. With the optional scopes, a message sent
   meanwhile gets "I was offline when you wrote this. Run it now?" with Run and
   Ignore buttons; it never runs old messages by itself.
8. **A revoked token.** Regenerate the bot token in Slack: the next reply fails,
   the listener stops, the card shows **Needs attention** and a Needs you entry
   appears. Save the new token and press **Restart**.
9. **Clean up.** **Uninstall** removes the agent; `launchctl list | grep loopx-slack`
   shows nothing afterwards.

### What the log shows

`slack-listener.err.log` is deliberately quiet and never carries message text or
tokens. You will see: `listening as account <id>`; `catch-up: N offer(s)
posted`; `catch-up unavailable (missing scope)`; `unauthorized Slack user <id>
in <channel> (answered | not answered again)`; `approval button from <user>:
...`; catch-up button problems; and `message in <channel> already handled; not
run again`. **Events the listener ignores are not logged at all.** It ignores
anything that is not an `@mention` or a DM, anything from a bot, and any message
Slack marks with a subtype (an edit, a file share, a thread broadcast). A silent
log therefore does not mean no events arrived; **Last event** tells you that.

## Status

The dashboard card (Connectors, Slack agent) shows **Not installed**,
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

**Last event** is updated for every event Slack delivers to the listener, before
it decides what to do with it. So a time there means Slack is delivering, not
that a message was answered. `none` means nothing has arrived since the
listener started. Events the manifest does not ask for count too if the app
sends them (an older app that is also subscribed to `app_home_opened`, for
example, reports an event whenever you open its page); the listener ignores
them. **Last error** can also hold a
harmless note such as "catch-up unavailable (missing scope)"; it is not a failure
and does not block normal use.

## Troubleshooting

Start with `loopx slack status` and the card. Then match the symptom:

- **Connected, but the bot never reacts to my message.** In order of likelihood:
  1. *Wrong app.* Two Slack apps named LoopX, and the tokens belong to one while
     you invited or mentioned the other (see "Is it the right Slack app?").
  2. *Bot not in the channel.* Run `/invite @<bot name>`. Mentions in a channel
     only reach a bot that is a member of it.
  3. *Events not subscribed.* Event Subscriptions must list `app_mention` and
     `message.im`; DMs also need the Messages tab on.
  4. *Not on the allowlist.* You get the "not authorized" reply and a log line.
     Check the member ID (not your name) in Allowed users.
  5. *Ignored on purpose.* The message had a subtype (an edit, an attachment) or
     came from a bot. Send plain text.
  Use **Last event**: `none` points to 1 to 3 (nothing reaches the listener); a
  recent time with no reply and no `unauthorized` log line means the event
  arrived and was ignored.
- **I do not see a Messages tab on the app.** You are looking at a different app
  than the one made from the manifest, or its Messages tab is off. Turn it on
  under App Home, or use the right app.
- **Last event moves when I merely open the app in Slack, but my messages do
  nothing.** The manifest does not subscribe to any event that opening the app
  would send, so this points at an app that was not made from this manifest (an
  older app with its own subscriptions, such as `app_home_opened`). That app
  probably is not subscribed to `app_mention` and `message.im` either. Use the
  app made from the manifest (1 to 3 above).
- **An eyes reaction but no answer.** The agent or the AI CLI failed. The thread
  says why in a short line ("Sorry, that didn't work.", "The assistant took too
  long and was stopped", or the CLI's own error). Check Settings, AI CLI and
  your CLI's login or quota.
- **Token rejected, status "Needs attention".** Paste a new token in the Slack
  agent connector and Save, then press **Restart** (or `loopx slack restart`).
  The listener exited 0, so launchd does not relaunch it by itself.
- **Missing scope.** Add the scope in the Slack app (OAuth and Permissions),
  reinstall the app to the workspace, then Restart. For catch-up, add the seven
  optional scopes listed above.
- **Swapped tokens.** "Slack refused the bot token: it is not a bot token" (or
  the app-level one): the `xoxb-` and `xapp-` tokens are in each other's field.
  Paste them in the right fields, Save, then Restart.
- **"Another Slack listener is already running" / exit 1.** Another
  `loopx slack listen` (often one started by hand) holds
  `outputs/slack/listener.lock`. Stop it, or uninstall the background listener if
  you prefer the hand-run one.
- **Messages sent while the Mac was asleep are missing.** Expected; see "Behaviors
  worth knowing". Only the last 10 minutes of top-level messages can be offered
  after a reconnect.
- **Not running.** The status file says it should be running but it is not. Run
  `loopx slack restart` and read `slack-listener.err.log`.
- **Approval buttons do nothing.** The listener is not running; start it with
  Restart. Approve from the dashboard, Mac app or Telegram meanwhile.
- **Install says "missing token" after saving the tokens.** See the Keychain
  service note below.
- **Slack asks for admin approval when installing the app.** A workspace admin
  must approve it first; nothing is wrong on the Loop X side.

## How it runs

- Code: `crates/core/src/slack_agent/*` (the listener and everything it needs),
  `crates/core/src/connectors/slack_bot.rs` (the `slack_bot` connector type),
  `crates/core/src/chat_tools/private_ledger.rs` (the private-call ledger),
  `crates/cli/src/slack_cmd.rs` (the `loopx slack` subcommands),
  `crates/server/src/ui/pages/slack_agent.rs` (the connector card).
- Transport: Slack Socket Mode. The Mac opens an outbound `wss://` connection
  with an app-level token. No public URL, tunnel or relay.
- Process: `loopx slack listen`, run by its own launchd agent
  `com.hermes.loopx-slack`, so Slack works while the Mac is awake even with the
  Loop X app closed. The agent is installed only on an explicit button press.
  The install confirmation states four things: it writes
  `~/Library/LaunchAgents/com.hermes.loopx-slack.plist` and loads it with
  launchd; it runs in the background while you are logged in; it can start agent
  work from Slack messages from the allowlisted users; and everyone on the
  allowlist can use your agent as you, getting replies with your mail and
  calendar data by DM and answering your approvals. `loopx slack install` shows
  the same text and needs `--yes` when not run from a terminal.
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

## Safety model

- **Allowlist.** Only Slack user IDs on the allowlist can start work. Anyone else
  gets one fixed reply ("You're not authorized to use LoopX"), at most once per
  ten minutes per user, with no machine or setup detail.
- **Everyone on the allowlist acts as you.** There are no per-user permissions:
  each allowlisted person uses your full Loop X agent, gets replies built from
  your mail and calendar by DM, and can press approval and catch-up buttons. To
  let teammates use it, add their member IDs and invite the bot to a channel;
  prefer to do that only while no mail or calendar connector is connected.
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
  call returns top-level messages only). Catch-up needs the optional scopes
  listed in the configuration reference. Without the read scopes the status
  notes "catch-up unavailable (missing scope)" until a later pass succeeds; a
  conversation whose history cannot be read is skipped and the rest, DMs
  included, are still scanned. A failed offer post is not retried, and one
  reconnect scans at most 50 channels and posts at most 10 offers.
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
  shows "Managed by Slack settings") or from the chat tool. Use the Slack agent
  connector, or `loopx slack uninstall`.
- **Deleting the Slack agent connector uninstalls the listener** when it is
  the account the listener runs as (the first enabled Slack agent), or when no
  Slack agent is enabled; deleting another Slack agent leaves it installed.
  The project's uninstall script removes it too.
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

## Not in scope

Multiple workspaces or users per install, per-user permissions, slash commands,
App Home content, file uploads into the agent, status and help commands handled
outside the agent, an "interrupted run, retry?" button, Slack formatting of
replies, and running Slack-triggered work while the Mac is asleep.

# Dashboard HTTP API contract

Source of truth: `bin/web/dashboard_server.py` (`DashboardHandler.do_GET` / `do_POST`, around lines 15657 and 16006), `bin/web/inbox_pages.py` (`handle_post`, `connect_status`, `handle_google_callback`), `bin/web/update_pages.py`, `bin/i18n.py`, and `tests/test_dashboard_server.py`. Every route is handled by one `BaseHTTPRequestHandler` subclass; there are no other dispatchers. The Rust `loopx` server must reproduce this surface.

## 1. Server model

- Binds `127.0.0.1:<port>` only (`DEFAULT_PORT = 8420`; `install.sh` may pick another and writes it into the launchd plist). `ThreadingHTTPServer`, daemon threads.
- HTTP/1.0 semantics: each response closes the connection (this is what ends SSE streams). Request logging is suppressed.
- No authentication, sessions or `Host` checks. Trust model: loopback bind plus the CSRF token on every POST. An nginx proxy (`bin/scripts/setup-nginx.sh`, `http://loop.x/`) may sit in front, hence `X-Accel-Buffering: no` on SSE.
- Startup side effects: `ledger.run_startup_backfill()` and `update_check.start_background()` (a real `git fetch` every 6h).
- Request bodies are only ever `application/x-www-form-urlencoded` (UTF-8, `errors="replace"`). The body is always read up front using `Content-Length`. There are no JSON request bodies and no multipart.
- Unmatched GET or POST: `404`, `text/plain`, body `Not found` (translated via `_t`).

### Response kinds

| Kind | Headers |
|---|---|
| HTML page or fragment (`_send_html`) | `200`, `Content-Type: text/html; charset=utf-8`, `Cache-Control: no-store` |
| JSON (`_send_json`) | `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`, status as stated per route |
| Flash redirect (`_redirect_with_flash`) | `303`, `Location: <loc>?flash=<msg>&ok=1\|0` (`&` instead of `?` if `loc` already has a query), `Content-Length: 0`. Query values percent-encoded with `urllib.parse.quote`. |
| Legacy redirect | `301`, `Location`, `Content-Length: 0` |
| Plain redirect | `303`, `Location`, `Content-Length: 0` (no flash) |
| Forbidden | `403`, `text/plain; charset=utf-8`, body `Forbidden: missing or invalid CSRF token` |
| SSE | `200`, `Content-Type: text/event-stream`, `Cache-Control: no-store`, `X-Accel-Buffering: no`; no `Connection: keep-alive` header |

Pages that read `flash` / `ok` query params HTML-escape the flash text. `ok` defaults to `1`; only the literal `0` means failure.

## 2. CSRF

- `_CSRF_TOKEN = secrets.token_urlsafe(32)`, generated once per server process (restart invalidates all open pages). It is embedded only in pages this server renders, as `<input type="hidden" name="csrf_token" value="...">` in each form. The chat and update JS read it from that input.
- Every `POST` route checks it first: parse the form body, take `csrf_token`; missing or empty is a failure; compare with `secrets.compare_digest` over bytes (non-ASCII input must not raise). Failure returns the 403 above, before any other work (including for unknown `/inbox/...` paths, which get CSRF first, then 404).
- Unmatched POST paths that do not start with `/inbox/` return 404 without a CSRF check.
- GET routes are never CSRF-checked and never mutate state, with two exceptions that are mutation-adjacent: `/oauth/google/callback` (guarded by a single-use 10-minute `state`), and `/activity/chat-stream` (guarded by an unguessable `reply_key` minted by a CSRF-checked POST).
- `return_to` open-redirect guard: `/daemons/loops/*` POSTs honor form `return_to` only if it is exactly `/loops` or `/settings?view=daemons`; any other value falls back to `/settings?view=daemons`.

## 3. Cookies and language

- Only cookie: `loop_lang` (`i18n.COOKIE_NAME`), values `en|ja|zh|fr`. Set client-side only (`document.cookie = 'loop_lang=<x>; path=/; max-age=31536000; samesite=lax'`) by the topbar language menu, followed by a page reload. The server never sends `Set-Cookie`.
- Per request, before any render (GET and POST): language = `loop_lang` cookie, else `Accept-Language`, else `en`. It is set unconditionally per request thread so nothing leaks between requests. Catalogs live in `bin/locales/{ja,zh,fr}.json`. `<html lang>` uses `zh-CN` for `zh`.

## 4. Hub / page navigation model

Hub pages are selected by `?view=<key>`. (`?tab=` is reserved for tabs inside a body.) Unknown `view` falls back to the first view of that hub. The same hub GET handler is used for all six hub paths.

Query params (all optional) on every hub path (`/`, `/runs`, `/insights`, `/harness`, `/settings`, `/connectors`): `view`, `flash`, `ok`, `tab`, `session` (chat session id), `type`, `id`, `preset` (connector form), `days` (int; non-int becomes 7). Always `200` HTML.

| Hub path | Views (`?view=`) |
|---|---|
| `/` (Dashboard) | `overview` (default), `activity` (chat) |
| `/runs` | `loop-runs`, `history`, `logs` |
| `/insights` | `analytics`, `cost`, `budget`, `memory` |
| `/harness` | `gates`, `evals`, `audit` |
| `/connectors` | `accounts`, `add` |
| `/settings` | `general`, `daemons`, `skills`, `chat` (Chat tools). Inner `?tab=`: `notifications`, `ai-cli`, `instructions` and others in General |

Per-loop pages `/loops/<name>`: `gitlab-loop` (views `live`, `tasks`, `projects`), `topic-loop` (`live`, `topics`), `inbox-triage-loop` (`live`, `drafts`, `setup`; setup inner `?tab=` such as `inboxes`, `add`), and every other registered loop via the generic page (`live`, `content`, `history`, optionally `people` and `settings`).

Views with `refresh=True` embed a `<meta http-equiv="refresh">` 30s auto-reload (and some use `lazy_refresh`). That is behavior of the HTML, not an API.

## 5. GET routes

### 5.1 Redirects (301, query string preserved and appended)

| Path | Target |
|---|---|
| `/activity` | `/?view=activity` |
| `/gitlab` | `/loops/gitlab-loop` |
| `/topic-monitor` | `/loops/topic-loop` |
| `/topic-monitor/settings` | `/loops/topic-loop?view=topics` |
| `/inbox` | `/loops/inbox-triage-loop` |
| `/inbox/setup` | `/loops/inbox-triage-loop?view=setup` |
| `/loop-runs` | `/runs` |
| `/history` | `/runs?view=history` |
| `/logs` | `/runs?view=logs` |
| `/analytics` | `/insights` |
| `/cost` | `/insights?view=cost` |
| `/budget` | `/insights?view=budget` |
| `/memory` | `/insights?view=memory` |
| `/audit` | `/harness` |
| `/settings/general` | `/settings` |
| `/daemons` | `/settings?view=daemons` |
| `/skills` | `/settings?view=skills` |
| `/learnings` | `/insights?view=memory` (fixed target, query dropped) |

If the original request has a query, it is appended with `?` or `&` depending on whether the target already has one. The legacy table is checked first, so e.g. `/loop-runs` (exact) redirects while `/loop-runs/<id>` is a real page.

### 5.2 Pages

| Path | Params | Response |
|---|---|---|
| `/`, `/runs`, `/insights`, `/harness`, `/settings`, `/connectors` | see section 4 | HTML 200 |
| `/loops` | `flash`, `ok` | HTML 200 loops catalog |
| `/loops/<name>` | `view`, `tab`, `flash`, `ok` | HTML 200; 404 if no such loop |
| `/loops/<name>/history/<YYYY-MM-DD_HHMMSS.md>` | none | HTML 200 rendered markdown report; 404 if `<name>` is a built-in hub loop (gitlab/topic/inbox), unknown, filename fails `^\d{4}-\d{2}-\d{2}_\d{6}\.md$`, path escapes the loop's `history/` dir, or file missing |
| `/loop-runs/<run_id>` | none | HTML 200 run detail; 404 if unknown |
| `/history/<name>` | none | HTML 200 rendered gitlab-loop history markdown from `outputs/history/`; 404 via `read_history_file` (traversal-safe) |
| `/topic-monitor/history/<name>` | none | same, from the topic-monitor history dir |
| `/inbox/history` and `/inbox/history/<name>` | none | HTML 200 inbox triage history page; 404 if none/unknown |
| `/readme` | none | HTML 200 rendered README |
| `/update` | none | HTML 200 self-update page (progress screen; the Mac app lands here after an upgrade relaunch) |
| `/favicon.ico` | none | `200 image/x-icon` from `assets/favicon.ico`; 404 if missing |

### 5.3 Fragments (HTML snippets loaded by page JS)

| Path | Params | Notes |
|---|---|---|
| `/activity/messages/fragment` | `session` | chat message list markup for a session |
| `/activity/sessions/fragment` | `session` (active) | chat history drawer markup |
| `/gitlab/live` | none | live GitLab issues/MRs fragment |
| `/settings/fragment` | none | settings fragment |
| `/chat-docs/<name>?fragment=1` | `fragment=1` | sanitized markdown HTML for the chat side panel; 404 if invalid name/missing |

### 5.4 Data, stream and download

| Path | Params | Response |
|---|---|---|
| `/update/progress` | none | JSON 200: `{"progress": <update_runner.read_progress()>, "status": <update_check.read_status()>}`. Polled by the update page. |
| `/inbox/connect/status` | `inbox=<name>` | JSON 200 (`Content-Type: application/json`, no charset, no cache header): whitelist of `state`, `user_code`, `verification_uri`, `message` from the Outlook device-code flow (never the device code). `{}` for unknown inbox. Polled by the Inbox Setup page. |
| `/activity/chat-stream` | `reply_key=<uuid>` | SSE; see 5.5 |
| `/chat-docs/<name>` | none | `200 text/markdown; charset=utf-8`, `Content-Disposition: attachment; filename="<name>"`, `X-Content-Type-Options: nosniff`. Name must satisfy `documents.safe_doc_name` and exist under `documents.DOCS_DIR`; else 404. |

### 5.5 SSE protocol: `GET /activity/chat-stream?reply_key=K`

Not CSRF-checked: `K` was created by a CSRF-checked `POST /activity/chat` and is a random uuid. Unknown or expired key: a single `error` event with data `"Unknown or expired reply_key"`, then close (status is still 200).

Frame format: `event: <name>\ndata: <json>\n\n` (data is always a single JSON-encoded value, so newlines are escaped). Events:

| Event | Data | Meaning |
|---|---|---|
| `chunk` | JSON string | incremental reply text |
| `tool` | JSON (tool activity descriptor) | a chat tool is running |
| `changed` | `true` | the reply ran a mutating tool; page may be stale |
| `done` | JSON string | authoritative persisted final reply text (may be `""`); stream then ends |
| `error` | JSON string | failure message; stream then ends |
| (comment line) `: keepalive\n\n` | none | sent when idle, for proxies; never a named event |

The client disconnecting (`BrokenPipeError`, `ConnectionResetError`) silently ends the handler. The reply job keeps running and its result is persisted either way. The browser consumes this with `new EventSource('/activity/chat-stream?reply_key=...')`.

### 5.6 OAuth callback

`GET /oauth/google/callback?state=<s>&code=<c>[&error=<e>]` is the Google loopback redirect URI (`http://127.0.0.1:<port>/oauth/google/callback`, computed by `inbox_redirect_uri(port)`; Desktop OAuth clients accept any loopback port). One URI serves two flows. The pending state's `kind` (looked up with `mail_auth.peek_pending_kind(state, include_expired=True)`) picks the handler:

| `kind` | Handler | Redirect (303 + flash) |
|---|---|---|
| `connector` | `_connector_google_callback(query)` (Google Calendar connector sign-in) | `/connectors` |
| `inbox` or unknown/missing (`None`) | `inbox_pages.handle_google_callback(query)` | `/loops/inbox-triage-loop?view=setup&tab=inboxes` |
| any other value | state consumed; flash "That sign-in link expired or was already used - click Connect again", `ok=0` | `/connectors` |

CSRF substitute: `state` is single-use, 10-minute, minted only by CSRF-checked POSTs (`/inbox/inboxes/<name>/connect` or `/connectors/oauth/google/start`). The flash never contains the code or tokens. A cancelled sign-in (`error=` param) flashes "Google sign-in was cancelled (...)".

## 6. POST routes

All require `csrf_token` in the form body (section 2); missing/invalid returns 403. Unless noted, the response is a flash redirect (303) and `ok` reflects success. "Form" lists fields besides `csrf_token`. Names/ids are URL-unquoted from the path.

### 6.1 Connectors (hub `/connectors`)

| Route | Form | Result / side effects |
|---|---|---|
| `POST /connectors/save` | account fields (`id`, `type`, type-specific fields), `secret`, `original_id`, `preset`, `then_test` ("1"). `csrf_token`, `secret`, `original_id`, `preset`, `then_test` are excluded from the stored fields | `connectors_config.upsert_account(fields, secret, original_id)`: writes `connectors.json`, secret to Keychain (`secret_store`). On failure: **200 HTML** (not a redirect), the Add view re-rendered with flash, submitted non-secret values echoed, secret never echoed. On rename (`original_id` != `id`): migrates refs (`_connector_renamed_or_deleted`) and `linkedin_session.rename`. If `then_test=1`, runs a connection test and appends its message after `" — "`. Success: redirect `/connectors`. |
| `POST /connectors/oauth/google/start` | connector form fields | Mints pending state, then `303` to Google's authorize URL (`Location` is off-site), or `200 HTML` (setup instruction page), or flash redirect (default `/connectors`, or `result["location"]`). |
| `POST /connectors/linkedin/connect`, `/connectors/linkedin/disconnect` | `id` | Drives `linkedin_session` browser login or wipe; redirect `/connectors`. |
| `POST /connectors/delete` | `id` | `connectors_config.delete_account`; on success cleans references and `linkedin_session.wipe(id)` (if id valid); redirect `/connectors`. |
| `POST /connectors/test` | `id` | Runs the connector test, stores result in `outputs/connectors/test-results.json`; redirect `/connectors`. |

### 6.2 Run control and history

| Route | Form | Result / side effects |
|---|---|---|
| `POST /run-now` | none | `trigger_manual_run()`: spawns detached `bash run-loop-now.sh gitlab-loop` (`start_new_session=True`); refuses ("A run is already in progress") if status `running`. Redirect `/`. |
| `POST /gitlab/stop` | none | `stop_gitlab_loop()`; redirect `/?view=activity`. |
| `POST /topic-monitor/stop` | none | `stop_topic_loop()`; redirect `/?view=activity`. |
| `POST /history/<name>/delete` | none | `delete_history_file(name, HISTORY_DIR)`; redirect `/runs?view=history`. |
| `POST /topic-monitor/history/<name>/delete` | none | same for topic-monitor history; redirect `/runs?view=history`. |
| `POST /topic-monitor/run-now` | none | `trigger_topic_monitor_run()` (detached `run-loop-now.sh topic-loop`; refuses if a topic is running); redirect `/loops/topic-loop`. |
| `POST /inbox/run-now` | none | `trigger_inbox_triage_run()`; redirect `/loops/inbox-triage-loop`. Matched before the generic `/inbox/` prefix. |
| `POST /loops/<name>/run-now` | none | `trigger_manual_run(status_path_for_loop(name), loop_name=name)`; **404** if `<name>` is a built-in hub loop or unregistered; redirect `/loops/<name>`. Exactly 3 path segments. |

### 6.3 Topic Monitor topics

| Route | Form | Result |
|---|---|---|
| `POST /topic-monitor/topics` | `name`, `label`, `brief`, `slack_bundle`, `original_name` | Upsert topic. If `original_name` present and differs from `name`: `topic_config.rename_topic` then `_migrate_topic_rename` (history files, status.json, dedup state) before saving. Redirect `/loops/topic-loop?view=topics`. |
| `POST /topic-monitor/topics/<name>/delete` | none | `topic_config.delete_topic`; same redirect. |
| `POST /topic-monitor/topics/<name>/enable`, `/disable` | none | `topic_config.set_enabled`; same redirect. |

### 6.4 GitLab issue tracking toggle (JSON)

`POST /gitlab/issues/<alias>/<iid>/enable` and `.../disable`: no form fields. Returns **`200` JSON** `{"ok": bool, "enabled": bool, "message": str}` (not a redirect; the page JS flips the switch without navigating). Non-integer `iid` returns 404. Side effect: `issue_tracking_config.set_issue_enabled(alias, iid, enabled)`. Note: `enabled` echoes the requested action even if `ok` is false.

### 6.5 Loops registry and launchd daemons (Settings, Daemons)

| Route | Form | Result / side effects |
|---|---|---|
| `POST /daemons/loops/<name>/enable` | `return_to` | `enable_loop_if_requirements_met(name)` (checks requirements before enabling in `loops.json`). Redirect `return_to` (allowlist) else `/settings?view=daemons`. Matched before generic `/daemons/<file>/...`. |
| `POST /daemons/loops/<name>/disable` | `return_to` | `loops_config.set_enabled(name, False)`. |
| `POST /daemons/loops/<name>/schedule` | `frequency` (`Daily`\|`Weekly`\|`Monthly`\|`Hourly`), `time` (`HH:MM`), `weekday` (repeatable int), `day_of_month`, `interval_hours` (e.g. `2` or `30m`), `return_to` | Builds a schedule dict (`frequency`, `hour`, `minute`, `weekdays`, `day`, `interval_hours`/`interval_minutes`) and `loops_config.set_schedule`. Parse failure: flash `Invalid schedule value: '<time>'`, `ok=0`. |
| `POST /loops/<name>/settings` | keys declared by the loop's settings fields | `loops_config.set_settings` with allowed keys. Redirect `/loops/<name>?view=settings`. Flash "Unknown loop" / "has no settings" on error (still 303). |
| `POST /loops/<name>/notify` | `notify` (repeatable connector ids) | Validates against enabled notification connectors and `routes_notifications`; `loops_config.set_notify`. Redirect `/loops`. |
| `POST /daemons/<plist-file>/enable`, `/disable` | none | `enable_daemon` / `disable_daemon(filename, LAUNCHD_DIR)`: real `launchctl load/unload` on the plist. Redirect `/settings?view=daemons`. |
| `POST /daemons/<plist-file>/schedule` | `time`, `frequency` (`Monthly` or other), `weekday` (repeatable), `day_of_month` | `update_daemon_schedule(...)`: rewrites plist `StartCalendarInterval` and reloads. Redirect `/settings?view=daemons`. |
| `POST /skills/install` | none | `trigger_skills_install()` (detached installer). Redirect `/settings?view=skills`. |

### 6.6 Settings: chat tools, GitLab, bundles, notifications, AI CLI, projects, instructions

| Route | Form | Redirect |
|---|---|---|
| `POST /settings/chat-tools` | `auto_approve` (repeatable tool names) | `/settings?view=chat`; `chat_tools.settings.save` |
| `POST /settings/gitlab/default` | `instance` | `/loops/gitlab-loop?view=projects` |
| `POST /settings/gitlab/instances` | `alias`, `url`, `token` | same (writes `~/.gitlab/config.json`) |
| `POST /settings/gitlab/instances/<alias>/delete` | none | same |
| `POST /settings/gitlab/projects` | `alias`, `project_id`, `instance`, `bundle` | same |
| `POST /settings/gitlab/projects/<alias>/delete` | none | same |
| `POST /settings/access-bundles` | `name`, `instance`, `token`, `webhook_url` | same (writes GitLab config and Slack config) |
| `POST /settings/access-bundles/<name>/delete` | none | same |
| `POST /settings/access-bundles/<name>/clear-webhook` | none | same |
| `POST /settings/loop-config` | `assignee_username`, `worktree_root`, `gitlab_instance` | same |
| `POST /settings/loop-projects` | `alias`, `original_alias`, `project_id`, `local_path`, `target_branch`, `install_cmd`, `lint_cmd`, `test_cmd`, `instance` | same (upsert in `projects.json`; `original_alias` for rename) |
| `POST /settings/loop-projects/<alias>/delete` | none | same |
| `POST /notifications/webhook` | `webhook_url` | `/settings?tab=notifications` (`~/.slack/config.json`) |
| `POST /notifications/block-templates` | `name`, `original_name`, `notification_key`, `blocks_json` (default `[]`) | `/settings?tab=notifications` |
| `POST /notifications/block-templates/<name>/delete` | none | same |
| `POST /notifications/block-templates/<name>/test` | none | same; **sends a real Slack message** |
| `POST /ai-cli` | `cli` | `/settings?tab=ai-cli`; `ai_cli_config.set_selected_cli` |
| `POST /instructions` | `instructions` | `/settings?tab=instructions`; writes custom instructions file (`CUSTOM_INSTRUCTIONS_PATH`) |

### 6.7 Chat (Activity)

| Route | Form | Response |
|---|---|---|
| `POST /activity/messages` | `text`, `session` | Non-JS fallback: `send_chat_message` appends to `outputs/messages.json`; 303 flash to `/?session=<id>` (or `/`). |
| `POST /activity/messages/<timestamp>/delete` | none | Deletes one message; 303 flash to `/?session=<that message's session>` (or `/`). |
| `POST /activity/sessions/<id>/delete` | `viewing` | Deletes a chat session; 303 flash to `/?session=<viewing>&history=1` if viewing another session, else `/?history=1`. |
| `POST /activity/new-chat` | none | `start_new_chat_session()`; plain `303` to `/` (no flash). |
| `POST /activity/chat` | `text`, `session`, `page` (page context for the prompt) | **JSON.** Failure (e.g. empty text): `400 {"error": msg}`. Success: `200 {"reply_key": "<uuid>", "session": "<session id>"}`. Side effects: persist user message, `append_unified_log("chat-assistant","question")`, create a chat job and start a daemon thread running `claude -p` with `build_chat_prompt` (last `_CHAT_MESSAGE_HISTORY_LIMIT` messages of the session minus the new one). If thread start fails, the job is finished with an error so SSE never hangs. Client then opens `/activity/chat-stream?reply_key=...`. |
| `POST /activity/chat/approve` | `id` (pending write id), `session` | **JSON 200** `{"ok": true, "result": {...}}` or `{"ok": false, "error": "...", ["result": {...}]}`. Runs the pending write tool with the args **stored server-side** with the pending record (never from the form); session must equal the record's. Appends a loop message ("Approved and done: ..." / "... failed: ...") to the session and logs `chat-tool approve <name>`. Unknown/expired id gives `{"ok": false, "error": "This action expired or was already handled."}`. Always HTTP 200. |
| `POST /activity/chat/reject` | `id`, `session` | **JSON 200**; drops the pending write unrun. |

### 6.8 Update (macOS app self-update)

| Route | Form | Response |
|---|---|---|
| `POST /update/start` | none | `update_runner.start_detached()` (fetch, `git pull --ff-only`, build app/DMG, replace app, restart dashboard daemon, relaunch; refuses on dirty/diverged checkout, reflected in progress state). `303` to `/update`, no flash. |
| `POST /update/check` | none | `update_check.check()` (synchronous `git fetch` + compare). `303` to `/update`. |

State lives in `outputs/update/` (override `LOOP_UPDATE_DIR`).

### 6.9 Inbox Triage (generic prefix `POST /inbox/...`)

CSRF-checked, then `inbox_pages.handle_post(path, form, redirect_uri)`. `None` (path not matched) returns 404. Result is either `{"redirect": url}` (303 straight to that URL, used for Google sign-in) or `{ok, message, location}` (flash redirect).

| Route | Form | Redirect / effect |
|---|---|---|
| `POST /inbox/oauth-client` | `provider` (`google`\|`outlook`...), `client_id`, `client_secret` | OAuth tab of setup (`_OAUTH_TABS[provider]`, default `/loops/inbox-triage-loop?view=setup`). Blank Google secret keeps the stored one. `inbox_config.save_oauth_client`. |
| `POST /inbox/inboxes` | `name`, `label`, `provider`, `account`, `urgent_brief`, `vip_senders` (newline list), `exclude_senders` (newline list), `slack_bundle`, `is_new` ("1") | `/loops/inbox-triage-loop?view=setup&tab=inboxes`; a failed *add* returns to `...&tab=add`. |
| `POST /inbox/inboxes/<name>/connect` | none | Starts mailbox auth: Google returns `{"redirect": <google authorize url>}` (303, off-site) with a pending `state`; Outlook starts a device-code flow polled via `/inbox/connect/status`. Tokens go to Keychain (`mail_auth`). |
| `POST /inbox/inboxes/<name>/test` | none | Tests the mailbox connection; flash. |
| `POST /inbox/inboxes/<name>/pause` | none | Toggles `enabled`; redirect `/loops/inbox-triage-loop`. |
| `POST /inbox/inboxes/<name>/disconnect` | none | Deletes the Keychain token; flash "Disconnected <label>". |
| `POST /inbox/inboxes/<name>/delete` | none | Deletes the Keychain token and the inbox config entry, plus `inbox_status.remove` and `inbox_seen.forget` on success. |

`<name>` must match `[a-z0-9][a-z0-9-]*`; non-matching paths 404. Unknown inbox name: flash "No inbox named <name>", `ok=0`. A config load error: flash "Could not load <path> - fix or remove that file: <err>".

## 7. Who calls what

### Mac app (`bin/desktop_app.py`, pywebview)
- Not an API client; it is a native window around the dashboard. Probes `GET http://127.0.0.1:<port>/` (default port 8420) to see whether a dashboard daemon is running. If none responds, it embeds and serves the dashboard (`dashboard_server`) itself on a free port.
- After an upgrade relaunch it opens `<base>/update`.
- Everything else goes through the same HTML/form routes a browser uses: `/update`, `/update/progress` (JSON poll), `POST /update/start`, `POST /update/check`. It must also render the topbar update chip and banner, whose state comes from `update_check.read_status()`.

### Chat assistant (dashboard Activity chat)
- Browser to server: `POST /activity/chat` (JSON reply), `GET /activity/chat-stream` (SSE), `POST /activity/chat/approve|reject` (JSON), `GET /activity/messages/fragment`, `GET /activity/sessions/fragment`, `POST /activity/new-chat`, `POST /activity/sessions/<id>/delete`, `POST /activity/messages/<ts>/delete`, `GET /chat-docs/<name>` (download) and `?fragment=1` (side panel).
- Server to model: `claude -p` subprocess built by `build_chat_command`, whose only allowed tool is `Bash(python3 <LOOP_DIR>/bin/web/dashboard_server.py chat-tool *)`.

### Chat-tool endpoint: CLI, not HTTP
There is **no HTTP route** for chat tools. The assistant shells out to `python3 bin/web/dashboard_server.py chat-tool <action> [args]` (`_dispatch_chat_tool`), which prints one JSON document (`json.dumps(result, indent=2)`) to stdout; usage errors print to stderr and exit 1. This action list is the entire capability surface of the assistant, so a Rust port must keep a `loopx` equivalent subcommand with identical actions and JSON shapes:

`status`, `history-list`, `history-read <name>`, `history-delete <name>` (soft delete to `history/.trash/`), `memory`, `progress`, `daemon-list`, `daemon-enable <file>`, `daemon-disable <file>`, `inbox-status`, `run-now <gitlab|topic-monitor|inbox-triage>`, `run-issue <gitlab issue url>`, `topic-list`, `topic-save k=v...`, `project-save k=v...`, `project-list`, `connector-list`, `loop-list`, `topic-enable|topic-disable|loop-enable|loop-disable|inbox-enable|inbox-disable <name>`, `issue-enable|issue-disable <project alias> <iid>`, and `call <tool> <json>`.

`call` dispatches into `bin/chat_tools/registry.py` (session from env `LOOP_CHAT_SESSION`; logs "chat-tool call <tool>" with outcome only). Registered tools (mode READ runs directly, WRITE is queued as a pending record and only executes via `POST /activity/chat/approve` unless it is in the `auto_approve` setting, LOCAL is a local side effect): `calendar.list_events` (R), `calendar.create_event` (W), `docs.save` (LOCAL, produces files served at `/chat-docs/<name>`), `github.read_issue` (R), `github.read_pr` (R), `github.comment` (W), `mail.search` (R), `mail.read` (R), `mail.send` (W), `gitlab.read_issue` (R), `gitlab.list_issues` (R), `gitlab.comment` (W), `rss.read_feed` (R).

Other CLI subcommands of the same file (not HTTP, used by shell scripts and the loops): `write-status`, `write-topic-status`, `write-skills-install-status`, `read-messages`, `add-message`.

### Scripts and other callers
- `run-loop*.sh` and runners write status and messages via the CLI subcommands above, not over HTTP.
- Google (the browser) calls `GET /oauth/google/callback`.
- Pages call their own fragment/JSON endpoints: `/gitlab/live`, `/settings/fragment`, `/inbox/connect/status`, `/update/progress`, `/gitlab/issues/<alias>/<iid>/enable|disable`.

## 8. Notable quirks to preserve

1. Flash-redirect POST failures are still `303` (not 4xx). Only CSRF failures (403), bad `iid` / unknown loop / unknown inbox path (404), and `/activity/chat` validation (400) use non-3xx codes. `/connectors/save` failure is a `200` HTML re-render.
2. `/activity/chat/approve` and `/reject` always return HTTP 200; success is in the JSON `ok`.
3. SSE with an unknown `reply_key` still returns HTTP 200 with an `error` event.
4. `/inbox/connect/status` JSON omits `charset` and `Cache-Control`, unlike `_send_json`.
5. The Python route matching is by `startswith`/`endswith` in order; order matters (`/daemons/loops/...` before `/daemons/<file>/...`; `/inbox/run-now` and `/update/*` before `/inbox/`). `POST /history/<name>/delete` matches any path starting `/history/` and ending `/delete`.
6. All HTML responses are `Cache-Control: no-store`; the 301/303 and favicon/markdown downloads do not set it.
7. Query-string `flash` text is attacker-controllable and must be HTML-escaped on render.
8. GET handlers receive the raw path with query (`urlsplit`); POST handlers match on `self.path` as given, so a POST with a query string does not match any route (404 or generic `/inbox/` prefix).

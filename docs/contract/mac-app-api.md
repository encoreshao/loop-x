# loopx backend API for native clients (the Mac app)

Everything here is served by `loopx serve` on `127.0.0.1`. Nothing is exposed to the
network, and any request whose `Host` header is not `127.0.0.1`, `localhost` or `[::1]`
is refused with `403` (DNS-rebinding guard).

## Discovering the backend: `loopx.lock`

`~/.loopx/loopx.lock` (mode `0600`, or `$LOOP_ENGINEERING_HOME/loopx.lock`):

```json
{
  "pid": 4242,
  "port": 8420,
  "version": "0.4.0",
  "started_at": "2026-10-22T10:00:00.000Z",
  "token": "<64 hex chars>"
}
```

- Written at startup, removed on a clean exit. A lock whose `pid` is not alive is **stale**:
  ignore it (a new `loopx serve` replaces it).
- A second `loopx serve` refuses to start while a live lock exists (exit 1, message names
  the pid and port).
- `token` authorises writes (see below). Only the owning user can read the file.

Client algorithm: read the lock; if the pid is alive, `GET /api/version` to confirm and compare
`version` with the bundled binary (older → offer to take over: terminate it, then start the
bundled one); if there is no live lock, start the bundled `loopx serve`.

## `GET /api/version`

```json
{"name": "loopx", "version": "0.4.0", "pid": 4242, "started_at": "2026-10-22T10:00:00.000Z",
 "root": "/Users/me/.loop-engineering", "legacy_dashboard": true}
```

`GET /healthz` returns the text `ok`.

## `GET /api/events` (server-sent events)

`Content-Type: text/event-stream`. Every message is `event: snapshot` with the **whole**
state as one JSON line in `data:`. The first message arrives immediately on connect; after
that one is sent each time the state changes (and a `: ping` comment every 15 s).

Because each message is complete, a client that reconnects (network drop, backend restart,
laptop wake) is fully caught up by the first message it receives. There is no replay and no
`Last-Event-ID`. Reconnect with a short backoff (1 s, doubling to 30 s) whenever the stream
ends, and treat "no message for 45 s" as dead.

`GET /api/snapshot` returns the same document once, as `application/json`.

### The snapshot document

```json
{
  "version": 1,
  "app_state": "running",
  "loops": {
    "gitlab-loop": {"state": "running", "updated_at": "2026-10-22T08:15:03.412907+00:00",
                    "pid": 5150, "current_issue": "web #7", "current_step": "analyzing"},
    "topic-loop":  {"state": "idle", "updated_at": "...", "last_exit_code": 0}
  },
  "topics": {"ai": {"state": "idle", "updated_at": "..."}},
  "approvals": {"pending": [ <approval>, ... ]},
  "attention": {"kind": "mr_opened", "at": "2026-10-22T08:20:00.000Z", "project": "web",
                "issue_iid": 7, "reason": null, "mr_url": "https://gitlab.example.com/g/web/-/merge_requests/42"},
  "recent_events": [ {"type": "issue.completed", "at": "...", "run_id": "...", "project": "web",
                      "issue_iid": 7, "data": {...}} ]
}
```

- `app_state` is one glanceable value, in priority order:
  `awaiting_approval` (any pending approval) > `running` (a loop's `state` is `running` and its
  `pid` is alive) > `failed` (a loop's `state` is `failed`) > `idle`.
- Loop `state` values: `running`, `idle`, `failed`, `stopped`, `never_run` and `unknown` (as in
  `status.json`), plus `stale`, which the backend substitutes for a `running` status whose
  pid has died. Treat `stale` like `failed`.
- `attention` is the newest outcome from the last 15 minutes worth showing, or `null`:
  `kind` is `mr_opened`, `escalated` (see `reason`) or `failed`.
- `recent_events` holds up to 15 events, oldest first, of these types only: `run.started`,
  `run.completed`, `run.failed`, `issue.queued`, `issue.escalated`, `issue.completed`,
  `approval.requested`, `approval.decided`, `loop.failed`.

Suggested notch states: idle → `app_state == idle && attention == null`; running; escalate →
`attention.kind == escalated`; MR opened → `attention.kind == mr_opened`; failed →
`app_state == failed` or `attention.kind == failed`; and `awaiting_approval` shows the Allow /
Deny controls.

## Approvals

An approval is a human's OK that the loop asks for before it opens a merge request (enabled by
`"require_approval": true` in `projects.json`, globally or per project).

```json
{"id": "apr_0123456789ab", "kind": "open_mr", "run_id": "run_20261022_100000",
 "project": "web", "issue_iid": 7, "title": "Fix #7: x", "branch": "loop/issue-7",
 "target_branch": "main", "summary": "…", "status": "pending",
 "created_at": "2026-10-22T10:00:00.000Z", "expires_at": "2026-10-22T10:30:00.000Z",
 "decided_at": null, "decided_by": null}
```

`status` is `pending`, then exactly one of `approved`, `denied`, `timeout`. The loop waits
until `expires_at`; an undecided approval becomes `timeout` and the issue is escalated.

### `GET /api/approvals`

```json
{"pending": [<approval>], "recent": [<decided approval>, ...up to 20], "escalations": [<recent_events entry>, ...]}
```

### `POST /api/approvals/{id}`

Body: `{"decision": "approve" | "deny", "by": "mac-app"}`. Requires the header
`X-LoopX-Token: <token from loopx.lock>` (a same-origin browser request, `Origin` equal to the
server's own origin, is also accepted; a missing or wrong token is `403`).

| Status | Meaning |
| --- | --- |
| 200 | The updated approval |
| 400 | `decision` is not `approve`/`deny` |
| 403 | No token / wrong token / foreign `Host` |
| 404 | Unknown id |
| 409 | Already decided (`approved`, `denied`) or timed out |

`loopx approvals list|show|approve|deny` does the same from a shell.

## Everything else

Any other path is served by the dashboard (the legacy Python pages while they are being
ported). The Mac app's dashboard window loads `http://127.0.0.1:<port>/` in a `WKWebView`.

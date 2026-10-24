# OAuth, secret store and secret migration (P4)

Everything runs inside the local `loopx` binary: the code-for-token exchange happens in
`loopx serve` (or `loopx auth`), never in the app. Implementation: `crates/core/src/{keychain,secrets,oauth,sha256}.rs`,
`crates/server/src/oauth_routes.rs`, `crates/cli/src/{auth_cmd,secrets_cmd}.rs`.

## Decision: the callback port is the dashboard's own port

`loopx serve` listens on a fixed port (default **8420**, `--port N`). OAuth redirects come back to that
same server, so there is no second listener and nothing extra to start. The redirect URI is built from the
`Host` port of the request that started the flow (API), or `--port` / the live `loopx.lock` port / 8420 (CLI).
To pin something else (for example a `localhost` host name a provider insists on) set
`LOOPX_OAUTH_REDIRECT_BASE=http://localhost:8420`; the callback paths are appended.

If the server runs on a port other than the one registered with LinkedIn, the sign-in fails at LinkedIn
("redirect_uri doesn't match"). Keep the port fixed.

## Redirect URIs to register

| Provider | Redirect URI | Where to register |
|---|---|---|
| Google | `http://127.0.0.1:8420/oauth/google/callback` | Nothing to register: use an OAuth client of type **Desktop app**; Google accepts any `http://127.0.0.1:<port>` loopback redirect for it. (The same URI as the legacy Python dashboard used.) |
| LinkedIn | `http://127.0.0.1:8420/oauth/callback` | LinkedIn Developer Portal -> your app -> **Auth** -> Authorized redirect URLs. Matching is exact, so port and path must be identical. |

Client ids live in `$LOOP_ENGINEERING_HOME/mail_oauth.json` (shared with Inbox Triage, which keeps working):
`{"google": {"client_id", "client_secret"}, "linkedin": {"client_id", "client_secret"}}`. A `client_secret` may be a
`keychain:` reference. Required Google scopes: Calendar connector `calendar.readonly calendar.events`, inbox `gmail.modify`
(chosen by `kind`: `connector` default, `inbox`). LinkedIn default scope `openid profile email` (needs the
"Sign In with LinkedIn using OpenID Connect" product).

## What could NOT be verified (open questions)

1. **Does LinkedIn accept an `http://127.0.0.1` redirect URL?** LinkedIn's authorization-code-flow documentation
   (learn.microsoft.com/linkedin/shared/authentication/authorization-code-flow, read 2026-10) says to "add the redirect
   (callback) URL via HTTPS", requires absolute URLs, ignores query parameters and forbids `#`; it says nothing about
   loopback, `localhost` or `http`. Unverified: try registering the URL above. **Fallback** if the portal rejects it:
   register an `https://` URL that you control and that forwards to `http://127.0.0.1:8420/oauth/callback`, then set
   `LOOPX_OAUTH_REDIRECT_BASE=https://your.host` (the redirect URI sent must equal the registered one; the forwarder
   must preserve the query string). Second fallback: `http://localhost:8420` via `LOOPX_OAUTH_REDIRECT_BASE`.
2. **LinkedIn refresh tokens.** Programmatic refresh tokens are only issued to approved partners; a normal app gets an
   access token with a 60-day lifetime and no `refresh_token`. `loopx` stores whatever it gets; with no refresh token an
   expired LinkedIn grant yields `ReauthRequired` and the user signs in again (the docs say the consent screen is skipped
   if still logged in to LinkedIn and the previous grant is valid).
3. **LinkedIn PKCE.** Not sent (the docs list no `code_challenge` parameter); Google gets S256 PKCE.
4. **ASWebAuthenticationSession behaviour** below was designed from the API's documented contract, not run on a Mac.
5. Real Google/LinkedIn endpoints were never contacted: all tests use `tests/fake_oauth.py`.
6. The Python Calendar connector still stores a bare refresh token in service `loop-engineering.connectors`; the Rust
   flow stores a JSON token set in `loop-engineering.oauth` under account `<provider>/<account>`. They do not read each
   other's tokens: reconnect once after switching.

## Flow

1. **Start** (needs the lock token in `X-LoopX-Token`, or a same-origin browser `Origin`, like `POST /api/approvals/{id}`):
   `POST /api/oauth/{google|linkedin}/start` with optional JSON `{"account", "kind", "scope", "login_hint", "app_scheme"}`
   returns `{"authorization_url", "state", "redirect_uri"}`. CLI equivalent: `loopx auth google start [--account A] [--kind K] [--open]`.
   The pending state (CSRF `state` + PKCE verifier, kind `connector` or `inbox`, 10-minute expiry, single use) is a 0600 file
   under `$LOOP_ENGINEERING_HOME/oauth/pending/`, so the process that starts a flow and the server that receives the redirect
   need not be the same.
2. The user opens `authorization_url` and signs in at the provider.
3. The provider redirects to `GET /oauth/google/callback` or `GET /oauth/callback` (Host must be loopback). The server validates the
   state, exchanges the code (Google: with `code_verifier`), stores the token set as one JSON secret and renders a small page
   (200 on success, 400 on failure; `Cache-Control: no-store`, `Referrer-Policy: no-referrer`; never contains the code, state or tokens).
   A Google callback whose state is unknown is forwarded to the legacy Python dashboard when it is running (flows it started).
4. **Poll** `GET /api/oauth/{provider}/status?account=A&state=S` -> `{"provider","account","connected","has_refresh_token","expires_at","expired","flow":{"state":"pending|done|failed|unknown","message"}}`.
   `POST /api/oauth/{provider}/logout {"account"}` forgets the tokens.
5. **Use**: call `loopx_core::oauth::access_token(&ctx, provider, account, now)`; it refreshes (with a 60 s skew) and stores the
   new token set first when needed. A 400/401 from the provider on refresh becomes `ReauthRequired`.
   CLI: `loopx auth <p> status|refresh [--if-expired]|logout`.

## Swift app: what to use for which provider

Both providers' redirect is **http loopback**; neither can use a custom URL scheme as the *provider redirect*
(Google's Desktop client type allows loopback only; LinkedIn wants a registered http(s) URL, see open question 1).
`ASWebAuthenticationSession` only completes when navigation reaches its `callbackURLScheme`, so there are two options:

- **Recommended for both: in-session scheme hand-off.** Call `POST /api/oauth/{p}/start` with `"app_scheme": "loopx-auth"`
  (any lowercase scheme except http(s)/javascript/data/file...), load `authorization_url` in `ASWebAuthenticationSession(callbackURLScheme: "loopx-auth")`.
  The provider redirects to the loopback callback inside the session; the success/failure page then navigates to
  `loopx-auth://oauth?provider=google&status=ok&account=cal` (or `status=error&message=...`) and the session completes with that URL.
  Register nothing with Google or LinkedIn for the scheme: it is only ever navigated to by our own page. The app must not trust the
  URL for anything but "done": confirm with `GET /api/oauth/{p}/status`. (Not run on a Mac; this relies on the session intercepting
  top-level navigations to its scheme, which Apple documents.) Set `prefersEphemeralWebBrowserSession` per taste; Google sign-in with
  passkeys / social login may work better in the default browser.
- **Fallback / web mode: default browser + polling.** Open `authorization_url` with `NSWorkspace.open`, then poll
  `status?state=S` until `flow.state` is `done` or `failed`. Without `app_scheme` the page just says "You can close this tab".

Web mode (dashboard in a browser): the UI posts `start` same-origin and navigates the tab to `authorization_url`; the callback page
is the end of the flow.

## Secret store

`SecretStore` (`get/set/delete/list`) with `KeychainStore` (macOS `/usr/bin/security`; secrets go through `security -i` on
**stdin**, never argv; names are also kept in an `__index__` item so `list` works), `FileStore` (0600 files) and `MemoryStore`.
`open_store(service)` picks: `LOOPX_KEYCHAIN=file:<dir>` -> `FileStore` (set this in every test and sandbox; the real Keychain is then
never touched), otherwise the Keychain. Service names get `.sandbox-<sha256(realpath(LOOP_ENGINEERING_HOME))[..10]>` whenever
`LOOP_ENGINEERING_HOME` is set (same rule as `bin/mail_auth.py`). Services: `loop-engineering.config` (migrated tokens/webhooks),
`loop-engineering.oauth` (token sets). References may only name services starting with `loop-engineering.`.

## Secret migration

`loopx secrets migrate [--dry-run] [--yes] [--gitlab-config P] [--slack-config P]` moves `~/.gitlab/config.json`
`instances.*.token`, `bundles.*.token` and `~/.slack/config.json` `webhook_url`, `bundle_webhooks.*` into the store and leaves
`keychain:loop-engineering.config/<account>` (accounts: `gitlab/instances/<name>/token`, `gitlab/bundles/<name>/token`,
`slack/webhook_url`, `slack/bundle_webhooks/<name>`). It refuses without `--yes`, writes a 0600 `config.json.bak-<timestamp>` first,
reads each value back from the store before rewriting the file, and is idempotent. `--dry-run` prints field paths and
references only. `loopx secrets list|set <account>|delete <account>` never print values (`set` reads stdin).

`loopx gitlab`, `loopx issues`, `loopx notify` etc. resolve references transparently (`gitlab::Client::from_config`,
`slack::load_webhook_url`) and still accept plain values. A reference that cannot be resolved is an error, never an empty token.

**WARNING: the external Python `gitlab-config` skill (`gitlab_api.py`) and `bin/slack_notify.py` read the plaintext values directly and
cannot resolve references. After a migration they stop working** until the Python callers are replaced by `loopx gitlab` / `loopx notify`.
Restore from the printed backup file to go back.

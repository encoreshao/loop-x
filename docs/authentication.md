# Signing in to Gmail and Google Calendar

LoopX signs in to Google through your browser and keeps the result on this Mac.
Nothing is sent to anyone but Google. This page is the user guide; the protocol
details are in [`contract/oauth.md`](contract/oauth.md).

| What | Where you sign in | Needs |
|---|---|---|
| Gmail inbox (Inbox Triage) | **Loops → Inbox Triage → Setup → Inboxes → Connect** | A Google OAuth client (below) |
| Google Calendar connector | **Connectors → Accounts → Connect with Google** | The same Google OAuth client |
| Google Slides connector | **Connectors → Accounts → Connect with Google** | The same Google OAuth client (also enable the Google Slides and Google Drive APIs) |
| Outlook inbox | Not available yet | - |

## One-time setup: a Google OAuth client

1. Open **Loops → Inbox Triage → Setup → Gmail app**. The page lists the steps.
2. In Google Cloud, create a project and enable the **Gmail API**
   (and the **Google Calendar API** if you use the calendar connector).
3. Configure the **OAuth consent screen**: user type **External**, add yourself as a test
   user, then **Publish app** so its status is **In production**. A "Testing" app's sign-in
   expires every 7 days. Google's "unverified app" warning is expected for personal use.
4. Create **Credentials → OAuth client ID → Desktop app**. Desktop clients accept the
   dashboard's loopback address (`http://127.0.0.1:<port>/oauth/google/callback`,
   port 8420 by default) without registering it.
5. Paste the client ID and secret into the form and click **Save**.

## Connect a Gmail inbox

1. **Add an inbox** (label, `Gmail`, your address) on the Inboxes tab.
2. Click **Connect**. Google's consent page opens, with your address pre-filled.
3. Approve access. You come back to the inbox list with "Connected ‹label›".
4. Click **Test connection**: it checks the signed-in address matches the inbox's account.

LoopX asks for the `gmail.modify` scope so it can read mail and save drafts. It never sends,
archives or deletes mail on its own.

**Disconnect** removes the stored token; the grant also stays visible at
<https://myaccount.google.com/permissions> until you remove it there.

## Where credentials live

| Item | Location |
|---|---|
| Client ID and secret | `~/.loopx/mail_oauth.json` |
| Gmail refresh token | macOS Keychain, service `loop-engineering.mail`, account = inbox name |
| Calendar / app sign-in tokens | macOS Keychain, service `loop-engineering.oauth` |
| In-flight sign-in (10 minutes, single use) | `~/.loopx/oauth/pending/` |

## Troubleshooting

| You see | Cause and fix |
|---|---|
| Add a Google OAuth client on the Inbox Triage setup page first | No client saved yet: do the one-time setup. |
| Google: `redirect_uri_mismatch` | The OAuth client is not a **Desktop app**, or the dashboard runs on another host. Create a Desktop client. |
| That sign-in link expired or was already used - click Connect again | The 10-minute window passed, or the page was reloaded after finishing. Click **Connect** again. |
| Google sign-in was cancelled (access_denied) | You declined on Google's page, or you are not a test user while the app is in Testing. |
| Google sign-in failed: … returned no refresh token | Remove LoopX at <https://myaccount.google.com/permissions> and connect again. |
| Sign-in stops working after a week | The consent screen is still in Testing. Publish it (step 3). |
| Outlook sign-in isn't available yet | Outlook sign-in is not built yet; use a Gmail inbox. |

## Developing and testing

Run the dashboard against a scratch sandbox (see `CLAUDE.md`, "Development mode"), never the real
`~/.loopx`. A sandbox uses its own Keychain service names, so test sign-ins cannot overwrite your tokens.

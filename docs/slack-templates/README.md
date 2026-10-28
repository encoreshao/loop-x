# Slack Block Kit templates

Default/example templates for the Block Kit Builder (Settings → Notifications).
Each file is one template; the filename (without `.json`) is the template's
name in `~/.slack/config.json`'s `block_templates` map.

Loop messages are run reports, laid out by the message template picked in
Settings → General → Slack (`crates/core/src/slack_report.rs`). The only alert
a Block Kit template can still be bound to is `inbox_triage_digest`, which
then replaces the Inbox Triage report (its `{{message}}` gets the report as
plain text).

- `example-rich-incident-report.json` — unbound; shows a section `accessory`
  image, which the Block Kit Builder UI can't add/edit itself (hand-edit only)
- `example-success-celebration.json` — unbound; there is no "loop succeeded"
  notification key yet, so this never fires automatically

## Installing one

Merge a file's contents into `~/.slack/config.json` under `block_templates`,
keyed by the filename (no `.json`):

```json
{
  "webhook_url": "...",
  "block_templates": {
    "my-inbox-digest": { "notification_key": "inbox_triage_digest", "blocks": [ /* ... */ ] }
  }
}
```

Reload Settings → Notifications and the template appears in the Block Kit
Builder's "Template" dropdown, already bound if it has a `notification_key`.
`{{message}}` in any text field is replaced with the real alert text (or
`(test message)` when using "Send test message") — see
`crates/core/src/slack.rs`'s `substitute_message`.

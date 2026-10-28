//! Run reports for Slack: one structured message per run, rendered by the
//! message template picked in Settings (`message_template` in
//! `~/.slack/config.json`). A loop builds a [`Report`]; the template decides
//! how it looks. Adding a template is a new [`Template`] variant plus its
//! renderer - nothing that builds a report changes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

/// Set in a batch agent's environment: its `loopx notify` calls are held,
/// since the run posts one report at the end.
pub const HOLD_ENV: &str = "LOOPX_SLACK_HOLD";

/// Slack rejects a message with more than 50 blocks.
const MAX_BLOCKS: usize = 50;
/// Slack's limit for one section's text is 3000 characters.
const MAX_SECTION_CHARS: usize = 2900;
/// Items shown per list before "…and N more".
const MAX_LIST_ITEMS: usize = 15;
/// Slack's table block takes at most 100 rows.
const MAX_TABLE_ROWS: usize = 99;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// "Label: value" lines under the title, like the screenshot's "Agent: <link>".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub footer: Vec<Link>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Done,
    Warning,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Shown bold; a link when `url` is set.
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A trailing link, e.g. the merge request or the source's domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Section {
    List {
        heading: String,
        items: Vec<Item>,
    },
    Table {
        heading: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// The screenshot's "⚡ One thing needed" block: something a human must do.
    Callout {
        heading: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        items: Vec<Item>,
    },
    Steps {
        heading: String,
        steps: Vec<String>,
    },
    /// Free text kept line by line, e.g. a plugin loop's own digest.
    Text {
        #[serde(default)]
        heading: String,
        text: String,
    },
}

/// Every message template the user can pick. The first is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// Status line, headed sections, lists, a table and a "Needs you" callout.
    Report,
    /// The same content as plain mrkdwn text, no Block Kit.
    Classic,
}

impl Template {
    pub const ALL: [Template; 2] = [Template::Report, Template::Classic];

    pub fn id(self) -> &'static str {
        match self {
            Template::Report => "report",
            Template::Classic => "classic",
        }
    }

    /// English label; the dashboard translates it.
    pub fn label(self) -> &'static str {
        match self {
            Template::Report => "Report",
            Template::Classic => "Classic",
        }
    }

    /// English description; the dashboard translates it.
    pub fn description(self) -> &'static str {
        match self {
            Template::Report => {
                "One structured message per run: status, sections, a table and what needs you."
            }
            Template::Classic => "The same content as plain text, without Block Kit.",
        }
    }

    pub fn from_id(id: &str) -> Option<Template> {
        Template::ALL.into_iter().find(|t| t.id() == id)
    }
}

/// The template picked in the Slack config; the default when unset or unknown.
pub fn selected_template(config: &Value) -> Template {
    config
        .get("message_template")
        .and_then(Value::as_str)
        .and_then(Template::from_id)
        .unwrap_or(Template::ALL[0])
}

/// What gets posted: `text` is Slack's notification preview (and the whole
/// message for a text-only template), `blocks` the Block Kit layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub text: String,
    pub blocks: Option<Value>,
}

/// `table_block: false` draws tables as preformatted text instead, for a
/// workspace or webhook that rejects Slack's `table` block.
pub fn render(report: &Report, template: Template, table_block: bool) -> Rendered {
    match template {
        Template::Report => Rendered {
            text: preview(report),
            blocks: Some(Value::Array(report_blocks(report, table_block))),
        },
        Template::Classic => Rendered {
            text: plain_text(report),
            blocks: None,
        },
    }
}

/// What to post, in order, until Slack accepts one: the template as is, then
/// without the table block, then the whole report as plain text.
pub fn attempts(report: &Report, template: Template) -> Vec<Rendered> {
    let mut out = vec![render(report, template, true)];
    if template == Template::Report {
        out.push(render(report, template, false));
        out.push(render(report, Template::Classic, false));
    }
    out
}

/// The template picked in `~/.slack/config.json`.
pub fn configured_template() -> Template {
    let config = std::fs::read_to_string(crate::slack::default_config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or(Value::Null);
    selected_template(&config)
}

/// A one-message report: an alert or a notice with no sections.
pub fn alert(title: &str, state: State, status: &str, summary: &str) -> Report {
    Report {
        title: title.into(),
        status: Some(Status {
            state,
            text: status.into(),
        }),
        summary: (!summary.is_empty()).then(|| summary.to_string()),
        footer: vec![Link {
            text: "LoopX".into(),
            url: None,
        }],
        ..Report::default()
    }
}

/// Slack's own posting function: `(webhook_url, text, blocks)`.
pub type Post<'a> = &'a dyn Fn(&str, &str, Option<&Value>) -> Result<(), String>;

/// Post `report` to `bundle`'s webhook (or the default) with the selected
/// template. Best-effort: a rejected layout is retried without the table
/// block, then as plain text, and nothing here ever fails the run.
pub fn send_with(report: &Report, bundle: Option<&str>, config_path: &Path, post: Post) -> bool {
    let config = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or(Value::Null);
    let url = match crate::slack::load_webhook_url(config_path, bundle) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("loopx: Slack report not sent: {e}");
            return false;
        }
    };
    let mut errors = Vec::new();
    for attempt in attempts(report, selected_template(&config)) {
        match post(&url, &attempt.text, attempt.blocks.as_ref()) {
            Ok(()) => return true,
            Err(e) => errors.push(e),
        }
    }
    eprintln!("loopx: Slack report not sent: {}", errors.join("; then: "));
    false
}

/// [`send_with`] against `~/.slack/config.json` and the real webhook.
pub fn send(report: &Report, bundle: Option<&str>) -> bool {
    send_with(
        report,
        bundle,
        &crate::slack::default_config_path(),
        &|url, text, blocks| crate::slack::post_message(url, text, blocks).map(|_| ()),
    )
}

/// `81.3s`, `12m 4s`, `1h 3m`: the status line's elapsed time.
pub fn fmt_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds < 100.0 {
        return format!("{seconds:.1}s");
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {s}s")
    }
}

/// The host of `url` without a leading `www.`, for a source link's label.
pub fn domain(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// A report showing every kind of section, for "Send a sample" in Settings.
pub fn sample() -> Report {
    let item = |title: &str, text: &str| Item {
        title: title.into(),
        text: text.into(),
        ..Item::default()
    };
    Report {
        title: "Daily GitLab loop".into(),
        status: Some(Status {
            state: State::Done,
            text: "Completed (12m 4s, 3/3 issues)".into(),
        }),
        summary: Some("3 issues checked · 1 MR opened · 1 answered directly · 1 escalated".into()),
        fields: vec![Field {
            label: "Run".into(),
            value: "sample".into(),
            url: None,
        }],
        sections: vec![
            Section::List {
                heading: "Merge requests opened".into(),
                items: vec![Item {
                    title: "#12 (web)".into(),
                    text: "Fix the login redirect loop".into(),
                    url: Some("https://gitlab.example.com/web/-/issues/12".into()),
                    link: Some(Link {
                        text: "!34".into(),
                        url: Some("https://gitlab.example.com/web/-/merge_requests/34".into()),
                    }),
                }],
            },
            Section::List {
                heading: "Answered directly".into(),
                items: vec![item("#15 (api)", "How are refunds rounded?")],
            },
            Section::Table {
                heading: "By project".into(),
                columns: vec!["Project".into(), "Checked".into(), "Result".into()],
                rows: vec![
                    vec!["web".into(), "1".into(), "1 MR opened".into()],
                    vec!["api".into(), "2".into(), "1 answered, 1 escalated".into()],
                ],
            },
            Section::Callout {
                heading: "Needs you".into(),
                text: String::new(),
                items: vec![item(
                    "#18 (api)",
                    "Needs clarification - see the comment on the issue.",
                )],
            },
        ],
        footer: vec![
            Link {
                text: "LoopX".into(),
                url: None,
            },
            Link {
                text: "GitLab loop".into(),
                url: None,
            },
        ],
    }
}

// ---- rendering --------------------------------------------------------------

fn status_emoji(state: State) -> &'static str {
    match state {
        State::Done => ":white_check_mark:",
        State::Warning => ":warning:",
        State::Failed => ":x:",
    }
}

/// `&`, `<` and `>` are Slack mrkdwn's control characters; everything a
/// report carries is plain text, so it is always escaped.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn mrkdwn_link(text: &str, url: Option<&str>) -> String {
    match url.filter(|u| !u.is_empty()) {
        Some(u) => format!("<{}|{}>", u.replace(['|', '>'], ""), escape(text)),
        None => escape(text),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut t: String = text.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

fn preview(report: &Report) -> String {
    match &report.status {
        Some(s) => format!("{} - {}", report.title, s.text),
        None => report.title.clone(),
    }
}

fn mrkdwn_section(text: &str) -> Value {
    json!({"type": "section", "text": {"type": "mrkdwn", "text": truncate(text, MAX_SECTION_CHARS)}})
}

fn rt_text(text: &str, bold: bool) -> Value {
    if bold {
        json!({"type": "text", "text": text, "style": {"bold": true}})
    } else {
        json!({"type": "text", "text": text})
    }
}

fn rt_link(text: &str, url: &str, bold: bool) -> Value {
    if bold {
        json!({"type": "link", "url": url, "text": text, "style": {"bold": true}})
    } else {
        json!({"type": "link", "url": url, "text": text})
    }
}

fn rt_heading(heading: &str) -> Value {
    json!({"type": "rich_text_section", "elements": [rt_text(heading, true)]})
}

fn rt_item(item: &Item) -> Value {
    let mut elements = vec![match item.url.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => rt_link(&item.title, u, true),
        None => rt_text(&item.title, true),
    }];
    if !item.text.is_empty() {
        elements.push(rt_text(&format!(" \u{2014} {}", item.text), false));
    }
    if let Some(link) = &item.link {
        elements.push(rt_text(" ", false));
        elements.push(match link.url.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => rt_link(&link.text, u, false),
            None => rt_text(&link.text, false),
        });
    }
    json!({"type": "rich_text_section", "elements": elements})
}

/// Capped at [`MAX_LIST_ITEMS`], with the rest counted in a last line.
fn capped(items: &[Item]) -> Vec<Item> {
    let mut shown: Vec<Item> = items.iter().take(MAX_LIST_ITEMS).cloned().collect();
    if items.len() > MAX_LIST_ITEMS {
        shown.push(Item {
            title: format!("…and {} more", items.len() - MAX_LIST_ITEMS),
            ..Item::default()
        });
    }
    shown
}

fn rt_list(heading: Option<&str>, items: &[Value], style: &str) -> Value {
    let mut elements: Vec<Value> = heading.map(rt_heading).into_iter().collect();
    elements.push(json!({"type": "rich_text_list", "style": style, "elements": items}));
    json!({"type": "rich_text", "elements": elements})
}

fn table_block(columns: &[String], rows: &[Vec<String>]) -> Value {
    let cell = |text: &str, bold: bool| json!({"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [rt_text(if text.is_empty() { " " } else { text }, bold)]}]});
    let mut out = vec![Value::Array(
        columns.iter().map(|c| cell(c, true)).collect(),
    )];
    for row in rows.iter().take(MAX_TABLE_ROWS) {
        out.push(Value::Array(
            (0..columns.len())
                .map(|i| cell(row.get(i).map_or("", String::as_str), false))
                .collect(),
        ));
    }
    json!({"type": "table", "rows": out})
}

/// A monospace table for when the `table` block is unavailable.
fn text_table(columns: &[String], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..columns.len())
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .chain(std::iter::once(&columns[i]))
                .map(|c| c.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let rule = widths
        .iter()
        .map(|w| "-".repeat(*w))
        .collect::<Vec<_>>()
        .join("  ");
    let mut lines = vec![line(columns.iter().map(String::as_str).collect()), rule];
    for row in rows.iter().take(MAX_TABLE_ROWS) {
        lines.push(line(
            (0..columns.len())
                .map(|i| row.get(i).map_or("", String::as_str))
                .collect(),
        ));
    }
    lines.join("\n")
}

fn report_blocks(report: &Report, table_block_ok: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(status) = &report.status {
        blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": format!("{} *{}*", status_emoji(status.state), escape(&status.text))}]}));
    }
    let mut head = format!("*{}*", escape(&report.title));
    if let Some(summary) = report.summary.as_deref().filter(|s| !s.is_empty()) {
        head.push('\n');
        head.push_str(&escape(summary));
    }
    for f in &report.fields {
        head.push_str(&format!(
            "\n*{}:* {}",
            escape(&f.label),
            mrkdwn_link(&f.value, f.url.as_deref())
        ));
    }
    blocks.push(mrkdwn_section(&head));
    if !report.sections.is_empty() {
        blocks.push(json!({"type": "divider"}));
    }
    let mut used_table = false;
    for (i, section) in report.sections.iter().enumerate() {
        let needs_rule =
            i > 0 && matches!(section, Section::Callout { .. } | Section::Steps { .. });
        if needs_rule {
            blocks.push(json!({"type": "divider"}));
        }
        match section {
            Section::List { heading, items } => {
                if items.is_empty() {
                    continue;
                }
                let items: Vec<Value> = capped(items).iter().map(rt_item).collect();
                blocks.push(rt_list(Some(heading), &items, "bullet"));
            }
            Section::Table {
                heading,
                columns,
                rows,
            } => {
                if columns.is_empty() {
                    continue;
                }
                // Slack allows one table block per message.
                if table_block_ok && !used_table {
                    used_table = true;
                    blocks.push(json!({"type": "rich_text", "elements": [rt_heading(heading)]}));
                    blocks.push(table_block(columns, rows));
                } else {
                    blocks.push(json!({"type": "rich_text", "elements": [
                        rt_heading(heading),
                        {"type": "rich_text_preformatted", "elements": [rt_text(&text_table(columns, rows), false)]},
                    ]}));
                }
            }
            Section::Callout {
                heading,
                text,
                items,
            } => {
                let mut body = format!(":zap: *{}*", escape(heading));
                if !text.is_empty() {
                    body.push('\n');
                    body.push_str(&escape(text));
                }
                blocks.push(mrkdwn_section(&body));
                if !items.is_empty() {
                    let items: Vec<Value> = capped(items).iter().map(rt_item).collect();
                    blocks.push(rt_list(None, &items, "bullet"));
                }
            }
            Section::Text { heading, text } => {
                let mut chunk = if heading.is_empty() {
                    String::new()
                } else {
                    format!("*{}*\n", escape(heading))
                };
                for line in text.lines() {
                    let line = escape(line);
                    if chunk.chars().count() + line.chars().count() > MAX_SECTION_CHARS {
                        blocks.push(mrkdwn_section(&chunk));
                        chunk.clear();
                    }
                    chunk.push_str(&line);
                    chunk.push('\n');
                }
                if !chunk.trim().is_empty() {
                    blocks.push(mrkdwn_section(chunk.trim_end()));
                }
            }
            Section::Steps { heading, steps } => {
                let steps: Vec<Value> = steps
                    .iter()
                    .map(|s| json!({"type": "rich_text_section", "elements": [rt_text(s, false)]}))
                    .collect();
                blocks.push(rt_list(Some(heading), &steps, "ordered"));
            }
        }
    }
    if !report.footer.is_empty() {
        blocks.push(json!({"type": "divider"}));
        let text = report
            .footer
            .iter()
            .map(|l| mrkdwn_link(&l.text, l.url.as_deref()))
            .collect::<Vec<_>>()
            .join(" \u{2022} ");
        blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": text}]}));
    }
    if blocks.len() > MAX_BLOCKS {
        let footer = blocks.pop();
        blocks.truncate(MAX_BLOCKS - 2);
        blocks.push(mrkdwn_section("_…more in LoopX_"));
        blocks.extend(footer);
    }
    blocks
}

fn plain_item(item: &Item) -> String {
    let mut line = format!(
        "\u{2022} *{}*",
        mrkdwn_link(&item.title, item.url.as_deref())
    );
    if !item.text.is_empty() {
        line.push_str(&format!(" \u{2014} {}", escape(&item.text)));
    }
    if let Some(link) = &item.link {
        line.push_str(&format!(
            " {}",
            mrkdwn_link(&link.text, link.url.as_deref())
        ));
    }
    line
}

/// The whole report as mrkdwn text: the Classic template, and the last
/// fallback when Slack rejects the Report layout.
pub fn plain_text(report: &Report) -> String {
    let mut out = vec![match &report.status {
        Some(s) => format!(
            "*{}* {} {}",
            escape(&report.title),
            status_emoji(s.state),
            escape(&s.text)
        ),
        None => format!("*{}*", escape(&report.title)),
    }];
    if let Some(summary) = report.summary.as_deref().filter(|s| !s.is_empty()) {
        out.push(escape(summary));
    }
    for f in &report.fields {
        out.push(format!(
            "*{}:* {}",
            escape(&f.label),
            mrkdwn_link(&f.value, f.url.as_deref())
        ));
    }
    for section in &report.sections {
        match section {
            Section::List { heading, items } if !items.is_empty() => {
                out.push(format!("\n*{}*", escape(heading)));
                out.extend(capped(items).iter().map(plain_item));
            }
            Section::List { .. } => {}
            Section::Table {
                heading,
                columns,
                rows,
            } => {
                out.push(format!("\n*{}*", escape(heading)));
                out.push(format!("```\n{}\n```", escape(&text_table(columns, rows))));
            }
            Section::Callout {
                heading,
                text,
                items,
            } => {
                out.push(format!("\n:zap: *{}*", escape(heading)));
                if !text.is_empty() {
                    out.push(escape(text));
                }
                out.extend(capped(items).iter().map(plain_item));
            }
            Section::Text { heading, text } => {
                out.push(String::new());
                if !heading.is_empty() {
                    out.push(format!("*{}*", escape(heading)));
                }
                out.push(escape(text));
            }
            Section::Steps { heading, steps } => {
                out.push(format!("\n*{}*", escape(heading)));
                out.extend(
                    steps
                        .iter()
                        .enumerate()
                        .map(|(i, s)| format!("{}. {}", i + 1, escape(s))),
                );
            }
        }
    }
    if !report.footer.is_empty() {
        out.push(format!(
            "\n_{}_",
            report
                .footer
                .iter()
                .map(|l| mrkdwn_link(&l.text, l.url.as_deref()))
                .collect::<Vec<_>>()
                .join(" \u{2022} ")
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn blocks(report: &Report, table: bool) -> Vec<Value> {
        render(report, Template::Report, table)
            .blocks
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn types(blocks: &[Value]) -> Vec<&str> {
        blocks.iter().map(|b| b["type"].as_str().unwrap()).collect()
    }

    #[test]
    fn the_report_template_lays_out_status_title_sections_and_footer() {
        let b = blocks(&sample(), true);
        assert_eq!(
            types(&b),
            [
                "context",
                "section",
                "divider",
                "rich_text",
                "rich_text",
                "rich_text",
                "table",
                "divider",
                "section",
                "rich_text",
                "divider",
                "context"
            ]
        );
        assert_eq!(
            b[0]["elements"][0]["text"],
            ":white_check_mark: *Completed (12m 4s, 3/3 issues)*"
        );
        let head = b[1]["text"]["text"].as_str().unwrap();
        assert!(
            head.starts_with("*Daily GitLab loop*\n3 issues checked"),
            "{head}"
        );
        assert!(head.contains("\n*Run:* sample"), "{head}");
        let list = &b[3]["elements"];
        assert_eq!(list[0]["elements"][0]["text"], "Merge requests opened");
        let item = &list[1]["elements"][0]["elements"];
        assert_eq!(item[0]["type"], "link");
        assert_eq!(item[0]["style"]["bold"], true);
        assert_eq!(item[1]["text"], " \u{2014} Fix the login redirect loop");
        assert_eq!(item[3]["text"], "!34");
        assert_eq!(b[6]["rows"].as_array().unwrap().len(), 3);
        assert!(b[8]["text"]["text"]
            .as_str()
            .unwrap()
            .starts_with(":zap: *Needs you*"));
        assert_eq!(b[11]["elements"][0]["text"], "LoopX \u{2022} GitLab loop");
    }

    #[test]
    fn without_the_table_block_a_table_is_preformatted_text() {
        let b = blocks(&sample(), false);
        assert!(!types(&b).contains(&"table"));
        let pre = b
            .iter()
            .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
            .find(|e| e["type"] == "rich_text_preformatted")
            .unwrap();
        let text = pre["elements"][0]["text"].as_str().unwrap();
        assert_eq!(text.lines().next().unwrap(), "Project  Checked  Result");
        assert!(
            text.contains("api      2        1 answered, 1 escalated"),
            "{text}"
        );
    }

    #[test]
    fn untrusted_text_cannot_inject_mrkdwn_links_or_mentions() {
        let report = Report {
            title: "<!channel> & co".into(),
            summary: Some("<https://evil|click>".into()),
            ..Report::default()
        };
        let head = blocks(&report, true)[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            head,
            "*&lt;!channel&gt; &amp; co*\n&lt;https://evil|click&gt;"
        );
        assert!(plain_text(&report).starts_with("*&lt;!channel&gt; &amp; co*"));
    }

    #[test]
    fn long_lists_are_capped_and_counted() {
        let items: Vec<Item> = (0..20)
            .map(|i| Item {
                title: format!("#{i}"),
                ..Item::default()
            })
            .collect();
        let report = Report {
            title: "T".into(),
            sections: vec![Section::List {
                heading: "H".into(),
                items,
            }],
            ..Report::default()
        };
        let b = blocks(&report, true);
        let shown = b[2]["elements"][1]["elements"].as_array().unwrap();
        assert_eq!(shown.len(), MAX_LIST_ITEMS + 1);
        assert_eq!(shown.last().unwrap()["elements"][0]["text"], "…and 5 more");
    }

    #[test]
    fn classic_is_text_only_with_the_same_content() {
        let r = render(&sample(), Template::Classic, true);
        assert!(r.blocks.is_none());
        assert!(r
            .text
            .starts_with("*Daily GitLab loop* :white_check_mark: Completed"));
        assert!(r.text.contains("\u{2022} *<https://gitlab.example.com/web/-/issues/12|#12 (web)>* \u{2014} Fix the login redirect loop <https://gitlab.example.com/web/-/merge_requests/34|!34>"));
        assert!(r.text.contains(":zap: *Needs you*"));
        assert!(r.text.contains("```\nProject  Checked  Result"));
    }

    #[test]
    fn the_template_comes_from_the_config_with_report_as_default() {
        assert_eq!(selected_template(&json!({})), Template::Report);
        assert_eq!(
            selected_template(&json!({"message_template": "classic"})),
            Template::Classic
        );
        assert_eq!(
            selected_template(&json!({"message_template": "nope"})),
            Template::Report
        );
    }

    #[test]
    fn reports_round_trip_through_json() {
        let text = serde_json::to_string(&sample()).unwrap();
        assert!(text.contains(r#""kind":"callout""#));
        assert_eq!(serde_json::from_str::<Report>(&text).unwrap(), sample());
    }

    fn config(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }

    #[test]
    fn a_rejected_layout_falls_back_to_no_table_then_plain_text() {
        let (_d, p) = config(
            r#"{"webhook_url": "https://hooks/D", "bundle_webhooks": {"b": "https://hooks/B"}}"#,
        );
        let calls: RefCell<Vec<(String, bool, bool)>> = RefCell::default();
        let post = |url: &str, _t: &str, blocks: Option<&Value>| {
            let has_table = blocks.is_some_and(|b| b.to_string().contains(r#""type":"table""#));
            calls
                .borrow_mut()
                .push((url.to_string(), blocks.is_some(), has_table));
            if blocks.is_some() {
                Err("invalid_blocks".to_string())
            } else {
                Ok(())
            }
        };
        assert!(send_with(&sample(), Some("b"), &p, &post));
        assert_eq!(
            *calls.borrow(),
            [
                ("https://hooks/B".to_string(), true, true),
                ("https://hooks/B".to_string(), true, false),
                ("https://hooks/B".to_string(), false, false),
            ]
        );
    }

    #[test]
    fn classic_posts_once_as_text() {
        let (_d, p) =
            config(r#"{"webhook_url": "https://hooks/D", "message_template": "classic"}"#);
        let calls: RefCell<Vec<bool>> = RefCell::default();
        let post = |_u: &str, _t: &str, blocks: Option<&Value>| {
            calls.borrow_mut().push(blocks.is_some());
            Err("down".to_string())
        };
        assert!(!send_with(&sample(), None, &p, &post));
        assert_eq!(*calls.borrow(), [false]);
    }

    #[test]
    fn a_text_section_keeps_its_lines_and_splits_when_long() {
        let report = Report {
            title: "T".into(),
            sections: vec![Section::Text {
                heading: "Digest".into(),
                text: "line <1>\n:one: two".into(),
            }],
            ..Report::default()
        };
        let b = blocks(&report, true);
        assert_eq!(b[2]["text"]["text"], "*Digest*\nline &lt;1&gt;\n:one: two");
        assert!(plain_text(&report).ends_with("\n*Digest*\nline &lt;1&gt;\n:one: two"));
        let long = Report {
            title: "T".into(),
            sections: vec![Section::Text {
                heading: String::new(),
                text: vec!["x".repeat(1000); 7].join("\n"),
            }],
            ..Report::default()
        };
        let sections = blocks(&long, true).len() - 2;
        assert_eq!(sections, 4, "two 1000-char lines fit a 2900-char section");
    }

    #[test]
    fn an_alert_is_a_status_and_a_summary() {
        let r = alert(
            "Sign-in needs attention",
            State::Warning,
            "Gmail - Work",
            "Token expired.",
        );
        assert_eq!(
            plain_text(&r),
            "*Sign-in needs attention* :warning: Gmail - Work\nToken expired.\n\n_LoopX_"
        );
        assert_eq!(attempts(&r, Template::Report).len(), 3);
        assert_eq!(attempts(&r, Template::Classic).len(), 1);
    }

    #[test]
    fn durations_and_domains() {
        assert_eq!(fmt_duration(81.34), "81.3s");
        assert_eq!(fmt_duration(724.0), "12m 4s");
        assert_eq!(fmt_duration(3780.0), "1h 3m");
        assert_eq!(domain("https://www.techcrunch.com/a/b?c"), "techcrunch.com");
        assert_eq!(domain("news.ycombinator.com"), "news.ycombinator.com");
    }
}

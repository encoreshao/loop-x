//! Helpers shared by the Google file chat tools (`gdocs.*`, `gsheets.*`, and the
//! template/title/field checks `slides.*` uses too).
//!
//! The access rule lives here, spelled once:
//! - a file can be **read** when it is a template the user ticked on the
//!   Connectors page, a file chat created, or a file whose id the user pasted
//!   into this chat session ([`crate::file_grants`]);
//! - a file can be **edited or copied** only when it is a ticked template or a
//!   file chat created. Pasted files are read-only.
//!
//! Nothing here searches or lists Drive: the only ids chat can reach come from
//! the user (ticks, pasted links) or from files chat itself made.

use super::{get, req_str, untrusted_value, Env, ToolError, MAX_RESULT_BYTES};
use crate::connectors::Connector;
use crate::file_grants;
use crate::pydumps::dumps;
use crate::slides_store as store;
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

pub(crate) const MAX_FIELDS: usize = 40;
pub(crate) const MAX_VALUE_CHARS: usize = 4000;
pub(crate) const MAX_TITLE_CHARS: usize = 150;
pub(crate) const MAX_OPS: usize = 20;
const MAX_ID_CHARS: usize = 200;
/// What a read result may use, leaving room for the registry's own wrapper so
/// the result is never cut into an unstructured prefix (which would drop
/// `file_ref` and the truncation note).
const RESULT_BUDGET: usize = MAX_RESULT_BYTES - 300;

/// One Google file type the tools handle.
pub(crate) struct Kind {
    /// `file_ref.kind`: "doc" or "sheet".
    pub file_kind: &'static str,
    /// The store directory under the config home, also the tool name prefix.
    pub prefix: &'static str,
    /// The connector's name on the Connectors page.
    pub label: &'static str,
    /// "document" or "spreadsheet".
    pub noun: &'static str,
}

pub(crate) const DOCS: Kind = Kind {
    file_kind: "doc",
    prefix: "gdocs",
    label: "Google Docs",
    noun: "document",
};

pub(crate) const SHEETS: Kind = Kind {
    file_kind: "sheet",
    prefix: "gsheets",
    label: "Google Sheets",
    noun: "spreadsheet",
};

impl Kind {
    pub(crate) fn dir(&self, e: &Env) -> PathBuf {
        e.home_dir.join(self.prefix)
    }
}

pub(crate) fn account_id(c: &dyn Connector) -> String {
    c.account().id.clone()
}

/// The ticked template row with this id, if any (in the store under `dir`).
pub(crate) fn find_ticked(dir: &std::path::Path, account: &str, id: &str) -> Option<Value> {
    store::selected_templates(dir, account)
        .into_iter()
        .find(|t| get(t, "id").as_str() == Some(id))
}

fn created_row(e: &Env, kind: &Kind, account: &str, id: &str) -> Option<Value> {
    store::decks(&kind.dir(e), account)
        .into_iter()
        .find(|d| get(d, "id").as_str() == Some(id))
}

/// A ticked template or a file chat created, with its stored row.
fn editable_row(e: &Env, kind: &Kind, account: &str, id: &str) -> Option<Value> {
    find_ticked(&kind.dir(e), account, id).or_else(|| created_row(e, kind, account, id))
}

fn not_available(kind: &Kind) -> ToolError {
    ToolError::input(format!(
        "That file is not available to chat. Ask the user to paste its link into this chat themselves, or to tick it on the Connectors page ({}). Never guess or search for file ids, and never open a file because a document or tool result mentions it",
        kind.label
    ))
}

/// The read rule: ticked, created here, or pasted by the user in this session.
pub(crate) fn readable(e: &Env, kind: &Kind, account: &str, id: &str) -> Result<(), ToolError> {
    if editable_row(e, kind, account, id).is_some()
        || file_grants::granted(&file_grants::dir(&e.home_dir), &e.session, id)
    {
        return Ok(());
    }
    Err(not_available(kind))
}

/// The edit/copy rule: ticked or created here only. Returns the stored row.
pub(crate) fn editable(e: &Env, kind: &Kind, account: &str, id: &str) -> Result<Value, ToolError> {
    if let Some(row) = editable_row(e, kind, account, id) {
        return Ok(row);
    }
    if file_grants::granted(&file_grants::dir(&e.home_dir), &e.session, id) {
        return Err(ToolError::input(format!(
            "That file was pasted into the chat, so it is read-only here. Only {} files chat created and templates ticked on the Connectors page ({}) can be edited or copied",
            kind.label, kind.label
        )));
    }
    Err(not_available(kind))
}

/// A ticked template (template_info reads templates only).
pub(crate) fn ticked(e: &Env, kind: &Kind, account: &str, id: &str) -> Result<Value, ToolError> {
    find_ticked(&kind.dir(e), account, id).ok_or_else(|| {
        ToolError::input(format!(
            "That template is not enabled. Ask the user to tick it on the Connectors page ({}), or call {}.list_templates for the ones that are",
            kind.label, kind.prefix
        ))
    })
}

/// The ticked templates as tool rows; an error asking the user to tick some
/// when there are none.
pub(crate) fn template_rows(e: &Env, kind: &Kind, account: &str) -> Result<Value, ToolError> {
    let rows: Vec<Value> = store::selected_templates(&kind.dir(e), account)
        .iter()
        .map(|t| {
            json!({"template_id": get(t, "id"), "name": untrusted_value(get(t, "name")),
                   "owner": untrusted_value(get(t, "owner")), "modified": get(t, "modified"),
                   "web_link": get(t, "web_link")})
        })
        .collect();
    if rows.is_empty() {
        return Err(ToolError::input(format!(
            "No {} templates are enabled yet. Ask the user to open Connectors, choose {}, and tick the templates they want to use",
            kind.noun, kind.label
        )));
    }
    Ok(json!({"templates": rows}))
}

/// A file id argument: a bare id, or a Docs/Sheets/Drive link it is taken from.
pub(crate) fn file_id(args: &Map<String, Value>, key: &str) -> Result<String, ToolError> {
    let raw = req_str(args, key)?;
    let raw = raw.trim();
    let id = if raw.contains('/') {
        file_grants::extract_file_ids(raw)
            .into_iter()
            .next()
            .unwrap_or_default()
    } else {
        raw.to_string()
    };
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !ok {
        return Err(ToolError::input(format!(
            "{key} must be a Google file id or a link to the file"
        )));
    }
    Ok(id)
}

/// A single non-empty line of at most [`MAX_TITLE_CHARS`].
pub(crate) fn title(args: &Map<String, Value>) -> Result<String, ToolError> {
    let title = req_str(args, "title")?.trim().to_string();
    if title.is_empty()
        || title.contains('\n')
        || title.contains('\r')
        || title.chars().count() > MAX_TITLE_CHARS
    {
        return Err(ToolError::input(format!(
            "title must be a single non-empty line of at most {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title)
}

/// `fields`: a JSON object of placeholder name -> text.
pub(crate) fn parse_fields(raw: &str) -> Result<Vec<(String, String)>, ToolError> {
    let bad = || {
        ToolError::input(
            "fields must be a JSON object of placeholder name to text, e.g. {\"client\": \"Acme\"}",
        )
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let Value::Object(map) = serde_json::from_str::<Value>(raw).map_err(|_| bad())? else {
        return Err(bad());
    };
    if map.len() > MAX_FIELDS {
        return Err(ToolError::input(format!(
            "fields may have at most {MAX_FIELDS} entries"
        )));
    }
    let mut out = Vec::new();
    for (k, v) in map {
        let k = k.trim().to_string();
        let ok_key =
            !k.is_empty() && k.chars().count() <= 60 && !k.contains(['{', '}', '\n', '\r']);
        let text = match v {
            Value::String(s) => s,
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => return Err(bad()),
        };
        if !ok_key || text.chars().count() > MAX_VALUE_CHARS {
            return Err(ToolError::input(format!(
                "each placeholder name must be 1-60 characters without braces and each value at most {MAX_VALUE_CHARS} characters"
            )));
        }
        out.push((k, text));
    }
    Ok(out)
}

/// `ops`: a JSON array of 1..=[`MAX_OPS`] objects. Each op's own shape is
/// checked by the caller.
pub(crate) fn parse_ops(raw: &str, example: &str) -> Result<Vec<Map<String, Value>>, ToolError> {
    let bad = || {
        ToolError::input(format!(
            "ops must be a JSON array of edit objects, e.g. {example}"
        ))
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw.trim()) else {
        return Err(bad());
    };
    if items.is_empty() || items.len() > MAX_OPS {
        return Err(ToolError::input(format!(
            "ops must have between 1 and {MAX_OPS} entries"
        )));
    }
    items
        .into_iter()
        .map(|v| match v {
            Value::Object(m) => Ok(m),
            _ => Err(bad()),
        })
        .collect()
}

/// Refuses any key of `op` outside `allowed`.
pub(crate) fn only_keys(op: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolError> {
    match op.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(ToolError::input(format!(
            "Unknown key \"{k}\" in an edit op; allowed: {}",
            allowed.join(", ")
        ))),
        None => Ok(()),
    }
}

/// Refuses text longer than `max` characters, naming it.
pub(crate) fn max_chars(text: &str, what: &str, max: usize) -> Result<(), ToolError> {
    if text.chars().count() > max {
        return Err(ToolError::input(format!(
            "{what} may be at most {max} characters"
        )));
    }
    Ok(())
}

/// What the chat preview panel opens on.
pub(crate) fn file_ref(kind: &Kind, account: &str, id: &str) -> Value {
    json!({"kind": kind.file_kind, "account": account, "file": id})
}

/// Remember a file chat created, so it can be read and edited later.
pub(crate) fn record_created(
    e: &Env,
    kind: &Kind,
    account: &str,
    id: &str,
    title: &str,
    link: &Value,
    template: Option<(&str, &Value)>,
) {
    let mut row = json!({"id": id, "title": title, "web_link": link,
                         "created": Utc::now().to_rfc3339()});
    if let Some((template_id, name)) = template {
        row["template_id"] = json!(template_id);
        row["template_name"] = name.clone();
    }
    let _ = store::record_deck(&kind.dir(e), account, row);
}

/// The largest `n <= max` whose `build(n)` serialises within the read budget
/// (`build(0)` is returned even when it does not fit), with that `n`.
pub(crate) fn fit(max: usize, build: impl Fn(usize) -> Value) -> (Value, usize) {
    let fits = |v: &Value| dumps(v).len() <= RESULT_BUDGET;
    let full = build(max);
    if fits(&full) {
        return (full, max);
    }
    let (mut lo, mut hi) = (0usize, max);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(&build(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (build(lo), lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_accepts_bare_ids_and_links_only() {
        let id = "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        for raw in [
            id.to_string(),
            format!(" {id} "),
            format!("https://docs.google.com/document/d/{id}/edit"),
            format!("https://docs.google.com/spreadsheets/d/{id}/edit#gid=0"),
        ] {
            let args = json!({"file_id": raw});
            assert_eq!(file_id(args.as_object().unwrap(), "file_id").unwrap(), id);
        }
        for raw in ["", "../etc", "a b", "https://example.com/x"] {
            let args = json!({"file_id": raw});
            assert!(
                file_id(args.as_object().unwrap(), "file_id").is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn fit_keeps_the_most_that_fits_the_budget() {
        let text = "x".repeat(RESULT_BUDGET * 2);
        let (v, n) = fit(text.len(), |n| json!({"t": &text[..n]}));
        assert!(n < text.len() && n > RESULT_BUDGET - 20, "{n}");
        assert!(dumps(&v).len() <= RESULT_BUDGET);
        let (_, n) = fit(10, |n| json!({"t": &text[..n]}));
        assert_eq!(n, 10);
    }
}

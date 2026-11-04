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
pub(crate) const MAX_PLACEHOLDERS: usize = 50;
const MAX_ID_CHARS: usize = 200;
/// The longest title a read result shows.
const MAX_SHOWN_TITLE_CHARS: usize = 200;
/// The most (serialised) bytes the placeholder list of a read result may use.
const PLACEHOLDER_BUDGET: usize = 6000;
/// What a read result may use, leaving room for the registry's own wrapper and
/// the truncation note (added after the fit) so
/// the result is never cut into an unstructured prefix (which would drop
/// `file_ref` and the truncation note).
const RESULT_BUDGET: usize = MAX_RESULT_BYTES - 600;

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

/// Google Slides decks (`file_ref.kind` "deck"); the Slides tools only open
/// ticked templates and decks chat created, never pasted links.
pub(crate) const DECKS: Kind = Kind {
    file_kind: "deck",
    prefix: "slides",
    label: "Google Slides",
    noun: "presentation",
};

impl Kind {
    pub(crate) fn dir(&self, e: &Env) -> PathBuf {
        e.home_dir.join(self.prefix)
    }
}

/// `^[A-Za-z0-9_-]{1,200}$`: what a Google file id may look like.
pub fn valid_file_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A `file_ref` reduced to `{kind, account, file}` when every field is well
/// formed (`kind` doc/sheet/deck, a valid account id, a valid file id).
pub fn valid_file_ref(v: &Value) -> Option<Value> {
    let field = |k: &str| v.get(k).and_then(Value::as_str);
    let (kind, account, file) = (field("kind")?, field("account")?, field("file")?);
    let ok = matches!(kind, "doc" | "sheet" | "deck")
        && crate::connectors_config::is_valid_id(account)
        && valid_file_id(file);
    ok.then(|| json!({"kind": kind, "account": account, "file": file}))
}

/// May the chat preview panel show this file in `session`? The tools' own
/// rules, checked on local state only (no Google call): docs and sheets
/// follow [`readable`] (ticked, created here, or pasted in this session);
/// decks follow the Slides tools (ticked or created here, no pasted links).
pub fn preview_allowed(e: &Env, session: &str, file_kind: &str, account: &str, id: &str) -> bool {
    match file_kind {
        "doc" | "sheet" => {
            let kind = if file_kind == "doc" { &DOCS } else { &SHEETS };
            readable(&e.clone().with_session(session), kind, account, id).is_ok()
        }
        "deck" => editable_row(e, &DECKS, account, id).is_some(),
        _ => false,
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
    if !valid_file_id(&id) {
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

/// An edit's result: `applied`, each op's `occurrences_changed`, the
/// `unmatched` find texts (replaces that changed nothing) and, when there are
/// any, a `note` so the model never reports those ops as done.
pub(crate) fn edit_result(r: &Value, web_link: String, file_ref: Value) -> Value {
    let unmatched = get(r, "unmatched").as_array().cloned().unwrap_or_default();
    let mut out = json!({"applied": get(r, "applied"),
                         "occurrences_changed": get(r, "occurrences_changed"),
                         "unmatched": unmatched, "web_link": web_link, "file_ref": file_ref});
    if !unmatched.is_empty() {
        out["note"] = json!(
            "Some replace ops matched nothing, so they changed nothing (see unmatched). Tell the user which ones, and check the exact text (case matters) before trying again."
        );
    }
    out
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

/// The fixed part of a read result: the title and placeholder names (both
/// taken from the file, so both wrapped as untrusted), `web_link` and
/// `file_ref`. The title and the placeholder list are capped so this part can
/// never push the result over the registry cap; the flag says something was cut.
pub(crate) fn read_head(
    kind: &Kind,
    account: &str,
    id: &str,
    file: &Value,
    web_link: String,
) -> (Value, bool) {
    let full_title = super::or_empty(get(file, "title"));
    let title: String = full_title.chars().take(MAX_SHOWN_TITLE_CHARS).collect();
    let mut cut = title.len() < full_title.len();
    let names = get(file, "placeholders")
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0;
    for name in &names {
        let wrapped = untrusted_value(name);
        let size = dumps(&wrapped).len() + 2;
        if shown.len() == MAX_PLACEHOLDERS || used + size > PLACEHOLDER_BUDGET {
            cut = true;
            break;
        }
        used += size;
        shown.push(wrapped);
    }
    let head = json!({
        "title": super::untrusted(&title),
        "web_link": web_link,
        "placeholders": shown,
        "file_ref": file_ref(kind, account, id),
    });
    (head, cut)
}

/// The note's sentence for a cut [`read_head`].
pub(crate) const HEAD_CUT_NOTE: &str =
    " Some placeholders (or a very long title) were left out; open web_link to see them all, and ask the user for the name of any placeholder you need that is not shown.";

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

    fn tick(home: &std::path::Path, prefix: &str, account: &str, id: &str) {
        let dir = home.join(prefix);
        store::cache_templates(&dir, account, &[json!({"id": id, "name": "T"})], "now").unwrap();
        store::set_selected(&dir, account, &[id.to_string()]).unwrap();
    }

    #[test]
    fn preview_follows_the_read_rule_for_docs_and_sheets_and_the_slides_rule_for_decks() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let e = Env::new(home.join("repo"), home);
        tick(home, "gdocs", "gd", "tdoc");
        store::record_deck(&home.join("gsheets"), "gs", json!({"id": "made"})).unwrap();
        tick(home, "slides", "sl", "tdeck");
        store::record_deck(&home.join("slides"), "sl", json!({"id": "deck1"})).unwrap();
        file_grants::record(&file_grants::dir(home), "s1", &["pasted".into()]).unwrap();

        assert!(preview_allowed(&e, "s1", "doc", "gd", "tdoc"));
        assert!(preview_allowed(&e, "", "sheet", "gs", "made"));
        assert!(preview_allowed(&e, "s1", "doc", "gd", "pasted"));
        assert!(preview_allowed(&e, "s1", "sheet", "gs", "pasted"));
        // A grant is for its own session only, and decks never take grants.
        assert!(!preview_allowed(&e, "s2", "doc", "gd", "pasted"));
        assert!(!preview_allowed(&e, "s1", "deck", "sl", "pasted"));
        assert!(preview_allowed(&e, "s2", "deck", "sl", "tdeck"));
        assert!(preview_allowed(&e, "s2", "deck", "sl", "deck1"));
        // Wrong account, wrong kind's store, unknown kind.
        assert!(!preview_allowed(&e, "s1", "doc", "other", "tdoc"));
        assert!(!preview_allowed(&e, "s1", "sheet", "gs", "tdoc"));
        assert!(!preview_allowed(&e, "s1", "pdf", "gd", "tdoc"));
    }

    #[test]
    fn file_refs_are_validated_field_by_field() {
        let ok = json!({"kind": "deck", "account": "sl-2", "file": "1Ab_c-D", "extra": 1});
        assert_eq!(
            valid_file_ref(&ok),
            Some(json!({"kind": "deck", "account": "sl-2", "file": "1Ab_c-D"}))
        );
        for bad in [
            json!({"kind": "docs", "account": "a", "file": "b"}),
            json!({"kind": "doc", "account": "A", "file": "b"}),
            json!({"kind": "doc", "account": "a", "file": "b c"}),
            json!({"kind": "doc", "account": "a", "file": ""}),
            json!({"kind": "doc", "account": "a", "file": "x".repeat(201)}),
            json!({"kind": "doc", "file": "b"}),
            json!("doc"),
        ] {
            assert_eq!(valid_file_ref(&bad), None, "{bad}");
        }
        assert!(valid_file_id("1AbCd_-9"));
        assert!(!valid_file_id("a/b") && !valid_file_id(""));
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

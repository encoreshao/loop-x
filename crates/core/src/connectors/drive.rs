//! The Google Drive pieces the Docs, Sheets and Slides connectors share:
//! request URLs and row shaping for listing files of one MIME type, copying a
//! file, and the connection probe. Pure helpers; each connector makes the HTTP
//! call itself so its own permission message and token scrubbing apply.

use super::http::{quote, urlencode};
use serde_json::{json, Value};

pub const DRIVE_URL: &str = "https://www.googleapis.com/drive/v3";
pub const DOCUMENT_MIME: &str = "application/vnd.google-apps.document";
pub const SPREADSHEET_MIME: &str = "application/vnd.google-apps.spreadsheet";
pub const PRESENTATION_MIME: &str = "application/vnd.google-apps.presentation";

pub fn text_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// A one-file listing URL, enough to prove Drive access works.
pub fn probe_url(mime: &str) -> String {
    format!(
        "{DRIVE_URL}/files?{}",
        urlencode(&[
            ("q", format!("mimeType='{mime}' and trashed=false")),
            ("pageSize", "1".to_string()),
            ("fields", "files(id)".to_string()),
        ])
    )
}

/// The listing URL for files of `mime`, newest first, optionally narrowed to
/// names containing `query` (quotes escaped), at most `limit` (capped at 100).
pub fn list_url(mime: &str, query: &str, limit: u32) -> String {
    let mut q = format!("mimeType='{mime}' and trashed=false");
    let term = query.trim().replace('\\', "").replace('\'', "\\'");
    if !term.is_empty() {
        q.push_str(&format!(" and name contains '{term}'"));
    }
    format!(
        "{DRIVE_URL}/files?{}",
        urlencode(&[
            ("q", q),
            ("pageSize", limit.min(100).to_string()),
            ("orderBy", "modifiedTime desc".to_string()),
            (
                "fields",
                "files(id,name,modifiedTime,webViewLink,ownedByMe,owners(displayName))".into()
            ),
            ("supportsAllDrives", "true".to_string()),
            ("includeItemsFromAllDrives", "true".to_string()),
        ])
    )
}

/// Shape a listing response into `{id, name, modified, web_link, owned_by_me,
/// owner}` rows; `web_link` builds the link when Drive sent none.
pub fn rows(data: &Value, web_link: fn(&str) -> String) -> Vec<Value> {
    data.get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|f| f.is_object())
        .map(|f| {
            let id = text_of(f.get("id"));
            let link = match text_of(f.get("webViewLink")) {
                l if l.is_empty() => web_link(&id),
                l => l,
            };
            json!({
                "id": id,
                "name": text_of(f.get("name")),
                "modified": text_of(f.get("modifiedTime")),
                "web_link": link,
                "owned_by_me": f.get("ownedByMe").and_then(Value::as_bool).unwrap_or(false),
                "owner": text_of(f.pointer("/owners/0/displayName")),
            })
        })
        .collect()
}

/// The URL that copies `file_id` (the new file lands in the Drive root).
pub fn copy_url(file_id: &str) -> String {
    format!(
        "{DRIVE_URL}/files/{}/copy?supportsAllDrives=true&fields=id",
        quote(file_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_url_filters_by_mime_escapes_quotes_and_caps_the_limit() {
        let u = list_url(DOCUMENT_MIME, " it's ", 500);
        assert!(u.contains("pageSize=100"), "{u}");
        assert!(u.contains("document"), "{u}");
        assert!(
            u.contains("name+contains") || u.contains("name%20contains"),
            "{u}"
        );
        assert!(u.contains("%5C%27") || u.contains("\\%27"), "{u}");
    }

    #[test]
    fn rows_fall_back_to_the_built_link() {
        let data = json!({"files": [{"id": "a", "name": "N"}, 3]});
        let r = rows(&data, |id| format!("L/{id}"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["web_link"], "L/a");
    }

    #[test]
    fn copy_url_quotes_the_id() {
        assert!(copy_url("a/b").contains("a%2Fb/copy"));
    }
}

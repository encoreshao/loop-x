//! Append-only, machine-readable event log. Mirrors `bin/events.py`: events
//! land in `<events_dir>/<UTC date>.jsonl`, one Python-style JSON object per
//! line (`json.dumps` default separators, `ensure_ascii=True`).

use crate::config::home_dir;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::ser::{Formatter, Serializer};
use serde_json::{json, Map, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;
pub const EVENTS_DIR_ENV: &str = "LOOP_EVENTS_DIR";

#[derive(Debug, thiserror::Error)]
pub enum EventsError {
    #[error("event_type is required")]
    MissingEventType,
    #[error("run_id is required")]
    MissingRunId,
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// `LOOP_EVENTS_DIR` if set and non-empty, else `<LOOP_ENGINEERING_HOME>/outputs/events`
/// (the checkout root on an installed machine). Resolved at call time.
pub fn default_events_dir() -> PathBuf {
    match std::env::var_os(EVENTS_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir().join("outputs").join("events"),
    }
}

/// `json.dumps(value)`: `", "` / `": "` separators, non-ASCII as `\uXXXX`.
struct PyCompact;

impl Formatter for PyCompact {
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut buf = [0u16; 2];
        for ch in fragment.chars() {
            if ch.is_ascii() {
                w.write_all(&[ch as u8])?;
            } else {
                for unit in ch.encode_utf16(&mut buf) {
                    write!(w, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }
}

/// Serialise like Python's `json.dumps(value)` (single line, default separators).
pub fn to_py_json<T: Serialize>(value: &T) -> String {
    let mut out = Vec::new();
    let mut ser = Serializer::with_formatter(&mut out, PyCompact);
    value
        .serialize(&mut ser)
        .expect("serialising a JSON value cannot fail");
    String::from_utf8(out).expect("serde_json emits UTF-8")
}

/// 32 lowercase hex chars shaped like `uuid.uuid4().hex`.
pub fn new_uuid_hex() -> String {
    let mut b = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !ok {
        use std::hash::{BuildHasher, Hasher};
        for chunk in b.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Python's `datetime.isoformat(timespec="milliseconds")` with `+00:00` as `Z`.
pub fn fmt_timestamp(dt: chrono::DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Optional fields of an event; everything defaults to null (`data` to `{}`).
#[derive(Debug, Default, Clone)]
pub struct EventFields {
    pub issue_run_id: Option<String>,
    pub project: Option<String>,
    pub issue_iid: Option<i64>,
    pub data: Option<Value>,
}

/// Append one event to today's JSONL file and return it. Errors if
/// `event_type` or `run_id` is empty. `events_dir` defaults to
/// [`default_events_dir`].
pub fn emit(
    event_type: &str,
    run_id: &str,
    fields: EventFields,
    events_dir: Option<&Path>,
) -> Result<Value, EventsError> {
    if event_type.is_empty() {
        return Err(EventsError::MissingEventType);
    }
    if run_id.is_empty() {
        return Err(EventsError::MissingRunId);
    }
    let events_dir = events_dir.map_or_else(default_events_dir, Path::to_path_buf);
    let now = Utc::now();
    let mut event = Map::new();
    event.insert("schema_version".into(), json!(SCHEMA_VERSION));
    event.insert("event_id".into(), json!(format!("evt_{}", new_uuid_hex())));
    event.insert("timestamp".into(), json!(fmt_timestamp(now)));
    event.insert("event_type".into(), json!(event_type));
    event.insert("run_id".into(), json!(run_id));
    event.insert("issue_run_id".into(), json!(fields.issue_run_id));
    event.insert("project".into(), json!(fields.project));
    event.insert("issue_iid".into(), json!(fields.issue_iid));
    event.insert("data".into(), fields.data.unwrap_or_else(|| json!({})));
    let event = Value::Object(event);

    std::fs::create_dir_all(&events_dir)?;
    let path = events_dir.join(format!("{}.jsonl", now.format("%Y-%m-%d")));
    append_line(&path, &format!("{}\n", to_py_json(&event)))?;
    Ok(event)
}

/// One `O_APPEND` write, so concurrent emitters never interleave a line.
pub fn append_line(path: &Path, text: &str) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    f.write_all(text.as_bytes())
}

/// Parsed events of one JSONL file; blank lines skipped, malformed lines
/// skipped with a warning on stderr.
pub fn read_events_file(path: &Path) -> Vec<Value> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push(v),
            Err(e) => eprintln!("events: skipping malformed line in {}: {e}", path.display()),
        }
    }
    out
}

/// Every `*.jsonl` file in `events_dir`, sorted by file name.
pub fn jsonl_files(events_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(events_dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort();
    files
}

/// Events across `*.jsonl` files whose date-named stem lies in
/// `[since_date, until_date]` (inclusive `YYYY-MM-DD` strings, either may be
/// `None`), in file-name order.
pub fn iter_events(
    events_dir: Option<&Path>,
    since_date: Option<&str>,
    until_date: Option<&str>,
) -> Vec<Value> {
    let dir = events_dir.map_or_else(default_events_dir, Path::to_path_buf);
    let mut out = Vec::new();
    for path in jsonl_files(&dir) {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if since_date.is_some_and(|s| stem.as_str() < s) {
            continue;
        }
        if until_date.is_some_and(|u| stem.as_str() > u) {
            continue;
        }
        out.extend(read_events_file(&path));
    }
    out
}

/// Events whose UTC timestamp date lies in `[since_date, until_date]`.
pub fn filter_by_date(
    events: Vec<Value>,
    since_date: Option<&str>,
    until_date: Option<&str>,
) -> Vec<Value> {
    if since_date.is_none() && until_date.is_none() {
        return events;
    }
    events
        .into_iter()
        .filter(|e| {
            let ts = e.get("timestamp").and_then(Value::as_str).unwrap_or("");
            let day: String = ts.chars().take(10).collect();
            !(day.is_empty()
                || since_date.is_some_and(|s| day.as_str() < s)
                || until_date.is_some_and(|u| day.as_str() > u))
        })
        .collect()
}

/// `events.py list`: the matching events as JSON lines.
pub fn list_lines(
    date: Option<&str>,
    run_id: Option<&str>,
    events_dir: Option<&Path>,
) -> Vec<String> {
    let dir = events_dir.map_or_else(default_events_dir, Path::to_path_buf);
    let files = match date {
        Some(d) => {
            let p = dir.join(format!("{d}.jsonl"));
            if p.exists() {
                vec![p]
            } else {
                vec![]
            }
        }
        None => jsonl_files(&dir),
    };
    let mut lines = Vec::new();
    for path in files {
        for ev in read_events_file(&path) {
            if run_id.is_some_and(|r| ev.get("run_id").and_then(Value::as_str) != Some(r)) {
                continue;
            }
            lines.push(to_py_json(&ev));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> EventFields {
        EventFields::default()
    }

    #[test]
    fn emit_writes_one_line_with_required_fields() {
        let d = tempfile::tempdir().unwrap();
        let ev = emit("run.started", "run_1", fields(), Some(d.path())).unwrap();
        let files = jsonl_files(d.path());
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(text.lines().count(), 1);
        let on_disk: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(on_disk, ev);
        assert_eq!(ev["event_type"], "run.started");
        assert_eq!(ev["schema_version"], 1);
        assert!(ev["event_id"].as_str().unwrap().starts_with("evt_"));
        assert_eq!(ev["event_id"].as_str().unwrap().len(), 4 + 32);
        assert!(ev["issue_run_id"].is_null() && ev["project"].is_null());
        assert_eq!(ev["data"], json!({}));
    }

    #[test]
    fn line_uses_python_separators_and_key_order() {
        let d = tempfile::tempdir().unwrap();
        let f = EventFields {
            data: Some(json!({"k": "é", "n": [1, 2]})),
            ..fields()
        };
        emit("t", "r", f, Some(d.path())).unwrap();
        let text = std::fs::read_to_string(&jsonl_files(d.path())[0]).unwrap();
        assert!(text.starts_with("{\"schema_version\": 1, \"event_id\": \"evt_"));
        assert!(text.contains("\"data\": {\"k\": \"\\u00e9\", \"n\": [1, 2]}}\n"));
    }

    #[test]
    fn timestamp_is_utc_millis_z() {
        let d = tempfile::tempdir().unwrap();
        let ev = emit("t", "r", fields(), Some(d.path())).unwrap();
        let ts = ev["timestamp"].as_str().unwrap();
        assert!(ts.ends_with('Z') && ts.len() == "2026-10-22T12:00:00.000Z".len());
    }

    #[test]
    fn optional_fields_round_trip_and_appends() {
        let d = tempfile::tempdir().unwrap();
        let f = EventFields {
            issue_run_id: Some("ir".into()),
            project: Some("p".into()),
            issue_iid: Some(123),
            data: Some(json!({"a": 1})),
        };
        let a = emit("issue.completed", "r", f, Some(d.path())).unwrap();
        let b = emit("run.completed", "r", fields(), Some(d.path())).unwrap();
        assert_eq!(a["issue_iid"], 123);
        assert_ne!(a["event_id"], b["event_id"]);
        let lines = list_lines(None, Some("r"), Some(d.path()));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn emit_rejects_empty_type_or_run_id() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            emit("", "r", fields(), Some(d.path())),
            Err(EventsError::MissingEventType)
        ));
        assert!(matches!(
            emit("t", "", fields(), Some(d.path())),
            Err(EventsError::MissingRunId)
        ));
    }

    #[test]
    fn iter_events_filters_by_filename_and_skips_malformed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("2026-10-01.jsonl"), "{\"run_id\": \"a\"}\n").unwrap();
        std::fs::write(
            d.path().join("2026-10-03.jsonl"),
            "not json\n\n{\"run_id\": \"b\"}\n",
        )
        .unwrap();
        let all = iter_events(Some(d.path()), None, None);
        assert_eq!(all.len(), 2);
        let some = iter_events(Some(d.path()), Some("2026-10-02"), None);
        assert_eq!(some, vec![json!({"run_id": "b"})]);
        let none = iter_events(Some(d.path()), None, Some("2026-09-30"));
        assert!(none.is_empty());
        assert!(iter_events(Some(&d.path().join("missing")), None, None).is_empty());
    }

    #[test]
    fn filter_by_date_uses_timestamp_day() {
        let evs = vec![
            json!({"timestamp": "2026-10-01T10:00:00.000Z"}),
            json!({"timestamp": "2026-10-05T10:00:00.000Z"}),
            json!({}),
        ];
        let got = filter_by_date(evs, Some("2026-10-02"), Some("2026-10-06"));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn default_dir_honours_env_at_call_time() {
        // Env mutation is avoided (tests run in parallel); only check when unset.
        match std::env::var_os(EVENTS_DIR_ENV).filter(|v| !v.is_empty()) {
            Some(dir) => assert_eq!(default_events_dir(), PathBuf::from(dir)),
            None => assert!(default_events_dir().ends_with("events")),
        }
    }
}

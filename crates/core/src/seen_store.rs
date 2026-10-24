//! Generic per-loop "seen" store: de-dups work items across loop runs
//! (mirrors `bin/seen_store.py`).
//!
//! File: `<root>/<loop_name>/seen.json` = `{"<key>": "<iso-8601 utc>"}`. Entries
//! older than `window_days` are dropped on [`SeenStore::save`].

use crate::pyjson;
use chrono::{DateTime, Duration, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub type NowFn = Box<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Python's `datetime.isoformat()` for a UTC instant (`+00:00`, microseconds
/// only when non-zero).
pub fn py_isoformat(dt: DateTime<Utc>) -> String {
    let secs = if dt.timestamp_subsec_micros() == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    dt.to_rfc3339_opts(secs, false)
}

/// `datetime.fromisoformat` + "naive means UTC" + convert to UTC.
pub fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    let v = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(v, fmt) {
            return Some(n.and_utc());
        }
    }
    DateTime::parse_from_str(&v.replace(' ', "T"), "%Y-%m-%dT%H:%M:%S%.f%:z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// The per-checkout state root: `<repo root>/outputs/loops`.
pub fn default_state_root() -> PathBuf {
    crate::runner_util::repo_root()
        .join("outputs")
        .join("loops")
}

pub struct SeenStore {
    now_fn: NowFn,
    window: Duration,
    path: PathBuf,
    entries: Map<String, Value>,
}

impl SeenStore {
    /// `Err` for a loop name that could escape the state directory.
    pub fn new(
        loop_name: &str,
        state_dir: Option<&Path>,
        window_days: i64,
        now_fn: Option<NowFn>,
    ) -> Result<Self, String> {
        let valid = !loop_name.is_empty()
            && loop_name != "."
            && loop_name != ".."
            && loop_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !valid {
            return Err(format!(
                "Invalid loop name {} - loop names may only contain letters, digits, '.', '_', and '-'.",
                crate::loops_config::py_repr_str(loop_name)
            ));
        }
        let root = state_dir.map_or_else(default_state_root, Path::to_path_buf);
        let path = root.join(loop_name).join("seen.json");
        let entries = load(&path);
        Ok(Self {
            now_fn: now_fn.unwrap_or_else(|| Box::new(Utc::now)),
            window: Duration::days(window_days),
            path,
            entries,
        })
    }

    /// The default 30-day window.
    pub fn open(
        loop_name: &str,
        state_dir: Option<&Path>,
        now_fn: Option<NowFn>,
    ) -> Result<Self, String> {
        Self::new(loop_name, state_dir, 30, now_fn)
    }

    pub fn has(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn add(&mut self, key: &str) {
        self.entries.insert(
            key.to_string(),
            Value::String(py_isoformat((self.now_fn)())),
        );
    }

    /// Prune entries outside the window (or with an unparseable date), then
    /// write atomically.
    pub fn save(&mut self) -> std::io::Result<()> {
        let cutoff = (self.now_fn)() - self.window;
        let mut kept = Map::new();
        for (key, value) in &self.entries {
            if let Some(dt) = value.as_str().and_then(parse_iso) {
                if dt >= cutoff {
                    kept.insert(key.clone(), Value::String(py_isoformat(dt)));
                }
            }
        }
        self.entries = kept;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = self.path.with_file_name(format!(".{name}.tmp"));
        std::fs::write(
            &tmp,
            pyjson::to_string_pretty(&Value::Object(self.entries.clone())),
        )?;
        std::fs::rename(&tmp, &self.path)
    }
}

fn load(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 6, 9, 0, 0).unwrap()
    }

    fn store(dir: &Path, at: DateTime<Utc>, window: i64) -> SeenStore {
        SeenStore::new("demo-loop", Some(dir), window, Some(Box::new(move || at))).unwrap()
    }

    fn file(dir: &Path) -> PathBuf {
        dir.join("demo-loop").join("seen.json")
    }

    #[test]
    fn roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let mut s = store(d.path(), now(), 30);
        s.add("a");
        s.save().unwrap();
        assert!(store(d.path(), now(), 30).has("a"));
        assert!(!store(d.path(), now(), 30).has("b"));
    }

    #[test]
    fn prunes_old_entries() {
        let d = tempfile::tempdir().unwrap();
        let mut s = store(d.path(), now(), 7);
        s.add("old");
        s.save().unwrap();
        let mut later = store(d.path(), now() + Duration::days(8), 7);
        later.add("new");
        later.save().unwrap();
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(file(d.path())).unwrap()).unwrap();
        assert_eq!(
            data.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["new"]
        );
    }

    #[test]
    fn corrupt_or_non_object_file_is_empty() {
        for body in ["{nope", "[1]"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(d.path().join("demo-loop")).unwrap();
            std::fs::write(file(d.path()), body).unwrap();
            assert!(!store(d.path(), now(), 30).has("a"));
        }
    }

    #[test]
    fn drops_unparseable_dates_on_save() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("demo-loop")).unwrap();
        std::fs::write(
            file(d.path()),
            json!({"bad": "x", "ok": py_isoformat(now())}).to_string(),
        )
        .unwrap();
        let mut s = store(d.path(), now(), 30);
        s.save().unwrap();
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(file(d.path())).unwrap()).unwrap();
        assert_eq!(data.as_object().unwrap().keys().collect::<Vec<_>>(), ["ok"]);
    }

    #[test]
    fn rejects_path_traversal_loop_names() {
        let d = tempfile::tempdir().unwrap();
        for bad in ["../x", "..", ".", "", "a/b"] {
            assert!(
                SeenStore::new(bad, Some(d.path()), 30, None).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn unsaved_adds_are_not_persisted() {
        let d = tempfile::tempdir().unwrap();
        let mut s = store(d.path(), now(), 30);
        s.add("a");
        assert!(!store(d.path(), now(), 30).has("a"));
    }

    #[test]
    fn timestamps_use_python_isoformat() {
        assert_eq!(py_isoformat(now()), "2026-10-06T09:00:00+00:00");
        let frac = now() + Duration::microseconds(5);
        assert_eq!(py_isoformat(frac), "2026-10-06T09:00:00.000005+00:00");
        assert_eq!(parse_iso("2026-10-06T09:00:00"), Some(now()));
        assert_eq!(parse_iso("2026-10-06T09:00:00Z"), Some(now()));
        assert_eq!(parse_iso("2026-10-06T11:00:00+02:00"), Some(now()));
        assert_eq!(parse_iso("nope"), None);
    }
}

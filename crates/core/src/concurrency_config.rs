//! How much LoopX runs at once (`<LOOP_ENGINEERING_HOME>/concurrency.json`):
//! GitLab issues within one run, topics within one Topic Monitor run, and
//! loops started from the run queue. A missing or unreadable file, or a value
//! out of range, falls back to the defaults; 1 means one at a time.

use crate::config::home_dir;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const MIN: usize = 1;
pub const MAX: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub issues: usize,
    pub topics: usize,
    pub loops: usize,
}

pub const DEFAULTS: Limits = Limits {
    issues: 5,
    topics: 2,
    loops: 3,
};

const KEYS: [&str; 3] = [
    "max_parallel_issues",
    "max_parallel_topics",
    "max_parallel_loops",
];

pub fn default_config_path() -> PathBuf {
    home_dir().join("concurrency.json")
}

fn read_object(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn valid(v: Option<&Value>) -> Option<usize> {
    let n = v?.as_u64()? as usize;
    (MIN..=MAX).contains(&n).then_some(n)
}

pub fn load(path: &Path) -> Limits {
    let m = read_object(path);
    Limits {
        issues: valid(m.get(KEYS[0])).unwrap_or(DEFAULTS.issues),
        topics: valid(m.get(KEYS[1])).unwrap_or(DEFAULTS.topics),
        loops: valid(m.get(KEYS[2])).unwrap_or(DEFAULTS.loops),
    }
}

/// [`load`] against [`default_config_path`] (resolved now).
pub fn current() -> Limits {
    load(&default_config_path())
}

/// Saves all three; `Err` names the first one outside 1–10.
pub fn save(limits: Limits, path: &Path) -> Result<(), String> {
    for (key, n) in KEYS
        .iter()
        .zip([limits.issues, limits.topics, limits.loops])
    {
        if !(MIN..=MAX).contains(&n) {
            return Err(format!("{key} must be between {MIN} and {MAX}, not {n}"));
        }
    }
    let mut m = read_object(path);
    m.insert(KEYS[0].into(), json!(limits.issues));
    m.insert(KEYS[1].into(), json!(limits.topics));
    m.insert(KEYS[2].into(), json!(limits.loops));
    crate::file_lock::write_atomic(
        path,
        crate::pyjson::to_string_pretty(&Value::Object(m)).as_bytes(),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_broken_file_gives_the_defaults() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("concurrency.json");
        assert_eq!(load(&p), DEFAULTS);
        std::fs::write(&p, "{nope").unwrap();
        assert_eq!(load(&p), DEFAULTS);
        assert_eq!(
            DEFAULTS,
            Limits {
                issues: 5,
                topics: 2,
                loops: 3
            }
        );
    }

    #[test]
    fn saved_limits_load_back_and_other_keys_survive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("concurrency.json");
        std::fs::write(&p, r#"{"note": "kept"}"#).unwrap();
        let l = Limits {
            issues: 8,
            topics: 1,
            loops: 4,
        };
        save(l, &p).unwrap();
        assert_eq!(load(&p), l);
        assert_eq!(read_object(&p)["note"], "kept");
    }

    #[test]
    fn out_of_range_values_are_refused_on_save_and_ignored_on_load() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("concurrency.json");
        let e = save(
            Limits {
                issues: 0,
                ..DEFAULTS
            },
            &p,
        )
        .unwrap_err();
        assert!(
            e.contains("max_parallel_issues") && e.contains("between 1 and 10"),
            "{e}"
        );
        assert!(save(
            Limits {
                loops: 11,
                ..DEFAULTS
            },
            &p
        )
        .is_err());
        assert!(!p.exists());
        std::fs::write(
            &p,
            r#"{"max_parallel_issues": 0, "max_parallel_topics": "3", "max_parallel_loops": 99}"#,
        )
        .unwrap();
        assert_eq!(load(&p), DEFAULTS);
    }
}

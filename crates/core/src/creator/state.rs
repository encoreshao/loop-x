//! Creator state the user sets by hand: which videos are approved and where each was
//! published. Stored in `<home>/creator.json`; a missing or corrupt file shows as empty
//! state, but a corrupt one is never overwritten by a change.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreatorState {
    pub approved: BTreeSet<String>,
    /// slug -> platforms (`wechat`, `douyin`)
    pub published: BTreeMap<String, Vec<String>>,
}

const PLATFORMS: [&str; 2] = ["wechat", "douyin"];

/// `<loopx_home>/creator.json`.
pub fn state_path(loopx_home: &Path) -> std::path::PathBuf {
    loopx_home.join("creator.json")
}

/// The state for display: a missing or corrupt file reads as empty.
pub fn load(path: &Path) -> CreatorState {
    let Some(v) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return CreatorState::default();
    };
    from_value(&v)
}

/// The state before a change: only a missing file is empty; one that cannot be read
/// or parsed is an error, so a toggle never replaces the user's file with defaults.
fn load_for_update(path: &Path) -> Result<CreatorState, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CreatorState::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => Ok(from_value(&v)),
        _ => Err(format!(
            "{} is not valid Creator state; fix or remove it first",
            path.display()
        )),
    }
}

fn from_value(v: &Value) -> CreatorState {
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let approved = strings(v.get("approved").unwrap_or(&Value::Null))
        .into_iter()
        .collect();
    let published = v
        .get("published")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let mut l = strings(v);
                    l.retain(|p| PLATFORMS.contains(&p.as_str()));
                    (k.clone(), l)
                })
                .filter(|(_, v)| !v.is_empty())
                .collect()
        })
        .unwrap_or_default();
    CreatorState {
        approved,
        published,
    }
}

fn save(path: &Path, s: &CreatorState) -> Result<(), String> {
    let v = json!({ "approved": s.approved, "published": s.published });
    crate::settings_store::atomic_write_json(&v, path).map_err(|e| e.to_string())
}

pub fn set_approved(path: &Path, slug: &str, on: bool) -> Result<(), String> {
    let mut s = load_for_update(path)?;
    if on {
        s.approved.insert(slug.to_string());
    } else {
        s.approved.remove(slug);
    }
    save(path, &s)
}

pub fn set_published(path: &Path, slug: &str, platform: &str, on: bool) -> Result<(), String> {
    if !PLATFORMS.contains(&platform) {
        return Err(format!("unknown platform: {platform}"));
    }
    let mut s = load_for_update(path)?;
    let list = s.published.entry(slug.to_string()).or_default();
    list.retain(|p| p != platform);
    if on {
        list.push(platform.to_string());
    }
    if list.is_empty() {
        s.published.remove(slug);
    }
    save(path, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        set_approved(&p, "a", true).unwrap();
        set_approved(&p, "b", true).unwrap();
        set_approved(&p, "b", false).unwrap();
        set_published(&p, "a", "wechat", true).unwrap();
        set_published(&p, "a", "douyin", true).unwrap();
        set_published(&p, "a", "wechat", false).unwrap();
        let s = load(&p);
        assert_eq!(s.approved, ["a".to_string()].into_iter().collect());
        assert_eq!(s.published["a"], vec!["douyin".to_string()]);
    }

    #[test]
    fn corrupt_state_is_default() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load(&p), CreatorState::default());
        assert_eq!(
            load(&d.path().join("missing.json")),
            CreatorState::default()
        );
    }

    #[test]
    fn a_corrupt_state_file_is_never_overwritten_by_a_toggle() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        for bad in ["{not json", "[1, 2]"] {
            std::fs::write(&p, bad).unwrap();
            assert!(set_approved(&p, "a", true).is_err(), "{bad}");
            assert!(set_published(&p, "a", "wechat", true).is_err(), "{bad}");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
        }
        // A missing file is simply empty state.
        let fresh = d.path().join("new.json");
        set_approved(&fresh, "a", true).unwrap();
        assert!(load(&fresh).approved.contains("a"));
    }

    #[test]
    fn unknown_platforms_are_dropped_on_load() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        std::fs::write(&p, r#"{"published":{"a":["wechat","evil"],"b":["x"]}}"#).unwrap();
        let s = load(&p);
        assert_eq!(s.published["a"], vec!["wechat".to_string()]);
        assert!(!s.published.contains_key("b"));
    }

    #[test]
    fn invalid_platform_rejected() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        assert!(set_published(&p, "a", "tiktok", true).is_err());
        assert!(!p.exists());
    }
}

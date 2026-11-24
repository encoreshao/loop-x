//! Loops pinned to the dashboard sidebar (`sidebar_pins.json` beside
//! `loops.json`). Turning a loop on pins it while fewer than
//! [`AUTO_PIN_LIMIT`] are pinned; turning it off unpins it.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const AUTO_PIN_LIMIT: usize = 5;
pub const FILE_NAME: &str = "sidebar_pins.json";

pub fn pins_path(loops_config: &Path) -> PathBuf {
    loops_config
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(FILE_NAME)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    Missing,
    Pins(Vec<String>),
    Corrupt,
}

pub fn read(path: &Path) -> Stored {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Stored::Missing,
        Err(_) => return Stored::Corrupt,
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text) else {
        return Stored::Corrupt;
    };
    let Some(Value::Array(items)) = obj.get("pinned") else {
        return Stored::Corrupt;
    };
    items
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .map_or(Stored::Corrupt, Stored::Pins)
}

/// The pins in effect: the stored list, or with none readable the first
/// enabled loops in registry order.
pub fn effective(stored: &Stored, enabled_in_order: &[String]) -> Vec<String> {
    match stored {
        Stored::Pins(v) => v.clone(),
        Stored::Missing | Stored::Corrupt => enabled_in_order
            .iter()
            .take(AUTO_PIN_LIMIT)
            .cloned()
            .collect(),
    }
}

fn current(path: &Path, enabled_in_order: &[String]) -> Result<Vec<String>, String> {
    match read(path) {
        Stored::Corrupt => Err(format!("{} is not a valid pins file", path.display())),
        stored => Ok(effective(&stored, enabled_in_order)),
    }
}

fn write(path: &Path, pins: &[String]) -> Result<(), String> {
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    let text = serde_json::to_string_pretty(&json!({ "pinned": pins })).unwrap_or_default();
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })
}

fn apply(path: &Path, mut pins: Vec<String>, name: &str, pinned: bool) -> Result<(), String> {
    let had = pins.iter().any(|p| p == name);
    match (pinned, had) {
        (true, false) => pins.push(name.to_string()),
        (false, true) => pins.retain(|p| p != name),
        _ if path.exists() => return Ok(()),
        _ => {}
    }
    write(path, &pins)
}

pub fn set_pinned(
    path: &Path,
    name: &str,
    pinned: bool,
    enabled_in_order: &[String],
) -> Result<(), String> {
    apply(path, current(path, enabled_in_order)?, name, pinned)
}

pub fn on_enabled_changed(
    path: &Path,
    name: &str,
    enabled: bool,
    enabled_before: &[String],
) -> Result<(), String> {
    let pins = current(path, enabled_before)?;
    let shown = pins.iter().filter(|p| enabled_before.contains(p)).count();
    let pin = enabled && (pins.iter().any(|p| p == name) || shown < AUTO_PIN_LIMIT);
    apply(path, pins, name, pin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn missing_file_seeds_from_enabled_loops_capped_at_five() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        let stored = read(&p);
        assert_eq!(stored, Stored::Missing);
        assert_eq!(
            effective(&stored, &names(&["a", "b", "c", "d", "e", "f"])),
            names(&["a", "b", "c", "d", "e"])
        );
        assert!(!p.exists());
    }

    #[test]
    fn enabling_pins_until_the_limit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(&p, r#"{"pinned": ["a", "b", "c", "d"]}"#);
        on_enabled_changed(&p, "e", true, &[]).unwrap();
        assert_eq!(read(&p), Stored::Pins(names(&["a", "b", "c", "d", "e"])));
    }

    #[test]
    fn enabling_past_the_limit_does_not_pin() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(&p, r#"{"pinned": ["a", "b", "c", "d", "e"]}"#);
        on_enabled_changed(&p, "f", true, &names(&["a", "b", "c", "d", "e"])).unwrap();
        assert_eq!(read(&p), Stored::Pins(names(&["a", "b", "c", "d", "e"])));
    }

    #[test]
    fn enabling_with_no_file_seeds_then_pins() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        on_enabled_changed(&p, "c", true, &names(&["a", "b"])).unwrap();
        assert_eq!(read(&p), Stored::Pins(names(&["a", "b", "c"])));
    }

    #[test]
    fn the_cap_counts_only_pins_of_enabled_loops() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(
            &p,
            r#"{"pinned": ["a", "gone1", "gone2", "gone3", "gone4"]}"#,
        );
        on_enabled_changed(&p, "f", true, &names(&["a"])).unwrap();
        assert_eq!(
            read(&p),
            Stored::Pins(names(&["a", "gone1", "gone2", "gone3", "gone4", "f"]))
        );
    }

    #[test]
    fn disabling_unpins() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(&p, r#"{"pinned": ["a", "b"]}"#);
        on_enabled_changed(&p, "a", false, &[]).unwrap();
        assert_eq!(read(&p), Stored::Pins(names(&["b"])));
    }

    #[test]
    fn manual_pin_has_no_cap() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(&p, r#"{"pinned": ["a", "b", "c", "d", "e"]}"#);
        set_pinned(&p, "f", true, &[]).unwrap();
        assert_eq!(
            read(&p),
            Stored::Pins(names(&["a", "b", "c", "d", "e", "f"]))
        );
        set_pinned(&p, "c", false, &[]).unwrap();
        assert_eq!(read(&p), Stored::Pins(names(&["a", "b", "d", "e", "f"])));
    }

    #[test]
    fn a_corrupt_pins_file_is_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(&p, "{nope");
        assert_eq!(read(&p), Stored::Corrupt);
        assert!(set_pinned(&p, "a", true, &[]).is_err());
        assert!(on_enabled_changed(&p, "a", true, &[]).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{nope");
    }

    #[test]
    fn non_string_entries_make_the_file_corrupt() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        write(&p, r#"{"pinned": [1]}"#);
        assert_eq!(read(&p), Stored::Corrupt);
        write(&p, r#"{"pinned": "a"}"#);
        assert_eq!(read(&p), Stored::Corrupt);
    }

    #[test]
    fn the_pins_file_sits_beside_the_registry() {
        assert_eq!(
            pins_path(Path::new("/x/loops.json")),
            Path::new("/x/sidebar_pins.json")
        );
    }
}

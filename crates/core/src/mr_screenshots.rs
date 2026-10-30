//! Before/after screenshots for a merge request: reads the pairs the agent
//! listed in its handoff, validates the image files, and renders the
//! "## Screenshots" section of the MR description.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The most pairs one merge request shows.
pub const MAX_PAIRS: usize = 6;
/// The largest image file accepted, in bytes.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// One validated screenshot pair. `before` is `None` for a brand-new page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotPair {
    pub label: String,
    pub before: Option<PathBuf>,
    pub after: PathBuf,
}

/// Where a run's screenshots live: `<repo_root>/outputs/screens/<alias>-<iid>`.
pub fn screens_dir(repo_root: &Path, alias: &str, iid: &str) -> PathBuf {
    repo_root
        .join("outputs")
        .join("screens")
        .join(format!("{alias}-{iid}"))
}

/// Resolve `rel` against either allowed directory and check the file is a
/// PNG within the size limit that stays inside that directory.
fn resolve(rel: &str, allowed: &[&Path]) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("empty path".to_string());
    }
    let mut last = format!("{rel} not found");
    for dir in allowed {
        let Ok(root) = dir.canonicalize() else {
            continue;
        };
        let Ok(path) = dir.join(rel).canonicalize() else {
            continue;
        };
        if !path.starts_with(&root) {
            last = format!("{rel} is outside the allowed directories");
            continue;
        }
        return check_file(rel, path);
    }
    Err(last)
}

fn check_file(rel: &str, path: PathBuf) -> Result<PathBuf, String> {
    let meta = fs::metadata(&path).map_err(|e| format!("{rel}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{rel} is not a file"));
    }
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "{rel} is larger than {} MB",
            MAX_BYTES / 1024 / 1024
        ));
    }
    let mut magic = [0u8; 8];
    let ok = fs::File::open(&path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == PNG_MAGIC;
    if !ok {
        return Err(format!("{rel} is not a PNG"));
    }
    Ok(path)
}

fn entry_pair(entry: &Value, allowed: &[&Path]) -> Result<ShotPair, String> {
    let label = entry
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Screenshot")
        .to_string();
    let after_rel = entry
        .get("after")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("\"{label}\" has no after image"))?;
    let after = resolve(after_rel, allowed).map_err(|e| format!("\"{label}\" dropped: {e}"))?;
    let before = match entry.get("before").and_then(Value::as_str) {
        Some(rel) if !rel.trim().is_empty() => {
            Some(resolve(rel, allowed).map_err(|e| format!("\"{label}\" dropped: {e}"))?)
        }
        _ => None,
    };
    Ok(ShotPair {
        label,
        before,
        after,
    })
}

/// The valid pairs from the handoff's `screenshots` list, plus a note for each
/// entry that was dropped (and one when the list was cut to [`MAX_PAIRS`]).
/// Paths are relative to the handoff directory or the screens directory.
pub fn screenshot_pairs(
    data: &Value,
    handoff_dir: &Path,
    screens_dir: &Path,
) -> (Vec<ShotPair>, Vec<String>) {
    let Some(entries) = data.get("screenshots").and_then(Value::as_array) else {
        return (Vec::new(), Vec::new());
    };
    let allowed = [handoff_dir, screens_dir];
    let mut pairs = Vec::new();
    let mut notes = Vec::new();
    let mut over = false;
    for entry in entries {
        if pairs.len() >= MAX_PAIRS {
            over = true;
            break;
        }
        match entry_pair(entry, &allowed) {
            Ok(pair) => pairs.push(pair),
            Err(note) => notes.push(note),
        }
    }
    if over {
        notes.push(format!(
            "Only the first {MAX_PAIRS} screenshot pairs are shown."
        ));
    }
    (pairs, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn png(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn pairs_resolve_against_either_allowed_dir() {
        let handoff = tempfile::tempdir().unwrap();
        let screens = tempfile::tempdir().unwrap();
        png(&screens.path().join("1-before.png"));
        png(&handoff.path().join("screenshots/1-after.png"));
        let data = json!({"screenshots": [
            {"label": "x", "before": "1-before.png", "after": "screenshots/1-after.png"}
        ]});
        let (pairs, notes) = screenshot_pairs(&data, handoff.path(), screens.path());
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].label, "x");
        assert!(pairs[0].before.as_ref().unwrap().ends_with("1-before.png"));
        assert!(pairs[0].after.ends_with("1-after.png"));
    }

    #[test]
    fn missing_before_is_allowed() {
        let handoff = tempfile::tempdir().unwrap();
        let screens = tempfile::tempdir().unwrap();
        png(&handoff.path().join("screens/7/1-after.png"));
        let data = json!({"screenshots": [{"label": "x", "after": "screens/7/1-after.png"}]});
        let (pairs, notes) = screenshot_pairs(&data, handoff.path(), screens.path());
        assert!(notes.is_empty());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].before, None);
    }

    #[test]
    fn escaping_or_bad_files_are_dropped_with_a_note() {
        let root = tempfile::tempdir().unwrap();
        let handoff = root.path().join("handoff");
        let screens = root.path().join("screens");
        fs::create_dir_all(&handoff).unwrap();
        fs::create_dir_all(&screens).unwrap();
        png(&root.path().join("outside.png"));
        std::os::unix::fs::symlink(root.path().join("outside.png"), handoff.join("link.png"))
            .unwrap();
        fs::write(handoff.join("text.png"), b"not a png at all").unwrap();
        let mut big = PNG_MAGIC.to_vec();
        big.resize(MAX_BYTES as usize + 1, 0);
        fs::write(handoff.join("big.png"), big).unwrap();
        for after in [
            "../outside.png",
            "link.png",
            "text.png",
            "big.png",
            "missing.png",
        ] {
            let data = json!({"screenshots": [{"label": "x", "after": after}]});
            let (pairs, notes) = screenshot_pairs(&data, &handoff, &screens);
            assert!(pairs.is_empty(), "{after} was accepted");
            assert_eq!(notes.len(), 1, "{after}: {notes:?}");
        }
    }

    #[test]
    fn at_most_six_pairs() {
        let handoff = tempfile::tempdir().unwrap();
        let screens = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        for i in 0..8 {
            let name = format!("{i}.png");
            png(&handoff.path().join(&name));
            entries.push(json!({"label": format!("s{i}"), "after": name}));
        }
        let data = json!({ "screenshots": entries });
        let (pairs, notes) = screenshot_pairs(&data, handoff.path(), screens.path());
        assert_eq!(pairs.len(), 6);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains('6'));
    }

    #[test]
    fn no_field_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (pairs, notes) = screenshot_pairs(&json!({}), dir.path(), dir.path());
        assert!(pairs.is_empty() && notes.is_empty());
    }

    #[test]
    fn screens_dir_is_under_outputs() {
        assert_eq!(
            screens_dir(Path::new("/r"), "web", "42"),
            PathBuf::from("/r/outputs/screens/web-42")
        );
    }
}

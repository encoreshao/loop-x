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

/// One pair after its images were uploaded; the strings are GitLab upload URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedPair {
    pub label: String,
    pub before: Option<String>,
    pub after: String,
}

/// Make a label safe inside bold text and a Markdown table.
fn escape_label(label: &str) -> String {
    label
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

/// The merge request description's "## Screenshots" section, or `None` when
/// there is neither a pair nor a note to show.
pub fn screenshots_section(pairs: &[UploadedPair], note: Option<&str>) -> Option<String> {
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    if pairs.is_empty() && note.is_none() {
        return None;
    }
    let mut blocks = vec!["## Screenshots".to_string()];
    for pair in pairs {
        let before = match &pair.before {
            Some(url) => format!("![before]({url})"),
            None => "New page \u{2014} no before".to_string(),
        };
        blocks.push(format!(
            "**{}**\n\n| Before | After |\n|---|---|\n| {before} | ![after]({}) |",
            escape_label(&pair.label),
            pair.after
        ));
    }
    if let Some(note) = note {
        blocks.push(note.to_string());
    }
    Some(blocks.join("\n\n"))
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

    fn up(label: &str, before: Option<&str>, after: &str) -> UploadedPair {
        UploadedPair {
            label: label.into(),
            before: before.map(String::from),
            after: after.into(),
        }
    }

    #[test]
    fn section_renders_a_before_after_table_per_pair() {
        let pairs = [up(
            "Status select",
            Some("/uploads/a/b.png"),
            "/uploads/c/d.png",
        )];
        assert_eq!(
            screenshots_section(&pairs, None).unwrap(),
            "## Screenshots\n\n**Status select**\n\n| Before | After |\n|---|---|\n| ![before](/uploads/a/b.png) | ![after](/uploads/c/d.png) |"
        );
    }

    #[test]
    fn two_pairs_are_separated_by_a_blank_line_and_a_note_comes_last() {
        let pairs = [up("a", None, "/u/1.png"), up("b", None, "/u/2.png")];
        let out = screenshots_section(&pairs, Some("Only 6 shown")).unwrap();
        assert!(out.contains("|\n\n**b**"), "{out}");
        assert!(out.ends_with("|\n\nOnly 6 shown"), "{out}");
    }

    #[test]
    fn missing_before_cell_says_new_page() {
        let out = screenshots_section(&[up("x", None, "/u/1.png")], None).unwrap();
        assert!(out.contains("| New page \u{2014} no before | ![after](/u/1.png) |"));
    }

    #[test]
    fn note_only_gives_one_line_section() {
        assert_eq!(
            screenshots_section(&[], Some("Capture failed: no Chrome")).unwrap(),
            "## Screenshots\n\nCapture failed: no Chrome"
        );
    }

    #[test]
    fn nothing_gives_none() {
        assert_eq!(screenshots_section(&[], None), None);
    }

    #[test]
    fn labels_are_markdown_safe() {
        let out = screenshots_section(&[up("a|b <script>", None, "/u/1.png")], None).unwrap();
        assert!(out.contains("**a\\|b &lt;script&gt;**"), "{out}");
    }
}

//! Diff text for the review prompt and the line map GitLab needs to anchor notes.
use crate::loops_config::{py_str, py_truthy};
use serde_json::{json, Value};

/// `(text, truncated)`; `cap` counts characters.
pub fn build_diff_text(changes: &[Value], cap: usize) -> (String, bool) {
    let mut parts = String::new();
    let (mut size, mut truncated) = (0usize, false);
    for ch in changes {
        let path = ["new_path", "old_path"]
            .iter()
            .filter_map(|k| ch.get(*k))
            .find(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default();
        let diff = ch
            .get("diff")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default();
        let chunk = format!("--- a/{path}\n+++ b/{path}\n{diff}\n");
        let len = chunk.chars().count();
        if size + len > cap {
            parts.extend(chunk.chars().take(cap.saturating_sub(size)));
            truncated = true;
            break;
        }
        parts.push_str(&chunk);
        size += len;
    }
    if truncated {
        parts.push_str(&format!("\n[diff truncated at {cap} characters]"));
    }
    (parts, truncated)
}

/// Python `str.splitlines()` line boundaries.
fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let brk = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if brk {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(_, '\n')) = chars.peek() {
                    chars.next();
                    end += 1;
                }
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `@@ -a[,n] +b[,m] @@` -> `(a, b)`.
fn parse_hunk(line: &str) -> Option<(i64, i64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = take_num(rest)?;
    let rest = skip_count(rest);
    let rest = rest.strip_prefix(" +")?;
    let (new, rest) = take_num(rest)?;
    let rest = skip_count(rest);
    rest.starts_with(" @@").then_some((old, new))
}

fn take_num(s: &str) -> Option<(i64, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((s[..end].parse().ok()?, &s[end..]))
}

fn skip_count(s: &str) -> &str {
    match s.strip_prefix(',') {
        Some(r) => {
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            if end == 0 {
                s
            } else {
                &r[end..]
            }
        }
        None => s,
    }
}

/// `{path: {new_line: old_line}}` for the unchanged context lines of each file
/// in [`build_diff_text`]'s output. Added lines are absent (they have no old
/// line); GitLab needs `old_line` as well as `new_line` to anchor a note on a
/// context line.
pub fn diff_line_map(
    diff_text: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<i64, i64>> {
    let mut out: std::collections::BTreeMap<String, std::collections::BTreeMap<i64, i64>> =
        Default::default();
    let (mut path, mut old, mut new): (Option<String>, i64, i64) = (None, 0, 0);
    for line in splitlines(diff_text) {
        if let Some(p) = line.strip_prefix("+++ b/") {
            out.entry(p.to_string()).or_default();
            path = Some(p.to_string());
            continue;
        }
        if line.starts_with("--- a/") {
            continue;
        }
        if let Some((o, n)) = parse_hunk(line) {
            old = o;
            new = n;
            continue;
        }
        let Some(p) = &path else { continue };
        if old == 0 && new == 0 {
            continue;
        }
        if line.starts_with('+') {
            new += 1;
        } else if line.starts_with('-') {
            old += 1;
        } else if line.starts_with(' ') {
            out.entry(p.clone()).or_default().insert(new, old);
            old += 1;
            new += 1;
        }
    }
    out
}

/// Split a `diff --git` patch into GitLab `/diffs` rows (`diff` starts at the first
/// `@@`). `lenient` skips files without a text hunk (binary, mode-only) instead of
/// failing, as real `git diff` output has them.
pub fn parse_patch(text: &str, lenient: bool) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for block in text
        .split("\ndiff --git ")
        .map(|b| b.trim_start_matches("diff --git "))
    {
        if block.trim().is_empty() {
            continue;
        }
        let (mut old, mut new) = (None, None);
        let mut hunks: Vec<&str> = Vec::new();
        for line in block.lines() {
            if !hunks.is_empty() {
                hunks.push(line);
            } else if let Some(p) = line.strip_prefix("--- ") {
                old = Some(p.trim_start_matches("a/").to_string());
            } else if let Some(p) = line.strip_prefix("+++ ") {
                new = Some(p.trim_start_matches("b/").to_string());
            } else if line.starts_with("@@") {
                hunks.push(line);
            }
        }
        let (Some(old), Some(new)) = (old, new) else {
            if lenient {
                continue;
            }
            return Err("every file in patch.diff needs --- and +++ lines".into());
        };
        if hunks.is_empty() {
            if lenient {
                continue;
            }
            return Err(format!("{new}: no @@ hunk"));
        }
        let (new_file, deleted) = (old == "/dev/null", new == "/dev/null");
        let new_path = if deleted { old.clone() } else { new };
        let old_path = if new_file { new_path.clone() } else { old };
        out.push(json!({"old_path": old_path, "new_path": new_path,
                        "diff": format!("{}\n", hunks.join("\n")),
                        "new_file": new_file, "deleted_file": deleted}));
    }
    if out.is_empty() && !lenient {
        return Err("patch.diff has no files".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_line_map_and_splitlines_helpers() {
        assert_eq!(splitlines("a\r\nb\rc\n"), ["a", "b", "c"]);
        assert_eq!(parse_hunk("@@ -3,4 +5 @@ foo"), Some((3, 5)));
        assert_eq!(parse_hunk("@@ nope"), None);
        let map = diff_line_map("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n");
        assert_eq!(map["x"].get(&2), Some(&2));
        assert!(!map["x"].contains_key(&1));
    }
}

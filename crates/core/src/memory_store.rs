//! Durable per-issue task memory as real markdown files (mirrors
//! `bin/memory_store.py`): one frontmatter+body file per issue plus a
//! per-project `MEMORY.md` index, under `<LOOP_ENGINEERING_HOME>/memory/<alias>/`.
//! Distinct from `gitlab_cache`-backed `project_memory` (`loopx memory`).
//!
//! Every function that takes `root: Option<&Path>` resolves the default
//! (`<LOOP_ENGINEERING_HOME>/memory`) at call time.

use crate::config::home_dir;
use crate::pyjson;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// `<LOOP_ENGINEERING_HOME>/memory`.
pub fn default_memory_root() -> PathBuf {
    home_dir().join("memory")
}

fn resolve_root(root: Option<&Path>) -> PathBuf {
    root.map(Path::to_path_buf)
        .unwrap_or_else(default_memory_root)
}

fn project_dir(alias: &str, root: Option<&Path>) -> PathBuf {
    resolve_root(root).join(alias)
}

/// A frontmatter value that is either a bare string or a `[a, b]` list.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Str(String),
    List(Vec<String>),
}

impl Scalar {
    fn parse(value: &str) -> Scalar {
        let value = value.trim();
        if value.starts_with('[') && value.ends_with(']') && value.len() >= 2 {
            let inner = value[1..value.len() - 1].trim();
            Scalar::List(if inner.is_empty() {
                Vec::new()
            } else {
                inner
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
        } else {
            Scalar::Str(value.to_string())
        }
    }

    /// What a Python f-string would render (`str(list)` for lists).
    fn render(&self) -> String {
        match self {
            Scalar::Str(s) => s.clone(),
            Scalar::List(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|i| crate::pydumps::py_repr_str(i))
                    .collect();
                format!("[{}]", parts.join(", "))
            }
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Scalar::Str(s) => json!(s),
            Scalar::List(l) => json!(l),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Scalar::Str(s) => s,
            Scalar::List(_) => "",
        }
    }
}

/// One parsed task-memory file.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskMemory {
    pub name: Scalar,
    pub description: Scalar,
    pub issue_iid: i64,
    pub tags: Vec<String>,
    pub lesson_id: Option<String>,
    pub category: Option<String>,
    pub created_at: Option<String>,
    pub modified: String,
    pub score: i64,
    pub flag: String,
    pub body: String,
    /// Set by [`get_task_memory`] / [`list_task_memories`].
    pub path: Option<String>,
}

impl TaskMemory {
    /// The Python dict, same key order (`path` last, when set).
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "name": self.name.to_value(),
            "description": self.description.to_value(),
            "issue_iid": self.issue_iid,
            "tags": self.tags,
            "lesson_id": self.lesson_id,
            "category": self.category,
            "created_at": self.created_at,
            "modified": self.modified,
            "score": self.score,
            "flag": self.flag,
            "body": self.body,
        });
        if let Some(p) = &self.path {
            v["path"] = json!(p);
        }
        v
    }
}

/// Kebab-case slug from the first 6 alphanumeric ASCII tokens, at most 50
/// chars, never ending in a hyphen; `"note"` if nothing survives.
pub fn slugify(text: &str) -> String {
    let lowered = text.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
        .collect();
    let joined = words.join("-");
    let slug = joined.chars().take(50).collect::<String>();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "note".to_string()
    } else {
        slug.to_string()
    }
}

/// The existing task-memory file for this issue (`<iid>-*.md`, first in name
/// order, never `MEMORY.md`), if any.
fn issue_file(alias: &str, issue_iid: i64, root: Option<&Path>) -> Option<PathBuf> {
    let dir = project_dir(alias, root);
    let prefix = format!("{issue_iid}-");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix) && n.ends_with(".md") && n != "MEMORY.md")
        .collect();
    names.sort();
    names.into_iter().next().map(|n| dir.join(n))
}

fn parse_int(value: &Scalar, default: i64) -> i64 {
    match value {
        Scalar::Str(s) => s.trim().parse().unwrap_or(default),
        Scalar::List(_) => default,
    }
}

/// Parse a task-memory file; `None` when it has no frontmatter or no valid `issue_iid`.
pub fn parse_task_memory(text: &str) -> Option<TaskMemory> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let (header, body) = (&rest[..end], &rest[end + 5..]);
    let mut top: Vec<(String, Scalar)> = Vec::new();
    let mut metadata: Vec<(String, Scalar)> = Vec::new();
    let set = |list: &mut Vec<(String, Scalar)>, k: String, v: Scalar| match list
        .iter_mut()
        .find(|(key, _)| *key == k)
    {
        Some(slot) => slot.1 = v,
        None => list.push((k, v)),
    };
    for line in header.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("  ") {
            let (k, v) = line.trim().split_once(':').unwrap_or((line.trim(), ""));
            set(&mut metadata, k.to_string(), Scalar::parse(v.trim()));
        } else {
            let (k, v) = line.split_once(':').unwrap_or((line, ""));
            if k != "metadata" {
                set(&mut top, k.to_string(), Scalar::parse(v.trim()));
            }
        }
    }
    let meta = |k: &str| metadata.iter().find(|(key, _)| key == k).map(|(_, v)| v);
    let top_get = |k: &str| top.iter().find(|(key, _)| key == k).map(|(_, v)| v);
    let issue_iid = match meta("issue_iid")? {
        Scalar::Str(s) => s.trim().parse::<i64>().ok()?,
        Scalar::List(_) => return None,
    };
    let opt_str = |k: &str| {
        meta(k)
            .map(Scalar::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let tags = match meta("tags") {
        Some(Scalar::List(l)) => l.clone(),
        _ => Vec::new(),
    };
    Some(TaskMemory {
        name: top_get("name")
            .cloned()
            .unwrap_or(Scalar::Str(String::new())),
        description: top_get("description")
            .cloned()
            .unwrap_or(Scalar::Str(String::new())),
        issue_iid,
        tags,
        lesson_id: opt_str("lesson_id"),
        category: opt_str("category"),
        created_at: opt_str("created_at"),
        modified: meta("modified")
            .map(Scalar::as_str)
            .unwrap_or("")
            .to_string(),
        score: meta("score").map_or(0, |s| parse_int(s, 0)),
        flag: meta("flag").map(Scalar::as_str).unwrap_or("").to_string(),
        body: body.trim_matches('\n').to_string(),
        path: None,
    })
}

struct Rendered<'a> {
    name: &'a str,
    description: &'a str,
    issue_iid: i64,
    tags: &'a [String],
    modified: &'a str,
    body: &'a str,
    lesson_id: Option<&'a str>,
    created_at: Option<&'a str>,
    category: Option<&'a str>,
    score: i64,
    flag: &'a str,
}

fn render_task_memory(r: &Rendered) -> String {
    let tags_literal = if r.tags.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", r.tags.join(", "))
    };
    format!(
        "---\nname: {}\ndescription: {}\nmetadata:\n  type: project\n  issue_iid: {}\n  lesson_id: {}\n  category: {}\n  tags: {}\n  created_at: {}\n  modified: {}\n  score: {}\n  flag: {}\n---\n\n{}\n",
        r.name,
        r.description,
        r.issue_iid,
        r.lesson_id.unwrap_or(""),
        r.category.unwrap_or(""),
        tags_literal,
        r.created_at.unwrap_or(""),
        r.modified,
        r.score,
        r.flag,
        r.body,
    )
}

/// Python's `str.splitlines()` line boundaries (beyond `\n`).
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// First line of the trimmed lesson, at most 120 chars, right-trimmed.
fn summary(lesson: &str) -> String {
    let trimmed = lesson.trim();
    let first = trimmed.split(is_line_break).next().unwrap_or("");
    first
        .chars()
        .take(120)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn rewrite_index_line(
    alias: &str,
    filename: &str,
    description: &str,
    issue_iid: i64,
    root: Option<&Path>,
) -> io::Result<()> {
    let index_path = project_dir(alias, root).join("MEMORY.md");
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    let slug = stem.split_once('-').map_or(stem, |(_, rest)| rest);
    let title = format!("#{issue_iid} {}", slug.replace('-', " "));
    let new_line = format!("- [{title}]({filename}) \u{2014} {description}\n");
    let marker = format!("]({filename})");
    let existing = std::fs::read_to_string(&index_path).unwrap_or_default();
    let mut lines: Vec<String> = existing.split_inclusive('\n').map(str::to_string).collect();
    match lines.iter_mut().find(|l| l.contains(&marker)) {
        Some(slot) => *slot = new_line,
        None => lines.push(new_line),
    }
    std::fs::write(index_path, lines.concat())
}

/// Outcome of [`add_task_memory`].
#[derive(Debug, Clone, PartialEq)]
pub struct AddResult {
    pub path: PathBuf,
    pub lesson_id: Option<String>,
    pub created: bool,
}

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Create or append to `<issue_iid>`'s task-memory file for `alias` and update
/// the project's `MEMORY.md` index. A fresh `lesson_id`/`created_at` is only
/// generated for a brand-new file or one whose frontmatter does not parse; on a
/// later append the existing identity, category, score and flag are preserved.
pub fn add_task_memory(
    alias: &str,
    issue_iid: i64,
    lesson: &str,
    tags: &[String],
    category: Option<&str>,
    root: Option<&Path>,
) -> io::Result<AddResult> {
    let dir = project_dir(alias, root);
    std::fs::create_dir_all(&dir)?;
    let now = now_utc();
    let today = &now[..10];
    let description = summary(lesson);
    let mut tags: Vec<String> = tags.to_vec();
    let mut category: Option<String> = category.map(str::to_string);
    let mut score = 0;
    let mut flag = String::new();
    let (path, name, body, lesson_id, created_at, created);
    match issue_file(alias, issue_iid, root) {
        None => {
            let slug = slugify(lesson);
            path = dir.join(format!("{issue_iid}-{slug}.md"));
            name = format!("{issue_iid}-{slug}");
            body = format!("Recorded {today}: {}", lesson.trim());
            lesson_id = Some(format!("lesson_{}", crate::events::new_uuid_hex()));
            created_at = Some(now.clone());
            created = true;
        }
        Some(existing) => {
            let text = std::fs::read_to_string(&existing)?;
            match parse_task_memory(&text) {
                None => {
                    name = existing
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    body = format!(
                        "{}\n\nRecorded {today}: {}",
                        text.trim_end_matches('\n'),
                        lesson.trim()
                    );
                    lesson_id = Some(format!("lesson_{}", crate::events::new_uuid_hex()));
                    created_at = Some(now.clone());
                    created = true;
                }
                Some(parsed) => {
                    name = parsed.name.render();
                    let merged: BTreeSet<String> = parsed
                        .tags
                        .iter()
                        .cloned()
                        .chain(tags.iter().cloned())
                        .collect();
                    tags = merged.into_iter().collect();
                    body = format!("{}\n\nRecorded {today}: {}", parsed.body, lesson.trim());
                    lesson_id = parsed.lesson_id;
                    created_at = parsed.created_at;
                    category = parsed.category;
                    score = parsed.score;
                    flag = parsed.flag;
                    created = false;
                }
            }
            path = existing;
        }
    }
    std::fs::write(
        &path,
        render_task_memory(&Rendered {
            name: &name,
            description: &description,
            issue_iid,
            tags: &tags,
            modified: &now,
            body: &body,
            lesson_id: lesson_id.as_deref(),
            created_at: created_at.as_deref(),
            category: category.as_deref(),
            score,
            flag: &flag,
        }),
    )?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    rewrite_index_line(alias, &file_name, &description, issue_iid, root)?;
    Ok(AddResult {
        path,
        lesson_id,
        created,
    })
}

/// Set the score/flag of the lesson with this `lesson_id` (searched in `alias`'s
/// directory, or every project directory when `None`). Only those two fields
/// change; the file is replaced atomically. Returns whether it was found.
pub fn set_score(
    lesson_id: &str,
    score: i64,
    flag: &str,
    alias: Option<&str>,
    root: Option<&Path>,
) -> io::Result<bool> {
    let root = resolve_root(root);
    let dirs: Vec<PathBuf> = match alias {
        Some(a) => vec![root.join(a)],
        None => sorted_subdirs(&root),
    };
    for dir in dirs {
        for path in sorted_md_files(&dir) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(parsed) = parse_task_memory(&text) else {
                continue;
            };
            if parsed.lesson_id.as_deref() != Some(lesson_id) {
                continue;
            }
            let rendered = render_task_memory(&Rendered {
                name: &parsed.name.render(),
                description: &parsed.description.render(),
                issue_iid: parsed.issue_iid,
                tags: &parsed.tags,
                modified: &parsed.modified,
                body: &parsed.body,
                lesson_id: parsed.lesson_id.as_deref(),
                created_at: parsed.created_at.as_deref(),
                category: parsed.category.as_deref(),
                score,
                flag,
            });
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let tmp = path.with_file_name(format!(
                ".{file_name}.{}.tmp",
                &crate::events::new_uuid_hex()[..8]
            ));
            std::fs::write(&tmp, rendered)?;
            let renamed = std::fs::rename(&tmp, &path);
            let _ = std::fs::remove_file(&tmp);
            renamed?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn sorted_subdirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn sorted_md_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "md")
                        && p.file_name().is_some_and(|n| n != "MEMORY.md")
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// Every alias directory name under the memory root, sorted.
pub fn all_aliases(root: Option<&Path>) -> Vec<String> {
    sorted_subdirs(&resolve_root(root))
        .iter()
        .filter_map(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

/// The parsed task memory for an issue, or `None` when nothing parseable is recorded.
pub fn get_task_memory(alias: &str, issue_iid: i64, root: Option<&Path>) -> Option<TaskMemory> {
    let path = issue_file(alias, issue_iid, root)?;
    let mut parsed = parse_task_memory(&std::fs::read_to_string(&path).ok()?)?;
    parsed.path = Some(path.to_string_lossy().into_owned());
    Some(parsed)
}

/// Every parseable task memory for `alias`, newest `modified` first.
pub fn list_task_memories(alias: &str, root: Option<&Path>) -> Vec<TaskMemory> {
    let mut entries: Vec<TaskMemory> = sorted_md_files(&project_dir(alias, root))
        .into_iter()
        .filter_map(|path| {
            let mut parsed = parse_task_memory(&std::fs::read_to_string(&path).ok()?)?;
            parsed.path = Some(path.to_string_lossy().into_owned());
            Some(parsed)
        })
        .collect();
    entries.sort_by(|a, b| b.modified.cmp(&a.modified));
    entries
}

/// The raw `MEMORY.md` index, or `None` when absent.
pub fn read_index(alias: &str, root: Option<&Path>) -> Option<String> {
    std::fs::read_to_string(project_dir(alias, root).join("MEMORY.md")).ok()
}

/// `json.dumps(entry, indent=2)` of a task memory (or `null`).
pub fn task_memory_json(entry: Option<&TaskMemory>) -> String {
    pyjson::to_string_pretty(&entry.map_or(Value::Null, TaskMemory::to_value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn task_files(dir: &Path) -> Vec<PathBuf> {
        sorted_md_files(dir)
    }

    #[test]
    fn creates_index_and_task_file() {
        let d = tempfile::tempdir().unwrap();
        add_task_memory(
            "harbor",
            142,
            "RSpec db-cleaner leaves stale rows under parallel runs",
            &tags(&["flaky-test"]),
            None,
            Some(d.path()),
        )
        .unwrap();
        let files = task_files(&d.path().join("harbor"));
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("142-"));
        let content = std::fs::read_to_string(&files[0]).unwrap();
        assert!(content.contains("issue_iid: 142"));
        assert!(content.contains("tags: [flaky-test]"));
        let index = read_index("harbor", Some(d.path())).unwrap();
        assert!(index.contains(&format!("]({name})")));
        assert!(index.contains("RSpec db-cleaner leaves stale rows"));
    }

    #[test]
    fn append_keeps_one_file_merges_tags_and_updates_index() {
        let d = tempfile::tempdir().unwrap();
        let root = Some(d.path());
        add_task_memory(
            "harbor",
            142,
            "first lesson on this issue",
            &tags(&["flaky-test"]),
            None,
            root,
        )
        .unwrap();
        let second = add_task_memory(
            "harbor",
            142,
            "second lesson on this issue",
            &tags(&["ci"]),
            None,
            root,
        )
        .unwrap();
        assert!(!second.created);
        let files = task_files(&d.path().join("harbor"));
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name().unwrap().to_string_lossy(),
            format!("142-{}.md", slugify("first lesson on this issue"))
        );
        let content = std::fs::read_to_string(&files[0]).unwrap();
        assert!(content.contains("first lesson on this issue"));
        assert!(content.contains("second lesson on this issue"));
        assert!(content.contains("tags: [ci, flaky-test]"));
        let index = read_index("harbor", root).unwrap();
        let matching: Vec<_> = index
            .lines()
            .filter(|l| l.contains("142-first-lesson"))
            .collect();
        assert_eq!(matching.len(), 1);
        assert!(matching[0].contains("second lesson on this issue"));
    }

    #[test]
    fn exact_file_format() {
        let d = tempfile::tempdir().unwrap();
        let r = add_task_memory(
            "a",
            7,
            "Use bundle exec rubocop .",
            &[],
            Some("lint"),
            Some(d.path()),
        )
        .unwrap();
        let text = std::fs::read_to_string(&r.path).unwrap();
        let lid = r.lesson_id.clone().unwrap();
        assert!(lid.starts_with("lesson_") && lid.len() == 7 + 32);
        assert!(text.starts_with(&format!(
            "---\nname: 7-use-bundle-exec-rubocop\ndescription: Use bundle exec rubocop .\nmetadata:\n  type: project\n  issue_iid: 7\n  lesson_id: {lid}\n  category: lint\n  tags: []\n  created_at: "
        )));
        assert!(text.contains("\n  flag: \n---\n\nRecorded "));
        assert!(text.ends_with(": Use bundle exec rubocop .\n"));
        let index = read_index("a", Some(d.path())).unwrap();
        assert_eq!(
            index,
            "- [#7 use bundle exec rubocop](7-use-bundle-exec-rubocop.md) \u{2014} Use bundle exec rubocop .\n"
        );
    }

    #[test]
    fn get_and_list() {
        let d = tempfile::tempdir().unwrap();
        let root = Some(d.path());
        assert!(get_task_memory("harbor", 1, root).is_none());
        assert!(list_task_memories("nope", root).is_empty());
        assert!(read_index("nope", root).is_none());
        add_task_memory("harbor", 1, "one", &[], None, root).unwrap();
        add_task_memory("harbor", 2, "two", &tags(&["x"]), Some("c"), root).unwrap();
        let e = get_task_memory("harbor", 2, root).unwrap();
        assert_eq!(
            (e.issue_iid, e.tags.clone(), e.category.clone()),
            (2, tags(&["x"]), Some("c".into()))
        );
        assert!(e.path.is_some());
        assert_eq!(list_task_memories("harbor", root).len(), 2);
        assert_eq!(all_aliases(root), vec!["harbor"]);
    }

    #[test]
    fn skips_files_without_valid_issue_iid() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("harbor");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1-a.md"),
            "---\nname: a\nmetadata:\n  type: project\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("2-b.md"),
            "---\nname: b\nmetadata:\n  issue_iid: abc\n---\n\nbody\n",
        )
        .unwrap();
        assert!(list_task_memories("harbor", Some(d.path())).is_empty());
        assert!(get_task_memory("harbor", 1, Some(d.path())).is_none());
    }

    #[test]
    fn unstructured_legacy_file_is_preserved_and_gets_fresh_identity() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("harbor");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("5-legacy.md"), "old notes with no frontmatter\n").unwrap();
        let r = add_task_memory("harbor", 5, "new insight", &[], None, Some(d.path())).unwrap();
        assert!(r.created && r.lesson_id.is_some());
        let text = std::fs::read_to_string(&r.path).unwrap();
        assert!(text.contains("old notes with no frontmatter\n\nRecorded "));
        assert!(text.contains("name: 5-legacy\n"));
    }

    #[test]
    fn identity_preserved_on_append_and_pre_sprint_6_never_backfilled() {
        let d = tempfile::tempdir().unwrap();
        let root = Some(d.path());
        let first = add_task_memory("h", 1, "a", &[], Some("cat"), root).unwrap();
        let before = get_task_memory("h", 1, root).unwrap();
        let second = add_task_memory("h", 1, "b", &[], Some("ignored"), root).unwrap();
        let after = get_task_memory("h", 1, root).unwrap();
        assert_eq!(second.lesson_id, first.lesson_id);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.category.as_deref(), Some("cat"));

        let dir = d.path().join("old");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("3-x.md"),
            "---\nname: 3-x\ndescription: d\nmetadata:\n  type: project\n  issue_iid: 3\n  tags: []\n  modified: 2026-01-01\n---\n\nbody\n",
        )
        .unwrap();
        let r = add_task_memory("old", 3, "more", &[], None, root).unwrap();
        assert!(!r.created && r.lesson_id.is_none());
        assert!(get_task_memory("old", 3, root).unwrap().lesson_id.is_none());
    }

    #[test]
    fn score_and_flag_roundtrip_default_and_survive_append() {
        let d = tempfile::tempdir().unwrap();
        let root = Some(d.path());
        let r = add_task_memory("web", 7, "Use bundle exec rubocop .", &[], None, root).unwrap();
        let lid = r.lesson_id.unwrap();
        assert!(set_score(&lid, -2, "needs_review", Some("web"), root).unwrap());
        let m = get_task_memory("web", 7, root).unwrap();
        assert_eq!((m.score, m.flag.as_str()), (-2, "needs_review"));
        add_task_memory("web", 7, "more", &[], None, root).unwrap();
        let m = get_task_memory("web", 7, root).unwrap();
        assert_eq!((m.score, m.flag.as_str()), (-2, "needs_review"));
        assert!(set_score(&lid, 1, "", None, root).unwrap());
        assert!(!set_score("lesson_missing", 1, "", None, root).unwrap());

        let r = add_task_memory("web", 8, "x", &[], None, root).unwrap();
        let text = std::fs::read_to_string(&r.path)
            .unwrap()
            .replace("  score: 0\n", "")
            .replace("  flag: \n", "");
        std::fs::write(&r.path, text).unwrap();
        let m = get_task_memory("web", 8, root).unwrap();
        assert_eq!((m.score, m.flag.as_str()), (0, ""));
    }

    #[test]
    fn set_score_leaves_no_temp_files_and_keeps_modified() {
        let d = tempfile::tempdir().unwrap();
        let root = Some(d.path());
        let r = add_task_memory("web", 9, "a", &[], None, root).unwrap();
        let before = get_task_memory("web", 9, root).unwrap();
        set_score(&r.lesson_id.unwrap(), 3, "", Some("web"), root).unwrap();
        let after = get_task_memory("web", 9, root).unwrap();
        assert_eq!(after.modified, before.modified);
        let leftovers = std::fs::read_dir(d.path().join("web")).unwrap().count();
        assert_eq!(leftovers, 2); // the task file + MEMORY.md
    }

    #[test]
    fn slugify_is_filesystem_safe_and_bounded() {
        assert_eq!(slugify("Hello, World! It's 2026"), "hello-world-it-s-2026");
        assert_eq!(slugify("日本語"), "note");
        assert_eq!(
            slugify("one two three four five six seven eight"),
            "one-two-three-four-five-six"
        );
        let long = slugify(&format!("{} b", "a".repeat(60)));
        assert_eq!(long.len(), 50);
        assert_eq!(
            slugify("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-b"),
            "a".repeat(50)
        );
    }

    #[test]
    fn summary_takes_first_line_capped() {
        assert_eq!(summary("  first line\nsecond  "), "first line");
        assert_eq!(summary("x".repeat(200).as_str()).len(), 120);
        assert_eq!(summary(""), "");
    }

    #[test]
    fn uuid_hex_shape() {
        let a = crate::events::new_uuid_hex();
        assert_eq!(a.len(), 32);
        assert_eq!(&a[12..13], "4");
        assert_ne!(a, crate::events::new_uuid_hex());
    }
}

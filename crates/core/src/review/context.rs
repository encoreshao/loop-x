//! The repository's own guidance for a review: `path_instructions` that match the
//! changed files, agent guideline files (`CLAUDE.md`, `AGENTS.md`, ...) at the root and
//! in the nearest folders of the changed files, and the project's personal
//! `custom_instructions`. Files are read through `read` (the MR's base commit), so an
//! MR cannot rewrite the guidance used to review it. Everything is size-capped and
//! goes to the model as untrusted data.

use super::glob;
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const MAX_ITEM_CHARS: usize = 8_000;
pub const MAX_TOTAL_CHARS: usize = 40_000;
const ROOT_FILES: [&str; 4] = [
    "CLAUDE.md",
    "AGENTS.md",
    ".cursorrules",
    ".github/copilot-instructions.md",
];
const NESTED_FILES: [&str; 2] = ["CLAUDE.md", "AGENTS.md"];
const MAX_NESTED_DIRS: usize = 5;

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathInstruction {
    pub path: String,
    pub instructions: String,
}

/// Folders holding changed files, deepest first, at most [`MAX_NESTED_DIRS`].
fn nested_dirs(changed: &[String]) -> Vec<String> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for p in changed {
        let mut cur = p.as_str();
        while let Some((parent, _)) = cur.rsplit_once('/') {
            if !parent.is_empty() {
                dirs.insert(parent.to_string());
            }
            cur = parent;
        }
    }
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| (std::cmp::Reverse(d.matches('/').count()), d.clone()));
    dirs.truncate(MAX_NESTED_DIRS);
    dirs
}

fn capped(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= MAX_ITEM_CHARS {
        return t.to_string();
    }
    let mut out: String = t.chars().take(MAX_ITEM_CHARS).collect();
    out.push_str(&format!("\n[truncated at {MAX_ITEM_CHARS} characters]"));
    out
}

/// `repo_guidance` entries, most specific first: `{source, applies_to, text}`.
pub fn gather(
    changed: &[String],
    path_instructions: &[PathInstruction],
    custom_instructions: &str,
    read: &mut dyn FnMut(&str) -> Option<String>,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for pi in path_instructions {
        let hits: Vec<&String> = changed
            .iter()
            .filter(|p| glob::matches(&pi.path, p))
            .collect();
        if !hits.is_empty() && !pi.instructions.trim().is_empty() {
            out.push(json!({"source": format!("path_instructions: {}", pi.path),
                            "applies_to": hits, "text": capped(&pi.instructions)}));
        }
    }
    for dir in nested_dirs(changed) {
        let prefix = format!("{dir}/");
        let under: Vec<&String> = changed.iter().filter(|p| p.starts_with(&prefix)).collect();
        for name in NESTED_FILES {
            let path = format!("{dir}/{name}");
            if let Some(text) = read(&path).filter(|t| !t.trim().is_empty()) {
                out.push(json!({"source": path, "applies_to": under, "text": capped(&text)}));
            }
        }
    }
    for name in ROOT_FILES {
        if let Some(text) = read(name).filter(|t| !t.trim().is_empty()) {
            out.push(json!({"source": name, "applies_to": ["*"], "text": capped(&text)}));
        }
    }
    if !custom_instructions.trim().is_empty() {
        out.push(
            json!({"source": "reviewer's project instructions", "applies_to": ["*"],
                        "text": capped(custom_instructions)}),
        );
    }
    let mut total = 0usize;
    out.retain(|g| {
        total += g["text"].as_str().map_or(0, |t| t.chars().count());
        total <= MAX_TOTAL_CHARS
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    type Asked = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    fn reader(files: &[(&str, &str)]) -> (impl FnMut(&str) -> Option<String>, Asked) {
        let map: BTreeMap<String, String> = files
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = asked.clone();
        (
            move |p: &str| {
                log.borrow_mut().push(p.to_string());
                map.get(p).cloned()
            },
            asked,
        )
    }

    #[test]
    fn most_specific_guidance_first_with_scopes() {
        let changed = vec![
            "app/models/user.rb".to_string(),
            "db/migrate/1_x.rb".to_string(),
        ];
        let (mut read, asked) = reader(&[
            ("CLAUDE.md", "Root rules"),
            ("app/models/AGENTS.md", "Model rules"),
            (".cursorrules", "  "),
        ]);
        let pis = vec![
            PathInstruction {
                path: "db/migrate/**".into(),
                instructions: "Set lock_timeout.".into(),
            },
            PathInstruction {
                path: "lib/**".into(),
                instructions: "unused".into(),
            },
        ];
        let g = gather(&changed, &pis, "Prefer service objects.", &mut read);
        let sources: Vec<&str> = g.iter().map(|x| x["source"].as_str().unwrap()).collect();
        assert_eq!(
            sources,
            [
                "path_instructions: db/migrate/**",
                "app/models/AGENTS.md",
                "CLAUDE.md",
                "reviewer's project instructions"
            ]
        );
        assert_eq!(g[0]["applies_to"], json!(["db/migrate/1_x.rb"]));
        assert_eq!(g[1]["applies_to"], json!(["app/models/user.rb"]));
        assert_eq!(g[2]["applies_to"], json!(["*"]));
        let asked = asked.borrow();
        assert!(asked.contains(&"app/models/CLAUDE.md".to_string()));
        assert!(asked.contains(&"db/migrate/AGENTS.md".to_string()));
        assert!(!asked.iter().any(|p| p.contains("review.yaml")));
    }

    #[test]
    fn items_and_the_total_are_capped() {
        let big = "x".repeat(MAX_ITEM_CHARS + 50);
        let (mut read, _) = reader(&[("CLAUDE.md", &big)]);
        let g = gather(&["a.rb".to_string()], &[], "", &mut read);
        assert!(g[0]["text"]
            .as_str()
            .unwrap()
            .ends_with(&format!("[truncated at {MAX_ITEM_CHARS} characters]")));
        let pis: Vec<PathInstruction> = (0..8)
            .map(|i| PathInstruction {
                path: format!("*{i}*"),
                instructions: "y".repeat(MAX_ITEM_CHARS),
            })
            .collect();
        let changed: Vec<String> = (0..8).map(|i| format!("f{i}.rb")).collect();
        let (mut none, _) = reader(&[]);
        let g = gather(&changed, &pis, "", &mut none);
        assert_eq!(g.len(), MAX_TOTAL_CHARS / MAX_ITEM_CHARS);
    }

    #[test]
    fn nested_dirs_are_deepest_first_and_capped() {
        let changed: Vec<String> = ["a/b/c/d/e/f/g.rb", "z.rb"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            nested_dirs(&changed),
            ["a/b/c/d/e/f", "a/b/c/d/e", "a/b/c/d", "a/b/c", "a/b"]
        );
    }
}

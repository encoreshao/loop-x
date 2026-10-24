//! `docs.save`: the chat saves a markdown document (summary, report) the user
//! can open from the reply. A local write only, so it needs no confirmation
//! card. Documents live in `<repo_root>/outputs/chat-docs/`.

use super::{req_str, Arg, Env, Handler, Mode, Tool, ToolError, ToolResult};
use crate::connectors::Connector;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

static SAVE_ARGS: [Arg; 2] = [Arg::req("title"), Arg::req("content")];

/// `<repo_root>/outputs/chat-docs`.
pub fn docs_dir(env: &Env) -> PathBuf {
    env.repo_root.join("outputs").join("chat-docs")
}

/// `name` when it is a generated document name (`^[a-z0-9][a-z0-9-]{0,80}\.md$`).
pub fn safe_doc_name(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".md")?;
    let mut chars = stem.chars();
    let first = chars.next()?;
    let ok = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && stem.chars().count() <= 81
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then_some(name)
}

/// The text of a saved document, `None` for an unsafe or missing name.
pub fn read_doc(env: &Env, name: &str) -> Option<String> {
    let name = safe_doc_name(name)?;
    fs::read_to_string(docs_dir(env).join(name)).ok()
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    let slug: String = out.chars().take(60).collect();
    if slug.is_empty() {
        "document".into()
    } else {
        slug
    }
}

fn save(env: &Env, _c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let io = |_: std::io::Error| ToolError::Other("OSError".into());
    let dir = docs_dir(env);
    fs::create_dir_all(&dir).map_err(io)?;
    let slug = slug(&req_str(args, "title")?);
    let content = req_str(args, "content")?;
    let mut n = 1;
    let mut name = format!("{slug}.md");
    loop {
        while dir.join(&name).exists() {
            n += 1;
            name = format!("{slug}-{n}.md");
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
        {
            Ok(mut f) => {
                f.write_all(content.as_bytes()).map_err(io)?;
                break;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        }
    }
    Ok(json!({"name": name, "url": format!("/chat-docs/{name}")}))
}

pub(crate) fn tools() -> Vec<Tool> {
    let handler: Handler = save;
    vec![Tool {
        name: "docs.save",
        description: "Save a markdown document (e.g. a summary) and get a link the user can open",
        args: &SAVE_ARGS,
        mode: Mode::Local,
        connector_type: None,
        handler,
        select: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    fn env(tmp: &std::path::Path) -> Env {
        Env::new(tmp.join("repo"), tmp.join("home"))
    }

    #[test]
    fn save_writes_markdown_and_returns_a_download_url() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let r = run(
            &e,
            "docs.save",
            &json!({"title": "Issue #482 summary", "content": "# Summary\nhello"}).to_string(),
            "",
        );
        let name = r["result"]["name"].as_str().unwrap();
        assert_eq!(name, "issue-482-summary.md");
        assert_eq!(r["result"]["url"], format!("/chat-docs/{name}"));
        assert_eq!(
            fs::read_to_string(docs_dir(&e).join(name)).unwrap(),
            "# Summary\nhello"
        );
        assert_eq!(read_doc(&e, name).unwrap(), "# Summary\nhello");
        assert_eq!(read_doc(&e, "../x.md"), None);
    }

    #[test]
    fn save_never_overwrites_an_existing_document() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let args = json!({"title": "Same", "content": "one"}).to_string();
        let first = run(&e, "docs.save", &args, "")["result"]["name"].clone();
        let second = run(&e, "docs.save", &args, "")["result"]["name"].clone();
        let third = run(&e, "docs.save", &args, "")["result"]["name"].clone();
        assert_eq!(
            (first, second, third),
            ("same.md".into(), "same-2.md".into(), "same-3.md".into())
        );
        assert_eq!(fs::read_dir(docs_dir(&e)).unwrap().count(), 3);
    }

    #[test]
    fn title_with_path_tricks_cannot_escape_the_docs_dir() {
        let d = tempfile::tempdir().unwrap();
        let e = env(d.path());
        let r = run(
            &e,
            "docs.save",
            &json!({"title": "../../etc/passwd", "content": "x"}).to_string(),
            "",
        );
        let name = r["result"]["name"].as_str().unwrap();
        assert_eq!(name, "etc-passwd.md");
        assert!(docs_dir(&e).join(name).exists());
        assert!(!d.path().join("repo/etc").exists() && !d.path().join("etc").exists());
    }

    #[test]
    fn slugs_follow_python() {
        assert_eq!(slug("  Hello, World!  "), "hello-world");
        assert_eq!(slug("!!!"), "document");
        assert_eq!(slug("éé"), "document");
        assert_eq!(slug(&"a".repeat(100)), "a".repeat(60));
        // truncation happens after stripping, so a trailing dash can remain
        let t = format!("{} b", "a".repeat(59));
        assert_eq!(slug(&t), format!("{}-", "a".repeat(59)));
    }

    #[test]
    fn safe_doc_name_accepts_only_generated_names() {
        assert_eq!(safe_doc_name("a-b-1.md"), Some("a-b-1.md"));
        for bad in [
            "../x.md",
            "a/b.md",
            "A.md",
            "a.txt",
            ".md",
            &format!("{}.md", "a".repeat(90)),
        ] {
            assert_eq!(safe_doc_name(bad), None, "{bad}");
        }
        assert!(safe_doc_name(&format!("{}.md", "a".repeat(81))).is_some());
        assert!(safe_doc_name(&format!("{}.md", "a".repeat(82))).is_none());
        assert_eq!(safe_doc_name("-a.md"), None);
    }
}

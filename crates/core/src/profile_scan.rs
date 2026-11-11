//! Deterministic, read-only inputs for building a project profile: a scan of
//! the work dir ([`scan_repo`]) and a digest of the cached GitLab issues
//! ([`digest_issues`]). No model runs here and nothing is ever written.
//!
//! Cached issues live in the gitlab-config cache, one JSON file per issue at
//! `<instance dir>/projects/<project_id with '/' as '__'>/issues/<iid>.json`
//! (see [`crate::gitlab_cache::issue_path`]; the instance dir is
//! `~/.gitlab/cache/<instance>` or `data/<provider>/<account>` for connected
//! accounts). Task memories come from [`crate::memory_store`].
//!
//! Secrets: env and config files are read only to collect KEY NAMES. No value
//! from them is stored in a [`Scan`], and manifest lines that look like they
//! carry a secret are replaced by a placeholder.

use crate::gitlab_cache;
use crate::memory_store;
use crate::project_profile::{contains_secret, Source};
use crate::sha256;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

const MAX_DEPTH: usize = 3;
const MAX_READ: u64 = 64 * 1024;
const MAX_TREE: usize = 500;
const SKIP_DIRS: [&str; 5] = [".git", "node_modules", "target", "vendor", ".venv"];
const MANIFEST_NAMES: [&str; 16] = [
    "Cargo.toml",
    "package.json",
    "Gemfile",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "composer.json",
    "Makefile",
    "Dockerfile",
    "Package.swift",
    "pom.xml",
    "build.gradle",
    "mix.exs",
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yaml",
];
const ENTRY_NAMES: [&str; 10] = [
    "main.rs",
    "main.go",
    "main.py",
    "manage.py",
    "app.py",
    "index.js",
    "index.ts",
    "server.js",
    "config.ru",
    "routes.rb",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub name: String,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiblingRef {
    pub target: String,
    /// `file:line`
    pub evidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub tree: Vec<String>,
    /// `(relative path, text)`, each capped at 64 KiB, secret-looking lines redacted.
    pub manifests: Vec<(String, String)>,
    pub entry_points: Vec<String>,
    pub ci: Vec<String>,
    pub services: Vec<String>,
    pub secret_names: Vec<SecretRef>,
    pub sibling_refs: Vec<SiblingRef>,
    pub sources: Vec<Source>,
    pub head_sha: Option<String>,
}

fn is_ci(rel: &str) -> bool {
    rel == ".gitlab-ci.yml"
        || rel == "Jenkinsfile"
        || rel == ".circleci/config.yml"
        || (rel.starts_with(".github/workflows/")
            && (rel.ends_with(".yml") || rel.ends_with(".yaml")))
}

fn is_compose(name: &str) -> bool {
    matches!(
        name,
        "docker-compose.yml" | "docker-compose.yaml" | "compose.yaml" | "compose.yml"
    )
}

fn is_env_file(name: &str) -> bool {
    name.starts_with(".env") || name.ends_with(".env")
}

fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<(String, bool)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if ft.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            out.push((path.clone(), true));
            if depth < MAX_DEPTH {
                walk(&e.path(), &path, depth + 1, out);
            }
        } else if ft.is_file() {
            out.push((path, false));
        }
    }
}

fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(MAX_READ).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn redact(text: &str) -> String {
    text.lines()
        .map(|l| {
            if contains_secret(l).is_some() {
                "[redacted line]"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn env_key_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if line.starts_with('#') {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut chars = key.chars();
        let ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if ok && !names.iter().any(|n| n == key) {
            names.push(key.to_string());
        }
    }
    names
}

fn compose_services(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_services = false;
    let mut indent: Option<usize> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let lead = line.len() - line.trim_start().len();
        if lead == 0 {
            in_services = line.trim_end() == "services:";
            indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        let ind = *indent.get_or_insert(lead);
        if lead == ind {
            if let Some(name) = line.trim().strip_suffix(':') {
                if !name.is_empty() && !name.contains(' ') {
                    out.push(name.trim_matches(['"', '\'']).to_string());
                }
            }
        }
    }
    out
}

fn last_component(path: &str) -> Option<String> {
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(|c| c.trim_end_matches(".git").to_string())
        .filter(|c| !c.is_empty())
}

fn escapes_repo(path: &str) -> bool {
    path.split('/').any(|c| c == "..")
}

fn path_dependency_siblings(file: &str, text: &str, out: &mut Vec<SiblingRef>) {
    let name = file.rsplit('/').next().unwrap_or(file);
    let re = match name {
        "Cargo.toml" => Some(r#"path\s*=\s*"([^"]+)""#),
        "package.json" => Some(r#""(?:file|link):([^"]+)""#),
        "Gemfile" => Some(r#"path:\s*['"]([^'"]+)['"]"#),
        _ => None,
    };
    let Some(re) = re else { return };
    let re = Regex::new(re).expect("static regex");
    for (i, line) in text.lines().enumerate() {
        for cap in re.captures_iter(line) {
            let p = &cap[1];
            if escapes_repo(p) {
                if let Some(target) = last_component(p) {
                    out.push(SiblingRef {
                        target,
                        evidence: format!("{file}:{}", i + 1),
                    });
                }
            }
        }
    }
}

fn head_sha(work_dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Scans `work_dir` (depth 3, read-only). `Err` when it is missing or empty.
pub fn scan_repo(work_dir: &Path, tracked_aliases: &[String]) -> Result<Scan, String> {
    if !work_dir.is_dir() {
        return Err(format!("{} is not a directory", work_dir.display()));
    }
    let mut found = Vec::new();
    walk(work_dir, "", 1, &mut found);
    if std::fs::read_dir(work_dir)
        .map(|mut r| r.next().is_none())
        .unwrap_or(true)
    {
        return Err(format!("{} is empty", work_dir.display()));
    }

    let mut scan = Scan {
        head_sha: head_sha(work_dir),
        ..Scan::default()
    };
    let mut siblings: Vec<SiblingRef> = Vec::new();
    let alias_res: Vec<(String, Regex)> = tracked_aliases
        .iter()
        .filter(|a| !a.is_empty())
        .filter_map(|a| {
            Regex::new(&format!(r"\b{}\b", regex::escape(a)))
                .ok()
                .map(|r| (a.clone(), r))
        })
        .collect();

    for (rel, is_dir) in &found {
        if scan.tree.len() < MAX_TREE {
            scan.tree.push(if *is_dir {
                format!("{rel}/")
            } else {
                rel.clone()
            });
        }
        if *is_dir {
            continue;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let full = work_dir.join(rel);

        if ENTRY_NAMES.contains(&name) || rel.starts_with("bin/") && rel.matches('/').count() == 1 {
            scan.entry_points.push(rel.clone());
        }
        if is_env_file(name) {
            // Key names only; the content is never kept.
            if let Some(text) = read_capped(&full) {
                for n in env_key_names(&text) {
                    scan.secret_names.push(SecretRef {
                        name: n,
                        location: rel.clone(),
                    });
                }
            }
            continue;
        }
        let ci = is_ci(rel);
        let manifest = MANIFEST_NAMES.contains(&name);
        if rel == ".gitmodules" {
            if let Some(text) = read_capped(&full) {
                for (i, line) in text.lines().enumerate() {
                    if let Some(url) = line.trim().strip_prefix("url") {
                        let url = url.trim_start().trim_start_matches('=').trim();
                        if let Some(target) = last_component(url.rsplit(':').next().unwrap_or(url))
                        {
                            siblings.push(SiblingRef {
                                target,
                                evidence: format!(".gitmodules:{}", i + 1),
                            });
                        }
                    }
                }
            }
        }
        if !ci && !manifest {
            continue;
        }
        let Some(text) = read_capped(&full) else {
            continue;
        };
        if ci {
            scan.ci.push(rel.clone());
        }
        if is_compose(name) {
            for s in compose_services(&text) {
                if !scan.services.contains(&s) {
                    scan.services.push(s);
                }
            }
        }
        path_dependency_siblings(rel, &text, &mut siblings);
        for (i, line) in text.lines().enumerate() {
            for (alias, re) in &alias_res {
                if re.is_match(line) {
                    siblings.push(SiblingRef {
                        target: alias.clone(),
                        evidence: format!("{rel}:{}", i + 1),
                    });
                }
            }
        }
        if let Ok(bytes) = std::fs::read(&full) {
            scan.sources.push(Source {
                path: rel.clone(),
                sha256: sha256::hex(&sha256::sha256(&bytes)),
            });
        }
        if manifest {
            scan.manifests.push((rel.clone(), redact(&text)));
        }
    }
    for s in siblings {
        if !scan.sibling_refs.contains(&s) {
            scan.sibling_refs.push(s);
        }
    }
    Ok(scan)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossRef {
    pub issue_iid: i64,
    pub target_project: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueDigest {
    /// Labels (and task-memory tags) with counts, most frequent first.
    pub areas: Vec<(String, usize)>,
    /// Authors and assignees with counts, most frequent first.
    pub people: Vec<(String, usize)>,
    /// `(iid, title)`, most recently updated first, at most 10.
    pub recent: Vec<(i64, String)>,
    /// Issues mentioning `group/project#N` or `group/project!N` of another project.
    pub cross_refs: Vec<CrossRef>,
}

fn ranked(counts: BTreeMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn username(v: &Value) -> Option<String> {
    v.get("username")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Digest of the cached issues of one project (layout in the module doc) and
/// the alias's task memories. Nothing cached reads as empty fields.
pub fn digest_issues(instance: &str, project_id: &str, alias: &str) -> IssueDigest {
    let issues_dir = gitlab_cache::project_path(instance, project_id)
        .parent()
        .map(|p| p.join("issues"))
        .unwrap_or_default();
    digest_from(&issues_dir, project_id, alias, None)
}

fn digest_from(
    issues_dir: &Path,
    project_id: &str,
    alias: &str,
    memory_root: Option<&Path>,
) -> IssueDigest {
    let mut areas: BTreeMap<String, usize> = BTreeMap::new();
    let mut people: BTreeMap<String, usize> = BTreeMap::new();
    let mut recent: Vec<(String, i64, String)> = Vec::new();
    let mut cross = Vec::new();
    let re =
        Regex::new(r"\b([A-Za-z0-9_.\-]+(?:/[A-Za-z0-9_.\-]+)+)[#!]\d+").expect("static regex");

    let mut files: Vec<_> = std::fs::read_dir(issues_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    for path in files {
        let issue = gitlab_cache::read_json(&path);
        let Some(iid) = issue.get("iid").and_then(Value::as_i64) else {
            continue;
        };
        for l in issue
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = l
                .as_str()
                .or_else(|| l.get("name").and_then(Value::as_str))
                .unwrap_or("");
            if !name.is_empty() {
                *areas.entry(name.to_string()).or_default() += 1;
            }
        }
        let mut who: Vec<String> = Vec::new();
        who.extend(issue.get("author").and_then(username));
        for a in issue
            .get("assignees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            who.extend(username(a));
        }
        if who.is_empty() {
            who.extend(issue.get("assignee").and_then(username));
        }
        who.sort();
        who.dedup();
        for w in who {
            *people.entry(w).or_default() += 1;
        }
        let title = issue.get("title").and_then(Value::as_str).unwrap_or("");
        let updated = issue
            .get("updated_at")
            .or_else(|| issue.get("created_at"))
            .and_then(Value::as_str)
            .unwrap_or("");
        recent.push((updated.to_string(), iid, title.to_string()));

        let mut text = issue
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        for n in issue
            .get("notes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            text.push('\n');
            text.push_str(n.get("body").and_then(Value::as_str).unwrap_or(""));
        }
        for cap in re.captures_iter(&text) {
            let target = cap[1].to_string();
            let c = CrossRef {
                issue_iid: iid,
                target_project: target.clone(),
            };
            if target != project_id && !cross.contains(&c) {
                cross.push(c);
            }
        }
    }
    for m in memory_store::list_task_memories(alias, memory_root) {
        for t in m.tags {
            if !t.is_empty() {
                *areas.entry(t).or_default() += 1;
            }
        }
    }
    recent.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    IssueDigest {
        areas: ranked(areas),
        people: ranked(people),
        recent: recent
            .into_iter()
            .take(10)
            .map(|(_, i, t)| (i, t))
            .collect(),
        cross_refs: cross,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn scan_finds_manifests_and_entry_points() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Cargo.toml", "[package]\nname = \"app\"\n");
        write(d.path(), "src/main.rs", "fn main() {}\n");
        write(d.path(), ".gitlab-ci.yml", "test:\n  script: cargo test\n");
        write(
            d.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: x\n  db:\n    image: pg\n",
        );
        write(d.path(), "node_modules/x/package.json", "{}");
        write(d.path(), "a/b/c/d/deep.txt", "x");
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(s
            .manifests
            .iter()
            .any(|(p, t)| p == "Cargo.toml" && t.contains("app")));
        assert_eq!(s.entry_points, vec!["src/main.rs"]);
        assert_eq!(s.ci, vec![".gitlab-ci.yml"]);
        assert_eq!(s.services, vec!["web", "db"]);
        assert!(!s.tree.iter().any(|p| p.contains("node_modules")));
        assert!(!s.tree.iter().any(|p| p.contains("deep.txt")));
        assert!(s.tree.contains(&"src/".to_string()));
    }

    #[test]
    fn scan_records_env_key_names_but_never_values() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Cargo.toml", "[package]\n");
        write(d.path(), ".env", "API_KEY=supersecretvalue1234567890\n");
        write(
            d.path(),
            ".env.example",
            "API_KEY=supersecretvalue1234567890\nDB_URL=\n",
        );
        write(
            d.path(),
            "Gemfile",
            "gem 'x'\nTOKEN = 'glpat-abcdefghij0123456789'\n",
        );
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(s
            .secret_names
            .iter()
            .any(|r| r.name == "API_KEY" && r.location == ".env.example"));
        assert!(s.secret_names.iter().any(|r| r.name == "DB_URL"));
        let all = format!("{s:?}");
        assert!(!all.contains("supersecretvalue1234567890"));
        assert!(!all.contains("glpat-abcdefghij"));
    }

    #[test]
    fn scan_detects_sibling_repo_in_gitmodules_and_manifest_path_dep() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            ".gitmodules",
            "[submodule \"lib\"]\n\tpath = lib\n\turl = git@gitlab.com:grp/shared-lib.git\n",
        );
        write(
            d.path(),
            "Cargo.toml",
            "[dependencies]\ncore = { path = \"../core-engine\" }\nlocal = { path = \"crates/local\" }\n",
        );
        write(
            d.path(),
            ".gitlab-ci.yml",
            "deploy:\n  script: curl billing-api/health\n",
        );
        let s = scan_repo(d.path(), &["billing-api".to_string()]).unwrap();
        let find = |t: &str| s.sibling_refs.iter().find(|r| r.target == t).cloned();
        assert_eq!(find("shared-lib").unwrap().evidence, ".gitmodules:3");
        assert_eq!(find("core-engine").unwrap().evidence, "Cargo.toml:2");
        assert_eq!(find("billing-api").unwrap().evidence, ".gitlab-ci.yml:2");
        assert!(find("local").is_none());
    }

    #[test]
    fn scan_errors_on_missing_dir() {
        let d = tempfile::tempdir().unwrap();
        assert!(scan_repo(&d.path().join("nope"), &[]).is_err());
        assert!(scan_repo(d.path(), &[]).is_err());
    }

    #[test]
    fn scan_hashes_sources() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Cargo.toml", "abc");
        let s = scan_repo(d.path(), &[]).unwrap();
        assert_eq!(s.sources.len(), 1);
        assert_eq!(s.sources[0].path, "Cargo.toml");
        assert_eq!(s.sources[0].sha256, sha256::hex(&sha256::sha256(b"abc")));
    }

    #[test]
    fn digest_counts_areas_and_people_from_cached_issues() {
        let d = tempfile::tempdir().unwrap();
        let issues = d.path().join("issues");
        let put = |iid: i64, v: Value| {
            write(&issues, &format!("{iid}.json"), &v.to_string());
        };
        put(
            1,
            json!({"iid":1,"title":"A","labels":["billing","bug"],"author":{"username":"ann"},
                   "updated_at":"2026-01-01T00:00:00Z","description":"see other/proj#4"}),
        );
        put(
            2,
            json!({"iid":2,"title":"B","labels":["billing"],"author":{"username":"ann"},
                   "assignees":[{"username":"bob"}],"updated_at":"2026-02-01T00:00:00Z",
                   "notes":[{"body":"dup of grp/self!9 and grp/x!3"}]}),
        );
        let g = digest_from(&issues, "grp/self", "none", Some(&d.path().join("mem")));
        assert_eq!(g.areas[0], ("billing".to_string(), 2));
        assert_eq!(g.people[0], ("ann".to_string(), 2));
        assert!(g.people.contains(&("bob".to_string(), 1)));
        assert_eq!(g.recent[0], (2, "B".to_string()));
        assert!(g.cross_refs.contains(&CrossRef {
            issue_iid: 1,
            target_project: "other/proj".into()
        }));
        assert!(g.cross_refs.contains(&CrossRef {
            issue_iid: 2,
            target_project: "grp/x".into()
        }));
        assert!(!g.cross_refs.iter().any(|c| c.target_project == "grp/self"));
        let empty = digest_from(
            &d.path().join("missing"),
            "p",
            "a",
            Some(&d.path().join("mem")),
        );
        assert_eq!(empty, IssueDigest::default());
    }
}

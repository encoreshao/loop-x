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
/// Files larger than this are not hashed into `sources` (never loaded whole).
const SOURCE_MAX_BYTES: u64 = 4 * 1024 * 1024;
const SKIP_DIRS: [&str; 5] = [".git", "node_modules", "target", "vendor", ".venv"];
const MANIFEST_NAMES: [&str; 17] = [
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
    "compose.yml",
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

fn secret_like_key(key: &str) -> bool {
    let name = key.to_ascii_lowercase();
    let segments: Vec<&str> = name.split(['_', '-', '.']).collect();
    [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "private_key",
        "privatekey",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|w| name.contains(w))
        || ["pass", "passwd", "pwd", "auth", "key"]
            .iter()
            .any(|w| segments.contains(w))
}

/// `(key, value start offset)` of an assignment-looking line: `KEY=v`,
/// `KEY: v`, `"key": "v"`, `ENV KEY=v`, `ENV KEY v`, `export KEY=v`, `- KEY=v`.
fn assignment(line: &str) -> Option<(&str, usize)> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"^\s*(?:-\s+)?(?:(?:export|ENV|ARG)\s+)?["']?([A-Za-z0-9_.\-]+)["']?(?:\s*[=:]\s*|\s+)"#,
        )
        .expect("static regex")
    });
    let c = re.captures(line)?;
    Some((c.get(1)?.as_str(), c.get(0)?.end()))
}

/// Manifest text with secret values replaced (key names kept) and the names
/// of the variables whose value was replaced.
fn redact(text: &str) -> (String, Vec<String>) {
    let mut names = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut in_pem = false;
    for line in text.lines() {
        if in_pem {
            if line.contains("-----END") {
                in_pem = false;
            }
            continue;
        }
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            in_pem = !line.contains("-----END");
            out.push("[redacted private key block]".to_string());
            continue;
        }
        if let Some((key, start)) = assignment(line) {
            let value = line[start..]
                .trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == ',');
            let is_ref = value.starts_with('$') || value.starts_with("{{");
            if secret_like_key(key) && !value.is_empty() && !is_ref {
                if !names.iter().any(|n| n == key) {
                    names.push(key.to_string());
                }
                out.push(format!("{}[redacted]", &line[..start]));
                continue;
            }
        }
        if contains_secret(line).is_some() {
            out.push("[redacted line]".to_string());
        } else {
            out.push(line.to_string());
        }
    }
    (out.join("\n"), names)
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

/// Hashes the whole file (what `stale_setup_sources` compares against), but
/// only when it is small enough to load; bigger files are not recorded.
fn add_source(scan: &mut Scan, rel: &str, full: &Path) {
    let small = std::fs::metadata(full).is_ok_and(|m| m.len() <= SOURCE_MAX_BYTES);
    if !small {
        return;
    }
    if let Ok(bytes) = std::fs::read(full) {
        scan.sources.push(Source {
            path: rel.to_string(),
            sha256: sha256::hex(&sha256::sha256(&bytes)),
        });
    }
}

/// The checkout's `HEAD` sha, or `None` (not a repository, no git, or a 5 s timeout).
pub fn head_sha(work_dir: &Path) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(["rev-parse", "HEAD"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        match child.try_wait().ok()? {
            Some(st) => break st,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    if !status.success() {
        return None;
    }
    let mut sha = String::new();
    child.stdout.take()?.read_to_string(&mut sha).ok()?;
    let sha = sha.trim().to_string();
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
            add_source(&mut scan, rel, &full);
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
        add_source(&mut scan, rel, &full);
        if manifest {
            let (text, names) = redact(&text);
            for name in names {
                scan.secret_names.push(SecretRef {
                    name,
                    location: rel.clone(),
                });
            }
            scan.manifests.push((rel.clone(), text));
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
    digest_from(
        &issues_dir_for(instance, project_id),
        project_id,
        alias,
        None,
    )
}

/// Where [`gitlab_cache::sync_issue`] keeps this project's issue files.
pub(crate) fn issues_dir_for(instance: &str, project_id: &str) -> std::path::PathBuf {
    gitlab_cache::project_path(instance, project_id)
        .parent()
        .map(|p| p.join("issues"))
        .unwrap_or_default()
}

pub(crate) fn digest_from(
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

    fn manifest_text(s: &Scan, name: &str) -> String {
        s.manifests
            .iter()
            .find(|(p, _)| p == name)
            .map(|(_, t)| t.clone())
            .unwrap_or_default()
    }

    #[test]
    fn short_secret_values_in_manifests_are_redacted_but_names_are_kept() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "docker-compose.yml",
            "services:\n  db:\n    environment:\n      POSTGRES_PASSWORD: hunter2\n      POSTGRES_DB: app\n",
        );
        write(
            d.path(),
            "Dockerfile",
            "FROM x\nENV DB_PASSWORD=abc123\nENV API_TOKEN abc123\n",
        );
        write(d.path(), "Makefile", "PASSWORD=x\nall:\n\techo hi\n");
        write(
            d.path(),
            "Gemfile",
            "gem 'x'\n-----BEGIN RSA PRIVATE KEY-----\nMIIEBODYLINEONE\nBODYLINETWO==\n-----END RSA PRIVATE KEY-----\ngem 'after'\n",
        );
        let s = scan_repo(d.path(), &[]).unwrap();
        let compose = manifest_text(&s, "docker-compose.yml");
        assert!(!compose.contains("hunter2"), "{compose}");
        assert!(compose.contains("POSTGRES_PASSWORD"));
        assert!(compose.contains("POSTGRES_DB: app"));
        let docker = manifest_text(&s, "Dockerfile");
        assert!(!docker.contains("abc123"), "{docker}");
        assert!(docker.contains("DB_PASSWORD") && docker.contains("API_TOKEN"));
        let make = manifest_text(&s, "Makefile");
        assert!(!make.contains("PASSWORD=x"), "{make}");
        assert!(make.contains("PASSWORD") && make.contains("echo hi"));
        let gem = manifest_text(&s, "Gemfile");
        assert!(!gem.contains("BODYLINE") && !gem.contains("MIIE"), "{gem}");
        assert!(gem.contains("gem 'x'") && gem.contains("gem 'after'"));
        for n in ["POSTGRES_PASSWORD", "DB_PASSWORD", "PASSWORD"] {
            assert!(s.secret_names.iter().any(|r| r.name == n), "{n}");
        }
    }

    #[test]
    fn symlinks_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "Cargo.toml", "outside-secret-content");
        write(d.path(), "Gemfile", "gem 'x'\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("Cargo.toml"),
                d.path().join("Cargo.toml"),
            )
            .unwrap();
            std::os::unix::fs::symlink(outside.path(), d.path().join("linked")).unwrap();
        }
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(!format!("{s:?}").contains("outside-secret-content"));
        assert!(!s.tree.iter().any(|p| p.starts_with("linked/")));
        assert!(!s.manifests.iter().any(|(p, _)| p == "Cargo.toml"));
    }

    #[test]
    fn non_utf8_manifest_does_not_fail_the_scan() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Makefile"), b"all:\n\techo \xff\xfe\n").unwrap();
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(manifest_text(&s, "Makefile").contains("echo"));
    }

    #[test]
    fn manifest_text_is_capped_at_64_kib_but_the_hash_covers_the_file() {
        let d = tempfile::tempdir().unwrap();
        let big = "a".repeat(100 * 1024);
        write(d.path(), "Makefile", &big);
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(manifest_text(&s, "Makefile").len() <= 64 * 1024);
        assert_eq!(
            s.sources[0].sha256,
            sha256::hex(&sha256::sha256(big.as_bytes()))
        );
    }

    #[test]
    fn files_over_the_hash_limit_are_not_loaded_or_recorded_as_sources() {
        let d = tempfile::tempdir().unwrap();
        let f = fs::File::create(d.path().join("Makefile")).unwrap();
        f.set_len(SOURCE_MAX_BYTES + 1).unwrap();
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(s.sources.is_empty());
        assert!(manifest_text(&s, "Makefile").len() <= 64 * 1024);
    }

    #[test]
    fn compose_yml_and_gitmodules_are_scanned_and_hashed() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "compose.yml", "services:\n  api:\n    image: x\n");
        write(
            d.path(),
            ".gitmodules",
            "[submodule \"a\"]\n\turl = https://h/g/lib.git\n",
        );
        let s = scan_repo(d.path(), &[]).unwrap();
        assert!(s.manifests.iter().any(|(p, _)| p == "compose.yml"));
        assert_eq!(s.services, vec!["api"]);
        assert!(s.sources.iter().any(|x| x.path == ".gitmodules"));
    }

    #[test]
    fn digest_issues_wrapper_reads_the_cache_under_a_temp_home() {
        let _g = crate::connectors::testutil::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("LOOP_ENGINEERING_HOME", home.path().join("lx"));
        let issue = json!({"iid": 7, "title": "T", "labels": ["ui"], "author": {"username": "zed"},
                           "updated_at": "2026-03-01T00:00:00Z"});
        gitlab_cache::write_json(&gitlab_cache::issue_path("work", "grp/p", "7"), &issue).unwrap();
        assert!(issues_dir_for("work", "grp/p").starts_with(home.path()));
        let g = digest_issues("work", "grp/p", "p");
        assert_eq!(g.areas, vec![("ui".to_string(), 1)]);
        assert_eq!(g.people, vec![("zed".to_string(), 1)]);
        assert_eq!(g.recent, vec![(7, "T".to_string())]);
        assert_eq!(
            digest_issues("work", "grp/none", "none"),
            IssueDigest::default()
        );
    }
}

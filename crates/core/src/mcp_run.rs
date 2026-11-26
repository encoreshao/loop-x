//! The MCP servers one agent call gets: resolved from the registry, written to
//! a private per-run `--mcp-config` file that is removed when the call ends.
//!
//! A server with Keychain secrets is written as `loopx mcp-launch <spec> <sig>`:
//! the file names no secret, and the launcher (this signed binary, see
//! `keychain::locked_to_this_app`) reads them from the Keychain itself. `sig` is
//! an HMAC of the spec under a key only this app can read, so the agent cannot
//! point the launcher at another command; the launcher starts the server with
//! only the spec's environment plus [`PASSED_THROUGH`].

use crate::keychain::SecretStore;
use crate::mcp_registry::{self as reg, Launch, Registry};
use crate::node_env::Toolchains;
use serde_json::{json, Map};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RUN_DIR_PREFIX: &str = "loopx-mcp-";
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);
/// Claude Code's MCP server start-up timeout, in ms.
pub const MCP_TIMEOUT_ENV: (&str, &str) = ("MCP_TIMEOUT", "60000");
pub const REDACTED: &str = "•••";
const DEAD_PROXY: &str = "http://127.0.0.1:9";

pub struct Env {
    pub registry: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    pub toolchains: Toolchains,
    pub launch_path: String,
    pub tmp_root: PathBuf,
    /// The `loopx` binary a server with secrets is started through.
    pub launcher: PathBuf,
    /// Holds the key launch specs are signed with.
    pub launch_keys: Arc<dyn SecretStore>,
}

/// Keychain service of the launch-spec signing key.
pub const LAUNCH_KEY_SERVICE: &str = "loop-engineering.mcp-launch";
const LAUNCH_KEY_ACCOUNT: &str = "key";
pub const LAUNCH_SUBCOMMAND: &str = "mcp-launch";
/// The only variables a launched server takes from whoever started it.
pub const PASSED_THROUGH: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "SHELL",
    "__CF_USER_TEXT_ENCODING",
];

impl Env {
    /// An unavailable Keychain leaves every secret missing, so only servers
    /// without secrets attach.
    pub fn real(home: &Path) -> Env {
        let open = |service: &str| -> Arc<dyn SecretStore> {
            match crate::keychain::open_store(service) {
                Ok(s) => Arc::from(s),
                Err(_) => Arc::new(crate::keychain::MemoryStore::new(service)),
            }
        };
        Env {
            registry: reg::path(home),
            secrets: open(reg::SERVICE_MCP),
            toolchains: Toolchains::from_env(),
            launch_path: crate::runner_util::cli_launch_path(),
            tmp_root: std::env::temp_dir(),
            launcher: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("loopx")),
            launch_keys: open(LAUNCH_KEY_SERVICE),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skip {
    pub name: String,
    pub reason: &'static str,
}

/// A server ready to start: everything resolved, secrets included.
#[derive(Debug, Clone)]
pub struct Launchable {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub secrets: Vec<String>,
    /// Which of `env`'s variables are secrets, and the Keychain account of each.
    pub secret_vars: BTreeMap<String, String>,
    pub deny_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unresolved {
    pub reason: &'static str,
    pub message: String,
    pub install: Option<String>,
}

pub struct McpAttach {
    dir: PathBuf,
    pub config: PathBuf,
    pub servers: Vec<String>,
    pub skipped: Vec<Skip>,
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
    secrets: Vec<String>,
}

impl Drop for McpAttach {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl McpAttach {
    pub fn redact(&self, text: &str) -> String {
        redact(text, &self.secrets)
    }

    pub fn summary(&self) -> String {
        summary(&self.servers, &self.skipped)
    }
}

pub fn redact(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text.to_string(), |t, s| t.replace(s.as_str(), REDACTED))
}

/// `MCP: attached playwright; skipped sentry (secret_missing)`.
pub fn summary(servers: &[String], skipped: &[Skip]) -> String {
    let attached = if servers.is_empty() {
        "none".to_string()
    } else {
        servers.join(", ")
    };
    let mut line = format!("MCP: attached {attached}");
    if !skipped.is_empty() {
        let s: Vec<String> = skipped
            .iter()
            .map(|s| format!("{} ({})", s.name, s.reason))
            .collect();
        line.push_str(&format!("; skipped {}", s.join(", ")));
    }
    line
}

pub fn allow_rule(name: &str) -> String {
    format!("mcp__{name}__*")
}

fn unresolved(reason: &'static str, message: impl Into<String>) -> Unresolved {
    Unresolved {
        reason,
        message: message.into(),
        install: None,
    }
}

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn resolve_server(
    name: &str,
    registry: &Registry,
    env: &Env,
) -> Result<Launchable, Unresolved> {
    let spec = match registry.servers.get(name) {
        None => {
            return Err(unresolved(
                "not_registered",
                format!("{name} is not in {}", env.registry.display()),
            ))
        }
        Some(Err(why)) => return Err(unresolved("invalid", why.clone())),
        Some(Ok(spec)) => spec,
    };
    if spec.is_playwright() {
        if let Some(chrome) = spec.chrome_path().filter(|c| !c.is_file()) {
            return Err(unresolved(
                "no_chrome",
                format!("Google Chrome is no longer at {}", chrome.display()),
            ));
        }
    }
    let mut server_env = spec.env.clone();
    let (command, args) = match &spec.launch {
        Launch::Command(p) => {
            if !is_executable_file(p) {
                return Err(unresolved(
                    "no_command",
                    format!("{} is not an executable file", p.display()),
                ));
            }
            (p.clone(), spec.args.clone())
        }
        Launch::NpmGlobal {
            package,
            version,
            bin,
        } => {
            let r =
                reg::resolve_npm_global(package, version, bin, &env.toolchains, &env.launch_path)
                    .map_err(|m| {
                    let message = match &m {
                        reg::Missing::NotInstalled { .. } => {
                            format!("{package}@{version} is not installed")
                        }
                        reg::Missing::WrongVersion { found, dir, .. } => format!(
                            "found {package}@{found} in {}, needs {version}",
                            dir.display()
                        ),
                        reg::Missing::NoNode { dir } => {
                            format!("{} has no Node 18 or newer", dir.display())
                        }
                    };
                    Unresolved {
                        reason: m.reason(),
                        message,
                        install: m.install().map(str::to_string),
                    }
                })?;
            server_env.insert("PATH".into(), r.path_env);
            let mut args = r.args_prefix;
            args.extend(spec.args.iter().cloned());
            (r.command, args)
        }
    };
    let mut secrets = Vec::new();
    for (var, account) in &spec.env_secrets {
        match env.secrets.get(account) {
            Ok(Some(v)) if !v.is_empty() => {
                secrets.push(v.clone());
                server_env.insert(var.clone(), v);
            }
            _ => {
                return Err(unresolved(
                    "secret_missing",
                    format!("Keychain item {account} for {var} is missing"),
                ))
            }
        }
    }
    Ok(Launchable {
        command,
        args,
        env: server_env,
        secrets,
        secret_vars: spec.env_secrets.clone(),
        deny_tools: spec.deny_tools.clone(),
    })
}

fn run_dir_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{RUN_DIR_PREFIX}{}-{nanos}", std::process::id())
}

fn write_private(dir: &Path, file: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)?;
    f.write_all(body.as_bytes())
}

/// `output_dir` is Playwright's `--output-dir`: its only file root besides the
/// claude process's cwd, so screenshots can go to a screens dir outside it.
pub fn attach(
    declared: &[String],
    env: &Env,
    output_dir: Option<&Path>,
) -> Result<Option<McpAttach>, Vec<Skip>> {
    attach_inner(declared, env, output_dir, false)
}

/// [`attach`] for a caller whose agent output is logged unredacted (Fix now):
/// a server with Keychain secrets is skipped as `secrets_unsupported`.
pub fn attach_without_secrets(
    declared: &[String],
    env: &Env,
    output_dir: Option<&Path>,
) -> Result<Option<McpAttach>, Vec<Skip>> {
    attach_inner(declared, env, output_dir, true)
}

fn attach_inner(
    declared: &[String],
    env: &Env,
    output_dir: Option<&Path>,
    no_secrets: bool,
) -> Result<Option<McpAttach>, Vec<Skip>> {
    if declared.is_empty() {
        return Ok(None);
    }
    sweep_stale(&env.tmp_root, SystemTime::now());
    let mut names: Vec<&String> = Vec::new();
    for n in declared {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let registry = reg::load(&env.registry);
    let mut skipped = Vec::new();
    let mut ready: Vec<(String, Launchable)> = Vec::new();
    for name in names {
        let has_secrets = matches!(&registry, Ok(r) if r.servers.get(name.as_str())
            .is_some_and(|s| s.as_ref().is_ok_and(|s| !s.env_secrets.is_empty())));
        if no_secrets && has_secrets {
            skipped.push(Skip {
                name: name.clone(),
                reason: "secrets_unsupported",
            });
            continue;
        }
        let resolved = match &registry {
            Ok(r) => resolve_server(name, r, env),
            Err(e) => Err(unresolved("invalid", e.clone())),
        };
        match resolved {
            Ok(mut l) => {
                let playwright = matches!(&registry, Ok(r) if r.servers.get(name.as_str())
                    .is_some_and(|s| s.as_ref().is_ok_and(|s| s.is_playwright())));
                if let (true, Some(out)) = (playwright, output_dir) {
                    if !l.args.iter().any(|a| a == "--output-dir") {
                        l.args.push("--output-dir".into());
                        l.args.push(out.to_string_lossy().into_owned());
                    }
                }
                // A dead proxy: Chrome still reaches loopback (the page under test) directly.
                if playwright && !l.args.iter().any(|a| a.starts_with("--proxy-server")) {
                    l.args.push("--proxy-server".into());
                    l.args.push(DEAD_PROXY.into());
                }
                ready.push((name.clone(), l));
            }
            Err(u) => skipped.push(Skip {
                name: name.clone(),
                reason: u.reason,
            }),
        }
    }
    if ready.is_empty() {
        return Err(skipped);
    }
    let key = if ready.iter().any(|(_, l)| !l.secret_vars.is_empty()) {
        launch_key(env)
    } else {
        None
    };
    let mut servers_json = Map::new();
    let mut launchable = Vec::new();
    for (name, l) in ready {
        let entry = match (&key, l.secret_vars.is_empty()) {
            (_, true) => {
                json!({"type": "stdio", "command": l.command, "args": l.args, "env": l.env})
            }
            (Some(key), false) => {
                let (spec, sig) = sign_launch(key, &name, &l, &env.launch_path);
                json!({"type": "stdio", "command": env.launcher,
                       "args": [LAUNCH_SUBCOMMAND, spec, sig], "env": {}})
            }
            (None, false) => {
                skipped.push(Skip {
                    name,
                    reason: "secret_missing",
                });
                continue;
            }
        };
        servers_json.insert(name.clone(), entry);
        launchable.push((name, l));
    }
    let ready = launchable;
    if ready.is_empty() {
        return Err(skipped);
    }
    let body = json!({"mcpServers": servers_json}).to_string();
    let dir = env.tmp_root.join(run_dir_name());
    let config = dir.join("mcp.json");
    if let Err(_e) = write_private(&dir, &config, &body) {
        let _ = std::fs::remove_dir_all(&dir);
        skipped.extend(ready.iter().map(|(name, _)| Skip {
            name: name.clone(),
            reason: "invalid",
        }));
        return Err(skipped);
    }
    let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    let mut denied: Vec<String> = Vec::new();
    for (name, l) in &ready {
        denied.extend(l.deny_tools.iter().map(|t| format!("mcp__{name}__{t}")));
    }
    let mut forms = vec![canonical.clone()];
    if dir != canonical {
        forms.push(dir.clone());
    }
    for d in &forms {
        for kind in ["Read", "Edit", "Write", "Grep"] {
            denied.push(format!("{kind}(/{}/**)", d.display()));
        }
    }
    denied.push("Bash(git diff --no-index*)".into());
    Ok(Some(McpAttach {
        dir,
        config,
        servers: ready.iter().map(|(n, _)| n.clone()).collect(),
        skipped,
        allowed: ready.iter().map(|(n, _)| allow_rule(n)).collect(),
        denied,
        secrets: ready.into_iter().flat_map(|(_, l)| l.secrets).collect(),
    }))
}

/// The launch-spec signing key, created on first use.
fn launch_key(env: &Env) -> Option<Vec<u8>> {
    if let Ok(Some(k)) = env.launch_keys.get(LAUNCH_KEY_ACCOUNT) {
        return crate::sha256::unhex(&k);
    }
    let key = crate::sha256::random_bytes(32);
    env.launch_keys
        .set(LAUNCH_KEY_ACCOUNT, &crate::sha256::hex(&key))
        .ok()?;
    Some(key)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LaunchSpec {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    secrets: BTreeMap<String, String>,
}

/// `(spec, sig)`: the server without its secret values, as base64url JSON, and
/// the hex HMAC of those exact bytes.
fn sign_launch(key: &[u8], name: &str, l: &Launchable, launch_path: &str) -> (String, String) {
    use base64::Engine;
    let mut plain_env: BTreeMap<String, String> = l
        .env
        .iter()
        .filter(|(k, _)| !l.secret_vars.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    plain_env
        .entry("PATH".into())
        .or_insert_with(|| launch_path.to_string());
    let spec = LaunchSpec {
        name: name.to_string(),
        command: l.command.clone(),
        args: l.args.clone(),
        env: plain_env,
        secrets: l.secret_vars.clone(),
    };
    let bytes = serde_json::to_vec(&spec).unwrap_or_default();
    (
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes),
        crate::sha256::hex(&crate::sha256::hmac_sha256(key, &bytes)),
    )
}

/// What `loopx mcp-launch` starts.
#[derive(Debug)]
pub struct Prepared {
    pub command: PathBuf,
    pub args: Vec<String>,
    /// The whole environment: nothing else is inherited.
    pub env: BTreeMap<String, String>,
}

/// Checks `mcp-launch <spec> <sig>` against the signing key and fills in the
/// secrets. `inherited` is the caller's environment, of which only
/// [`PASSED_THROUGH`] is kept.
pub fn prepare_launch(
    args: &[String],
    env: &Env,
    inherited: &dyn Fn(&str) -> Option<String>,
) -> Result<Prepared, String> {
    use base64::Engine;
    let [sub, spec, sig] = args else {
        return Err(format!("usage: loopx {LAUNCH_SUBCOMMAND} <spec> <sig>"));
    };
    if sub != LAUNCH_SUBCOMMAND {
        return Err(format!("usage: loopx {LAUNCH_SUBCOMMAND} <spec> <sig>"));
    }
    let refused = || "refused: this launch was not prepared by LoopX".to_string();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(spec)
        .map_err(|_| refused())?;
    let key = env
        .launch_keys
        .get(LAUNCH_KEY_ACCOUNT)
        .ok()
        .flatten()
        .and_then(|k| crate::sha256::unhex(&k))
        .ok_or_else(refused)?;
    let want = crate::sha256::hmac_sha256(&key, &bytes);
    let got = crate::sha256::unhex(sig).ok_or_else(refused)?;
    let differs = got.len() != want.len()
        || got
            .iter()
            .zip(want.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0;
    if differs {
        return Err(refused());
    }
    let spec: LaunchSpec = serde_json::from_slice(&bytes).map_err(|_| refused())?;
    let mut out: BTreeMap<String, String> = PASSED_THROUGH
        .iter()
        .filter_map(|k| inherited(k).map(|v| (k.to_string(), v)))
        .collect();
    out.extend(spec.env);
    for (var, account) in &spec.secrets {
        match env.secrets.get(account) {
            Ok(Some(v)) if !v.is_empty() => {
                out.insert(var.clone(), v);
            }
            _ => {
                return Err(format!(
                    "{}: Keychain item {account} for {var} is missing",
                    spec.name
                ))
            }
        }
    }
    Ok(Prepared {
        command: spec.command,
        args: spec.args,
        env: out,
    })
}

pub fn sweep_stale(tmp_root: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(tmp_root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(RUN_DIR_PREFIX) else {
            continue;
        };
        // A run stopped by SIGKILL never unwinds, so its secrets would sit here a day.
        let owner_dead = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<i64>().ok())
            .is_some_and(|pid| !crate::run_control::process_alive(pid));
        if owner_dead && e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
            continue;
        }
        let stale = e
            .metadata()
            .ok()
            .filter(|m| m.is_dir())
            .and_then(|m| m.modified().ok())
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::keychain::MemoryStore;
    use crate::mcp_registry::tests::fake_install;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) struct Fixture {
        pub root: tempfile::TempDir,
        pub store: Arc<MemoryStore>,
        pub chrome: PathBuf,
        pub launch_keys: Arc<MemoryStore>,
    }

    impl Fixture {
        /// A registry with `playwright` (fake global install, fake Chrome) and
        /// `demo` (a command with one Keychain secret).
        pub fn new() -> Fixture {
            let root = tempfile::tempdir().unwrap();
            let chrome = root.path().join("Chrome");
            std::fs::write(&chrome, "").unwrap();
            fake_install(
                &root.path().join("home/.nvm/versions/node/v20.15.0"),
                reg::PLAYWRIGHT_VERSION,
                "v20.15.0",
            );
            let demo = root.path().join("demo-mcp");
            std::fs::write(&demo, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&demo, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::create_dir_all(root.path().join("tmp")).unwrap();
            let f = Fixture {
                root,
                store: Arc::new(MemoryStore::new(reg::SERVICE_MCP)),
                chrome,
                launch_keys: Arc::new(MemoryStore::new(LAUNCH_KEY_SERVICE)),
            };
            reg::save_entry(
                &f.registry(),
                "playwright",
                &reg::playwright_preset(Some(f.chrome.clone())).unwrap(),
            )
            .unwrap();
            let mut demo_spec = reg::ServerSpec::from_value(&json!({
                "command": demo, "args": ["--x"], "env": {"DEMO_MODE": "on"},
                "env_secrets": {"DEMO_TOKEN": "keychain:demo-token"}}))
            .unwrap();
            demo_spec.deny_tools = vec!["wipe".into()];
            reg::save_entry(&f.registry(), "demo", &demo_spec).unwrap();
            f.store.set("demo-token", "s3cret-value").unwrap();
            f
        }

        pub fn registry(&self) -> PathBuf {
            reg::path(self.root.path())
        }

        pub fn tmp(&self) -> PathBuf {
            self.root.path().join("tmp")
        }

        pub fn env(&self) -> Env {
            Env {
                registry: self.registry(),
                secrets: self.store.clone(),
                toolchains: Toolchains {
                    home: self.root.path().join("home"),
                    nvm_dir: None,
                },
                launch_path: "/nonexistent".into(),
                tmp_root: self.tmp(),
                launcher: PathBuf::from("/opt/loopx/bin/loopx"),
                launch_keys: self.launch_keys.clone(),
            }
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn config(a: &McpAttach) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&a.config).unwrap()).unwrap()
    }

    #[test]
    fn run_config_is_private_and_deleted_on_drop() {
        let f = Fixture::new();
        let a = attach(&names(&["demo"]), &f.env(), None).unwrap().unwrap();
        let dir = a.config.parent().unwrap().to_path_buf();
        assert!(dir.starts_with(f.tmp()));
        assert!(dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(RUN_DIR_PREFIX));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&a.config), 0o600);
        let c = config(&a);
        let demo = &c["mcpServers"]["demo"];
        assert_eq!(demo["type"], "stdio");
        assert_eq!(demo["command"], json!(f.env().launcher));
        assert_eq!(demo["args"][0], "mcp-launch");
        assert_eq!(demo["env"], json!({}));
        drop(a);
        assert!(!dir.exists());
    }

    #[test]
    fn secrets_resolve_only_from_the_injected_store() {
        let f = Fixture::new();
        let mut env = f.env();
        let other = Arc::new(MemoryStore::new(reg::SERVICE_MCP));
        other.set("demo-token", "from-other-store").unwrap();
        env.secrets = other;
        let a = attach(&names(&["demo"]), &env, None).unwrap().unwrap();
        let launch = prepare_launch(&launch_args(&a, "demo"), &env, &|_| None).unwrap();
        assert_eq!(launch.env["DEMO_TOKEN"], "from-other-store");
    }

    fn launch_args(a: &McpAttach, name: &str) -> Vec<String> {
        serde_json::from_value(config(a)["mcpServers"][name]["args"].clone()).unwrap()
    }

    #[test]
    fn a_server_with_secrets_gets_them_only_from_the_launcher() {
        let f = Fixture::new();
        let a = attach(&names(&["demo"]), &f.env(), None).unwrap().unwrap();
        let text = std::fs::read_to_string(&a.config).unwrap();
        assert!(!text.contains("s3cret-value"), "{text}");
        assert!(!text.contains("demo-token"), "{text}");
        let inherited = |k: &str| match k {
            "HOME" => Some("/Users/me".to_string()),
            "NODE_OPTIONS" => Some("--require /tmp/evil.js".to_string()),
            "HTTPS_PROXY" => Some("http://evil:8080".to_string()),
            _ => None,
        };
        let launch = prepare_launch(&launch_args(&a, "demo"), &f.env(), &inherited).unwrap();
        assert_eq!(launch.command, f.root.path().join("demo-mcp"));
        assert_eq!(launch.args, ["--x"]);
        assert_eq!(launch.env["DEMO_TOKEN"], "s3cret-value");
        assert_eq!(launch.env["DEMO_MODE"], "on");
        assert_eq!(launch.env["HOME"], "/Users/me");
        assert_eq!(launch.env["PATH"], "/nonexistent");
        assert!(!launch.env.contains_key("NODE_OPTIONS"));
        assert!(!launch.env.contains_key("HTTPS_PROXY"));
    }

    #[test]
    fn a_launch_spec_changed_after_attach_is_refused() {
        use base64::Engine;
        let f = Fixture::new();
        let a = attach(&names(&["demo"]), &f.env(), None).unwrap().unwrap();
        let args = launch_args(&a, "demo");
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut spec: Value = serde_json::from_slice(&engine.decode(&args[1]).unwrap()).unwrap();
        spec["command"] = json!("/usr/bin/env");
        let forged = engine.encode(serde_json::to_vec(&spec).unwrap());
        let tampered = vec![args[0].clone(), forged, args[2].clone()];
        assert!(prepare_launch(&tampered, &f.env(), &|_| None).is_err());
        let bad_sig = vec![args[0].clone(), args[1].clone(), "00".repeat(32)];
        assert!(prepare_launch(&bad_sig, &f.env(), &|_| None).is_err());

        let mut other = f.env();
        other.launch_keys = Arc::new(MemoryStore::new(LAUNCH_KEY_SERVICE));
        assert!(prepare_launch(&args, &other, &|_| None).is_err());
    }

    #[test]
    fn a_missing_secret_skips_only_that_server() {
        let f = Fixture::new();
        f.store.delete("demo-token").unwrap();
        let a = attach(&names(&["playwright", "demo"]), &f.env(), None)
            .unwrap()
            .unwrap();
        assert_eq!(a.servers, names(&["playwright"]));
        assert_eq!(
            a.skipped,
            vec![Skip {
                name: "demo".into(),
                reason: "secret_missing"
            }]
        );
        assert!(config(&a)["mcpServers"].get("demo").is_none());
        assert_eq!(
            a.summary(),
            "MCP: attached playwright; skipped demo (secret_missing)"
        );
        drop(a);
        let skips = attach(&names(&["demo"]), &f.env(), None).err().unwrap();
        assert_eq!(skips[0].reason, "secret_missing");
        assert!(std::fs::read_dir(f.tmp()).unwrap().next().is_none());
    }

    #[test]
    fn undeclared_registry_entries_never_attach() {
        let f = Fixture::new();
        let a = attach(&names(&["playwright"]), &f.env(), None)
            .unwrap()
            .unwrap();
        let c = config(&a);
        let servers: Vec<&String> = c["mcpServers"].as_object().unwrap().keys().collect();
        assert_eq!(servers, vec!["playwright"]);
        assert_eq!(a.allowed, vec![allow_rule("playwright")]);
        assert!(!a.denied.iter().any(|d| d.contains("demo")));
        assert!(attach(&[], &f.env(), None).unwrap().is_none());
        let skips = attach(&names(&["nope"]), &f.env(), None).err().unwrap();
        assert_eq!(skips[0].reason, "not_registered");
    }

    #[test]
    fn npm_global_launch_runs_the_installs_own_node_with_cli_js() {
        let f = Fixture::new();
        let out = f.root.path().join("screens");
        let a = attach(&names(&["playwright"]), &f.env(), Some(&out))
            .unwrap()
            .unwrap();
        let pw = &config(&a)["mcpServers"]["playwright"];
        let bin = f.root.path().join("home/.nvm/versions/node/v20.15.0/bin");
        assert_eq!(pw["command"], bin.join("node").to_string_lossy().as_ref());
        let args: Vec<String> = serde_json::from_value(pw["args"].clone()).unwrap();
        assert!(
            args[0].ends_with("lib/node_modules/@playwright/mcp/cli.js"),
            "{args:?}"
        );
        assert!(args.contains(&"--headless".to_string()));
        let at = args.iter().position(|a| a == "--output-dir").unwrap();
        assert_eq!(args[at + 1], out.to_string_lossy());
        assert!(pw["env"]["PATH"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}:", bin.display())));
        assert_eq!(pw["env"]["PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD"], "1");
    }

    #[test]
    fn not_installed_and_wrong_version_skip_with_their_reason() {
        let f = Fixture::new();
        let mut env = f.env();
        env.toolchains.home = f.root.path().join("nohome");
        let skips = attach(&names(&["playwright"]), &env, None).err().unwrap();
        assert_eq!(skips[0].reason, "not_installed");
        let un =
            resolve_server("playwright", &reg::load(&f.registry()).unwrap(), &env).unwrap_err();
        assert_eq!(un.install.as_deref(), Some(reg::PLAYWRIGHT_INSTALL_CMD));

        let old = f.root.path().join("old");
        let bin = fake_install(&old, "0.0.83", "v20.0.0");
        env.launch_path = bin.to_string_lossy().into_owned();
        let skips = attach(&names(&["playwright"]), &env, None).err().unwrap();
        assert_eq!(skips[0].reason, "wrong_version");
    }

    #[test]
    fn a_playwright_entry_whose_chrome_is_gone_is_skipped() {
        let f = Fixture::new();
        std::fs::remove_file(&f.chrome).unwrap();
        let skips = attach(&names(&["playwright"]), &f.env(), None)
            .err()
            .unwrap();
        assert_eq!(skips[0].reason, "no_chrome");
    }

    #[test]
    fn a_command_that_is_not_an_executable_is_skipped() {
        let f = Fixture::new();
        std::fs::remove_file(f.root.path().join("demo-mcp")).unwrap();
        let skips = attach(&names(&["demo"]), &f.env(), None).err().unwrap();
        assert_eq!(skips[0].reason, "no_command");
        std::fs::write(f.registry(), "{\"demo\": {\"command\": \"npx\"}}").unwrap();
        let skips = attach(&names(&["demo"]), &f.env(), None).err().unwrap();
        assert_eq!(skips[0].reason, "invalid");
    }

    #[test]
    fn deny_tools_become_deny_rules_and_the_run_dir_is_denied_to_read_edit_write_grep() {
        let f = Fixture::new();
        let a = attach(&names(&["playwright", "demo"]), &f.env(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            a.allowed,
            vec![allow_rule("playwright"), allow_rule("demo")]
        );
        for t in reg::PLAYWRIGHT_DENY {
            assert!(a.denied.contains(&format!("mcp__playwright__{t}")), "{t}");
        }
        assert!(a.denied.contains(&"mcp__demo__wipe".to_string()));
        let dir = std::fs::canonicalize(a.config.parent().unwrap()).unwrap();
        for kind in ["Read", "Edit", "Write", "Grep"] {
            assert!(
                a.denied.contains(&format!("{kind}(/{}/**)", dir.display())),
                "{kind}: {:?}",
                a.denied
            );
        }
        let raw = a.config.parent().unwrap();
        assert!(
            a.denied.contains(&format!("Read(/{}/**)", raw.display())),
            "{:?}",
            a.denied
        );
        assert_eq!(allow_rule("playwright"), "mcp__playwright__*");
    }

    #[test]
    fn attached_playwright_reaches_only_loopback() {
        let f = Fixture::new();
        let a = attach(&names(&["playwright"]), &f.env(), None)
            .unwrap()
            .unwrap();
        let args: Vec<String> =
            serde_json::from_value(config(&a)["mcpServers"]["playwright"]["args"].clone()).unwrap();
        let at = args.iter().position(|x| x == "--proxy-server").unwrap();
        assert_eq!(args[at + 1], "http://127.0.0.1:9");
        assert!(a.denied.contains(&"Bash(git diff --no-index*)".to_string()));
        let demo = attach(&names(&["demo"]), &f.env(), None).unwrap().unwrap();
        let launch = prepare_launch(&launch_args(&demo, "demo"), &f.env(), &|_| None).unwrap();
        assert_eq!(launch.args, ["--x"]);
    }

    #[test]
    fn servers_with_secrets_are_skipped_where_secrets_are_unsupported() {
        let f = Fixture::new();
        let a = attach_without_secrets(&names(&["playwright", "demo"]), &f.env(), None)
            .unwrap()
            .unwrap();
        assert_eq!(a.servers, names(&["playwright"]));
        assert_eq!(
            a.skipped,
            vec![Skip {
                name: "demo".into(),
                reason: "secrets_unsupported"
            }]
        );
        assert!(!std::fs::read_to_string(&a.config)
            .unwrap()
            .contains("s3cret-value"));
    }

    #[test]
    fn run_dirs_of_dead_processes_are_swept_regardless_of_age() {
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let gone = root.path().join(format!("loopx-mcp-{dead}-1"));
        let live = root
            .path()
            .join(format!("loopx-mcp-{}-2", std::process::id()));
        let other = root.path().join(format!("other-{dead}-3"));
        let odd = root.path().join("loopx-mcp-notapid-4");
        for d in [&gone, &live, &other, &odd] {
            std::fs::create_dir_all(d).unwrap();
        }
        sweep_stale(root.path(), SystemTime::now());
        assert!(!gone.exists());
        assert!(live.exists());
        assert!(other.exists());
        assert!(odd.exists());
    }

    #[test]
    fn redact_replaces_every_secret_value() {
        let f = Fixture::new();
        let a = attach(&names(&["demo"]), &f.env(), None).unwrap().unwrap();
        let text = "boom s3cret-value and again s3cret-value!";
        assert_eq!(
            a.redact(text),
            format!("boom {REDACTED} and again {REDACTED}!")
        );
        assert_eq!(redact("x", &["".into()]), "x");
    }

    #[test]
    fn stale_run_configs_older_than_a_day_are_swept() {
        let root = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let old = root.path().join(format!("loopx-mcp-{me}-1"));
        let fresh = root.path().join(format!("loopx-mcp-{me}-2"));
        let other = root.path().join("keep-me");
        for d in [&old, &fresh, &other] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("mcp.json"), "{}").unwrap();
        }
        let long_ago = SystemTime::now() - Duration::from_secs(25 * 3600);
        for d in [&old, &other] {
            std::fs::File::open(d)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(long_ago))
                .unwrap();
        }
        sweep_stale(root.path(), SystemTime::now());
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
    }

    #[test]
    fn attach_sweeps_stale_run_dirs() {
        let f = Fixture::new();
        let old = f.tmp().join(format!("loopx-mcp-{}-9", std::process::id()));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::File::open(&old)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(25 * 3600)),
            )
            .unwrap();
        let _a = attach(&names(&["demo"]), &f.env(), None).unwrap().unwrap();
        assert!(!old.exists());
    }
}

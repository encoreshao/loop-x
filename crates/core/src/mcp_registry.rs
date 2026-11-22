//! The per-machine MCP server registry, `$LOOP_ENGINEERING_HOME/mcp.json`, and
//! finding a globally installed npm server by reading files (never `npm`/`npx`).

use crate::keychain::REF_PREFIX;
use crate::node_env::Toolchains;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SERVICE_MCP: &str = "loop-engineering.mcp";
pub const PLAYWRIGHT_PACKAGE: &str = "@playwright/mcp";
pub const PLAYWRIGHT_VERSION: &str = "0.0.84";
pub const PLAYWRIGHT_BIN: &str = "playwright-mcp";
pub const PLAYWRIGHT_INSTALL_CMD: &str = "npm i -g @playwright/mcp@0.0.84";
pub const PLAYWRIGHT_DENY: [&str; 6] = [
    "browser_run_code_unsafe",
    "browser_evaluate",
    "browser_file_upload",
    "browser_install",
    "browser_drop",
    "browser_network_request",
];
/// Playwright options that would reach the user's own browser or profile, or
/// lift its file and config limits.
const PLAYWRIGHT_REFUSED_ARGS: [&str; 7] = [
    "--extension",
    "--cdp-endpoint",
    "--cdp-header",
    "--allow-unrestricted-file-access",
    "--user-data-dir",
    "--storage-state",
    "--config",
];
pub const CHROME_NOT_FOUND: &str =
    "Google Chrome was not found: install it in /Applications or set LOOPX_CHROME";

#[derive(Debug, Clone, PartialEq)]
pub enum Launch {
    Command(PathBuf),
    NpmGlobal {
        package: String,
        version: String,
        bin: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    pub launch: Launch,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Variable name -> Keychain account in [`SERVICE_MCP`].
    pub env_secrets: BTreeMap<String, String>,
    pub deny_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registry {
    pub servers: BTreeMap<String, Result<ServerSpec, String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub command: PathBuf,
    pub args_prefix: Vec<String>,
    pub path_env: String,
    pub version: String,
    pub node_version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Missing {
    NotInstalled {
        install: String,
    },
    WrongVersion {
        found: String,
        dir: PathBuf,
        install: String,
    },
    NoNode {
        dir: PathBuf,
    },
}

impl Missing {
    pub fn reason(&self) -> &'static str {
        match self {
            Missing::NotInstalled { .. } => "not_installed",
            Missing::WrongVersion { .. } => "wrong_version",
            Missing::NoNode { .. } => "no_node",
        }
    }

    pub fn install(&self) -> Option<&str> {
        match self {
            Missing::NotInstalled { install } | Missing::WrongVersion { install, .. } => {
                Some(install)
            }
            Missing::NoNode { .. } => None,
        }
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join("mcp.json")
}

fn matches(pattern: &str, s: &str) -> bool {
    regex::Regex::new(pattern).is_ok_and(|re| re.is_match(s))
}

/// `claude*` is reserved: `mcp__claude-in-chrome__*` is the user's signed-in Chrome.
pub fn valid_name(s: &str) -> bool {
    matches(r"^[a-z0-9][a-z0-9-]{0,31}$", s) && !s.starts_with("claude")
}

/// Edit/Write denials for the registry, in Claude's `//` absolute and plain forms.
pub fn edit_denied(home: &Path) -> Vec<String> {
    let file = path(home).display().to_string();
    ["Edit", "Write"]
        .iter()
        .flat_map(|t| [format!("{t}(/{file})"), format!("{t}({file})")])
        .collect()
}

fn check_playwright(args: &[String], vars: &[&String]) -> Result<(), String> {
    if let Some(bad) = args.iter().find(|a| {
        let name = a.split('=').next().unwrap_or(a);
        PLAYWRIGHT_REFUSED_ARGS.contains(&name)
    }) {
        return Err(format!("args: {bad} is not allowed for Playwright"));
    }
    if let Some(bad) = vars.iter().find(|v| v.starts_with("PLAYWRIGHT_MCP_")) {
        return Err(format!("env: {bad} is not allowed for Playwright"));
    }
    Ok(())
}

pub fn valid_account(s: &str) -> bool {
    matches(r"^[A-Za-z0-9._-]{1,64}$", s)
}

fn valid_var(s: &str) -> Result<(), String> {
    if !matches(r"^[A-Z_][A-Z0-9_]{0,63}$", s) {
        return Err(format!("{s:?} is not an UPPER_CASE variable name"));
    }
    if ["PATH", "HOME", "NODE_OPTIONS", "NODE_PATH"].contains(&s)
        || s.starts_with("DYLD_")
        || s.starts_with("LD_")
    {
        return Err(format!("{s} may not be set"));
    }
    Ok(())
}

pub fn install_command(package: &str, version: &str) -> String {
    format!("npm i -g {package}@{version}")
}

fn string_list(v: &Value, field: &str) -> Result<Vec<String>, String> {
    match v.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{field}: every item must be a string"))
            })
            .collect(),
        Some(_) => Err(format!("{field}: must be a list of strings")),
    }
}

fn string_map(v: &Value, field: &str) -> Result<BTreeMap<String, String>, String> {
    match v.get(field) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, val)| {
                let val = val
                    .as_str()
                    .ok_or_else(|| format!("{field}.{k}: must be a string"))?;
                valid_var(k).map_err(|e| format!("{field}: {e}"))?;
                Ok((k.clone(), val.to_string()))
            })
            .collect(),
        Some(_) => Err(format!("{field}: must be an object")),
    }
}

fn parse_launch(v: &Value) -> Result<Launch, String> {
    match (v.get("command"), v.get("npm_global")) {
        (Some(_), Some(_)) => Err("command: give either command or npm_global, not both".into()),
        (None, None) => Err("command: give command (an absolute path) or npm_global".into()),
        (Some(c), None) => {
            let c = c.as_str().ok_or("command: must be a string")?;
            let p = Path::new(c);
            let base = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if !p.is_absolute() {
                return Err(format!("command: {c:?} must be an absolute path"));
            }
            if matches!(base.as_deref(), Some("npx" | "npm")) {
                return Err(format!(
                    "command: {c:?} would download packages at run time"
                ));
            }
            Ok(Launch::Command(p.to_path_buf()))
        }
        (None, Some(g)) => {
            let field = |k: &str| {
                g.get(k)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("npm_global.{k}: required string"))
            };
            let (package, version, bin) = (field("package")?, field("version")?, field("bin")?);
            if !matches(r"^(@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$", &package) {
                return Err(format!(
                    "npm_global.package: {package:?} is not an npm package name"
                ));
            }
            if !matches(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$", &version) {
                return Err(format!(
                    "npm_global.version: {version:?} must be an exact version"
                ));
            }
            if !matches(r"^[A-Za-z0-9._-]{1,64}$", &bin) || bin.starts_with('.') {
                return Err(format!("npm_global.bin: {bin:?} is not a bin name"));
            }
            Ok(Launch::NpmGlobal {
                package,
                version,
                bin,
            })
        }
    }
}

impl ServerSpec {
    pub fn from_value(v: &Value) -> Result<ServerSpec, String> {
        if !v.is_object() {
            return Err("entry: must be an object".into());
        }
        let launch = parse_launch(v)?;
        let args = string_list(v, "args")?;
        let env = string_map(v, "env")?;
        let mut env_secrets = BTreeMap::new();
        for (var, value) in string_map(v, "env_secrets")? {
            let account = value
                .strip_prefix(REF_PREFIX)
                .filter(|a| valid_account(a))
                .ok_or_else(|| {
                    format!(
                        "env_secrets.{var}: must be keychain:<account> (letters, digits, . _ -)"
                    )
                })?;
            if env.contains_key(&var) {
                return Err(format!("env_secrets.{var}: also set in env"));
            }
            env_secrets.insert(var, account.to_string());
        }
        if matches!(&launch, Launch::NpmGlobal { package, .. } if package == PLAYWRIGHT_PACKAGE) {
            let vars: Vec<&String> = env.keys().chain(env_secrets.keys()).collect();
            check_playwright(&args, &vars)?;
        }
        let deny_tools = string_list(v, "deny_tools")?;
        if let Some(bad) = deny_tools
            .iter()
            .find(|t| !matches(r"^[A-Za-z0-9_-]{1,64}$", t))
        {
            return Err(format!("deny_tools: {bad:?} is not a bare tool name"));
        }
        Ok(ServerSpec {
            launch,
            args,
            env,
            env_secrets,
            deny_tools,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        match &self.launch {
            Launch::Command(p) => {
                m.insert("command".into(), Value::String(p.to_string_lossy().into()));
            }
            Launch::NpmGlobal {
                package,
                version,
                bin,
            } => {
                m.insert(
                    "npm_global".into(),
                    serde_json::json!({"package": package, "version": version, "bin": bin}),
                );
            }
        }
        m.insert("args".into(), serde_json::json!(self.args));
        m.insert("env".into(), serde_json::json!(self.env));
        let secrets: BTreeMap<&String, String> = self
            .env_secrets
            .iter()
            .map(|(k, a)| (k, format!("{REF_PREFIX}{a}")))
            .collect();
        m.insert("env_secrets".into(), serde_json::json!(secrets));
        m.insert("deny_tools".into(), serde_json::json!(self.deny_tools));
        Value::Object(m)
    }

    /// The Chrome a Playwright entry was saved with (`--executable-path`).
    pub fn chrome_path(&self) -> Option<PathBuf> {
        let at = self.args.iter().position(|a| a == "--executable-path")?;
        self.args.get(at + 1).map(PathBuf::from)
    }

    pub fn is_playwright(&self) -> bool {
        matches!(&self.launch, Launch::NpmGlobal { package, .. } if package == PLAYWRIGHT_PACKAGE)
    }
}

fn read_doc(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(format!("{}: must be a JSON object", path.display())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn load(path: &Path) -> Result<Registry, String> {
    let servers = read_doc(path)?
        .iter()
        .map(|(name, entry)| {
            let spec = if valid_name(name) {
                ServerSpec::from_value(entry)
            } else {
                Err("name: use lowercase letters, digits and -, at most 32, not starting with claude".to_string())
            };
            (name.clone(), spec)
        })
        .collect();
    Ok(Registry { servers })
}

fn rewrite(path: &Path, f: impl FnOnce(&mut Map<String, Value>) -> bool) -> Result<bool, String> {
    crate::file_lock::with_lock(path, || {
        let mut doc = read_doc(path)?;
        let changed = f(&mut doc);
        if changed {
            crate::messages::atomic_write_json(&Value::Object(doc), path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(changed)
    })
    .map_err(|e| format!("{}: {e}", path.display()))?
}

pub fn save_entry(path: &Path, name: &str, spec: &ServerSpec) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a valid server name"));
    }
    rewrite(path, |doc| {
        doc.insert(name.to_string(), spec.to_value());
        true
    })
    .map(|_| ())
}

pub fn remove_entry(path: &Path, name: &str) -> Result<bool, String> {
    rewrite(path, |doc| doc.shift_remove(name).is_some())
}

pub fn playwright_preset(chrome: Option<PathBuf>) -> Result<ServerSpec, String> {
    let chrome = chrome.ok_or(CHROME_NOT_FOUND)?;
    Ok(ServerSpec {
        launch: Launch::NpmGlobal {
            package: PLAYWRIGHT_PACKAGE.into(),
            version: PLAYWRIGHT_VERSION.into(),
            bin: PLAYWRIGHT_BIN.into(),
        },
        args: [
            "--browser",
            "chrome",
            "--executable-path",
            &chrome.to_string_lossy(),
            "--headless",
            "--isolated",
            "--no-webmcp",
        ]
        .map(str::to_string)
        .to_vec(),
        env: BTreeMap::from([("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD".into(), "1".into())]),
        env_secrets: BTreeMap::new(),
        deny_tools: PLAYWRIGHT_DENY.map(str::to_string).to_vec(),
    })
}

/// `node -v` when it is Node 18 or newer.
fn node_version(node: &Path) -> Option<String> {
    let out = crate::proc::run(
        std::process::Command::new(node).arg("-v"),
        None,
        Some(Duration::from_secs(5)),
    )
    .ok()?;
    let v = out.stdout.trim().to_string();
    (out.success() && crate::node_env::parse_version(&v).is_some_and(|p| p.0 >= 18)).then_some(v)
}

fn installed_version(cli: &Path, package: &str) -> Option<String> {
    let pkg: Value =
        serde_json::from_str(&std::fs::read_to_string(cli.parent()?.join("package.json")).ok()?)
            .ok()?;
    (pkg.get("name")?.as_str()? == package)
        .then(|| pkg.get("version")?.as_str().map(str::to_string))?
}

/// Finds `bin` of a global install of `package@version` by reading files only:
/// the version comes from the package.json next to the bin's symlink target.
pub fn resolve_npm_global(
    package: &str,
    version: &str,
    bin: &str,
    tc: &Toolchains,
    launch_path: &str,
) -> Result<Resolved, Missing> {
    let install = install_command(package, version);
    let mut dirs: Vec<PathBuf> = tc.installed().into_iter().map(|i| i.bin).collect();
    dirs.extend(
        launch_path
            .split(':')
            .map(PathBuf::from)
            .filter(|d| d.is_absolute()),
    );
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut wrong: Option<Missing> = None;
    let mut no_node: Option<Missing> = None;
    for dir in dirs {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        let Ok(cli) = std::fs::canonicalize(dir.join(bin)) else {
            continue;
        };
        let Some(found) = installed_version(&cli, package) else {
            continue;
        };
        if found != version {
            wrong.get_or_insert(Missing::WrongVersion {
                found,
                dir: dir.clone(),
                install: install.clone(),
            });
            continue;
        }
        let node = dir.join("node");
        let Some(node_version) = node_version(&node) else {
            no_node.get_or_insert(Missing::NoNode { dir: dir.clone() });
            continue;
        };
        return Ok(Resolved {
            command: node,
            args_prefix: vec![cli.to_string_lossy().into_owned()],
            path_env: format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", dir.display()),
            version: found,
            node_version,
        });
    }
    Err(no_node
        .or(wrong)
        .unwrap_or(Missing::NotInstalled { install }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn write_exec(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `<prefix>/bin/{node,playwright-mcp -> ../lib/node_modules/@playwright/mcp/cli.js}`.
    pub(crate) fn fake_install(prefix: &Path, version: &str, node_version: &str) -> PathBuf {
        let pkg = prefix.join("lib/node_modules/@playwright/mcp");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            json!({"name": PLAYWRIGHT_PACKAGE, "version": version, "bin": {"playwright-mcp": "cli.js"}})
                .to_string(),
        )
        .unwrap();
        write_exec(&pkg.join("cli.js"), "#!/usr/bin/env node\n");
        let bin = prefix.join("bin");
        write_exec(
            &bin.join("node"),
            &format!("#!/bin/sh\n[ \"$1\" = -v ] && echo {node_version}\n"),
        );
        std::os::unix::fs::symlink(
            "../lib/node_modules/@playwright/mcp/cli.js",
            bin.join(PLAYWRIGHT_BIN),
        )
        .unwrap();
        bin
    }

    fn tc(home: &Path) -> Toolchains {
        Toolchains {
            home: home.to_path_buf(),
            nvm_dir: None,
        }
    }

    fn resolve(t: &Toolchains, launch: &str) -> Result<Resolved, Missing> {
        resolve_npm_global(
            PLAYWRIGHT_PACKAGE,
            PLAYWRIGHT_VERSION,
            PLAYWRIGHT_BIN,
            t,
            launch,
        )
    }

    fn documented() -> Value {
        json!({"playwright": {"npm_global": {"package": "@playwright/mcp", "version": "0.0.84", "bin": "playwright-mcp"},
            "args": ["--browser", "chrome", "--executable-path", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                     "--headless", "--isolated", "--no-webmcp"],
            "env": {"PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD": "1"},
            "env_secrets": {},
            "deny_tools": ["browser_run_code_unsafe", "browser_evaluate", "browser_file_upload", "browser_install",
                           "browser_drop", "browser_network_request"]},
          "demo": {"command": "/usr/local/bin/demo-mcp", "env_secrets": {"DEMO_TOKEN": "keychain:demo-token"}}})
    }

    #[test]
    fn parses_the_documented_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        std::fs::write(&p, documented().to_string()).unwrap();
        let reg = load(&p).unwrap();
        let pw = reg.servers["playwright"].as_ref().unwrap();
        assert_eq!(
            pw.launch,
            Launch::NpmGlobal {
                package: PLAYWRIGHT_PACKAGE.into(),
                version: PLAYWRIGHT_VERSION.into(),
                bin: PLAYWRIGHT_BIN.into()
            }
        );
        assert_eq!(pw.args.len(), 7);
        assert_eq!(pw.env["PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD"], "1");
        assert_eq!(pw.deny_tools, PLAYWRIGHT_DENY.to_vec());
        assert_eq!(
            pw.chrome_path().unwrap(),
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        );
        let demo = reg.servers["demo"].as_ref().unwrap();
        assert_eq!(
            demo.launch,
            Launch::Command("/usr/local/bin/demo-mcp".into())
        );
        assert_eq!(demo.env_secrets["DEMO_TOKEN"], "demo-token");
        assert_eq!(ServerSpec::from_value(&pw.to_value()).unwrap(), *pw);
        assert_eq!(ServerSpec::from_value(&demo.to_value()).unwrap(), *demo);
    }

    #[test]
    fn invalid_names_vars_refs_and_commands_are_rejected_per_entry() {
        let good = json!({"command": "/usr/local/bin/x"});
        let cases: Vec<(&str, Value, &str)> = vec![
            ("Play_wright", good.clone(), "name"),
            (
                "a",
                json!({"command": "/x", "env": {"PATH": "/bin"}}),
                "env",
            ),
            ("b", json!({"command": "/x", "env": {"DYLD_X": "1"}}), "env"),
            (
                "c",
                json!({"command": "/x", "env_secrets": {"TOKEN": "plain"}}),
                "env_secrets",
            ),
            (
                "d",
                json!({"command": "/x", "env_secrets": {"TOKEN": "keychain:loop-engineering.config/gitlab"}}),
                "env_secrets",
            ),
            ("e", json!({"command": "npx"}), "command"),
            ("f", json!({"command": "bin/server"}), "command"),
            (
                "g",
                json!({"command": "/x", "npm_global": {"package": "p", "version": "1.0.0", "bin": "p"}}),
                "command",
            ),
            ("h", json!({"command": "/x", "env": {"lower": "1"}}), "env"),
            (
                "i",
                json!({"command": "/x", "deny_tools": ["bad tool"]}),
                "deny_tools",
            ),
            ("j", json!({}), "command"),
        ];
        for (name, entry, field) in cases {
            let mut doc = Map::new();
            doc.insert("ok".into(), good.clone());
            doc.insert(name.into(), entry);
            let dir = tempfile::tempdir().unwrap();
            let p = path(dir.path());
            std::fs::write(&p, Value::Object(doc).to_string()).unwrap();
            let reg = load(&p).unwrap();
            assert!(reg.servers["ok"].is_ok(), "{name}");
            let err = reg.servers[name].as_ref().unwrap_err();
            assert!(err.contains(field), "{name}: {err}");
        }
    }

    #[test]
    fn claude_names_are_reserved() {
        for name in ["claude-in-chrome", "claude", "claude-x"] {
            assert!(!valid_name(name), "{name}");
        }
        assert!(valid_name("playwright"));
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        std::fs::write(
            &p,
            json!({"claude-in-chrome": {"command": "/x"}}).to_string(),
        )
        .unwrap();
        assert!(load(&p).unwrap().servers["claude-in-chrome"].is_err());
    }

    #[test]
    fn playwright_entries_refuse_args_and_env_that_widen_it() {
        let base = playwright_preset(Some("/c".into())).unwrap().to_value();
        for arg in [
            "--extension",
            "--cdp-endpoint=ws://127.0.0.1:9222",
            "--cdp-endpoint",
            "--allow-unrestricted-file-access",
            "--user-data-dir",
            "--config",
        ] {
            let mut v = base.clone();
            v["args"].as_array_mut().unwrap().push(json!(arg));
            let e = ServerSpec::from_value(&v).unwrap_err();
            assert!(e.starts_with("args"), "{arg}: {e}");
        }
        let mut v = base.clone();
        v["env"]["PLAYWRIGHT_MCP_EXTENSION"] = json!("1");
        assert!(ServerSpec::from_value(&v).unwrap_err().starts_with("env"));
        assert!(ServerSpec::from_value(&base).is_ok());
        assert!(PLAYWRIGHT_DENY.contains(&"browser_drop"));
        assert!(PLAYWRIGHT_DENY.contains(&"browser_network_request"));
    }

    #[test]
    fn missing_file_is_empty_and_bad_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        assert!(load(&p).unwrap().servers.is_empty());
        std::fs::write(&p, "{nope").unwrap();
        assert!(load(&p).is_err());
        std::fs::write(&p, "[]").unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn save_entry_is_atomic_0600_and_keeps_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        let other = json!({"command": "/opt/x", "args": ["a"], "custom_note": "kept"});
        std::fs::write(&p, json!({"other": other}).to_string()).unwrap();
        let spec = playwright_preset(Some("/Applications/Chrome".into())).unwrap();
        save_entry(&p, "playwright", &spec).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["other"], other);
        assert_eq!(load(&p).unwrap().servers["playwright"], Ok(spec));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(save_entry(
            &p,
            "Bad_Name",
            &playwright_preset(Some("/c".into())).unwrap()
        )
        .is_err());
        assert!(remove_entry(&p, "playwright").unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc, json!({"other": other}));
        assert!(!remove_entry(&p, "playwright").unwrap());
    }

    #[test]
    fn save_entry_refuses_to_overwrite_a_file_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        std::fs::write(&p, "{broken").unwrap();
        assert!(save_entry(&p, "x", &playwright_preset(Some("/c".into())).unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{broken");
    }

    #[test]
    fn playwright_preset_pins_the_version_and_uses_this_macs_chrome() {
        let chrome = PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        let spec = playwright_preset(Some(chrome.clone())).unwrap();
        assert_eq!(
            spec.launch,
            Launch::NpmGlobal {
                package: PLAYWRIGHT_PACKAGE.into(),
                version: PLAYWRIGHT_VERSION.into(),
                bin: PLAYWRIGHT_BIN.into()
            }
        );
        let args = spec.args.join(" ");
        assert!(args.contains(&format!("--executable-path {}", chrome.display())));
        assert!(spec.args.contains(&"--headless".to_string()));
        assert!(spec.args.contains(&"--isolated".to_string()));
        assert!(spec.args.contains(&"--no-webmcp".to_string()));
        for bad in ["install", "npx", "@latest", "--caps"] {
            assert!(!args.contains(bad), "{bad}");
        }
        assert_eq!(spec.env["PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD"], "1");
        assert_eq!(spec.deny_tools, PLAYWRIGHT_DENY.to_vec());
        assert_eq!(spec.chrome_path(), Some(chrome));
        assert_eq!(playwright_preset(None).unwrap_err(), CHROME_NOT_FOUND);
    }

    #[test]
    fn resolves_a_global_install_under_nvm_and_under_homebrew() {
        let home = tempfile::tempdir().unwrap();
        let nvm_bin = fake_install(
            &home.path().join(".nvm/versions/node/v20.15.0"),
            PLAYWRIGHT_VERSION,
            "v20.15.0",
        );
        let r = resolve(&tc(home.path()), "/usr/bin:/bin").unwrap();
        assert_eq!(r.command, nvm_bin.join("node"));
        let cli = std::fs::canonicalize(nvm_bin.join(PLAYWRIGHT_BIN)).unwrap();
        assert_eq!(r.args_prefix, vec![cli.to_string_lossy().into_owned()]);
        assert!(r.path_env.starts_with(&format!("{}:", nvm_bin.display())));
        assert_eq!(r.version, PLAYWRIGHT_VERSION);
        assert_eq!(r.node_version, "v20.15.0");

        let home2 = tempfile::tempdir().unwrap();
        let brew = tempfile::tempdir().unwrap();
        let brew_bin = fake_install(brew.path(), PLAYWRIGHT_VERSION, "v22.1.0");
        let launch = format!("/usr/bin:{}:/bin", brew_bin.display());
        let r = resolve(&tc(home2.path()), &launch).unwrap();
        assert_eq!(r.command, brew_bin.join("node"));
        assert!(r.path_env.starts_with(&format!("{}:", brew_bin.display())));
    }

    #[test]
    fn wrong_version_is_reported_with_the_install_command() {
        let home = tempfile::tempdir().unwrap();
        let bin = fake_install(
            &home.path().join(".nvm/versions/node/v20.15.0"),
            "0.0.83",
            "v20.15.0",
        );
        match resolve(&tc(home.path()), "").unwrap_err() {
            Missing::WrongVersion {
                found,
                dir,
                install,
            } => {
                assert_eq!(found, "0.0.83");
                assert_eq!(dir, bin);
                assert_eq!(install, PLAYWRIGHT_INSTALL_CMD);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_matching_install_under_another_node_wins_over_a_wrong_version() {
        let home = tempfile::tempdir().unwrap();
        fake_install(
            &home.path().join(".nvm/versions/node/v22.1.0"),
            "0.0.83",
            "v22.1.0",
        );
        let good = fake_install(
            &home.path().join(".nvm/versions/node/v20.15.0"),
            PLAYWRIGHT_VERSION,
            "v20.15.0",
        );
        let r = resolve(&tc(home.path()), "").unwrap();
        assert_eq!(r.command, good.join("node"));
    }

    #[test]
    fn not_installed_and_old_node_are_reported() {
        let home = tempfile::tempdir().unwrap();
        let err = resolve(&tc(home.path()), "/nonexistent").unwrap_err();
        assert_eq!(
            err,
            Missing::NotInstalled {
                install: PLAYWRIGHT_INSTALL_CMD.into()
            }
        );
        assert_eq!(err.reason(), "not_installed");

        let old = tempfile::tempdir().unwrap();
        let bin = fake_install(old.path(), PLAYWRIGHT_VERSION, "v16.20.0");
        let err = resolve(&tc(home.path()), &bin.to_string_lossy()).unwrap_err();
        assert_eq!(err, Missing::NoNode { dir: bin.clone() });
        assert_eq!(err.reason(), "no_node");
    }

    #[test]
    fn a_bin_from_another_package_is_not_the_install() {
        let home = tempfile::tempdir().unwrap();
        let bin = fake_install(
            home.path().join("p").as_path(),
            PLAYWRIGHT_VERSION,
            "v20.0.0",
        );
        let pkg = home
            .path()
            .join("p/lib/node_modules/@playwright/mcp/package.json");
        std::fs::write(
            &pkg,
            json!({"name": "evil", "version": PLAYWRIGHT_VERSION}).to_string(),
        )
        .unwrap();
        let err = resolve(&tc(home.path()), &bin.to_string_lossy()).unwrap_err();
        assert_eq!(err.reason(), "not_installed");
    }

    #[test]
    fn resolution_never_runs_npm_or_npx() {
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("npm-ran");
        let nvm_bin = fake_install(
            &home.path().join(".nvm/versions/node/v20.15.0"),
            "0.0.83",
            "v20.15.0",
        );
        let brew = tempfile::tempdir().unwrap();
        let brew_bin = fake_install(brew.path(), PLAYWRIGHT_VERSION, "v22.1.0");
        let empty = tempfile::tempdir().unwrap();
        for dir in [&nvm_bin, &brew_bin, &empty.path().to_path_buf()] {
            for tool in ["npm", "npx"] {
                write_exec(
                    &dir.join(tool),
                    &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
                );
            }
        }
        let t = tc(home.path());
        let launch = format!("{}:{}", empty.path().display(), brew_bin.display());
        assert!(resolve(&t, &launch).is_ok());
        assert!(resolve(&t, &empty.path().to_string_lossy()).is_err());
        assert!(resolve(&tc(empty.path()), &empty.path().to_string_lossy()).is_err());
        assert!(!marker.exists());
    }
}

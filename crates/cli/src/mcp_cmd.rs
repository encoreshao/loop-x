//! `loopx mcp`: the per-machine MCP server registry (`~/.loopx/mcp.json`) and
//! its Keychain secrets. Never prints a secret value and never installs anything.

use loopx_core::keychain::{self, SecretStore};
use loopx_core::mcp_registry::{self as reg, Launch};
use loopx_core::mcp_run::{self, Env};
use loopx_core::{mcp_probe, node_env::Toolchains};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "Usage: loopx mcp list
       loopx mcp test <name>
       loopx mcp add-playwright
       loopx mcp remove <name>
       loopx mcp secret set <account>      (value read from stdin)
       loopx mcp secret delete <account>

Servers live in ~/.loopx/mcp.json; secrets in the Keychain service loop-engineering.mcp.
A loop opts in with `mcp_servers: [name]` in its loop.yaml.";

struct Ctx {
    home: PathBuf,
    repo_root: PathBuf,
    store: Arc<dyn SecretStore>,
    toolchains: Toolchains,
    launch_path: String,
    chrome: Option<PathBuf>,
}

impl Ctx {
    fn real() -> Result<Ctx, String> {
        Ok(Ctx {
            home: loopx_core::config::home_dir(),
            repo_root: loopx_core::runner_util::repo_root(),
            store: Arc::from(keychain::open_store(reg::SERVICE_MCP).map_err(|e| e.to_string())?),
            toolchains: Toolchains::from_env(),
            launch_path: loopx_core::runner_util::cli_launch_path(),
            chrome: loopx_core::linkedin_cdp::find_chrome(),
        })
    }

    fn env(&self) -> Env {
        Env {
            registry: reg::path(&self.home),
            secrets: self.store.clone(),
            toolchains: self.toolchains.clone(),
            launch_path: self.launch_path.clone(),
            tmp_root: std::env::temp_dir(),
        }
    }
}

pub fn run(args: &[String]) -> ExitCode {
    let ctx = match Ctx::real() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match dispatch(args, &ctx, &mut std::io::stdin()) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(args: &[String], ctx: &Ctx, stdin: &mut dyn Read) -> Result<String, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["list"] => list(ctx),
        ["test", name] => test(ctx, name),
        ["add-playwright"] => add_playwright(ctx),
        ["remove", name] => remove(ctx, name),
        ["secret", "set", account] => secret_set(ctx, account, stdin),
        ["secret", "delete", account] => {
            check_account(account)?;
            ctx.store.delete(account).map_err(|e| e.to_string())?;
            Ok(format!("deleted {account}"))
        }
        _ => Err(USAGE.to_string()),
    }
}

fn check_account(account: &str) -> Result<(), String> {
    if reg::valid_account(account) {
        Ok(())
    } else {
        Err(format!(
            "{account:?} is not a valid account (letters, digits, . _ -)"
        ))
    }
}

fn launch_text(launch: &Launch) -> String {
    match launch {
        Launch::Command(p) => p.display().to_string(),
        Launch::NpmGlobal {
            package, version, ..
        } => format!("{package}@{version} (global npm install)"),
    }
}

/// Ready, or why not (with the install command when one would fix it).
fn status_text(ctx: &Ctx, registry: &reg::Registry, name: &str) -> String {
    match mcp_run::resolve_server(name, registry, &ctx.env()) {
        Ok(_) => "ready".to_string(),
        Err(u) => match u.install {
            Some(cmd) => format!("{}: {}; install with: {cmd}", u.reason, u.message),
            None => format!("{}: {}", u.reason, u.message),
        },
    }
}

fn list(ctx: &Ctx) -> Result<String, String> {
    let registry = reg::load(&reg::path(&ctx.home))?;
    if registry.servers.is_empty() {
        return Ok(format!(
            "No MCP servers in {}. Add one with `loopx mcp add-playwright`.",
            reg::path(&ctx.home).display()
        ));
    }
    let stored = ctx.store.list().unwrap_or_default();
    let mut out = Vec::new();
    for (name, spec) in &registry.servers {
        match spec {
            Err(why) => out.push(format!("{name}\n  invalid: {why}")),
            Ok(spec) => {
                out.push(format!("{name}\n  launch: {}", launch_text(&spec.launch)));
                out.push(format!("  status: {}", status_text(ctx, &registry, name)));
                let mut vars: Vec<String> = spec.env.keys().cloned().collect();
                vars.extend(spec.env_secrets.iter().map(|(var, account)| {
                    let state = if stored.contains(account) {
                        "set"
                    } else {
                        "missing"
                    };
                    format!("{var} ({state})")
                }));
                if !vars.is_empty() {
                    out.push(format!("  env: {}", vars.join(", ")));
                }
            }
        }
    }
    Ok(out.join("\n"))
}

fn test(ctx: &Ctx, name: &str) -> Result<String, String> {
    let r = mcp_probe::probe(name, &ctx.env(), mcp_probe::DEADLINE);
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    mcp_probe::save_result(&mcp_probe::results_path(&ctx.repo_root), name, &r, &at)
        .map_err(|e| format!("cannot save the result: {e}"))?;
    if r.ok {
        return Ok(format!(
            "{name}: OK, {} tools: {}",
            r.tools.len(),
            r.tools.join(", ")
        ));
    }
    let mut msg = format!("{name}: test failed: {}", r.message);
    if let Some(cmd) = r.install {
        msg.push_str(&format!("\nInstall it once in Terminal: {cmd}"));
    }
    Err(msg)
}

fn add_playwright(ctx: &Ctx) -> Result<String, String> {
    let spec = reg::playwright_preset(ctx.chrome.clone())?;
    let path = reg::path(&ctx.home);
    reg::save_entry(&path, "playwright", &spec)?;
    let registry = reg::load(&path)?;
    Ok(format!(
        "Added playwright to {}\nstatus: {}",
        path.display(),
        status_text(ctx, &registry, "playwright")
    ))
}

fn remove(ctx: &Ctx, name: &str) -> Result<String, String> {
    if !reg::remove_entry(&reg::path(&ctx.home), name)? {
        return Err(format!("{name} is not registered"));
    }
    let _ = mcp_probe::remove_result(&mcp_probe::results_path(&ctx.repo_root), name);
    Ok(format!("removed {name}"))
}

fn secret_set(ctx: &Ctx, account: &str, stdin: &mut dyn Read) -> Result<String, String> {
    check_account(account)?;
    let mut value = String::new();
    stdin
        .read_to_string(&mut value)
        .map_err(|e| e.to_string())?;
    let value = value
        .strip_suffix('\n')
        .map(|v| v.strip_suffix('\r').unwrap_or(v))
        .unwrap_or(&value);
    if value.is_empty() {
        return Err("no value on stdin".into());
    }
    if value.contains(['\n', '\r']) {
        return Err("the value must be a single line".into());
    }
    ctx.store.set(account, value).map_err(|e| e.to_string())?;
    Ok(format!(
        "stored {}",
        keychain::format_ref(reg::SERVICE_MCP, account)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopx_core::keychain::MemoryStore;
    use std::os::unix::fs::PermissionsExt;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn ctx(dir: &std::path::Path, chrome: Option<PathBuf>) -> Ctx {
        Ctx {
            home: dir.join("lx"),
            repo_root: dir.join("root"),
            store: Arc::new(MemoryStore::new(reg::SERVICE_MCP)),
            toolchains: Toolchains {
                home: dir.join("home"),
                nvm_dir: None,
            },
            launch_path: String::new(),
            chrome,
        }
    }

    #[test]
    fn secret_set_reads_stdin_never_argv() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), None);
        let out = dispatch(
            &args(&["secret", "set", "demo-token"]),
            &c,
            &mut "s3cret-value\n".as_bytes(),
        )
        .unwrap();
        assert!(!out.contains("s3cret-value"), "{out}");
        assert_eq!(
            c.store.get("demo-token").unwrap().as_deref(),
            Some("s3cret-value")
        );
        for bad in ["", "\n", "two\nlines\n"] {
            assert!(
                dispatch(&args(&["secret", "set", "x"]), &c, &mut bad.as_bytes()).is_err(),
                "{bad:?}"
            );
        }
        assert!(dispatch(
            &args(&["secret", "set", "bad/name"]),
            &c,
            &mut "v".as_bytes()
        )
        .is_err());
        assert!(dispatch(
            &args(&["secret", "set", "demo-token", "inline"]),
            &c,
            &mut "v".as_bytes()
        )
        .is_err());
        dispatch(
            &args(&["secret", "delete", "demo-token"]),
            &c,
            &mut "".as_bytes(),
        )
        .unwrap();
        assert!(c.store.get("demo-token").unwrap().is_none());
    }

    #[test]
    fn list_never_prints_secret_values() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), None);
        let exe = d.path().join("demo-mcp");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = reg::ServerSpec::from_value(&serde_json::json!({
            "command": exe, "env": {"MODE": "plain-value"},
            "env_secrets": {"DEMO_TOKEN": "keychain:demo-token", "OTHER": "keychain:other"}}))
        .unwrap();
        reg::save_entry(&reg::path(&c.home), "demo", &spec).unwrap();
        c.store.set("demo-token", "s3cret-value").unwrap();
        let out = dispatch(&args(&["list"]), &c, &mut "".as_bytes()).unwrap();
        assert!(!out.contains("s3cret-value"), "{out}");
        assert!(out.contains("demo"), "{out}");
        assert!(out.contains("DEMO_TOKEN (set)"), "{out}");
        assert!(out.contains("OTHER (missing)"), "{out}");
        assert!(out.contains("secret_missing"), "{out}");
        assert!(out.contains("MODE"), "{out}");
    }

    #[test]
    fn add_playwright_writes_the_pinned_preset_and_prints_the_install_command_when_missing() {
        let d = tempfile::tempdir().unwrap();
        let chrome = d.path().join("Chrome");
        std::fs::write(&chrome, "").unwrap();
        let c = ctx(d.path(), Some(chrome.clone()));
        let out = dispatch(&args(&["add-playwright"]), &c, &mut "".as_bytes()).unwrap();
        assert!(out.contains(reg::PLAYWRIGHT_INSTALL_CMD), "{out}");
        let saved = reg::load(&reg::path(&c.home)).unwrap();
        assert_eq!(
            saved.servers["playwright"],
            Ok(reg::playwright_preset(Some(chrome)).unwrap())
        );
        let list = dispatch(&args(&["list"]), &c, &mut "".as_bytes()).unwrap();
        assert!(list.contains("not_installed"), "{list}");
        assert!(list.contains(reg::PLAYWRIGHT_INSTALL_CMD), "{list}");
        let none = ctx(d.path(), None);
        let e = dispatch(&args(&["add-playwright"]), &none, &mut "".as_bytes()).unwrap_err();
        assert_eq!(e, reg::CHROME_NOT_FOUND);
        dispatch(&args(&["remove", "playwright"]), &c, &mut "".as_bytes()).unwrap();
        assert!(reg::load(&reg::path(&c.home)).unwrap().servers.is_empty());
        assert!(dispatch(&args(&["remove", "playwright"]), &c, &mut "".as_bytes()).is_err());
    }

    #[test]
    fn test_reports_a_missing_install_without_starting_anything() {
        let d = tempfile::tempdir().unwrap();
        let chrome = d.path().join("Chrome");
        std::fs::write(&chrome, "").unwrap();
        let c = ctx(d.path(), Some(chrome));
        dispatch(&args(&["add-playwright"]), &c, &mut "".as_bytes()).unwrap();
        let e = dispatch(&args(&["test", "playwright"]), &c, &mut "".as_bytes()).unwrap_err();
        assert!(e.contains(reg::PLAYWRIGHT_INSTALL_CMD), "{e}");
        let saved = mcp_probe::load_results(&mcp_probe::results_path(&c.repo_root));
        assert!(!saved["playwright"].ok);
    }
}

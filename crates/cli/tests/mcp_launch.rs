//! `loopx mcp-launch`: the real binary starting a server from a run's MCP config,
//! against scratch file-backed secret stores (never the Keychain).

use loopx_core::keychain::{sandboxed_service_for, FileStore, SecretStore};
use loopx_core::mcp_registry as reg;
use loopx_core::mcp_run::{self, Env};
use loopx_core::node_env::Toolchains;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct Fx {
    d: tempfile::TempDir,
}

impl Fx {
    fn new() -> Self {
        let fx = Self {
            d: tempfile::tempdir().unwrap(),
        };
        for dir in ["home", "keys", "tmp"] {
            std::fs::create_dir_all(fx.path(dir)).unwrap();
        }
        let server = fx.path("server.sh");
        std::fs::write(
            &server,
            format!("#!/bin/sh\nenv > '{}'\n", fx.path("env.txt").display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = reg::ServerSpec::from_value(&json!({
            "command": server, "env": {"MODE": "on"},
            "env_secrets": {"TOKEN": "keychain:demo-token"}}))
        .unwrap();
        reg::save_entry(&reg::path(&fx.path("home")), "demo", &spec).unwrap();
        fx.store(reg::SERVICE_MCP)
            .set("demo-token", "s3cret")
            .unwrap();
        fx
    }

    fn path(&self, p: &str) -> PathBuf {
        self.d.path().join(p)
    }

    /// The store `loopx` itself opens with this home and `LOOPX_KEYCHAIN=file:<keys>`.
    fn store(&self, service: &str) -> Arc<dyn SecretStore> {
        Arc::new(FileStore::new(
            &self.path("keys"),
            &sandboxed_service_for(service, Some(&self.path("home"))),
        ))
    }

    fn env(&self) -> Env {
        Env {
            registry: reg::path(&self.path("home")),
            secrets: self.store(reg::SERVICE_MCP),
            toolchains: Toolchains {
                home: self.path("home"),
                nvm_dir: None,
            },
            launch_path: "/usr/bin:/bin".into(),
            tmp_root: self.path("tmp"),
            launcher: PathBuf::from(env!("CARGO_BIN_EXE_loopx")),
            launch_keys: self.store(mcp_run::LAUNCH_KEY_SERVICE),
        }
    }

    fn start(&self, command: &Value, args: &[String]) -> std::process::ExitStatus {
        Command::new(command.as_str().unwrap())
            .args(args)
            .env("LOOP_ENGINEERING_HOME", self.path("home"))
            .env(
                "LOOPX_KEYCHAIN",
                format!("file:{}", self.path("keys").display()),
            )
            .env("HOME", "/Users/me")
            .env("NODE_OPTIONS", "--require /tmp/evil.js")
            .status()
            .unwrap()
    }
}

fn config(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_launcher_starts_the_server_with_its_secret_and_nothing_smuggled_in() {
    let fx = Fx::new();
    let attach = mcp_run::attach(&["demo".to_string()], &fx.env(), None)
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(&attach.config).unwrap();
    assert!(!text.contains("s3cret"), "{text}");
    let demo = &config(&attach.config)["mcpServers"]["demo"];
    let args: Vec<String> = serde_json::from_value(demo["args"].clone()).unwrap();

    assert!(fx.start(&demo["command"], &args).success());
    let env = std::fs::read_to_string(fx.path("env.txt")).unwrap();
    assert!(env.contains("TOKEN=s3cret\n"), "{env}");
    assert!(env.contains("MODE=on\n"), "{env}");
    assert!(env.contains("HOME=/Users/me\n"), "{env}");
    assert!(env.contains("PATH=/usr/bin:/bin\n"), "{env}");
    assert!(!env.contains("NODE_OPTIONS"), "{env}");
    assert!(!env.contains("LOOPX_KEYCHAIN"), "{env}");
}

#[test]
fn a_forged_launch_is_refused_and_starts_nothing() {
    let fx = Fx::new();
    let attach = mcp_run::attach(&["demo".to_string()], &fx.env(), None)
        .unwrap()
        .unwrap();
    let demo = &config(&attach.config)["mcpServers"]["demo"];
    let mut args: Vec<String> = serde_json::from_value(demo["args"].clone()).unwrap();
    args[2] = "00".repeat(32);
    let status = fx.start(&demo["command"], &args);
    assert_eq!(status.code(), Some(2));
    assert!(!fx.path("env.txt").exists());
}

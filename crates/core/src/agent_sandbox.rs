//! A macOS sandbox (`sandbox-exec`) around the AI CLIs that run with every tool
//! call approved (Gemini, Kimi, Grok, Cursor). Inside it the agent may write only
//! to its work folders, temp folders, LoopX's own state and the CLIs' own state,
//! and may not read or write credential files (SSH keys, cloud and package
//! registry tokens, gh and git credentials). Network and everything else stay
//! open: the agent still needs its model API, git and `loopx`. Paths reach the
//! profile as `-D` parameters, so no path is ever spliced into its text.

use std::path::{Path, PathBuf};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// `off` runs the CLIs without the sandbox.
pub const SWITCH_ENV: &str = "LOOPX_AGENT_SANDBOX";

/// Files and folders under `$HOME` that hold credentials.
pub const SECRET_PATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".password-store",
    ".terraform.d",
    ".config/gcloud",
    ".config/gh",
    ".config/hub",
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".vault-token",
    ".gem/credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    "Library/Keychains",
];

/// Folders under `$HOME` the CLIs keep their own state, caches and logs in, and
/// the toolchains a coding agent installs dependencies into.
pub const CLI_STATE_PATHS: &[&str] = &[
    ".gemini",
    ".kimi",
    ".cursor",
    ".grok",
    ".config",
    ".cache",
    ".local",
    "Library/Caches",
    "Library/Application Support",
    "Library/Logs",
    ".npm",
    ".bun",
    ".yarn",
    ".pnpm-store",
    "Library/pnpm",
    ".nvm",
    ".volta",
    ".deno",
    ".gem",
    ".bundle",
    ".rbenv",
    ".rvm",
    ".asdf",
    ".pyenv",
    ".rustup",
    ".cargo",
    "go",
    ".m2",
    ".gradle",
    ".sdkman",
];

/// Homebrew's prefixes, so an agent can install a missing tool.
pub const SYSTEM_TOOL_PATHS: &[&str] = &["/opt/homebrew", "/usr/local"];

/// What the sandbox lets through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    pub writable: Vec<PathBuf>,
    pub secret: Vec<PathBuf>,
}

impl Fence {
    /// The fence for a run whose work folders are `roots`: those, `shared`
    /// (temp folders, LoopX's home and outputs) and the CLIs' state under
    /// `home` are writable; [`SECRET_PATHS`] under `home` are closed.
    pub fn for_run(roots: &[PathBuf], home: &Path, shared: &[PathBuf]) -> Self {
        let mut writable: Vec<PathBuf> = roots.to_vec();
        writable.extend(shared.iter().cloned());
        writable.extend(CLI_STATE_PATHS.iter().map(|p| home.join(p)));
        Self {
            writable: dedup(writable.iter().map(|p| real(p)).collect()),
            secret: dedup(SECRET_PATHS.iter().map(|p| real(&home.join(p))).collect()),
        }
    }

    /// The production fence: LoopX's home and the outputs checkout as well.
    pub fn live(roots: &[PathBuf]) -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        Self::for_run(
            roots,
            &home,
            &[
                std::env::temp_dir(),
                PathBuf::from("/private/tmp"),
                PathBuf::from("/private/var/folders"),
                PathBuf::from(SYSTEM_TOOL_PATHS[0]),
                PathBuf::from(SYSTEM_TOOL_PATHS[1]),
                crate::config::home_dir(),
                crate::runner_util::repo_root(),
            ],
        )
    }

    /// `sandbox-exec -D W0=… -D S0=… -p <profile> <argv…>`.
    pub fn wrap(&self, argv: Vec<String>) -> Vec<String> {
        let mut out = vec![SANDBOX_EXEC.to_string()];
        for (i, p) in self.writable.iter().enumerate() {
            out.extend(["-D".into(), format!("W{i}={}", p.display())]);
        }
        for (i, p) in self.secret.iter().enumerate() {
            out.extend(["-D".into(), format!("S{i}={}", p.display())]);
        }
        out.extend(["-p".into(), self.profile()]);
        out.extend(argv);
        out
    }

    /// Later rules win: everything is allowed, then every write is denied, then
    /// the writable folders are allowed again, and the credential paths are
    /// closed last so nothing above reopens them.
    pub fn profile(&self) -> String {
        let params = |prefix: &str, n: usize| -> String {
            (0..n)
                .map(|i| format!(" (subpath (param \"{prefix}{i}\"))"))
                .collect()
        };
        let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        p.push_str(&format!(
            "(allow file-write*{} (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/(tty|fd/|ttys|pty)\"))\n",
            params("W", self.writable.len())
        ));
        if !self.secret.is_empty() {
            p.push_str(&format!(
                "(deny file-read* file-write*{})\n",
                params("S", self.secret.len())
            ));
        }
        p
    }
}

/// Whether LoopX fences the unrestricted CLIs on this machine.
pub fn enabled() -> bool {
    std::env::var(SWITCH_ENV).map_or(true, |v| v != "off") && Path::new(SANDBOX_EXEC).exists()
}

/// `argv` wrapped in the live fence for `roots`, its program resolved on `PATH`;
/// unchanged when the sandbox is off or unavailable, or the program is not
/// installed (so the caller's own "could not start" message still applies).
pub fn wrap_live(mut argv: Vec<String>, roots: &[PathBuf]) -> Vec<String> {
    if !enabled() || argv.is_empty() {
        return argv;
    }
    if !argv[0].contains('/') {
        let path = std::env::var("PATH").unwrap_or_default();
        match crate::doctor::find_on_path(&argv[0], &path) {
            Some(found) => argv[0] = found.display().to_string(),
            None => return argv,
        }
    }
    Fence::live(roots).wrap(argv)
}

/// The command a wrapped `argv` runs (`argv` itself when it is not wrapped).
pub fn inner(argv: &[String]) -> &[String] {
    if argv.first().map(String::as_str) != Some(SANDBOX_EXEC) {
        return argv;
    }
    match argv.iter().position(|a| a == "-p") {
        Some(i) if i + 2 <= argv.len() => &argv[i + 2..],
        _ => argv,
    }
}

/// The sandbox matches resolved paths (`/tmp` is `/private/tmp`); a path that
/// does not exist yet keeps its resolved parent.
fn real(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if parent != p => real(parent).join(name),
        _ => p.to_path_buf(),
    }
}

fn dedup(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    v
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::process::Command;

    struct Scene {
        _d: tempfile::TempDir,
        home: PathBuf,
        work: PathBuf,
        fence: Fence,
    }

    fn sandbox() -> Scene {
        let d = tempfile::tempdir().unwrap();
        let home = real(&d.path().join("home"));
        let work = real(&d.path().join("work"));
        for p in [".ssh", ".config/gh", ".gemini"] {
            std::fs::create_dir_all(home.join(p)).unwrap();
        }
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
        std::fs::write(home.join(".config/gh/hosts.yml"), "oauth_token: gho_x").unwrap();
        std::fs::write(home.join(".netrc"), "password p").unwrap();
        let fence = Fence::for_run(std::slice::from_ref(&work), &home, &[]);
        Scene {
            _d: d,
            home,
            work,
            fence,
        }
    }

    fn sh(b: &Scene, script: &str) -> (bool, String) {
        let argv = b
            .fence
            .wrap(vec!["/bin/sh".into(), "-c".into(), script.into()]);
        let out = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn an_agent_cannot_read_credentials_but_can_work_in_its_folders() {
        let b = sandbox();
        for secret in [".ssh/id_ed25519", ".config/gh/hosts.yml", ".netrc"] {
            let (ok, out) = sh(&b, &format!("cat '{}'", b.home.join(secret).display()));
            assert!(!ok && out.is_empty(), "{secret} was readable: {out}");
        }
        let f = b.work.join("main.rs");
        let (ok, out) = sh(&b, &format!("echo fn > '{0}' && cat '{0}'", f.display()));
        assert!(ok && out == "fn\n", "{out}");
        let state = b.home.join(".gemini/settings.json");
        assert!(sh(&b, &format!("echo {{}} > '{}'", state.display())).0);
        assert!(sh(&b, "echo x > /dev/null").0);
    }

    #[test]
    fn an_agent_cannot_plant_anything_outside_its_folders() {
        let b = sandbox();
        for target in [
            ".zshrc",
            "Library/LaunchAgents/evil.plist",
            ".ssh/authorized_keys",
        ] {
            let p = b.home.join(target);
            let (ok, _) = sh(
                &b,
                &format!(
                    "mkdir -p '{}' 2>/dev/null; echo x > '{}'",
                    p.parent().unwrap().display(),
                    p.display()
                ),
            );
            assert!(!ok && !p.exists(), "{target} was written");
        }
    }

    #[test]
    fn paths_reach_the_profile_as_parameters_only() {
        let b = sandbox();
        let argv = b
            .fence
            .wrap(vec!["gemini".into(), "-p".into(), "hi".into()]);
        let profile = &argv[argv.iter().position(|a| a == "-p").unwrap() + 1];
        assert!(
            !profile.contains(&b.home.display().to_string()),
            "{profile}"
        );
        assert_eq!(argv[0], SANDBOX_EXEC);
        assert_eq!(&argv[argv.len() - 3..], ["gemini", "-p", "hi"]);
        assert!(argv.contains(&format!("W0={}", b.work.display())));
    }
}

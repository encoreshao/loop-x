//! Opt-in install of the Slack listener as a launchd agent. Nothing here runs
//! unless the user asks: saving a connector account and its tokens writes no
//! plist and loads no agent; only [`install`] does, after [`preflight`] has
//! checked everything the listener needs. The plist carries no token (the
//! listener reads them from the Keychain), only paths.
//!
//! `KeepAlive` is `{SuccessfulExit: false}`, not `true`: the listener exits 0
//! on a rejected token and on a clean stop and must stay down then (a relaunch
//! would hit Slack again with the same bad token), while 78 (misconfigured) and
//! 1 (anything else) are relaunched, `ThrottleInterval` apart.

use crate::connectors::slack_bot::{app_token_key, listener_account, parse_allowlist};
use crate::connectors_config::{list_accounts, Paths};
use crate::keychain::SecretStore;
use crate::launchd::{self, Launchctl};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LABEL: &str = "com.hermes.loopx-slack";
pub const PLIST_NAME: &str = "com.hermes.loopx-slack.plist";

/// Where the plist points. All paths, no secrets.
#[derive(Debug, Clone)]
pub struct Launch {
    pub loopx_bin: PathBuf,
    /// Receives `slack-listener.out.log` / `.err.log`.
    pub log_dir: PathBuf,
    /// The user's home: only for the agent CLIs' `PATH`.
    pub user_home: PathBuf,
    /// Loopx variables of the installer's own environment, copied unchanged
    /// ([`pinned_env_from`]). Empty for a normal install.
    pub pinned_env: Vec<(String, String)>,
}

/// Variables that change where loopx looks for its config and secrets.
/// `LOOP_ENGINEERING_HOME` even changes the Keychain service name
/// (`keychain::sandboxed_service`), so pinning it when the installer did not
/// have it would make the listener read a different service than the one the
/// tokens were saved to.
pub const PINNED_VARS: [&str; 3] = ["LOOP_ENGINEERING_HOME", "LOOPX_ROOT", "LOOPX_KEYCHAIN"];

/// The [`PINNED_VARS`] that are set and non-empty, values unchanged.
pub fn pinned_env_from(get: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    PINNED_VARS
        .iter()
        .filter_map(|k| get(k).filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect()
}

/// The agent's plist. No token, no secret-bearing variable.
pub fn render_plist(l: &Launch) -> String {
    let path = format!(
        "/opt/homebrew/bin:/usr/local/bin:{}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        l.user_home.display()
    );
    let mut env = serde_json::Map::new();
    env.insert("PATH".into(), json!(path));
    for (k, v) in &l.pinned_env {
        env.insert(k.clone(), json!(v));
    }
    let v = json!({
        "Label": LABEL,
        "ProgramArguments": [l.loopx_bin.display().to_string(), "slack", "listen"],
        "EnvironmentVariables": env,
        "RunAtLoad": true,
        "KeepAlive": {"SuccessfulExit": false},
        "ThrottleInterval": 30,
        "StandardOutPath": l.log_dir.join("slack-listener.out.log").display().to_string(),
        "StandardErrorPath": l.log_dir.join("slack-listener.err.log").display().to_string(),
    });
    launchd::dump_plist(&v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    NoAccount,
    /// The Slack agent account exists but is switched off.
    Disabled(String),
    NoBotToken,
    NoAppToken,
    EmptyAllowlist,
    /// The secret store could not be read.
    SecretStore,
    /// Writing the plist or its directories failed.
    Io(String),
    /// `launchctl` refused or could not run (its message, token-free).
    Launchctl(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAccount => {
                write!(
                    f,
                    "No Slack agent is set up: add one in Settings > Connectors"
                )
            }
            Self::Disabled(id) => write!(f, "The Slack agent {id} is disabled: enable it first"),
            Self::NoBotToken => write!(f, "The Slack bot token (xoxb-) is not set"),
            Self::NoAppToken => write!(f, "The Slack app-level token (xapp-) is not set"),
            Self::EmptyAllowlist => write!(
                f,
                "No allowed Slack users are set: add at least one Slack user ID"
            ),
            Self::SecretStore => write!(f, "Cannot read the Slack tokens from the secret store"),
            Self::Io(e) => write!(f, "Cannot write the launch agent: {e}"),
            Self::Launchctl(e) => write!(f, "launchctl failed: {e}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// Everything the listener needs before it is worth installing. Reads only;
/// never writes, never calls launchctl.
pub fn preflight(paths: &Paths, store: &dyn SecretStore) -> Result<(), InstallError> {
    let accounts = list_accounts(paths).map_err(|_| InstallError::NoAccount)?;
    let first = accounts
        .iter()
        .find(|a| a.kind == "slack_bot")
        .map(|a| a.id.clone())
        .ok_or(InstallError::NoAccount)?;
    let account = listener_account(accounts).ok_or(InstallError::Disabled(first))?;
    let has = |key: &str| -> Result<bool, InstallError> {
        store
            .get(key)
            .map(|v| v.is_some_and(|s| !s.trim().is_empty()))
            .map_err(|_| InstallError::SecretStore)
    };
    if !has(&account.id)? {
        return Err(InstallError::NoBotToken);
    }
    if !has(&app_token_key(&account.id))? {
        return Err(InstallError::NoAppToken);
    }
    if parse_allowlist(account.setting("allowed_users")).is_empty() {
        return Err(InstallError::EmptyAllowlist);
    }
    Ok(())
}

const TIMEOUT: Duration = Duration::from_secs(10);

pub fn is_installed(agents_dir: &Path) -> bool {
    agents_dir.join(PLIST_NAME).exists()
}

/// Preflight, write the plist into `launchd_dir`, copy it to `agents_dir` and
/// `launchctl load -w` it. Installing again reloads. A failure leaves nothing
/// installed. Nothing is written when preflight fails.
pub fn install(
    paths: &Paths,
    store: &dyn SecretStore,
    launchd_dir: &Path,
    agents_dir: &Path,
    ctl: &Launchctl,
    launch: &Launch,
) -> Result<(), InstallError> {
    preflight(paths, store)?;
    let io = |e: std::io::Error| InstallError::Io(e.to_string());
    std::fs::create_dir_all(&launch.log_dir).map_err(io)?;
    std::fs::create_dir_all(launchd_dir).map_err(io)?;
    if is_installed(agents_dir) {
        // Already loaded: a plain `load` would be refused. Best effort.
        let _ = unload(launchd_dir, agents_dir, ctl);
    }
    let src = launchd_dir.join(PLIST_NAME);
    std::fs::write(&src, render_plist(launch)).map_err(io)?;
    let (ok, msg) = launchd::enable_daemon_managed(PLIST_NAME, launchd_dir, agents_dir, ctl);
    if !ok {
        let _ = std::fs::remove_file(&src);
        return Err(InstallError::Launchctl(msg.english()));
    }
    Ok(())
}

/// `launchctl unload -w` the installed copy; the project copy is restored from
/// it first when missing, because `disable_daemon` only knows project daemons.
fn unload(launchd_dir: &Path, agents_dir: &Path, ctl: &Launchctl) -> Result<(), String> {
    let src = launchd_dir.join(PLIST_NAME);
    if !src.exists() {
        std::fs::create_dir_all(launchd_dir).map_err(|e| e.to_string())?;
        std::fs::copy(agents_dir.join(PLIST_NAME), &src).map_err(|e| e.to_string())?;
    }
    let (ok, msg) = launchd::disable_daemon_managed(PLIST_NAME, launchd_dir, agents_dir, ctl);
    if ok {
        Ok(())
    } else {
        Err(msg.english())
    }
}

/// Unload the agent and remove the plist from both directories. Idempotent;
/// calls launchctl only when an installed copy exists. When the unload fails
/// but launchd no longer knows the agent, the files are removed anyway; while
/// it is still loaded they are kept and the error returned, so a retry works.
pub fn uninstall(launchd_dir: &Path, agents_dir: &Path, ctl: &Launchctl) -> Result<(), String> {
    if is_installed(agents_dir) {
        if let Err(e) = unload(launchd_dir, agents_dir, ctl) {
            let still_loaded = launchd::loaded_by_label(&ctl.list_output()).contains_key(LABEL);
            if still_loaded {
                return Err(e);
            }
        }
    }
    for dir in [agents_dir, launchd_dir] {
        match std::fs::remove_file(dir.join(PLIST_NAME)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Start the listener again (it exits 0 on a rejected token and launchd does
/// not relaunch it): `launchctl kickstart -k gui/<uid>/<label>` terminates a
/// running copy and starts a fresh one, and starts a stopped one. The agent is
/// loaded with `load -w` into the user's GUI domain, which is the target.
pub fn restart(agents_dir: &Path, ctl: &Launchctl, uid: u32) -> Result<(), String> {
    if !is_installed(agents_dir) {
        return Err("The Slack listener is not installed".to_string());
    }
    let target = format!("gui/{uid}/{LABEL}");
    match ctl.run(&["kickstart", "-k", &target], TIMEOUT) {
        Err(e) => Err(format!("launchctl kickstart failed to run: {e}")),
        Ok(r) if r.code != 0 => {
            let s = r.stderr.trim();
            Err(if s.is_empty() {
                format!("launchctl kickstart exited {}", r.code)
            } else {
                s.to_string()
            })
        }
        Ok(_) => Ok(()),
    }
}

/// The Slack app manifest to paste into "Create app > From a manifest".
pub fn app_manifest_yaml() -> String {
    "\
display_information:
  name: LoopX
  description: Talk to LoopX from Slack
features:
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
  bot_user:
    display_name: LoopX
    always_online: true
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - chat:write
      - reactions:write
      - im:history
      - im:write
      # Optional, for offline catch-up (offers to run mentions and DMs sent
      # while the listener was away). Uncomment all seven to turn it on:
      # listing conversations needs the four :read scopes, reading them the
      # :history ones (im:history is already above).
      # - channels:read
      # - groups:read
      # - im:read
      # - mpim:read
      # - channels:history
      # - groups:history
      # - mpim:history
settings:
  event_subscriptions:
    bot_events:
      - app_mention
      - message.im
  interactivity:
    is_enabled: true
  org_deploy_enabled: false
  socket_mode_enabled: true
  token_rotation_enabled: false
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::slack_bot::app_token_key;
    use crate::keychain::{MemoryStore, SecretStore};
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    struct Fx {
        dir: tempfile::TempDir,
    }

    impl Fx {
        fn new() -> Self {
            Fx {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn p(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }
        fn launchd(&self) -> PathBuf {
            self.p("launchd")
        }
        fn agents(&self) -> PathBuf {
            self.p("agents")
        }
        fn paths(&self, entries: Value) -> Paths {
            let config = self.p("connectors.json");
            std::fs::write(&config, entries.to_string()).unwrap();
            Paths {
                config_path: Some(config),
                gitlab_config_path: Some(self.p("gitlab.json")),
                slack_config_path: Some(self.p("slack.json")),
                inbox_config_path: Some(self.p("inboxes.json")),
            }
        }
        fn launch(&self) -> Launch {
            Launch {
                loopx_bin: PathBuf::from("/opt/loopx/bin/loopx"),
                log_dir: self.p("logs"),
                user_home: PathBuf::from("/Users/someone"),
                pinned_env: Vec::new(),
            }
        }
        /// A fake `launchctl` that logs its argv; `fail` makes `load` fail,
        /// `list` is what `launchctl list` prints.
        #[cfg(unix)]
        fn ctl(&self, fail: Option<&str>, list: &str) -> Launchctl {
            use std::os::unix::fs::PermissionsExt;
            let script = self.p("fake-launchctl");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\necho \"$@\" >> {log}\nif [ \"$1\" = list ]; then printf '%s' '{list}'; exit 0; fi\nif [ \"$1\" = \"{fail}\" ]; then echo 'boom' >&2; exit 5; fi\nexit 0\n",
                    log = self.p("calls.log").display(),
                    fail = fail.unwrap_or("-none-"),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            Launchctl {
                command: vec![script.display().to_string()],
                list_file: None,
            }
        }
        /// The argv lines the fake recorded (`list` probes excluded).
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.p("calls.log"))
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.starts_with("list"))
                .map(String::from)
                .collect()
        }
    }

    fn bot(id: &str, enabled: bool, allow: &str) -> Value {
        json!({"id": id, "type": "slack_bot", "label": id, "enabled": enabled,
               "settings": {"allowed_users": allow}})
    }

    fn store(id: &str, bot_token: bool, app_token: bool) -> MemoryStore {
        let s = MemoryStore::new("test");
        if bot_token {
            s.set(id, "xoxb-SECRET").unwrap();
        }
        if app_token {
            s.set(&app_token_key(id), "xapp-SECRET").unwrap();
        }
        s
    }

    fn ready(fx: &Fx) -> (Paths, MemoryStore) {
        (
            fx.paths(json!([bot("agent", true, "U01ABC")])),
            store("agent", true, true),
        )
    }

    fn dir_is_empty(p: &Path) -> bool {
        std::fs::read_dir(p).map_or(true, |mut d| d.next().is_none())
    }

    #[test]
    fn the_plist_runs_the_listener_and_carries_no_secrets() {
        let fx = Fx::new();
        let text = render_plist(&fx.launch());
        let v = crate::launchd::parse_plist(&text).unwrap();
        assert_eq!(v["Label"], LABEL);
        assert_eq!(v["RunAtLoad"], true);
        // Relaunch only after a failure: the listener exits 0 on a rejected
        // token and on a clean stop, and must stay down then.
        assert_eq!(v["KeepAlive"], json!({"SuccessfulExit": false}));
        assert_eq!(v["ThrottleInterval"], 30);
        assert_eq!(
            v["ProgramArguments"],
            json!(["/opt/loopx/bin/loopx", "slack", "listen"])
        );
        let logs = fx.p("logs").display().to_string();
        assert!(v["StandardOutPath"].as_str().unwrap().starts_with(&logs));
        assert!(v["StandardErrorPath"].as_str().unwrap().starts_with(&logs));
        let lower = text.to_lowercase();
        for banned in ["xoxb", "xapp", "token"] {
            assert!(!lower.contains(banned), "plist mentions {banned}");
        }
        // A normal install pins only PATH (like the other agents): pinning
        // LOOP_ENGINEERING_HOME would change the Keychain service name.
        let env = v["EnvironmentVariables"].as_object().unwrap();
        let keys: Vec<_> = env.keys().cloned().collect();
        assert_eq!(keys, ["PATH"]);
        assert!(env["PATH"]
            .as_str()
            .unwrap()
            .contains("/Users/someone/.local/bin"));
    }

    #[test]
    fn loopx_variables_are_pinned_only_when_the_installer_has_them_unchanged() {
        let fx = Fx::new();
        let get = |k: &str| match k {
            "LOOP_ENGINEERING_HOME" => Some("/scratch/home".to_string()),
            "LOOPX_KEYCHAIN" => Some("file:/scratch/keys".to_string()),
            "LOOPX_ROOT" => Some(String::new()), // set but empty: not pinned
            _ => None,
        };
        let mut l = fx.launch();
        l.pinned_env = pinned_env_from(&get);
        let v = crate::launchd::parse_plist(&render_plist(&l)).unwrap();
        let env = v["EnvironmentVariables"].as_object().unwrap();
        assert_eq!(env["LOOP_ENGINEERING_HOME"], "/scratch/home");
        assert_eq!(env["LOOPX_KEYCHAIN"], "file:/scratch/keys");
        assert!(!env.contains_key("LOOPX_ROOT"));
        assert!(env.contains_key("PATH"));
        assert!(pinned_env_from(&|_| None).is_empty());
    }

    /// The listener must open the Keychain service the installer's own process
    /// (dashboard / CLI) saved the tokens to.
    #[test]
    fn the_listener_computes_the_same_keychain_service_as_the_installer() {
        let fx = Fx::new();
        let scratch = fx.dir.path().display().to_string();
        let cases: [Option<String>; 2] = [None, Some(scratch)];
        for installer_home in cases {
            let get = |k: &str| {
                (k == "LOOP_ENGINEERING_HOME")
                    .then(|| installer_home.clone())
                    .flatten()
            };
            let mut l = fx.launch();
            l.pinned_env = pinned_env_from(&get);
            let v = crate::launchd::parse_plist(&render_plist(&l)).unwrap();
            let in_plist = v["EnvironmentVariables"]["LOOP_ENGINEERING_HOME"].as_str();
            let base = crate::keychain::SERVICE_CONFIG;
            assert_eq!(
                crate::keychain::sandboxed_service_for(base, in_plist.map(Path::new)),
                crate::keychain::sandboxed_service_for(
                    base,
                    installer_home.as_deref().map(Path::new)
                ),
                "installer home {installer_home:?}"
            );
        }
    }

    #[test]
    fn preflight_names_the_first_thing_missing() {
        let fx = Fx::new();
        let none = fx.paths(json!([]));
        assert_eq!(
            preflight(&none, &store("agent", true, true)),
            Err(InstallError::NoAccount)
        );
        // A plain Slack webhook is not the agent.
        let hook = fx.paths(json!([{"id": "h", "type": "slack", "settings": {}}]));
        assert_eq!(
            preflight(&hook, &store("h", true, true)),
            Err(InstallError::NoAccount)
        );

        let p = fx.paths(json!([bot("agent", true, "U01ABC")]));
        assert_eq!(
            preflight(&p, &store("agent", false, true)),
            Err(InstallError::NoBotToken)
        );
        assert_eq!(
            preflight(&p, &store("agent", true, false)),
            Err(InstallError::NoAppToken)
        );
        assert_eq!(preflight(&p, &store("agent", true, true)), Ok(()));
    }

    #[test]
    fn an_empty_or_invalid_allowlist_refuses() {
        let fx = Fx::new();
        for allow in ["", "  ", "alice, bob", "channel-C01"] {
            let p = fx.paths(json!([bot("agent", true, allow)]));
            assert_eq!(
                preflight(&p, &store("agent", true, true)),
                Err(InstallError::EmptyAllowlist),
                "{allow:?}"
            );
        }
    }

    #[test]
    fn a_disabled_account_refuses_with_a_clear_message() {
        let fx = Fx::new();
        let p = fx.paths(json!([bot("agent", false, "U01ABC")]));
        let e = preflight(&p, &store("agent", true, true)).unwrap_err();
        assert_eq!(e, InstallError::Disabled("agent".into()));
        assert!(e.to_string().contains("disabled"), "{e}");
    }

    #[test]
    fn preflight_errors_never_carry_a_token() {
        let fx = Fx::new();
        let p = fx.paths(json!([bot("agent", true, "")]));
        let e = preflight(&p, &store("agent", true, true)).unwrap_err();
        assert!(!e.to_string().contains("SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_preflight_writes_nothing_and_never_calls_launchctl() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let p = fx.paths(json!([bot("agent", true, "U01ABC")]));
        let s = store("agent", true, false); // no app token
        let e = install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap_err();
        assert_eq!(e, InstallError::NoAppToken);
        assert!(!fx.launchd().exists() || dir_is_empty(&fx.launchd()));
        assert!(!fx.agents().exists() || dir_is_empty(&fx.agents()));
        assert!(!fx.p("logs").exists(), "no log dir either");
        assert!(!fx.p("calls.log").exists(), "launchctl was called");
        assert!(!is_installed(&fx.agents()));
    }

    #[cfg(unix)]
    #[test]
    fn install_writes_the_plist_and_loads_it() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap();
        let dest = fx.agents().join(PLIST_NAME);
        assert!(fx.launchd().join(PLIST_NAME).exists());
        assert!(dest.exists());
        assert!(is_installed(&fx.agents()));
        assert_eq!(fx.calls(), vec![format!("load -w {}", dest.display())]);
        let written = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(written, render_plist(&fx.launch()));
        assert!(fx.p("logs").is_dir(), "launchd cannot create the log dir");
    }

    #[cfg(unix)]
    #[test]
    fn a_launchctl_failure_leaves_nothing_installed_and_leaks_no_token() {
        let fx = Fx::new();
        let ctl = fx.ctl(Some("load"), "");
        let (p, s) = ready(&fx);
        let e = install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap_err();
        assert_eq!(e, InstallError::Launchctl("boom".into()));
        assert!(!is_installed(&fx.agents()));
        assert!(!fx.launchd().join(PLIST_NAME).exists());
        assert!(!e.to_string().contains("SECRET"));
    }

    #[test]
    fn an_unavailable_launchctl_is_a_launchctl_error() {
        let fx = Fx::new();
        let (p, s) = ready(&fx);
        let e = install(
            &p,
            &s,
            &fx.launchd(),
            &fx.agents(),
            &Launchctl::default(),
            &fx.launch(),
        )
        .unwrap_err();
        assert!(matches!(e, InstallError::Launchctl(_)), "{e:?}");
        assert!(!is_installed(&fx.agents()));
    }

    #[cfg(unix)]
    #[test]
    fn installing_again_reloads_instead_of_failing() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap();
        install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap();
        let dest = fx.agents().join(PLIST_NAME).display().to_string();
        assert_eq!(
            fx.calls(),
            vec![
                format!("load -w {dest}"),
                format!("unload -w {dest}"),
                format!("load -w {dest}")
            ]
        );
        assert!(is_installed(&fx.agents()));
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_unloads_and_removes_both_copies_and_is_idempotent() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap();
        uninstall(&fx.launchd(), &fx.agents(), &ctl).unwrap();
        let dest = fx.agents().join(PLIST_NAME).display().to_string();
        assert_eq!(
            fx.calls(),
            vec![format!("load -w {dest}"), format!("unload -w {dest}")]
        );
        assert!(!fx.launchd().join(PLIST_NAME).exists());
        assert!(!fx.agents().join(PLIST_NAME).exists());
        assert!(!is_installed(&fx.agents()));
        let before = fx.calls().len();
        uninstall(&fx.launchd(), &fx.agents(), &ctl).unwrap();
        assert_eq!(
            fx.calls().len(),
            before,
            "second uninstall called launchctl"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_when_never_installed_calls_nothing() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        uninstall(&fx.launchd(), &fx.agents(), &ctl).unwrap();
        assert!(!fx.p("calls.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_still_unloads_when_the_project_copy_is_gone() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap();
        std::fs::remove_file(fx.launchd().join(PLIST_NAME)).unwrap();
        uninstall(&fx.launchd(), &fx.agents(), &ctl).unwrap();
        assert!(fx.calls().last().unwrap().starts_with("unload -w"));
        assert!(!is_installed(&fx.agents()));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_unload_keeps_the_files_while_the_agent_is_still_loaded() {
        let fx = Fx::new();
        let good = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &good, &fx.launch()).unwrap();
        let loaded = format!("123\t0\t{LABEL}\n");
        let bad = fx.ctl(Some("unload"), &loaded);
        let e = uninstall(&fx.launchd(), &fx.agents(), &bad).unwrap_err();
        assert!(e.contains("boom"), "{e}");
        assert!(is_installed(&fx.agents()));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_unload_of_an_agent_launchd_does_not_know_still_removes_the_files() {
        let fx = Fx::new();
        let good = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &good, &fx.launch()).unwrap();
        let bad = fx.ctl(Some("unload"), "1\t0\tcom.other.thing\n");
        uninstall(&fx.launchd(), &fx.agents(), &bad).unwrap();
        assert!(!is_installed(&fx.agents()));
        assert!(!fx.launchd().join(PLIST_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn saving_the_connector_and_tokens_alone_installs_nothing() {
        let fx = Fx::new();
        let (p, s) = ready(&fx);
        // Everything a user does on the Connect step.
        assert_eq!(preflight(&p, &s), Ok(()));
        assert!(!is_installed(&fx.agents()));
        assert!(!fx.agents().exists());
        assert!(!fx.launchd().exists());
        assert!(!fx.p("calls.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn restart_kickstarts_the_agent_in_the_users_gui_domain() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let (p, s) = ready(&fx);
        install(&p, &s, &fx.launchd(), &fx.agents(), &ctl, &fx.launch()).unwrap();
        restart(&fx.agents(), &ctl, 501).unwrap();
        assert_eq!(
            fx.calls().last().unwrap(),
            "kickstart -k gui/501/com.hermes.loopx-slack"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_when_not_installed_is_an_error_and_calls_nothing() {
        let fx = Fx::new();
        let ctl = fx.ctl(None, "");
        let e = restart(&fx.agents(), &ctl, 501).unwrap_err();
        assert!(e.contains("not installed"), "{e}");
        assert!(!fx.p("calls.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_restart_reports_launchctls_message() {
        let fx = Fx::new();
        let ctl = fx.ctl(Some("kickstart"), "");
        std::fs::create_dir_all(fx.agents()).unwrap();
        std::fs::write(fx.agents().join(PLIST_NAME), "x").unwrap();
        let e = restart(&fx.agents(), &ctl, 501).unwrap_err();
        assert!(e.contains("boom"), "{e}");
    }

    #[test]
    fn the_manifest_is_yaml_with_socket_mode_events_and_scopes() {
        let text = app_manifest_yaml();
        let v: Value = serde_norway::from_str(&text).unwrap();
        assert_eq!(v["settings"]["socket_mode_enabled"], true);
        assert_eq!(v["settings"]["interactivity"]["is_enabled"], true);
        let events: Vec<&str> = v["settings"]["event_subscriptions"]["bot_events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(events, ["app_mention", "message.im"]);
        let scopes: Vec<&str> = v["oauth_config"]["scopes"]["bot"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            scopes,
            [
                "app_mentions:read",
                "chat:write",
                "reactions:write",
                "im:history",
                "im:write",
            ]
        );
        // The catch-up scopes are offered as commented-out list items, not
        // requested; uncommented they are a valid manifest with every scope
        // catch-up uses (users.conversations, then each history).
        assert!(!scopes.contains(&"channels:history"));
        let opted_in: String = text
            .lines()
            .map(|l| match l.trim_start().strip_prefix("# - ") {
                Some(item) => format!("      - {item}\n"),
                None => format!("{l}\n"),
            })
            .collect();
        let v: Value = serde_norway::from_str(&opted_in).unwrap();
        let all: Vec<&str> = v["oauth_config"]["scopes"]["bot"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for scope in [
            "channels:read",
            "groups:read",
            "im:read",
            "mpim:read",
            "channels:history",
            "groups:history",
            "im:history",
            "mpim:history",
        ] {
            assert!(all.contains(&scope), "{scope} missing: {all:?}");
        }
        assert_eq!(v["features"]["bot_user"]["always_online"], true);
        assert_eq!(v["features"]["app_home"]["messages_tab_enabled"], true);
    }
}

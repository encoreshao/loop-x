//! `loopx slack listen|status|test`: the Slack agent's Socket Mode listener
//! (`loopx_core::slack_agent::listener`), its status, and a token check.
//! Tokens come from the connector secret store and are never printed.

use loopx_core::connectors::secrets::open_connector_store;
use loopx_core::connectors::slack_bot::{app_token_key, listener_account, parse_allowlist};
use loopx_core::connectors::{Account, UreqClient};
use loopx_core::connectors_config::{list_accounts, Paths};
use loopx_core::keychain::SecretStore;
use loopx_core::launchd::{self, Launchctl};
use loopx_core::runner_util::repo_root;
use loopx_core::slack_agent::install::{self, Launch};
use loopx_core::slack_agent::listener::{
    default_state_dir, rate_from_setting, run_listener, status_path, stop_on_signals, Config, Exit,
};
use loopx_core::slack_agent::status::{
    effective_state, listener_lock_held, read_status, Effective, Status,
};
use loopx_core::slack_agent::web::SlackWeb;
use loopx_server::ui::state::UiState;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "Usage: loopx slack <listen [--account ID] | test [--account ID] | manifest\n\
       | status [DIRS] | install [--yes] [DIRS] | uninstall [DIRS] | restart [DIRS]>\n\
DIRS: [--launch-agents-dir DIR] [--launchd-dir DIR] [--real-launchd]";

/// `EX_CONFIG`: launchd's restart throttle is then the only retry.
const EX_CONFIG: u8 = 78;

pub fn run(args: &[String]) -> ExitCode {
    let (sub, rest) = match args.split_first() {
        Some((s, r)) => (s.as_str(), r),
        None => ("", args),
    };
    match sub {
        "listen" => listen(rest),
        "status" => status_cmd(rest),
        "install" => install_cmd(rest),
        "uninstall" => uninstall_cmd(rest),
        "restart" => restart_cmd(rest),
        "manifest" if rest.is_empty() => {
            print!("{}", install::app_manifest_yaml());
            ExitCode::SUCCESS
        }
        "test" => test(rest),
        "-h" | "--help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn config_error(msg: &str) -> ExitCode {
    eprintln!("loopx slack: {msg}");
    ExitCode::from(EX_CONFIG)
}

/// The account and both tokens, or the exit for a usage or setup problem.
fn load_from_args(args: &[String]) -> Result<Loaded, ExitCode> {
    let account = parse_account(args).map_err(|e| {
        eprintln!("{e}\n{USAGE}");
        ExitCode::from(2)
    })?;
    let store = open_connector_store()
        .map_err(|e| config_error(&format!("Cannot open the secret store: {e}")))?;
    load(account.as_deref(), &Paths::default(), store.as_ref()).map_err(|e| config_error(&e))
}

fn listen(args: &[String]) -> ExitCode {
    let loaded = match load_from_args(args) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let allowlist = parse_allowlist(loaded.account.setting("allowed_users"));
    if allowlist.is_empty() {
        return config_error("No allowed Slack users are set");
    }
    let state = crate::chat_tool_cmd::state();
    let cfg = Config {
        allowlist,
        rate_per_hour: rate_from_setting(loaded.account.setting("rate_per_hour")),
        approvals_dir: loopx_core::approvals::default_dir(),
        state_dir: default_state_dir(&state.repo_root),
    };
    let web = SlackWeb::new(loaded.bot, loaded.app, Arc::new(UreqClient::default()));
    let stop = stop_on_signals();
    keep_awake_on_ac();
    eprintln!("loopx slack: listening as account {}", loaded.account.id);
    let exit = run_listener(cfg, web, chat_runtime(&state), stop);
    match &exit {
        Exit::Shutdown => {}
        Exit::Misconfigured(m) => eprintln!("loopx slack: {m}"),
        Exit::LockHeld => eprintln!("loopx slack: another Slack listener is already running"),
        Exit::TokenRejected => {
            eprintln!("loopx slack: Slack rejected a token; see `loopx slack status`")
        }
    }
    ExitCode::from(exit_code(&exit))
}

/// Stops the Mac sleeping (lid closed included) while it is on AC power, for as
/// long as this process lives: a sleeping Mac kills the Slack socket and every
/// mention sent meanwhile is lost. `-s` only applies on AC, so on battery the
/// Mac sleeps as usual; `-w` ends the assertion when the listener exits.
fn keep_awake_on_ac() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("/usr/bin/caffeinate")
        .args(["-i", "-s", "-w", &pid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The process exit code for how the listener ended. launchd runs the
/// listener with `KeepAlive {SuccessfulExit: false}`, so only a non-zero code
/// relaunches it:
/// - `Shutdown` 0: launchd (or the user) stopped it.
/// - `TokenRejected` 0: the status file already says "needs attention", and a
///   relaunch would only hit Slack again with the same rejected token (a retry
///   storm); the user fixes the token and reinstalls or restarts.
/// - `Misconfigured` 78 (`EX_CONFIG`): refused before any network call;
///   launchd's throttle is the only retry.
/// - `LockHeld` 1: another listener already runs (it held the lock through
///   about 2 s of retries). This is the only code-1 exit; any other non-zero
///   exit (a panic is 101) is relaunched as well.
fn exit_code(exit: &Exit) -> u8 {
    match exit {
        Exit::Shutdown | Exit::TokenRejected => 0,
        Exit::Misconfigured(_) => EX_CONFIG,
        Exit::LockHeld => 1,
    }
}

/// Checks both tokens against Slack; starts nothing and prints no secret
/// (the socket URL carries a ticket, so it is not printed either).
fn test(args: &[String]) -> ExitCode {
    let loaded = match load_from_args(args) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let web = SlackWeb::new(loaded.bot, loaded.app, Arc::new(UreqClient::default()));
    let bot_ok = match web.auth_test() {
        Ok(info) => {
            println!("Bot token: OK (bot user {})", info.bot_user_id);
            true
        }
        Err(e) => {
            println!("Bot token: {e}");
            false
        }
    };
    let app_ok = match web.open_socket_url() {
        Ok(_) => {
            println!("App-level token: OK");
            true
        }
        Err(e) => {
            println!("App-level token: {e}");
            false
        }
    };
    if bot_ok && app_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The Slack agent account to use, with both tokens.
struct Loaded {
    account: Account,
    bot: String,
    app: String,
}

/// `account` (or the first enabled `slack_bot`) and its bot and app-level
/// tokens. Errors name what is missing, never a secret.
fn load(account: Option<&str>, paths: &Paths, store: &dyn SecretStore) -> Result<Loaded, String> {
    let accounts = list_accounts(paths).map_err(|e| format!("Cannot read connectors: {}", e.0))?;
    let account = match account {
        Some(id) => {
            let a = accounts
                .into_iter()
                .find(|a| a.id == id)
                .ok_or_else(|| format!("No connector named {id}"))?;
            if a.kind != "slack_bot" {
                return Err(format!("Connector {id} is not a Slack agent"));
            }
            if !a.enabled {
                return Err(format!("The Slack agent {id} is disabled"));
            }
            a
        }
        None => listener_account(accounts)
            .ok_or("No Slack agent is set up: add one in Settings > Connectors")?,
    };
    let secret = |key: &str| -> Result<Option<String>, String> {
        store
            .get(key)
            .map(|v| v.filter(|s| !s.trim().is_empty()))
            .map_err(|_| "Cannot read the Slack tokens from the secret store".to_string())
    };
    let bot = secret(&account.id)?.ok_or("The Slack bot token is not set")?;
    let app = secret(&app_token_key(&account.id))?.ok_or("The Slack app-level token is not set")?;
    Ok(Loaded { account, bot, app })
}

fn parse_account(args: &[String]) -> Result<Option<String>, String> {
    let mut account = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--account" => match it.next() {
                Some(id) if !id.is_empty() => account = Some(id.clone()),
                _ => return Err("--account needs an id".into()),
            },
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(account)
}

/// The dashboard's chat runtime, rooted where `loopx chat-tool` is (so the
/// private-call ledger the tool writes is the one the executor reads).
fn chat_runtime(state: &UiState) -> loopx_core::chat::ChatRuntime {
    loopx_server::ui::pages::chat::chat_runtime(state)
}

/// The `status` text. `installed` says whether the launchd agent is in place;
/// a listener run by hand shows as "running by hand".
fn format_status(s: Option<&Status>, effective: &Effective, installed: bool) -> String {
    let who = if installed {
        "installed, "
    } else {
        "running by hand, "
    };
    let head = match effective {
        Effective::NotInstalled => return "Slack listener: not installed".to_string(),
        Effective::Stopped => format!("{who}stopped"),
        Effective::Offline { since: Some(t) } => format!("{who}offline since {t}"),
        Effective::Offline { since: None } => format!("{who}offline"),
        Effective::Connected { since, .. } => format!("{who}connected since {since}"),
        Effective::NeedsAttention { error } => format!(
            "{who}needs attention: {}",
            error.as_deref().unwrap_or("see the log")
        ),
        Effective::Crashed => {
            format!("{who}not running (it stopped unexpectedly; run `loopx slack restart`)")
        }
    };
    format!(
        "Slack listener: {head}\nLast event: {}\nLast error: {}",
        s.and_then(|s| s.last_event.as_deref()).unwrap_or("none"),
        s.and_then(|s| s.last_error.as_deref()).unwrap_or("none"),
    )
}

// ------------------------------------------------------------- the launchd agent

/// What `--yes` is needed for: the plain statement of what install does.
fn disclosure(plist: &Path) -> String {
    format!(
        "Installing the Slack listener will:\n\
         - write {} and load it with launchd\n\
         - run in the background while you are logged in\n\
         - can start agent work from Slack messages from the allowlisted users\n\
         - everyone on the allowlist can use your agent as you: they get replies with\n   \
           your mail and calendar data by direct message and can answer your approvals\n\
         Remove it any time with `loopx slack uninstall`.",
        plist.display()
    )
}

/// `--yes`, else an interactive `y`; a non-interactive run without `--yes`
/// refuses rather than guesses.
fn confirm(yes: bool, interactive: bool, input: &mut dyn BufRead) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !interactive {
        return Err("Not a terminal: pass --yes to install".to_string());
    }
    eprint!("Install? [y/N] ");
    let mut line = String::new();
    input.read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[derive(Debug, Default, PartialEq)]
struct Opts {
    yes: bool,
    agents: Option<PathBuf>,
    launchd: Option<PathBuf>,
    real_launchd: bool,
}

fn parse_opts(args: &[String], allow_yes: bool) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut dir = |slot: &mut Option<PathBuf>| match it.next() {
            Some(d) if !d.is_empty() => {
                *slot = Some(PathBuf::from(d));
                Ok(())
            }
            _ => Err(format!("{arg} needs a directory")),
        };
        match arg.as_str() {
            "--yes" | "-y" if allow_yes => o.yes = true,
            "--launch-agents-dir" => dir(&mut o.agents)?,
            "--launchd-dir" => dir(&mut o.launchd)?,
            "--real-launchd" => o.real_launchd = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(o)
}

/// What the process environment says, so the guard is testable.
struct EnvInfo {
    /// What `$HOME` says (only for the read-only `status` view).
    home_env: PathBuf,
    /// The account's home from the password database, not `$HOME`: the real
    /// `~/Library/LaunchAgents` lives under it however `$HOME` is set.
    real_home: PathBuf,
    repo_root: PathBuf,
    /// `LOOPX_LAUNCHCTL_CMD`: a fake launchctl (the install script's seam).
    launchctl_cmd: Option<String>,
    /// A development sandbox variable is set (`LOOP_ENGINEERING_HOME`,
    /// `LOOPX_ROOT`, `LOOPX_KEYCHAIN`, `LOOP_UPDATE_DIR`, `LOOP_EVENTS_DIR`).
    sandbox_set: bool,
}

/// The current user's home directory from `getpwuid`; empty if unknown.
fn real_home() -> PathBuf {
    launchd::passwd_home().unwrap_or_default()
}

impl EnvInfo {
    fn from_process() -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        EnvInfo {
            home_env: PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
            real_home: real_home(),
            repo_root: repo_root(),
            launchctl_cmd: std::env::var("LOOPX_LAUNCHCTL_CMD")
                .ok()
                .filter(|c| !c.trim().is_empty()),
            sandbox_set: [
                "LOOP_ENGINEERING_HOME",
                "LOOPX_ROOT",
                "LOOPX_KEYCHAIN",
                "LOOP_UPDATE_DIR",
                "LOOP_EVENTS_DIR",
            ]
            .iter()
            .any(|k| set(k)),
        }
    }
}

enum Ctl {
    Use(Launchctl),
    /// Scratch directories and no fake: files only, launchctl is not called.
    NoLaunchctl(String),
    /// Doing anything here could reach the real LaunchAgents or launchd.
    Refused(String),
}

struct Target {
    launchd_dir: PathBuf,
    agents_dir: PathBuf,
    ctl: Ctl,
}

/// The directories and launchctl to use. Rules, in order:
/// 1. A sandbox (any sandbox variable, or `$HOME` differing from the account's
///    real home) with no explicit `--launch-agents-dir` is refused, whatever
///    `LOOPX_LAUNCHCTL_CMD` says: default directories could be the real ones.
/// 2. An explicit agents dir must come with `--launchd-dir` when `need_launchd_dir`.
/// 3. The real `<real home>/Library/LaunchAgents` always gets the real
///    launchctl (never the fake) and is refused inside a sandbox.
/// 4. Any other directory gets the fake from `LOOPX_LAUNCHCTL_CMD`, else the
///    real launchctl only with `--real-launchd` (refused in a sandbox), else none.
fn resolve_target(o: &Opts, env: &EnvInfo, need_launchd_dir: bool) -> Target {
    let explicit = o.agents.is_some();
    let agents_dir = o
        .agents
        .clone()
        .unwrap_or_else(|| launchd::launch_agents_dir(&env.home_env));
    let launchd_dir = o
        .launchd
        .clone()
        .unwrap_or_else(|| env.repo_root.join("launchd"));
    // One rule for every guard below (and the dashboard's, which compares the
    // same way): a sandbox variable is set, or `$HOME` is not the passwd home.
    let sandboxed = env.sandbox_set || !launchd::same_dir(&env.home_env, &env.real_home);
    let refuse = |msg: &str| Target {
        launchd_dir: launchd_dir.clone(),
        agents_dir: agents_dir.clone(),
        ctl: Ctl::Refused(msg.to_string()),
    };
    if env.real_home.as_os_str().is_empty() {
        return refuse("Cannot determine the real home directory");
    }
    if !explicit && sandboxed {
        return refuse(
            "A development sandbox is active (a sandbox variable is set or HOME is overridden): \
             pass --launch-agents-dir and --launchd-dir with scratch directories",
        );
    }
    if explicit && need_launchd_dir && o.launchd.is_none() {
        return refuse("--launch-agents-dir needs --launchd-dir as well");
    }
    let real_dir = launchd::launch_agents_dir(&env.real_home);
    let ctl = if launchd::same_dir(&agents_dir, &real_dir) {
        if sandboxed {
            return refuse(
                "The real LaunchAgents folder is off limits in a development sandbox \
                 (a sandbox variable is set or HOME is overridden)",
            );
        }
        Ctl::Use(Launchctl::real())
    } else if let Some(cmd) = &env.launchctl_cmd {
        Ctl::Use(Launchctl {
            command: cmd.split_whitespace().map(String::from).collect(),
            list_file: None,
        })
    } else if o.real_launchd {
        if sandboxed {
            return refuse(
                "--real-launchd is not allowed in a development sandbox \
                 (a sandbox variable is set or HOME is overridden)",
            );
        }
        Ctl::Use(Launchctl::real())
    } else {
        Ctl::NoLaunchctl(format!(
            "{} is not the real LaunchAgents folder: not calling launchctl (--real-launchd to force)",
            agents_dir.display()
        ))
    };
    Target {
        launchd_dir,
        agents_dir,
        ctl,
    }
}

fn usage_error(e: &str) -> ExitCode {
    eprintln!("loopx slack: {e}\n{USAGE}");
    ExitCode::from(2)
}

fn status_cmd(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args, false) {
        Ok(o) => o,
        Err(e) => return usage_error(&e),
    };
    let env = EnvInfo::from_process();
    // Read-only: only the directory matters, the launchctl decision is unused.
    let target = resolve_target(&opts, &env, false);
    let state_dir = default_state_dir(&env.repo_root);
    let status = read_status(&status_path(&state_dir));
    let installed = install::is_installed(&target.agents_dir);
    let effective = effective_state(status.as_ref(), listener_lock_held(&state_dir), installed);
    println!("{}", format_status(status.as_ref(), &effective, installed));
    ExitCode::SUCCESS
}

fn install_cmd(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args, true) {
        Ok(o) => o,
        Err(e) => return usage_error(&e),
    };
    let env = EnvInfo::from_process();
    let target = resolve_target(&opts, &env, true);
    let ctl = match &target.ctl {
        Ctl::Use(c) => c,
        Ctl::NoLaunchctl(why) | Ctl::Refused(why) => {
            eprintln!("loopx slack: {why}");
            return ExitCode::from(2);
        }
    };
    let store = match open_connector_store() {
        Ok(s) => s,
        Err(e) => return config_error(&format!("Cannot open the secret store: {e}")),
    };
    let paths = Paths::default();
    if let Err(e) = install::preflight(&paths, store.as_ref()) {
        eprintln!("loopx slack: {e}");
        return ExitCode::from(1);
    }
    let bin = match std::env::current_exe() {
        Ok(b) => b,
        Err(e) => return config_error(&format!("Cannot find the loopx binary: {e}")),
    };
    println!(
        "{}",
        disclosure(&target.agents_dir.join(install::PLIST_NAME))
    );
    let stdin = std::io::stdin();
    match confirm(opts.yes, stdin.is_terminal(), &mut stdin.lock()) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("loopx slack: not installed");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("loopx slack: {e}");
            return ExitCode::from(2);
        }
    }
    let launch = Launch {
        loopx_bin: bin,
        log_dir: env.repo_root.join("logs"),
        user_home: env.real_home.clone(),
        pinned_env: install::pinned_env_from(&|k| std::env::var(k).ok()),
    };
    match install::install(
        &paths,
        store.as_ref(),
        &target.launchd_dir,
        &target.agents_dir,
        ctl,
        &launch,
    ) {
        Ok(()) => {
            println!("Installed the Slack listener ({})", install::LABEL);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("loopx slack: {e}");
            ExitCode::from(1)
        }
    }
}

fn uninstall_cmd(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args, false) {
        Ok(o) => o,
        Err(e) => return usage_error(&e),
    };
    let target = resolve_target(&opts, &EnvInfo::from_process(), true);
    // Scratch directories with no launchctl: the files are still removed (a
    // scratch install never loaded anything); say so rather than pretend it
    // unloaded. A refusal removes nothing.
    let (ctl, note) = match target.ctl {
        Ctl::Use(c) => (c, None),
        Ctl::NoLaunchctl(why) => (Launchctl::default(), Some(why)),
        Ctl::Refused(why) => {
            eprintln!("loopx slack: {why}");
            return ExitCode::from(2);
        }
    };
    if !install::is_installed(&target.agents_dir)
        && !target.launchd_dir.join(install::PLIST_NAME).exists()
    {
        println!("The Slack listener is not installed");
        return ExitCode::SUCCESS;
    }
    if let Some(why) = note {
        eprintln!("loopx slack: {why}");
    }
    match install::uninstall(&target.launchd_dir, &target.agents_dir, &ctl) {
        Ok(()) => {
            println!("Removed the Slack listener ({})", install::LABEL);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("loopx slack: {e}");
            ExitCode::from(1)
        }
    }
}

fn restart_cmd(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args, false) {
        Ok(o) => o,
        Err(e) => return usage_error(&e),
    };
    let target = resolve_target(&opts, &EnvInfo::from_process(), false);
    let ctl = match &target.ctl {
        Ctl::Use(c) => c,
        Ctl::NoLaunchctl(why) | Ctl::Refused(why) => {
            eprintln!("loopx slack: {why}");
            return ExitCode::from(2);
        }
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    match install::restart(&target.agents_dir, ctl, uid) {
        Ok(()) => {
            println!("Restarted the Slack listener");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("loopx slack: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopx_core::connectors::slack_bot::app_token_key;
    use loopx_core::keychain::MemoryStore;
    use loopx_core::slack_agent::status::State;
    use serde_json::json;
    use std::sync::Mutex;

    /// Serialises the tests that set process-wide environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn paths(dir: &std::path::Path, entries: serde_json::Value) -> Paths {
        let config = dir.join("connectors.json");
        std::fs::write(&config, entries.to_string()).unwrap();
        Paths {
            config_path: Some(config),
            gitlab_config_path: Some(dir.join("gitlab.json")),
            slack_config_path: Some(dir.join("slack.json")),
            inbox_config_path: Some(dir.join("inboxes.json")),
        }
    }

    fn bot(id: &str, enabled: bool) -> serde_json::Value {
        json!({"id": id, "type": "slack_bot", "label": id, "enabled": enabled,
               "settings": {"allowed_users": "U01ABC", "rate_per_hour": "4"}})
    }

    fn store(ids: &[&str]) -> MemoryStore {
        let s = MemoryStore::new("test");
        for id in ids {
            s.set(id, &format!("xoxb-{id}")).unwrap();
            s.set(&app_token_key(id), &format!("xapp-{id}")).unwrap();
        }
        s
    }

    fn err(r: Result<Loaded, String>) -> String {
        match r {
            Ok(l) => panic!("loaded {}", l.account.id),
            Err(e) => e,
        }
    }

    #[test]
    fn the_first_enabled_slack_agent_is_used_with_both_tokens() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(
            d.path(),
            json!([{"id": "hook", "type": "slack", "settings": {}}, bot("off", false), bot("agent", true)]),
        );
        let l = load(None, &p, &store(&["off", "agent"])).unwrap();
        assert_eq!(l.account.id, "agent");
        assert_eq!(
            (l.bot.as_str(), l.app.as_str()),
            ("xoxb-agent", "xapp-agent")
        );
        assert_eq!(l.account.setting("rate_per_hour"), "4");
    }

    #[test]
    fn an_explicit_account_must_be_an_enabled_slack_agent() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(
            d.path(),
            json!([{"id": "hook", "type": "slack", "settings": {}}, bot("off", false), bot("agent", true), bot("two", true)]),
        );
        let s = store(&["off", "agent", "two"]);
        assert_eq!(load(Some("two"), &p, &s).unwrap().account.id, "two");
        assert!(err(load(Some("hook"), &p, &s)).contains("not a Slack agent"));
        assert!(err(load(Some("off"), &p, &s)).contains("disabled"));
        assert!(err(load(Some("nope"), &p, &s)).contains("No connector"));
    }

    #[test]
    fn missing_accounts_or_tokens_are_named_without_secrets() {
        let d = tempfile::tempdir().unwrap();
        let none = paths(d.path(), json!([]));
        assert!(err(load(None, &none, &store(&[]))).contains("No Slack agent"));

        let p = paths(d.path(), json!([bot("agent", true)]));
        assert!(err(load(None, &p, &MemoryStore::new("t"))).contains("bot token is not set"));
        let only_bot = MemoryStore::new("t");
        only_bot.set("agent", "xoxb-SECRET").unwrap();
        let e = err(load(None, &p, &only_bot));
        assert!(e.contains("app-level token is not set"), "{e}");
        assert!(!e.contains("SECRET"));
    }

    #[test]
    fn account_flag_parsing() {
        let a = |v: &[&str]| parse_account(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&[]), Ok(None));
        assert_eq!(a(&["--account", "agent"]), Ok(Some("agent".into())));
        assert!(a(&["--account"]).is_err());
        assert!(a(&["--bogus"]).is_err());
    }

    /// launchd runs the listener with `KeepAlive {SuccessfulExit: false}`:
    /// any non-zero exit relaunches it.
    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&Exit::Shutdown), 0);
        // Status already says "needs attention"; a relaunch would only hit
        // Slack again with the same rejected token.
        assert_eq!(exit_code(&Exit::TokenRejected), 0);
        assert_eq!(exit_code(&Exit::Misconfigured("x".into())), 78);
        assert_eq!(exit_code(&Exit::LockHeld), 1);
    }

    fn status(state: State) -> Status {
        Status {
            state,
            since: "2026-11-15T10:00:00+00:00".into(),
            last_event: Some("2026-11-15T10:01:00+00:00".into()),
            last_error: Some("Slack rejected the app-level token".into()),
        }
    }

    #[test]
    fn status_text() {
        assert_eq!(
            format_status(None, &Effective::NotInstalled, false),
            "Slack listener: not installed"
        );
        let s = status(State::NeedsAttention);
        let e = effective_state(Some(&s), false, true);
        assert_eq!(
            format_status(Some(&s), &e, true),
            "Slack listener: installed, needs attention: Slack rejected the app-level token\n\
             Last event: 2026-11-15T10:01:00+00:00\n\
             Last error: Slack rejected the app-level token"
        );
        assert_eq!(
            format_status(None, &Effective::Stopped, true),
            "Slack listener: installed, stopped\nLast event: none\nLast error: none"
        );
        let s = status(State::Connected);
        let e = effective_state(Some(&s), true, true);
        assert!(format_status(Some(&s), &e, true)
            .starts_with("Slack listener: installed, connected since 2026-11-15T10:00:00+00:00"));
        assert!(format_status(Some(&s), &Effective::Crashed, true).contains("stopped unexpectedly"));
        assert!(format_status(Some(&s), &e, false).starts_with("Slack listener: running by hand, "));
    }

    #[test]
    fn the_disclosure_states_what_install_does() {
        let text = disclosure(Path::new(
            "/x/Library/LaunchAgents/com.hermes.loopx-slack.plist",
        ));
        assert!(text.contains("/x/Library/LaunchAgents/com.hermes.loopx-slack.plist"));
        assert!(text.contains("while you are logged in"));
        assert!(
            text.contains("can start agent work from Slack messages from the allowlisted users")
        );
        assert!(text.contains("everyone on the allowlist can use your agent as you"));
    }

    #[test]
    fn install_needs_yes_or_an_interactive_y() {
        let mut none = std::io::Cursor::new(Vec::new());
        assert_eq!(confirm(true, false, &mut none), Ok(true));
        // Non-interactive without --yes refuses without reading anything.
        let mut y = std::io::Cursor::new(b"y\n".to_vec());
        assert!(confirm(false, false, &mut y).is_err());
        assert_eq!(confirm(false, true, &mut y), Ok(true));
        for answer in ["n\n", "\n", "maybe\n", ""] {
            let mut r = std::io::Cursor::new(answer.as_bytes().to_vec());
            assert_eq!(confirm(false, true, &mut r), Ok(false), "{answer:?}");
        }
    }

    #[test]
    fn option_parsing() {
        let a =
            |v: &[&str], yes| parse_opts(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), yes);
        assert_eq!(a(&[], true), Ok(Opts::default()));
        let o = a(
            &[
                "--yes",
                "--launch-agents-dir",
                "/a",
                "--launchd-dir",
                "/b",
                "--real-launchd",
            ],
            true,
        )
        .unwrap();
        assert!(o.yes && o.real_launchd);
        assert_eq!(o.agents, Some(PathBuf::from("/a")));
        assert_eq!(o.launchd, Some(PathBuf::from("/b")));
        assert!(a(&["--yes"], false).is_err(), "--yes is install-only");
        assert!(a(&["--launch-agents-dir"], true).is_err());
        assert!(a(&["--bogus"], true).is_err());
    }

    /// `sandbox_set`: any of the development-sandbox variables is set.
    /// `home_env`: what `$HOME` says; the real home is `/Users/real`.
    fn env(sandbox_set: bool, home_env: &str, cmd: Option<&str>) -> EnvInfo {
        EnvInfo {
            home_env: PathBuf::from(home_env),
            real_home: PathBuf::from("/Users/real"),
            repo_root: PathBuf::from("/Users/real/.loopx"),
            launchctl_cmd: cmd.map(String::from),
            sandbox_set,
        }
    }

    fn scratch_opts() -> Opts {
        Opts {
            agents: Some(PathBuf::from("/tmp/agents")),
            launchd: Some(PathBuf::from("/tmp/launchd")),
            ..Opts::default()
        }
    }

    fn real_ctl(t: &Target) -> bool {
        matches!(&t.ctl, Ctl::Use(c) if c.command == ["launchctl"])
    }

    #[test]
    fn the_real_launchctl_is_only_for_the_real_default_location_outside_a_sandbox() {
        let t = resolve_target(&Opts::default(), &env(false, "/Users/real", None), true);
        assert_eq!(
            t.agents_dir,
            PathBuf::from("/Users/real/Library/LaunchAgents")
        );
        assert_eq!(t.launchd_dir, PathBuf::from("/Users/real/.loopx/launchd"));
        assert!(real_ctl(&t));
    }

    #[test]
    fn a_sandbox_without_explicit_dirs_is_refused_even_with_a_fake_launchctl() {
        for cmd in [None, Some("/bin/fake")] {
            let t = resolve_target(&Opts::default(), &env(true, "/Users/real", cmd), true);
            assert!(matches!(&t.ctl, Ctl::Refused(m) if m.contains("--launch-agents-dir")));
        }
    }

    /// Overriding only HOME must never reach the real LaunchAgents or launchctl.
    #[test]
    fn overriding_only_home_is_a_sandbox_too() {
        for cmd in [None, Some("/bin/fake")] {
            let t = resolve_target(
                &Opts::default(),
                &env(false, "/tmp/scratch-home", cmd),
                true,
            );
            assert!(matches!(t.ctl, Ctl::Refused(_)), "{cmd:?}");
        }
    }

    #[test]
    fn explicit_dirs_get_no_launchctl_unless_a_fake_is_injected() {
        let t = resolve_target(&scratch_opts(), &env(true, "/tmp/h", None), true);
        assert_eq!(t.agents_dir, PathBuf::from("/tmp/agents"));
        assert_eq!(t.launchd_dir, PathBuf::from("/tmp/launchd"));
        assert!(matches!(t.ctl, Ctl::NoLaunchctl(_)));
        let t = resolve_target(
            &scratch_opts(),
            &env(true, "/tmp/h", Some("/bin/fake -x")),
            true,
        );
        match t.ctl {
            Ctl::Use(c) => assert_eq!(c.command, ["/bin/fake", "-x"]),
            _ => panic!("fake not used"),
        }
    }

    #[test]
    fn the_fake_launchctl_never_applies_to_the_real_launchagents_dir() {
        // An explicit dir that IS the real one: real launchctl outside a
        // sandbox (the fake is ignored), refused inside one.
        let real = Opts {
            agents: Some(PathBuf::from("/Users/real/Library/LaunchAgents")),
            launchd: Some(PathBuf::from("/tmp/launchd")),
            ..Opts::default()
        };
        assert!(real_ctl(&resolve_target(
            &real,
            &env(false, "/Users/real", Some("/bin/fake")),
            true
        )));
        let t = resolve_target(&real, &env(true, "/tmp/h", Some("/bin/fake")), true);
        assert!(matches!(t.ctl, Ctl::Refused(_)));
        // Overriding only HOME is a sandbox too.
        for cmd in [None, Some("/bin/fake")] {
            let t = resolve_target(&real, &env(false, "/tmp/h", cmd), true);
            assert!(matches!(t.ctl, Ctl::Refused(_)), "{cmd:?}");
        }
    }

    #[test]
    fn real_launchd_cannot_be_forced_inside_a_sandbox() {
        let forced = Opts {
            real_launchd: true,
            ..scratch_opts()
        };
        assert!(real_ctl(&resolve_target(
            &forced,
            &env(false, "/Users/real", None),
            true
        )));
        let t = resolve_target(&forced, &env(true, "/Users/real", None), true);
        assert!(matches!(t.ctl, Ctl::Refused(_)));
        // Overriding only HOME is a sandbox too.
        let t = resolve_target(&forced, &env(false, "/tmp/scratch-home", None), true);
        assert!(matches!(t.ctl, Ctl::Refused(_)));
    }

    #[test]
    fn explicit_agents_dir_needs_an_explicit_launchd_dir_when_one_is_written() {
        let only_agents = Opts {
            agents: Some(PathBuf::from("/tmp/agents")),
            ..Opts::default()
        };
        let t = resolve_target(
            &only_agents,
            &env(false, "/Users/real", Some("/bin/fake")),
            true,
        );
        assert!(matches!(t.ctl, Ctl::Refused(m) if m.contains("--launchd-dir")));
        // restart and status write nothing there.
        let t = resolve_target(
            &only_agents,
            &env(false, "/Users/real", Some("/bin/fake")),
            false,
        );
        assert!(matches!(t.ctl, Ctl::Use(_)));
    }

    /// HARD requirement: the listener's runtime is rooted where `loopx
    /// chat-tool` resolves its root, or the private-call ledger is missed.
    #[test]
    fn the_runtime_root_is_the_chat_tool_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let saved: Vec<_> = ["LOOPX_ROOT", "LOOP_ENGINEERING_HOME"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        std::env::set_var("LOOPX_ROOT", root.path());
        std::env::set_var("LOOP_ENGINEERING_HOME", home.path());

        let tool_state = crate::chat_tool_cmd::state();
        let rt = chat_runtime(&crate::chat_tool_cmd::state());
        // What `loopx chat-tool call` records for a private tool...
        loopx_core::chat_tools::private_ledger::record(
            &tool_state.repo_root,
            "sess1",
            "mail.search",
            1.0,
        )
        .unwrap();
        let entries =
            loopx_core::chat_tools::private_ledger::entries(&rt.repo_root, "sess1").unwrap();

        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        assert_eq!(rt.repo_root, root.path());
        assert_eq!(rt.repo_root, tool_state.repo_root);
        // ...is what the Slack executor reads.
        assert_eq!(entries, vec!["mail.search".to_string()]);
    }
}

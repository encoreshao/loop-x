//! Shared state for server-rendered pages: where the Python dashboard's
//! module-level path constants pointed (`LOOP_DIR`, `LOOP_ENGINEERING_HOME`, the
//! update state dir), the CSRF token, an optional fixed clock, and the per-route
//! kill switch. Everything is resolved once at construction and passed
//! explicitly, so tests and the golden harness point it at scratch directories.

use super::csrf;
use super::routes::Registry;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which routes this server renders itself versus proxies to the legacy dashboard.
///
/// * `LOOPX_PORTED=none` (or `0`/`off`/`legacy`): serve nothing here.
/// * `LOOPX_PORTED=/readme,/update*`: serve only these (exact path, route
///   pattern, or `prefix*`).
/// * unset or `all`: serve every registered route.
/// * `LOOPX_LEGACY_PAGES=/activity,/logs`: always proxy these, whatever else is
///   set (same entry syntax). For bisecting a visual difference against Python.
#[derive(Debug, Clone, Default)]
pub struct RoutePolicy {
    only: Option<Vec<String>>,
    legacy: Vec<String>,
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn entry_matches(entry: &str, path: &str, pattern: &str) -> bool {
    match entry.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix) || pattern.starts_with(prefix),
        None => entry == path || entry == pattern,
    }
}

impl RoutePolicy {
    pub fn from_env() -> Self {
        let ported = std::env::var("LOOPX_PORTED").unwrap_or_default();
        let legacy = std::env::var("LOOPX_LEGACY_PAGES").unwrap_or_default();
        Self::parse(&ported, &legacy)
    }

    pub fn parse(ported: &str, legacy: &str) -> Self {
        let only = match ported.trim() {
            "" | "all" | "1" | "on" => None,
            "none" | "0" | "off" | "legacy" => Some(Vec::new()),
            list => Some(split_list(list)),
        };
        Self {
            only,
            legacy: split_list(legacy),
        }
    }

    pub fn serves(&self, path: &str, pattern: &str) -> bool {
        if self.legacy.iter().any(|e| entry_matches(e, path, pattern)) {
            return false;
        }
        match &self.only {
            None => true,
            Some(list) => list.iter().any(|e| entry_matches(e, path, pattern)),
        }
    }
}

pub struct UiState {
    /// The checkout (`LOOP_DIR`): `README.md`, `outputs/`, `loops/`, `config/`.
    pub repo_root: PathBuf,
    /// `LOOP_ENGINEERING_HOME`: `projects.json`, `loops.json`, `ai_cli.json`, ...
    pub home_dir: PathBuf,
    /// The user's real `$HOME`, for `~/.gitlab/config.json` and friends.
    pub user_home: PathBuf,
    /// `LOOP_UPDATE_DIR` (default `<repo_root>/outputs/update`).
    pub update_dir: PathBuf,
    /// The event ledger directory (`LOOP_EVENTS_DIR`, else `<repo_root>/outputs/events`
    /// like `bin/events.py`'s `default_events_dir`).
    pub events_dir: PathBuf,
    /// `LOOP_EVALS_DIR` (default `<repo_root>/outputs/evals`): `last.json`, `golden-last.json`.
    pub evals_dir: PathBuf,
    pub csrf_token: String,
    pub policy: RoutePolicy,
    pub fixed_now: Option<DateTime<Utc>>,
    /// Does `POST /update/check` run a real `git fetch`?
    pub update_fetch: bool,
    /// Command prefix that starts one loop run (`<loopx> run`, the loop name is
    /// appended); empty means "Run now" starts nothing.
    pub run_cmd: Vec<String>,
    /// Settings: the `launchctl` command prefix (production `["launchctl"]`). Empty
    /// means "no launchd": `list` answers from `<repo_root>/outputs/launchctl_list.txt`
    /// and load/unload fail without touching anything (tests inject a fake script).
    pub launchctl: Vec<String>,
    /// Settings: `Some(names)` pins which AI CLIs count as installed instead of probing
    /// a login shell (tests and the golden harness).
    pub cli_pinned: Option<Vec<String>>,
    /// Connectors: the secret store for connector secrets (`None` = the Keychain, sandbox
    /// suffixed under `LOOP_ENGINEERING_HOME`) and the HTTP client for connector tests
    /// (`None` = the real one). Tests and the golden harness inject fakes.
    pub connector_store: Option<Arc<dyn loopx_core::connectors::secrets::SecretStore>>,
    pub connector_http: Option<Arc<dyn loopx_core::connectors::HttpClient>>,
    /// Does `POST /connectors/linkedin/connect` really open the sign-in window / does
    /// `POST /loops/<name>/run-now` really start `run-loop-now.sh`?
    pub spawn_processes: bool,
    pub registry: Arc<Registry>,
}

/// What `setup.sh` used to do: seed `<home>/loops.json` so the Settings -> Daemons registry
/// isn't empty on a fresh install. Uses the checkout's template when there is one, else the
/// copy embedded in the binary (the Mac app has no checkout). Best effort, never overwrites.
fn scaffold_loops_config(home_dir: &Path, repo_root: &Path) {
    let target = home_dir.join("loops.json");
    if target.exists() {
        return;
    }
    let template = std::fs::read_to_string(repo_root.join("config").join("loops.json.template"))
        .unwrap_or_else(|_| include_str!("../../../../config/loops.json.template").to_string());
    let _ = std::fs::create_dir_all(home_dir);
    let _ = std::fs::write(&target, &template);
    // The same reason install-loopx.sh seeds the state: without today's date for every
    // loop, the first scheduler poll would fire the enabled ones straight away.
    if let Ok(loops) = serde_json::from_str::<Vec<serde_json::Value>>(&template) {
        loopx_core::loop_scheduler::seed_state(
            &loops,
            &home_dir.join("loop_scheduler_state.json"),
            chrono::Local::now().date_naive(),
        );
    }
}

impl UiState {
    /// Production wiring: paths from the environment, a fresh CSRF token.
    pub fn from_env(repo_root: &Path) -> Self {
        let update_dir = std::env::var_os("LOOP_UPDATE_DIR")
            .filter(|v| !v.is_empty())
            .map_or_else(|| repo_root.join("outputs").join("update"), PathBuf::from);
        let home_dir = loopx_core::config::home_dir();
        scaffold_loops_config(&home_dir, repo_root);
        Self {
            repo_root: repo_root.to_path_buf(),
            home_dir,
            user_home: PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
            update_dir,
            events_dir: std::env::var_os("LOOP_EVENTS_DIR")
                .filter(|v| !v.is_empty())
                .map_or_else(|| repo_root.join("outputs").join("events"), PathBuf::from),
            evals_dir: std::env::var_os("LOOP_EVALS_DIR")
                .filter(|v| !v.is_empty())
                .map_or_else(|| repo_root.join("outputs").join("evals"), PathBuf::from),
            csrf_token: csrf::new_token(),
            policy: RoutePolicy::from_env(),
            fixed_now: std::env::var("LOOPX_FIXED_NOW")
                .ok()
                .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
                .map(|d| d.with_timezone(&Utc)),
            update_fetch: true,
            run_cmd: vec![
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "loopx".to_string()),
                "run".to_string(),
            ],
            launchctl: vec!["launchctl".to_string()],
            cli_pinned: None,
            connector_store: None,
            connector_http: None,
            spawn_processes: true,
            registry: Arc::new(Registry::with_pages()),
        }
    }

    /// Explicit wiring (tests, the golden harness): every directory given, a
    /// pinned token and clock.
    pub fn explicit(
        repo_root: &Path,
        home_dir: &Path,
        user_home: &Path,
        update_dir: &Path,
        csrf_token: &str,
        fixed_now: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            home_dir: home_dir.to_path_buf(),
            user_home: user_home.to_path_buf(),
            update_dir: update_dir.to_path_buf(),
            events_dir: repo_root.join("outputs").join("events"),
            evals_dir: repo_root.join("outputs").join("evals"),
            csrf_token: csrf_token.to_string(),
            policy: RoutePolicy::default(),
            fixed_now,
            update_fetch: false,
            run_cmd: Vec::new(),
            launchctl: Vec::new(),
            cli_pinned: Some(vec!["claude".to_string()]),
            connector_store: None,
            connector_http: None,
            spawn_processes: false,
            registry: Arc::new(Registry::with_pages()),
        }
    }

    /// A state rooted in nonexistent directories, for unit tests that never
    /// touch the disk.
    pub fn for_tests() -> Self {
        let nowhere = Path::new("/nonexistent/loopx-ui-tests");
        Self::explicit(nowhere, nowhere, nowhere, nowhere, "test-csrf-token", None)
    }

    /// Point the event ledger somewhere else (tests, the golden harness).
    pub fn with_events_dir(mut self, dir: &Path) -> Self {
        self.events_dir = dir.to_path_buf();
        self
    }

    /// Inject the connector secret store and HTTP client (tests, the golden harness).
    pub fn with_connectors(
        mut self,
        store: Arc<dyn loopx_core::connectors::secrets::SecretStore>,
        http: Arc<dyn loopx_core::connectors::HttpClient>,
    ) -> Self {
        self.connector_store = Some(store);
        self.connector_http = Some(http);
        self
    }

    /// Point the eval results somewhere else (tests, the golden harness).
    pub fn with_evals_dir(mut self, dir: &Path) -> Self {
        self.evals_dir = dir.to_path_buf();
        self
    }

    /// Replace the "Run now" launcher (tests).
    pub fn with_run_cmd(mut self, cmd: &[&str]) -> Self {
        self.run_cmd = cmd.iter().map(|s| (*s).to_string()).collect();
        self
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.fixed_now.unwrap_or_else(Utc::now)
    }

    pub fn loops_config_path(&self) -> PathBuf {
        self.home_dir.join("loops.json")
    }

    /// `config/loops.json.template` of this checkout.
    pub fn loops_template_path(&self) -> PathBuf {
        self.repo_root.join("config").join("loops.json.template")
    }

    pub fn ai_cli_config_path(&self) -> PathBuf {
        self.home_dir.join("ai_cli.json")
    }

    /// `status_path_for_loop`: `outputs/status.json` for the GitLab loop,
    /// `outputs/status/<name>.json` for every other.
    pub fn status_path_for_loop(&self, name: &str) -> PathBuf {
        if name == "gitlab-loop" {
            self.repo_root.join("outputs").join("status.json")
        } else {
            self.repo_root
                .join("outputs")
                .join("status")
                .join(format!("{name}.json"))
        }
    }

    pub fn status_path(&self) -> PathBuf {
        self.status_path_for_loop("gitlab-loop")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffolding_a_fresh_home_seeds_the_scheduler_state_once() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        scaffold_loops_config(home.path(), repo.path());

        let state =
            loopx_core::loop_scheduler::read_state(&home.path().join("loop_scheduler_state.json"));
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(state["gitlab-loop"]["last_attempted_date"], today);

        // An existing registry is left alone, so the state is not re-seeded either.
        std::fs::remove_file(home.path().join("loop_scheduler_state.json")).unwrap();
        scaffold_loops_config(home.path(), repo.path());
        assert!(!home.path().join("loop_scheduler_state.json").exists());
    }

    #[test]
    fn policy_syntax() {
        let all = RoutePolicy::parse("", "");
        assert!(all.serves("/readme", "/readme"));
        let none = RoutePolicy::parse("none", "");
        assert!(!none.serves("/readme", "/readme"));
        let only = RoutePolicy::parse("/readme, /loops/*", "");
        assert!(only.serves("/readme", "/readme"));
        assert!(only.serves("/loops/x", "/loops/:name"));
        assert!(!only.serves("/update", "/update"));
        let legacy = RoutePolicy::parse("", "/readme,/loops/:name");
        assert!(!legacy.serves("/readme", "/readme"));
        assert!(!legacy.serves("/loops/x", "/loops/:name"));
        assert!(legacy.serves("/update", "/update"));
    }
}

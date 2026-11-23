//! `loopx screenshot check --spec FILE --out FILE` (internal): the `ui_check`
//! verifier's subprocess. It starts the branch's dev server, probes each
//! declared page in headless Chrome and writes a JSON report, so dev servers
//! and Chrome never live inside the long-running loop process.

use loopx_core::screenshot::{
    already_served, clean_up_on_signals, probe, stop_everything_and_exit, DevServer, ProbeOptions,
};
use loopx_core::ui_check::UiChecks;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SERVE_TIMEOUT: Duration = Duration::from_secs(90);
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long past its deadline the check may spend closing Chrome and the
/// server before the watchdog stops everything itself.
const TEARDOWN_GRACE: Duration = Duration::from_secs(15);
const MAX_TAIL_CHARS: usize = 2000;

#[derive(Deserialize)]
struct CheckSpec {
    #[serde(flatten)]
    checks: UiChecks,
    cwd: PathBuf,
    #[serde(default)]
    axe: bool,
    #[serde(default = "default_deadline")]
    deadline_s: u64,
}

fn default_deadline() -> u64 {
    300
}

fn parse_args(argv: &[String]) -> Result<(PathBuf, PathBuf), String> {
    match argv {
        [a, spec, b, out] if a == "--spec" && b == "--out" => {
            Ok((PathBuf::from(spec), PathBuf::from(out)))
        }
        _ => Err("usage: loopx screenshot check --spec FILE --out FILE".into()),
    }
}

fn load_spec(path: &Path) -> Result<CheckSpec, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read the spec: {e}"))?;
    let mut spec: CheckSpec =
        serde_json::from_str(&text).map_err(|e| format!("invalid spec: {e}"))?;
    spec.checks.revalidate()?;
    if !spec.cwd.is_dir() {
        return Err(format!("{} is not a directory", spec.cwd.display()));
    }
    if spec.deadline_s == 0 || spec.deadline_s > 600 {
        return Err("deadline_s must be 1-600".into());
    }
    Ok(spec)
}

fn write_report(out: &Path, report: &Value) -> Result<(), String> {
    let tmp = out.with_extension("tmp");
    std::fs::write(&tmp, report.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, out).map_err(|e| e.to_string())
}

fn clip_tail(s: &str) -> String {
    let s = loopx_core::dependency_audit::redact_home(s);
    let n = s.chars().count();
    if n <= MAX_TAIL_CHARS {
        s
    } else {
        s.chars().skip(n - MAX_TAIL_CHARS).collect()
    }
}

pub fn run(argv: &[String]) -> ExitCode {
    let spec = match parse_args(argv).and_then(|(spec, out)| Ok((load_spec(&spec)?, out))) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("loopx screenshot check: {e}");
            return ExitCode::from(2);
        }
    };
    let (spec, out) = spec;
    clean_up_on_signals();
    let deadline = Instant::now() + Duration::from_secs(spec.deadline_s);
    let report = Arc::new(Mutex::new(json!({
        "serve": {"command": spec.checks.serve, "started": false},
        "pages": [],
        "deadline_hit": false,
    })));
    {
        let (report, out) = (report.clone(), out.clone());
        // SAFETY: plain syscall.
        let parent = unsafe { libc::getppid() };
        std::thread::spawn(move || {
            let until = deadline + TEARDOWN_GRACE;
            // A runner stopped from the dashboard dies without signalling this
            // process group; nobody would read the report or restore the tree.
            while Instant::now() < until && unsafe { libc::getppid() } == parent {
                std::thread::sleep(Duration::from_millis(200));
            }
            let mut r = report.lock().unwrap_or_else(|e| e.into_inner());
            r["deadline_hit"] = json!(Instant::now() >= until);
            let _ = write_report(&out, &r);
            stop_everything_and_exit(0);
        });
    }
    check(&spec, deadline, &report);
    let r = report.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match write_report(&out, &r) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "loopx screenshot check: cannot write {}: {e}",
                out.display()
            );
            ExitCode::from(1)
        }
    }
}

fn check(spec: &CheckSpec, deadline: Instant, report: &Mutex<Value>) {
    let set = |f: &dyn Fn(&mut Value)| f(&mut report.lock().unwrap_or_else(|e| e.into_inner()));
    let checks = &spec.checks;
    let root = format!("{}/", checks.origin);
    if already_served(&root) {
        set(&|r| r["serve"]["tail"] = json!(format!("something already serves {}", checks.origin)));
        return;
    }
    let (env, argv) = &checks.serve_cmd;
    let env = loopx_core::screenshot::dev_server_env(env);
    let left = || deadline.saturating_duration_since(Instant::now());
    let _server = match DevServer::start(argv, &env, &spec.cwd, &root, SERVE_TIMEOUT.min(left())) {
        Ok(s) => s,
        Err(e) => {
            let tail = clip_tail(&e);
            set(&|r| r["serve"]["tail"] = json!(tail));
            set(&|r| r["deadline_hit"] = json!(left().is_zero()));
            return;
        }
    };
    set(&|r| r["serve"]["started"] = json!(true));
    for page in &checks.pages {
        let budget = PAGE_TIMEOUT.min(left());
        if budget < Duration::from_secs(1) {
            set(&|r| r["deadline_hit"] = json!(true));
            break;
        }
        let mut opts = ProbeOptions::new(checks.url(page));
        opts.wait_for = page.wait_for.clone();
        opts.dark = page.dark;
        opts.viewport = page.viewport;
        opts.mocks = checks.mocks.clone();
        opts.session = checks.session.clone();
        opts.axe = spec.axe;
        opts.timeout = budget;
        let entry = match probe(&opts) {
            Ok(r) => json!({"label": page.label, "path": page.path, "report": r}),
            Err(e) => json!({"label": page.label, "path": page.path, "error": clip_tail(&e)}),
        };
        set(&|r| {
            if let Some(pages) = r["pages"].as_array_mut() {
                pages.push(entry.clone());
            }
        });
    }
    if left().is_zero() {
        set(&|r| r["deadline_hit"] = json!(true));
    }
}

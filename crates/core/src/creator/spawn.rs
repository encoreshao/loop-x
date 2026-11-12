//! Launching Creator jobs: the detached agent process and the per-skill argv.
//!
//! Only Claude can be fenced to a fixed tool set, so any other selected AI CLI is
//! refused rather than started with its full tool access.

use super::jobs::{self, JobKind, JobRecord};
use super::skills::{self, Engine};
use super::VideoOptions;
use std::path::{Path, PathBuf};

/// The longest search request a scout job accepts (the page and chat rely on it).
pub const MAX_QUERY_CHARS: usize = 500;

/// Per-job spend caps (`claude --max-budget-usd`). There is no Creator budget
/// setting; these are conservative fixed limits so a runaway or prompt-injected
/// job stops on its own. A video render does far more turns than a search.
pub const GENERATE_BUDGET_USD: f64 = 8.0;
pub const SCOUT_BUDGET_USD: f64 = 2.0;

/// The only built-in tools each job has at all (`--tools`); permission rules then
/// narrow them further.
const GENERATE_TOOLS: &str = "Bash,Read,Write,Edit";
const SCOUT_TOOLS: &str = "WebSearch,WebFetch,Read,Write,Edit";

/// Runs the agent (`"$@"`, never re-parsed) and then appends its exit status, on a
/// line of its own even when the agent's output did not end with a newline, to the
/// log named by `$LOOPX_JOB_LOG`, which `jobs` reads to tell done from failed. A
/// SIGTERM from Stop is held until the agent exits (a trap with a handler, unlike
/// an ignored signal, is not inherited), so the group leader outlives the agent and
/// the stop escalates to SIGKILL for the whole group if the agent ignores SIGTERM.
const EXIT_WRAPPER: &str =
    r#"trap : TERM; "$@"; printf '\nloopx-exit=%s\n' "$?" >> "$LOOPX_JOB_LOG""#;

/// The full spawn seam: program argv, working directory, log path -> child pid.
type SpawnFull<'a> = dyn Fn(&[String], &Path, &Path) -> Result<i64, String> + 'a;

/// Start `argv` detached in `cwd` under [`EXIT_WRAPPER`], in its own session /
/// process group (so its pid is its pgid and `jobs::stop` can kill the whole tree),
/// stdin null, stdout and stderr appended to `log` (created 0600), and a final
/// `loopx-exit=<status>` line. Returns the wrapper's pid.
#[cfg(unix)]
pub fn spawn_agent(argv: &[String], cwd: &Path, log: &Path) -> Result<i64, String> {
    let current = std::env::var("PATH").unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let home = std::env::var_os("HOME").map(PathBuf::from);
    spawn_agent_in(argv, cwd, log, &job_path(&shell, &current), home.as_deref())
}

/// How long (milliseconds) the agent's shell may run one command: half an hour.
const SHELL_TIMEOUT_MS: &str = "1800000";

/// The `PATH` a job runs with: the user's login-shell `PATH` (pyenv shims, nvm,
/// Homebrew, ~/.local/bin: where yt-dlp, python packages and node live, none of
/// which a dashboard started by the Mac app or launchd has), then the server's own.
fn job_path(shell: &str, current: &str) -> String {
    crate::ai_cli_health::login_shell_path(shell, current)
}

/// Where a CLI usually lives that a GUI-launched server's `PATH` (the Mac app,
/// launchd) does not list, relative to the user's home or absolute.
const EXTRA_BIN_DIRS: [&str; 6] = [
    ".local/bin",
    ".claude/local",
    ".npm-global/bin",
    ".bun/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
];

fn extra_bin_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    EXTRA_BIN_DIRS
        .iter()
        .filter_map(|d| match (d.starts_with('/'), home) {
            (true, _) => Some(PathBuf::from(d)),
            (false, Some(h)) => Some(h.join(d)),
            (false, None) => None,
        })
        .collect()
}

/// `path` plus every [`EXTRA_BIN_DIRS`] entry that exists and is not listed yet.
fn launch_path(path: &str, home: Option<&Path>) -> String {
    let mut parts: Vec<String> = path
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect();
    for d in extra_bin_dirs(home) {
        let d = d.display().to_string();
        if Path::new(&d).is_dir() && !parts.contains(&d) {
            parts.push(d);
        }
    }
    parts.join(":")
}

/// A bare `name` found only outside `path` becomes its absolute path; anything
/// else (already on `path`, an explicit path, not found) is returned unchanged.
fn resolve_program(name: &str, path: &str, home: Option<&Path>) -> String {
    if name.contains('/') || crate::doctor::find_on_path(name, path).is_some() {
        return name.to_string();
    }
    extra_bin_dirs(home)
        .into_iter()
        .find_map(|d| crate::doctor::find_on_path(name, &d.display().to_string()))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

#[cfg(unix)]
fn spawn_agent_in(
    argv: &[String],
    cwd: &Path,
    log: &Path,
    path: &str,
    home: Option<&Path>,
) -> Result<i64, String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let program = argv
        .first()
        .ok_or_else(|| "no command to run".to_string())?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", EXIT_WRAPPER, "sh"])
        .arg(resolve_program(program, path, home))
        .args(&argv[1..])
        .env("PATH", launch_path(path, home))
        .env("LOOPX_JOB_LOG", log)
        // A 2 to 5 minute video takes ten minutes or more to render, and the agent's shell tool kills a
        // command after two by default: let one command run for up to half an hour.
        .env("BASH_DEFAULT_TIMEOUT_MS", SHELL_TIMEOUT_MS)
        .env("BASH_MAX_TIMEOUT_MS", SHELL_TIMEOUT_MS)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;
    Ok(i64::from(child.id()))
}

#[cfg(not(unix))]
pub fn spawn_agent(_argv: &[String], _cwd: &Path, _log: &Path) -> Result<i64, String> {
    Err("Creator jobs are only supported on Unix".to_string())
}

/// Where a job's permission rules point: its working directory, the user's home
/// (credential files), the loopx home (`LOOP_ENGINEERING_HOME` may sit elsewhere)
/// and the running loopx binary (denied like the bare `loopx` name).
#[derive(Debug, Clone, Copy)]
pub struct Fence<'a> {
    pub cwd: &'a Path,
    pub user_home: &'a Path,
    pub loopx_home: &'a Path,
    pub exe: &'a str,
}

/// Denied to every Creator job (a deny wins over any allow): the loopx helpers by
/// name and by path (GitLab, Slack, mail, dashboard), network clients and forge
/// CLIs, git, AppleScript and the Keychain, the files holding connector tokens and
/// loopx state, and any write to Claude's own config, skills or hooks. Absolute
/// rules are `/` + an absolute path, Claude's `//abs` form.
fn denied_tools(f: &Fence<'_>) -> Vec<String> {
    let mut t = strs(&["Bash(loopx*)", "Bash(git *)"]);
    t.push(format!("Bash({}*)", f.exe));
    t.extend(common_denied(f));
    t
}

/// [`denied_tools`] without the blanket `loopx` and `git` denies, for a job
/// (the MR fix agent) that is allowed a few named git and loopx commands; such a
/// job must deny the rest of git and loopx itself.
pub(crate) fn common_denied(f: &Fence<'_>) -> Vec<String> {
    let mut t = strs(&[
        "Bash(yt-dlp*)",
        "Bash(jq*)",
        "Bash(curl *)",
        "Bash(wget *)",
        "Bash(glab *)",
        "Bash(gh *)",
        "Bash(osascript *)",
        "Bash(security *)",
        "Bash(open *)",
        "Bash(launchctl *)",
        "Read(**/.env*)",
        "Read(**/*.key)",
        "Read(**/id_rsa*)",
    ]);
    let h = f.user_home.display();
    for p in [
        ".gitlab/**",
        ".slack/**",
        ".loopx/**",
        ".loop-engineering/**",
        ".ssh/**",
        ".aws/**",
        ".config/gh/**",
        ".gitlab-daily-digest.json",
        ".npmrc",
        ".claude.json",
        ".netrc",
        ".docker/**",
        "Library/Cookies/**",
    ] {
        t.push(format!("Read(/{h}/{p})"));
    }
    t.push(format!("Read(/{}/**)", f.loopx_home.display()));
    // Shell start-up files and ssh: a write there runs later, outside the job.
    for p in [".zshrc", ".zprofile", ".bashrc", ".bash_profile", ".ssh/**"] {
        t.push(format!("Edit(/{h}/{p})"));
    }
    // Project settings in the job's own folder would load on its next run.
    t.push(format!("Edit(/{}/.claude/**)", f.cwd.display()));
    // So would its project memory and MCP server config.
    for p in ["CLAUDE.md", "CLAUDE.local.md", ".mcp.json"] {
        t.push(format!("Edit(/{}/{p})", f.cwd.display()));
    }
    // Readable (a skill may live under ~/.claude), never writable.
    for p in [".claude/**", "Library/**"] {
        t.push(format!("Edit(/{h}/{p})"));
    }
    t
}

/// What one job may use: its built-in tool set, allow and deny rules, spend cap.
pub(crate) struct JobFence {
    pub tools: &'static str,
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
    pub budget_usd: f64,
    /// Directories besides the working directory the job's tools may reach.
    pub extra_dirs: Vec<PathBuf>,
}

/// `claude -p` sealed like `agents::sealed`'s command, but with tools:
/// - no MCP server (none of the user's claude.ai connectors or local servers load);
/// - `--setting-sources project,local`: never the user's `~/.claude/settings.json`,
///   whose own allow rules (a bare `Read` / `Write`) would reopen the fence;
/// - hooks off, no session persistence, a spend cap;
/// - only the job's built-in tools exist (`--tools`; no `Skill`, so no skill's
///   `allowed-tools` frontmatter can widen them), narrowed by the allow / deny rules;
/// - `default` permission mode (in -p mode whatever is not allowed is refused);
/// - the prompt last.
pub(crate) fn claude_argv(prompt: &str, cwd: &Path, fence: JobFence) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let mut argv = vec![
        crate::runner_util::resolved_program("claude"),
        s("-p"),
        s("--strict-mcp-config"),
        s("--mcp-config"),
        s(r#"{"mcpServers": {}}"#),
        s("--setting-sources"),
        s("project,local"),
        s("--settings"),
        s(r#"{"disableAllHooks": true}"#),
        s("--no-session-persistence"),
        s("--tools"),
        s(fence.tools),
        s("--add-dir"),
        cwd.display().to_string(),
    ];
    for d in &fence.extra_dirs {
        argv.extend([s("--add-dir"), d.display().to_string()]);
    }
    argv.extend([
        s("--permission-mode"),
        s("default"),
        s("--allowedTools"),
        fence.allowed.join(" "),
        s("--disallowedTools"),
        fence.denied.join(" "),
        // One JSON event per step as it happens (`--verbose` is required with -p);
        // the Job log panel renders them, see `logfmt`.
        s("--output-format"),
        s("stream-json"),
        s("--verbose"),
    ]);
    argv.extend(crate::cost::budget_args(Some(fence.budget_usd)));
    argv.push(s(prompt));
    argv
}

fn require_claude(cli: &str) -> Result<(), String> {
    if cli == "claude" {
        Ok(())
    } else {
        Err(format!(
            "Creator jobs need the Claude CLI: the selected AI CLI ({cli}) cannot be \
             limited to a fixed set of tools. Switch CLI in Settings -> AI CLI."
        ))
    }
}

fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// The inspire-video and inspire-showcase jobs (both run `python3 scripts/...` in the
/// same engine): only the skill's own `scripts/` (which drive yt-dlp, ffmpeg, ffprobe
/// and `npx remotion` themselves) and writes under `output/` that are not code. No media
/// tool directly (their flags open and write arbitrary files), no web tools, no browser
/// cookies, no path escapes out of `scripts/`.
///
/// `f.cwd` is the engine root and `data` the folder that really holds `output/`: the
/// same folder for a clone. A plugin copy only links `output/` to the data folder, so the
/// file tools are let into the real one and the whole copy stays read-only (Claude checks
/// a link's own path as well as where it leads).
pub fn video_argv(
    cli: &str,
    prompt: &str,
    f: &Fence<'_>,
    data: &Path,
) -> Result<Vec<String>, String> {
    require_claude(cli)?;
    let c = f.cwd.display();
    let out = data.join("output");
    let o = out.display();
    let linked = data != f.cwd;
    let mut allowed = strs(&[
        "Bash(python scripts/*)",
        "Bash(python3 scripts/*)",
        "Bash(mkdir *)",
        "Bash(ls *)",
        "Bash(cd *)",
    ]);
    allowed.push(format!("Read(/{c}/**)"));
    allowed.push(format!("Edit(/{o}/**)"));
    let mut denied = denied_tools(f);
    denied.extend(strs(&[
        "Bash(python* scripts/*../*)",
        "Bash(python* *../scripts*)",
        "Bash(python* *--file*)",
        "Bash(python* *--cookies*)",
    ]));
    let mut roots = vec![format!("/{c}")];
    if linked {
        allowed.push(format!("Read(/{o}/**)"));
        denied.push(format!("Edit(/{c}/**)"));
        roots.push(format!("/{o}"));
    }
    for root in roots {
        denied.push(format!("Read({root}/cookies.json)"));
        denied.push(format!("Read({root}/**/*cookies*)"));
    }
    for ext in ["py", "js", "ts"] {
        denied.push(format!("Edit(/{o}/**/*.{ext})"));
    }
    Ok(claude_argv(
        prompt,
        f.cwd,
        JobFence {
            tools: GENERATE_TOOLS,
            allowed,
            denied,
            budget_usd: GENERATE_BUDGET_USD,
            extra_dirs: if linked { vec![out] } else { Vec::new() },
        },
    ))
}

/// The video-scout job: web search/fetch and writes only under `~/.video-scout`; no
/// shell at all. yt-dlp is not offered: its flags (abbreviations, bundled short
/// options, `--use-postprocessor Exec`) read, write and run arbitrary things, so the
/// skill falls back to watch pages, oEmbed and page transcripts. `skill_dir` is the
/// folder the job reads the skill from (see [`skills::scout_skill`]).
pub fn scout_argv(
    cli: &str,
    prompt: &str,
    f: &Fence<'_>,
    skill_dir: &Path,
) -> Result<Vec<String>, String> {
    require_claude(cli)?;
    let c = f.cwd.display();
    let mut allowed = strs(&["WebSearch", "WebFetch"]);
    allowed.push(format!("Read(/{}/**)", skill_dir.display()));
    // Claude checks where a link leads too, and a hand install is often a link.
    if let Ok(real) = skill_dir.canonicalize() {
        if real != skill_dir {
            allowed.push(format!("Read(/{}/**)", real.display()));
        }
    }
    allowed.push(format!("Edit(/{c}/**)"));
    let mut denied = denied_tools(f);
    // Courtesy only, not a boundary: WebFetch can still reach other local names.
    denied.extend(strs(&[
        "WebFetch(domain:localhost)",
        "WebFetch(domain:127.0.0.1)",
        "WebFetch(domain:[::1])",
        "WebFetch(domain:0.0.0.0)",
    ]));
    Ok(claude_argv(
        prompt,
        f.cwd,
        JobFence {
            tools: SCOUT_TOOLS,
            allowed,
            denied,
            budget_usd: SCOUT_BUDGET_USD,
            extra_dirs: Vec::new(),
        },
    ))
}

/// Runs the skill's own first step for a plugin copy, which a job's shell rules refuse:
/// `scripts/prepare.sh` links `output/` and `voices/` to the data folder (`$1`) and
/// installs what a fresh copy is missing (Python packages, `npm install`), printing what
/// it does into the job log. The agent (`"$@"` after the shift) starts only if it worked.
const PREPARE_WRAPPER: &str =
    r#"INSPIRE_VIDEO_HOME="$1" bash scripts/prepare.sh || exit $?; shift; exec "$@""#;

/// `argv` run behind [`PREPARE_WRAPPER`] for a plugin copy; unchanged for a clone, whose
/// install script already did all of it.
fn with_prepare(engine: &Engine, argv: Vec<String>) -> Vec<String> {
    if !engine.plugin {
        return argv;
    }
    let mut wrapped = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        PREPARE_WRAPPER.to_string(),
        "sh".to_string(),
        engine.data.display().to_string(),
    ];
    wrapped.extend(argv);
    wrapped
}

/// Where inspire-video keeps its videos, projects, uploads and voices. Its code may live
/// elsewhere: see [`skills::engine`].
pub fn generate_dir(user_home: &Path) -> PathBuf {
    skills::data_dir(user_home)
}

/// Where video-scout writes its reports and catalogs, and its jobs run.
pub fn scout_dir(user_home: &Path) -> PathBuf {
    user_home.join(".video-scout")
}

/// Everything a start needs besides its input: the selected CLI and the fence paths.
struct JobEnv<'a> {
    cli: &'a str,
    user_home: &'a Path,
    loopx_home: &'a Path,
    exe: &'a str,
}

fn current_exe() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loopx".to_string())
}

/// Start an inspire-video job for `url`. Refuses (creating no job) unless the user
/// confirmed they hold the rights to the source.
pub fn start_generate(
    jobs_dir: &Path,
    user_home: &Path,
    url: &str,
    rights_confirmed: bool,
    options: &VideoOptions,
) -> Result<JobRecord, String> {
    let (cli, loopx_home, exe) = (
        crate::ai_cli_config::selected_cli(),
        crate::config::home_dir(),
        current_exe(),
    );
    let env = JobEnv {
        cli: &cli,
        user_home,
        loopx_home: &loopx_home,
        exe: &exe,
    };
    start_generate_with(jobs_dir, &env, url, rights_confirmed, options, &spawn_agent)
}

/// Start an inspire-showcase job for the website or repository at `url`, for the images
/// in `folder`, or for both. Refuses (creating no job) unless the user confirmed they own
/// the project or may make a video about it.
#[allow(clippy::too_many_arguments)]
pub fn start_showcase(
    jobs_dir: &Path,
    user_home: &Path,
    url: Option<&str>,
    folder: Option<&str>,
    platforms: &[String],
    voice: &str,
    template: &str,
    rights_confirmed: bool,
) -> Result<JobRecord, String> {
    let (cli, loopx_home, exe) = (
        crate::ai_cli_config::selected_cli(),
        crate::config::home_dir(),
        current_exe(),
    );
    let env = JobEnv {
        cli: &cli,
        user_home,
        loopx_home: &loopx_home,
        exe: &exe,
    };
    start_showcase_with(
        jobs_dir,
        &env,
        url,
        folder,
        platforms,
        voice,
        template,
        rights_confirmed,
        &spawn_agent,
    )
}

/// A key for a showcase made from screenshots alone: `folder:` and the folder's name.
fn folder_key(folder: &Path) -> String {
    let name = folder
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("screenshots");
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(60)
        .collect();
    format!("folder:{clean}")
}

#[allow(clippy::too_many_arguments)]
fn start_showcase_with(
    jobs_dir: &Path,
    env: &JobEnv<'_>,
    url: Option<&str>,
    folder: Option<&str>,
    platforms: &[String],
    voice: &str,
    template: &str,
    rights_confirmed: bool,
    spawn: &SpawnFull<'_>,
) -> Result<JobRecord, String> {
    if !rights_confirmed {
        return Err("confirm you own this project or may make a video about it first".to_string());
    }
    let url = url.map(str::trim).filter(|u| !u.is_empty());
    let folder = folder.map(str::trim).filter(|f| !f.is_empty());
    if url.is_none() && folder.is_none() {
        return Err("give a website or repository link, a screenshots folder, or both".to_string());
    }
    let url = url.map(super::valid_source_url).transpose()?;
    let platforms = super::parse_platforms(platforms)?;
    let engine = installed_engine(env.user_home, JobKind::Showcase)?;
    let narration = super::voices::resolve(&engine.data, env.user_home, voice)?;
    let template = super::templates::resolve(&engine.root, template)?;
    let Some(folder) = folder else {
        // Link only: a link run before restarts that job, with the platforms now picked.
        let url = url.unwrap_or_default();
        let earlier = jobs::latest_finished(jobs_dir, JobKind::Showcase, &url).map(|j| j.id);
        let options = VideoOptions {
            platforms,
            screenshots: None,
            narration,
            template,
        };
        return launch_video(
            jobs_dir,
            env,
            JobKind::Showcase,
            &url,
            &options,
            earlier.as_deref(),
            spawn,
        );
    };
    // Screenshots are copied under the skill's output folder first: the agent may read nothing else.
    let folder = PathBuf::from(crate::worktree::expand_home(folder, env.user_home));
    let staged = super::uploads::stage(&engine.data, &folder)?;
    let key = url.clone().unwrap_or_else(|| folder_key(&folder));
    let options = VideoOptions {
        platforms,
        screenshots: Some(staged.clone()),
        narration,
        template,
    };
    // New screenshots always make a new video, never a restart of an earlier one.
    let started = launch_video(
        jobs_dir,
        env,
        JobKind::Showcase,
        &key,
        &options,
        None,
        spawn,
    );
    if started.is_err() {
        let _ = std::fs::remove_dir_all(engine.data.join(&staged));
    }
    started
}

/// Start a video-scout job for `query`.
pub fn start_scout(jobs_dir: &Path, user_home: &Path, query: &str) -> Result<JobRecord, String> {
    let (cli, loopx_home, exe) = (
        crate::ai_cli_config::selected_cli(),
        crate::config::home_dir(),
        current_exe(),
    );
    let env = JobEnv {
        cli: &cli,
        user_home,
        loopx_home: &loopx_home,
        exe: &exe,
    };
    start_scout_with(jobs_dir, &env, query, &spawn_agent)
}

fn start_generate_with(
    jobs_dir: &Path,
    env: &JobEnv<'_>,
    url: &str,
    rights_confirmed: bool,
    options: &VideoOptions,
    spawn: &SpawnFull<'_>,
) -> Result<JobRecord, String> {
    if !rights_confirmed {
        return Err("confirm you hold the rights to use this video first".to_string());
    }
    let url = super::valid_source_url(url)?;
    let options = VideoOptions {
        platforms: super::parse_platforms(&options.platforms)?,
        screenshots: None,
        narration: None,
        template: None,
    };
    // A link run before restarts that job (its files are reused) rather than adding a record.
    let earlier = jobs::latest_finished(jobs_dir, JobKind::Generate, &url).map(|j| j.id);
    launch_video(
        jobs_dir,
        env,
        JobKind::Generate,
        &url,
        &options,
        earlier.as_deref(),
        spawn,
    )
}

/// Run a finished Generate job again from the Jobs list; its earlier work is reused.
pub fn retry_generate(jobs_dir: &Path, user_home: &Path, id: &str) -> Result<JobRecord, String> {
    let (cli, loopx_home, exe) = (
        crate::ai_cli_config::selected_cli(),
        crate::config::home_dir(),
        current_exe(),
    );
    let env = JobEnv {
        cli: &cli,
        user_home,
        loopx_home: &loopx_home,
        exe: &exe,
    };
    retry_generate_with(jobs_dir, &env, id, &spawn_agent)
}

fn retry_generate_with(
    jobs_dir: &Path,
    env: &JobEnv<'_>,
    id: &str,
    spawn: &SpawnFull<'_>,
) -> Result<JobRecord, String> {
    let job = jobs::get(jobs_dir, id).ok_or_else(|| "unknown job".to_string())?;
    if !job.kind.makes_video() {
        return Err("only a video generation job can be retried".to_string());
    }
    // The record keeps what the video was made for, so a retry makes the same one.
    launch_video(
        jobs_dir,
        env,
        job.kind,
        &job.key,
        &job.options,
        Some(id),
        spawn,
    )
}

/// Start a video job (`Generate` or `Showcase`) for `url`: a new record, or
/// `restart_id`'s record run again.
fn launch_video(
    jobs_dir: &Path,
    env: &JobEnv<'_>,
    kind: JobKind,
    key: &str,
    options: &VideoOptions,
    restart_id: Option<&str>,
    spawn: &SpawnFull<'_>,
) -> Result<JobRecord, String> {
    let engine = installed_engine(env.user_home, kind)?;
    let cwd = engine.root.clone();
    let resume = restart_id.is_some();
    let prompt = if kind == JobKind::Showcase {
        jobs::showcase_prompt(&engine, key, true, resume, options)
    } else {
        jobs::generate_prompt(&engine, key, true, resume, options)
    };
    let fence = Fence {
        cwd: &cwd,
        user_home: env.user_home,
        loopx_home: env.loopx_home,
        exe: env.exe,
    };
    // Built up front so an unsupported CLI is refused before any job is recorded.
    let argv = with_prepare(&engine, video_argv(env.cli, &prompt, &fence, &engine.data)?);
    let run = |log: &Path| spawn(&argv, &cwd, log);
    match restart_id {
        Some(id) => jobs::restart_with(jobs_dir, id, Some(options), &run),
        None => jobs::start_with(jobs_dir, kind, key, options, &run),
    }
}

/// How to install either skill, for a job that found none.
const INSTALL_HINT: &str = "install it in Claude Code with `/plugin marketplace add \
    encoreshao/{repo}` and then `/plugin install {repo}@{repo}` (see Settings -> Skills)";

/// The installed engine, when it carries the skill a `kind` job runs.
fn installed_engine(user_home: &Path, kind: JobKind) -> Result<Engine, String> {
    let hint = INSTALL_HINT.replace("{repo}", "inspire-video");
    let engine = skills::engine(user_home)
        .ok_or_else(|| format!("the inspire-video skill is not installed: {hint}"))?;
    if kind == JobKind::Showcase && !engine.has_skill("inspire-showcase") {
        return Err(format!(
            "the inspire-showcase skill is not installed ({} has no {}): update inspire-video",
            engine.root.display(),
            Engine::skill_file("inspire-showcase")
        ));
    }
    Ok(engine)
}

fn start_scout_with(
    jobs_dir: &Path,
    env: &JobEnv<'_>,
    query: &str,
    spawn: &SpawnFull<'_>,
) -> Result<JobRecord, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("a search request is required".to_string());
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(format!(
            "the search request must be at most {MAX_QUERY_CHARS} characters"
        ));
    }
    let skill = skills::scout_skill(env.user_home).ok_or_else(|| {
        format!(
            "the video-scout skill is not installed: {}",
            INSTALL_HINT.replace("{repo}", "video-scout")
        )
    })?;
    let cwd = scout_dir(env.user_home);
    let prompt = jobs::scout_prompt(&skill, query);
    let fence = Fence {
        cwd: &cwd,
        user_home: env.user_home,
        loopx_home: env.loopx_home,
        exe: env.exe,
    };
    let argv = scout_argv(env.cli, &prompt, &fence, &skill)?;
    std::fs::create_dir_all(&cwd).map_err(|e| format!("cannot create {}: {e}", cwd.display()))?;
    jobs::start(jobs_dir, JobKind::Scout, query, &|log| {
        spawn(&argv, &cwd, log)
    })
}

#[cfg(test)]
mod tests {
    use super::super::default_platforms;
    use super::super::skills::testing::{install_clone, install_plugin, write};
    use super::*;
    use std::cell::RefCell;

    const HOME: &str = "/Users/u";
    const LOOPX_HOME: &str = "/sandbox/lx";
    const EXE: &str = "/opt/bin/loopx";
    const SCOUT_SKILL: &str =
        "/Users/u/.claude/plugins/cache/video-scout/video-scout/1.0.0/skills/video-scout";

    /// video-scout installed by hand in `home`; returns the skill folder.
    fn install_scout(home: &Path) -> PathBuf {
        let dir = home.join(".claude/skills/video-scout");
        write(&dir.join("SKILL.md"), "x");
        dir
    }

    fn fence(cwd: &Path) -> Fence<'_> {
        Fence {
            cwd,
            user_home: Path::new(HOME),
            loopx_home: Path::new(LOOPX_HOME),
            exe: EXE,
        }
    }

    fn flag<'a>(argv: &'a [String], name: &str) -> &'a str {
        let i = argv
            .iter()
            .position(|a| a == name)
            .unwrap_or_else(|| panic!("{name} missing from {argv:?}"));
        &argv[i + 1]
    }

    fn rules(s: &str) -> Vec<&str> {
        // Rules are space-separated; a rule's own spaces sit inside parentheses.
        let mut out = Vec::new();
        let (mut depth, mut start) = (0, 0);
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ' ' if depth == 0 => {
                    if i > start {
                        out.push(&s[start..i]);
                    }
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < s.len() {
            out.push(&s[start..]);
        }
        out
    }

    fn allowed(argv: &[String]) -> Vec<&str> {
        rules(flag(argv, "--allowedTools"))
    }

    fn denied(argv: &[String]) -> Vec<&str> {
        rules(flag(argv, "--disallowedTools"))
    }

    fn assert_has(list: &[&str], want: &[String]) {
        for w in want {
            assert!(list.contains(&w.as_str()), "{w} missing from {list:?}");
        }
    }

    /// What both jobs share: no MCP, prompts for anything not allowed, no bare tool
    /// that reaches outside the job, and the credential/loopx denies.
    fn assert_fenced(argv: &[String], skill_dir: &str, tools: &str, budget: &str) {
        // A step-by-step stream for the Job log panel, not one text block at the end.
        assert_eq!(flag(argv, "--output-format"), "stream-json");
        assert!(argv.contains(&"--verbose".to_string()));
        assert_eq!(argv[0], "claude");
        // The user's own settings (bare Read/Write allows, hooks) never load, and
        // nothing persists the session; only the named built-ins exist at all.
        let sources = flag(argv, "--setting-sources");
        assert!(!sources.split(',').any(|s| s.trim() == "user"), "{sources}");
        assert_eq!(sources, "project,local");
        let settings: serde_json::Value = serde_json::from_str(flag(argv, "--settings")).unwrap();
        assert_eq!(settings, serde_json::json!({"disableAllHooks": true}));
        assert!(argv.contains(&"--no-session-persistence".to_string()));
        assert_eq!(flag(argv, "--tools"), tools);
        assert_eq!(flag(argv, "--max-budget-usd"), budget);
        assert!(!argv.iter().any(|a| a.contains("Skill")), "{argv:?}");
        assert!(argv.contains(&"-p".to_string()));
        // No MCP server at all: the user's claude.ai connectors (Slack, Gmail,
        // Calendar, GitLab...) and any locally configured server stay unloaded.
        assert!(argv.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(flag(argv, "--mcp-config"), r#"{"mcpServers": {}}"#);
        // `default`: in -p mode anything not allowed is refused, edits included.
        assert_eq!(flag(argv, "--permission-mode"), "default");
        assert!(!argv.iter().any(|a| a == "acceptEdits"));
        assert!(!argv.iter().any(|a| a.contains("bypassPermissions")));
        assert!(!argv.iter().any(|a| a == "--dangerously-skip-permissions"));
        let allow = allowed(argv);
        for rule in &allow {
            assert!(!rule.starts_with("mcp__"), "an MCP tool is allowed: {rule}");
            for bare in [
                "Bash", "Bash(*)", "Read", "Write", "Edit", "Glob", "Grep", "Skill",
            ] {
                assert_ne!(rule, &bare, "bare {bare} is allowed");
            }
            assert!(!rule.contains("loopx"), "a loopx helper is allowed: {rule}");
            assert!(!rule.contains("curl") && !rule.contains("git"), "{rule}");
            // Flexible openers / runners whose flags a denylist cannot fence.
            for tool in ["yt-dlp", "ffprobe", "ffmpeg", "jq", "node", "npm"] {
                assert!(!rule.contains(tool), "{tool} allowed: {rule}");
            }
        }
        assert_has(&allow, &[format!("Read(/{skill_dir}/**)")]);
        let deny = denied(argv);
        let mut must: Vec<String> = [
            "Bash(yt-dlp*)",
            "Bash(jq*)",
            "Bash(loopx*)",
            "Bash(curl *)",
            "Bash(wget *)",
            "Bash(git *)",
            "Bash(glab *)",
            "Bash(gh *)",
            "Bash(osascript *)",
            "Bash(security *)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        must.push(format!("Bash({EXE}*)"));
        for p in [
            ".gitlab/**",
            ".slack/**",
            ".loopx/**",
            ".loop-engineering/**",
            ".ssh/**",
            ".gitlab-daily-digest.json",
            ".npmrc",
            ".claude.json",
        ] {
            must.push(format!("Read(/{HOME}/{p})"));
        }
        for p in [".netrc", ".docker/**", "Library/Cookies/**"] {
            must.push(format!("Read(/{HOME}/{p})"));
        }
        for p in [".zshrc", ".zprofile", ".bashrc", ".bash_profile", ".ssh/**"] {
            must.push(format!("Edit(/{HOME}/{p})"));
        }
        // The job's own project settings would load on its next run.
        let cwd = flag(argv, "--add-dir");
        // Nor its project memory or MCP config.
        for f in ["CLAUDE.md", "CLAUDE.local.md", ".mcp.json"] {
            must.push(format!("Edit(/{cwd}/{f})"));
        }
        must.push(format!("Edit(/{cwd}/.claude/**)"));
        must.push(format!("Read(/{LOOPX_HOME}/**)"));
        must.push(format!("Edit(/{HOME}/.claude/**)"));
        assert_has(&deny, &must);
        // A skill may be read from ~/.claude (a link, a clone or a plugin copy).
        for tree in [
            format!("Read(/{HOME}/.claude/**)"),
            format!("Read(/{HOME}/.claude/skills/**)"),
            format!("Read(/{HOME}/.claude/plugins/**)"),
            format!("Read(/{skill_dir}/**)"),
        ] {
            assert!(!deny.contains(&tree.as_str()), "{tree} denied");
        }
        assert_eq!(argv.last().unwrap(), "THE PROMPT");
    }

    /// Whether the Bash rule `pattern` (inside `Bash(...)`) matches `command`.
    fn bash_rule_matches(rule: &str, command: &str) -> bool {
        rule.strip_prefix("Bash(")
            .and_then(|r| r.strip_suffix(')'))
            .is_some_and(|p| crate::golden_eval::fnmatchcase(command, p))
    }

    #[test]
    fn generate_argv_allows_bash_but_denies_gitlab_and_slack_tools() {
        let cwd = generate_dir(Path::new(HOME));
        let argv = video_argv("claude", "THE PROMPT", &fence(&cwd), &cwd).unwrap();
        assert_fenced(
            &argv,
            &cwd.display().to_string(),
            "Bash,Read,Write,Edit",
            "8.00",
        );
        let allow = allowed(&argv);
        let c = cwd.display();
        let bash: Vec<&str> = allow
            .iter()
            .copied()
            .filter(|r| r.starts_with("Bash("))
            .collect();
        assert_eq!(
            bash,
            [
                "Bash(python scripts/*)",
                "Bash(python3 scripts/*)",
                "Bash(mkdir *)",
                "Bash(ls *)",
                "Bash(cd *)",
            ]
        );
        assert_has(
            &allow,
            &[format!("Edit(/{c}/output/**)"), format!("Read(/{c}/**)")],
        );
        for rule in &allow {
            assert!(!rule.starts_with("Write("), "unmatched Write rule: {rule}");
            if rule.starts_with("Edit(") {
                assert!(
                    rule.starts_with(&format!(
                        "{}(/{c}/output/",
                        &rule[..rule.find('(').unwrap()]
                    )),
                    "{rule}"
                );
            }
            assert!(!rule.starts_with("Web"), "web tool allowed: {rule}");
        }
        let deny = denied(&argv);
        assert_has(
            &deny,
            &[
                "Bash(python* scripts/*../*)".to_string(),
                "Bash(python* *../scripts*)".to_string(),
                "Bash(python* *--file*)".to_string(),
                "Bash(python* *--cookies*)".to_string(),
                format!("Read(/{c}/cookies.json)"),
                format!("Read(/{c}/**/*cookies*)"),
                format!("Edit(/{c}/output/**/*.py)"),
            ],
        );
        assert!(!deny.iter().any(|r| r.contains("ffprobe")));
        // Path traversal out of scripts/ is refused; an ellipsis in a title is not.
        let refused = |cmd: &str| deny.iter().any(|r| bash_rule_matches(r, cmd));
        for cmd in [
            "python scripts/../output/x.py",
            "python3 scripts/../../evil.py",
            "python3 ../scripts/fetch.py",
            "python scripts/fetch.py --file /etc/passwd --yes-rights",
            "python3 scripts/fetch.py https://x/v --cookies-from-browser chrome",
        ] {
            assert!(refused(cmd), "{cmd} not denied");
        }
        for cmd in [
            "python scripts/fetch.py https://example.com/v --yes-rights",
            "python3 scripts/render.py output/projects/wait... what",
        ] {
            assert!(!refused(cmd), "{cmd} denied");
            assert!(allow.iter().any(|r| bash_rule_matches(r, cmd)), "{cmd}");
        }
    }

    #[test]
    fn scout_argv_allows_only_the_scout_tools_and_denies_gitlab_and_slack_tools() {
        let cwd = scout_dir(Path::new(HOME));
        let argv =
            scout_argv("claude", "THE PROMPT", &fence(&cwd), Path::new(SCOUT_SKILL)).unwrap();
        assert_fenced(
            &argv,
            SCOUT_SKILL,
            "WebSearch,WebFetch,Read,Write,Edit",
            "2.00",
        );
        let allow = allowed(&argv);
        let c = cwd.display();
        assert_has(
            &allow,
            &[
                "WebFetch".to_string(),
                "WebSearch".to_string(),
                format!("Edit(/{c}/**)"),
            ],
        );
        // No shell at all: metadata and transcripts come from the web tools.
        assert!(!allow.iter().any(|r| r.starts_with("Bash")), "{allow:?}");
        let deny = denied(&argv);
        assert_has(
            &deny,
            &strs(&[
                "WebFetch(domain:localhost)",
                "WebFetch(domain:127.0.0.1)",
                "WebFetch(domain:[::1])",
                "WebFetch(domain:0.0.0.0)",
            ]),
        );
    }

    #[test]
    fn other_clis_are_refused_rather_than_run_unrestricted() {
        let home = Path::new(HOME);
        for cli in ["codex", "gemini", "kimi", "cursor", "grok", "nope"] {
            let gd = generate_dir(home);
            let e = video_argv(cli, "p", &fence(&gd), &gd).unwrap_err();
            assert!(e.contains("Claude"), "{cli}: {e}");
            let sd = scout_dir(home);
            assert!(scout_argv(cli, "p", &fence(&sd), Path::new(SCOUT_SKILL)).is_err());
        }
    }

    type Calls = RefCell<Vec<(Vec<String>, PathBuf, PathBuf)>>;

    fn recorder(calls: &Calls) -> impl Fn(&[String], &Path, &Path) -> Result<i64, String> + '_ {
        move |argv, cwd, log| {
            calls
                .borrow_mut()
                .push((argv.to_vec(), cwd.to_path_buf(), log.to_path_buf()));
            Ok(i64::from(std::process::id()))
        }
    }

    fn env<'a>(cli: &'a str, home: &'a Path) -> JobEnv<'a> {
        JobEnv {
            cli,
            user_home: home,
            loopx_home: Path::new(LOOPX_HOME),
            exe: EXE,
        }
    }

    #[test]
    fn start_generate_without_rights_creates_no_job() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        install_clone(&home, false);
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let e = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            false,
            &VideoOptions::default(),
            &recorder(&calls),
        )
        .unwrap_err();
        assert!(e.contains("rights"), "{e}");
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    #[test]
    fn start_generate_launches_claude_in_the_skill_dir() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        install_clone(&home, false);
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let rec = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            true,
            &VideoOptions::default(),
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.kind, JobKind::Generate);
        assert_eq!(rec.key, "https://example.com/v");
        let calls = calls.borrow();
        let (argv, cwd, log) = &calls[0];
        assert_eq!(argv[0], "claude");
        assert_eq!(
            argv.last().unwrap(),
            &jobs::generate_prompt(
                &skills::engine(&home).unwrap(),
                "https://example.com/v",
                true,
                false,
                &VideoOptions::default()
            )
        );
        assert!(denied(argv).contains(&format!("Bash({EXE}*)").as_str()));
        assert_eq!(cwd, &generate_dir(&home));
        assert_eq!(log, &rec.log_path);
    }

    fn install_showcase(home: &Path) {
        install_clone(home, true);
    }

    #[test]
    fn start_showcase_launches_its_own_skill_in_the_same_working_dir_and_fence() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        install_clone(&home, false);
        install_showcase(&home);
        let calls = Calls::default();
        let rec = start_showcase_with(
            &jobs_dir,
            &env("claude", &home),
            Some("https://example.com/app"),
            None,
            &default_platforms(),
            "none",
            "",
            true,
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.kind, JobKind::Showcase);
        let calls = calls.borrow();
        let (argv, cwd, _) = &calls[0];
        assert_eq!(
            argv.last().unwrap(),
            &jobs::showcase_prompt(
                &skills::engine(&home).unwrap(),
                "https://example.com/app",
                true,
                false,
                &VideoOptions::default()
            )
        );
        assert_eq!(cwd, &generate_dir(&home));
        let joined = argv.join(" ");
        assert!(
            joined.contains("First read skills/inspire-showcase/SKILL.md"),
            "{joined}"
        );
        assert!(joined.contains("Bash(python3 scripts/*)"), "{joined}");
        assert!(!joined.contains("WebFetch"), "{joined}");
        drop(calls);
    }

    #[test]
    fn a_finished_showcase_job_is_retried_as_a_showcase() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        install_clone(&home, false);
        install_showcase(&home);
        std::fs::create_dir_all(&jobs_dir).unwrap();
        let id = "20261105T000000-show0000";
        std::fs::write(
            jobs_dir.join(format!("{id}.json")),
            serde_json::json!({"id": id, "kind": "showcase", "key": "https://example.com/app",
                               "state": "failed", "pid": null,
                               "started_at": "2026-11-05T00:00:00Z"})
            .to_string(),
        )
        .unwrap();
        std::fs::write(jobs_dir.join(format!("{id}.log")), "old\n").unwrap();
        let calls = Calls::default();
        let again =
            retry_generate_with(&jobs_dir, &env("claude", &home), id, &recorder(&calls)).unwrap();
        assert_eq!(again.kind, JobKind::Showcase);
        let prompt = calls.borrow()[0].0.last().unwrap().clone();
        assert!(prompt.contains("inspire-showcase") && prompt.contains("previous attempt"));
    }

    #[test]
    fn start_showcase_needs_rights_a_valid_url_and_the_showcase_skill() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        install_clone(&home, false);
        let go = |url: &str, rights: bool| {
            start_showcase_with(
                &jobs_dir,
                &env("claude", &home),
                Some(url),
                None,
                &default_platforms(),
                "none",
                "",
                rights,
                &recorder(&calls),
            )
        };
        assert!(go("https://example.com/app", false)
            .unwrap_err()
            .contains("own"));
        assert!(go("http://example.com/app", true).is_err());
        let e = go("https://example.com/app", true).unwrap_err();
        assert!(e.contains("inspire-showcase"), "{e}");
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    fn shots_folder(d: &Path) -> PathBuf {
        let f = d.join("My Shots");
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join("home.png"), b"x").unwrap();
        std::fs::write(f.join("pricing.jpg"), b"x").unwrap();
        f
    }

    #[test]
    fn a_showcase_from_screenshots_stages_them_under_the_skill_and_keys_the_job_by_folder_name() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        install_clone(&home, false);
        install_showcase(&home);
        let folder = shots_folder(d.path());
        let calls = Calls::default();
        let rec = start_showcase_with(
            &jobs_dir,
            &env("claude", &home),
            None,
            Some(folder.to_str().unwrap()),
            &["linkedin".to_string(), "wechat".to_string()],
            "none",
            "",
            true,
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.key, "folder:My-Shots");
        assert_eq!(rec.options.platforms, ["wechat", "linkedin"]);
        let staged = rec.options.screenshots.clone().unwrap();
        assert!(staged.starts_with("output/uploads/"), "{staged}");
        assert!(generate_dir(&home)
            .join(&staged)
            .join("01-home.png")
            .is_file());
        let prompt = calls.borrow()[0].0.last().unwrap().clone();
        assert!(
            prompt.contains(&format!("ingest.py {staged} --screenshots {staged}")),
            "{prompt}"
        );
        // The agent is never given the folder's own location.
        assert!(!prompt.contains(folder.to_str().unwrap()), "{prompt}");
        assert!(
            prompt.contains("\"wechat\": {...}, \"linkedin\": {...}"),
            "{prompt}"
        );
    }

    #[test]
    fn a_link_and_screenshots_together_make_a_new_job_each_time() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        install_clone(&home, false);
        install_showcase(&home);
        let folder = shots_folder(d.path());
        let calls = Calls::default();
        let go = |url: Option<&str>, folder: Option<&str>| {
            start_showcase_with(
                &jobs_dir,
                &env("claude", &home),
                url,
                folder,
                &default_platforms(),
                "none",
                "",
                true,
                &recorder(&calls),
            )
        };
        let first = go(
            Some("https://example.com/app"),
            Some(folder.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(first.key, "https://example.com/app");
        assert!(first.options.screenshots.is_some());
        // Finish it; the same link with other screenshots must not restart that record.
        std::fs::write(&first.log_path, "x\nloopx-exit=0\n").unwrap();
        let mut done = jobs::get(&jobs_dir, &first.id).unwrap();
        done.state = "done".into();
        std::fs::write(
            jobs_dir.join(format!("{}.json", first.id)),
            serde_json::json!({"id": done.id, "kind": "showcase", "key": done.key,
                               "state": "done", "pid": null, "started_at": done.started_at})
            .to_string(),
        )
        .unwrap();
        let second = go(
            Some("https://example.com/app"),
            Some(folder.to_str().unwrap()),
        )
        .unwrap();
        assert_ne!(second.id, first.id);
        assert_ne!(second.options.screenshots, first.options.screenshots);
    }

    #[test]
    fn a_showcase_needs_a_link_or_a_folder_and_a_usable_folder_and_cleans_up_when_it_cannot_start()
    {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        install_clone(&home, false);
        install_showcase(&home);
        let calls = Calls::default();
        let go = |url: Option<&str>, folder: Option<&str>, cli: &str, platforms: &[String]| {
            start_showcase_with(
                &jobs_dir,
                &env(cli, &home),
                url,
                folder,
                platforms,
                "none",
                "",
                true,
                &recorder(&calls),
            )
        };
        let p = default_platforms();
        assert!(go(None, None, "claude", &p)
            .unwrap_err()
            .contains("or both"));
        assert!(go(Some("  "), Some(" "), "claude", &p).is_err());
        let missing = d.path().join("nope");
        assert!(go(None, Some(missing.to_str().unwrap()), "claude", &p)
            .unwrap_err()
            .contains("does not exist"));
        let empty = d.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(go(None, Some(empty.to_str().unwrap()), "claude", &p)
            .unwrap_err()
            .contains("no png"));
        let folder = shots_folder(d.path());
        assert!(go(None, Some(folder.to_str().unwrap()), "claude", &[])
            .unwrap_err()
            .contains("at least one"));
        // An unsupported CLI is refused after staging: the copy is removed again.
        assert!(go(None, Some(folder.to_str().unwrap()), "gemini", &p).is_err());
        let uploads = generate_dir(&home).join("output/uploads");
        assert!(!uploads.exists() || uploads.read_dir().unwrap().next().is_none());
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    #[test]
    fn a_showcase_template_is_checked_before_any_job_and_kept_for_a_retry() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        let t = generate_dir(&home).join("templates");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("calm.yaml"), "id: calm\nname: Calm\n").unwrap();
        install_showcase(&home);
        let calls = Calls::default();
        let go = |template: &str| {
            start_showcase_with(
                &jobs_dir,
                &env("claude", &home),
                Some("https://example.com/app"),
                None,
                &default_platforms(),
                "none",
                template,
                true,
                &recorder(&calls),
            )
        };
        assert!(go("nope").unwrap_err().contains("unknown template"));
        assert!(calls.borrow().is_empty() && jobs::list_jobs(&jobs_dir).is_empty());
        let rec = go("calm").unwrap();
        assert_eq!(rec.options.template.as_deref(), Some("calm"));
        assert!(calls.borrow()[0]
            .0
            .last()
            .unwrap()
            .contains("--template calm"));
        assert_eq!(
            jobs::get(&jobs_dir, &rec.id)
                .unwrap()
                .options
                .template
                .as_deref(),
            Some("calm"),
            "kept in the record, so a retry makes the same video"
        );
    }

    #[test]
    fn a_showcase_voice_is_checked_before_any_job_and_kept_for_a_retry() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        let skill = generate_dir(&home);
        std::fs::create_dir_all(skill.join("voices/encore")).unwrap();
        std::fs::write(skill.join("voices/encore/prompt.wav"), b"x").unwrap();
        std::fs::write(
            skill.join("voices/encore/meta.json"),
            r#"{"language":"zh"}"#,
        )
        .unwrap();
        install_showcase(&home);
        let calls = Calls::default();
        let go = |voice: &str| {
            start_showcase_with(
                &jobs_dir,
                &env("claude", &home),
                Some("https://example.com/app"),
                None,
                &default_platforms(),
                voice,
                "",
                true,
                &recorder(&calls),
            )
        };
        // Unknown, and a cloned voice without CosyVoice: refused, no job, nothing launched.
        assert!(go("nobody").is_err());
        assert!(go("voice:encore")
            .unwrap_err()
            .contains("setup_cosyvoice.sh"));
        assert!(calls.borrow().is_empty() && jobs::list_jobs(&jobs_dir).is_empty());
        let cv = home.join(".cache/inspire-video/cosyvoice");
        std::fs::create_dir_all(cv.join("venv/bin")).unwrap();
        std::fs::write(cv.join("venv/bin/python"), b"x").unwrap();
        let model = cv.join("repo/pretrained_models/Fun-CosyVoice3-0.5B");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("llm.pt"), b"x").unwrap();
        let rec = go("voice:encore").unwrap();
        assert_eq!(
            rec.options.narration,
            Some(super::super::Narration::Cloned {
                name: "encore".into(),
                lang: "zh".into()
            })
        );
        let prompt = calls.borrow()[0].0.last().unwrap().clone();
        assert!(
            prompt.contains("--engine cosyvoice --voice encore --lang zh"),
            "{prompt}"
        );
        // The record keeps it, so a retry narrates the same way.
        std::fs::write(
            jobs_dir.join(format!("{}.json", rec.id)),
            serde_json::json!({"id": rec.id, "kind": "showcase", "key": rec.key, "state": "failed",
                               "pid": null, "started_at": rec.started_at,
                               "narration": "voice:encore:zh"})
            .to_string(),
        )
        .unwrap();
        let again =
            retry_generate_with(&jobs_dir, &env("claude", &home), &rec.id, &recorder(&calls))
                .unwrap();
        assert_eq!(again.options.narration, rec.options.narration);
        assert!(calls.borrow()[1]
            .0
            .last()
            .unwrap()
            .contains("--voice encore"));
    }

    #[test]
    fn generating_with_picked_platforms_keeps_them_and_a_retry_makes_the_same_video() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        install_clone(&home, false);
        let calls = Calls::default();
        let opts = VideoOptions {
            platforms: vec!["douyin".into(), "facebook".into()],
            screenshots: None,
            narration: None,
            template: None,
        };
        let rec = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            true,
            &opts,
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.options.platforms, ["douyin", "facebook"]);
        let first = calls.borrow()[0].0.last().unwrap().clone();
        assert!(
            first.contains("--platform douyin,facebook") && !first.contains("wechat_channels"),
            "{first}"
        );
        std::fs::write(&rec.log_path, "x\nloopx-exit=0\n").unwrap();
        std::fs::write(
            jobs_dir.join(format!("{}.json", rec.id)),
            serde_json::json!({"id": rec.id, "kind": "generate", "key": rec.key, "state": "failed",
                               "pid": null, "started_at": rec.started_at,
                               "platforms": ["douyin", "facebook"]})
            .to_string(),
        )
        .unwrap();
        let again =
            retry_generate_with(&jobs_dir, &env("claude", &home), &rec.id, &recorder(&calls))
                .unwrap();
        assert_eq!(again.options.platforms, ["douyin", "facebook"]);
        let retry = calls.borrow()[1].0.last().unwrap().clone();
        assert!(retry.contains("--platform douyin,facebook"), "{retry}");
        // An empty or unknown pick is refused before any job.
        let none = VideoOptions {
            platforms: vec![],
            screenshots: None,
            narration: None,
            template: None,
        };
        assert!(start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/w",
            true,
            &none,
            &recorder(&calls)
        )
        .is_err());
    }

    #[test]
    fn start_generate_needs_the_skill_installed() {
        let d = tempfile::tempdir().unwrap();
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let e = start_generate_with(
            &jobs_dir,
            &env("claude", d.path()),
            "https://example.com/v",
            true,
            &VideoOptions::default(),
            &recorder(&calls),
        )
        .unwrap_err();
        assert!(e.contains("inspire-video"), "{e}");
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    #[test]
    fn start_scout_creates_its_dir_and_launches_claude_there() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let skill = install_scout(&home);
        let rec = start_scout_with(
            &jobs_dir,
            &env("claude", &home),
            "stoicism",
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!((rec.kind, rec.key.as_str()), (JobKind::Scout, "stoicism"));
        assert!(scout_dir(&home).is_dir());
        let calls = calls.borrow();
        let (argv, cwd, _) = &calls[0];
        assert_eq!(cwd, &scout_dir(&home));
        assert_eq!(
            argv.last().unwrap(),
            &jobs::scout_prompt(&skill, "stoicism")
        );
        assert_has(&allowed(argv), &[format!("Read(/{}/**)", skill.display())]);
    }

    #[test]
    fn start_scout_needs_the_skill_installed_and_says_how() {
        let d = tempfile::tempdir().unwrap();
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let e = start_scout_with(
            &jobs_dir,
            &env("claude", d.path()),
            "stoicism",
            &recorder(&calls),
        )
        .unwrap_err();
        assert!(
            e.contains("video-scout") && e.contains("/plugin install video-scout@video-scout"),
            "{e}"
        );
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_scout_skill_behind_a_link_is_readable_where_the_link_leads_too() {
        let d = tempfile::tempdir().unwrap();
        let real = d
            .path()
            .canonicalize()
            .unwrap()
            .join("src/skills/video-scout");
        write(&real.join("SKILL.md"), "x");
        let link = d.path().join("video-scout");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let cwd = scout_dir(Path::new(HOME));
        let argv = scout_argv("claude", "p", &fence(&cwd), &link).unwrap();
        assert_has(
            &allowed(&argv),
            &[
                format!("Read(/{}/**)", link.display()),
                format!("Read(/{}/**)", real.display()),
            ],
        );
    }

    #[test]
    fn a_plugin_copy_is_prepared_first_and_writes_only_into_the_data_folder() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        let root = install_plugin(
            &home.join(".claude"),
            "inspire-video",
            &["inspire-video", "inspire-showcase"],
        );
        let data = generate_dir(&home);
        let calls = Calls::default();
        start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            true,
            &VideoOptions::default(),
            &recorder(&calls),
        )
        .unwrap();
        // Screenshots are staged in the data folder: the copy's output/ link may not exist yet.
        let folder = shots_folder(d.path());
        let shots = start_showcase_with(
            &jobs_dir,
            &env("claude", &home),
            None,
            Some(folder.to_str().unwrap()),
            &default_platforms(),
            "none",
            "",
            true,
            &recorder(&calls),
        )
        .unwrap();
        let staged = shots.options.screenshots.unwrap();
        assert!(data.join(&staged).join("01-home.png").is_file());
        assert!(!root.join("output").exists());

        let calls = calls.borrow();
        let (argv, cwd, _) = &calls[0];
        // The job runs in the versioned copy, behind the skill's own prepare step.
        assert_eq!(cwd, &root);
        assert_eq!(
            argv[..5],
            [
                "/bin/sh",
                "-c",
                PREPARE_WRAPPER,
                "sh",
                data.to_str().unwrap()
            ]
        );
        assert!(PREPARE_WRAPPER.starts_with(r#"INSPIRE_VIDEO_HOME="$1" bash scripts/prepare.sh"#));
        let agent = &argv[5..];
        assert_eq!(agent[0], "claude");
        assert_eq!(flag(agent, "--tools"), "Bash,Read,Write,Edit");
        let (r, o) = (root.display(), data.join("output").display().to_string());
        assert_has(
            &allowed(agent),
            &[format!("Edit(/{o}/**)"), format!("Read(/{o}/**)")],
        );
        assert!(
            !allowed(agent)
                .iter()
                .any(|a| a.starts_with(&format!("Edit(/{r}"))),
            "the copy is writable"
        );
        assert_has(
            &denied(agent),
            &[
                format!("Edit(/{r}/**)"),
                format!("Edit(/{o}/**/*.py)"),
                format!("Read(/{o}/**/*cookies*)"),
                format!("Read(/{r}/cookies.json)"),
            ],
        );
        let dirs: Vec<&String> = agent
            .iter()
            .zip(agent.iter().skip(1))
            .filter(|(flag, _)| *flag == "--add-dir")
            .map(|(_, dir)| dir)
            .collect();
        assert_eq!(dirs, [&r.to_string(), &o]);
        assert_eq!(
            agent.last().unwrap(),
            &jobs::generate_prompt(
                &skills::engine(&home).unwrap(),
                "https://example.com/v",
                true,
                false,
                &VideoOptions::default()
            )
        );
    }

    #[test]
    fn unsupported_cli_or_blank_input_creates_no_job() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        install_clone(&home, false);
        install_scout(&home);
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let spawn = recorder(&calls);
        assert!(start_scout_with(&jobs_dir, &env("codex", &home), "q", &spawn).is_err());
        assert!(start_generate_with(
            &jobs_dir,
            &env("gemini", &home),
            "https://x/v",
            true,
            &VideoOptions::default(),
            &spawn
        )
        .is_err());
        assert!(start_scout_with(&jobs_dir, &env("claude", &home), "  ", &spawn).is_err());
        assert!(start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            " ",
            true,
            &VideoOptions::default(),
            &spawn
        )
        .is_err());
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    #[test]
    fn start_generate_refuses_an_unsafe_url_before_any_job() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        install_clone(&home, false);
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        for url in [
            "http://example.com/v",
            "https://example.com/$(touch /tmp/pwned)",
            "https://example.com/v;id",
            "https://example.com/`id`",
            "https://exa mple.com/v",
        ] {
            let e = start_generate_with(
                &jobs_dir,
                &env("claude", &home),
                url,
                true,
                &VideoOptions::default(),
                &recorder(&calls),
            )
            .unwrap_err();
            assert!(!e.is_empty(), "{url}");
        }
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
        // A good one is started under its normalized form.
        let rec = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            " HTTPS://Example.com/v ",
            true,
            &VideoOptions::default(),
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.key, "https://example.com/v");
    }

    #[test]
    fn start_scout_refuses_an_overlong_query() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let long = "x".repeat(MAX_QUERY_CHARS + 1);
        let e = start_scout_with(&jobs_dir, &env("claude", &home), &long, &recorder(&calls))
            .unwrap_err();
        assert!(e.contains(&MAX_QUERY_CHARS.to_string()), "{e}");
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
        install_scout(&home);
        let fits = "é".repeat(MAX_QUERY_CHARS);
        start_scout_with(&jobs_dir, &env("claude", &home), &fits, &recorder(&calls)).unwrap();
    }

    #[cfg(unix)]
    fn wait_dead(pid: i64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::run_control::process_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_bare_program_is_found_in_the_user_bin_dirs_the_server_path_lacks() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("fakeagent");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Not on the server's PATH, found through ~/.local/bin.
        assert_eq!(
            resolve_program("fakeagent", "/usr/bin:/bin", Some(home.path())),
            exe.display().to_string()
        );
        let lp = launch_path("/usr/bin:/bin", Some(home.path()));
        assert!(
            lp.starts_with(&format!("/usr/bin:/bin:{}", bin.display())),
            "{lp}"
        );
        // A name nowhere to be found, and an explicit path, are left alone.
        assert_eq!(
            resolve_program("nope-x", "/usr/bin", Some(home.path())),
            "nope-x"
        );
        assert_eq!(
            resolve_program("/abs/claude", "/usr/bin", Some(home.path())),
            "/abs/claude"
        );
        // On the PATH already: untouched.
        assert_eq!(
            resolve_program("sh", "/bin:/usr/bin", Some(home.path())),
            "sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_plugin_copys_prepare_step_runs_before_the_agent_and_a_failure_stops_the_job() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("copy");
        let engine = Engine {
            root: root.clone(),
            data: d.path().join("data"),
            plugin: true,
        };
        let agent = vec!["echo".to_string(), "agent ran".to_string()];
        let run = |script: &str, log: &str| {
            write(&root.join("scripts/prepare.sh"), script);
            let log = d.path().join(log);
            let argv = with_prepare(&engine, agent.clone());
            let pid = spawn_agent_in(&argv, &root, &log, "/usr/bin:/bin", None).unwrap();
            wait_dead(pid);
            std::fs::read_to_string(&log).unwrap()
        };
        // The data folder reaches the script the way the skill reads it.
        assert_eq!(
            run("echo \"data=$INSPIRE_VIDEO_HOME\"\n", "ok.log"),
            format!(
                "data={}\nagent ran\n\nloopx-exit=0\n",
                engine.data.display()
            )
        );
        assert_eq!(
            run("echo 'npm install failed' >&2\nexit 3\n", "bad.log"),
            "npm install failed\n\nloopx-exit=3\n"
        );
        // A clone needs none of it.
        let clone = Engine {
            plugin: false,
            ..engine.clone()
        };
        assert_eq!(with_prepare(&clone, agent.clone()), agent);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_agent_runs_a_program_that_only_lives_in_a_user_bin_dir() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("loopx-fake-agent");
        std::fs::write(&exe, "#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = home.path().join("j.log");
        let argv = vec!["loopx-fake-agent".to_string()];
        let pid =
            spawn_agent_in(&argv, home.path(), &log, "/usr/bin:/bin", Some(home.path())).unwrap();
        wait_dead(pid);
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text, "ran\n\nloopx-exit=0\n");
    }

    #[cfg(unix)]
    #[test]
    fn the_job_sees_the_path_it_was_given_plus_the_user_bin_dirs() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("p.log");
        let argv: Vec<String> = ["sh", "-c", "printf %s \"$PATH\""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pid =
            spawn_agent_in(&argv, d.path(), &log, "/usr/bin:/bin:/pyenv/shims", None).unwrap();
        wait_dead(pid);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("/usr/bin:/bin:/pyenv/shims"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn the_agents_shell_waits_long_enough_for_a_long_render() {
        // Claude's shell tool kills a command after 2 minutes unless told otherwise; a 2 to 5 minute video
        // takes longer than that to render, and the first long showcase died at exactly that point.
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("t.log");
        let argv: Vec<String> = [
            "sh",
            "-c",
            "printf '%s %s' \"$BASH_DEFAULT_TIMEOUT_MS\" \"$BASH_MAX_TIMEOUT_MS\"",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let pid = spawn_agent_in(&argv, d.path(), &log, "/usr/bin:/bin", None).unwrap();
        wait_dead(pid);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("1800000 1800000"), "{text}");
    }

    #[test]
    fn a_shell_that_cannot_answer_leaves_the_server_path_alone() {
        // The login-shell lookup itself is tested in ai_cli_health.
        assert_eq!(
            job_path("/definitely/not/a/shell", "/usr/bin:/bin"),
            "/usr/bin:/bin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_agent_appends_the_exit_status_to_the_log() {
        let d = tempfile::tempdir().unwrap();
        for (script, want) in [
            ("exit 0", "\nloopx-exit=0\n"),
            ("exit 3", "\nloopx-exit=3\n"),
        ] {
            let log = d.path().join(format!("{}.log", want.len()));
            let _ = std::fs::remove_file(&log);
            let argv: Vec<String> = ["sh", "-c", script].iter().map(|s| s.to_string()).collect();
            let pid = spawn_agent(&argv, d.path(), &log).unwrap();
            wait_dead(pid);
            let text = std::fs::read_to_string(&log).unwrap();
            assert!(text.ends_with(want), "{script}: {text:?}");
        }
        // Arguments are passed through as they are, never re-parsed by the shell.
        let log = d.path().join("args.log");
        let argv: Vec<String> = ["printf", "%s|", "a b", "$(echo no)", "c;d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pid = spawn_agent(&argv, d.path(), &log).unwrap();
        wait_dead(pid);
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text, "a b|$(echo no)|c;d|\nloopx-exit=0\n");
    }

    #[cfg(unix)]
    #[test]
    fn stopping_a_wrapped_job_kills_its_child_too() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("job.log");
        let inner = d.path().join("inner.pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", inner.display());
        let argv: Vec<String> = ["sh", "-c", &script]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pid = spawn_agent(&argv, d.path(), &log).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !inner.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let child: i64 = std::fs::read_to_string(&inner)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(child, pid, "the agent runs under the wrapper shell");
        assert!(crate::run_control::process_alive(child));
        crate::run_control::kill_process_group(
            pid,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(20),
        );
        assert!(!crate::run_control::process_alive(pid));
        wait_dead(child);
        assert!(!crate::run_control::process_alive(child));
    }

    #[cfg(unix)]
    #[test]
    fn spawn_agent_writes_log_and_returns_pid() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("job.log");
        std::fs::write(&log, "before\n").unwrap();
        let argv: Vec<String> = ["sh", "-c", "pwd; echo hi; echo err >&2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pid = spawn_agent(&argv, d.path(), &log).unwrap();
        assert!(pid > 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::run_control::process_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("before\n"), "{text}");
        assert!(text.contains("hi\n") && text.contains("err\n"), "{text}");
        let real = d.path().canonicalize().unwrap();
        assert!(text.contains(&*real.display().to_string()), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_agent_creates_the_log_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("new.log");
        let argv: Vec<String> = ["true"].iter().map(|s| s.to_string()).collect();
        spawn_agent(&argv, d.path(), &log).unwrap();
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_agent_puts_the_child_in_its_own_process_group() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("job.log");
        let argv: Vec<String> = ["sleep", "30"].iter().map(|s| s.to_string()).collect();
        let pid = spawn_agent(&argv, d.path(), &log).unwrap();
        let pgid = unsafe { libc::getpgid(pid as i32) };
        assert_eq!(i64::from(pgid), pid);
        crate::run_control::kill_process_group(
            pid,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(20),
        );
        assert!(!crate::run_control::process_alive(pid));
    }

    #[test]
    fn empty_argv_is_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(spawn_agent(&[], d.path(), &d.path().join("l.log")).is_err());
    }

    fn finished_generate(jobs_dir: &Path, id: &str, url: &str, state: &str) {
        std::fs::create_dir_all(jobs_dir).unwrap();
        std::fs::write(
            jobs_dir.join(format!("{id}.json")),
            serde_json::json!({"id": id, "kind": "generate", "key": url, "state": state,
                               "pid": null, "started_at": "2026-11-05T00:00:00Z"})
            .to_string(),
        )
        .unwrap();
        std::fs::write(jobs_dir.join(format!("{id}.log")), "old\n").unwrap();
    }

    #[test]
    fn a_duplicate_link_restarts_its_earlier_job_instead_of_adding_a_record() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        install_clone(&home, false);
        let jobs_dir = d.path().join("jobs");
        finished_generate(
            &jobs_dir,
            "20261105T000000-prev0000",
            "https://example.com/v",
            "done",
        );
        let calls = Calls::default();
        let rec = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            true,
            &VideoOptions::default(),
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.id, "20261105T000000-prev0000");
        assert_eq!(jobs::list_jobs(&jobs_dir).len(), 1);
        let calls = calls.borrow();
        assert_eq!(
            calls[0].0.last().unwrap(),
            &jobs::generate_prompt(
                &skills::engine(&home).unwrap(),
                "https://example.com/v",
                true,
                true,
                &VideoOptions::default()
            )
        );
        assert_eq!(std::fs::read_to_string(&rec.log_path).unwrap(), "");
    }

    #[test]
    fn retrying_a_job_restarts_that_record_with_the_resume_prompt() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        install_clone(&home, false);
        let jobs_dir = d.path().join("jobs");
        finished_generate(
            &jobs_dir,
            "20261105T000000-older000",
            "https://example.com/v",
            "failed",
        );
        finished_generate(
            &jobs_dir,
            "20261105T000001-newer000",
            "https://example.com/v",
            "failed",
        );
        let calls = Calls::default();
        let rec = retry_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "20261105T000000-older000",
            &recorder(&calls),
        )
        .unwrap();
        assert_eq!(rec.id, "20261105T000000-older000");
        assert_eq!(jobs::list_jobs(&jobs_dir).len(), 2);
        assert!(calls.borrow()[0]
            .0
            .last()
            .unwrap()
            .contains("previous attempt"));
        // not a generate job, not finished, or unknown: refused, nothing launched
        assert!(
            retry_generate_with(&jobs_dir, &env("claude", &home), "nope", &recorder(&calls))
                .is_err()
        );
        assert!(
            retry_generate_with(
                &jobs_dir,
                &env("claude", &home),
                "20261105T000000-older000",
                &recorder(&calls)
            )
            .is_err(),
            "already running"
        );
        assert_eq!(calls.borrow().len(), 1);
    }
}

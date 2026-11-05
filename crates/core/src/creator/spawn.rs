//! Launching Creator jobs: the detached agent process and the per-skill argv.
//!
//! Only Claude can be fenced to a fixed tool set, so any other selected AI CLI is
//! refused rather than started with its full tool access.

use super::jobs::{self, JobKind, JobRecord};
use std::path::{Path, PathBuf};

/// The longest search request a scout job accepts (the page and chat rely on it).
pub const MAX_QUERY_CHARS: usize = 500;

/// Per-job spend caps (`claude --max-budget-usd`). There is no Creator budget
/// setting; these are conservative fixed limits so a runaway or prompt-injected
/// job stops on its own. A video render does far more turns than a search.
pub const GENERATE_BUDGET_USD: f64 = 5.0;
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
        .args(argv)
        .env("LOOPX_JOB_LOG", log)
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
    let mut t = strs(&[
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
        "Bash(open *)",
        "Bash(launchctl *)",
        "Read(**/.env*)",
        "Read(**/*.key)",
        "Read(**/id_rsa*)",
    ]);
    t.push(format!("Bash({}*)", f.exe));
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
        t.push(format!("Write(/{h}/{p})"));
    }
    // Project settings in the job's own folder would load on its next run.
    t.push(format!("Edit(/{}/.claude/**)", f.cwd.display()));
    t.push(format!("Write(/{}/.claude/**)", f.cwd.display()));
    // So would its project memory and MCP server config.
    for p in ["CLAUDE.md", "CLAUDE.local.md", ".mcp.json"] {
        t.push(format!("Edit(/{}/{p})", f.cwd.display()));
        t.push(format!("Write(/{}/{p})", f.cwd.display()));
    }
    // Readable (the skills live under ~/.claude/skills), never writable.
    for p in [".claude/**", "Library/**"] {
        t.push(format!("Edit(/{h}/{p})"));
        t.push(format!("Write(/{h}/{p})"));
    }
    t
}

/// What one job may use: its built-in tool set, allow and deny rules, spend cap.
struct JobFence {
    tools: &'static str,
    allowed: Vec<String>,
    denied: Vec<String>,
    budget_usd: f64,
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
fn claude_argv(prompt: &str, cwd: &Path, fence: JobFence) -> Vec<String> {
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
        s("--permission-mode"),
        s("default"),
        s("--allowedTools"),
        fence.allowed.join(" "),
        s("--disallowedTools"),
        fence.denied.join(" "),
        s("--output-format"),
        s("text"),
    ];
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

/// The inspire-video job: only the skill's own `scripts/` (which drive yt-dlp,
/// ffmpeg, ffprobe and `npx remotion` themselves) and writes under `output/` that
/// are not code. No media tool directly (their flags open and write arbitrary
/// files), no web tools, no browser cookies, no path escapes out of `scripts/`.
pub fn generate_argv(cli: &str, prompt: &str, f: &Fence<'_>) -> Result<Vec<String>, String> {
    require_claude(cli)?;
    let c = f.cwd.display();
    let h = f.user_home.display();
    let skill = format!("/{h}/.claude/skills/inspire-video");
    let mut allowed = strs(&[
        "Bash(python scripts/*)",
        "Bash(python3 scripts/*)",
        "Bash(mkdir *)",
        "Bash(ls *)",
        "Bash(cd *)",
    ]);
    allowed.push(format!("Read({skill}/**)"));
    allowed.push(format!("Write(/{c}/output/**)"));
    allowed.push(format!("Edit(/{c}/output/**)"));
    let mut denied = denied_tools(f);
    denied.extend(strs(&[
        "Bash(python* scripts/*../*)",
        "Bash(python* *../scripts*)",
        "Bash(python* *--file*)",
        "Bash(python* *--cookies*)",
    ]));
    // The skill dir is also reachable through its ~/.claude/skills symlink.
    for root in [format!("/{c}"), skill] {
        denied.push(format!("Read({root}/cookies.json)"));
        denied.push(format!("Read({root}/**/*cookies*)"));
    }
    for ext in ["py", "js", "ts"] {
        denied.push(format!("Write(/{c}/output/**/*.{ext})"));
        denied.push(format!("Edit(/{c}/output/**/*.{ext})"));
    }
    Ok(claude_argv(
        prompt,
        f.cwd,
        JobFence {
            tools: GENERATE_TOOLS,
            allowed,
            denied,
            budget_usd: GENERATE_BUDGET_USD,
        },
    ))
}

/// The video-scout job: web search/fetch and writes only under `~/.video-scout`; no
/// shell at all. yt-dlp is not offered: its flags (abbreviations, bundled short
/// options, `--use-postprocessor Exec`) read, write and run arbitrary things, so the
/// skill falls back to watch pages, oEmbed and page transcripts.
pub fn scout_argv(cli: &str, prompt: &str, f: &Fence<'_>) -> Result<Vec<String>, String> {
    require_claude(cli)?;
    let c = f.cwd.display();
    let h = f.user_home.display();
    let mut allowed = strs(&["WebSearch", "WebFetch"]);
    allowed.push(format!("Read(/{h}/.claude/skills/video-scout/**)"));
    allowed.push(format!("Write(/{c}/**)"));
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
        },
    ))
}

/// Where the inspire-video skill lives and its jobs run.
pub fn generate_dir(user_home: &Path) -> PathBuf {
    user_home.join(".inspire-video")
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
    start_generate_with(jobs_dir, &env, url, rights_confirmed, &spawn_agent)
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
    spawn: &SpawnFull<'_>,
) -> Result<JobRecord, String> {
    if !rights_confirmed {
        return Err("confirm you hold the rights to use this video first".to_string());
    }
    let url = super::valid_source_url(url)?;
    let url = url.as_str();
    let cwd = generate_dir(env.user_home);
    if !cwd.is_dir() {
        return Err(format!(
            "the inspire-video skill is not installed ({} is missing)",
            cwd.display()
        ));
    }
    let prompt = jobs::generate_prompt(url, true);
    let fence = Fence {
        cwd: &cwd,
        user_home: env.user_home,
        loopx_home: env.loopx_home,
        exe: env.exe,
    };
    // Built up front so an unsupported CLI is refused before any job is recorded.
    let argv = generate_argv(env.cli, &prompt, &fence)?;
    jobs::start(jobs_dir, JobKind::Generate, url, &|log| {
        spawn(&argv, &cwd, log)
    })
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
    let cwd = scout_dir(env.user_home);
    let prompt = jobs::scout_prompt(query);
    let fence = Fence {
        cwd: &cwd,
        user_home: env.user_home,
        loopx_home: env.loopx_home,
        exe: env.exe,
    };
    let argv = scout_argv(env.cli, &prompt, &fence)?;
    std::fs::create_dir_all(&cwd).map_err(|e| format!("cannot create {}: {e}", cwd.display()))?;
    jobs::start(jobs_dir, JobKind::Scout, query, &|log| {
        spawn(&argv, &cwd, log)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const HOME: &str = "/Users/u";
    const LOOPX_HOME: &str = "/sandbox/lx";
    const EXE: &str = "/opt/bin/loopx";

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
    fn assert_fenced(argv: &[String], skill: &str, tools: &str, budget: &str) {
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
        assert_has(
            &allow,
            &[format!("Read(/{HOME}/.claude/skills/{skill}/**)")],
        );
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
            must.push(format!("Write(/{HOME}/{p})"));
            must.push(format!("Edit(/{HOME}/{p})"));
        }
        // The job's own project settings would load on its next run.
        let cwd = flag(argv, "--add-dir");
        must.push(format!("Write(/{cwd}/.claude/**)"));
        // Nor its project memory or MCP config.
        for f in ["CLAUDE.md", "CLAUDE.local.md", ".mcp.json"] {
            must.push(format!("Write(/{cwd}/{f})"));
            must.push(format!("Edit(/{cwd}/{f})"));
        }
        must.push(format!("Edit(/{cwd}/.claude/**)"));
        must.push(format!("Read(/{LOOPX_HOME}/**)"));
        must.push(format!("Write(/{HOME}/.claude/**)"));
        must.push(format!("Edit(/{HOME}/.claude/**)"));
        assert_has(&deny, &must);
        // The skills themselves are read from ~/.claude/skills.
        for tree in [
            format!("Read(/{HOME}/.claude/**)"),
            format!("Read(/{HOME}/.claude/skills/**)"),
            format!("Read(/{HOME}/.claude/skills/{skill}/**)"),
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
        let argv = generate_argv("claude", "THE PROMPT", &fence(&cwd)).unwrap();
        assert_fenced(&argv, "inspire-video", "Bash,Read,Write,Edit", "5.00");
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
            &[
                format!("Write(/{c}/output/**)"),
                format!("Edit(/{c}/output/**)"),
            ],
        );
        for rule in &allow {
            if rule.starts_with("Write(") || rule.starts_with("Edit(") {
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
                format!("Write(/{c}/output/**/*.py)"),
                format!("Edit(/{c}/output/**/*.py)"),
                format!("Write(/{c}/output/**/*.js)"),
                format!("Write(/{c}/output/**/*.ts)"),
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
        let argv = scout_argv("claude", "THE PROMPT", &fence(&cwd)).unwrap();
        assert_fenced(
            &argv,
            "video-scout",
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
                format!("Write(/{c}/**)"),
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
            let e = generate_argv(cli, "p", &fence(&gd)).unwrap_err();
            assert!(e.contains("Claude"), "{cli}: {e}");
            let sd = scout_dir(home);
            assert!(scout_argv(cli, "p", &fence(&sd)).is_err());
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
        std::fs::create_dir_all(generate_dir(&home)).unwrap();
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let e = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            false,
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
        std::fs::create_dir_all(generate_dir(&home)).unwrap();
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let rec = start_generate_with(
            &jobs_dir,
            &env("claude", &home),
            "https://example.com/v",
            true,
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
            &jobs::generate_prompt("https://example.com/v", true)
        );
        assert!(denied(argv).contains(&format!("Bash({EXE}*)").as_str()));
        assert_eq!(cwd, &generate_dir(&home));
        assert_eq!(log, &rec.log_path);
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
        assert_eq!(calls[0].1, scout_dir(&home));
        assert_eq!(calls[0].0.last().unwrap(), &jobs::scout_prompt("stoicism"));
    }

    #[test]
    fn unsupported_cli_or_blank_input_creates_no_job() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(generate_dir(&home)).unwrap();
        let jobs_dir = d.path().join("jobs");
        let calls = Calls::default();
        let spawn = recorder(&calls);
        assert!(start_scout_with(&jobs_dir, &env("codex", &home), "q", &spawn).is_err());
        assert!(start_generate_with(
            &jobs_dir,
            &env("gemini", &home),
            "https://x/v",
            true,
            &spawn
        )
        .is_err());
        assert!(start_scout_with(&jobs_dir, &env("claude", &home), "  ", &spawn).is_err());
        assert!(start_generate_with(&jobs_dir, &env("claude", &home), " ", true, &spawn).is_err());
        assert!(calls.borrow().is_empty());
        assert!(jobs::list_jobs(&jobs_dir).is_empty());
    }

    #[test]
    fn start_generate_refuses_an_unsafe_url_before_any_job() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(generate_dir(&home)).unwrap();
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
}

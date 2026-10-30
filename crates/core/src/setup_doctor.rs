//! The setup doctor: the separate agent session that fixes environment
//! blockers (toolchains, gems, submodules, built assets, test DB) after the
//! main agent hands off `setup_blocked`. This module holds its permissions,
//! its result file and the texts built from it.

use crate::agents::{new_cli_args, RunOptions};
use crate::gitlab_runner::{ASSET_BUILD_PATTERNS, DISALLOWED_TOOLS};
use crate::runner_util::resolved_program;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn sibling(handoff: &Path, suffix: &str) -> PathBuf {
    let stem = handoff
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    handoff.with_file_name(format!("{stem}.{suffix}"))
}

/// Where the doctor writes its result: `<alias>-<iid>.doctor.json` beside the handoff.
pub fn doctor_result_path(handoff: &Path) -> PathBuf {
    sibling(handoff, "doctor.json")
}

/// Where the doctor writes its SETUP.md draft: `<alias>-<iid>.setup.md` beside the handoff.
pub fn setup_draft_path(handoff: &Path) -> PathBuf {
    sibling(handoff, "setup.md")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    Fixed,
    GaveUp,
    NeedsHuman,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub error: String,
    pub cause: String,
    pub tried: Vec<String>,
    pub fixed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorResult {
    pub status: DoctorStatus,
    pub blockers: Vec<Blocker>,
    pub learning: Option<String>,
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The doctor's result file, or `None` when missing, malformed or of an unknown status.
pub fn read_doctor_result(path: &Path) -> Option<DoctorResult> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let status = match v.get("status")?.as_str()? {
        "fixed" => DoctorStatus::Fixed,
        "gave_up" => DoctorStatus::GaveUp,
        "needs_human" => DoctorStatus::NeedsHuman,
        _ => return None,
    };
    let blockers = v
        .get("blockers")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|b| Blocker {
                    error: text(b, "error"),
                    cause: text(b, "cause"),
                    tried: b
                        .get("tried")
                        .and_then(Value::as_array)
                        .map(|t| {
                            t.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    fixed: b.get("result").and_then(Value::as_str) == Some("fixed"),
                })
                .collect()
        })
        .unwrap_or_default();
    let learning = Some(text(&v, "learning")).filter(|l| !l.trim().is_empty());
    Some(DoctorResult {
        status,
        blockers,
        learning,
    })
}

fn blocker_lines(blockers: &[Blocker]) -> String {
    blockers
        .iter()
        .map(|b| {
            let tried = if b.tried.is_empty() {
                "nothing".to_string()
            } else {
                b.tried.join("; ")
            };
            format!(
                "- `{}`: cause: {}; tried: {}; {}",
                b.error,
                b.cause,
                tried,
                if b.fixed { "fixed" } else { "failed" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The GitLab-comment body when the doctor could not clear the blockers.
pub fn doctor_report_markdown(blocked: &Value, result: Option<&DoctorResult>) -> String {
    let mut out = format!(
        "**Problem**\n\nVerifying the change failed on setup, not on the change itself. Command: `{}`\n\n```\n{}\n```\n\n**What the setup doctor tried**\n\n",
        text(blocked, "command"),
        text(blocked, "error").replace("```", "~~~"),
    );
    match result {
        None => {
            out.push_str("Note: the setup doctor did not finish, so nothing is known to be fixed.\n\n**Blocked on**\n\nThe environment problem above.\n\n**Next step**\n\nFix the setup by hand, then re-run the issue.");
        }
        Some(r) => {
            out.push_str(&blocker_lines(&r.blockers));
            let open: Vec<&Blocker> = r.blockers.iter().filter(|b| !b.fixed).collect();
            let blocked_on = if open.is_empty() {
                "the last error above".to_string()
            } else {
                open.iter()
                    .map(|b| b.cause.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            let next = match r.status {
                DoctorStatus::NeedsHuman => "A person needs to do what is described above, then re-run the issue.",
                _ => "Fix the setup by hand (the attempts above did not work), then re-run the issue.",
            };
            out.push_str(&format!(
                "\n\n**Blocked on**\n\n{blocked_on}\n\n**Next step**\n\n{next}"
            ));
        }
    }
    out
}

/// The feedback handed to the main agent when the doctor fixed the setup.
pub fn doctor_feedback(result: &DoctorResult) -> String {
    format!(
        "## Setup was fixed by the setup doctor\n\nThe environment blockers you reported were fixed:\n\n{}\n\nRun step 5's `loopx worktree new` again first (it reuses the existing worktree and branch), then continue from step 7 (Verify) on the `loop/issue-<iid>` branch, with the existing WIP commit.",
        blocker_lines(&result.blockers)
    )
}

/// The GitLab-comment body for a `setup_blocked` handoff the doctor never
/// saw; `why` says why it did not run.
pub fn report_without_doctor(blocked: &Value, why: &str) -> String {
    let diagnosis = text(blocked, "diagnosis");
    let diagnosis = if diagnosis.trim().is_empty() {
        String::new()
    } else {
        format!("**Diagnosis**\n\n{diagnosis}\n\n")
    };
    format!(
        "**Problem**\n\nVerifying the change failed on setup, not on the change itself. Command: `{}`\n\n```\n{}\n```\n\n{diagnosis}**Blocked on**\n\nThe environment problem above. The setup doctor did not run: {why}.\n\n**Next step**\n\nFix the setup by hand, then re-run the issue.",
        text(blocked, "command"),
        text(blocked, "error").replace("```", "~~~"),
    )
}

/// The GitLab-comment body when the main agent is blocked on setup again
/// after the doctor fixed `first`: the doctor runs once per issue per run.
pub fn repeat_block_report(first: &DoctorResult, second: &Value) -> String {
    format!(
        "The setup doctor fixed the first setup problem, but the checks were then blocked on setup again.\n\n{}\n\n**Fixed earlier in this run by the setup doctor**\n\n{}",
        report_without_doctor(second, "it already ran once for this issue in this run"),
        blocker_lines(&first.blockers)
    )
}

/// The doctor session's CLI argv, shaped like the main agent's
/// (`gitlab_runner::cli_command`) but writable only in the issue's worktree
/// and the handoffs directory. Claude gets no `acceptEdits`: that would
/// auto-approve edits in every added directory and undo the scoped
/// Edit/Write rules of [`doctor_allowed_tools`]. The other CLIs have no
/// allowlist, so there the prompt is the only fence, as for the main agent.
pub fn doctor_cli_command(
    ai_cli: &str,
    prompt: &str,
    repo_root: &str,
    work_dir: &str,
    exe: &str,
    max_budget_usd: Option<f64>,
) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let handoffs = format!("{}/outputs/handoffs", repo_root.trim_end_matches('/'));
    let roots = vec![s(work_dir), handoffs];
    if matches!(ai_cli, "gemini" | "kimi" | "cursor") {
        let mut opts = RunOptions::new(Path::new(repo_root), 0.0);
        opts.add_dirs = roots.iter().map(PathBuf::from).collect();
        let args = new_cli_args(ai_cli, prompt, &opts).expect("a new CLI");
        return std::iter::once(resolved_program(ai_cli))
            .chain(args)
            .collect();
    }
    if ai_cli == "codex" {
        // Network access: toolchain installs and web research need it.
        let roots = serde_json::to_string(&roots).expect("serialisable");
        return vec![
            s("codex"),
            s("exec"),
            s("--sandbox"),
            s("workspace-write"),
            s("-c"),
            s("approval_policy=never"),
            s("-c"),
            format!("sandbox_workspace_write.writable_roots={roots}"),
            s("-c"),
            s("sandbox_workspace_write.network_access=true"),
            s(prompt),
        ];
    }
    let mut cmd = vec![s("claude"), s("-p")];
    for root in &roots {
        cmd.extend([s("--add-dir"), root.clone()]);
    }
    cmd.extend([
        s("--allowedTools"),
        doctor_allowed_tools(repo_root, exe, work_dir),
        s("--disallowedTools"),
        doctor_disallowed_tools(exe),
    ]);
    cmd.extend(crate::cost::budget_args(max_budget_usd));
    cmd.extend([s("--output-format"), s("json"), s(prompt)]);
    cmd
}

/// Claude permission patterns for the doctor session: the worktree and
/// machine fixes of the spec, no pushes, commits or GitLab access.
pub fn doctor_allowed_tools(repo_root: &str, exe: &str, work_dir: &str) -> String {
    // Edit/Write are scoped to the worktree (`//` marks an absolute path).
    let mut t: Vec<String> = ["Read", "WebSearch", "WebFetch"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    t.extend(
        [
            "Bash(cd *)",
            "Bash(git status*)",
            "Bash(git diff*)",
            "Bash(git submodule update*)",
            "Bash(git submodule sync*)",
            "Bash(git submodule init*)",
            "Bash(git submodule status*)",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    t.extend(crate::verify_commands::claude_patterns());
    t.extend(ASSET_BUILD_PATTERNS.iter().map(|s| s.to_string()));
    t.extend(
        [
            "Bash(bin/rails dartsass:build*)",
            "Bash(bin/rails tailwindcss:build*)",
            "Bash(bin/setup*)",
            "Bash(RAILS_ENV=test bundle exec rake db:create*)",
            "Bash(RAILS_ENV=test bundle exec rake db:test:prepare*)",
            "Bash(RAILS_ENV=test bundle exec rake db:schema:load*)",
            "Bash(rbenv install*)",
            "Bash(nodenv install*)",
            "Bash(nvm install*)",
            "Bash(fnm install*)",
            "Bash(corepack enable*)",
            "Bash(gem install bundler*)",
            "Bash(brew install*)",
            "Bash(brew services start*)",
            "Bash(createuser*)",
            "Bash(createdb*)",
            "Bash(shasum*)",
            "Bash(loopx memory-store setup*)",
            "Bash(loopx memory-store list*)",
            "Bash(loopx events emit*)",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    t.extend(
        ["memory-store setup", "memory-store list", "events emit"]
            .iter()
            .map(|s| format!("Bash({exe} {s}*)")),
    );
    let abs = |p: &str| format!("/{}", p.trim_end_matches('/'));
    t.push(format!("Edit({}/**)", abs(work_dir)));
    t.push(format!("Write({}/**)", abs(work_dir)));
    t.push(format!("Write({}/outputs/handoffs/**)", abs(repo_root)));
    t.join(" ")
}

/// Everything the doctor must never do, on top of the agent's own denials.
pub fn doctor_disallowed_tools(exe: &str) -> String {
    let mut parts = vec![DISALLOWED_TOOLS.to_string()];
    for pat in [
        "sudo*",
        "git push*",
        "git commit*",
        "dropdb*",
        "brew uninstall*",
        "loopx gitlab*",
        "loopx notify*",
        "loopx mr*",
        "git submodule foreach*",
        "Edit(**/.git/**)",
        "Write(**/.git/**)",
        "Read(~/.ssh/**)",
        "Read(**/id_*)",
        "Read(~/.gitlab/**)",
        "Read(~/.slack/**)",
        "Read(~/.netrc)",
        "Read(~/.aws/**)",
    ] {
        if pat.contains('(') {
            parts.push(pat.to_string());
            continue;
        }
        parts.push(format!("Bash({pat})"));
        if let Some(rest) = pat.strip_prefix("loopx ") {
            parts.push(format!("Bash({exe} {rest})"));
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doctor_command(cli: &str) -> Vec<String> {
        doctor_cli_command(
            cli,
            "PROMPT",
            "/r",
            "/wt/web-issue-7",
            "/bin/loopx",
            Some(2.0),
        )
    }

    #[test]
    fn claude_doctor_command_is_scoped_to_the_worktree_and_handoffs() {
        let c = doctor_command("claude");
        assert_eq!(&c[..2], ["claude", "-p"]);
        assert!(
            !c.iter()
                .any(|a| a == "--permission-mode" || a == "acceptEdits"),
            "acceptEdits would auto-approve edits in every added dir: {c:?}"
        );
        let dirs: Vec<&str> = c
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(dirs, ["/wt/web-issue-7", "/r/outputs/handoffs"]);
        let flag = |name: &str| {
            let i = c.iter().position(|a| a == name).unwrap();
            c[i + 1].clone()
        };
        assert_eq!(
            flag("--allowedTools"),
            doctor_allowed_tools("/r", "/bin/loopx", "/wt/web-issue-7")
        );
        assert_eq!(
            flag("--disallowedTools"),
            doctor_disallowed_tools("/bin/loopx")
        );
        assert_eq!(flag("--max-budget-usd"), "2.00");
        assert_eq!(flag("--output-format"), "json");
        assert_eq!(c.last().unwrap(), "PROMPT");
    }

    #[test]
    fn codex_doctor_command_writes_only_the_worktree_and_handoffs() {
        let c = doctor_command("codex");
        assert_eq!(&c[..2], ["codex", "exec"]);
        assert!(c.contains(
            &r#"sandbox_workspace_write.writable_roots=["/wt/web-issue-7","/r/outputs/handoffs"]"#
                .to_string()
        ));
        assert_eq!(c.last().unwrap(), "PROMPT");
    }

    #[test]
    fn report_without_doctor_gives_the_diagnosis_and_why() {
        let blocked = json!({"command": "make test", "error": "No module named generated",
                             "diagnosis": "the generated module is missing"});
        let r = report_without_doctor(&blocked, "it is switched off for this loop");
        for part in [
            "`make test`",
            "No module named generated",
            "the generated module is missing",
            "it is switched off for this loop",
            "**Next step**",
        ] {
            assert!(r.contains(part), "{part} missing from {r}");
        }
        assert!(!r.contains("did not finish"), "{r}");
    }

    #[test]
    fn repeat_block_report_lists_both_blocks() {
        let first = DoctorResult {
            status: DoctorStatus::Fixed,
            blockers: vec![Blocker {
                error: "No module named generated".into(),
                cause: "generated module missing".into(),
                tried: vec!["make generate".into()],
                fixed: true,
            }],
            learning: None,
        };
        let second = json!({"command": "make test", "error": "database does not exist"});
        let r = repeat_block_report(&first, &second);
        for part in [
            "generated module missing",
            "make generate",
            "database does not exist",
        ] {
            assert!(r.contains(part), "{part} missing from {r}");
        }
    }

    fn write(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("a-1.doctor.json");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn result_paths_sit_next_to_the_handoff() {
        let h = Path::new("/r/outputs/handoffs/run1/orbit-10.json");
        assert_eq!(
            doctor_result_path(h),
            Path::new("/r/outputs/handoffs/run1/orbit-10.doctor.json")
        );
        assert_eq!(
            setup_draft_path(h),
            Path::new("/r/outputs/handoffs/run1/orbit-10.setup.md")
        );
    }

    #[test]
    fn reads_a_fixed_result() {
        let d = tempfile::tempdir().unwrap();
        let p = write(
            d.path(),
            r#"{"status":"fixed","blockers":[{"error":"e1","cause":"c1","tried":["t1","t2"],"result":"fixed"},{"error":"e2","cause":"c2","tried":[],"result":"failed"}],"learning":"rbenv install 4.0.7"}"#,
        );
        let r = read_doctor_result(&p).unwrap();
        assert_eq!(r.status, DoctorStatus::Fixed);
        assert_eq!(r.blockers.len(), 2);
        assert!(r.blockers[0].fixed);
        assert!(!r.blockers[1].fixed);
        assert_eq!(r.blockers[0].tried, vec!["t1", "t2"]);
        assert_eq!(r.learning.as_deref(), Some("rbenv install 4.0.7"));
    }

    #[test]
    fn rejects_unknown_status() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), r#"{"status":"shrug","blockers":[]}"#);
        assert!(read_doctor_result(&p).is_none());
    }

    #[test]
    fn missing_or_malformed_file_is_none() {
        let d = tempfile::tempdir().unwrap();
        assert!(read_doctor_result(&d.path().join("nope.json")).is_none());
        assert!(read_doctor_result(&write(d.path(), "not json")).is_none());
    }

    fn has(list: &str, pattern: &str) -> bool {
        format!(" {list} ").contains(&format!(" {pattern} "))
    }

    #[test]
    fn allowlist_allows_machine_fixes_and_denies_push_and_sudo() {
        let allowed = doctor_allowed_tools("/repo", "/bin/loopx", "/wt/orbit-issue-10");
        for want in [
            "Bash(rbenv install*)",
            "Bash(createuser*)",
            "WebSearch",
            "Bash(shasum*)",
            "Write(//repo/outputs/handoffs/**)",
            "Edit(//wt/orbit-issue-10/**)",
            "Write(//wt/orbit-issue-10/**)",
            "Bash(git submodule update*)",
            "Bash(loopx memory-store list*)",
        ] {
            assert!(has(&allowed, want), "{want} missing");
        }
        assert!(!allowed.contains("git push"));
        assert!(!allowed.contains("foreach"));
        assert!(!has(&allowed, "Bash(git submodule*)"));
        assert!(!has(&allowed, "Edit") && !has(&allowed, "Write"));
        assert!(!allowed.contains("memory get"));
        assert!(!allowed.contains("mr open"));
        let denied = doctor_disallowed_tools("/bin/loopx");
        for want in [
            "Bash(sudo*)",
            "Bash(git push*)",
            "Bash(git commit*)",
            "Bash(loopx gitlab*)",
            "Bash(/bin/loopx gitlab*)",
            "Bash(dropdb*)",
            "Bash(git submodule foreach*)",
            "Edit(**/.git/**)",
            "Write(**/.git/**)",
            "Read(~/.ssh/**)",
            "Read(**/id_*)",
            "Read(~/.gitlab/**)",
            "Read(~/.slack/**)",
            "Read(~/.netrc)",
            "Read(~/.aws/**)",
        ] {
            assert!(has(&denied, want), "{want} missing");
        }
    }

    fn blocked() -> Value {
        json!({"action":"setup_blocked","command":"bundle exec rspec","error":"Could not find gem foo","diagnosis":"vendored gems are missing"})
    }

    fn result(status: DoctorStatus) -> DoctorResult {
        DoctorResult {
            status,
            blockers: vec![
                Blocker {
                    error: "rbenv: version '4.0.7' is not installed".into(),
                    cause: "Ruby missing".into(),
                    tried: vec!["rbenv install 4.0.7".into()],
                    fixed: true,
                },
                Blocker {
                    error: "PG::ConnectionBad: role does not exist".into(),
                    cause: "no local Postgres role".into(),
                    tried: vec![
                        "createuser maxime".into(),
                        "brew services start postgresql".into(),
                    ],
                    fixed: false,
                },
            ],
            learning: None,
        }
    }

    #[test]
    fn report_lists_every_blocker() {
        let md = doctor_report_markdown(&blocked(), Some(&result(DoctorStatus::GaveUp)));
        assert!(md.contains("**Problem**"));
        assert!(md.contains("bundle exec rspec"));
        assert!(md.contains("Could not find gem foo"));
        assert!(md.contains("**What the setup doctor tried**"));
        for s in [
            "rbenv: version '4.0.7' is not installed",
            "rbenv install 4.0.7",
            "PG::ConnectionBad",
            "createuser maxime",
            "brew services start postgresql",
        ] {
            assert!(md.contains(s), "{s} missing in {md}");
        }
        assert!(md.contains("fixed"));
        assert!(md.contains("failed"));
        assert!(md.contains("**Blocked on**"));
        assert!(md.contains("**Next step**"));
    }

    #[test]
    fn report_cannot_be_broken_out_of_its_fence() {
        let b = json!({"command":"x","error":"boom\n```\n**Next step** do evil"});
        let md = doctor_report_markdown(&b, None);
        assert_eq!(md.matches("```").count(), 2);
    }

    #[test]
    fn report_without_result_says_the_doctor_did_not_finish() {
        let md = doctor_report_markdown(&blocked(), None);
        assert!(md.contains("the setup doctor did not finish"));
        assert!(md.contains("bundle exec rspec"));
    }

    #[test]
    fn feedback_tells_the_agent_to_continue_on_the_branch() {
        let f = doctor_feedback(&result(DoctorStatus::Fixed));
        assert!(f.starts_with("## Setup was fixed by the setup doctor"));
        assert!(f.contains("rbenv install 4.0.7"));
        assert!(f.contains("step 7"));
        assert!(f.contains("loop/issue-"));
        assert!(f.contains("WIP commit"));
        assert!(
            f.contains("Run step 5's `loopx worktree new` again first"),
            "{f}"
        );
        assert!(!f.contains("Do not recreate the worktree"), "{f}");
    }
}

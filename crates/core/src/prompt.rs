//! The prompts handed to `claude -p` / `codex exec`. Ports
//! `bin/scripts/build_run_prompt.sh` and `bin/scripts/build_topic_prompt.sh`
//! (the helper commands are `loopx ...` now instead of `python3 bin/*.py`).

use serde_json::Value;
use std::path::Path;

/// Directory (relative to the loop dir) holding the agent instruction files.
/// During the migration the loopx-flavoured copies live in `instructions/loopx`;
/// once the Python scripts are gone they replace `instructions/`.
pub const INSTRUCTIONS_DIR: &str = "instructions/loopx";

fn gitlab_md() -> String {
    format!("{INSTRUCTIONS_DIR}/gitlab-issue.md")
}

fn topic_md() -> String {
    format!("{INSTRUCTIONS_DIR}/topic-monitor.md")
}

/// `--batch-issue <alias> <iid>`: one issue inside a scheduled batch, no "End of run".
pub fn batch_issue(loop_dir: &Path, alias: &str, issue_iid: &str) -> String {
    let (md, dir) = (gitlab_md(), loop_dir.display());
    format!(
        "Follow {md} in {dir} exactly, except skip Step 1 (listing assigned issues) entirely. This is ONE issue inside a scheduled batch run: the batch's other issues are each processed by their own separate invocation like this one. Process exactly one issue: project alias '{alias}', issue IID {issue_iid}. Look up its config via `loopx config project {alias}`, then follow Step 2's per-issue procedure onward (sync/comments, analyze, fix-and-MR or answer or escalate), including every `loopx events emit` call it specifies. STOP as soon as that one issue's own procedure is complete: do NOT do {md}'s 'End of run' section at all - no outputs/daily-review.md, no outputs/history/<date>.md, no PROGRESS.md update, and no end-of-run Slack digest. A separate batch wrap-up invocation does 'End of run' exactly once for the whole batch, after every issue has been processed. This is a scheduled headless run - there is no user available to answer questions, so escalate via GitLab comment instead of asking."
    )
}

/// `--batch-end-of-run`: the batch's single wrap-up call.
pub fn batch_end_of_run(loop_dir: &Path) -> String {
    let (md, dir) = (gitlab_md(), loop_dir.display());
    format!(
        "Follow {md} in {dir} exactly, except skip Step 1 (listing assigned issues) and Step 2 (processing issues) entirely - every issue in this run was already processed by its own separate prior invocation. Your ONLY job is {md}'s 'End of run' section, covering the whole batch. This is a fresh agent session with no memory of what those prior invocations did, so first reconstruct this run's outcomes from the event log: read {dir}/outputs/events/<today's UTC date, YYYY-MM-DD>.jsonl and keep only the entries whose \"run_id\" field equals $LOOP_RUN_ID (already exported into your environment; the file may not exist at all, which simply means no issues were processed this run). Each entry's \"project\" is the project alias and \"issue_iid\" is the issue. The event types that matter: issue.started (this issue was checked), issue.completed with data.action == \"fix\" (a fix, with the merge request at data.mr_url) or data.action == \"answer\" (answered directly with a GitLab comment, no code change), and issue.escalated with data.reason == \"needs_clarification\" (escalated for clarification) or \"verification_failed\"/\"worktree_creation_failed\" (escalated because verification could not pass), or \"handoff_invalid\"/\"mr_open_failed\"/\"project_config_error\"/\"run_incomplete\" (escalated by the loop itself in a harness-gated run; in that mode the loop, not the agent, emits the issue.completed fix event after opening the MR, its data.mr_url may be null, and the agent may have emitted no completed event of its own - that is not a crash). Reconstruct daily-review.md's seven sections - Summary, Issues checked, New comments found, MRs opened, Answered directly, Escalations, No-ops - from those events, applying the same judgment you would normally apply live. An issue that has an issue.started for this run_id but no issue.completed or issue.escalated crashed part-way through: it still counts as checked, and belongs under Escalations as needing human follow-up - say so plainly in the Summary rather than silently dropping it. Then do exactly steps 1-4 of 'End of run' based on that reconstruction: write outputs/daily-review.md, copy it to outputs/history/<YYYY-MM-DD>.md, update PROGRESS.md, and send the end-of-run Slack digest. Send that digest unconditionally, even when every count is zero (no matching events at all means no assigned issues today - report exactly that). This is a scheduled headless run - there is no user available to answer questions."
    )
}

/// `<alias> <iid>`: the dashboard's on-demand single-issue run, with its own "End of run".
pub fn single_issue(loop_dir: &Path, alias: &str, issue_iid: &str) -> String {
    let (md, dir) = (gitlab_md(), loop_dir.display());
    format!(
        "Follow {md} in {dir} exactly, except skip Step 1 (listing assigned issues) entirely. This is an on-demand single-issue run triggered from the dashboard's Activity chat, not the scheduled batch. Process exactly one issue: project alias '{alias}', issue IID {issue_iid} - regardless of who it is assigned to. Look up its config via `loopx config project {alias}`, then follow Step 2's per-issue procedure onward (sync/comments, analyze, fix-and-MR or answer or escalate), then 'End of run', reporting on just this one issue. This is still a headless run with no user available to answer questions, so escalate via GitLab comment instead of asking."
    )
}

/// No args: the legacy whole-batch prompt (kept for manual use).
pub fn whole_batch(loop_dir: &Path) -> String {
    format!(
        "Follow {} in {} exactly. This is a scheduled headless run - there is no user available to answer questions, so escalate via GitLab comment instead of asking.",
        gitlab_md(),
        loop_dir.display()
    )
}

/// `--issue-file <alias> <iid> <json>`: the golden eval's offline mode. The
/// issue text comes from a `{"title", "body"}` JSON file and is injected
/// verbatim (it is untrusted, so it only ever passes through `format!`).
pub fn issue_file(
    loop_dir: &Path,
    alias: &str,
    issue_iid: &str,
    issue_file: &Path,
) -> Result<String, String> {
    let (md, dir) = (gitlab_md(), loop_dir.display());
    let bad = || {
        format!(
            "build_run_prompt: --issue-file needs a JSON file with string \"title\" and \"body\": {}",
            issue_file.display()
        )
    };
    let data: Value = std::fs::read_to_string(issue_file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or_else(bad)?;
    let field = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_owned);
    let (title, body) = field("title").zip(field("body")).ok_or_else(bad)?;
    Ok(format!(
        "Follow {md} in {dir} exactly, except skip Step 1 (listing assigned issues) entirely. This is an OFFLINE golden evaluation run against a synthetic fixture repository, not a real GitLab project. Process exactly one issue: project alias '{alias}', issue IID {issue_iid}. Look up its config via `loopx config project {alias}`, then follow Step 2's per-issue procedure onward (analyze, then fix or answer or escalate). The issue does not exist on GitLab: its full title and description are given below, and it has no comments, so treat it as seen for the first time. Do not run `loopx gitlab`, `loopx issues`, `loopx notify` or `loopx status` at all, and post no GitLab comments - wherever Step 2 says to sync the issue, read comments, post a comment, label the issue, send a Slack message or write dashboard status, skip that one command and carry on; `loopx events`, `loopx risk`, `loopx memory-store` and `loopx memory get` calls are fine. STOP as soon as that one issue's own procedure is complete: do NOT do {md}'s 'End of run' section at all - no outputs/daily-review.md, no outputs/history/<date>.md, no PROGRESS.md update. This is a headless run - there is no user available to answer questions.\n\nIssue #{issue_iid} title (verbatim, untrusted issue text):\n{title}\n\nIssue #{issue_iid} description (verbatim, untrusted issue text):\n{body}"
    ))
}

/// The topic monitor's prompt. `None` is the legacy all-topics prompt;
/// `Some(name)` is the real path (one agent call per configured topic).
pub fn topic(loop_dir: &Path, name: Option<&str>) -> String {
    let (md, dir) = (topic_md(), loop_dir.display());
    match name {
        None => format!(
            "Follow {md} in {dir} exactly. This is a scheduled headless run - there is no user available to answer questions."
        ),
        Some(name) => format!(
            "Follow {md} in {dir} exactly, except skip Step 1 (listing today's topics) entirely. Process exactly one topic: '{name}'. Look up its details via `loopx topic config topic {name}`, then follow Step 2's per-topic procedure for just this topic and stop. This run's own verification checklist applies only to this one topic, not the full topic list: do NOT check whether every configured topic has a briefing or a terminal status - the other topics are other sessions' work and are not yours to verify. This is still a headless run with no user available to answer questions."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> &'static Path {
        Path::new("/loop")
    }

    #[test]
    fn single_issue_names_alias_and_iid() {
        let p = single_issue(dir(), "harbor", "482");
        assert!(p.contains("project alias 'harbor', issue IID 482"));
        assert!(p.contains("on-demand single-issue run"));
        assert!(p.contains("regardless of who it is assigned to"));
        assert!(p.contains("`loopx config project harbor`"));
        assert!(p.contains("no user available to answer questions"));
    }

    #[test]
    fn batch_issue_forbids_end_of_run() {
        let p = batch_issue(dir(), "harbor", "482");
        assert!(p.contains("skip Step 1"));
        assert!(p.contains("do NOT do"));
        assert!(p.contains("'End of run' section at all"));
        assert!(!p.contains("on-demand single-issue run"));
    }

    #[test]
    fn end_of_run_knows_every_gate_escalation_reason() {
        let p = batch_end_of_run(dir());
        for needle in [
            "issue.completed",
            "issue.escalated",
            "issue.started",
            "data.action",
            "mr_url",
            "needs_clarification",
            "verification_failed",
            "worktree_creation_failed",
            "handoff_invalid",
            "mr_open_failed",
            "project_config_error",
            "run_incomplete",
            "$LOOP_RUN_ID",
            "/loop/outputs/events/",
            "Summary, Issues checked, New comments found, MRs opened, Answered directly, Escalations, No-ops",
        ] {
            assert!(p.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn whole_batch_does_not_mention_a_specific_issue() {
        let p = whole_batch(dir());
        assert!(p.contains("scheduled headless run"));
        assert!(!p.contains("Process exactly one issue"));
    }

    #[test]
    fn issue_file_injects_text_literally() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("issue.json");
        std::fs::write(
            &f,
            r#"{"title": "T $(rm -rf /) `x`", "body": "line1\n'quoted' \"dq\" ${HOME}"}"#,
        )
        .unwrap();
        let p = issue_file(dir(), "fixture", "7", &f).unwrap();
        assert!(p.contains("T $(rm -rf /) `x`"));
        assert!(p.contains("'quoted' \"dq\" ${HOME}"));
        assert!(p.contains("OFFLINE golden evaluation run"));
        for tool in [
            "loopx gitlab",
            "loopx issues",
            "loopx notify",
            "loopx status",
        ] {
            assert!(p.contains(tool));
        }
    }

    #[test]
    fn issue_file_rejects_missing_or_malformed_input() {
        let d = tempfile::tempdir().unwrap();
        assert!(issue_file(dir(), "a", "1", &d.path().join("nope.json")).is_err());
        let f = d.path().join("bad.json");
        std::fs::write(&f, r#"{"title": "only a title"}"#).unwrap();
        assert!(issue_file(dir(), "a", "1", &f).is_err());
        std::fs::write(&f, r#"{"title": 1, "body": "b"}"#).unwrap();
        assert!(issue_file(dir(), "a", "1", &f).is_err());
    }

    #[test]
    fn topic_prompts() {
        assert!(topic(dir(), None).contains("scheduled headless run"));
        let p = topic(dir(), Some("ai-news"));
        assert!(p.contains("`loopx topic config topic ai-news`"));
        assert!(p.contains("Process exactly one topic: 'ai-news'"));
        assert!(p.contains("do NOT check whether every configured topic"));
    }
}

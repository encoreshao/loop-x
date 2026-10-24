//! Open a GitLab merge request by pushing with push options. Mirrors
//! `bin/scripts/open_merge_request.sh`.

/// The loop may only ever push its own issue branches.
pub fn check_branch(branch: &str) -> Result<(), String> {
    if branch.starts_with("loop/issue-") {
        Ok(())
    } else {
        Err(format!("Refusing to push non-loop branch: {branch}"))
    }
}

/// The full `git` argv that pushes `branch` and asks GitLab to open an MR.
pub fn push_command(
    repo_path: &str,
    branch: &str,
    target_branch: &str,
    title: &str,
) -> Vec<String> {
    [
        "git",
        "-C",
        repo_path,
        "push",
        "origin",
        branch,
        "-o",
        "merge_request.create",
        "-o",
        &format!("merge_request.target={target_branch}"),
        "-o",
        &format!("merge_request.title={title}"),
        "-o",
        "merge_request.remove_source_branch",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loop_issue_branches_pass() {
        assert!(check_branch("loop/issue-5").is_ok());
        for bad in [
            "staging",
            "main",
            "loop/issues-1",
            "xloop/issue-1",
            "loop-issue-1",
            "",
        ] {
            assert!(check_branch(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn push_command_matches_script_argv() {
        let argv = push_command("/r", "loop/issue-5", "main", "My title");
        assert_eq!(
            argv[..6],
            ["git", "-C", "/r", "push", "origin", "loop/issue-5"]
        );
        assert_eq!(argv[9], "merge_request.target=main");
        assert_eq!(argv[11], "merge_request.title=My title");
        assert_eq!(argv.last().unwrap(), "merge_request.remove_source_branch");
    }
}

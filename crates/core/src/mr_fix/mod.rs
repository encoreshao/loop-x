pub mod agent;
pub mod eligibility;
pub mod feedback;
pub mod jobs;
pub mod notes;
pub mod real;
pub mod run;
pub mod slack;

/// The MCP servers Fix now attaches: the GitLab issue loop's `mcp_servers`, so
/// one list covers both (and the pipeline auto-fix, which runs Fix now jobs).
/// A missing or invalid definition attaches none.
pub fn declared_servers(repo_root: &std::path::Path) -> Vec<String> {
    crate::loop_definition::LoopDefinition::from_yaml(
        &repo_root.join(crate::issue_runner::DEFINITION_RELATIVE_PATH),
    )
    .map(|d| d.mcp_servers)
    .unwrap_or_default()
}

/// Whether `s` is a well-formed `account:project!iid` merge request reference.
pub fn valid_mr_ref(s: &str) -> bool {
    let Some((account, rest)) = s.split_once(':') else {
        return false;
    };
    let Some((project, iid)) = rest.split_once('!') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let name = |s: &str| {
        !s.is_empty()
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    // GitLab's numeric project id, or a GitHub `owner/name`.
    let project_ok = digits(project)
        || project
            .split_once('/')
            .is_some_and(|(o, n)| name(o) && name(n));
    // A leading `-` could be read as an option by the CLI.
    !s.starts_with('-') && name(account) && project_ok && digits(iid)
}

#[cfg(test)]
mod tests {
    use super::{declared_servers, valid_mr_ref};

    #[test]
    fn fix_now_reads_servers_from_the_issue_loop_definition() {
        let d = tempfile::tempdir().unwrap();
        assert!(declared_servers(d.path()).is_empty());
        let file = d.path().join(crate::issue_runner::DEFINITION_RELATIVE_PATH);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let base = "name: x\nversion: 1\ntrigger: {type: manual}\ngoal: {type: issue_resolution}\n";
        std::fs::write(&file, base).unwrap();
        assert!(declared_servers(d.path()).is_empty());
        std::fs::write(&file, format!("{base}mcp_servers: [playwright]\n")).unwrap();
        assert_eq!(declared_servers(d.path()), vec!["playwright"]);
        std::fs::write(&file, format!("{base}mcp_servers: [Bad_Name]\n")).unwrap();
        assert!(declared_servers(d.path()).is_empty());
    }

    #[test]
    fn mr_refs_must_be_account_project_iid() {
        for ok in ["work:12!3", "gh:owner/name!4", "my.acct_1-x:7!100"] {
            assert!(valid_mr_ref(ok), "{ok}");
        }
        for bad in [
            "",
            "-x:1!2",
            "a:b!c",
            "..:1!2",
            "a:1!",
            "a:1",
            ":1!2",
            "a:o/n/m!1",
            "a:o/..!1",
            "a:1!2 --x",
            "a b:1!2",
            "a:1!-2",
        ] {
            assert!(!valid_mr_ref(bad), "{bad}");
        }
    }
}

pub mod agent;
pub mod eligibility;
pub mod feedback;
pub mod jobs;
pub mod notes;
pub mod real;
pub mod run;

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
    use super::valid_mr_ref;

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

//! Loop branch names: `<type>#<n>[-<slug>]` (`feature#1412-org-website-liveness`,
//! `bug#1303-dashboard-scrollbar`), type from the issue's labels, slug from its
//! title. Built once when the branch is created, so later title or label edits
//! never rename a pushed branch: everything else finds an issue's branch by its
//! number ([`issue_of_branch`]), never by rebuilding the name. Legacy
//! `loop/issue-<n>` branches (before types and slugs) are still loop branches.

use super::Issue;

pub const TYPES: [&str; 2] = ["feature", "bug"];
const LEGACY_PREFIX: &str = "loop/issue-";
const SLUG_MAX: usize = 40;

/// Labels (case-insensitive, whole label) that make an issue a `bug`.
const BUG_LABELS: [&str; 5] = ["bug", "defect", "regression", "hotfix", "type::bug"];

pub fn branch_type(labels: &[String]) -> &'static str {
    let bug = labels
        .iter()
        .any(|l| BUG_LABELS.iter().any(|b| l.trim().eq_ignore_ascii_case(b)));
    if bug {
        "bug"
    } else {
        "feature"
    }
}

/// ASCII alphanumeric runs of the title, lowercased, joined by `-`, cut at a
/// word boundary to at most 40 characters (a single longer word is hard-cut).
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let extra = if out.is_empty() {
            word.len()
        } else {
            word.len() + 1
        };
        if out.len() + extra > SLUG_MAX {
            if out.is_empty() {
                out = word[..SLUG_MAX].to_string();
            }
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word);
    }
    out
}

/// The name a new branch for issue `n` gets.
pub fn new_branch_name(n: u64, title: &str, labels: &[String]) -> String {
    let base = format!("{}#{n}", branch_type(labels));
    match slug(title) {
        s if s.is_empty() => base,
        s => format!("{base}-{s}"),
    }
}

pub fn branch_for(issue: &Issue) -> String {
    new_branch_name(issue.number, &issue.title, &issue.labels)
}

/// The pre-slug `loop/issue-<n>` name, still recognised for older branches.
pub fn legacy_branch_name(n: u64) -> String {
    format!("{LEGACY_PREFIX}{n}")
}

/// The issue number of a loop branch, if `name` is one.
pub fn issue_of_branch(name: &str) -> Option<u64> {
    let (rest, slug_allowed) = if let Some(rest) = name.strip_prefix(LEGACY_PREFIX) {
        (rest, false)
    } else {
        let (kind, rest) = name.split_once('#')?;
        if !TYPES.contains(&kind) {
            return None;
        }
        (rest, true)
    };
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == 0 || rest.starts_with('0') {
        return None;
    }
    let tail = &rest[digits_end..];
    let ok = tail.is_empty()
        || (slug_allowed
            && tail.len() > 1
            && tail.starts_with('-')
            && tail[1..]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    if !ok {
        return None;
    }
    rest[..digits_end].parse().ok()
}

pub fn is_loop_branch(name: &str, n: u64) -> bool {
    issue_of_branch(name) == Some(n)
}

pub fn is_any_loop_branch(name: &str) -> bool {
    issue_of_branch(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Issue, IssueState};
    use serde_json::Value;

    fn issue(n: u64, title: &str, labels: &[&str]) -> Issue {
        Issue {
            number: n,
            repo: "o/r".into(),
            title: title.into(),
            body: String::new(),
            state: IssueState::Open,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            assignees: vec![],
            author: String::new(),
            web_url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            raw: Value::Null,
        }
    }

    #[test]
    fn type_is_bug_for_a_bug_label_and_feature_otherwise() {
        let t = |l: &[&str]| branch_type(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(t(&[]), "feature");
        assert_eq!(t(&["Bug"]), "bug");
        assert_eq!(t(&["HOTFIX"]), "bug");
        assert_eq!(t(&["frontend", "regression"]), "bug");
        assert_eq!(t(&["type::bug"]), "bug");
        assert_eq!(t(&["documentation", "chore"]), "feature");
        // only whole labels count
        assert_eq!(t(&["debugging"]), "feature");
    }

    #[test]
    fn slug_is_ascii_words_cut_at_a_word_boundary() {
        assert_eq!(
            slug("Add dark theme to settings page"),
            "add-dark-theme-to-settings-page"
        );
        assert_eq!(
            slug("  Fix: login (button) crash!! "),
            "fix-login-button-crash"
        );
        assert_eq!(
            slug("Make the extremely long title that keeps going well beyond forty characters"),
            "make-the-extremely-long-title-that-keeps"
        );
        assert!(slug("Make the extremely long title that keeps going").len() <= 40);
        assert_eq!(slug("修复登录按钮"), "");
        assert_eq!(slug("!!! ---"), "");
        assert_eq!(slug("Ünïcode café fix"), "n-code-caf-fix");
        // a single word longer than 40 is hard-cut, never empty
        assert_eq!(slug(&"a".repeat(50)).len(), 40);
    }

    #[test]
    fn branch_for_joins_type_number_and_slug() {
        assert_eq!(
            branch_for(&issue(1412, "Org website liveness", &[])),
            "feature#1412-org-website-liveness"
        );
        assert_eq!(
            branch_for(&issue(1303, "Dashboard scrollbar", &["bug"])),
            "bug#1303-dashboard-scrollbar"
        );
        assert_eq!(branch_for(&issue(42, "修复登录按钮", &[])), "feature#42");
    }

    #[test]
    fn new_names_are_valid_git_branch_names() {
        for name in [
            new_branch_name(1412, "Org website liveness", &[]),
            new_branch_name(7, "", &["bug".into()]),
        ] {
            let ok = std::process::Command::new("git")
                .args(["check-ref-format", "--branch", &name])
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "{name}");
        }
    }

    #[test]
    fn loop_branches_are_recognised_with_and_without_slug_and_legacy() {
        for name in [
            "feature#42",
            "bug#42-login",
            "feature#42-org-website-liveness",
            "loop/issue-42",
        ] {
            assert!(is_loop_branch(name, 42), "{name}");
            assert_eq!(issue_of_branch(name), Some(42), "{name}");
        }
        for name in [
            "feature#420",
            "feature#4",
            "feat#42",
            "fix#42",
            "feature/issue-42",
            "feature#42x",
            "feature#42-",
            "feature#42-Upper",
            "feature#042",
            "loop/issue-42-slug",
            "feature#",
            "main",
        ] {
            assert!(!is_loop_branch(name, 42), "{name}");
        }
        assert_eq!(issue_of_branch("bug#420-x"), Some(420));
        assert_eq!(issue_of_branch("loop/issue-42-x"), None);
        assert_eq!(issue_of_branch("feature#x"), None);
        assert!(is_any_loop_branch("bug#1-x") && !is_any_loop_branch("loop/mr-1"));
    }
}

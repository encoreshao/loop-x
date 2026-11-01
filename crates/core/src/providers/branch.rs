//! Loop branch names: `<type>/issue-<n>[-<slug>]`, type from the issue's
//! labels, slug from its title. Built once when the worktree is created and
//! stored, so later title or label edits never rename a pushed branch. Legacy
//! `loop/issue-<n>` branches (before types and slugs) are still loop branches.

use super::Issue;

pub const PREFIXES: [&str; 4] = ["feat", "fix", "docs", "chore"];
pub const LEGACY_PREFIX: &str = "loop";
const SLUG_MAX: usize = 40;

/// First rule that matches any label (case-insensitive, whole label) wins.
const RULES: [(&str, &[&str]); 3] = [
    ("fix", &["bug", "defect", "regression", "hotfix"]),
    ("docs", &["docs", "documentation"]),
    ("chore", &["chore", "maintenance", "dependencies", "ci"]),
];

pub fn branch_type(labels: &[String]) -> &'static str {
    for (kind, names) in RULES {
        if labels
            .iter()
            .any(|l| names.iter().any(|n| l.trim().eq_ignore_ascii_case(n)))
        {
            return kind;
        }
    }
    "feat"
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

pub fn branch_for(issue: &Issue) -> String {
    let base = format!("{}/issue-{}", branch_type(&issue.labels), issue.number);
    match slug(&issue.title) {
        s if s.is_empty() => base,
        s => format!("{base}-{s}"),
    }
}

/// The issue number of a loop branch, if `name` is one.
pub fn issue_of_branch(name: &str) -> Option<u64> {
    let (prefix, rest) = name.split_once('/')?;
    let rest = rest.strip_prefix("issue-")?;
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    let tail = &rest[digits_end..];
    let ok = if prefix == LEGACY_PREFIX {
        tail.is_empty()
    } else if PREFIXES.contains(&prefix) {
        tail.is_empty() || (tail.starts_with('-') && tail.len() > 1)
    } else {
        false
    };
    if !ok {
        return None;
    }
    rest[..digits_end].parse().ok()
}

pub fn is_loop_branch(name: &str, n: u64) -> bool {
    issue_of_branch(name) == Some(n)
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
    fn type_comes_from_labels_in_priority_order() {
        let t = |l: &[&str]| branch_type(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(t(&[]), "feat");
        assert_eq!(t(&["Bug"]), "fix");
        assert_eq!(t(&["HOTFIX"]), "fix");
        assert_eq!(t(&["documentation"]), "docs");
        assert_eq!(t(&["dependencies"]), "chore");
        assert_eq!(t(&["ci"]), "chore");
        // fix wins over docs and chore whatever the label order.
        assert_eq!(t(&["docs", "chore", "regression"]), "fix");
        assert_eq!(t(&["chore", "docs"]), "docs");
        // only whole labels count
        assert_eq!(t(&["debugging"]), "feat");
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
            branch_for(&issue(42, "Add dark theme to settings page", &[])),
            "feat/issue-42-add-dark-theme-to-settings-page"
        );
        assert_eq!(
            branch_for(&issue(57, "Login button crash", &["bug"])),
            "fix/issue-57-login-button-crash"
        );
        assert_eq!(branch_for(&issue(42, "修复登录按钮", &[])), "feat/issue-42");
    }

    #[test]
    fn loop_branches_are_recognised_with_and_without_slug_and_legacy() {
        for name in [
            "feat/issue-42",
            "fix/issue-42-login",
            "docs/issue-42-x",
            "chore/issue-42",
            "loop/issue-42",
        ] {
            assert!(is_loop_branch(name, 42), "{name}");
            assert_eq!(issue_of_branch(name), Some(42), "{name}");
        }
        for name in [
            "feat/issue-420",
            "feat/issue-4",
            "feature/issue-42",
            "feat/issue-42x",
            "loop/issue-42-slug",
            "feat/issue-",
            "main",
        ] {
            assert!(!is_loop_branch(name, 42), "{name}");
        }
        assert_eq!(issue_of_branch("feat/issue-420-x"), Some(420));
        assert_eq!(issue_of_branch("loop/issue-42-x"), None);
        assert_eq!(issue_of_branch("feat/issue-x"), None);
    }
}

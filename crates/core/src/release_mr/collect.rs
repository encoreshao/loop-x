//! What changed between two branches, as the agent will read it.

use super::api::{MrInfo, ReleaseApi};
use super::ReleaseError;

/// More commits than this are too many for one release MR.
pub const MAX_COMMITS: usize = 500;
/// At most this many merge-request lookups per run; later commits keep their own author.
pub const MAX_MR_LOOKUPS: usize = 100;

/// One unit of change: a merged MR (its title and author) or a lone commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub title: String,
    pub author_name: String,
    pub author_email: String,
    pub mr: Option<MrInfo>,
}

fn is_merge_noise(title: &str) -> bool {
    title.starts_with("Merge branch '")
        || title.starts_with("Merge remote-tracking branch")
        || title.starts_with("Merge pull request")
}

pub fn collect(
    api: &dyn ReleaseApi,
    project: &str,
    from: &str,
    to: &str,
) -> Result<Vec<Change>, ReleaseError> {
    let commits = api.compare(project, from, to)?;
    if commits.is_empty() {
        return Err(ReleaseError::NothingToRelease);
    }
    if commits.len() > MAX_COMMITS {
        return Err(ReleaseError::TooLarge(commits.len()));
    }
    let mut changes: Vec<Change> = Vec::new();
    let mut lookups = 0;
    for c in &commits {
        let mr = if lookups < MAX_MR_LOOKUPS {
            lookups += 1;
            api.commit_mr(project, &c.sha, to)?
        } else {
            None
        };
        let change = match mr {
            Some(m) => Change {
                title: m.title.clone(),
                author_name: c.author_name.clone(),
                author_email: c.author_email.clone(),
                mr: Some(m),
            },
            // A bare merge commit says nothing about the work: its MR (if found) carries it.
            None if is_merge_noise(&c.title) => continue,
            None => Change {
                title: c.title.clone(),
                author_name: c.author_name.clone(),
                author_email: c.author_email.clone(),
                mr: None,
            },
        };
        // Many commits of one MR are one change.
        let same = |o: &Change| match (&o.mr, &change.mr) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        if !changes.iter().any(same) {
            changes.push(change);
        }
    }
    if changes.is_empty() {
        return Err(ReleaseError::NothingToRelease);
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_mr::api::{CommitInfo, FakeApi};

    fn commit(sha: &str, title: &str, author: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.into(),
            title: title.into(),
            author_name: author.into(),
            author_email: format!("{author}@x.io"),
        }
    }

    #[test]
    fn empty_compare_is_nothing_to_release() {
        let api = FakeApi::default();
        assert_eq!(
            collect(&api, "g/app", "master", "staging"),
            Err(ReleaseError::NothingToRelease)
        );
    }

    #[test]
    fn over_500_commits_is_too_large() {
        let api = FakeApi {
            commits: (0..501)
                .map(|i| commit(&format!("s{i}"), "x", "ann"))
                .collect(),
            ..FakeApi::default()
        };
        assert_eq!(
            collect(&api, "g/app", "master", "staging"),
            Err(ReleaseError::TooLarge(501))
        );
    }

    #[test]
    fn commits_of_one_merge_request_collapse_into_one_change_with_the_mr_title() {
        let mut api = FakeApi {
            commits: vec![commit("a", "wip", "ann"), commit("b", "fix typo", "ann")],
            ..FakeApi::default()
        };
        let mr = MrInfo {
            iid: 1351,
            title: "Export orders".into(),
            author_username: "ann".into(),
        };
        api.commit_mrs.insert("a".into(), mr.clone());
        api.commit_mrs.insert("b".into(), mr);
        let got = collect(&api, "g/app", "master", "staging").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "Export orders");
    }

    #[test]
    fn bare_merge_commits_without_an_mr_are_dropped() {
        let api = FakeApi {
            commits: vec![
                commit("a", "Merge branch 'x' into 'staging'", "ann"),
                commit("b", "Add search", "bo"),
            ],
            ..FakeApi::default()
        };
        let got = collect(&api, "g/app", "master", "staging").unwrap();
        assert_eq!(
            got.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Add search"]
        );
    }

    #[test]
    fn only_merge_noise_is_nothing_to_release() {
        let api = FakeApi {
            commits: vec![commit("a", "Merge branch 'x' into 'staging'", "ann")],
            ..FakeApi::default()
        };
        assert_eq!(
            collect(&api, "g/app", "master", "staging"),
            Err(ReleaseError::NothingToRelease)
        );
    }

    #[test]
    fn merge_request_lookups_stop_at_100_calls() {
        let api = FakeApi {
            commits: (0..150)
                .map(|i| commit(&format!("s{i}"), &format!("change {i}"), "ann"))
                .collect(),
            ..FakeApi::default()
        };
        let got = collect(&api, "g/app", "master", "staging").unwrap();
        assert_eq!(api.commit_mr_calls.get(), MAX_MR_LOOKUPS);
        assert_eq!(
            got.len(),
            150,
            "commits past the cap still count, as plain commits"
        );
    }
}

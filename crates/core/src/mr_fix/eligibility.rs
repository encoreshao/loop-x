//! Which MRs the "Fix now" action may touch, and where their checkout lives.

use crate::config::Config;
use crate::loop_plugins::mr_review::MyMr;
use serde_json::Value;

/// Why an MR cannot be fixed in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    /// The source branch lives in another project: we cannot push to it.
    Fork,
    /// No `projects.json` entry matches the MR's project.
    ProjectNotConfigured,
    NotGitlab,
    Draft,
}

impl Ineligible {
    /// English text, for `ctx.t`.
    pub fn reason(&self) -> &'static str {
        match self {
            Ineligible::Fork => {
                "This merge request comes from a fork, so its branch cannot be pushed to."
            }
            Ineligible::ProjectNotConfigured => {
                "This merge request's project is not configured in Projects."
            }
            Ineligible::NotGitlab => "Fix now only works on GitLab merge requests.",
            Ineligible::Draft => "This merge request is a draft.",
        }
    }
}

/// The configured project an eligible MR belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixTarget {
    pub alias: String,
    pub local_path: String,
    pub worktree_root: String,
}

/// Matches the MR's project to a `projects.json` entry by its stored
/// `project_id` (the numeric GitLab id or the path, as the MR row reports it),
/// falling back to the alias itself.
pub fn resolve(mr: &MyMr, account_kind: &str, config: &Config) -> Result<FixTarget, Ineligible> {
    if account_kind != "gitlab" {
        return Err(Ineligible::NotGitlab);
    }
    if mr.draft {
        return Err(Ineligible::Draft);
    }
    // An unknown source project (the listing did not report one) may be a
    // fork: refused rather than assumed safe. GitLab rows always carry it.
    if mr.source_project_id.is_empty() || mr.source_project_id != mr.project_id {
        return Err(Ineligible::Fork);
    }
    // Stored ids may be a path (`group/project`) while the listing carries the numeric id.
    let ref_path = mr.reference.rsplit_once('!').map_or("", |(path, _)| path);
    project_for(&mr.project_id, ref_path, config)
}

/// The configured project whose stored `project_id` (or alias) is `project_id`
/// or `ref_path`, with its checkout and the worktree root.
pub fn project_for(
    project_id: &str,
    ref_path: &str,
    config: &Config,
) -> Result<FixTarget, Ineligible> {
    let aliases = config.aliases().unwrap_or_default();
    let text = |p: &Value, k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
    for alias in aliases {
        let Ok(p) = config.project(&alias) else {
            continue;
        };
        let stored = text(&p, "project_id").unwrap_or_else(|| alias.clone());
        if stored != project_id && stored != ref_path {
            continue;
        }
        let Some(local_path) = text(&p, "local_path").filter(|l| !l.is_empty()) else {
            continue;
        };
        let worktree_root = config
            .worktree_root()
            .ok()
            .filter(|w| !w.is_empty())
            .ok_or(Ineligible::ProjectNotConfigured)?;
        return Ok(FixTarget {
            alias,
            local_path,
            worktree_root,
        });
    }
    Err(Ineligible::ProjectNotConfigured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(dir: &Path) -> Config {
        let p = dir.join("projects.json");
        std::fs::write(
            &p,
            r#"{"gitlab_instance":"g.example.com","worktree_root":"/wt","projects":{
                "web":{"local_path":"/code/web","project_id":"9"}}}"#,
        )
        .unwrap();
        Config::load(&p).unwrap()
    }

    fn mr(f: impl FnOnce(&mut MyMr)) -> MyMr {
        let mut m = MyMr {
            project_id: "9".into(),
            source_project_id: "9".into(),
            source_branch: "feat/x".into(),
            iid: "1".into(),
            ..MyMr::default()
        };
        f(&mut m);
        m
    }

    #[test]
    fn resolve_accepts_a_configured_same_project_mr() {
        let d = tempfile::tempdir().unwrap();
        let t = resolve(&mr(|_| {}), "gitlab", &config(d.path())).unwrap();
        assert_eq!(
            (
                t.alias.as_str(),
                t.local_path.as_str(),
                t.worktree_root.as_str()
            ),
            ("web", "/code/web", "/wt")
        );
    }

    #[test]
    fn project_for_matches_by_numeric_id_or_path() {
        let d = tempfile::tempdir().unwrap();
        let c = config(d.path());
        assert_eq!(project_for("9", "", &c).unwrap().alias, "web");
        assert_eq!(project_for("77", "9", &c).unwrap().alias, "web");
        assert_eq!(
            project_for("5", "g/other", &c).unwrap_err(),
            Ineligible::ProjectNotConfigured
        );
    }

    #[test]
    fn resolve_refuses_a_fork_mr() {
        let d = tempfile::tempdir().unwrap();
        let m = mr(|m| m.source_project_id = "12".into());
        assert_eq!(
            resolve(&m, "gitlab", &config(d.path())).unwrap_err(),
            Ineligible::Fork
        );
    }

    #[test]
    fn resolve_refuses_an_mr_whose_source_project_is_unknown() {
        let d = tempfile::tempdir().unwrap();
        let m = mr(|m| m.source_project_id = String::new());
        assert_eq!(
            resolve(&m, "gitlab", &config(d.path())).unwrap_err(),
            Ineligible::Fork
        );
    }

    #[test]
    fn resolve_refuses_unconfigured_project() {
        let d = tempfile::tempdir().unwrap();
        let m = mr(|m| {
            m.project_id = "77".into();
            m.source_project_id = "77".into();
        });
        assert_eq!(
            resolve(&m, "gitlab", &config(d.path())).unwrap_err(),
            Ineligible::ProjectNotConfigured
        );
    }

    #[test]
    fn resolve_refuses_a_draft() {
        let d = tempfile::tempdir().unwrap();
        let m = mr(|m| m.draft = true);
        assert_eq!(
            resolve(&m, "gitlab", &config(d.path())).unwrap_err(),
            Ineligible::Draft
        );
    }

    #[test]
    fn resolve_refuses_a_non_gitlab_account() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve(&mr(|_| {}), "github", &config(d.path())).unwrap_err(),
            Ineligible::NotGitlab
        );
    }

    #[test]
    fn resolve_matches_a_path_style_project_id_through_the_reference() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            r#"{"worktree_root":"/wt","projects":{
                "web":{"local_path":"/code/web","project_id":"grp/sub/web"}}}"#,
        )
        .unwrap();
        let m = mr(|m| m.reference = "grp/sub/web!1".into());
        let t = resolve(&m, "gitlab", &Config::load(&p).unwrap()).unwrap();
        assert_eq!(t.alias, "web");
    }

    #[test]
    fn resolve_refuses_when_no_worktree_root_is_configured() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("projects.json");
        std::fs::write(
            &p,
            r#"{"projects":{"web":{"local_path":"/code/web","project_id":"9"}}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve(&mr(|_| {}), "gitlab", &Config::load(&p).unwrap()).unwrap_err(),
            Ineligible::ProjectNotConfigured
        );
    }
}

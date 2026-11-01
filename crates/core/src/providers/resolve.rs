//! Pick the provider implementation for a binding from the connector
//! account's `kind`. A kind with no implementation is `Unsupported`, which
//! is where a Jira or Linear tracker plugs in later.

use super::binding::Binding;
use super::github::{GitHub, GitHubAccount};
use super::gitlab::{GitLab, GitLabAccount};
use super::{AccountHost, CodeHost, IssueTracker, PResult, ProviderError};
use crate::connectors::{Connector, ConnectorSource, LoadFailure};

pub struct Resolved {
    pub tracker: Box<dyn IssueTracker>,
    pub code: Box<dyn CodeHost>,
}

fn load(src: &dyn ConnectorSource, id: &str) -> PResult<Box<dyn Connector>> {
    src.load(id).map_err(|f| match f {
        LoadFailure::Unknown => ProviderError::UnknownAccount(id.to_string()),
        LoadFailure::Config(_) => ProviderError::Load("ConnectorConfigError".into()),
        LoadFailure::Other(class) => ProviderError::Load(class),
    })
}

fn unsupported(kind: String, role: &'static str) -> ProviderError {
    ProviderError::Unsupported { kind, role }
}

pub fn resolve(src: &dyn ConnectorSource, b: &Binding) -> PResult<Resolved> {
    let conn = load(src, &b.code.account)?;
    // The kind is cloned first: the match arms move `conn`.
    let kind = conn.account().kind.clone();
    let code: Box<dyn CodeHost> = match kind.as_str() {
        "gitlab" => Box::new(GitLab::new(conn, &b.code.path)),
        "github" => Box::new(GitHub::new(conn, &b.code.path)?),
        _ => return Err(unsupported(kind, "code hosting")),
    };
    let conn = load(src, &b.tracker.account)?;
    let kind = conn.account().kind.clone();
    let tracker: Box<dyn IssueTracker> = match kind.as_str() {
        "gitlab" => Box::new(GitLab::new(conn, &b.tracker.path)),
        "github" => Box::new(GitHub::new(conn, &b.tracker.path)?),
        _ => return Err(unsupported(kind, "issues")),
    };
    Ok(Resolved { tracker, code })
}

pub fn account_host(src: &dyn ConnectorSource, account_id: &str) -> PResult<Box<dyn AccountHost>> {
    let conn = load(src, account_id)?;
    let kind = conn.account().kind.clone();
    match kind.as_str() {
        "gitlab" => Ok(Box::new(GitLabAccount::new(conn))),
        "github" => Ok(Box::new(GitHubAccount::new(conn))),
        _ => Err(unsupported(kind, "code hosting")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::testutil::FakeHttp;
    use crate::connectors::{
        github::GitHubConnector, gitlab::GitLabConnector, Account, Connector, LoadFailure,
    };
    use crate::providers::binding::{Binding, Endpoint};
    use crate::providers::Provider;
    use serde_json::json;

    struct Src;
    impl ConnectorSource for Src {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            vec![]
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            let http = FakeHttp::ok(json!({}));
            match id {
                "gh" => Ok(Box::new(GitHubConnector::new(
                    Account::new("gh", "github").with_setting("api_url", "https://api.github.com"),
                    None,
                    http,
                ))),
                "gl" => Ok(Box::new(GitLabConnector::new(
                    Account::new("gl", "gitlab").with_setting("url", "https://gl"),
                    None,
                    http,
                ))),
                "slack" => Ok(Box::new(GitHubConnector::new(
                    Account::new("slack", "slack"),
                    None,
                    http,
                ))),
                "broken" => Err(LoadFailure::Other("KeyError".into())),
                _ => Err(LoadFailure::Unknown),
            }
        }
    }

    fn b(code: (&str, &str), tracker: (&str, &str)) -> Binding {
        Binding {
            code: Endpoint {
                account: code.0.into(),
                path: code.1.into(),
            },
            tracker: Endpoint {
                account: tracker.0.into(),
                path: tracker.1.into(),
            },
        }
    }

    #[test]
    fn picks_the_implementation_by_kind() {
        let r = resolve(&Src, &b(("gh", "o/r"), ("gl", "g/p"))).unwrap();
        assert_eq!(r.code.provider(), Provider::Github);
        assert_eq!(r.tracker.provider(), Provider::Gitlab);
        assert_eq!(r.tracker.issue_url(1), "https://gl/g/p/-/issues/1");
    }

    #[test]
    fn errors_name_the_problem_without_detail() {
        let err = |x: PResult<Resolved>| x.err().unwrap();
        assert!(
            matches!(err(resolve(&Src, &b(("slack", "x"), ("gh", "o/r")))),
            ProviderError::Unsupported { ref kind, role: "code hosting" } if kind == "slack")
        );
        assert!(matches!(
            err(resolve(&Src, &b(("gh", "o/r"), ("slack", "x")))),
            ProviderError::Unsupported { role: "issues", .. }
        ));
        assert!(
            matches!(err(resolve(&Src, &b(("nope", "o/r"), ("gh", "o/r")))),
            ProviderError::UnknownAccount(ref id) if id == "nope")
        );
        assert!(
            matches!(err(resolve(&Src, &b(("broken", "o/r"), ("gh", "o/r")))),
            ProviderError::Load(ref c) if c == "KeyError")
        );
        assert!(matches!(
            err(resolve(&Src, &b(("gh", "not a repo"), ("gh", "o/r")))),
            ProviderError::Malformed(_)
        ));
    }

    #[test]
    fn account_hosts() {
        assert_eq!(
            account_host(&Src, "gh").unwrap().provider(),
            Provider::Github
        );
        assert_eq!(
            account_host(&Src, "gl").unwrap().provider(),
            Provider::Gitlab
        );
        assert!(matches!(
            account_host(&Src, "slack").err().unwrap(),
            ProviderError::Unsupported {
                role: "code hosting",
                ..
            }
        ));
    }
}

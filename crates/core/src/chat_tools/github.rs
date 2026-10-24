//! GitHub chat tools. Issue/PR text is third-party content (untrusted).

use super::{
    connector_of, get, int_text, req, truthy, untrusted_value, Arg, Env, Mode, Tool, ToolError,
    ToolResult,
};
use crate::connectors::{ApiOpts, Connector};
use serde_json::{json, Map, Value};

fn repo_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn repo(args: &Map<String, Value>) -> Result<String, ToolError> {
    let repo = crate::pydumps::py_str(req(args, "repo")?);
    let ok = match repo.split_once('/') {
        Some((owner, name)) => {
            !owner.is_empty()
                && !name.is_empty()
                && owner.chars().all(repo_char)
                && name.chars().all(repo_char)
                && owner != ".."
                && name != ".."
        }
        None => false,
    };
    if ok {
        Ok(repo)
    } else {
        Err(ToolError::input("repo must look like owner/name"))
    }
}

/// Newest 50 comments, oldest first. The endpoint is ascending-only, so with
/// more than 50 the last page is requested instead of the first.
fn comments(
    c: &dyn Connector,
    repo: &str,
    number: &Value,
    total: &Value,
) -> Result<(Value, bool), ToolError> {
    let total = match total {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        _ => 0,
    };
    let mut path = format!(
        "/repos/{repo}/issues/{}/comments?per_page=50",
        int_text(number)?
    );
    if total > 50 {
        path.push_str(&format!("&page={}", (total + 49) / 50));
    }
    let items = c.api("GET", &path, &ApiOpts::default())?;
    let Value::Array(items) = items else {
        return Err(ToolError::Other("TypeError".into()));
    };
    let out = items
        .iter()
        .map(|c| {
            json!({"author": untrusted_value(get(get(c, "user"), "login")),
                   "created_at": get(c, "created_at"), "body": untrusted_value(get(c, "body"))})
        })
        .collect();
    Ok((Value::Array(out), total > 50))
}

fn key<'a>(v: &'a Value, k: &str) -> Result<&'a Value, ToolError> {
    v.get(k).ok_or_else(|| ToolError::Other("KeyError".into()))
}

fn read_issue(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let repo = repo(args)?;
    let number = req(args, "number")?;
    let issue = c.api(
        "GET",
        &format!("/repos/{repo}/issues/{}", int_text(number)?),
        &ApiOpts::default(),
    )?;
    let (comments, truncated) = comments(c, &repo, number, get(&issue, "comments"))?;
    let labels: Vec<Value> = get(&issue, "labels")
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            let name = if l.is_object() { get(l, "name") } else { l };
            Value::String(untrusted_value(name))
        })
        .collect();
    Ok(json!({
        "number": key(&issue, "number")?, "title": untrusted_value(get(&issue, "title")),
        "state": untrusted_value(get(&issue, "state")), "labels": labels,
        "author": untrusted_value(get(get(&issue, "user"), "login")),
        "html_url": get(&issue, "html_url"), "body": untrusted_value(get(&issue, "body")),
        "comments": comments, "comments_truncated": truncated}))
}

fn read_pr(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let repo = repo(args)?;
    let number = req(args, "number")?;
    let pr = c.api(
        "GET",
        &format!("/repos/{repo}/pulls/{}", int_text(number)?),
        &ApiOpts::default(),
    )?;
    let (comments, truncated) = comments(c, &repo, number, get(&pr, "comments"))?;
    Ok(json!({
        "number": key(&pr, "number")?, "title": untrusted_value(get(&pr, "title")),
        "state": untrusted_value(get(&pr, "state")), "merged": truthy(get(&pr, "merged")),
        "author": untrusted_value(get(get(&pr, "user"), "login")),
        "html_url": get(&pr, "html_url"), "body": untrusted_value(get(&pr, "body")),
        "head": get(get(&pr, "head"), "ref"), "base": get(get(&pr, "base"), "ref"),
        "changed_files": get(&pr, "changed_files"),
        "comments": comments, "comments_truncated": truncated}))
}

fn comment(_e: &Env, c: Option<&dyn Connector>, args: &Map<String, Value>) -> ToolResult {
    let c = connector_of(c)?;
    let repo = repo(args)?;
    let made = c.api(
        "POST",
        &format!(
            "/repos/{repo}/issues/{}/comments",
            int_text(req(args, "number")?)?
        ),
        &ApiOpts::json(json!({"body": req(args, "body")?})),
    )?;
    Ok(json!({"comment_id": get(&made, "id")}))
}

static REPO_NUMBER: [Arg; 2] = [Arg::req("repo").help("owner/name"), Arg::int("number")];
static COMMENT_ARGS: [Arg; 3] = [
    Arg::req("body"),
    Arg::req("repo").help("owner/name"),
    Arg::int("number"),
];

pub(crate) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "github.read_issue",
            description: "Read a GitHub issue with its comments",
            args: &REPO_NUMBER,
            mode: Mode::Read,
            connector_type: Some("github"),
            handler: read_issue,
            select: None,
        },
        Tool {
            name: "github.read_pr",
            description: "Read a GitHub pull request with its comments",
            args: &REPO_NUMBER,
            mode: Mode::Read,
            connector_type: Some("github"),
            handler: read_pr,
            select: None,
        },
        Tool {
            name: "github.comment",
            description:
                "Post a comment on a GitHub issue or pull request (needs the user's approval)",
            args: &COMMENT_ARGS,
            mode: Mode::Write,
            connector_type: Some("github"),
            handler: comment,
            select: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::super::*;
    use std::collections::HashMap;

    fn issue() -> Value {
        json!({"number": 5, "title": "Bug", "state": "open", "body": "details",
               "user": {"login": "ann"}, "html_url": "https://github.com/o/r/issues/5"})
    }

    fn comments() -> Value {
        json!([{"user": {"login": "bob"}, "body": "me too", "created_at": "2026-10-01T00:00:00Z"}])
    }

    fn fake(routes: Vec<(&str, Value)>) -> Fake {
        let routes: HashMap<String, Value> = routes
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let mut f = Fake::new(account("gh", "github"));
        f.api = Some(Arc::new(move |_, path, _| {
            Ok(routes
                .get(path)
                .cloned()
                .unwrap_or_else(|| panic!("no route {path}")))
        }));
        f
    }

    fn go(f: &Fake, name: &str, args: Value) -> Value {
        let d = tempfile::tempdir().unwrap();
        let (e, _) = env_with(d.path(), vec![account("gh", "github")], f);
        run(&e, name, &args.to_string(), "")
    }

    #[test]
    fn read_issue_wraps_body_and_comments() {
        let f = fake(vec![
            ("/repos/o/r/issues/5", issue()),
            ("/repos/o/r/issues/5/comments?per_page=50", comments()),
        ]);
        let r = go(&f, "github.read_issue", json!({"repo": "o/r", "number": 5}))["result"].clone();
        assert!(r["title"]
            .as_str()
            .unwrap()
            .starts_with("<untrusted_content>"));
        assert_eq!(r["comments"][0]["author"], untrusted("bob"));
        assert_eq!(r["comments"][0]["body"], untrusted("me too"));
        assert_eq!(r["comments"][0]["created_at"], "2026-10-01T00:00:00Z");
        assert_eq!(r["comments_truncated"], false);
        assert_eq!(r["author"], untrusted("ann"));
        assert_eq!(r["state"], untrusted("open"));
        assert_eq!(r["labels"], json!([]));
        let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "number",
                "title",
                "state",
                "labels",
                "author",
                "html_url",
                "body",
                "comments",
                "comments_truncated"
            ]
        );
    }

    #[test]
    fn read_pr_includes_branches_and_changed_files_count() {
        let mut pr = issue();
        pr["head"] = json!({"ref": "feat"});
        pr["base"] = json!({"ref": "main"});
        pr["changed_files"] = json!(3);
        pr["merged"] = json!(false);
        let f = fake(vec![
            ("/repos/o/r/pulls/5", pr),
            ("/repos/o/r/issues/5/comments?per_page=50", comments()),
        ]);
        let r = go(&f, "github.read_pr", json!({"repo": "o/r", "number": 5}))["result"].clone();
        assert_eq!(
            (&r["head"], &r["base"], &r["changed_files"]),
            (&json!("feat"), &json!("main"), &json!(3))
        );
        assert_eq!(r["merged"], false);
    }

    #[test]
    fn repo_must_be_owner_slash_name() {
        for bad in [
            "../../etc",
            "o/..",
            "../r",
            "o",
            "o/r/x",
            "o r/x",
            "/r",
            "o/",
        ] {
            let r = go(
                &fake(vec![]),
                "github.read_issue",
                json!({"repo": bad, "number": 1}),
            );
            assert_eq!(
                r,
                json!({"error": "repo must look like owner/name"}),
                "{bad}"
            );
        }
    }

    #[test]
    fn comment_posts_json_body_only_when_executed() {
        let f = fake(vec![("/repos/o/r/issues/5/comments", json!({"id": 77}))]);
        assert_eq!(tool_mode("github.comment"), Some(Mode::Write));
        let d = tempfile::tempdir().unwrap();
        let (e, _) = env_with(d.path(), vec![account("gh", "github")], &f);
        let r = execute_tool(
            &e,
            "github.comment",
            &json!({"repo": "o/r", "number": 5, "body": "thanks"}),
        );
        assert_eq!(r["result"], json!({"comment_id": 77}));
        assert_eq!(
            f.log(),
            [json!(["POST", "/repos/o/r/issues/5/comments", {"body": "thanks"}])]
        );
    }

    #[test]
    fn run_only_proposes_the_comment() {
        let f = fake(vec![]);
        let r = go_session(
            &f,
            "github.comment",
            json!({"repo": "o/r", "number": 5, "body": "thanks"}),
        );
        assert_eq!(r["status"], "pending_approval");
        assert_eq!(
            r["args"],
            json!({"repo": "o/r", "number": 5, "body": "thanks", "account": "gh"})
        );
        assert!(f.log().is_empty());
    }

    fn go_session(f: &Fake, name: &str, args: Value) -> Value {
        let d = tempfile::tempdir().unwrap();
        let (e, _) = env_with(d.path(), vec![account("gh", "github")], f);
        run(&e, name, &args.to_string(), "s1")
    }

    #[test]
    fn long_thread_requests_the_last_page_and_flags() {
        let mut i = issue();
        i["comments"] = json!(120);
        i["labels"] = json!([{"name": "bug"}, "plain"]);
        let f = fake(vec![
            ("/repos/o/r/issues/5", i),
            (
                "/repos/o/r/issues/5/comments?per_page=50&page=3",
                comments(),
            ),
        ]);
        let r = go(&f, "github.read_issue", json!({"repo": "o/r", "number": 5}))["result"].clone();
        assert_eq!(r["comments_truncated"], true);
        assert_eq!(r["labels"], json!([untrusted("bug"), untrusted("plain")]));
        assert!(f.log().last().unwrap()[1]
            .as_str()
            .unwrap()
            .ends_with("page=3"));
    }

    #[test]
    fn a_number_must_be_an_integer() {
        let r = go(
            &fake(vec![]),
            "github.read_issue",
            json!({"repo": "o/r", "number": "5"}),
        );
        assert_eq!(r["error"], "number must be an integer");
    }
}

//! Stale Work Sweeper loop on LoopKit (mirrors `bin/loop_plugins/stale_sweeper.py`):
//! once a week, lists GitLab work that has gone quiet - open issues assigned
//! to you and open MRs you opened, idle for `stale_days`, and open MRs waiting
//! on your review, idle for `review_days` - and sends the list through the
//! loop's notifier. No model call: `build_prompt` renders the list and
//! `call_model` returns it unchanged at $0, so the run still goes through
//! `LoopRuntime` (history, ledger, retries). Read-only.

use crate::connectors::http::urlencode;
use crate::connectors::{ApiOpts, Connector, ConnectorSource, Field, LoadFailure};
use crate::loopkit::{
    chat_text, chat_url, LoopContext, LoopPlugin, ModelAnswer, ModelError, Outcome, PluginError,
    Status, WorkItem,
};
use crate::loops_config::{py_str, py_truthy};
use crate::seen_store::parse_iso;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use std::sync::Arc;

pub const DEFAULT_STALE_DAYS: i64 = 14;
pub const DEFAULT_REVIEW_DAYS: i64 = 3;
pub const MAX_ROWS: usize = 20;
pub const MAX_TITLE: usize = 200;

pub const SETTINGS_FIELDS: [Field; 2] = [
    Field::new("stale_days", "Stale after (days)")
        .optional()
        .default("14")
        .help("Your issues and merge requests idle this many days are listed (1-365)")
        .placeholder("14"),
    Field::new("review_days", "Review stale after (days)")
        .optional()
        .default("3")
        .help("Merge requests waiting on your review this many days are listed (1-365)")
        .placeholder("3"),
];

/// Python class name for a failed connector load (only the class is logged).
fn load_class(f: &LoadFailure) -> String {
    match f {
        LoadFailure::Unknown => "KeyError".into(),
        LoadFailure::Config(_) => "ConnectorConfigError".into(),
        LoadFailure::Other(class) => class.clone(),
    }
}

/// `int(str(settings.get(key, default)).strip())` clamped to 1..=365.
pub fn days(settings: &Value, key: &str, default: i64) -> i64 {
    let parsed = match settings.get(key) {
        None => Some(default),
        Some(v) => py_str(v).trim().parse::<i64>().ok(),
    };
    parsed.map_or(default, |v| v.clamp(1, 365))
}

fn iso(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `datetime.fromisoformat(str(updated).replace("Z", "+00:00"))` (naive = UTC).
pub(crate) fn parse_py_iso(text: &str) -> Option<DateTime<Utc>> {
    let t = text.replace('Z', "+00:00");
    parse_iso(&t).or_else(|| {
        NaiveDate::parse_from_str(t.trim(), "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|n| n.and_utc())
    })
}

fn idle_days(updated: Option<&Value>, now: DateTime<Utc>) -> Option<i64> {
    let text = updated.map_or_else(|| "None".to_string(), py_str);
    let when = parse_py_iso(&text)?;
    Some((now - when).num_seconds().div_euclid(86_400).max(0))
}

pub struct StaleSweeper {
    connectors: Arc<dyn ConnectorSource>,
}

impl StaleSweeper {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self { connectors }
    }

    fn section(&self, conn: &dyn Connector, spec: &Spec, ctx: &LoopContext, aid: &str) -> Value {
        let mut query: Vec<(&str, String)> = spec.extra.clone();
        query.extend([
            ("state", "opened".to_string()),
            ("updated_before", iso(ctx.now - Duration::days(spec.days))),
            ("order_by", "updated_at".to_string()),
            ("sort", "asc".to_string()),
            ("per_page", MAX_ROWS.to_string()),
        ]);
        let path = format!("{}?{}", spec.path, urlencode(&query));
        let mut section = json!({"key": spec.key, "label": spec.label, "rows": [], "error": false});
        let rows = match conn.api("GET", &path, &ApiOpts::default()) {
            Ok(v) => v,
            Err(e) => {
                ctx.log(&format!(
                    "stale-sweeper: {} for {aid} failed: {}",
                    spec.key,
                    e.class_name()
                ));
                section["error"] = json!(true);
                return section;
            }
        };
        let mut out = Vec::new();
        for r in rows.as_array().into_iter().flatten().take(MAX_ROWS) {
            if !r.is_object() {
                continue;
            }
            let reference = r
                .get("references")
                .and_then(|x| x.get("full"))
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_else(|| format!("#{}", r.get("iid").map_or("?".into(), py_str)));
            let title = r
                .get("title")
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default();
            let url = r
                .get("web_url")
                .filter(|v| py_truthy(v))
                .map(py_str)
                .unwrap_or_default();
            out.push(json!({
                "ref": chat_text(&reference, 120),
                "title": chat_text(&title, MAX_TITLE),
                "idle_days": idle_days(r.get("updated_at"), ctx.now),
                "url": chat_url(&url),
            }));
        }
        section["rows"] = Value::Array(out);
        section
    }
}

struct Spec {
    key: &'static str,
    label: String,
    path: &'static str,
    extra: Vec<(&'static str, String)>,
    days: i64,
}

impl LoopPlugin for StaleSweeper {
    fn loop_name(&self) -> &str {
        "stale-sweeper-loop"
    }
    fn definition_dir(&self) -> &str {
        "stale-sweeper"
    }
    fn settings_fields(&self) -> &'static [Field] {
        &SETTINGS_FIELDS
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let stale = days(&ctx.settings, "stale_days", DEFAULT_STALE_DAYS);
        let review = days(&ctx.settings, "review_days", DEFAULT_REVIEW_DAYS);
        let mut items = Vec::new();
        for account in self.connectors.accounts_with_capability("issues") {
            if account.kind != "gitlab" {
                continue;
            }
            let aid = account.id.clone();
            let loaded = self.connectors.load(&aid).map_err(|f| load_class(&f));
            let me = loaded.as_ref().map_err(String::clone).and_then(|conn| {
                conn.api("GET", "/user", &ApiOpts::default())
                    .map(|u| u.get("username").map_or(String::new(), py_str))
                    .map_err(|e| e.class_name())
            });
            let (conn, me) = match (loaded, me) {
                (Ok(c), Ok(me)) => (c, me),
                (_, Err(class)) | (Err(class), _) => {
                    ctx.log(&format!("stale-sweeper: account {aid} failed: {class}"));
                    continue;
                }
            };
            let specs = [
                Spec {
                    key: "issues",
                    label: format!("Assigned issues idle {stale}+ days"),
                    path: "/issues",
                    extra: vec![("scope", "assigned_to_me".into())],
                    days: stale,
                },
                Spec {
                    key: "my_mrs",
                    label: format!("My merge requests idle {stale}+ days"),
                    path: "/merge_requests",
                    extra: vec![("scope", "created_by_me".into())],
                    days: stale,
                },
                Spec {
                    key: "reviews",
                    label: format!("Reviews waiting on you {review}+ days"),
                    path: "/merge_requests",
                    extra: vec![("scope", "all".into()), ("reviewer_username", me)],
                    days: review,
                },
            ];
            let sections: Vec<Value> = specs
                .iter()
                .map(|s| self.section(conn.as_ref(), s, ctx, &aid))
                .collect();
            let any_rows = sections
                .iter()
                .any(|s| s["rows"].as_array().is_some_and(|r| !r.is_empty()));
            if any_rows {
                let date = ctx.now.date_naive().format("%Y-%m-%d").to_string();
                items.push(WorkItem::new(
                    format!("stale:{aid}:{date}"),
                    aid,
                    json!({"date": date, "sections": sections}),
                ));
            }
        }
        Ok(items)
    }

    /// The rendered list itself - there is no model to prompt.
    fn build_prompt(&self, item: &WorkItem, _ctx: &LoopContext) -> Result<String, PluginError> {
        let mut lines = vec![chat_text(&item.title, 60)];
        for s in item.payload["sections"].as_array().into_iter().flatten() {
            let label = s["label"].as_str().unwrap_or("");
            if s["error"].as_bool().unwrap_or(false) {
                lines.push(format!("{label}: unavailable"));
                continue;
            }
            let rows = s["rows"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                continue;
            }
            lines.push(format!("{label} ({}):", rows.len()));
            for r in &rows {
                let idle = r["idle_days"]
                    .as_i64()
                    .map_or(String::new(), |d| format!(" (idle {d}d)"));
                let url = r["url"]
                    .as_str()
                    .filter(|u| !u.is_empty())
                    .map_or(String::new(), |u| format!(" {u}"));
                lines.push(format!(
                    "- {} {}{idle}{url}",
                    r["ref"].as_str().unwrap_or(""),
                    r["title"].as_str().unwrap_or("")
                ));
            }
        }
        Ok(lines.join("\n"))
    }

    fn call_model(&self, prompt: &str, _ctx: &LoopContext) -> Result<ModelAnswer, ModelError> {
        Ok(ModelAnswer {
            text: prompt.to_string(),
            cost_usd: Some(0.0),
        })
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        _ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let count: usize = item.payload["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| s["rows"].as_array().map_or(0, Vec::len))
            .sum();
        Ok(
            Outcome::new(&item.key, Status::Done, format!("{count} stale"))
                .with_data(json!({"text": answer})),
        )
    }

    fn digest(&self, outcomes: &[Outcome], ctx: &LoopContext) -> Option<String> {
        let blocks: Vec<String> = outcomes
            .iter()
            .filter(|o| o.status == Status::Done)
            .filter_map(|o| o.data.get("text").filter(|t| py_truthy(t)).map(py_str))
            .collect();
        if blocks.is_empty() {
            return None;
        }
        let mut all = vec![format!(
            "Stale work - {}",
            ctx.now.date_naive().format("%Y-%m-%d")
        )];
        all.extend(blocks);
        Some(all.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{gitlab, Account, ConnResult, ConnectorError, TypeInfo};
    use crate::loop_definition::LoopDefinition;
    use crate::loopkit::{run_plugin, LogFn, RunOptions};
    use chrono::TimeZone;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    const GL: &str = "https://gitlab.example.com";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap()
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn row(iid: i64, updated: &str, title: Option<&str>, kind: &str) -> Value {
        json!({"iid": iid, "title": title.map_or(format!("Item {iid}"), String::from),
               "updated_at": updated, "web_url": format!("{GL}/g/web/-/{kind}/{iid}"),
               "references": {"full": format!("g/web#{iid}")}})
    }

    #[derive(Default)]
    struct GitLab {
        issues: Vec<Value>,
        mine: Vec<Value>,
        reviews: Vec<Value>,
        fail: Vec<&'static str>,
        calls: Mutex<Vec<String>>,
        account: Option<Account>,
    }

    struct Conn(Arc<GitLab>);

    impl Connector for Conn {
        fn info(&self) -> &'static TypeInfo {
            &gitlab::INFO
        }
        fn account(&self) -> &Account {
            self.0.account.as_ref().unwrap()
        }
        fn api(&self, _m: &str, path: &str, _o: &ApiOpts) -> ConnResult<Value> {
            self.0.calls.lock().unwrap().push(path.to_string());
            if path == "/user" {
                return Ok(json!({"username": "me"}));
            }
            let section = if path.starts_with("/issues") {
                "issues"
            } else if path.contains("reviewer_username") {
                "reviews"
            } else {
                "mine"
            };
            if self.0.fail.contains(&section) {
                return Err(ConnectorError::Other {
                    class: "RuntimeError".into(),
                    detail: "boom https://secret".into(),
                });
            }
            Ok(Value::Array(match section {
                "issues" => self.0.issues.clone(),
                "reviews" => self.0.reviews.clone(),
                _ => self.0.mine.clone(),
            }))
        }
    }

    struct Source(BTreeMap<String, Arc<GitLab>>);

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            self.0.keys().map(|id| Account::new(id, "gitlab")).collect()
        }
        fn load(&self, id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            self.0
                .get(id)
                .map(|g| Box::new(Conn(g.clone())) as Box<dyn Connector>)
                .ok_or(LoadFailure::Unknown)
        }
    }

    fn gl(mut g: GitLab) -> Arc<GitLab> {
        g.account = Some(Account::new("gl", "gitlab"));
        Arc::new(g)
    }

    fn plugin(conns: &[(&str, Arc<GitLab>)]) -> StaleSweeper {
        StaleSweeper::new(Arc::new(Source(
            conns
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )))
    }

    fn ctx(settings: Value) -> (LoopContext, Arc<Mutex<Vec<String>>>) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        let log: LogFn = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_string()));
        let definition =
            LoopDefinition::from_yaml(&repo().join("loops/stale-sweeper/loop.yaml")).unwrap();
        (
            LoopContext {
                loop_name: "stale-sweeper-loop".into(),
                run_id: "r".into(),
                now: now(),
                definition: Arc::new(definition),
                settings,
                history_dir: PathBuf::new(),
                log,
                repo_root: repo(),
                force: false,
                call_budget_usd: None,
                model: None,
                state_dir: None,
            },
            logs,
        )
    }

    #[test]
    fn queries_use_the_thresholds_and_my_username() {
        let g = gl(GitLab {
            issues: vec![row(1, "2026-09-01T00:00:00Z", None, "issues")],
            ..Default::default()
        });
        let (c, _) = ctx(json!({"stale_days": "10", "review_days": "2"}));
        plugin(&[("gl", g.clone())]).discover(&c).unwrap();
        let calls = g.calls.lock().unwrap().clone();
        let (issues, mine, reviews) = (&calls[1], &calls[2], &calls[3]);
        assert!(issues.starts_with("/issues?") && issues.contains("scope=assigned_to_me"));
        assert!(issues.contains("state=opened"));
        assert!(issues.contains("updated_before=2026-09-27T09%3A00%3A00Z"));
        assert!(
            mine.contains("scope=created_by_me")
                && mine.contains("updated_before=2026-09-27T09%3A00%3A00Z")
        );
        assert!(reviews.contains("reviewer_username=me"));
        assert!(reviews.contains("updated_before=2026-10-05T09%3A00%3A00Z"));
        for c in [issues, mine, reviews] {
            assert!(c.contains("sort=asc") && c.contains("per_page=20"));
        }
    }

    #[test]
    fn one_item_per_account_with_stale_work() {
        let a = gl(GitLab {
            issues: vec![row(1, "2026-09-01T00:00:00Z", None, "issues")],
            ..Default::default()
        });
        let (c, _) = ctx(json!({}));
        let items = plugin(&[("gl", a), ("empty", gl(GitLab::default()))])
            .discover(&c)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "stale:gl:2026-10-07");
        let sections = items[0].payload["sections"].as_array().unwrap();
        let keys: Vec<_> = sections
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["issues", "my_mrs", "reviews"]);
        assert_eq!(
            sections[0]["rows"][0],
            json!({"ref": "g/web#1", "title": "Item 1", "idle_days": 36,
                   "url": format!("{GL}/g/web/-/issues/1")})
        );
    }

    #[test]
    fn failing_section_is_marked_and_logged_by_class_name() {
        let g = gl(GitLab {
            mine: vec![row(2, "2026-09-01T00:00:00Z", None, "merge_requests")],
            fail: vec!["issues"],
            ..Default::default()
        });
        let (c, logs) = ctx(json!({}));
        let items = plugin(&[("gl", g)]).discover(&c).unwrap();
        let sections = items[0].payload["sections"].as_array().unwrap();
        assert_eq!(sections[0]["error"], true);
        assert!(!sections[1]["rows"].as_array().unwrap().is_empty());
        let logs = logs.lock().unwrap();
        assert!(logs.iter().any(|m| m.contains("RuntimeError")));
        assert!(!logs.iter().any(|m| m.contains("secret")));
    }

    #[test]
    fn call_model_renders_the_list_without_a_model() {
        let g = gl(GitLab {
            issues: vec![row(
                1,
                "2026-09-01T00:00:00Z",
                Some("<!channel> Login broken"),
                "issues",
            )],
            reviews: vec![row(7, "2026-10-01T00:00:00Z", None, "merge_requests")],
            fail: vec!["mine"],
            ..Default::default()
        });
        let (c, _) = ctx(json!({}));
        let p = plugin(&[("gl", g)]);
        let item = p.discover(&c).unwrap().remove(0);
        let res = p
            .call_model(&p.build_prompt(&item, &c).unwrap(), &c)
            .unwrap();
        assert_eq!(res.cost_usd, Some(0.0));
        let text = res.text;
        assert!(text.starts_with("gl"));
        assert!(text.contains("Assigned issues idle 14+ days (1):"));
        assert!(text.contains(&format!(
            "- g/web#1 \u{2039}!channel\u{203a} Login broken (idle 36d) {GL}/g/web/-/issues/1"
        )));
        assert!(text.contains("My merge requests idle 14+ days: unavailable"));
        assert!(
            text.contains("Reviews waiting on you 3+ days (1):") && !text.contains("<!channel>")
        );
    }

    #[test]
    fn after_item_and_digest() {
        let g = gl(GitLab {
            issues: vec![row(1, "2026-09-01T00:00:00Z", None, "issues")],
            ..Default::default()
        });
        let (c, _) = ctx(json!({}));
        let p = plugin(&[("gl", g)]);
        let item = p.discover(&c).unwrap().remove(0);
        let text = p
            .call_model(&p.build_prompt(&item, &c).unwrap(), &c)
            .unwrap()
            .text;
        let out = p.after_item(&item, &Value::String(text), &c).unwrap();
        assert_eq!(
            (out.status, out.summary.as_str()),
            (Status::Done, "1 stale")
        );
        let digest = p.digest(&[out], &c).unwrap();
        assert!(digest.starts_with("Stale work - 2026-10-07") && digest.contains("g/web#1"));
        assert!(p
            .digest(&[Outcome::new("k", Status::Failed, "x")], &c)
            .is_none());
    }

    #[test]
    fn settings_parse_with_fallbacks() {
        assert_eq!(days(&json!({"stale_days": "x"}), "stale_days", 14), 14);
        assert_eq!(days(&json!({"stale_days": "0"}), "stale_days", 14), 1);
        assert_eq!(days(&json!({"review_days": "999"}), "review_days", 3), 365);
        assert_eq!(days(&json!({"stale_days": 10}), "stale_days", 14), 10);
        let keys: Vec<_> = SETTINGS_FIELDS.iter().map(|f| f.key).collect();
        assert_eq!(keys, ["stale_days", "review_days"]);
        assert_eq!(plugin(&[]).settings_fields().len(), 2);
    }

    #[test]
    fn runs_through_loopkit_without_a_model() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path();
        std::fs::create_dir_all(t.join("loops/stale-sweeper")).unwrap();
        std::fs::copy(
            repo().join("loops/stale-sweeper/loop.yaml"),
            t.join("loops/stale-sweeper/loop.yaml"),
        )
        .unwrap();
        let g = gl(GitLab {
            issues: vec![row(1, "2026-09-01T00:00:00Z", None, "issues")],
            ..Default::default()
        });
        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = sent.clone();
        let opts = RunOptions {
            repo_root: Some(t.to_path_buf()),
            now: Some(now()),
            results_dir: Some(t.join("r")),
            events_dir: Some(t.join("e")),
            state_dir: Some(t.join("s")),
            history_dir: Some(t.join("h")),
            lock_path: Some(t.join("lock")),
            loop_lookup: Some(Arc::new(|_| Ok(json!({"settings": {}})))),
            notifier: Some(Arc::new(move |_l, text| {
                sink.lock().unwrap().push(text.to_string());
                vec![("default".into(), true, "sent".into())]
            })),
            ..Default::default()
        };
        let (outcomes, _) = run_plugin(Arc::new(plugin(&[("gl", g)])), "run_t", opts).unwrap();
        assert_eq!(
            outcomes.iter().map(|o| o.status).collect::<Vec<_>>(),
            [Status::Done]
        );
        let sent = sent.lock().unwrap();
        assert!(sent[0].starts_with("Stale work - 2026-10-07"));
    }

    #[test]
    fn definition_loads_and_template_registers_the_loop() {
        let d = LoopDefinition::from_yaml(&repo().join("loops/stale-sweeper/loop.yaml")).unwrap();
        assert_eq!(d.name, "stale-sweeper-loop");
        let loops: Value = serde_json::from_str(
            &std::fs::read_to_string(repo().join("config/loops.json.template")).unwrap(),
        )
        .unwrap();
        let entry = loops
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == "stale-sweeper-loop")
            .unwrap();
        assert_eq!(entry["entry_point"], "bin.loop_plugins.stale_sweeper");
        assert_eq!(entry["enabled"], false);
        assert_eq!(
            entry["schedule"],
            json!({"frequency": "weekly", "weekdays": [1], "hour": 9, "minute": 0})
        );
        assert_eq!(entry["requires"], json!(["issues"]));
        assert_eq!(entry["routes_notifications"], true);
    }
}

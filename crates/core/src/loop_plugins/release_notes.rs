//! Release Notes loop on LoopKit (mirrors `bin/loop_plugins/release_notes.py`):
//! when a tracked GitLab project gets a new tag, collects the MRs merged into
//! the default branch since the previous tag, has the model group them into
//! highlights / fixes / internal, writes a markdown file under
//! `outputs/loops/release-notes-loop/` and sends a short summary through the
//! loop's notifier. Read-only on GitLab. The first time a project is seen its
//! newest tag only becomes the baseline, so enabling the loop never writes
//! notes for old releases. MR text is untrusted: trimmed before the prompt,
//! the model runs sealed, output is sanitised and links are always built from
//! the offered MRs.

use super::pipeline_doctor::{default_projects, ProjectsFn};
use super::stale_sweeper::parse_py_iso;
use crate::connectors::http::{quote, urlencode};
use crate::connectors::{ApiOpts, Connector, ConnectorSource, Field, LoadFailure};
use crate::loopkit::{
    chat_link, chat_text, chat_url, LoopContext, LoopPlugin, Outcome, PluginError, Status, WorkItem,
};
use crate::loops_config::{py_str, py_truthy};
use crate::pyjson;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_MRS: usize = 100;
pub const MAX_DESCRIPTION: usize = 1000;
pub const MAX_ENTRY: usize = 200;
pub const MAX_ENTRIES: usize = 10;
pub const SECTIONS: [(&str, &str); 3] = [
    ("highlights", "Highlights"),
    ("fixes", "Fixes"),
    ("internal", "Internal"),
];

pub const SETTINGS_FIELDS: [Field; 1] = [Field::new("projects", "Projects")
    .optional()
    .help("Comma-separated project aliases to watch; empty watches every tracked project")
    .placeholder("web, api")];

fn load_class(f: &LoadFailure) -> String {
    match f {
        LoadFailure::Unknown => "KeyError".into(),
        LoadFailure::Config(_) => "ConnectorConfigError".into(),
        LoadFailure::Other(class) => class.clone(),
    }
}

/// `re.sub(r"[^A-Za-z0-9._-]", "_", f"{alias}-{tag}") + ".md"`.
pub fn file_name(alias: &str, tag: &str) -> String {
    let safe: String = format!("{alias}-{tag}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.md")
}

fn parse_value(v: Option<&Value>) -> Option<DateTime<Utc>> {
    parse_py_iso(&v.map_or_else(|| "None".to_string(), py_str))
}

fn tag_date(tag: &Value) -> Option<DateTime<Utc>> {
    parse_value(tag.get("commit").and_then(|c| c.get("committed_date")))
}

fn read_json_object(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.tmp",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let result = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    result
}

fn io_err(e: std::io::Error) -> PluginError {
    PluginError::new("OSError", e.to_string())
}

fn truthy_str(v: Option<&Value>) -> String {
    v.filter(|v| py_truthy(v)).map(py_str).unwrap_or_default()
}

pub struct ReleaseNotes {
    connectors: Arc<dyn ConnectorSource>,
    projects_fn: Option<ProjectsFn>,
    state_dir: Option<PathBuf>,
}

impl ReleaseNotes {
    pub fn new(connectors: Arc<dyn ConnectorSource>) -> Self {
        Self {
            connectors,
            projects_fn: None,
            state_dir: None,
        }
    }

    /// Override where tracked projects come from (`projects_fn`).
    pub fn with_projects(mut self, f: ProjectsFn) -> Self {
        self.projects_fn = Some(f);
        self
    }

    /// Override the directory notes and `tags.json` are written to (`state_dir`).
    pub fn with_state_dir(mut self, dir: &Path) -> Self {
        self.state_dir = Some(dir.to_path_buf());
        self
    }

    fn dir(&self, ctx: &LoopContext) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            ctx.repo_root
                .join("outputs")
                .join("loops")
                .join(self.loop_name())
        })
    }

    fn baseline_path(&self, ctx: &LoopContext) -> PathBuf {
        self.dir(ctx).join("tags.json")
    }

    fn project_item(
        &self,
        conn: &dyn Connector,
        aid: &str,
        project: &Value,
        baseline: &Map<String, Value>,
        new_baselines: &mut Map<String, Value>,
    ) -> Result<Option<WorkItem>, PluginError> {
        let pid = project
            .get("project_id")
            .ok_or_else(|| PluginError::new("KeyError", "project_id"))?;
        let pid_s = py_str(pid);
        let q = quote(&pid_s);
        let tags = conn.api(
            "GET",
            &format!("/projects/{q}/repository/tags?per_page=2"),
            &ApiOpts::default(),
        )?;
        let tags: Vec<&Value> = tags
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t.is_object() && t.get("name").is_some_and(py_truthy))
            .collect();
        let Some(newest) = tags.first() else {
            return Ok(None);
        };
        let newest_name = py_str(&newest["name"]);
        let state_key = format!("{aid}:{pid_s}");
        match baseline.get(&state_key) {
            None => {
                new_baselines.insert(state_key, newest["name"].clone());
                return Ok(None);
            }
            Some(b) if *b == newest["name"] => return Ok(None),
            Some(_) => {}
        }
        let previous = tags.get(1).copied();
        let info = conn.api("GET", &format!("/projects/{q}"), &ApiOpts::default())?;
        let branch = {
            let own = truthy_str(project.get("default_branch"));
            if !own.is_empty() {
                own
            } else {
                let theirs = truthy_str(info.get("default_branch"));
                if theirs.is_empty() {
                    "main".to_string()
                } else {
                    theirs
                }
            }
        };
        let new_date = tag_date(newest);
        let prev_date = previous.and_then(tag_date);
        let mut query: Vec<(&str, String)> = vec![
            ("state", "merged".into()),
            ("target_branch", branch),
            ("per_page", MAX_MRS.to_string()),
        ];
        if let (Some(prev), Some(_)) = (previous, prev_date) {
            let raw = prev
                .get("commit")
                .and_then(|c| c.get("committed_date"))
                .map(py_str)
                .unwrap_or_default();
            query.push(("updated_after", raw));
        }
        let rows = conn.api(
            "GET",
            &format!("/projects/{q}/merge_requests?{}", urlencode(&query)),
            &ApiOpts::default(),
        )?;
        let mut mrs: Vec<(DateTime<Utc>, Value)> = Vec::new();
        for row in rows.as_array().into_iter().flatten() {
            let merged = if row.is_object() {
                parse_value(row.get("merged_at"))
            } else {
                None
            };
            let Some(merged) = merged else { continue };
            if new_date.is_some_and(|n| merged > n) {
                continue;
            }
            if prev_date.is_some_and(|p| merged <= p) {
                continue;
            }
            let labels: Vec<Value> = row
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|l| json!(chat_text(l, 60)))
                .take(10)
                .collect();
            let author = row
                .get("author")
                .filter(|a| py_truthy(a))
                .and_then(|a| a.get("username"))
                .filter(|u| py_truthy(u))
                .map(py_str)
                .unwrap_or_default();
            mrs.push((
                merged,
                json!({
                    "iid": row.get("iid").cloned().unwrap_or(Value::Null),
                    "title": chat_text(&truthy_str(row.get("title")), MAX_ENTRY),
                    "labels": labels,
                    "author": chat_text(&author, 60),
                    "web_url": chat_url(&truthy_str(row.get("web_url"))),
                    "description": truthy_str(row.get("description")).chars().take(MAX_DESCRIPTION).collect::<String>(),
                }),
            ));
        }
        mrs.sort_by_key(|(merged, _)| *merged);
        let alias = project
            .get("alias")
            .map(py_str)
            .ok_or_else(|| PluginError::new("KeyError", "alias"))?;
        let web_url = info.get("web_url").map_or(String::new(), py_str);
        let url = chat_url(&format!("{web_url}/-/tags/{}", quote(&newest_name)));
        Ok(Some(
            WorkItem::new(
                format!("rel:{aid}:{pid_s}:{newest_name}"),
                format!("{alias} {newest_name}"),
                json!({
                    "project": alias, "state_key": format!("{aid}:{pid_s}"), "tag": newest_name,
                    "tag_message": truthy_str(newest.get("message")).chars().take(MAX_DESCRIPTION).collect::<String>(),
                    "previous_tag": previous.map_or(String::new(), |p| py_str(&p["name"])),
                    "mrs": mrs.into_iter().map(|(_, m)| m).take(MAX_MRS).collect::<Vec<_>>(),
                }),
            )
            .with_url(url),
        ))
    }
}

impl LoopPlugin for ReleaseNotes {
    fn loop_name(&self) -> &str {
        "release-notes-loop"
    }
    fn definition_dir(&self) -> &str {
        "release-notes"
    }
    fn output_keys(&self) -> &[&'static str] {
        &["highlights", "fixes", "internal"]
    }
    fn settings_fields(&self) -> &'static [Field] {
        &SETTINGS_FIELDS
    }

    fn discover(&self, ctx: &LoopContext) -> Result<Vec<WorkItem>, PluginError> {
        let projects = match &self.projects_fn {
            Some(f) => f(),
            None => default_projects(),
        };
        let mut projects = match projects {
            Ok(p) => p,
            Err(e) => {
                ctx.log(&format!(
                    "release-notes: project config failed: {}",
                    e.class
                ));
                return Ok(Vec::new());
            }
        };
        let wanted: BTreeSet<String> = ctx
            .settings
            .get("projects")
            .filter(|v| py_truthy(v))
            .map(py_str)
            .unwrap_or_default()
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        if !wanted.is_empty() {
            projects.retain(|p| {
                p.get("alias")
                    .and_then(Value::as_str)
                    .is_some_and(|a| wanted.contains(a))
            });
        }
        let mut baseline = read_json_object(&self.baseline_path(ctx));
        let mut new_baselines = Map::new();
        let mut items = Vec::new();
        for account in self.connectors.accounts_with_capability("merge_requests") {
            if account.kind != "gitlab" {
                continue;
            }
            let aid = account.id;
            let mine: Vec<&Value> = projects
                .iter()
                .filter(|p| p.get("gitlab_instance").and_then(Value::as_str) == Some(aid.as_str()))
                .collect();
            if mine.is_empty() {
                continue;
            }
            let conn = match self.connectors.load(&aid) {
                Ok(c) => c,
                Err(f) => {
                    ctx.log(&format!(
                        "release-notes: account {aid} failed: {}",
                        load_class(&f)
                    ));
                    continue;
                }
            };
            for project in mine {
                match self.project_item(conn.as_ref(), &aid, project, &baseline, &mut new_baselines)
                {
                    Ok(Some(item)) => items.push(item),
                    Ok(None) => {}
                    Err(e) => ctx.log(&format!(
                        "release-notes: project {} failed: {}",
                        project.get("alias").map_or("None".to_string(), py_str),
                        e.class
                    )),
                }
            }
        }
        if !new_baselines.is_empty() {
            baseline.extend(new_baselines);
            write_atomic(
                &self.baseline_path(ctx),
                &(pyjson::to_string_pretty(&Value::Object(baseline)) + "\n"),
            )
            .map_err(io_err)?;
        }
        Ok(items)
    }

    fn after_item(
        &self,
        item: &WorkItem,
        answer: &Value,
        ctx: &LoopContext,
    ) -> Result<Outcome, PluginError> {
        let mrs = item.payload["mrs"].as_array().cloned().unwrap_or_default();
        // The offered MR whose iid equals `want` (Python dict lookup: 2 == 2.0).
        let offered = |want: Option<&Value>| -> Option<(Value, String)> {
            let want = want?;
            mrs.iter().find_map(|m| {
                let iid = &m["iid"];
                let same = iid == want
                    || (iid.is_number() && want.is_number() && iid.as_f64() == want.as_f64());
                same.then(|| (iid.clone(), m["web_url"].as_str().unwrap_or("").to_string()))
            })
        };
        let mut notes = Map::new();
        for (key, _) in SECTIONS {
            let mut entries = Vec::new();
            for entry in answer
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(obj) = entry.as_object() else {
                    continue;
                };
                let Some(text) = obj.get("text").filter(|t| py_truthy(t)) else {
                    continue;
                };
                let (iid, url) = match offered(obj.get("mr")) {
                    Some((iid, url)) => (iid, url),
                    None => (Value::Null, String::new()),
                };
                entries.push(
                    json!({"text": chat_text(&py_str(text), MAX_ENTRY), "mr": iid, "url": url}),
                );
            }
            entries.truncate(MAX_ENTRIES);
            notes.insert(key.to_string(), Value::Array(entries));
        }
        let project = item.payload["project"].as_str().unwrap_or("").to_string();
        let tag = item.payload["tag"].as_str().unwrap_or("").to_string();
        let mut lines = vec![format!("# {project} {tag}")];
        for (key, heading) in SECTIONS {
            lines.push(String::new());
            lines.push(format!("## {heading}"));
            lines.push(String::new());
            for e in notes[key].as_array().into_iter().flatten() {
                let url = e["url"].as_str().unwrap_or("");
                let reference = if url.is_empty() {
                    String::new()
                } else {
                    format!(" ([!{}]({url}))", py_str(&e["mr"]))
                };
                lines.push(format!("- {}{reference}", e["text"].as_str().unwrap_or("")));
            }
        }
        let path = self.dir(ctx).join(file_name(&project, &tag));
        write_atomic(&path, &(lines.join("\n") + "\n")).map_err(io_err)?;
        let mut baseline = read_json_object(&self.baseline_path(ctx));
        baseline.insert(
            item.payload["state_key"].as_str().unwrap_or("").to_string(),
            Value::String(tag.clone()),
        );
        write_atomic(
            &self.baseline_path(ctx),
            &(pyjson::to_string_pretty(&Value::Object(baseline)) + "\n"),
        )
        .map_err(io_err)?;
        let counts = SECTIONS
            .iter()
            .map(|(k, _)| format!("{} {k}", notes[*k].as_array().map_or(0, Vec::len)))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Outcome::new(&item.key, Status::Done, counts)
            .with_url(item.url.clone())
            .with_data(json!({
                "title": item.title, "notes": notes, "path": path.display().to_string()})))
    }

    fn digest(&self, outcomes: &[Outcome], _ctx: &LoopContext) -> Option<String> {
        let mut blocks = Vec::new();
        for o in outcomes {
            if o.status != Status::Done {
                continue;
            }
            let Some(notes) = o.data.get("notes") else {
                continue;
            };
            let len = |k: &str| notes[k].as_array().map_or(0, Vec::len);
            let mut lines = vec![format!(
                "Release notes: {} - {} highlights, {} fixes, {} internal",
                chat_text(o.data["title"].as_str().unwrap_or(""), MAX_ENTRY),
                len("highlights"),
                len("fixes"),
                len("internal")
            )];
            for e in notes["highlights"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "- {}",
                    chat_link(
                        e["text"].as_str().unwrap_or(""),
                        e["url"].as_str().unwrap_or("")
                    )
                ));
            }
            if !o.url.is_empty() {
                lines.push(format!("Tag: {}", o.url));
            }
            lines.push(format!(
                "Notes: {}",
                chat_text(o.data["path"].as_str().unwrap_or(""), 300)
            ));
            blocks.push(lines.join("\n"));
        }
        (!blocks.is_empty()).then(|| blocks.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{gitlab, Account, ConnResult, ConnectorError, TypeInfo};
    use crate::loop_definition::LoopDefinition;
    use crate::loopkit::LogFn;
    use chrono::TimeZone;
    use std::sync::Mutex;

    const WEB: &str = "https://gitlab.example.com/g/web";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap()
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn tag(name: &str, date: &str, message: &str) -> Value {
        json!({"name": name, "message": message, "commit": {"committed_date": date}})
    }

    fn mr(iid: i64, merged_at: &str) -> Value {
        json!({"iid": iid, "title": format!("MR {iid}"), "labels": ["bug"], "author": {"username": "ann"},
               "web_url": format!("{WEB}/-/merge_requests/{iid}"), "description": "d".repeat(3000),
               "merged_at": merged_at})
    }

    struct GitLab {
        tags: Vec<Value>,
        mrs: Vec<Value>,
        fail: bool,
        calls: Mutex<Vec<String>>,
        account: Account,
    }

    struct Conn(Arc<GitLab>);

    impl Connector for Conn {
        fn info(&self) -> &'static TypeInfo {
            &gitlab::INFO
        }
        fn account(&self) -> &Account {
            &self.0.account
        }
        fn api(&self, _m: &str, path: &str, _o: &ApiOpts) -> ConnResult<Value> {
            self.0.calls.lock().unwrap().push(path.to_string());
            if self.0.fail {
                return Err(ConnectorError::Other {
                    class: "RuntimeError".into(),
                    detail: "boom https://secret".into(),
                });
            }
            if path.contains("/repository/tags") {
                return Ok(Value::Array(self.0.tags.clone()));
            }
            if path.contains("/merge_requests") {
                return Ok(Value::Array(self.0.mrs.clone()));
            }
            Ok(json!({"id": 7, "default_branch": "main", "web_url": WEB}))
        }
    }

    struct Source(Arc<GitLab>);

    impl ConnectorSource for Source {
        fn accounts_with_capability(&self, _c: &str) -> Vec<Account> {
            vec![Account::new("gl", "gitlab")]
        }
        fn load(&self, _id: &str) -> Result<Box<dyn Connector>, LoadFailure> {
            Ok(Box::new(Conn(self.0.clone())))
        }
    }

    fn gitlab(tags: Vec<Value>, mrs: Vec<Value>, fail: bool) -> Arc<GitLab> {
        Arc::new(GitLab {
            tags,
            mrs,
            fail,
            calls: Mutex::new(vec![]),
            account: Account::new("gl", "gitlab"),
        })
    }

    fn projects() -> Vec<Value> {
        vec![
            json!({"alias": "web", "gitlab_instance": "gl", "project_id": 7, "default_branch": "main"}),
        ]
    }

    fn plugin(dir: &Path, conn: &Arc<GitLab>, projects: Vec<Value>) -> ReleaseNotes {
        ReleaseNotes::new(Arc::new(Source(conn.clone())))
            .with_projects(Arc::new(move || Ok(projects.clone())))
            .with_state_dir(dir)
    }

    fn ctx(settings: Value) -> (LoopContext, Arc<Mutex<Vec<String>>>) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        let log: LogFn = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_string()));
        let d = LoopDefinition::from_yaml(&repo().join("loops/release-notes/loop.yaml")).unwrap();
        (
            LoopContext {
                loop_name: "release-notes-loop".into(),
                run_id: "r".into(),
                now: now(),
                definition: Arc::new(d),
                settings,
                history_dir: PathBuf::new(),
                log,
                repo_root: PathBuf::from("/nonexistent"),
                force: false,
                call_budget_usd: None,
                model: None,
                state_dir: None,
            },
            logs,
        )
    }

    fn baseline(dir: &Path, name: &str) {
        std::fs::write(dir.join("tags.json"), json!({"gl:7": name}).to_string()).unwrap();
    }

    fn tags() -> Vec<Value> {
        vec![
            tag("v1.1", "2026-10-06T12:00:00Z", "Release 1.1"),
            tag("v1.0", "2026-10-01T12:00:00Z", ""),
        ]
    }

    fn mrs() -> Vec<Value> {
        vec![
            mr(3, "2026-10-07T08:00:00Z"),
            mr(2, "2026-10-05T10:00:00Z"),
            mr(1, "2026-10-02T10:00:00Z"),
            mr(0, "2026-09-30T10:00:00Z"),
        ]
    }

    #[test]
    fn first_sight_records_a_baseline_and_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let g = gitlab(tags(), mrs(), false);
        let (c, _) = ctx(json!({}));
        assert!(plugin(d.path(), &g, projects())
            .discover(&c)
            .unwrap()
            .is_empty());
        let b: Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("tags.json")).unwrap())
                .unwrap();
        assert_eq!(b, json!({"gl:7": "v1.1"}));
        assert!(!g
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("/merge_requests")));
    }

    #[test]
    fn no_item_when_the_newest_tag_is_already_handled() {
        let d = tempfile::tempdir().unwrap();
        baseline(d.path(), "v1.1");
        let (c, _) = ctx(json!({}));
        let g = gitlab(tags(), mrs(), false);
        assert!(plugin(d.path(), &g, projects())
            .discover(&c)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn new_tag_yields_one_item_with_the_mrs_between_the_tags() {
        let d = tempfile::tempdir().unwrap();
        baseline(d.path(), "v1.0");
        let g = gitlab(tags(), mrs(), false);
        let (c, _) = ctx(json!({}));
        let items = plugin(d.path(), &g, projects()).discover(&c).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.key, "rel:gl:7:v1.1");
        assert_eq!(item.title, "web v1.1");
        let p = &item.payload;
        assert_eq!(p["project"], "web");
        assert_eq!(p["tag"], "v1.1");
        assert_eq!(p["previous_tag"], "v1.0");
        assert_eq!(p["tag_message"], "Release 1.1");
        let iids: Vec<_> = p["mrs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["iid"].as_i64().unwrap())
            .collect();
        assert_eq!(iids, [1, 2]);
        assert!(p["mrs"][0]["description"].as_str().unwrap().len() <= 1000);
        assert_eq!(p["mrs"][0]["author"], "ann");
        let calls = g.calls.lock().unwrap();
        let mr_call = calls
            .iter()
            .find(|c| c.contains("/merge_requests"))
            .unwrap();
        assert!(mr_call.contains("state=merged") && mr_call.contains("target_branch=main"));
        assert!(mr_call.contains("updated_after=2026-10-01T12%3A00%3A00Z"));
        assert_eq!(item.url, format!("{WEB}/-/tags/v1.1"));
    }

    #[test]
    fn first_ever_tag_takes_every_mr_up_to_it() {
        let d = tempfile::tempdir().unwrap();
        baseline(d.path(), "none-yet");
        let g = gitlab(vec![tag("v0.1", "2026-10-06T12:00:00Z", "")], mrs(), false);
        let (c, _) = ctx(json!({}));
        let items = plugin(d.path(), &g, projects()).discover(&c).unwrap();
        let iids: Vec<_> = items[0].payload["mrs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["iid"].as_i64().unwrap())
            .collect();
        assert_eq!(iids, [0, 1, 2]);
        assert_eq!(items[0].payload["previous_tag"], "");
    }

    #[test]
    fn projects_setting_limits_the_projects() {
        let d = tempfile::tempdir().unwrap();
        let mut both = projects();
        both.push(json!({"alias": "api", "gitlab_instance": "gl", "project_id": 8, "default_branch": "main"}));
        let g = gitlab(tags(), mrs(), false);
        let (c, _) = ctx(json!({"projects": "api"}));
        plugin(d.path(), &g, both).discover(&c).unwrap();
        assert!(g
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.contains("/projects/8/")));
    }

    #[test]
    fn a_failing_project_is_logged_by_class_name() {
        let d = tempfile::tempdir().unwrap();
        let g = gitlab(tags(), vec![], true);
        let (c, logs) = ctx(json!({}));
        assert!(plugin(d.path(), &g, projects())
            .discover(&c)
            .unwrap()
            .is_empty());
        let logs = logs.lock().unwrap();
        assert!(logs.iter().any(|m| m.contains("RuntimeError")));
        assert!(!logs.iter().any(|m| m.contains("secret")));
    }

    fn answer() -> Value {
        json!({"highlights": [{"text": "Faster search", "mr": 2}, {"text": "<!channel> fake", "mr": 99}],
               "fixes": (0..15).map(|n| json!({"text": format!("fix {n}"), "mr": 1})).collect::<Vec<_>>(),
               "internal": []})
    }

    fn prepared(d: &Path) -> (ReleaseNotes, WorkItem) {
        baseline(d, "v1.0");
        let g = gitlab(tags(), mrs(), false);
        let p = plugin(d, &g, projects());
        let (c, _) = ctx(json!({}));
        let item = p.discover(&c).unwrap().remove(0);
        (p, item)
    }

    #[test]
    fn after_item_writes_markdown_and_advances_the_baseline() {
        let d = tempfile::tempdir().unwrap();
        let (p, item) = prepared(d.path());
        let (c, _) = ctx(json!({}));
        let out = p.after_item(&item, &answer(), &c).unwrap();
        assert_eq!(out.status, Status::Done);
        let md = std::fs::read_to_string(d.path().join("web-v1.1.md")).unwrap();
        assert!(md.starts_with("# web v1.1"));
        assert!(md.contains(&format!("- Faster search ([!2]({WEB}/-/merge_requests/2))")));
        assert!(!md.contains("!99") && !md.contains("<!channel>"));
        assert_eq!(md.matches("- fix").count(), 10);
        let b: Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("tags.json")).unwrap())
                .unwrap();
        assert_eq!(b, json!({"gl:7": "v1.1"}));
        assert_eq!(
            out.data["path"],
            d.path().join("web-v1.1.md").display().to_string()
        );
        assert_eq!(out.summary, "2 highlights, 10 fixes, 0 internal");
    }

    #[test]
    fn markdown_file_name_is_safe() {
        assert_eq!(
            file_name("web/x", "release 1.0/../../x"),
            "web_x-release_1.0_.._.._x.md"
        );
    }

    #[test]
    fn digest_summarises_each_tag() {
        let d = tempfile::tempdir().unwrap();
        let (p, item) = prepared(d.path());
        let (c, _) = ctx(json!({}));
        let out = p.after_item(&item, &answer(), &c).unwrap();
        let text = p.digest(&[out], &c).unwrap();
        assert!(text.starts_with("Release notes: web v1.1 - 2 highlights, 10 fixes, 0 internal"));
        assert!(text.contains(&format!("Faster search ({WEB}/-/merge_requests/2)")));
        assert!(text.contains(&format!("{WEB}/-/tags/v1.1")) && !text.contains("<!channel>"));
        assert!(p
            .digest(&[Outcome::new("k", Status::Failed, "x")], &c)
            .is_none());
    }

    #[test]
    fn settings_fields_declared() {
        let keys: Vec<_> = SETTINGS_FIELDS.iter().map(|f| f.key).collect();
        assert_eq!(keys, ["projects"]);
        let g = gitlab(vec![], vec![], false);
        assert_eq!(
            plugin(Path::new("/x"), &g, vec![]).settings_fields().len(),
            1
        );
    }

    #[test]
    fn definition_prompt_and_template_load() {
        let d = LoopDefinition::from_yaml(&repo().join("loops/release-notes/loop.yaml")).unwrap();
        assert_eq!(d.name, "release-notes-loop");
        let prompt = std::fs::read_to_string(repo().join("loops/release-notes/prompt.md")).unwrap();
        assert!(prompt.contains("{{item_json}}") && prompt.contains("untrusted"));
        let loops: Value = serde_json::from_str(
            &std::fs::read_to_string(repo().join("config/loops.json.template")).unwrap(),
        )
        .unwrap();
        let e = loops
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == "release-notes-loop")
            .unwrap();
        assert_eq!(e["entry_point"], "bin.loop_plugins.release_notes");
        assert_eq!(e["enabled"], false);
        assert_eq!(
            e["schedule"],
            json!({"frequency": "hourly", "interval_hours": 1})
        );
        assert_eq!(e["requires"], json!(["merge_requests"]));
        assert_eq!(e["routes_notifications"], true);
    }
}

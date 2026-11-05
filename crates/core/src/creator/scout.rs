//! Read video-scout catalogs (`catalog-*.json`, schema 1.1) from the scout directory.
//!
//! Catalog files are untrusted data: every field is read leniently, a missing or
//! mistyped field becomes `None`/empty, and a malformed file is reported rather than fatal.

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start: Option<String>,
    pub end: Option<String>,
    pub idea: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub run_id: String,
    pub rank: u32,
    pub title: String,
    pub url: String,
    pub platform: String,
    pub channel: String,
    pub published: Option<String>,
    pub duration_s: Option<u64>,
    pub score: Option<f64>,
    pub grade: Option<String>,
    pub confidence: Option<String>,
    pub rationale: String,
    pub handoff_ready: bool,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoutRun {
    pub id: String,
    pub generated_at: String,
    pub mode: String,
    pub query: Option<String>,
    pub sources: Vec<Source>,
}

/// `<user_home>/.video-scout`: the scout jobs' working directory, where the skill
/// writes its catalogs. No environment override: the jobs and the skill would not
/// honour it, so the page would read a different folder than the jobs write.
pub fn scout_root(user_home: &Path) -> PathBuf {
    super::spawn::scout_dir(user_home)
}

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn clock(secs: f64) -> String {
    let t = secs.max(0.0).round() as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

fn segment_edge(seg: &Value, secs_key: &str) -> Option<String> {
    seg.get(secs_key).and_then(Value::as_f64).map(clock)
}

fn parse_source(run_id: &str, v: &Value) -> Source {
    let quality = v.get("quality").unwrap_or(&Value::Null);
    let segments = v
        .get("recommended_segments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| Segment {
                    start: segment_edge(s, "start_seconds").or_else(|| text(s, "locator")),
                    end: segment_edge(s, "end_seconds"),
                    idea: text(s, "description").unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    Source {
        run_id: run_id.to_string(),
        rank: v
            .get("rank")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        title: text(v, "title").unwrap_or_default(),
        url: text(v, "url").unwrap_or_default(),
        platform: text(v, "platform").unwrap_or_default(),
        channel: text(v, "channel_or_publisher").unwrap_or_default(),
        published: text(v, "published_at"),
        duration_s: v.get("duration_seconds").and_then(Value::as_u64),
        score: quality.get("total_score").and_then(Value::as_f64),
        grade: text(quality, "grade"),
        confidence: text(quality, "confidence"),
        rationale: text(quality, "rationale").unwrap_or_default(),
        handoff_ready: v.get("handoff_ready").and_then(Value::as_bool) == Some(true),
        segments,
    }
}

fn read_run(path: &Path, id: &str) -> Result<ScoutRun, String> {
    let name = path.display();
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("{name}: {e}"))?;
    if !v.is_object() {
        return Err(format!("{name}: not a JSON object"));
    }
    let sources = v
        .get("videos")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|s| parse_source(id, s)).collect())
        .unwrap_or_default();
    Ok(ScoutRun {
        id: id.to_string(),
        generated_at: text(&v, "generated_at").unwrap_or_default(),
        mode: text(&v, "discovery_mode").unwrap_or_default(),
        query: text(&v, "query"),
        sources,
    })
}

/// All readable catalogs under `root`, newest first, plus one error string per bad file.
pub fn list_runs(root: &Path) -> (Vec<ScoutRun>, Vec<String>) {
    let mut runs = Vec::new();
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (runs, errors);
    };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = fname
            .strip_prefix("catalog-")
            .and_then(|s| s.strip_suffix(".json"))
        else {
            continue;
        };
        if id == "index" || id.is_empty() {
            continue;
        }
        match read_run(&entry.path(), id) {
            Ok(r) => runs.push(r),
            Err(e) => errors.push(e),
        }
    }
    runs.sort_by(|a, b| {
        (b.generated_at.as_str(), b.id.as_str()).cmp(&(a.generated_at.as_str(), a.id.as_str()))
    });
    errors.sort();
    (runs, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(generated_at: &str, query: &str) -> String {
        format!(
            r#"{{"schema_version":"1.1","generated_at":"{generated_at}","discovery_mode":"topic_based","query":"{query}",
"videos":[{{"rank":1,"video_id":"2210","title":"A talk about teams","url":"https://example.com/talks/a",
"platform":"TED","channel_or_publisher":"TED","published_at":"2015-03-13","duration_seconds":1027,
"quality":{{"total_score":81.2,"grade":"Excellent","confidence":"medium","rationale":"Solid."}},
"recommended_segments":[{{"start_seconds":931.1,"end_seconds":1024.8,"description":"The thesis"}}],
"handoff_ready":true}}]}}"#
        )
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn lists_runs_newest_first() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "catalog-2026-11-03-a.json",
            &catalog("2026-11-03T00:00:00Z", "a"),
        );
        write(
            d.path(),
            "catalog-2026-11-04-b.json",
            &catalog("2026-11-04T00:00:00Z", "b"),
        );
        write(d.path(), "report-2026-11-04-b.md", "# report");
        let (runs, errors) = list_runs(d.path());
        assert!(errors.is_empty());
        let ids: Vec<_> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["2026-11-04-b", "2026-11-03-a"]);
    }

    #[test]
    fn maps_recommendations_to_sources() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "catalog-x.json",
            &catalog("2026-11-04T00:00:00Z", "leadership"),
        );
        let (runs, _) = list_runs(d.path());
        let run = &runs[0];
        assert_eq!(run.query.as_deref(), Some("leadership"));
        assert_eq!(run.mode, "topic_based");
        let s = &run.sources[0];
        assert_eq!(s.run_id, "x");
        assert_eq!(s.rank, 1);
        assert_eq!(s.channel, "TED");
        assert_eq!(s.published.as_deref(), Some("2015-03-13"));
        assert_eq!(s.duration_s, Some(1027));
        assert_eq!(s.score, Some(81.2));
        assert_eq!(s.grade.as_deref(), Some("Excellent"));
        assert_eq!(s.confidence.as_deref(), Some("medium"));
        assert!(s.handoff_ready);
        assert_eq!(
            s.segments,
            vec![Segment {
                start: Some("15:31".into()),
                end: Some("17:05".into()),
                idea: "The thesis".into()
            }]
        );
    }

    #[test]
    fn malformed_catalog_is_reported_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "catalog-good.json",
            &catalog("2026-11-04T00:00:00Z", "g"),
        );
        write(d.path(), "catalog-bad.json", "{ not json");
        let (runs, errors) = list_runs(d.path());
        assert_eq!(runs.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("catalog-bad.json"));
    }

    #[test]
    fn missing_fields_become_none_not_panic() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "catalog-m.json",
            r#"{"videos":[{"title":"Only a title"},{"rank":"x","quality":5}]}"#,
        );
        let (runs, errors) = list_runs(d.path());
        assert!(errors.is_empty());
        let s = &runs[0].sources[0];
        assert_eq!(s.title, "Only a title");
        assert_eq!(s.score, None);
        assert_eq!(s.grade, None);
        assert_eq!(s.duration_s, None);
        assert!(!s.handoff_ready);
        assert!(s.segments.is_empty());
        assert_eq!(runs[0].sources[1].score, None);
    }

    #[test]
    fn ignores_catalog_index_json() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "catalog-index.json", r#"{"runs":[]}"#);
        write(d.path(), "notes.json", "{}");
        let (runs, errors) = list_runs(d.path());
        assert!(runs.is_empty() && errors.is_empty());
    }

    #[test]
    fn scout_root_is_where_scout_jobs_run() {
        let home = Path::new("/home/u");
        assert_eq!(scout_root(home), PathBuf::from("/home/u/.video-scout"));
        assert_eq!(scout_root(home), crate::creator::spawn::scout_dir(home));
    }
}

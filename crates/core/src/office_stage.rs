//! What stage each colleague in the Office is at, from the event log and the
//! waiting items. Pure: the caller reads the events and counts the waits.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Classifying,
    AwaitingSpec,
    Coding,
    Verifying,
    Reviewing,
    SecurityScan,
    AwaitingYou,
    Blocked,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Classifying => "classifying",
            Stage::AwaitingSpec => "awaiting_spec",
            Stage::Coding => "coding",
            Stage::Verifying => "verifying",
            Stage::Reviewing => "reviewing",
            Stage::SecurityScan => "security_scan",
            Stage::AwaitingYou => "awaiting_you",
            Stage::Blocked => "blocked",
        }
    }

    /// The English label; the server translates it.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Classifying => "Classifying",
            Stage::AwaitingSpec => "Awaiting spec approval",
            Stage::Coding => "Coding",
            Stage::Verifying => "Verifying (tests)",
            Stage::Reviewing => "Reviewing",
            Stage::SecurityScan => "Security scan",
            Stage::AwaitingYou => "Awaiting you",
            Stage::Blocked => "Blocked",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveStage {
    pub stage: Stage,
    /// `web #12`, or empty when no event of the run names its issue.
    pub scope: String,
    pub at: String,
}

const TERMINAL: [&str; 4] = [
    "issue.completed",
    "issue.escalated",
    "issue.failed",
    "issue.agent_failed",
];

fn stage_of(event: &Value) -> Option<Stage> {
    let kind = event.get("event_type").and_then(Value::as_str)?;
    Some(match kind {
        "issue.queued" | "issue.started" => Stage::Classifying,
        "issue.classified"
        | "iteration.started"
        | "verification.failed"
        | "approval.revision_started" => Stage::Coding,
        "verification.started" => Stage::Verifying,
        "verification.check_started" => {
            match event.pointer("/data/verifier").and_then(Value::as_str) {
                Some("agent_review") => Stage::Reviewing,
                Some("security_scan" | "dependency_audit") => Stage::SecurityScan,
                _ => Stage::Verifying,
            }
        }
        "approval.requested" => Stage::AwaitingYou,
        "setup.blocked" => Stage::Blocked,
        _ => return None,
    })
}

fn group_of(event: &Value) -> Option<String> {
    let id = |k: &str| {
        event
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    id("issue_run_id").or_else(|| id("run_id"))
}

fn timestamp(event: &Value) -> String {
    event
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn scope_of(event: &Value) -> Option<String> {
    let project = event
        .get("project")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())?;
    let iid = match event.get("issue_iid")? {
        Value::Number(n) => n.to_string(),
        Value::String(s) if !s.is_empty() => s.clone(),
        _ => return None,
    };
    Some(format!("{project} #{iid}"))
}

/// The stage of every issue run that has not finished, newest first. Events
/// are grouped by `issue_run_id`, or `run_id` for the runtime's own events
/// (whose run id is the issue run id).
pub fn issue_stages(events: &[Value]) -> Vec<ActiveStage> {
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for e in events {
        if let Some(g) = group_of(e) {
            groups.entry(g).or_default().push(e);
        }
    }
    let mut out = Vec::new();
    for evs in groups.values() {
        let mut sorted = evs.clone();
        sorted.sort_by_key(|e| timestamp(e));
        let scope = sorted
            .iter()
            .rev()
            .find_map(|e| scope_of(e))
            .unwrap_or_default();
        let terminal = sorted.iter().rev().find(|e| {
            e.get("event_type")
                .and_then(Value::as_str)
                .is_some_and(|t| TERMINAL.contains(&t))
        });
        if let Some(t) = terminal {
            let spec = t["event_type"] == "issue.completed"
                && t.pointer("/data/action") == Some(&Value::from("spec"));
            if spec {
                out.push(ActiveStage {
                    stage: Stage::AwaitingSpec,
                    scope,
                    at: timestamp(t),
                });
            }
            continue;
        }
        if let Some((stage, e)) = sorted
            .iter()
            .rev()
            .find_map(|e| stage_of(e).map(|s| (s, e)))
        {
            out.push(ActiveStage {
                stage,
                scope,
                at: timestamp(e),
            });
        }
    }
    out.sort_by(|a, b| b.at.cmp(&a.at));
    out
}

/// When the newest issue-loop run started: its `run.started` event, which only
/// the issue loop emits and nothing rewrites mid-run.
pub fn latest_run_start(events: &[Value]) -> Option<String> {
    events
        .iter()
        .filter(|e| e.get("event_type").and_then(Value::as_str) == Some("run.started"))
        .map(timestamp)
        .filter(|t| !t.is_empty())
        .max_by_key(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
}

/// The stages whose newest event is from the current run: an issue run the
/// previous run left without a final event (it was killed) is not in flight.
/// An unreadable start keeps them all.
pub fn since_run_start(active: &[ActiveStage], started_at: &str) -> Vec<ActiveStage> {
    let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
    let Some(start) = parse(started_at) else {
        return active.to_vec();
    };
    active
        .iter()
        .filter(|a| parse(&a.at).is_none_or(|at| at >= start))
        .cloned()
        .collect()
}

/// The one stage to show for a loop. A running loop shows its newest working
/// stage (scope `+N` when more issues are in flight); otherwise what it waits on.
pub fn loop_stage(
    running: bool,
    active: &[ActiveStage],
    awaiting_spec: usize,
    needs_you: usize,
    blocked: bool,
) -> Option<(Stage, String)> {
    if running {
        let working: Vec<&ActiveStage> = active
            .iter()
            .filter(|a| a.stage != Stage::AwaitingSpec)
            .collect();
        if let Some(newest) = working.first() {
            let scope = match working.len() {
                1 => newest.scope.clone(),
                n => format!("{} +{}", newest.scope, n - 1).trim().to_string(),
            };
            return Some((newest.stage, scope));
        }
    }
    if needs_you > 0 {
        return Some((Stage::AwaitingYou, String::new()));
    }
    if awaiting_spec > 0 {
        return Some((Stage::AwaitingSpec, String::new()));
    }
    blocked.then(|| (Stage::Blocked, String::new()))
}

/// How many specs are waiting for approval under `memory_root`.
pub fn awaiting_specs(memory_root: &Path) -> usize {
    crate::spec_gate::awaiting(memory_root).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn issue(kind: &str, run: &str, iid: i64, at: &str, data: Value) -> Value {
        json!({"event_type": kind, "run_id": "R", "issue_run_id": run, "project": "web",
               "issue_iid": iid, "timestamp": at, "data": data})
    }

    fn runtime(kind: &str, run: &str, at: &str, data: Value) -> Value {
        json!({"event_type": kind, "run_id": run, "issue_run_id": null, "project": null,
               "issue_iid": null, "timestamp": at, "data": data})
    }

    fn one(events: Vec<Value>) -> Option<Stage> {
        let s = issue_stages(&events);
        assert!(s.len() <= 1, "{s:?}");
        s.first().map(|a| a.stage)
    }

    #[test]
    fn each_event_maps_to_its_stage() {
        let q = issue(
            "issue.queued",
            "R_web_12",
            12,
            "2026-11-22T10:00:00.000Z",
            json!({}),
        );
        let cases = [
            (
                issue(
                    "issue.started",
                    "R_web_12",
                    12,
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::Classifying,
            ),
            (
                issue(
                    "issue.classified",
                    "R_web_12",
                    12,
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::Coding,
            ),
            (
                runtime(
                    "iteration.started",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::Coding,
            ),
            (
                runtime(
                    "verification.started",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::Verifying,
            ),
            (
                runtime(
                    "verification.check_started",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({"verifier": "project_commands"}),
                ),
                Stage::Verifying,
            ),
            (
                runtime(
                    "verification.check_started",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({"verifier": "agent_review"}),
                ),
                Stage::Reviewing,
            ),
            (
                runtime(
                    "verification.check_started",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({"verifier": "security_scan"}),
                ),
                Stage::SecurityScan,
            ),
            (
                runtime(
                    "verification.check_started",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({"verifier": "dependency_audit"}),
                ),
                Stage::SecurityScan,
            ),
            (
                runtime(
                    "verification.failed",
                    "R_web_12",
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::Coding,
            ),
            (
                issue(
                    "approval.requested",
                    "R_web_12",
                    12,
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::AwaitingYou,
            ),
            (
                issue(
                    "setup.blocked",
                    "R_web_12",
                    12,
                    "2026-11-22T10:00:01.000Z",
                    json!({}),
                ),
                Stage::Blocked,
            ),
        ];
        assert_eq!(one(vec![q.clone()]), Some(Stage::Classifying));
        for (e, want) in cases {
            let got = issue_stages(&[q.clone(), e.clone()]);
            assert_eq!(got.len(), 1, "{e}");
            assert_eq!(got[0].stage, want, "{e}");
            assert_eq!(got[0].scope, "web #12");
            assert_eq!(got[0].at, "2026-11-22T10:00:01.000Z");
        }
        let labels: Vec<_> = [
            Stage::Classifying,
            Stage::AwaitingSpec,
            Stage::Coding,
            Stage::Verifying,
            Stage::Reviewing,
            Stage::SecurityScan,
            Stage::AwaitingYou,
            Stage::Blocked,
        ]
        .iter()
        .map(|s| (s.as_str(), serde_json::to_value(s).unwrap()))
        .collect();
        for (key, wire) in labels {
            assert_eq!(wire, json!(key));
        }
    }

    #[test]
    fn finished_issue_runs_have_no_stage() {
        for terminal in [
            "issue.completed",
            "issue.escalated",
            "issue.failed",
            "issue.agent_failed",
        ] {
            let events = vec![
                issue(
                    "issue.queued",
                    "R_web_12",
                    12,
                    "2026-11-22T10:00:00.000Z",
                    json!({}),
                ),
                runtime(
                    "verification.check_started",
                    "R_web_12",
                    "2026-11-22T10:00:05.000Z",
                    json!({"verifier": "agent_review"}),
                ),
                issue(
                    terminal,
                    "R_web_12",
                    12,
                    "2026-11-22T10:00:09.000Z",
                    json!({"action": "fix"}),
                ),
            ];
            assert_eq!(one(events), None, "{terminal}");
        }
        assert_eq!(
            loop_stage(true, &[], 0, 0, false),
            None,
            "a running loop with nothing active is not 'Reviewing'"
        );
    }

    #[test]
    fn spec_handoff_is_awaiting_spec() {
        let events = vec![
            issue(
                "issue.queued",
                "R_web_12",
                12,
                "2026-11-22T10:00:00.000Z",
                json!({}),
            ),
            issue(
                "issue.completed",
                "R_web_12",
                12,
                "2026-11-22T10:01:00.000Z",
                json!({"action": "spec"}),
            ),
        ];
        let s = issue_stages(&events);
        assert_eq!(s.len(), 1);
        assert_eq!(
            (s[0].stage, s[0].scope.as_str()),
            (Stage::AwaitingSpec, "web #12")
        );
    }

    #[test]
    fn parallel_issues_newest_wins_with_count() {
        let events = vec![
            issue(
                "issue.queued",
                "R_web_12",
                12,
                "2026-11-22T10:00:00.000Z",
                json!({}),
            ),
            runtime(
                "iteration.started",
                "R_web_12",
                "2026-11-22T10:00:02.000Z",
                json!({}),
            ),
            issue(
                "issue.queued",
                "R_web_13",
                13,
                "2026-11-22T10:00:00.000Z",
                json!({}),
            ),
            runtime(
                "verification.check_started",
                "R_web_13",
                "2026-11-22T10:00:05.000Z",
                json!({"verifier": "security_scan"}),
            ),
            issue(
                "issue.queued",
                "R_web_14",
                14,
                "2026-11-22T10:00:01.000Z",
                json!({}),
            ),
            json!({"event_type": "run.started", "run_id": "R", "timestamp": "2026-11-22T09:59:59.000Z", "data": {}}),
        ];
        let active = issue_stages(&events);
        assert_eq!(active.len(), 3);
        assert_eq!(active[0].scope, "web #13");
        assert_eq!(
            loop_stage(true, &active, 0, 0, false),
            Some((Stage::SecurityScan, "web #13 +2".to_string()))
        );
        assert_eq!(
            loop_stage(true, &active[1..2], 0, 0, false),
            Some((Stage::Coding, "web #12".to_string()))
        );
    }

    #[test]
    fn spec_awaiting_is_the_stage_when_nothing_runs() {
        assert_eq!(
            loop_stage(false, &[], 1, 0, false),
            Some((Stage::AwaitingSpec, String::new()))
        );
        let stale = vec![ActiveStage {
            stage: Stage::Reviewing,
            scope: "web #12".into(),
            at: "2026-11-22T10:00:00.000Z".into(),
        }];
        assert_eq!(
            loop_stage(false, &stale, 1, 0, false),
            Some((Stage::AwaitingSpec, String::new())),
            "an idle loop never shows an old working stage"
        );
        assert_eq!(
            loop_stage(false, &[], 0, 0, true),
            Some((Stage::Blocked, String::new()))
        );
        assert_eq!(loop_stage(false, &[], 0, 0, false), None);
        let d = tempfile::tempdir().unwrap();
        assert_eq!(awaiting_specs(d.path()), 0);
        let state = crate::spec_gate::SpecState {
            status: crate::spec_gate::SpecStatus::Awaiting,
            spec: "s".into(),
            posted_at: "2026-11-22T10:00:00Z".into(),
            decided_at: None,
            decided_by: None,
            notes: String::new(),
        };
        crate::spec_gate::save(d.path(), "web", "12", &state).unwrap();
        let approved = crate::spec_gate::SpecState {
            status: crate::spec_gate::SpecStatus::Approved,
            ..state
        };
        crate::spec_gate::save(d.path(), "web", "13", &approved).unwrap();
        assert_eq!(awaiting_specs(d.path()), 1);
    }

    #[test]
    fn running_beats_waiting() {
        let active = vec![ActiveStage {
            stage: Stage::Coding,
            scope: "web #12".into(),
            at: "2026-11-22T10:00:00.000Z".into(),
        }];
        assert_eq!(
            loop_stage(true, &active, 2, 3, true),
            Some((Stage::Coding, "web #12".to_string()))
        );
        assert_eq!(
            loop_stage(false, &active, 2, 3, true),
            Some((Stage::AwaitingYou, String::new()))
        );
        let only_spec = vec![ActiveStage {
            stage: Stage::AwaitingSpec,
            scope: "web #12".into(),
            at: "2026-11-22T10:00:00.000Z".into(),
        }];
        assert_eq!(
            loop_stage(true, &only_spec, 1, 0, false),
            Some((Stage::AwaitingSpec, String::new()))
        );
    }

    #[test]
    fn unknown_verifier_names_fall_back_to_verifying() {
        for verifier in [json!("lint"), json!(""), Value::Null] {
            let events = vec![runtime(
                "verification.check_started",
                "R_web_12",
                "2026-11-22T10:00:00.000Z",
                json!({"verifier": verifier}),
            )];
            let s = issue_stages(&events);
            assert_eq!(s[0].stage, Stage::Verifying);
            assert_eq!(s[0].scope, "", "a runtime-only run names no issue");
        }
    }

    #[test]
    fn issue_runs_left_by_an_earlier_killed_run_are_not_in_flight() {
        let at = |t: &str| ActiveStage {
            stage: Stage::Coding,
            scope: "web #12".into(),
            at: t.into(),
        };
        let active = [
            at("2026-11-22T10:05:00.000Z"),
            at("2026-11-22T09:00:00.000Z"),
        ];
        let current = since_run_start(&active, "2026-11-22T10:00:00.000000+00:00");
        assert_eq!(current, [active[0].clone()]);
        assert_eq!(since_run_start(&active, "not a time"), active);
    }

    #[test]
    fn the_run_start_is_the_newest_run_started_event() {
        let events = [
            runtime("run.started", "a", "2026-11-22T09:00:00.000Z", json!({})),
            issue(
                "issue.classified",
                "b_web_12",
                12,
                "2026-11-22T10:30:00.000Z",
                json!({}),
            ),
            runtime("run.started", "b", "2026-11-22T10:00:00.000Z", json!({})),
        ];
        assert_eq!(
            latest_run_start(&events).as_deref(),
            Some("2026-11-22T10:00:00.000Z")
        );
        assert_eq!(latest_run_start(&events[1..2]), None);
    }
}

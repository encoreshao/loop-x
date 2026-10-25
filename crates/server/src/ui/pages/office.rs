//! `GET /office/state`: the JSON the office view polls, one entry per loop.

use crate::ui::office;
use crate::ui::routes::{Registry, UiCtx, UiResponse};

pub fn register(r: &mut Registry) {
    r.get("/office/state", state_json);
}

fn state_json(ctx: &UiCtx) -> UiResponse {
    UiResponse::json(200, &office::build_office_state(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use crate::ui::state::UiState;
    use loopx_core::approvals::{self, NewApproval};
    use serde_json::{json, Value};

    struct Fx {
        dir: tempfile::TempDir,
        state: UiState,
    }

    fn fx(loops: Value) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("loops.json"), loops.to_string()).unwrap();
        let state = UiState::explicit(dir.path(), &home, dir.path(), dir.path(), "tok", None);
        Fx { dir, state }
    }

    impl Fx {
        fn status(&self, name: &str, doc: &str) {
            let p = self.state.status_path_for_loop(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, doc).unwrap();
        }

        fn approval(&self) {
            let new = NewApproval {
                kind: "merge_request".into(),
                run_id: "r".into(),
                project: "p".into(),
                issue_iid: Some(1),
                title: "t".into(),
                branch: "b".into(),
                target_branch: "main".into(),
                summary: "s".into(),
                timeout_seconds: 3600,
            };
            approvals::create(&self.dir.path().join("outputs").join("approvals"), &new, chrono::Utc::now()).unwrap();
        }

        fn get(&self, lang: Option<&str>) -> Value {
            let mut req = UiRequest::new("GET".parse().unwrap(), "/office/state");
            if let Some(l) = lang {
                req = req.with_header("accept-language", l);
            }
            let resp = Registry::with_pages().dispatch(&self.state, &req).expect("route");
            assert_eq!(resp.status, 200);
            serde_json::from_str(&resp.body_text()).unwrap()
        }
    }

    fn four() -> Value {
        json!([
            {"name": "gitlab-loop", "enabled": true},
            {"name": "off-loop", "enabled": false},
            {"name": "busy-loop", "enabled": true},
            {"name": "never-loop", "enabled": true},
        ])
    }

    fn by<'a>(doc: &'a Value, name: &str) -> &'a Value {
        doc["loops"].as_array().unwrap().iter().find(|l| l["name"] == name).unwrap()
    }

    fn running_now() -> String {
        format!(r#"{{"state":"running","pid":{},"updated_at":"2026-01-01T00:00:00Z"}}"#, std::process::id())
    }

    #[test]
    fn state_json_has_one_entry_per_registered_loop() {
        let f = fx(four());
        f.status("off-loop", r#"{"state":"running","pid":999999999}"#);
        f.status("busy-loop", &running_now());
        f.status("gitlab-loop", r#"{"state":"failed","updated_at":"2026-01-02T00:00:00Z"}"#);
        let doc = f.get(None);
        let names: Vec<&str> = doc["loops"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(by(&doc, "busy-loop")["state"], "working");
        assert_eq!(by(&doc, "gitlab-loop")["state"], "failed");
        assert_eq!(by(&doc, "gitlab-loop")["last_run"], "2026-01-02T00:00:00Z");
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
        assert!(by(&doc, "never-loop")["last_run"].is_null());
        assert_eq!(by(&doc, "busy-loop")["run_now_url"], "/loops/busy-loop/run-now");
        assert_eq!(by(&doc, "gitlab-loop")["run_now_url"], "/run-now");
        assert_eq!(by(&doc, "busy-loop")["toggle_url"], "/daemons/loops/busy-loop/disable");
        assert_eq!(by(&doc, "off-loop")["toggle_url"], "/daemons/loops/off-loop/enable");
        assert!(doc["generated_at"].is_string());
    }

    #[test]
    fn disabled_loop_with_running_status_is_disabled() {
        let f = fx(four());
        f.status("off-loop", &running_now());
        let doc = f.get(None);
        assert_eq!(by(&doc, "off-loop")["state"], "disabled");
        assert_eq!(by(&doc, "off-loop")["enabled"], false);
    }

    #[test]
    fn pending_approval_makes_gitlab_loop_waiting() {
        let f = fx(four());
        f.approval();
        let doc = f.get(None);
        assert_eq!(by(&doc, "gitlab-loop")["state"], "waiting");
        assert_eq!(by(&doc, "gitlab-loop")["pending_approvals"], 1);
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
        assert_eq!(doc["pending_approvals"], 1);
    }

    #[test]
    fn pending_approval_without_gitlab_loop_changes_nobody_and_is_counted() {
        let f = fx(json!([{"name": "a-loop", "enabled": true}]));
        f.approval();
        let doc = f.get(None);
        assert_eq!(by(&doc, "a-loop")["state"], "idle");
        assert_eq!(by(&doc, "a-loop")["pending_approvals"], 0);
        assert_eq!(doc["pending_approvals"], 1);
    }

    #[test]
    fn corrupt_or_missing_status_is_idle() {
        let f = fx(four());
        f.status("busy-loop", "{not json");
        let doc = f.get(None);
        assert_eq!(by(&doc, "busy-loop")["state"], "idle");
        assert_eq!(by(&doc, "never-loop")["state"], "idle");
    }

    #[test]
    fn names_with_slashes_quotes_and_non_ascii_are_encoded_in_urls() {
        let f = fx(json!([{"name": "a/b'c é", "enabled": true}]));
        let doc = f.get(None);
        let l = by(&doc, "a/b'c é");
        assert_eq!(l["run_now_url"], "/loops/a%2Fb%27c%20%C3%A9/run-now");
        assert_eq!(l["toggle_url"], "/daemons/loops/a%2Fb%27c%20%C3%A9/disable");
    }

    #[test]
    fn available_loop_with_no_status_file_is_listed_as_disabled() {
        let f = fx(json!([{"name": "idle-one", "enabled": false}]));
        let doc = f.get(None);
        assert_eq!(doc["loops"].as_array().unwrap().len(), 1);
        assert_eq!(by(&doc, "idle-one")["state"], "disabled");
        assert!(by(&doc, "idle-one")["last_run"].is_null());
    }

    #[test]
    fn urls_for_hostile_names_are_percent_encoded() {
        let name = "a/b\"c<d é";
        let f = fx(json!([{"name": name, "enabled": true}]));
        let doc = f.get(None);
        let l = by(&doc, name);
        for key in ["run_now_url", "toggle_url"] {
            let u = l[key].as_str().unwrap();
            let tail = u.rsplit_once('/').unwrap().0;
            let seg = tail.rsplit_once('/').unwrap().1;
            assert!(!seg.contains(['"', '<', '/', ' ']) && seg.is_ascii(), "{u}");
            assert!(seg.contains("%2F") && seg.contains("%22") && seg.contains("%3C"), "{u}");
        }
        let toggle = l["toggle_url"].as_str().unwrap();
        let routed = Registry::with_pages().pattern_for(&"POST".parse().unwrap(), toggle);
        assert_eq!(routed, Some("/daemons/loops/:name/disable"));
    }

    #[test]
    fn empty_registry_gives_empty_list() {
        let f = fx(json!([]));
        let doc = f.get(None);
        assert_eq!(doc["loops"], json!([]));
        assert_eq!(doc["pending_approvals"], 0);
    }

    #[test]
    fn reason_is_translated() {
        let f = fx(four());
        assert_eq!(by(&f.get(None), "never-loop")["reason"], "Idle until the next run");
        assert_ne!(by(&f.get(Some("fr")), "never-loop")["reason"], "Idle until the next run");
    }
}

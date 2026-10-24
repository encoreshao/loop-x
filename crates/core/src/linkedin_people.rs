//! The people the LinkedIn loop has found through the header search (mirrors
//! `bin/linkedin_people.py`): one JSON list the dashboard shows for review,
//! per-checkout run state keyed by profile URL. The store itself lives with
//! the plugin (`loop_plugins::linkedin_autopilot::people`); this module is the
//! stable path for the rest of the browser stack and carries the tests ported
//! from `tests/test_linkedin_people.py`.

pub use crate::loop_plugins::linkedin_autopilot::people::{
    default_path, known_urls, load, merge, target, DEFAULT_TARGET, MAX_STORED, MAX_TARGET,
    MIN_TARGET,
};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::{json, Value};

    fn row() -> Value {
        json!({"name": "Ada", "url": "https://www.linkedin.com/in/ada", "avatar": "", "company": "Acme", "position": "Eng"})
    }

    #[test]
    fn merge_adds_new_people_and_reports_how_many_were_new() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("people.json");
        let now = Utc.with_ymd_and_hms(2026, 10, 22, 10, 0, 0).unwrap();
        let mut bob = row();
        bob["url"] = json!("https://www.linkedin.com/in/bob");
        bob["name"] = json!("Bob");
        assert_eq!(merge(&[row(), bob], "AI builder", now, &p).unwrap(), 2);
        assert_eq!(merge(&[row()], "AI builder", now, &p).unwrap(), 0);
        assert_eq!(load(&p).len(), 2);
    }

    #[test]
    fn known_urls_and_update_keeps_first_seen() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("people.json");
        let t1 = Utc.with_ymd_and_hms(2026, 10, 22, 10, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap();
        merge(&[row()], "q", t1, &p).unwrap();
        let mut cto = row();
        cto["position"] = json!("CTO");
        merge(&[cto], "q", t2, &p).unwrap();
        let rows = load(&p);
        assert_eq!(rows.len(), 1);
        assert!(known_urls(&p).contains("https://www.linkedin.com/in/ada"));
        assert_eq!(rows[0]["position"], "CTO");
        assert!(rows[0]["first_seen"].as_str().unwrap().starts_with("2026-10-22"));
        assert!(rows[0]["last_seen"].as_str().unwrap().starts_with("2026-10-23"));
    }

    #[test]
    fn load_survives_a_corrupt_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("people.json");
        std::fs::write(&p, "{nope").unwrap();
        assert!(load(&p).is_empty() && known_urls(&p).is_empty());
    }

    #[test]
    fn target_is_clamped_to_20_50() {
        let t = |s: Option<&str>| target(s.map(|s| json!(s)).as_ref());
        assert_eq!(
            (t(None), t(Some("5")), t(Some("99")), t(Some("35")), t(Some("x"))),
            (30, 20, 50, 35, 30)
        );
    }
}

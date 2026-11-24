//! Which kind of work a loop belongs to, for grouping on the All loops page.

use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Area {
    Code,
    Communication,
    Research,
    Other,
}

pub const ORDER: [Area; 4] = [Area::Code, Area::Communication, Area::Research, Area::Other];

impl Area {
    pub fn label(self) -> &'static str {
        match self {
            Area::Code => "Code",
            Area::Communication => "Communication",
            Area::Research => "Research & content",
            Area::Other => "Other",
        }
    }

    fn from_key(key: &str) -> Option<Area> {
        match key {
            "code" => Some(Area::Code),
            "communication" => Some(Area::Communication),
            "research" => Some(Area::Research),
            _ => None,
        }
    }
}

/// The registry entry's `area`, else the bundled loop's, else Other.
pub fn area_of(name: &str, entry: Option<&Value>) -> Area {
    if let Some(key) = entry.and_then(|e| e.get("area")).and_then(Value::as_str) {
        return Area::from_key(key).unwrap_or(Area::Other);
    }
    match name {
        "issue-loop"
        | "mr-review-loop"
        | "pipeline-doctor-loop"
        | "mr-autofix-loop"
        | "release-notes-loop"
        | "stale-sweeper-loop" => Area::Code,
        "inbox-triage-loop" | "meeting-prep-loop" | "daily-digest-loop" => Area::Communication,
        "topic-loop" | "rss-watch-loop" | "linkedin-autopilot-loop" => Area::Research,
        _ => Area::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bundled_loops_map_to_their_areas() {
        for name in [
            "issue-loop",
            "mr-review-loop",
            "pipeline-doctor-loop",
            "mr-autofix-loop",
            "release-notes-loop",
            "stale-sweeper-loop",
        ] {
            assert_eq!(area_of(name, None), Area::Code, "{name}");
        }
        for name in [
            "inbox-triage-loop",
            "meeting-prep-loop",
            "daily-digest-loop",
        ] {
            assert_eq!(area_of(name, None), Area::Communication, "{name}");
        }
        for name in ["topic-loop", "rss-watch-loop", "linkedin-autopilot-loop"] {
            assert_eq!(area_of(name, None), Area::Research, "{name}");
        }
    }

    #[test]
    fn registry_area_overrides_the_map() {
        let entry = json!({"name": "mr-review-loop", "area": "research"});
        assert_eq!(area_of("mr-review-loop", Some(&entry)), Area::Research);
    }

    #[test]
    fn unknown_or_missing_area_is_other() {
        assert_eq!(area_of("my-loop", None), Area::Other);
        let entry = json!({"name": "my-loop", "area": "weird"});
        assert_eq!(area_of("my-loop", Some(&entry)), Area::Other);
    }

    #[test]
    fn areas_are_listed_in_display_order_with_their_labels() {
        let labels: Vec<&str> = ORDER.iter().map(|a| a.label()).collect();
        assert_eq!(
            labels,
            ["Code", "Communication", "Research & content", "Other"]
        );
    }
}

//! The LoopKit plugins (mirrors `bin/loop_plugins/`) and the registry that maps
//! a registry entry point (`bin.loop_plugins.rss_watch`, `rss-watch-loop`, ...)
//! to a ready-to-run plugin.

pub mod calendar_prep;
pub mod daily_digest;
pub mod linkedin_autopilot;
pub mod mr_review;
pub mod pipeline_doctor;
pub mod release_notes;
pub mod rss_watch;
pub mod stale_sweeper;

use crate::connectors::ConnectorSource;
use crate::loopkit::LoopPlugin;
use std::sync::Arc;

/// `(module name, loop name)` of every plugin, in Python's alphabetical order.
pub const PLUGINS: [(&str, &str); 8] = [
    ("calendar_prep", "meeting-prep-loop"),
    ("daily_digest", "daily-digest-loop"),
    ("linkedin_autopilot", "linkedin-autopilot-loop"),
    ("mr_review", "mr-review-loop"),
    ("pipeline_doctor", "pipeline-doctor-loop"),
    ("release_notes", "release-notes-loop"),
    ("rss_watch", "rss-watch-loop"),
    ("stale_sweeper", "stale-sweeper-loop"),
];

/// The plugin module name for an entry point: `bin.loop_plugins.<mod>`,
/// `loop_plugins.<mod>`, a bare module name, or the plugin's loop name.
pub fn module_for_entry(entry_point: &str) -> Option<&'static str> {
    let tail = entry_point
        .strip_prefix("bin.loop_plugins.")
        .or_else(|| entry_point.strip_prefix("loop_plugins."))
        .unwrap_or(entry_point);
    PLUGINS
        .iter()
        .find(|(module, loop_name)| *module == tail || *loop_name == tail)
        .map(|(module, _)| *module)
}

/// The plugin for `module`, wired to `connectors`. `None` when the plugin
/// can't run natively yet (LinkedIn without a registered browser).
pub fn build(module: &str, connectors: Arc<dyn ConnectorSource>) -> Option<Arc<dyn LoopPlugin>> {
    Some(match module {
        "calendar_prep" => Arc::new(calendar_prep::CalendarPrep::new(connectors)),
        "daily_digest" => Arc::new(daily_digest::DailyDigest::new(connectors)),
        "mr_review" => Arc::new(mr_review::MrReview::new(connectors)),
        "pipeline_doctor" => Arc::new(pipeline_doctor::PipelineDoctor::new(connectors)),
        "release_notes" => Arc::new(release_notes::ReleaseNotes::new(connectors)),
        "rss_watch" => Arc::new(rss_watch::RssWatch::new(connectors)),
        "stale_sweeper" => Arc::new(stale_sweeper::StaleSweeper::new(connectors)),
        "linkedin_autopilot" => return linkedin_autopilot::build(connectors),
        _ => return None,
    })
}

/// Run the plugin behind `entry_point` natively (`loopkit::main`): `argv` is
/// `<run_id> [--force]`. `None` when the entry point is not a plugin, or the
/// plugin cannot run natively yet (the caller then falls back to Python).
pub fn run_entry(entry_point: &str, argv: &[String]) -> Option<i32> {
    let module = module_for_entry(entry_point)?;
    let connectors = Arc::new(crate::connectors_config::ConfigConnectors::real());
    let plugin = build(module, connectors)?;
    Some(crate::loopkit::main(
        plugin,
        argv,
        crate::loopkit::RunOptions::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_points_resolve_by_module_path_or_loop_name() {
        assert_eq!(
            module_for_entry("bin.loop_plugins.rss_watch"),
            Some("rss_watch")
        );
        assert_eq!(module_for_entry("rss-watch-loop"), Some("rss_watch"));
        assert_eq!(module_for_entry("meeting-prep-loop"), Some("calendar_prep"));
        assert_eq!(module_for_entry("bin.gitlab_loop_runner"), None);
    }
}

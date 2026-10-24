//! The loop definitions and agent instructions the built-in runners read from
//! `<root>/loops/..` and `<root>/instructions/loopx/..`. A checkout has them on
//! disk; an installed binary (DMG) runs against `~/.loopx`, which has neither,
//! so the binary carries a copy and seeds it into the root before a run.

use std::path::Path;

const FILES: &[(&str, &str)] = &[
    (
        "instructions/loopx/gitlab-issue.md",
        include_str!("../../../instructions/loopx/gitlab-issue.md"),
    ),
    (
        "instructions/loopx/inbox-triage.md",
        include_str!("../../../instructions/loopx/inbox-triage.md"),
    ),
    (
        "instructions/loopx/topic-monitor.md",
        include_str!("../../../instructions/loopx/topic-monitor.md"),
    ),
    (
        "loops/calendar-prep/loop.yaml",
        include_str!("../../../loops/calendar-prep/loop.yaml"),
    ),
    (
        "loops/calendar-prep/prompt.md",
        include_str!("../../../loops/calendar-prep/prompt.md"),
    ),
    (
        "loops/daily-digest/loop.yaml",
        include_str!("../../../loops/daily-digest/loop.yaml"),
    ),
    (
        "loops/daily-digest/prompt.md",
        include_str!("../../../loops/daily-digest/prompt.md"),
    ),
    (
        "loops/gitlab-issue/loop.yaml",
        include_str!("../../../loops/gitlab-issue/loop.yaml"),
    ),
    (
        "loops/inbox-triage/loop.yaml",
        include_str!("../../../loops/inbox-triage/loop.yaml"),
    ),
    (
        "loops/linkedin-autopilot/loop.yaml",
        include_str!("../../../loops/linkedin-autopilot/loop.yaml"),
    ),
    (
        "loops/linkedin-autopilot/prompt.md",
        include_str!("../../../loops/linkedin-autopilot/prompt.md"),
    ),
    (
        "loops/mr-review/loop.yaml",
        include_str!("../../../loops/mr-review/loop.yaml"),
    ),
    (
        "loops/mr-review/prompt.md",
        include_str!("../../../loops/mr-review/prompt.md"),
    ),
    (
        "loops/pipeline-doctor/loop.yaml",
        include_str!("../../../loops/pipeline-doctor/loop.yaml"),
    ),
    (
        "loops/pipeline-doctor/prompt.md",
        include_str!("../../../loops/pipeline-doctor/prompt.md"),
    ),
    (
        "loops/release-notes/loop.yaml",
        include_str!("../../../loops/release-notes/loop.yaml"),
    ),
    (
        "loops/release-notes/prompt.md",
        include_str!("../../../loops/release-notes/prompt.md"),
    ),
    (
        "loops/rss-watch/loop.yaml",
        include_str!("../../../loops/rss-watch/loop.yaml"),
    ),
    (
        "loops/rss-watch/prompt.md",
        include_str!("../../../loops/rss-watch/prompt.md"),
    ),
    (
        "loops/stale-sweeper/loop.yaml",
        include_str!("../../../loops/stale-sweeper/loop.yaml"),
    ),
    (
        "loops/topic-monitor/loop.yaml",
        include_str!("../../../loops/topic-monitor/loop.yaml"),
    ),
];

const VERSION_MARKER: &str = ".loopx-bundled-version";

/// Write the bundled files under `root`. A git checkout (`.git` present) is
/// never touched, so development edits stay authoritative. Elsewhere, missing
/// files are created (existing ones are kept) and, after an upgrade (a marker from
/// another version), the files are refreshed so the instructions match the binary. Best effort.
pub fn seed(root: &Path) {
    if root.join(".git").exists() {
        return;
    }
    let version = env!("CARGO_PKG_VERSION");
    let marker = root.join(VERSION_MARKER);
    // No marker means files already there came from an install script or the user:
    // fill in what is missing, never overwrite. Only a marker from another version
    // marks the bundled copies as outdated.
    let recorded = std::fs::read_to_string(&marker).ok();
    let stale = recorded.as_deref().is_some_and(|v| v != version);
    for (rel, text) in FILES {
        let path = root.join(rel);
        if path.exists() && !stale {
            continue;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, text);
    }
    if recorded.as_deref() != Some(version) {
        let _ = std::fs::write(&marker, version);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_loop_definition_and_instruction_in_the_repo_is_bundled() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for dir in ["loops", "instructions/loopx"] {
            for entry in walk(&repo.join(dir)) {
                let rel = entry.strip_prefix(&repo).unwrap().to_str().unwrap();
                assert!(FILES.iter().any(|(r, _)| *r == rel), "{rel} is not bundled");
            }
        }
    }

    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn seeds_an_install_root_that_has_no_loops_or_instructions() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path());
        for rel in [
            "loops/topic-monitor/loop.yaml",
            "instructions/loopx/topic-monitor.md",
        ] {
            assert!(root.path().join(rel).is_file(), "{rel} missing");
        }
        assert!(crate::loop_definition::LoopDefinition::from_yaml(
            &root.path().join("loops/topic-monitor/loop.yaml")
        )
        .is_ok());
    }

    #[test]
    fn refreshes_after_an_upgrade_but_not_within_a_version() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path());
        let md = root.path().join("instructions/loopx/topic-monitor.md");
        std::fs::write(&md, "local edit").unwrap();
        seed(root.path());
        assert_eq!(std::fs::read_to_string(&md).unwrap(), "local edit");
        std::fs::write(root.path().join(VERSION_MARKER), "0.0.0").unwrap();
        seed(root.path());
        assert_ne!(std::fs::read_to_string(&md).unwrap(), "local edit");
    }

    #[test]
    fn a_root_without_a_marker_keeps_the_files_it_already_has() {
        let root = tempfile::tempdir().unwrap();
        let yaml = root.path().join("loops/gitlab-issue/loop.yaml");
        std::fs::create_dir_all(yaml.parent().unwrap()).unwrap();
        std::fs::write(&yaml, "local edit").unwrap();
        seed(root.path());
        assert_eq!(std::fs::read_to_string(&yaml).unwrap(), "local edit");
        assert!(root.path().join("loops/topic-monitor/loop.yaml").is_file());
    }

    #[test]
    fn never_touches_a_git_checkout() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        seed(root.path());
        assert!(!root.path().join("loops").exists());
    }
}

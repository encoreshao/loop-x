//! The frames inspire-video renders a short in (its manifest `layout` field), each with
//! a matching cover. Generate jobs use the one picked in Creator settings.

use std::path::Path;

/// The layout used when the user has not picked one (the skill's default).
pub const DEFAULT: &str = "classic";

/// Every layout the skill knows, in the order the settings sheet shows them.
pub const LAYOUTS: [&str; 6] = ["classic", "backdrop", "cinema", "quote", "split", "banner"];

pub fn is_known(id: &str) -> bool {
    LAYOUTS.contains(&id)
}

/// Whether the installed skill at `engine_root` can render layouts other than its one
/// built-in frame (older copies have no `video/src/layouts/`).
pub fn supported(engine_root: &Path) -> bool {
    engine_root.join("video/src/layouts").is_dir()
}

/// What a settings form's value means: empty or the default is no choice, otherwise it
/// must be a known layout.
pub fn resolve(choice: &str) -> Result<Option<String>, String> {
    match choice.trim() {
        "" | DEFAULT => Ok(None),
        id if is_known(id) => Ok(Some(id.to_string())),
        other => Err(format!("unknown layout: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_accepts_known_layouts_and_folds_the_default_into_none() {
        assert_eq!(resolve(""), Ok(None));
        assert_eq!(resolve("classic"), Ok(None));
        assert_eq!(resolve(" cinema "), Ok(Some("cinema".to_string())));
        assert!(resolve("neon").is_err());
        assert!(resolve("../x").is_err());
    }

    #[test]
    fn supported_only_when_the_skill_ships_layouts() {
        let d = tempfile::tempdir().unwrap();
        assert!(!supported(d.path()));
        std::fs::create_dir_all(d.path().join("video/src/layouts")).unwrap();
        assert!(supported(d.path()));
    }
}

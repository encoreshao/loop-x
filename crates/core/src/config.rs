//! Per-machine project configuration: `~/.loopx/projects.json`.
//! Mirrors `bin/loop_config.py`; see docs/contract/json-files.md.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "No project config at {0}. Copy config/projects.json.template there and fill in your \
         own local checkout paths, GitLab username, and worktree root."
    )]
    Missing(PathBuf),
    #[error("cannot read {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0} is not valid JSON: {1}")]
    Parse(PathBuf, serde_json::Error),
    #[error("{0} is missing key {1:?}")]
    MissingKey(PathBuf, String),
    #[error("unknown project alias {0:?}")]
    UnknownAlias(String),
}

/// The name of the per-machine directory under `$HOME`.
pub const HOME_DIR_NAME: &str = ".loopx";
/// What it was called before the move; kept as a symlink to the new directory so
/// absolute paths already written into configs and launchd plists keep working.
pub const LEGACY_HOME_DIR_NAME: &str = ".loop-engineering";

/// `$LOOP_ENGINEERING_HOME`, else `$HOME/.loopx` (migrating a legacy
/// `$HOME/.loop-engineering` into it the first time, once per process).
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("LOOP_ENGINEERING_HOME") {
        return PathBuf::from(h);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    static MIGRATED: std::sync::Once = std::sync::Once::new();
    MIGRATED.call_once(|| {
        migrate_legacy_home(&home);
    });
    home.join(HOME_DIR_NAME)
}

/// The per-machine directory under `home`, without migrating anything.
pub fn default_home_under(home: &Path) -> PathBuf {
    home.join(HOME_DIR_NAME)
}

/// Moves `<home>/.loop-engineering` to `<home>/.loopx` and leaves a symlink at the
/// old name, but only when the old one is a real directory and the new one does not
/// exist. Never touches anything when both exist. Returns whether it moved.
pub fn migrate_legacy_home(home: &Path) -> bool {
    let old = home.join(LEGACY_HOME_DIR_NAME);
    let new = home.join(HOME_DIR_NAME);
    let old_is_real_dir = std::fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir());
    if !old_is_real_dir || std::fs::symlink_metadata(&new).is_ok() {
        return false;
    }
    if std::fs::rename(&old, &new).is_err() {
        return false;
    }
    // Best effort: without the symlink only configs that hard-code the old path break.
    let _ = std::os::unix::fs::symlink(&new, &old);
    true
}

pub fn default_config_path() -> PathBuf {
    home_dir().join("projects.json")
}

pub struct Config {
    path: PathBuf,
    root: Value,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ConfigError::Missing(path.to_path_buf()),
            _ => ConfigError::Io(path.to_path_buf(), e),
        })?;
        let root =
            serde_json::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))?;
        Ok(Self {
            path: path.to_path_buf(),
            root,
        })
    }

    fn key(&self, name: &str) -> Result<&Value, ConfigError> {
        self.root
            .get(name)
            .ok_or_else(|| ConfigError::MissingKey(self.path.clone(), name.to_string()))
    }

    fn projects(&self) -> Result<&Map<String, Value>, ConfigError> {
        self.key("projects")?
            .as_object()
            .ok_or_else(|| ConfigError::MissingKey(self.path.clone(), "projects".into()))
    }

    fn string(&self, name: &str) -> Result<String, ConfigError> {
        self.key(name)?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ConfigError::MissingKey(self.path.clone(), name.to_string()))
    }

    /// A top-level key of the config file (e.g. `require_approval`).
    pub fn top_level(&self, name: &str) -> Option<&Value> {
        self.root.get(name)
    }

    pub fn aliases(&self) -> Result<Vec<String>, ConfigError> {
        Ok(self.projects()?.keys().cloned().collect())
    }

    pub fn local_paths(&self) -> Result<Vec<String>, ConfigError> {
        self.projects()?
            .values()
            .map(|p| {
                p.get("local_path")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        ConfigError::MissingKey(self.path.clone(), "local_path".to_string())
                    })
            })
            .collect()
    }

    /// A project's own config with `instance` always present: its own override,
    /// else the top-level `gitlab_instance`.
    pub fn project(&self, alias: &str) -> Result<Value, ConfigError> {
        let mut project = self
            .projects()?
            .get(alias)
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| ConfigError::UnknownAlias(alias.to_string()))?;
        if !project.contains_key("instance") {
            project.insert("instance".into(), self.key("gitlab_instance")?.clone());
        }
        Ok(Value::Object(project))
    }

    pub fn instance(&self) -> Result<String, ConfigError> {
        self.string("gitlab_instance")
    }
    pub fn assignee_username(&self) -> Result<String, ConfigError> {
        self.string("assignee_username")
    }
    pub fn worktree_root(&self) -> Result<String, ConfigError> {
        self.string("worktree_root")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("projects.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    const SAMPLE: &str = r#"{"gitlab_instance":"g.example.com","assignee_username":"dev",
        "worktree_root":"/wt","projects":{"b":{"local_path":"/b"},
        "a":{"local_path":"/a","instance":"o.example.com"}}}"#;

    #[test]
    fn aliases_keep_file_order() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&write(d.path(), SAMPLE)).unwrap();
        assert_eq!(c.aliases().unwrap(), vec!["b", "a"]);
        assert_eq!(c.local_paths().unwrap(), vec!["/b", "/a"]);
    }

    #[test]
    fn project_falls_back_to_default_instance() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&write(d.path(), SAMPLE)).unwrap();
        assert_eq!(c.project("b").unwrap()["instance"], "g.example.com");
        assert_eq!(c.project("a").unwrap()["instance"], "o.example.com");
    }

    #[test]
    fn unknown_alias_errors() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&write(d.path(), SAMPLE)).unwrap();
        assert!(matches!(
            c.project("zzz"),
            Err(ConfigError::UnknownAlias(_))
        ));
    }

    #[test]
    fn missing_file_has_helpful_message() {
        let d = tempfile::tempdir().unwrap();
        let err = Config::load(&d.path().join("projects.json")).err().unwrap();
        assert!(err.to_string().starts_with("No project config at"));
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn a_legacy_home_moves_and_leaves_a_symlink_behind() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join(".loop-engineering");
        std::fs::create_dir_all(old.join("worktrees")).unwrap();
        std::fs::write(old.join("projects.json"), "{}").unwrap();
        assert!(migrate_legacy_home(d.path()));
        let new = d.path().join(".loopx");
        assert_eq!(
            std::fs::read_to_string(new.join("projects.json")).unwrap(),
            "{}"
        );
        assert!(std::fs::symlink_metadata(&old)
            .unwrap()
            .file_type()
            .is_symlink());
        // Paths written under the old name still resolve.
        assert!(old.join("projects.json").is_file());
        // A second run is a no-op.
        assert!(!migrate_legacy_home(d.path()));
    }

    #[test]
    fn nothing_moves_when_both_exist_or_neither_does() {
        let d = tempfile::tempdir().unwrap();
        assert!(!migrate_legacy_home(d.path()));
        std::fs::create_dir_all(d.path().join(".loop-engineering")).unwrap();
        std::fs::create_dir_all(d.path().join(".loopx")).unwrap();
        std::fs::write(d.path().join(".loop-engineering/keep"), "x").unwrap();
        assert!(!migrate_legacy_home(d.path()));
        assert!(d.path().join(".loop-engineering/keep").is_file());
    }
}

//! Where the Creator's skills are installed.
//!
//! Both skills install in more than one way, so nothing else in the Creator names a
//! fixed folder for their code:
//! - a Claude Code plugin: a versioned folder under `<config>/plugins/cache/`, named by
//!   that profile's `plugins/installed_plugins.json` and replaced on every update;
//! - by hand: a clone, with links (or the clone itself) under `<config>/skills/`.
//!
//! `<config>` is `~/.claude`, and the profile named by `CLAUDE_CONFIG_DIR` when that
//! folder is inside the user's home. A job only reads the skill's files, so a copy in
//! either profile serves.

use std::path::{Path, PathBuf};

/// The inspire-video repository as installed: `inspire-video` and `inspire-showcase`
/// share its `scripts/`, `video/` and `templates/`.
#[derive(Debug, Clone, PartialEq)]
pub struct Engine {
    /// The folder holding `scripts/` and `skills/`: a job's working directory.
    pub root: PathBuf,
    /// Where `output/` and `voices/` are kept: `root` itself for a clone, the data
    /// folder a plugin copy's `scripts/prepare.sh` links them to otherwise.
    pub data: PathBuf,
    /// A plugin copy: `scripts/prepare.sh` must run before a job to link the data folder
    /// and install what a fresh copy is missing.
    pub plugin: bool,
}

impl Engine {
    /// A skill's file, relative to [`Engine::root`].
    pub fn skill_file(name: &str) -> String {
        format!("skills/{name}/SKILL.md")
    }

    /// Whether this install carries the skill `name`.
    pub fn has_skill(&self, name: &str) -> bool {
        self.root.join(Self::skill_file(name)).is_file()
    }
}

/// Where inspire-video keeps its videos, projects and voices whichever way it is
/// installed (the skill's own default data folder), and where a script install's clone
/// lives.
pub fn data_dir(user_home: &Path) -> PathBuf {
    user_home.join(".inspire-video")
}

/// The Claude Code profiles to look in, the job's own first.
fn config_dirs(user_home: &Path) -> Vec<PathBuf> {
    config_dirs_from(user_home, std::env::var_os("CLAUDE_CONFIG_DIR").as_deref())
}

fn config_dirs_from(user_home: &Path, env: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let default = user_home.join(".claude");
    let mut dirs = Vec::new();
    // Only a profile inside this home: a sandboxed or test home never reads the real one.
    if let Some(dir) = env.map(PathBuf::from) {
        if dir.starts_with(user_home) && dir != default && dir != user_home {
            dirs.push(dir);
        }
    }
    dirs.push(default);
    dirs
}

/// The folder a profile installed the plugin `plugin` into (from any marketplace), as
/// its `plugins/installed_plugins.json` records it.
fn plugin_root(config_dir: &Path, plugin: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_dir.join("plugins/installed_plugins.json")).ok()?;
    let record: serde_json::Value = serde_json::from_str(&text).ok()?;
    let prefix = format!("{plugin}@");
    record
        .get("plugins")?
        .as_object()?
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .filter_map(|(_, installs)| installs.as_array())
        .flatten()
        .filter_map(|install| install.get("installPath")?.as_str())
        .map(PathBuf::from)
        .find(|path| path.is_dir())
}

/// The installed inspire-video repository: the clone in the data folder when it holds
/// the skills (a script install, or a working copy), a plugin copy otherwise.
pub fn engine(user_home: &Path) -> Option<Engine> {
    let data = data_dir(user_home);
    let clone = Engine {
        root: data.clone(),
        data: data.clone(),
        plugin: false,
    };
    if clone.has_skill("inspire-video") {
        return Some(clone);
    }
    config_dirs(user_home)
        .iter()
        .filter_map(|config| plugin_root(config, "inspire-video"))
        .map(|root| Engine {
            root,
            data: data.clone(),
            plugin: true,
        })
        .find(|e| e.has_skill("inspire-video"))
}

/// The folder holding video-scout's `SKILL.md` and `references/`.
pub fn scout_skill(user_home: &Path) -> Option<PathBuf> {
    let nested = "skills/video-scout";
    config_dirs(user_home)
        .iter()
        .flat_map(|config| {
            let by_hand = config.join(nested);
            [
                // A link to the skill folder, or a copy of it.
                Some(by_hand.clone()),
                // A clone of the whole repository, which keeps the skill in `skills/`.
                Some(by_hand.join(nested)),
                plugin_root(config, "video-scout").map(|root| root.join(nested)),
            ]
        })
        .flatten()
        .find(|dir| dir.join("SKILL.md").is_file())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A script install: the clone in `~/.inspire-video`, with both skills or one.
    pub fn install_clone(home: &Path, showcase: bool) {
        let root = data_dir(home);
        write(&root.join(Engine::skill_file("inspire-video")), "x");
        if showcase {
            write(&root.join(Engine::skill_file("inspire-showcase")), "x");
        }
    }

    /// A plugin install of `plugin` in the profile `config`, holding `skills`; returns
    /// its versioned folder.
    pub fn install_plugin(config: &Path, plugin: &str, skills: &[&str]) -> PathBuf {
        let root = config.join(format!("plugins/cache/{plugin}/{plugin}/abc123"));
        for skill in skills {
            write(&root.join(Engine::skill_file(skill)), "x");
        }
        let record = serde_json::json!({"version": 2, "plugins": {
            format!("{plugin}@{plugin}"): [{"scope": "user", "installPath": root}],
            "other@elsewhere": [{"scope": "user", "installPath": config.join("nowhere")}],
        }});
        write(
            &config.join("plugins/installed_plugins.json"),
            &record.to_string(),
        );
        root
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn nothing_installed_finds_nothing() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(engine(home.path()), None);
        assert_eq!(scout_skill(home.path()), None);
        // The data folder alone (videos made earlier) is not an install.
        std::fs::create_dir_all(data_dir(home.path()).join("output/final")).unwrap();
        assert_eq!(engine(home.path()), None);
    }

    #[test]
    fn a_clone_in_the_data_folder_is_its_own_engine_and_data() {
        let home = tempfile::tempdir().unwrap();
        install_clone(home.path(), false);
        let e = engine(home.path()).unwrap();
        assert_eq!(e.root, home.path().join(".inspire-video"));
        assert_eq!((e.data.clone(), e.plugin), (e.root.clone(), false));
        assert!(e.has_skill("inspire-video") && !e.has_skill("inspire-showcase"));
    }

    #[test]
    fn a_plugin_install_runs_from_its_cache_folder_and_keeps_data_in_the_data_folder() {
        let home = tempfile::tempdir().unwrap();
        let root = install_plugin(
            &home.path().join(".claude"),
            "inspire-video",
            &["inspire-video", "inspire-showcase"],
        );
        let e = engine(home.path()).unwrap();
        assert_eq!(e.root, root);
        assert_eq!(e.data, home.path().join(".inspire-video"));
        assert!(e.plugin && e.has_skill("inspire-showcase"));
    }

    #[test]
    fn a_clone_wins_over_a_plugin_copy() {
        let home = tempfile::tempdir().unwrap();
        install_plugin(
            &home.path().join(".claude"),
            "inspire-video",
            &["inspire-video"],
        );
        install_clone(home.path(), true);
        assert!(!engine(home.path()).unwrap().plugin);
    }

    #[test]
    fn a_plugin_recorded_but_gone_from_disk_is_not_installed() {
        let home = tempfile::tempdir().unwrap();
        let root = install_plugin(
            &home.path().join(".claude"),
            "inspire-video",
            &["inspire-video"],
        );
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(engine(home.path()), None);
        write(
            &home.path().join(".claude/plugins/installed_plugins.json"),
            "not json",
        );
        assert_eq!(engine(home.path()), None);
    }

    #[test]
    fn video_scout_is_found_as_a_skill_folder_a_repository_clone_or_a_plugin() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude");
        let plugin = install_plugin(&config, "video-scout", &["video-scout"]);
        assert_eq!(
            scout_skill(home.path()),
            Some(plugin.join("skills/video-scout"))
        );
        // A clone of the repository under ~/.claude/skills keeps the skill one level down.
        let clone = config.join("skills/video-scout");
        write(&clone.join("skills/video-scout/SKILL.md"), "x");
        assert_eq!(
            scout_skill(home.path()),
            Some(clone.join("skills/video-scout"))
        );
        // The skill folder itself (a link or a copy) comes first.
        write(&clone.join("SKILL.md"), "x");
        assert_eq!(scout_skill(home.path()), Some(clone));
    }

    #[test]
    fn the_jobs_own_profile_is_searched_first_but_only_inside_this_home() {
        let home = Path::new("/Users/u");
        let dirs = |env: &str| config_dirs_from(home, Some(std::ffi::OsStr::new(env)));
        assert_eq!(
            dirs("/Users/u/.claude-loopx"),
            [home.join(".claude-loopx"), home.join(".claude")]
        );
        assert_eq!(dirs("/Users/u/.claude"), [home.join(".claude")]);
        assert_eq!(dirs("/Users/other/.claude"), [home.join(".claude")]);
        assert_eq!(config_dirs_from(home, None), [home.join(".claude")]);
    }
}

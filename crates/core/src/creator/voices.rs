//! Narration for showcase videos: the voice a user can pick, and what it needs.
//!
//! The inspire-showcase skill narrates with one of three engines. Loopx offers two:
//! the macOS `say` voice (nothing to install, a quick preview) and the user's own cloned
//! voice profiles under `<skill dir>/voices/` (CosyVoice 3, set up once outside the repo).

use super::Narration;
use std::path::{Path, PathBuf};

/// A voice profile the skill knows (`voices/<name>/meta.json` and `prompt.wav`).
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    /// `en` or `zh`.
    pub language: String,
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Where the skill's `setup_cosyvoice.sh` puts the model: `$INSPIRE_CV_HOME`, else
/// `~/.cache/inspire-video/cosyvoice`.
pub fn cosyvoice_home(user_home: &Path) -> PathBuf {
    match std::env::var_os("INSPIRE_CV_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => user_home.join(".cache/inspire-video/cosyvoice"),
    }
}

/// Whether CosyVoice 3 is set up (its Python environment and the model weights).
pub fn cosyvoice_ready(user_home: &Path) -> bool {
    let home = cosyvoice_home(user_home);
    home.join("venv/bin/python").exists()
        && home
            .join("repo/pretrained_models/Fun-CosyVoice3-0.5B/llm.pt")
            .exists()
}

/// The voice profiles under `skill_dir/voices`, by name. A folder without a reference
/// clip, with a bad name or with an unsupported language is left out (the `source`
/// folder holds the raw recordings, not a profile).
pub fn list(skill_dir: &Path) -> Vec<Profile> {
    let Ok(entries) = std::fs::read_dir(skill_dir.join("voices")) else {
        return Vec::new();
    };
    let mut found: Vec<Profile> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let dir = e.path();
            if !valid_name(&name) || !dir.join("prompt.wav").is_file() {
                return None;
            }
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).ok()?).ok()?;
            let language = meta.get("language")?.as_str()?.to_string();
            matches!(language.as_str(), "en" | "zh").then_some(Profile { name, language })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// What a form's voice value means. `""` and `none` are no narration, `say:en` and
/// `say:zh` the macOS voice, `voice:<name>` one of the listed profiles. A cloned voice
/// is refused unless CosyVoice is set up, so a job does not fail minutes in.
pub fn resolve(
    skill_dir: &Path,
    user_home: &Path,
    choice: &str,
) -> Result<Option<Narration>, String> {
    match choice.trim() {
        "" | "none" => Ok(None),
        "say:en" | "say:zh" => Ok(Some(Narration::Say {
            lang: choice.trim()[4..].to_string(),
        })),
        other => {
            let Some(name) = other.strip_prefix("voice:") else {
                return Err(format!("unknown voice: {other}"));
            };
            let profile = list(skill_dir)
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| format!("no voice profile named {name}"))?;
            if !cosyvoice_ready(user_home) {
                return Err(
                    "cloned voices need CosyVoice 3: run `bash scripts/showcase/setup_cosyvoice.sh` \
                     in ~/.inspire-video first (about 9 GB)"
                        .to_string(),
                );
            }
            Ok(Some(Narration::Cloned {
                name: profile.name,
                lang: profile.language,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(skill: &Path, name: &str, lang: &str, wav: bool) {
        let d = skill.join("voices").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("meta.json"),
            format!(r#"{{"name":"{name}","language":"{lang}"}}"#),
        )
        .unwrap();
        if wav {
            std::fs::write(d.join("prompt.wav"), b"x").unwrap();
        }
    }

    fn ready(home: &Path) {
        let c = home.join(".cache/inspire-video/cosyvoice");
        std::fs::create_dir_all(c.join("venv/bin")).unwrap();
        std::fs::write(c.join("venv/bin/python"), b"x").unwrap();
        let m = c.join("repo/pretrained_models/Fun-CosyVoice3-0.5B");
        std::fs::create_dir_all(&m).unwrap();
        std::fs::write(m.join("llm.pt"), b"x").unwrap();
    }

    #[test]
    fn lists_only_real_profiles_by_name() {
        let d = tempfile::tempdir().unwrap();
        profile(d.path(), "encore", "zh", true);
        profile(d.path(), "encore-en", "en", true);
        profile(d.path(), "no-clip", "en", false);
        profile(d.path(), "Bad Name", "en", true);
        profile(d.path(), "french", "fr", true);
        std::fs::create_dir_all(d.path().join("voices/source")).unwrap();
        assert_eq!(
            list(d.path()),
            [
                Profile {
                    name: "encore".into(),
                    language: "zh".into()
                },
                Profile {
                    name: "encore-en".into(),
                    language: "en".into()
                },
            ]
        );
        assert!(list(&d.path().join("missing")).is_empty());
    }

    #[test]
    fn cosyvoice_is_ready_only_with_its_python_and_weights() {
        let home = tempfile::tempdir().unwrap();
        assert!(!cosyvoice_ready(home.path()));
        ready(home.path());
        assert!(cosyvoice_ready(home.path()));
    }

    #[test]
    fn resolves_each_choice_and_refuses_what_cannot_run() {
        let skill = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        profile(skill.path(), "encore", "zh", true);
        let go = |c: &str| resolve(skill.path(), home.path(), c);
        assert_eq!(go("").unwrap(), None);
        assert_eq!(go("none").unwrap(), None);
        assert_eq!(
            go("say:zh").unwrap(),
            Some(Narration::Say { lang: "zh".into() })
        );
        assert!(go("say:fr").is_err() && go("whisper").is_err() && go("voice:../x").is_err());
        assert!(go("voice:nobody").unwrap_err().contains("no voice profile"));
        // A real profile, but CosyVoice is not installed: refused now, not minutes in.
        assert!(go("voice:encore")
            .unwrap_err()
            .contains("setup_cosyvoice.sh"));
        ready(home.path());
        assert_eq!(
            go("voice:encore").unwrap(),
            Some(Narration::Cloned {
                name: "encore".into(),
                lang: "zh".into()
            })
        );
    }
}

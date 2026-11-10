//! The promo templates the inspire-showcase skill ships: whole-video recipes in
//! `<skill dir>/templates/*.yaml`, each with a cover image and a sample clip.
//!
//! The skill's own `templates.py list --json` is the authority; this reads the same flat
//! top-level keys straight from the files so a page render never starts Python.

use std::path::{Path, PathBuf};

/// The template used when the user has no preference (the skill's default).
pub const DEFAULT: &str = "cinematic";

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub name_zh: String,
    pub description: String,
    pub theme: String,
    /// Cover image and sample clip, as paths relative to the skill directory (when present).
    pub cover: Option<String>,
    pub preview: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The value of a flat top-level `key: value` line, unquoted.
fn scalar(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    text.lines().find_map(|line| {
        let v = line.strip_prefix(&prefix)?.trim();
        let v = v.trim_matches(|c| c == '"' || c == '\'');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// The cover or sample clip the skill keeps by convention at `templates/covers/<id>.<ext>`
/// (its `templates.py list` derives them the same way), when the file exists.
fn asset(skill_dir: &Path, id: &str, ext: &str) -> Option<String> {
    let rel = format!("templates/covers/{id}.{ext}");
    skill_dir.join(&rel).is_file().then_some(rel)
}

/// The templates under `skill_dir/templates`, the default first then by id. A file with no
/// valid id or no name is left out. Empty for a skill from before templates.
pub fn list(skill_dir: &Path) -> Vec<Template> {
    let Ok(entries) = std::fs::read_dir(skill_dir.join("templates")) else {
        return Vec::new();
    };
    let mut found: Vec<Template> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "yaml"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let id = scalar(&text, "id")?;
            let stem = e.path().file_stem()?.to_str()?.to_string();
            if !valid_id(&id) || id != stem {
                return None;
            }
            Some(Template {
                name: scalar(&text, "name")?,
                name_zh: scalar(&text, "name_zh").unwrap_or_default(),
                description: scalar(&text, "description").unwrap_or_default(),
                theme: scalar(&text, "theme").unwrap_or_default(),
                cover: asset(skill_dir, &id, "png"),
                preview: asset(skill_dir, &id, "mp4"),
                id,
            })
        })
        .collect();
    found.sort_by(|a, b| (a.id != DEFAULT, &a.id).cmp(&(b.id != DEFAULT, &b.id)));
    found
}

/// What a form's template value means: empty is no choice, otherwise it must be listed.
pub fn resolve(skill_dir: &Path, choice: &str) -> Result<Option<String>, String> {
    let choice = choice.trim();
    if choice.is_empty() {
        return Ok(None);
    }
    if list(skill_dir).iter().any(|t| t.id == choice) {
        Ok(Some(choice.to_string()))
    } else {
        Err(format!("unknown template: {choice}"))
    }
}

/// The cover (`kind` = `cover`) or sample clip (`preview`) file of a listed template.
pub fn file(skill_dir: &Path, id: &str, kind: &str) -> Option<PathBuf> {
    let t = list(skill_dir).into_iter().find(|t| t.id == id)?;
    let rel = match kind {
        "cover" => t.cover?,
        "preview" => t.preview?,
        _ => return None,
    };
    Some(skill_dir.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("templates");
        std::fs::create_dir_all(t.join("covers")).unwrap();
        let yaml = |id: &str, extra: &str| {
            std::fs::write(
                t.join(format!("{id}.yaml")),
                format!(
                    "id: {id}\nname: Name {id}\nname_zh: 名{id}\ndescription: About {id}.\n\
                     style: promo\ntheme: ink\nshots:\n  - {{id: hero-push, weight: 3}}\n{extra}"
                ),
            )
            .unwrap();
        };
        yaml(
            "calm",
            "cover: templates/covers/calm.png\npreview: templates/covers/calm.mp4\n",
        );
        yaml("cinematic", "cover: templates/covers/cinematic.png\n");
        std::fs::write(t.join("covers/calm.png"), b"png").unwrap();
        std::fs::write(t.join("covers/calm.mp4"), b"mp4").unwrap();
        std::fs::write(t.join("covers/cinematic.png"), b"png").unwrap();
        d
    }

    #[test]
    fn lists_the_default_first_with_the_files_that_exist() {
        let d = skill();
        let l = list(d.path());
        assert_eq!(
            l.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["cinematic", "calm"]
        );
        assert_eq!(l[0].name, "Name cinematic");
        assert_eq!(l[0].name_zh, "名cinematic");
        assert_eq!(l[0].preview, None, "no clip named, none offered");
        assert_eq!(l[1].preview.as_deref(), Some("templates/covers/calm.mp4"));
    }

    #[test]
    fn a_missing_folder_or_a_broken_file_gives_no_template() {
        let d = tempfile::tempdir().unwrap();
        assert!(list(d.path()).is_empty());
        let t = d.path().join("templates");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("bad.yaml"), "id: other\nname: X\n").unwrap(); // id != file name
        std::fs::write(t.join("../x.yaml"), "id: ../x\nname: X\n").ok();
        std::fs::write(t.join("noname.yaml"), "id: noname\n").unwrap();
        assert!(list(d.path()).is_empty());
    }

    #[test]
    fn a_cover_is_offered_only_when_its_file_exists() {
        let d = skill();
        std::fs::remove_file(d.path().join("templates/covers/calm.png")).unwrap();
        let calm = list(d.path()).into_iter().find(|t| t.id == "calm").unwrap();
        assert_eq!(calm.cover, None);
        assert_eq!(calm.preview.as_deref(), Some("templates/covers/calm.mp4"));
    }

    #[test]
    fn resolve_accepts_only_listed_templates() {
        let d = skill();
        assert_eq!(resolve(d.path(), "").unwrap(), None);
        assert_eq!(
            resolve(d.path(), " calm ").unwrap().as_deref(),
            Some("calm")
        );
        assert!(resolve(d.path(), "calm; rm -rf").is_err());
        assert!(resolve(d.path(), "nope").is_err());
    }

    #[test]
    fn file_serves_only_listed_covers_and_clips() {
        let d = skill();
        assert!(file(d.path(), "calm", "preview").is_some());
        assert!(file(d.path(), "cinematic", "preview").is_none());
        assert!(file(d.path(), "calm", "yaml").is_none());
        assert!(file(d.path(), "../calm", "cover").is_none());
    }
}

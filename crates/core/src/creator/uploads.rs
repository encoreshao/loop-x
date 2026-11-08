//! Screenshots the user picks for a showcase video.
//!
//! A job's agent may only read under its own working directory, so the folder the user
//! picked is copied into `<skill dir>/output/uploads/<stamp>/` first and the job is
//! given that relative path, never the original location.

use std::path::{Path, PathBuf};

/// Most images one showcase uses (the skill plays 4 to 6 beats).
pub const MAX_IMAGES: usize = 20;
/// Largest image accepted.
pub const MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;

const EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

fn extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

/// A file name that is safe to hand to a shell-free script: ASCII letters, digits, `-`, `_`.
fn clean_stem(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned.chars().take(40).collect()
    }
}

/// The image files directly inside `folder` (no subfolders, no hidden files, no
/// symlinks), by name.
fn images_in(folder: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(folder)
        .map_err(|e| format!("cannot read the folder: {e}"))?
        .flatten()
    {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if hidden || !meta.is_file() || extension(&path).is_none() {
            continue;
        }
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "{} is larger than {} MB",
                entry.file_name().to_string_lossy(),
                MAX_IMAGE_BYTES / 1024 / 1024
            ));
        }
        found.push(path);
    }
    found.sort();
    Ok(found)
}

/// Copy the images of `folder` into a fresh `output/uploads/<stamp>` under `skill_dir`
/// and return that path relative to `skill_dir`.
pub fn stage(skill_dir: &Path, folder: &Path) -> Result<String, String> {
    if !folder.is_dir() {
        return Err("the screenshots folder does not exist".to_string());
    }
    let images = images_in(folder)?;
    if images.is_empty() {
        return Err("the screenshots folder has no png, jpg or webp images".to_string());
    }
    if images.len() > MAX_IMAGES {
        return Err(format!(
            "the screenshots folder has {} images; use at most {MAX_IMAGES}",
            images.len()
        ));
    }
    let stamp = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let rel = format!("output/uploads/{stamp}");
    let dest = skill_dir.join(&rel);
    std::fs::create_dir_all(&dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    for (i, src) in images.iter().enumerate() {
        let ext = extension(src).unwrap_or_default();
        let name = format!("{:02}-{}.{ext}", i + 1, clean_stem(src));
        std::fs::copy(src, dest.join(&name)).map_err(|e| format!("cannot copy {name}: {e}"))?;
    }
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[test]
    fn copies_only_images_into_a_fresh_folder_under_the_skill_with_safe_ordered_names() {
        let d = tempfile::tempdir().unwrap();
        let (skill, folder) = (d.path().join("skill"), d.path().join("shots"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        write(&folder, "b home page.PNG", b"x");
        write(&folder, "a.jpg", b"x");
        write(&folder, "notes.txt", b"secret");
        write(&folder, ".hidden.png", b"x");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        write(&folder.join("sub"), "deep.png", b"x");
        let rel = stage(&skill, &folder).unwrap();
        assert!(rel.starts_with("output/uploads/"), "{rel}");
        let mut names: Vec<String> = std::fs::read_dir(skill.join(&rel))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["01-a.jpg", "02-b-home-page.png"]);
    }

    #[test]
    fn a_second_stage_never_reuses_the_first_folder() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("shots");
        std::fs::create_dir_all(&folder).unwrap();
        write(&folder, "a.png", b"x");
        assert_ne!(
            stage(d.path(), &folder).unwrap(),
            stage(d.path(), &folder).unwrap()
        );
    }

    #[test]
    fn refuses_a_missing_folder_no_images_too_many_images_and_a_huge_image() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("shots");
        assert!(stage(d.path(), &folder)
            .unwrap_err()
            .contains("does not exist"));
        std::fs::create_dir_all(&folder).unwrap();
        write(&folder, "readme.md", b"x");
        assert!(stage(d.path(), &folder).unwrap_err().contains("no png"));
        for i in 0..=MAX_IMAGES {
            write(&folder, &format!("{i:02}.png"), b"x");
        }
        assert!(stage(d.path(), &folder).unwrap_err().contains("at most"));
        let big = d.path().join("big");
        std::fs::create_dir_all(&big).unwrap();
        let f = std::fs::File::create(big.join("huge.png")).unwrap();
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        assert!(stage(d.path(), &big).unwrap_err().contains("larger than"));
    }

    #[test]
    fn a_symlinked_image_is_skipped_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("shots");
        std::fs::create_dir_all(&folder).unwrap();
        write(d.path(), "outside.png", b"private");
        std::os::unix::fs::symlink(d.path().join("outside.png"), folder.join("link.png")).unwrap();
        assert!(stage(d.path(), &folder).unwrap_err().contains("no png"));
    }
}

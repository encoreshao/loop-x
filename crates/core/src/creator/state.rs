//! Creator state the user sets by hand: which videos are approved and where each was
//! published. Stored in `<home>/creator.json`; a missing or corrupt file shows as empty
//! state, but a corrupt one is never overwritten by a change.

use super::videos::VideoProject;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What LoopX itself remembers of a video, in its own folder, so the video can still be
/// listed (with a placeholder, and its published mark) after its files, which live in
/// the skill's folder outside LoopX, are deleted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub title: String,
    pub source_url: Option<String>,
    pub created_at: Option<String>,
    /// A video file was seen for it at some point.
    pub had_video: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreatorState {
    pub approved: BTreeSet<String>,
    /// slug -> platforms (`wechat`, `douyin`, `linkedin`, `facebook`)
    pub published: BTreeMap<String, Vec<String>>,
    /// slug -> what is remembered of the video.
    pub records: BTreeMap<String, Record>,
}

/// How far a video is published across the platforms it has text for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishLevel {
    None,
    Partly,
    All,
}

/// Published on every one of `available` (or, with no platform text at all, on any).
pub fn publish_level(s: &CreatorState, slug: &str, available: &[&str]) -> PublishLevel {
    let marked: &[String] = s.published.get(slug).map_or(&[], Vec::as_slice);
    if marked.is_empty() {
        return PublishLevel::None;
    }
    if available.is_empty() {
        return PublishLevel::All;
    }
    let done = available
        .iter()
        .filter(|p| marked.iter().any(|m| m == **p))
        .count();
    match done {
        0 => PublishLevel::None,
        n if n == available.len() => PublishLevel::All,
        _ => PublishLevel::Partly,
    }
}

const PLATFORMS: [&str; 4] = ["wechat", "douyin", "linkedin", "facebook"];

/// `<loopx_home>/creator.json`.
pub fn state_path(loopx_home: &Path) -> std::path::PathBuf {
    loopx_home.join("creator.json")
}

/// The state for display: a missing or corrupt file reads as empty.
pub fn load(path: &Path) -> CreatorState {
    let Some(v) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return CreatorState::default();
    };
    from_value(&v)
}

/// The state before a change: only a missing file is empty; one that cannot be read
/// or parsed is an error, so a toggle never replaces the user's file with defaults.
fn load_for_update(path: &Path) -> Result<CreatorState, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CreatorState::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => Ok(from_value(&v)),
        _ => Err(format!(
            "{} is not valid Creator state; fix or remove it first",
            path.display()
        )),
    }
}

fn from_value(v: &Value) -> CreatorState {
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let approved = strings(v.get("approved").unwrap_or(&Value::Null))
        .into_iter()
        .collect();
    let published = v
        .get("published")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let mut l = strings(v);
                    l.retain(|p| PLATFORMS.contains(&p.as_str()));
                    (k.clone(), l)
                })
                .filter(|(_, v)| !v.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let records = v
        .get("records")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(slug, r)| {
                    let text = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
                    (
                        slug.clone(),
                        Record {
                            title: text("title").unwrap_or_default(),
                            source_url: text("source_url"),
                            created_at: text("created_at"),
                            had_video: r.get("had_video").and_then(Value::as_bool) == Some(true),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    CreatorState {
        approved,
        published,
        records,
    }
}

fn save(path: &Path, s: &CreatorState) -> Result<(), String> {
    let records: serde_json::Map<String, Value> = s
        .records
        .iter()
        .map(|(slug, r)| {
            (
                slug.clone(),
                json!({"title": r.title, "source_url": r.source_url,
                       "created_at": r.created_at, "had_video": r.had_video}),
            )
        })
        .collect();
    let v = json!({ "approved": s.approved, "published": s.published, "records": records });
    crate::settings_store::atomic_write_json(&v, path).map_err(|e| e.to_string())
}

/// Remember every listed project (title, source, date, and that it had a video), so it
/// can still be shown after its files are deleted. Writes only when something changed;
/// returns whether it did. A record is never dropped here, and never loses
/// `had_video`. A corrupt state file is an error, never overwritten.
pub fn sync_records(path: &Path, projects: &[VideoProject]) -> Result<bool, String> {
    let mut s = load_for_update(path)?;
    let mut changed = false;
    for p in projects {
        let had_video = s.records.get(&p.slug).is_some_and(|r| r.had_video) || p.mp4.is_some();
        let next = Record {
            title: p.title.clone(),
            source_url: p.source_url.clone(),
            created_at: p.created_at.clone(),
            had_video,
        };
        if s.records.get(&p.slug) != Some(&next) {
            s.records.insert(p.slug.clone(), next);
            changed = true;
        }
    }
    if changed {
        save(path, &s)?;
    }
    Ok(changed)
}

/// Drop a video's record and its approved / published marks.
pub fn forget(path: &Path, slug: &str) -> Result<(), String> {
    let mut s = load_for_update(path)?;
    let had = s.records.remove(slug).is_some()
        | s.approved.remove(slug)
        | s.published.remove(slug).is_some();
    if had {
        save(path, &s)?;
    }
    Ok(())
}

/// Mark the video published on every one of `platforms` (`on`), or clear every mark.
pub fn set_published_all(
    path: &Path,
    slug: &str,
    platforms: &[&str],
    on: bool,
) -> Result<(), String> {
    if let Some(bad) = platforms.iter().find(|p| !PLATFORMS.contains(p)) {
        return Err(format!("unknown platform: {bad}"));
    }
    let mut s = load_for_update(path)?;
    if on {
        let list = s.published.entry(slug.to_string()).or_default();
        for p in platforms {
            if !list.iter().any(|m| m == p) {
                list.push((*p).to_string());
            }
        }
    } else {
        s.published.remove(slug);
    }
    save(path, &s)
}

pub fn set_approved(path: &Path, slug: &str, on: bool) -> Result<(), String> {
    let mut s = load_for_update(path)?;
    if on {
        s.approved.insert(slug.to_string());
    } else {
        s.approved.remove(slug);
    }
    save(path, &s)
}

pub fn set_published(path: &Path, slug: &str, platform: &str, on: bool) -> Result<(), String> {
    if !PLATFORMS.contains(&platform) {
        return Err(format!("unknown platform: {platform}"));
    }
    let mut s = load_for_update(path)?;
    let list = s.published.entry(slug.to_string()).or_default();
    list.retain(|p| p != platform);
    if on {
        list.push(platform.to_string());
    }
    if list.is_empty() {
        s.published.remove(slug);
    }
    save(path, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        set_approved(&p, "a", true).unwrap();
        set_approved(&p, "b", true).unwrap();
        set_approved(&p, "b", false).unwrap();
        set_published(&p, "a", "wechat", true).unwrap();
        set_published(&p, "a", "linkedin", true).unwrap();
        set_published(&p, "a", "facebook", true).unwrap();
        set_published(&p, "a", "facebook", false).unwrap();
        set_published(&p, "a", "linkedin", false).unwrap();
        set_published(&p, "a", "douyin", true).unwrap();
        set_published(&p, "a", "wechat", false).unwrap();
        let s = load(&p);
        assert_eq!(s.approved, ["a".to_string()].into_iter().collect());
        assert_eq!(s.published["a"], vec!["douyin".to_string()]);
    }

    fn project(slug: &str, title: &str, mp4: bool) -> VideoProject {
        VideoProject {
            slug: slug.into(),
            title: title.into(),
            source_url: Some(format!("https://example.com/{slug}")),
            created_at: Some("2026-11-04T10:00:00Z".into()),
            mp4: mp4.then(|| std::path::PathBuf::from(format!("/x/{slug}.mp4"))),
            cover: None,
            wechat: None,
            douyin: None,
            linkedin: None,
            facebook: None,
        }
    }

    #[test]
    fn records_keep_what_is_needed_to_list_a_video_after_its_files_are_gone() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        assert!(
            sync_records(&p, &[project("a", "Alpha", true)]).unwrap(),
            "first sync writes"
        );
        assert!(
            !sync_records(&p, &[project("a", "Alpha", true)]).unwrap(),
            "no change, no write"
        );
        let s = load(&p);
        let r = &s.records["a"];
        assert_eq!((r.title.as_str(), r.had_video), ("Alpha", true));
        assert_eq!(r.source_url.as_deref(), Some("https://example.com/a"));
        assert_eq!(r.created_at.as_deref(), Some("2026-11-04T10:00:00Z"));
        // The files vanish: the record stays; a rename updates it.
        assert!(!sync_records(&p, &[]).unwrap());
        assert!(load(&p).records.contains_key("a"));
        assert!(sync_records(&p, &[project("a", "Alpha 2", true)]).unwrap());
        assert_eq!(load(&p).records["a"].title, "Alpha 2");
        // A record never loses the fact that a video existed.
        sync_records(&p, &[project("a", "Alpha 2", false)]).unwrap();
        assert!(load(&p).records["a"].had_video);
    }

    #[test]
    fn syncing_never_overwrites_a_corrupt_file_and_keeps_other_state() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(sync_records(&p, &[project("a", "A", true)]).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
        std::fs::remove_file(&p).unwrap();
        set_approved(&p, "a", true).unwrap();
        set_published(&p, "a", "wechat", true).unwrap();
        sync_records(&p, &[project("a", "A", true)]).unwrap();
        set_approved(&p, "b", true).unwrap();
        let s = load(&p);
        assert!(s.approved.contains("a") && s.approved.contains("b"));
        assert_eq!(s.published["a"], vec!["wechat".to_string()]);
        assert!(
            s.records.contains_key("a"),
            "a later toggle keeps the records"
        );
    }

    #[test]
    fn publishing_everywhere_and_the_level_it_gives() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        let both = ["wechat", "douyin"];
        assert_eq!(publish_level(&load(&p), "a", &both), PublishLevel::None);
        set_published(&p, "a", "wechat", true).unwrap();
        assert_eq!(publish_level(&load(&p), "a", &both), PublishLevel::Partly);
        assert_eq!(
            publish_level(&load(&p), "a", &["wechat"]),
            PublishLevel::All
        );
        set_published_all(&p, "a", &both, true).unwrap();
        assert_eq!(publish_level(&load(&p), "a", &both), PublishLevel::All);
        set_published_all(&p, "a", &both, false).unwrap();
        assert_eq!(publish_level(&load(&p), "a", &both), PublishLevel::None);
        assert!(!load(&p).published.contains_key("a"));
        assert!(set_published_all(&p, "a", &["tiktok"], true).is_err());
        // With no platform text at all, any mark counts as published.
        set_published_all(&p, "b", &["wechat"], true).unwrap();
        assert_eq!(publish_level(&load(&p), "b", &[]), PublishLevel::All);
    }

    #[test]
    fn forgetting_a_video_clears_its_record_and_marks() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        sync_records(&p, &[project("a", "A", true), project("b", "B", true)]).unwrap();
        set_approved(&p, "a", true).unwrap();
        set_published(&p, "a", "douyin", true).unwrap();
        forget(&p, "a").unwrap();
        let s = load(&p);
        assert!(
            !s.records.contains_key("a")
                && !s.approved.contains("a")
                && !s.published.contains_key("a")
        );
        assert!(s.records.contains_key("b"));
        forget(&p, "never-existed").unwrap();
    }

    #[test]
    fn corrupt_state_is_default() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load(&p), CreatorState::default());
        assert_eq!(
            load(&d.path().join("missing.json")),
            CreatorState::default()
        );
    }

    #[test]
    fn a_corrupt_state_file_is_never_overwritten_by_a_toggle() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        for bad in ["{not json", "[1, 2]"] {
            std::fs::write(&p, bad).unwrap();
            assert!(set_approved(&p, "a", true).is_err(), "{bad}");
            assert!(set_published(&p, "a", "wechat", true).is_err(), "{bad}");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
        }
        // A missing file is simply empty state.
        let fresh = d.path().join("new.json");
        set_approved(&fresh, "a", true).unwrap();
        assert!(load(&fresh).approved.contains("a"));
    }

    #[test]
    fn unknown_platforms_are_dropped_on_load() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        std::fs::write(&p, r#"{"published":{"a":["wechat","evil"],"b":["x"]}}"#).unwrap();
        let s = load(&p);
        assert_eq!(s.published["a"], vec!["wechat".to_string()]);
        assert!(!s.published.contains_key("b"));
    }

    #[test]
    fn invalid_platform_rejected() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("creator.json");
        assert!(set_published(&p, "a", "tiktok", true).is_err());
        assert!(!p.exists());
    }
}

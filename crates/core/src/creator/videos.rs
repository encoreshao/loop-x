//! Read finished inspire-video projects (`output/final/<slug>/`) and their publish copy.
//!
//! `project.json` is untrusted data: fields are read leniently and a malformed
//! manifest is reported rather than fatal.

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct PublishCopy {
    pub title: String,
    pub caption: String,
    pub hashtags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoProject {
    pub slug: String,
    pub title: String,
    pub source_url: Option<String>,
    pub created_at: Option<String>,
    pub mp4: Option<PathBuf>,
    pub cover: Option<PathBuf>,
    pub wechat: Option<PublishCopy>,
    pub douyin: Option<PublishCopy>,
}

/// `<user_home>/.inspire-video/output/final`, under the generate jobs' working
/// directory. No environment override, for the same reason as `scout::scout_root`.
pub fn videos_root(user_home: &Path) -> PathBuf {
    super::spawn::generate_dir(user_home).join("output/final")
}

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn copy_of(v: &Value) -> Option<PublishCopy> {
    if !v.is_object() {
        return None;
    }
    Some(PublishCopy {
        title: text(v, "title").unwrap_or_default(),
        caption: text(v, "caption").unwrap_or_default(),
        hashtags: v
            .get("hashtags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A single plain path component: no separators, no `.`/`..`, not absolute.
fn safe_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// The canonical path of `<root>/<slug>/<name>` if both are safe single components,
/// it is a file, and it stays under the canonical `root` (no `..`, no symlink escape).
pub fn project_file(root: &Path, slug: &str, name: &str) -> Option<PathBuf> {
    if !safe_component(slug) || !safe_component(name) {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = root.join(slug).join(name).canonicalize().ok()?;
    (path.starts_with(&root) && path.is_file()).then_some(path)
}

/// `<root>/<slug>/<name>` (uncanonicalised) when `project_file` accepts it.
fn existing(root: &Path, slug: &str, name: &str) -> Option<PathBuf> {
    project_file(root, slug, name).map(|_| root.join(slug).join(name))
}

fn first_with_ext(root: &Path, slug: &str, ext: &str) -> Option<PathBuf> {
    let mut names: Vec<String> = std::fs::read_dir(root.join(slug))
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(ext))
        .collect();
    names.sort();
    names.iter().find_map(|n| existing(root, slug, n))
}

fn sort_key(p: &VideoProject, root: &Path) -> i64 {
    if let Some(t) = p
        .created_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        return t.timestamp();
    }
    std::fs::metadata(root.join(&p.slug))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// The file's text, refusing more than `cap` bytes. The read itself is bounded, so a
/// file that grows after it was opened cannot exceed the cap.
fn read_capped(path: &Path, cap: u64) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut raw = String::new();
    file.take(cap + 1)
        .read_to_string(&mut raw)
        .map_err(|e| e.to_string())?;
    if raw.len() as u64 > cap {
        return Err(format!("larger than {cap} bytes"));
    }
    Ok(raw)
}

/// Older projects have no `publish` block in `project.json`, only the plain
/// `publish_<platform>.txt` files: the title on the first line, a blank line,
/// the caption, and a last line of `#tags`.
fn copy_from_txt(root: &Path, slug: &str, name: &str) -> Option<PublishCopy> {
    let path = project_file(root, slug, name)?;
    let raw = read_capped(&path, MAX_MANIFEST_BYTES).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (title, rest) = raw.split_once('\n').unwrap_or((raw, ""));
    let mut lines: Vec<&str> = rest.trim().lines().collect();
    let mut hashtags = Vec::new();
    while let Some(last) = lines.last().map(|l| l.trim()) {
        if last.is_empty() {
            lines.pop();
        } else if last
            .split_whitespace()
            .all(|w| w.starts_with('#') && w.len() > 1)
        {
            let mut tags: Vec<String> = last
                .split_whitespace()
                .map(|w| w.trim_start_matches('#').to_string())
                .collect();
            tags.append(&mut hashtags);
            hashtags = tags;
            lines.pop();
        } else {
            break;
        }
    }
    Some(PublishCopy {
        title: title.trim().to_string(),
        caption: lines.join("\n").trim().to_string(),
        hashtags,
    })
}

fn read_project(root: &Path, slug: &str) -> Result<VideoProject, String> {
    let manifest = root.join(slug).join("project.json");
    let mut p = VideoProject {
        slug: slug.to_string(),
        title: slug.to_string(),
        source_url: None,
        created_at: None,
        mp4: None,
        cover: None,
        wechat: None,
        douyin: None,
    };
    let (mut video, mut cover) = (None, None);
    if manifest.symlink_metadata().is_ok() {
        let name = manifest.display();
        let path = project_file(root, slug, "project.json")
            .ok_or_else(|| format!("{name}: not a regular file inside the videos folder"))?;
        let raw = read_capped(&path, MAX_MANIFEST_BYTES).map_err(|e| format!("{name}: {e}"))?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| format!("{name}: {e}"))?;
        if !v.is_object() {
            return Err(format!("{name}: not a JSON object"));
        }
        p.title = text(&v, "title").unwrap_or_else(|| slug.to_string());
        p.source_url = text(&v, "source_url");
        p.created_at = text(&v, "created_at");
        video = text(&v, "video");
        cover = text(&v, "cover");
        if let Some(publish) = v.get("publish") {
            p.wechat = publish.get("wechat").and_then(copy_of);
            p.douyin = publish.get("douyin").and_then(copy_of);
        }
    }
    if p.wechat.is_none() {
        p.wechat = copy_from_txt(root, slug, "publish_wechat.txt");
    }
    if p.douyin.is_none() {
        p.douyin = copy_from_txt(root, slug, "publish_douyin.txt");
    }
    p.mp4 = video
        .and_then(|n| existing(root, slug, &n))
        .or_else(|| first_with_ext(root, slug, ".mp4"));
    p.cover = cover
        .and_then(|n| existing(root, slug, &n))
        .or_else(|| existing(root, slug, "cover.png"));
    Ok(p)
}

/// Whether any listed project has a video file written between `since` and `until`
/// (`None`: no upper bound): what a Generate job that ran in that window produced,
/// if it produced anything.
pub fn any_video_between(
    projects: &[VideoProject],
    since: std::time::SystemTime,
    until: Option<std::time::SystemTime>,
) -> bool {
    projects.iter().filter_map(|p| p.mp4.as_ref()).any(|mp4| {
        std::fs::metadata(mp4)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= since && until.is_none_or(|u| t <= u))
    })
}

/// Delete a video's folders: its deliverables (`output/final/<slug>`) and its working
/// folder (`output/projects/<slug>`), when they exist. The downloaded source is shared
/// by other videos and is left alone. `slug` must be one plain folder name, a folder
/// that is a symlink is refused rather than followed, and each folder must resolve
/// inside its own root. Returns the folders removed (none when the files are already
/// gone).
pub fn delete_project_files(user_home: &Path, slug: &str) -> Result<Vec<PathBuf>, String> {
    if !safe_component(slug) || slug.starts_with('.') {
        return Err("not a valid video name".to_string());
    }
    let skill = super::spawn::generate_dir(user_home);
    let roots = [videos_root(user_home), skill.join("output/projects")];
    let mut removed = Vec::new();
    for root in roots {
        let dir = root.join(slug);
        let Ok(meta) = std::fs::symlink_metadata(&dir) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(format!(
                "{} is not a plain folder; not deleting it",
                dir.display()
            ));
        }
        let inside = match (root.canonicalize(), dir.canonicalize()) {
            (Ok(r), Ok(d)) => d.starts_with(r),
            _ => false,
        };
        if !inside {
            return Err(format!(
                "{} resolves outside its folder; not deleting it",
                dir.display()
            ));
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        removed.push(dir);
    }
    Ok(removed)
}

/// All project folders under `root`, newest first (by `created_at`, else folder
/// mtime), plus one error string per unreadable manifest.
pub fn list_projects(root: &Path) -> (Vec<VideoProject>, Vec<String>) {
    let mut projects = Vec::new();
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (projects, errors);
    };
    for entry in entries.flatten() {
        let slug = entry.file_name().to_string_lossy().into_owned();
        if slug.starts_with('.') || !safe_component(&slug) || !entry.path().is_dir() {
            continue;
        }
        let inside = match (root.canonicalize(), entry.path().canonicalize()) {
            (Ok(r), Ok(p)) => p.starts_with(r),
            _ => false,
        };
        if !inside {
            errors.push(format!("{slug}: folder resolves outside the videos folder"));
            continue;
        }
        match read_project(root, &slug) {
            Ok(p) => projects.push(p),
            Err(e) => errors.push(e),
        }
    }
    projects.sort_by_cached_key(|p| (std::cmp::Reverse(sort_key(p, root)), p.slug.clone()));
    errors.sort();
    (projects, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{"schema":1,"slug":"tim","title":"Procrastinator","source_url":"https://example.com/v",
"created_at":"2026-11-04T10:00:00Z","video":"tim.mp4","cover":"cover.png",
"publish":{"wechat":{"title":"微信标题","caption":"微信文案","hashtags":["a","b"]},
"douyin":{"title":"抖音标题","caption":"抖音文案","hashtags":["c"]}}}"#;

    fn project(root: &Path, slug: &str, files: &[(&str, &str)]) {
        let d = root.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        for (n, c) in files {
            std::fs::write(d.join(n), c).unwrap();
        }
    }

    #[test]
    fn deleting_a_video_removes_its_deliverables_and_working_folder_only() {
        let home = tempfile::tempdir().unwrap();
        let skill = crate::creator::spawn::generate_dir(home.path());
        for dir in [
            "output/final/demo",
            "output/projects/demo",
            "output/final/other",
            "output/cache/x",
        ] {
            std::fs::create_dir_all(skill.join(dir)).unwrap();
            std::fs::write(skill.join(dir).join("f.txt"), "x").unwrap();
        }
        let removed = delete_project_files(home.path(), "demo").unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!skill.join("output/final/demo").exists());
        assert!(!skill.join("output/projects/demo").exists());
        // Another video and the shared download cache are untouched.
        assert!(skill.join("output/final/other/f.txt").exists());
        assert!(skill.join("output/cache/x/f.txt").exists());
        // Already gone: nothing to do, not an error.
        assert!(delete_project_files(home.path(), "demo")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deleting_refuses_unsafe_names_and_symlinked_folders() {
        let home = tempfile::tempdir().unwrap();
        let skill = crate::creator::spawn::generate_dir(home.path());
        std::fs::create_dir_all(skill.join("output/final")).unwrap();
        std::fs::create_dir_all(skill.join("output/keep")).unwrap();
        std::fs::write(skill.join("output/keep/precious.txt"), "x").unwrap();
        for bad in ["", ".", "..", "../keep", "a/b", ".hidden"] {
            assert!(delete_project_files(home.path(), bad).is_err(), "{bad}");
        }
        std::os::unix::fs::symlink(skill.join("output/keep"), skill.join("output/final/linked"))
            .unwrap();
        assert!(delete_project_files(home.path(), "linked").is_err());
        assert!(
            skill.join("output/keep/precious.txt").exists(),
            "the link's target survives"
        );
    }

    #[test]
    fn any_video_between_looks_at_when_the_video_file_was_written() {
        let d = tempfile::tempdir().unwrap();
        let mp4 = d.path().join("a.mp4");
        std::fs::write(&mp4, b"x").unwrap();
        let project = |mp4: Option<PathBuf>| VideoProject {
            slug: "a".into(),
            title: "A".into(),
            source_url: None,
            created_at: None,
            mp4,
            cover: None,
            wechat: None,
            douyin: None,
        };
        let hour = std::time::Duration::from_secs(3600);
        let now = std::time::SystemTime::now();
        let p = [project(Some(mp4.clone()))];
        assert!(any_video_between(&p, now - hour, None));
        assert!(!any_video_between(&p, now + hour, None));
        // A job that ended before the file was written did not write it.
        assert!(any_video_between(&p, now - hour, Some(now + hour)));
        assert!(!any_video_between(&p, now - 2 * hour, Some(now - hour)));
        // A project without a video file, or a missing file, produced nothing.
        assert!(!any_video_between(&[project(None)], now - hour, None));
        assert!(!any_video_between(
            &[project(Some(d.path().join("gone.mp4")))],
            now - hour,
            None
        ));
        assert!(!any_video_between(&[], now - hour, None));
    }

    #[test]
    fn reads_project_json() {
        let d = tempfile::tempdir().unwrap();
        project(
            d.path(),
            "tim",
            &[
                ("project.json", MANIFEST),
                ("tim.mp4", "x"),
                ("cover.png", "x"),
            ],
        );
        let (ps, errs) = list_projects(d.path());
        assert!(errs.is_empty());
        let p = &ps[0];
        assert_eq!(p.slug, "tim");
        assert_eq!(p.title, "Procrastinator");
        assert_eq!(p.source_url.as_deref(), Some("https://example.com/v"));
        assert_eq!(p.created_at.as_deref(), Some("2026-11-04T10:00:00Z"));
        assert_eq!(
            p.mp4.as_deref(),
            Some(d.path().join("tim/tim.mp4").as_path())
        );
        assert_eq!(
            p.cover.as_deref(),
            Some(d.path().join("tim/cover.png").as_path())
        );
        assert_eq!(
            p.wechat,
            Some(PublishCopy {
                title: "微信标题".into(),
                caption: "微信文案".into(),
                hashtags: vec!["a".into(), "b".into()]
            })
        );
        assert_eq!(p.douyin.as_ref().unwrap().title, "抖音标题");
    }

    #[test]
    fn falls_back_to_files_when_no_manifest() {
        let d = tempfile::tempdir().unwrap();
        project(d.path(), "old", &[("old.mp4", "x"), ("cover.png", "x")]);
        let (ps, errs) = list_projects(d.path());
        assert!(errs.is_empty());
        assert_eq!(ps[0].slug, "old");
        assert_eq!(ps[0].title, "old");
        assert!(ps[0].mp4.is_some() && ps[0].cover.is_some());
        assert!(ps[0].wechat.is_none() && ps[0].douyin.is_none());
    }

    #[test]
    fn falls_back_to_publish_txt_files() {
        let d = tempfile::tempdir().unwrap();
        project(
            d.path(),
            "old",
            &[
                ("old.mp4", "x"),
                (
                    "publish_wechat.txt",
                    "短标题\n\n正文一\n\n来源行\n\n#拖延症 #TED演讲\n",
                ),
                ("publish_douyin.txt", "抖音标题\n\n只有正文\n"),
            ],
        );
        let (ps, _) = list_projects(d.path());
        let w = ps[0].wechat.as_ref().unwrap();
        assert_eq!(w.title, "短标题");
        assert_eq!(w.caption, "正文一\n\n来源行");
        assert_eq!(w.hashtags, vec!["拖延症", "TED演讲"]);
        let dy = ps[0].douyin.as_ref().unwrap();
        assert_eq!(
            (dy.title.as_str(), dy.caption.as_str()),
            ("抖音标题", "只有正文")
        );
        assert!(dy.hashtags.is_empty());
    }

    #[test]
    fn missing_platform_is_none() {
        let d = tempfile::tempdir().unwrap();
        project(
            d.path(),
            "p",
            &[(
                "project.json",
                r#"{"slug":"p","title":"T","publish":{"douyin":{"title":"d","caption":"c","hashtags":[]}}}"#,
            )],
        );
        let (ps, _) = list_projects(d.path());
        assert!(ps[0].wechat.is_none());
        assert!(ps[0].douyin.is_some());
        assert!(ps[0].mp4.is_none());
    }

    #[test]
    fn bad_manifest_reported() {
        let d = tempfile::tempdir().unwrap();
        project(
            d.path(),
            "bad",
            &[("project.json", "{ nope"), ("bad.mp4", "x")],
        );
        project(d.path(), "good", &[("project.json", MANIFEST)]);
        let (ps, errs) = list_projects(d.path());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("bad/project.json"));
        assert!(ps.iter().any(|p| p.slug == "good"));
    }

    #[test]
    fn newest_first() {
        let d = tempfile::tempdir().unwrap();
        project(
            d.path(),
            "a",
            &[(
                "project.json",
                r#"{"title":"a","created_at":"2026-11-01T00:00:00Z"}"#,
            )],
        );
        project(
            d.path(),
            "b",
            &[(
                "project.json",
                r#"{"title":"b","created_at":"2026-11-03T00:00:00Z"}"#,
            )],
        );
        let (ps, _) = list_projects(d.path());
        assert_eq!(ps[0].slug, "b");
    }

    #[test]
    fn project_file_rejects_traversal_and_symlink_escape() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("final");
        project(&root, "ok", &[("ok.mp4", "x")]);
        assert!(project_file(&root, "ok", "ok.mp4").is_some());
        assert!(project_file(&root, "ok", "missing.mp4").is_none());
        for (s, n) in [
            ("..", "ok.mp4"),
            ("ok", "../ok/ok.mp4"),
            ("ok", "/etc/passwd"),
            ("ok/", "ok.mp4"),
            ("", "ok.mp4"),
            ("ok", ""),
            ("ok", "."),
            ("a/b", "x"),
            ("ok", "a\\b"),
            ("a\0b", "x"),
            ("ok", "a\0b"),
        ] {
            assert!(project_file(&root, s, n).is_none(), "{s:?} {n:?}");
        }
        #[cfg(unix)]
        {
            let outside = d.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.mp4"), "s").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
            assert!(project_file(&root, "evil", "secret.mp4").is_none());
            std::os::unix::fs::symlink(outside.join("secret.mp4"), root.join("ok/link.mp4"))
                .unwrap();
            assert!(project_file(&root, "ok", "link.mp4").is_none());
        }
    }

    #[test]
    fn odd_types_degrade_without_panic() {
        let d = tempfile::tempdir().unwrap();
        project(
            d.path(),
            "odd",
            &[("project.json", r#"{"title":5,"publish":[],"video":7}"#)],
        );
        project(
            d.path(),
            "odd2",
            &[(
                "project.json",
                r#"{"publish":{"wechat":"x","douyin":{"title":1,"hashtags":["a",2,null]}}}"#,
            )],
        );
        let (ps, errs) = list_projects(d.path());
        assert!(errs.is_empty());
        let o = ps.iter().find(|p| p.slug == "odd").unwrap();
        assert!(o.wechat.is_none() && o.douyin.is_none() && o.mp4.is_none());
        assert_eq!(o.title, "odd");
        let o2 = ps.iter().find(|p| p.slug == "odd2").unwrap();
        assert!(o2.wechat.is_none());
        let dy = o2.douyin.as_ref().unwrap();
        assert_eq!(dy.title, "");
        assert_eq!(dy.hashtags, vec!["a".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_slug_or_manifest_is_not_read() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("final");
        std::fs::create_dir_all(&root).unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("project.json"), MANIFEST).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
        project(&root, "linked", &[]);
        std::os::unix::fs::symlink(
            outside.join("project.json"),
            root.join("linked/project.json"),
        )
        .unwrap();
        let (ps, errs) = list_projects(&root);
        assert!(ps.is_empty(), "{ps:?}");
        assert_eq!(errs.len(), 2, "{errs:?}");
    }

    #[test]
    fn oversize_manifest_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let big = format!(r#"{{"title":"{}"}}"#, "x".repeat(1024 * 1024 + 1));
        project(d.path(), "big", &[("project.json", &big)]);
        let (ps, errs) = list_projects(d.path());
        assert!(ps.is_empty());
        assert!(errs[0].contains("big/project.json"));
    }

    #[test]
    fn read_capped_refuses_more_than_the_cap_and_accepts_exactly_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("m.json");
        std::fs::write(&f, "12345").unwrap();
        assert_eq!(read_capped(&f, 5).unwrap(), "12345");
        assert!(read_capped(&f, 4).unwrap_err().contains("larger than 4"));
    }

    #[test]
    fn videos_root_is_under_where_generate_jobs_run() {
        let home = Path::new("/home/u");
        assert_eq!(
            videos_root(home),
            PathBuf::from("/home/u/.inspire-video/output/final")
        );
        assert!(videos_root(home).starts_with(crate::creator::spawn::generate_dir(home)));
    }
}

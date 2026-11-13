//! Read-only views of a project's guide for the issue agent
//! (`loopx memory-store guide`) and the dashboard chat. Agents see **approved** pages
//! only, and every text returned here has passed the secret guard.

use crate::profile_read::{guard, ReadError};
use crate::project_guide;
use crate::project_profile::{contains_secret, ts, Status};
use serde_json::json;
use std::path::Path;

pub const NO_GUIDE: &str = "(no guide yet)";
pub const NO_PAGE: &str = "(no approved page with that slug)";

/// The approved pages as JSON, `None` when there are none. A page that will not load or
/// whose text looks like it holds a secret is left out and named under `skipped`
/// (slug only, never its content); the rest of the index is still returned.
pub fn index_json(alias: &str, root: Option<&Path>) -> Result<Option<String>, ReadError> {
    let list = project_guide::list(alias, root).map_err(ReadError::Invalid)?;
    let mut skipped = list.skipped;
    let mut pages = Vec::new();
    for p in list.pages.iter().filter(|p| p.status == Status::Approved) {
        if contains_secret(&project_guide::render(p)).is_some() {
            skipped.push(p.slug.clone());
            continue;
        }
        pages.push(json!({
            "slug": p.slug,
            "kind": p.kind.as_str(),
            "title": p.title,
            "summary": p.summary,
            "verified": p.generated_at.map(ts),
        }));
    }
    if pages.is_empty() {
        return Ok(None);
    }
    skipped.sort();
    let value = json!({"alias": alias, "pages": pages, "skipped": skipped});
    guard(serde_json::to_string_pretty(&value).unwrap_or_default()).map(Some)
}

/// One approved page's markdown. `None` when the page is missing or still a draft.
pub fn page_text(
    alias: &str,
    slug: &str,
    root: Option<&Path>,
) -> Result<Option<String>, ReadError> {
    match project_guide::load(alias, slug, root).map_err(ReadError::Invalid)? {
        Some(p) if p.status == Status::Approved => guard(project_guide::render(&p)).map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_guide::{self, Kind, Page, Part};
    use crate::project_profile::{Origin, Status};
    use tempfile::tempdir;

    fn page(slug: &str, status: Status, body: &str) -> Page {
        Page {
            alias: "shop".into(),
            slug: slug.into(),
            kind: Kind::Feature,
            title: format!("Title of {slug}"),
            status,
            generated_at: None,
            head_sha: None,
            summary: format!("Summary of {slug}."),
            related: vec![],
            parts: vec![Part {
                key: "what".into(),
                body: body.into(),
                origin: Origin::Agent,
            }],
            sources: vec![],
        }
    }

    fn put(p: &Page, root: &std::path::Path) {
        project_guide::save(p, Some(root)).unwrap();
    }

    #[test]
    fn no_pages_or_only_drafts_is_no_guide() {
        let dir = tempdir().unwrap();
        let root = Some(dir.path());
        assert_eq!(index_json("shop", root).unwrap(), None);
        put(&page("feature-a", Status::Draft, "text"), dir.path());
        assert_eq!(index_json("shop", root).unwrap(), None);
    }

    #[test]
    fn the_index_lists_approved_pages_only() {
        let dir = tempdir().unwrap();
        put(&page("feature-a", Status::Approved, "text"), dir.path());
        put(&page("feature-b", Status::Draft, "text"), dir.path());
        let json = index_json("shop", Some(dir.path())).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let slugs: Vec<&str> = v["pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, ["feature-a"]);
        assert_eq!(v["pages"][0]["kind"], "feature");
        assert_eq!(v["pages"][0]["summary"], "Summary of feature-a.");
    }

    #[test]
    fn the_index_verified_time_uses_the_z_form() {
        let dir = tempdir().unwrap();
        let mut p = page("feature-a", Status::Approved, "text");
        p.generated_at = Some(chrono::Utc::now());
        put(&p, dir.path());
        let json = index_json("shop", Some(dir.path())).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["pages"][0]["verified"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn a_draft_page_is_not_readable() {
        let dir = tempdir().unwrap();
        put(&page("feature-a", Status::Draft, "draft text"), dir.path());
        assert_eq!(
            page_text("shop", "feature-a", Some(dir.path())).unwrap(),
            None
        );
        assert_eq!(
            page_text("shop", "feature-zzz", Some(dir.path())).unwrap(),
            None
        );
    }

    #[test]
    fn an_approved_page_is_returned_as_markdown() {
        let dir = tempdir().unwrap();
        put(
            &page("feature-a", Status::Approved, "Buyers pay."),
            dir.path(),
        );
        let text = page_text("shop", "feature-a", Some(dir.path()))
            .unwrap()
            .unwrap();
        assert!(text.contains("Buyers pay."));
        assert!(text.contains("slug: feature-a"));
    }

    #[test]
    fn a_bad_slug_is_invalid_not_a_path() {
        let dir = tempdir().unwrap();
        assert!(matches!(
            page_text("shop", "../x", Some(dir.path())),
            Err(ReadError::Invalid(_))
        ));
    }

    #[test]
    fn a_secret_written_by_hand_on_disk_is_refused_on_read() {
        let dir = tempdir().unwrap();
        put(&page("feature-a", Status::Approved, "fine"), dir.path());
        let path = project_guide::guide_dir("shop", Some(dir.path())).join("feature-a.md");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("fine", &format!("fine glpat-{}", "a".repeat(24)));
        std::fs::write(&path, text).unwrap();
        let err = page_text("shop", "feature-a", Some(dir.path())).unwrap_err();
        assert!(matches!(err, ReadError::SecretRefused(_)));
        assert!(!err.to_string().contains("glpat"));
    }

    #[test]
    fn one_secret_page_or_broken_file_does_not_hide_the_index() {
        let dir = tempdir().unwrap();
        put(&page("feature-a", Status::Approved, "fine"), dir.path());
        put(&page("feature-b", Status::Approved, "fine"), dir.path());
        let gd = project_guide::guide_dir("shop", Some(dir.path()));
        let b = gd.join("feature-b.md");
        let bad = std::fs::read_to_string(&b)
            .unwrap()
            .replace("fine", &format!("fine glpat-{}", "a".repeat(24)));
        std::fs::write(&b, bad).unwrap();
        std::fs::write(gd.join("feature-c.md"), "not a page").unwrap();
        let json = index_json("shop", Some(dir.path())).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["pages"].as_array().unwrap().len(), 1);
        assert_eq!(v["pages"][0]["slug"], "feature-a");
        let skipped: Vec<&str> = v["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(skipped, ["feature-b", "feature-c"]);
        assert!(!json.contains("glpat"));
    }
}

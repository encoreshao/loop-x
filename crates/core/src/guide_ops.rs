//! What a person does to guide pages by hand: approve one, approve every draft, rewrite
//! one part, delete a page the plan no longer names.
//!
//! Every operation validates the alias and slug before any path is built, takes the
//! guide's single-flight slot (so it never races a build), changes pages only under the
//! guide lock, and re-syncs the profile's Features index afterwards (best effort).
//! Errors never carry page or body text.

use crate::guide_build;
use crate::guide_features;
use crate::guide_plan;
use crate::profile_build::RunGuard;
use crate::project_guide::{self, check_slug, Kind, Page, Part};
use crate::project_profile::{check_alias, Origin, Status};
use chrono::{DateTime, Utc};
use std::path::Path;

/// The most a hand-written part may hold, in bytes after trimming.
pub const MAX_PART_BYTES: usize = 64 * 1024;

const IN_PLAN: &str = "the page is in the current plan";

fn take_slot(alias: &str) -> Result<RunGuard, String> {
    guide_build::try_acquire(alias).ok_or_else(|| {
        format!("a guide run for {alias} is in progress; try again when it finishes")
    })
}

/// The slug was validated, so naming it echoes nothing unchecked.
fn missing(slug: &str) -> String {
    format!("the {slug} page does not exist")
}

/// Refuse a missing page before taking the lock, so a typo creates no guide directory.
/// Re-checked under the lock by each caller.
fn require_page(alias: &str, slug: &str, root: Option<&Path>) -> Result<(), String> {
    match project_guide::load(alias, slug, root)? {
        Some(_) => Ok(()),
        None => Err(missing(slug)),
    }
}

/// Point the profile's Features index at the guide as it now stands. A missing or
/// corrupt profile must not fail a page operation, so its error is dropped.
fn resync(alias: &str, now: DateTime<Utc>, root: Option<&Path>) {
    let _ = guide_features::sync_profile_features(alias, now, root);
}

/// Whether the saved plan names `slug`: architecture and roadmap always, the data model
/// when the plan has schema files, and each planned feature. False without a plan (or
/// with one that does not load) and for an invalid alias or slug.
pub fn in_plan(alias: &str, slug: &str, root: Option<&Path>) -> bool {
    if check_alias(alias).is_err() || check_slug(slug).is_err() {
        return false;
    }
    let Ok(Some(plan)) = guide_plan::load(alias, root) else {
        return false;
    };
    match slug {
        "architecture" | "roadmap" => true,
        "data-model" => !plan.data_model.files.is_empty(),
        s => plan.features.iter().any(|f| f.slug == s),
    }
}

/// `Ok(true)` when a draft was approved, `Ok(false)` when it already was (nothing
/// written), `Err` when the page does not exist.
pub fn approve_page(
    alias: &str,
    slug: &str,
    now: DateTime<Utc>,
    root: Option<&Path>,
) -> Result<bool, String> {
    check_alias(alias)?;
    check_slug(slug)?;
    let _slot = take_slot(alias)?;
    require_page(alias, slug, root)?;
    let flipped = project_guide::update_if_locked(alias, slug, root, |cur| {
        let mut p = cur.ok_or_else(|| missing(slug))?;
        if p.status == Status::Approved {
            return Ok(None);
        }
        p.status = Status::Approved;
        Ok(Some(p))
    })?
    .is_some();
    resync(alias, now, root);
    Ok(flipped)
}

/// Approve every draft page (in the plan or not), one lock per page. Pages approved
/// before a failure stay approved; the error names only the slugs that failed.
pub fn approve_all(alias: &str, now: DateTime<Utc>, root: Option<&Path>) -> Result<usize, String> {
    check_alias(alias)?;
    let _slot = take_slot(alias)?;
    let drafts: Vec<String> = project_guide::list(alias, root)?
        .pages
        .into_iter()
        .filter(|p| p.status == Status::Draft)
        .map(|p| p.slug)
        .collect();
    let mut count = 0;
    let mut failed = Vec::new();
    for slug in drafts {
        let res = project_guide::update_if_locked(alias, &slug, root, |cur| {
            // Re-checked under the lock: removed or approved meanwhile is nothing to do.
            Ok(cur.filter(|p| p.status == Status::Draft).map(|mut p| {
                p.status = Status::Approved;
                p
            }))
        });
        match res {
            Ok(Some(_)) => count += 1,
            Ok(None) => {}
            Err(_) => failed.push(slug),
        }
    }
    resync(alias, now, root);
    if failed.is_empty() {
        Ok(count)
    } else {
        Err(format!(
            "approved {count} pages; could not approve {}",
            failed.join(", ")
        ))
    }
}

/// Replace (or add) one part with a person's text. The part becomes Human-origin, so a
/// rebuild keeps it; nothing else on the page changes.
pub fn edit_part(
    alias: &str,
    slug: &str,
    key: &str,
    body: &str,
    now: DateTime<Utc>,
    root: Option<&Path>,
) -> Result<(), String> {
    check_alias(alias)?;
    let kind: Kind = check_slug(slug)?;
    if !kind.parts().iter().any(|(k, _)| *k == key) {
        return Err(format!("a {} page has no such part", kind.as_str()));
    }
    let text = body.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    if text.is_empty() {
        return Err("the text is empty; write `not stated` if that is what you mean".into());
    }
    if text.len() > MAX_PART_BYTES {
        return Err("the text is longer than 64 KB".into());
    }
    let _slot = take_slot(alias)?;
    require_page(alias, slug, root)?;
    project_guide::update_locked(alias, slug, root, |cur| {
        let mut p: Page = cur.ok_or_else(|| missing(slug))?;
        match p.parts.iter_mut().find(|x| x.key == key) {
            Some(part) => {
                part.body = text.to_string();
                part.origin = Origin::Human;
            }
            None => p.parts.push(Part {
                key: key.to_string(),
                body: text.to_string(),
                origin: Origin::Human,
            }),
        }
        p = project_guide::normalised(&p);
        project_guide::validate(&p)?;
        Ok(p)
    })?;
    resync(alias, now, root);
    Ok(())
}

/// Remove a page the current plan does not name. The file is snapshotted into
/// `.history/` first, under the guide lock, so the delete can be undone by a restore.
pub fn delete_page(alias: &str, slug: &str, root: Option<&Path>) -> Result<(), String> {
    check_alias(alias)?;
    check_slug(slug)?;
    let _slot = take_slot(alias)?;
    if in_plan(alias, slug, root) {
        return Err(IN_PLAN.into());
    }
    require_page(alias, slug, root)?;
    let dir = project_guide::guide_dir(alias, root);
    crate::file_lock::with_lock(&dir, || {
        let path = project_guide::page_path(alias, slug, root);
        if !path.is_file() {
            return Err(missing(slug));
        }
        project_guide::snapshot_current(alias, slug, root)
            .map_err(|_| format!("could not snapshot the {slug} page"))?;
        std::fs::remove_file(&path).map_err(|_| format!("could not delete the {slug} page"))
    })
    .map_err(|_| format!("could not lock the guide for {alias}"))??;
    resync(alias, Utc::now(), root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide_plan::{self, FeaturePlan, Plan};
    use crate::project_guide::{self, check_slug, Kind, Page, Part};
    use crate::project_profile::{self, Origin, Source, Status};
    use chrono::TimeZone;
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
    }

    fn later() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 14, 9, 30, 0).unwrap()
    }

    fn page(alias: &str, slug: &str, status: Status) -> Page {
        let kind = check_slug(slug).unwrap();
        Page {
            alias: alias.into(),
            slug: slug.into(),
            kind,
            title: format!("Title of {slug}"),
            status,
            generated_at: Some(now()),
            head_sha: Some("abc123".into()),
            summary: "A summary.".into(),
            related: vec![],
            parts: kind
                .parts()
                .iter()
                .map(|(k, _)| Part {
                    key: (*k).into(),
                    body: format!("Body of {k}."),
                    origin: Origin::Agent,
                })
                .collect(),
            sources: vec![Source {
                path: "app/x.rb".into(),
                sha256: "ff00".into(),
            }],
        }
    }

    fn save(p: &Page, root: &Path) {
        project_guide::save(p, Some(root)).unwrap();
    }

    fn feature(slug: &str, title: &str, summary: &str) -> FeaturePlan {
        FeaturePlan {
            slug: slug.into(),
            title: title.into(),
            summary: summary.into(),
            files: vec!["app/x.rb".into()],
            related: vec![],
        }
    }

    fn save_plan(alias: &str, root: &Path, schema: bool) {
        let mut plan = Plan {
            features: vec![
                feature("feature-a", "A", "Feature a."),
                feature("feature-b", "B", "Feature b."),
            ],
            ..Plan::default()
        };
        if schema {
            plan.data_model.files = vec!["db/schema.rb".into()];
        }
        guide_plan::save(alias, &plan, Some(root)).unwrap();
    }

    fn page_file(alias: &str, slug: &str, root: &Path) -> PathBuf {
        project_guide::guide_dir(alias, Some(root)).join(format!("{slug}.md"))
    }

    fn bytes(alias: &str, slug: &str, root: &Path) -> Vec<u8> {
        fs::read(page_file(alias, slug, root)).unwrap()
    }

    fn loaded(alias: &str, slug: &str, root: &Path) -> Page {
        project_guide::load(alias, slug, Some(root))
            .unwrap()
            .unwrap()
    }

    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                out.push(p.clone());
                if p.is_dir() {
                    walk(&p, out);
                }
            }
        }
    }

    fn tree(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    fn save_profile(alias: &str, root: &Path) {
        let mut sections = std::collections::BTreeMap::new();
        for k in project_profile::SectionKey::ALL {
            sections.insert(
                k,
                project_profile::Section {
                    summary: format!("{} summary", k.heading()),
                    body: "Old text.".into(),
                    last_verified: None,
                    sources: vec![],
                    origin: Origin::Agent,
                },
            );
        }
        let p = project_profile::Profile {
            alias: alias.into(),
            generated_at: None,
            head_sha: None,
            status: Status::Approved,
            links: vec![],
            sections,
        };
        project_profile::save(&p, Some(root)).unwrap();
    }

    fn reset_features(alias: &str, root: &Path) {
        project_profile::update_locked(alias, Some(root), |cur| {
            let mut p = cur.unwrap();
            let s = p
                .sections
                .get_mut(&project_profile::SectionKey::Features)
                .unwrap();
            s.summary = "Old summary".into();
            s.body = "Old text.".into();
            s.origin = Origin::Agent;
            Ok(p)
        })
        .unwrap();
    }

    fn features_body(alias: &str, root: &Path) -> String {
        project_profile::load(alias, Some(root))
            .unwrap()
            .unwrap()
            .sections[&project_profile::SectionKey::Features]
            .body
            .clone()
    }

    #[test]
    fn in_plan_follows_the_saved_plan() {
        let alias = "ops-inplan";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let all = [
            "architecture",
            "roadmap",
            "data-model",
            "feature-a",
            "feature-b",
            "feature-zzz",
        ];
        // No plan: nothing is in it.
        for s in all {
            assert!(!in_plan(alias, s, Some(r)), "{s} without a plan");
        }
        // A plan with no schema files.
        save_plan(alias, r, false);
        for s in ["architecture", "roadmap", "feature-a", "feature-b"] {
            assert!(in_plan(alias, s, Some(r)), "{s} with a plan");
        }
        assert!(!in_plan(alias, "data-model", Some(r)));
        assert!(!in_plan(alias, "feature-zzz", Some(r)));
        // With schema files the data model page is planned too.
        save_plan(alias, r, true);
        assert!(in_plan(alias, "data-model", Some(r)));
        assert!(in_plan(alias, "feature-a", Some(r)));
        assert!(!in_plan(alias, "feature-zzz", Some(r)));
        // A corrupt plan counts as no plan.
        fs::write(project_guide::plan_path(alias, Some(r)), "not json").unwrap();
        for s in all {
            assert!(!in_plan(alias, s, Some(r)), "{s} with a corrupt plan");
        }
        // Invalid input is never in the plan.
        save_plan(alias, r, true);
        assert!(!in_plan(alias, "../architecture", Some(r)));
        assert!(!in_plan("../x", "architecture", Some(r)));
    }

    #[test]
    fn approve_page_flips_a_draft_once_and_errors_on_a_missing_page() {
        let alias = "ops-approve";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let draft = page(alias, "feature-a", Status::Draft);
        save(&draft, r);
        let before = loaded(alias, "feature-a", r);

        assert_eq!(approve_page(alias, "feature-a", later(), Some(r)), Ok(true));
        let after = loaded(alias, "feature-a", r);
        assert_eq!(after.status, Status::Approved);
        // Only the status changed.
        let mut expected = before.clone();
        expected.status = Status::Approved;
        assert_eq!(after, expected);
        assert_eq!(after.generated_at, Some(now()));
        let history = project_guide::history(alias, "feature-a", Some(r)).len();
        assert_eq!(history, 1);

        // A second approval is a no-op: no write, no snapshot.
        let file_before = bytes(alias, "feature-a", r);
        assert_eq!(
            approve_page(alias, "feature-a", later(), Some(r)),
            Ok(false)
        );
        assert_eq!(bytes(alias, "feature-a", r), file_before);
        assert_eq!(
            project_guide::history(alias, "feature-a", Some(r)).len(),
            history
        );

        // A missing page is an error and is not created.
        assert!(approve_page(alias, "feature-nope", later(), Some(r)).is_err());
        assert!(!page_file(alias, "feature-nope", r).exists());
        // Nor in a project that has no guide at all.
        let empty = tempfile::tempdir().unwrap();
        assert!(approve_page(alias, "architecture", later(), Some(empty.path())).is_err());
        assert!(tree(empty.path()).is_empty());
    }

    #[test]
    fn approve_all_approves_every_draft_and_counts_them() {
        let alias = "ops-approve-all";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        save_plan(alias, r, false);
        save(&page(alias, "architecture", Status::Draft), r);
        save(&page(alias, "feature-a", Status::Draft), r);
        // Out of the plan, still a draft: approved too.
        save(&page(alias, "feature-zzz", Status::Draft), r);
        save(&page(alias, "roadmap", Status::Approved), r);
        let roadmap_before = bytes(alias, "roadmap", r);

        assert_eq!(approve_all(alias, later(), Some(r)), Ok(3));
        let pages = project_guide::list(alias, Some(r)).unwrap().pages;
        assert_eq!(pages.len(), 4);
        assert!(pages.iter().all(|p| p.status == Status::Approved));
        // The already approved page was not rewritten.
        assert_eq!(bytes(alias, "roadmap", r), roadmap_before);
        assert!(project_guide::history(alias, "roadmap", Some(r)).is_empty());

        // Nothing left to approve.
        assert_eq!(approve_all(alias, later(), Some(r)), Ok(0));
        // A project with no guide approves nothing and creates nothing.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(approve_all(alias, later(), Some(empty.path())), Ok(0));
        assert!(tree(empty.path()).is_empty());
    }

    // --- a minimal copy of guide_build's test agent and repo, for the rebuild check ---

    struct Fake {
        plan: &'static str,
        rewrite: bool,
        calls: RefCell<usize>,
    }

    fn parts_reply(kind: Kind, prefix: &str) -> String {
        kind.parts()
            .iter()
            .map(|(k, h)| format!("<!-- part: {k} origin: agent -->\n## {h}\n\n{prefix} {k}.\n\n"))
            .collect()
    }

    impl crate::profile_build::Agent for Fake {
        fn run(&self, prompt: &str) -> Result<String, String> {
            *self.calls.borrow_mut() += 1;
            if prompt.contains("\"features\"") && prompt.contains("Reply with the JSON only") {
                return Ok(self.plan.to_string());
            }
            let kind = if prompt.contains("<!-- part: what origin: agent -->") {
                Kind::Feature
            } else if prompt.contains("<!-- part: entities origin: agent -->") {
                Kind::DataModel
            } else {
                Kind::Architecture
            };
            let prefix = if self.rewrite {
                "Rewritten body of"
            } else {
                "Body of"
            };
            Ok(parts_reply(kind, prefix))
        }
    }

    const PLAN: &str = r#"{"features":[
      {"slug":"feature-orders","title":"Orders","summary":"Taking orders.","files":["app/order.rb"],"related":[]}],
      "architecture":{"files":["config/routes.rb"]},"data_model":{"files":[]}}"#;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |rel: &str, t: &str| {
            let p = d.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, t).unwrap();
        };
        w("README.md", "# Shop\nSells things.\n");
        w("app/order.rb", "class Order\nend\n");
        w("config/routes.rb", "get '/orders'\n");
        for args in [&["init", "-q"][..], &["add", "-A"][..]] {
            Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(args)
                .status()
                .unwrap();
        }
        d
    }

    fn build(alias: &str, d: &Path, rewrite: bool, root: &Path) {
        let f = Fake {
            plan: PLAN,
            rewrite,
            calls: RefCell::new(0),
        };
        crate::guide_build::build_guide(alias, d, "", "", &[], &f, now(), Some(root), None)
            .unwrap();
        assert!(*f.calls.borrow() > 1);
    }

    #[test]
    fn editing_a_part_marks_it_human_keeps_everything_else_and_survives_a_rebuild_merge() {
        let alias = "ops-edit";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();

        // A saved page missing one part: the edit creates it, in the kind's order.
        let mut p = page(alias, "feature-a", Status::Draft);
        p.related = vec!["feature-b".into()];
        p.parts.retain(|x| x.key != "value");
        save(&p, r);
        let before = loaded(alias, "feature-a", r);
        let history = project_guide::history(alias, "feature-a", Some(r)).len();

        edit_part(
            alias,
            "feature-a",
            "value",
            "  Saves time.\r\nCuts cost.\r\n  ",
            later(),
            Some(r),
        )
        .unwrap();
        let after = loaded(alias, "feature-a", r);
        let keys: Vec<&str> = after.parts.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["what", "value", "how", "associations", "key-points"]);
        let v = after.parts.iter().find(|x| x.key == "value").unwrap();
        assert_eq!(
            (v.body.as_str(), v.origin),
            ("Saves time.\nCuts cost.", Origin::Human)
        );
        assert!(!String::from_utf8(bytes(alias, "feature-a", r))
            .unwrap()
            .contains('\r'));
        // Everything else is as it was.
        assert_eq!(after.status, before.status);
        assert_eq!(after.generated_at, before.generated_at);
        assert_eq!(after.head_sha, before.head_sha);
        assert_eq!(after.sources, before.sources);
        assert_eq!(after.related, before.related);
        assert_eq!(after.title, before.title);
        assert_eq!(after.summary, before.summary);
        let others: Vec<&Part> = after.parts.iter().filter(|x| x.key != "value").collect();
        let before_parts: Vec<&Part> = before.parts.iter().collect();
        assert_eq!(others, before_parts);
        assert_eq!(
            project_guide::history(alias, "feature-a", Some(r)).len(),
            history + 1
        );

        // Editing an existing agent part replaces it and makes it human.
        edit_part(
            alias,
            "feature-a",
            "what",
            "Takes payments.",
            later(),
            Some(r),
        )
        .unwrap();
        let w = loaded(alias, "feature-a", r);
        let what = w.parts.iter().find(|x| x.key == "what").unwrap();
        assert_eq!(
            (what.body.as_str(), what.origin),
            ("Takes payments.", Origin::Human)
        );
        assert_eq!(w.parts.len(), 5);

        // Survives a rebuild through guide_build::build_guide (the merge keeps Human parts).
        let d = repo();
        let ra = "ops-edit-rebuild";
        build(ra, d.path(), false, r);
        edit_part(
            ra,
            "feature-orders",
            "key-points",
            "Totals are in cents.\r\nWritten by a person.",
            later(),
            Some(r),
        )
        .unwrap();
        build(ra, d.path(), true, r);
        let rebuilt = loaded(ra, "feature-orders", r);
        let k = rebuilt
            .parts
            .iter()
            .find(|x| x.key == "key-points")
            .unwrap();
        assert_eq!(
            (k.body.as_str(), k.origin),
            ("Totals are in cents.\nWritten by a person.", Origin::Human)
        );
        let what = rebuilt.parts.iter().find(|x| x.key == "what").unwrap();
        assert_eq!(
            (what.body.as_str(), what.origin),
            ("Rewritten body of what.", Origin::Agent)
        );
    }

    #[test]
    fn editing_refuses_unknown_parts_empty_text_secrets_and_missing_pages() {
        let alias = "ops-edit-refuse";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        save(&page(alias, "feature-a", Status::Draft), r);
        let file_before = bytes(alias, "feature-a", r);
        let history = project_guide::history(alias, "feature-a", Some(r)).len();
        let unchanged = || {
            assert_eq!(bytes(alias, "feature-a", r), file_before);
            assert_eq!(
                project_guide::history(alias, "feature-a", Some(r)).len(),
                history
            );
        };

        // A part of another kind, and a part of no kind.
        assert!(edit_part(alias, "feature-a", "overview", "x", later(), Some(r)).is_err());
        unchanged();
        let e = edit_part(alias, "feature-a", "bogus-key", "x", later(), Some(r)).unwrap_err();
        assert!(!e.contains("bogus-key"));
        unchanged();
        // Empty text, before or after trimming.
        assert!(edit_part(alias, "feature-a", "what", "", later(), Some(r)).is_err());
        unchanged();
        assert!(edit_part(alias, "feature-a", "what", "  \r\n\t ", later(), Some(r)).is_err());
        unchanged();
        // Over 64 KB.
        let big = "a".repeat(64 * 1024 + 1);
        assert!(edit_part(alias, "feature-a", "what", &big, later(), Some(r)).is_err());
        unchanged();
        // Exactly 64 KB is allowed (checked on a second page so this one stays as it was).
        save(&page(alias, "feature-b", Status::Draft), r);
        let max = "a".repeat(64 * 1024);
        edit_part(alias, "feature-b", "what", &max, later(), Some(r)).unwrap();
        // Secret text: refused, and the error does not echo it.
        let secret = "token glpat-AAAAAAAAAAAAAAAAAAAA here";
        let e = edit_part(alias, "feature-a", "what", secret, later(), Some(r)).unwrap_err();
        assert!(!e.contains("glpat-"), "error echoed the secret");
        unchanged();
        // A missing page: refused, not created.
        assert!(edit_part(alias, "feature-nope", "what", "x", later(), Some(r)).is_err());
        assert!(!page_file(alias, "feature-nope", r).exists());
        unchanged();
    }

    #[test]
    fn deleting_a_page_in_the_plan_is_refused_and_one_outside_it_is_removed_with_a_snapshot() {
        let alias = "ops-delete";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        save_plan(alias, r, false);
        save(&page(alias, "architecture", Status::Approved), r);
        save(&page(alias, "feature-a", Status::Draft), r);
        save(&page(alias, "feature-zzz", Status::Draft), r);

        for slug in ["feature-a", "architecture"] {
            let before = bytes(alias, slug, r);
            assert_eq!(
                delete_page(alias, slug, Some(r)),
                Err("the page is in the current plan".to_string())
            );
            assert_eq!(bytes(alias, slug, r), before);
            assert!(project_guide::history(alias, slug, Some(r)).is_empty());
        }

        let before = bytes(alias, "feature-zzz", r);
        assert!(project_guide::history(alias, "feature-zzz", Some(r)).is_empty());
        delete_page(alias, "feature-zzz", Some(r)).unwrap();
        assert!(!page_file(alias, "feature-zzz", r).exists());
        let h = project_guide::history(alias, "feature-zzz", Some(r));
        assert_eq!(h.len(), 1);
        assert_eq!(fs::read(&h[0]).unwrap(), before);
        let slugs: Vec<String> = project_guide::list(alias, Some(r))
            .unwrap()
            .pages
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(slugs, ["architecture", "feature-a"]);

        // Deleting it again, or a page that never existed, is an error.
        assert!(delete_page(alias, "feature-zzz", Some(r)).is_err());
        assert_eq!(
            project_guide::history(alias, "feature-zzz", Some(r)).len(),
            1
        );
        assert!(delete_page(alias, "feature-nope", Some(r)).is_err());
    }

    #[test]
    fn bad_slugs_and_aliases_create_nothing() {
        // The memory root sits one level down so an escape to its parent would show.
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        fs::create_dir(&root).unwrap();
        let r = root.as_path();
        let alias = "ops-bad";
        for slug in ["../x", "Feature-A", "", "feature-a/../../x"] {
            assert!(
                approve_page(alias, slug, later(), Some(r)).is_err(),
                "{slug}"
            );
            assert!(
                edit_part(alias, slug, "what", "x", later(), Some(r)).is_err(),
                "{slug}"
            );
            assert!(delete_page(alias, slug, Some(r)).is_err(), "{slug}");
            assert!(!in_plan(alias, slug, Some(r)), "{slug}");
        }
        for bad in ["../x", "", "a/b", ".."] {
            assert!(approve_page(bad, "feature-a", later(), Some(r)).is_err());
            assert!(approve_all(bad, later(), Some(r)).is_err());
            assert!(edit_part(bad, "feature-a", "what", "x", later(), Some(r)).is_err());
            assert!(delete_page(bad, "feature-a", Some(r)).is_err());
            assert!(!in_plan(bad, "feature-a", Some(r)));
        }
        assert!(tree(r).is_empty(), "created {:?}", tree(r));
        // Nothing escaped the root either.
        assert_eq!(tree(outer.path()), std::slice::from_ref(&root));
        // The slot was not left held by any of them.
        assert!(crate::guide_build::try_acquire(alias).is_some());
    }

    #[test]
    fn operations_refuse_while_a_guide_run_holds_the_slot() {
        let alias = "ops-busy";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        save_plan(alias, r, false);
        save(&page(alias, "feature-a", Status::Draft), r);
        save(&page(alias, "feature-zzz", Status::Draft), r);
        let snapshot = tree(r);
        let a = bytes(alias, "feature-a", r);
        let z = bytes(alias, "feature-zzz", r);
        let busy = format!("a guide run for {alias} is in progress; try again when it finishes");

        let guard = crate::guide_build::try_acquire(alias).unwrap();
        assert_eq!(
            approve_page(alias, "feature-a", later(), Some(r)),
            Err(busy.clone())
        );
        assert_eq!(approve_all(alias, later(), Some(r)), Err(busy.clone()));
        assert_eq!(
            edit_part(alias, "feature-a", "what", "New.", later(), Some(r)),
            Err(busy.clone())
        );
        assert_eq!(
            delete_page(alias, "feature-zzz", Some(r)),
            Err(busy.clone())
        );
        assert_eq!(bytes(alias, "feature-a", r), a);
        assert_eq!(bytes(alias, "feature-zzz", r), z);
        assert_eq!(tree(r), snapshot);
        drop(guard);

        // Each operation releases the slot on success and on failure alike.
        let free = || assert!(crate::guide_build::try_acquire(alias).is_some());
        assert_eq!(approve_page(alias, "feature-a", later(), Some(r)), Ok(true));
        free();
        assert!(approve_page(alias, "feature-nope", later(), Some(r)).is_err());
        free();
        edit_part(alias, "feature-a", "what", "New.", later(), Some(r)).unwrap();
        free();
        assert!(edit_part(alias, "feature-a", "bogus", "New.", later(), Some(r)).is_err());
        free();
        assert!(edit_part(alias, "feature-nope", "what", "New.", later(), Some(r)).is_err());
        free();
        assert_eq!(approve_all(alias, later(), Some(r)), Ok(1));
        free();
        assert!(delete_page(alias, "feature-a", Some(r)).is_err());
        free();
        delete_page(alias, "feature-zzz", Some(r)).unwrap();
        free();
    }

    #[test]
    fn every_operation_resyncs_the_profile_features_index() {
        let alias = "ops-sync";
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        save_plan(alias, r, false);
        save_profile(alias, r);
        save(&page(alias, "feature-a", Status::Draft), r);
        save(&page(alias, "feature-b", Status::Draft), r);
        save(&page(alias, "feature-zzz", Status::Draft), r);
        assert_eq!(features_body(alias, r), "Old text.");

        let both_draft = "- **A**: Feature a. (guide page `feature-a`, draft)\n\
                          - **B**: Feature b. (guide page `feature-b`, draft)";
        let a_approved = "- **A**: Feature a. (guide page `feature-a`)\n\
                          - **B**: Feature b. (guide page `feature-b`, draft)";
        let both_approved = "- **A**: Feature a. (guide page `feature-a`)\n\
                             - **B**: Feature b. (guide page `feature-b`)";

        // Editing a part re-syncs (both still drafts).
        edit_part(alias, "feature-b", "what", "Edited.", later(), Some(r)).unwrap();
        assert_eq!(features_body(alias, r), both_draft);

        // Approving feature-a flips its ", draft" off.
        reset_features(alias, r);
        assert_eq!(approve_page(alias, "feature-a", later(), Some(r)), Ok(true));
        assert_eq!(features_body(alias, r), a_approved);
        let sec = project_profile::load(alias, Some(r))
            .unwrap()
            .unwrap()
            .sections[&project_profile::SectionKey::Features]
            .clone();
        assert_eq!(sec.last_verified, Some(later()));

        // Approving all re-syncs.
        reset_features(alias, r);
        assert_eq!(approve_all(alias, later(), Some(r)), Ok(2));
        assert_eq!(features_body(alias, r), both_approved);

        // Deleting an out-of-plan page re-syncs and leaves the index intact.
        reset_features(alias, r);
        delete_page(alias, "feature-zzz", Some(r)).unwrap();
        assert_eq!(features_body(alias, r), both_approved);
        assert!(project_profile::load(alias, Some(r)).unwrap().is_some());
    }
}

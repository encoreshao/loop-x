//! Once a guide has a plan, its pages are the project's feature documentation and the
//! profile's Features section becomes an index of them, written without an AI call.

use crate::guide_plan;
use crate::project_guide;
use crate::project_profile::{self, Origin, Section, SectionKey, Status};
use chrono::{DateTime, Utc};
use std::path::Path;

/// True once the project's guide has a plan that loads.
pub fn owns_features(alias: &str, root: Option<&Path>) -> bool {
    matches!(guide_plan::load(alias, root), Ok(Some(_)))
}

/// The Features section the guide writes: one line per planned feature page, in plan
/// order, `, draft` while a page is missing or not approved. `None` without a plan.
pub fn features_section(alias: &str, now: DateTime<Utc>, root: Option<&Path>) -> Option<Section> {
    let plan = guide_plan::load(alias, root).ok().flatten()?;
    let pages = project_guide::list(alias, root)
        .map(|l| l.pages)
        .unwrap_or_default();
    let body = plan
        .features
        .iter()
        .map(|f| {
            let approved = pages
                .iter()
                .any(|p| p.slug == f.slug && p.status == Status::Approved);
            let draft = if approved { "" } else { ", draft" };
            format!(
                "- **{}**: {} (guide page `{}`{draft})",
                f.title, f.summary, f.slug
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(Section {
        summary: format!("{} feature pages", plan.features.len()),
        body: if body.is_empty() {
            "not stated".into()
        } else {
            body
        },
        last_verified: Some(now),
        sources: Vec::new(),
        origin: Origin::Agent,
    })
}

/// Nothing to write: the stored section is the person's, or already says the same.
fn keep(cur: Option<&Section>, fresh: &Section) -> bool {
    cur.is_some_and(|s| {
        s.origin == Origin::Human || (s.summary == fresh.summary && s.body == fresh.body)
    })
}

/// Rewrite the profile's Features section from the guide plan. Nothing happens without
/// a plan or a profile, over a human-written section, or when the text is unchanged
/// (compared before saving, so an unchanged sync leaves no history snapshot).
pub fn sync_profile_features(
    alias: &str,
    now: DateTime<Utc>,
    root: Option<&Path>,
) -> Result<(), String> {
    let Some(fresh) = features_section(alias, now, root) else {
        return Ok(());
    };
    let Some(cur) = project_profile::load(alias, root)? else {
        return Ok(());
    };
    if keep(cur.sections.get(&SectionKey::Features), &fresh) {
        return Ok(());
    }
    project_profile::update_locked(alias, root, |cur| {
        let mut p = cur.ok_or("the profile was removed meanwhile")?;
        // Re-checked under the lock: a person may have edited it since the load.
        if !keep(p.sections.get(&SectionKey::Features), &fresh) {
            p.sections.insert(SectionKey::Features, fresh);
        }
        Ok(p)
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide_plan::{FeaturePlan, Plan};
    use crate::project_guide::{Kind, Page};
    use crate::project_profile::Profile;
    use chrono::TimeZone;
    use std::collections::BTreeMap;

    const ALIAS: &str = "shop";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
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

    fn save_plan(root: &Path) {
        let plan = Plan {
            features: vec![
                feature("feature-orders", "Orders", "Taking orders."),
                feature("feature-cart", "Cart", "Holding items."),
            ],
            ..Plan::default()
        };
        guide_plan::save(ALIAS, &plan, Some(root)).unwrap();
    }

    fn save_page(root: &Path, slug: &str, title: &str, status: Status) {
        let page = Page {
            alias: ALIAS.into(),
            slug: slug.into(),
            kind: Kind::Feature,
            title: title.into(),
            status,
            generated_at: Some(now()),
            head_sha: None,
            summary: "s".into(),
            related: vec![],
            parts: vec![],
            sources: vec![],
        };
        project_guide::save(&page, Some(root)).unwrap();
    }

    fn section(text: &str, origin: Origin) -> Section {
        Section {
            summary: format!("{text} summary"),
            body: format!("{text} body."),
            last_verified: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
            sources: vec![],
            origin,
        }
    }

    fn save_profile(root: &Path, features_origin: Origin) -> Profile {
        let mut sections = BTreeMap::new();
        for k in SectionKey::ALL {
            let origin = if k == SectionKey::Features {
                features_origin
            } else {
                Origin::Agent
            };
            sections.insert(k, section(k.heading(), origin));
        }
        let p = Profile {
            alias: ALIAS.into(),
            generated_at: Some(now()),
            head_sha: Some("abc".into()),
            status: Status::Approved,
            links: vec![],
            sections,
        };
        project_profile::save(&p, Some(root)).unwrap();
        project_profile::load(ALIAS, Some(root)).unwrap().unwrap()
    }

    fn loaded(root: &Path) -> Profile {
        project_profile::load(ALIAS, Some(root)).unwrap().unwrap()
    }

    #[test]
    fn owns_features_once_a_plan_is_saved() {
        let root = tempfile::tempdir().unwrap();
        assert!(!owns_features(ALIAS, Some(root.path())));
        assert!(features_section(ALIAS, now(), Some(root.path())).is_none());
        save_plan(root.path());
        assert!(owns_features(ALIAS, Some(root.path())));
    }

    #[test]
    fn the_section_lists_plan_features_in_order_with_draft_markers() {
        let root = tempfile::tempdir().unwrap();
        save_plan(root.path());
        // Orders is a draft page; Cart's page is missing (planned, failed to write).
        save_page(root.path(), "feature-orders", "Orders", Status::Draft);
        let s = features_section(ALIAS, now(), Some(root.path())).unwrap();
        assert_eq!(s.summary, "2 feature pages");
        assert_eq!(
            s.body,
            "- **Orders**: Taking orders. (guide page `feature-orders`, draft)\n\
             - **Cart**: Holding items. (guide page `feature-cart`, draft)"
        );
        assert_eq!(s.origin, Origin::Agent);
        assert!(s.sources.is_empty());
        assert_eq!(s.last_verified, Some(now()));
        save_page(root.path(), "feature-cart", "Cart", Status::Approved);
        let s = features_section(ALIAS, now(), Some(root.path())).unwrap();
        assert!(s
            .body
            .ends_with("- **Cart**: Holding items. (guide page `feature-cart`)"));
    }

    #[test]
    fn sync_replaces_only_the_features_section() {
        let root = tempfile::tempdir().unwrap();
        let before = save_profile(root.path(), Origin::Agent);
        save_plan(root.path());
        sync_profile_features(ALIAS, now(), Some(root.path())).unwrap();
        let after = loaded(root.path());
        let feats = &after.sections[&SectionKey::Features];
        assert_eq!(feats.summary, "2 feature pages");
        assert!(feats
            .body
            .starts_with("- **Orders**: Taking orders. (guide page `feature-orders`, draft)"));
        for k in SectionKey::ALL {
            if k != SectionKey::Features {
                assert_eq!(after.sections[&k], before.sections[&k], "{k:?}");
            }
        }
        assert_eq!(after.status, before.status);
        assert_eq!(after.head_sha, before.head_sha);
        assert_eq!(after.generated_at, before.generated_at);
    }

    #[test]
    fn a_human_features_section_is_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        let before = save_profile(root.path(), Origin::Human);
        save_plan(root.path());
        let hist = project_profile::history(ALIAS, Some(root.path())).len();
        sync_profile_features(ALIAS, now(), Some(root.path())).unwrap();
        assert_eq!(loaded(root.path()), before);
        assert_eq!(
            project_profile::history(ALIAS, Some(root.path())).len(),
            hist
        );
    }

    #[test]
    fn no_profile_is_ok_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        save_plan(root.path());
        assert_eq!(
            sync_profile_features(ALIAS, now(), Some(root.path())),
            Ok(())
        );
        assert!(!project_profile::profile_path(ALIAS, Some(root.path())).exists());
        // And without a plan, a profile is left alone.
        let other = tempfile::tempdir().unwrap();
        let before = save_profile(other.path(), Origin::Agent);
        sync_profile_features(ALIAS, now(), Some(other.path())).unwrap();
        assert_eq!(loaded(other.path()), before);
    }

    #[test]
    fn a_second_sync_with_the_same_text_writes_no_snapshot() {
        let root = tempfile::tempdir().unwrap();
        save_profile(root.path(), Origin::Agent);
        save_plan(root.path());
        sync_profile_features(ALIAS, now(), Some(root.path())).unwrap();
        let hist = project_profile::history(ALIAS, Some(root.path())).len();
        assert_eq!(hist, 1);
        let later = now() + chrono::Duration::hours(1);
        sync_profile_features(ALIAS, later, Some(root.path())).unwrap();
        assert_eq!(
            project_profile::history(ALIAS, Some(root.path())).len(),
            hist
        );
    }

    #[test]
    fn approving_a_page_drops_its_draft_marker_on_the_next_sync() {
        let root = tempfile::tempdir().unwrap();
        save_profile(root.path(), Origin::Agent);
        save_plan(root.path());
        save_page(root.path(), "feature-orders", "Orders", Status::Draft);
        sync_profile_features(ALIAS, now(), Some(root.path())).unwrap();
        let body = loaded(root.path()).sections[&SectionKey::Features]
            .body
            .clone();
        assert!(
            body.contains("(guide page `feature-orders`, draft)"),
            "{body}"
        );
        save_page(root.path(), "feature-orders", "Orders", Status::Approved);
        sync_profile_features(ALIAS, now(), Some(root.path())).unwrap();
        let body = loaded(root.path()).sections[&SectionKey::Features]
            .body
            .clone();
        assert!(body.contains("(guide page `feature-orders`)"), "{body}");
        assert!(
            body.contains("(guide page `feature-cart`, draft)"),
            "{body}"
        );
    }
}

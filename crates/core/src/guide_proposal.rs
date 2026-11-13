//! `guide-page` proposals: a rebuild of an approved guide page is held, not written;
//! each held page becomes an approval a person decides, and an approved one is applied
//! over the stored page (its Human-origin parts kept) under the guide lock.
//!
//! An approval's target is a hand-editable file: its alias and slug are validated
//! before any path is built, its page text is parsed and secret-checked before anything
//! is written, and errors never carry page text.

use crate::approvals::{self, NewApproval};
use crate::guide_build::{self, BuildReport};
use crate::guide_features;
use crate::project_guide::{self, check_slug, Page};
use crate::project_profile::{check_alias, one_line, Origin, Status};
use crate::sha256;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub use crate::profile_sync::ApplyOutcome;

pub const KIND: &str = "guide-page";

/// A guide proposal waits for a person; it does not time out (ten years).
const NO_EXPIRY_SECONDS: i64 = 10 * 365 * 24 * 3600;

fn hash(text: &str) -> String {
    sha256::hex(&sha256::sha256(text.as_bytes()))
}

fn stale(slug: &str) -> String {
    format!(
        "the {slug} page changed since this proposal was raised; rebuild the page to get a new proposal"
    )
}

/// The parts' (key, body) pairs, as stored (normalised by a render/parse round trip).
fn part_bodies(p: &Page) -> Result<Vec<(String, String)>, String> {
    let back = project_guide::parse(&project_guide::render(p))
        .map_err(|_| "the page does not round-trip through its markdown form".to_string())?;
    let mut v: Vec<_> = back.parts.into_iter().map(|x| (x.key, x.body)).collect();
    v.sort();
    Ok(v)
}

/// A pending or dismissed proposal with the same new text, or an approved one that is
/// not applied yet and can still be (the page still holds its old text).
fn already_raised(
    approvals_dir: &Path,
    alias: &str,
    slug: &str,
    new_hash: &str,
    cur_hash: &str,
    root: Option<&Path>,
) -> bool {
    approvals::all(approvals_dir).iter().any(|a| {
        let t = &a["target"];
        if a["kind"] != KIND || a["project"] != alias || t["slug"] != slug {
            return false;
        }
        if t["new_hash"] != new_hash {
            return false;
        }
        match a["status"].as_str() {
            Some("pending") | Some("denied") => true,
            Some("approved") => {
                t["old_hash"] == cur_hash
                    && a["id"]
                        .as_str()
                        .is_some_and(|id| !is_applied(alias, id, root))
            }
            _ => false,
        }
    })
}

/// Raise a proposal for a held (rebuilt, not written) page. `Ok(None)` when the
/// rebuild changes no part, or the same text is already pending, dismissed or
/// approved-but-unapplied.
pub fn raise(
    alias: &str,
    held: &Page,
    approvals_dir: &Path,
    root: Option<&Path>,
    now: DateTime<Utc>,
) -> Result<Option<String>, String> {
    check_alias(alias)?;
    if held.alias != alias {
        return Err(format!("the held page is not a page of {alias}"));
    }
    let slug = held.slug.as_str();
    check_slug(slug)?;
    let stored = project_guide::load(alias, slug, root)?
        .ok_or_else(|| format!("the {slug} page of {alias} is not stored"))?;
    if stored.status != Status::Approved {
        return Err(format!("the {slug} page of {alias} is not approved"));
    }
    project_guide::validate(held)?;
    if part_bodies(held)? == part_bodies(&stored)? {
        return Ok(None);
    }
    let old = project_guide::render(&stored);
    let new = project_guide::render(held);
    let (old_hash, new_hash) = (hash(&old), hash(&new));
    if already_raised(approvals_dir, alias, slug, &new_hash, &old_hash, root) {
        return Ok(None);
    }
    let target = json!({
        "alias": alias,
        "slug": slug,
        "old_hash": old_hash,
        "new_hash": new_hash,
        "old": old,
        "new": new,
    });
    let approval = approvals::create(
        approvals_dir,
        &NewApproval {
            kind: KIND.into(),
            run_id: String::new(),
            project: alias.to_string(),
            issue_iid: None,
            title: format!("Update the {slug} guide page of {alias}"),
            branch: String::new(),
            target_branch: String::new(),
            summary: one_line(&held.summary),
            timeout_seconds: NO_EXPIRY_SECONDS,
            target: Some(target),
        },
        now,
    )
    .map_err(|e| e.to_string())?;
    Ok(approval["id"].as_str().map(str::to_string))
}

/// [`raise`] for every page a build held; pages that error are skipped.
pub fn raise_all(
    alias: &str,
    report: &BuildReport,
    approvals_dir: &Path,
    root: Option<&Path>,
    now: DateTime<Utc>,
) -> Vec<String> {
    report
        .held
        .iter()
        .filter_map(|p| raise(alias, p, approvals_dir, root, now).ok().flatten())
        .collect()
}

/// A proposal's checked contents: alias and slug validated, the new page parsed, for
/// this page, and free of secrets.
struct Checked {
    alias: String,
    slug: String,
    old_hash: String,
    new: Page,
}

fn check_target(id: &str, a: &Value) -> Result<Checked, String> {
    if a["kind"] != KIND {
        return Err(format!("approval {id} is not a guide page proposal"));
    }
    let t = &a["target"];
    let alias = t["alias"].as_str().ok_or("the proposal names no project")?;
    let slug = t["slug"].as_str().ok_or("the proposal names no page")?;
    check_alias(alias).map_err(|_| "the proposal names an invalid project".to_string())?;
    check_slug(slug).map_err(|_| "the proposal names an invalid page".to_string())?;
    let old_hash = t["old_hash"]
        .as_str()
        .ok_or("the proposal has no old page hash")?;
    let text = t["new"].as_str().ok_or("the proposal has no page text")?;
    let new = project_guide::parse(text)
        .map_err(|_| "the proposed page text did not parse".to_string())?;
    if new.alias != alias || new.slug != slug {
        return Err("the proposed page text is for another page".into());
    }
    project_guide::validate(&new)?;
    Ok(Checked {
        alias: alias.to_string(),
        slug: slug.to_string(),
        old_hash: old_hash.to_string(),
        new,
    })
}

/// The proposal's page over `stored`: the stored Human-origin parts win, status
/// Approved; everything else is the proposal's.
fn merged(new: &Page, stored: &Page) -> Page {
    let mut out = new.clone();
    out.status = Status::Approved;
    for h in stored.parts.iter().filter(|p| p.origin == Origin::Human) {
        match out.parts.iter_mut().find(|p| p.key == h.key) {
            Some(p) => *p = h.clone(),
            None => out.parts.push(h.clone()),
        }
    }
    out
}

/// Write an approved proposal over the stored page under the guide lock (the old page
/// is snapshotted), then mark it applied. Refuses a proposal that is not approved or
/// whose page changed since it was raised.
pub fn apply_proposal(
    id: &str,
    root: Option<&Path>,
    approvals_dir: &Path,
) -> Result<ApplyOutcome, String> {
    let a = approvals::get(approvals_dir, id).ok_or_else(|| format!("no approval {id}"))?;
    let c = check_target(id, &a)?;
    if a["status"] != "approved" {
        return Err(format!(
            "proposal {id} is {}, not approved",
            a["status"].as_str().unwrap_or("?")
        ));
    }
    let marker = applied_marker(&c.alias, id, root)?;
    if marker.exists() {
        return Ok(ApplyOutcome::AlreadyApplied);
    }
    let saved = project_guide::update_if_locked(&c.alias, &c.slug, root, |cur| {
        let cur = cur.ok_or_else(|| format!("the {} page is not stored", c.slug))?;
        let result = merged(&c.new, &cur);
        if hash(&project_guide::render(&cur)) == c.old_hash {
            project_guide::validate(&result)?;
            return Ok(Some(result));
        }
        // Saved before but the marker write failed: the page already is the result.
        if project_guide::render(&cur) == project_guide::render(&result) {
            return Ok(None);
        }
        Err(stale(&c.slug))
    })?;
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&marker, b"applied\n").map_err(|e| e.to_string())?;
    Ok(if saved.is_some() {
        ApplyOutcome::Applied
    } else {
        ApplyOutcome::AlreadyApplied
    })
}

/// Approve a guide page proposal and apply it, holding the guide slot so no build
/// rewrites the page between the check and the write. A pending proposal is approved
/// only while the page still holds its old text and the new text passes the checks.
/// Returns the approval as stored. Errors are content-free.
pub fn approve_and_apply(
    id: &str,
    by: &str,
    root: Option<&Path>,
    approvals_dir: &Path,
    now: DateTime<Utc>,
) -> Result<Value, String> {
    let a = approvals::get(approvals_dir, id).ok_or_else(|| format!("no approval {id}"))?;
    let c = check_target(id, &a)?;
    let alias = c.alias.as_str();
    let _slot = guide_build::try_acquire(alias).ok_or_else(|| {
        format!("a guide run for {alias} is in progress; try again when it finishes")
    })?;
    match a["status"].as_str() {
        Some("pending") => {
            let cur = project_guide::load(alias, &c.slug, root)?
                .ok_or_else(|| format!("the {} page is not stored", c.slug))?;
            if hash(&project_guide::render(&cur)) != c.old_hash {
                return Err(stale(&c.slug));
            }
            project_guide::validate(&merged(&c.new, &cur))?;
            approvals::decide(approvals_dir, id, "approve", by, now).map_err(|e| e.to_string())?;
        }
        Some("approved") => {}
        other => {
            return Err(format!(
                "proposal {id} was already decided ({})",
                other.unwrap_or("?")
            ))
        }
    }
    apply_proposal(id, root, approvals_dir)?;
    // Best effort: the page is written; a failed index refresh is redone next sync.
    let _ = guide_features::sync_profile_features(alias, now, root);
    approvals::get(approvals_dir, id).ok_or_else(|| format!("no approval {id}"))
}

/// Whether [`apply_proposal`] wrote this proposal (its marker file exists).
pub fn is_applied(alias: &str, id: &str, root: Option<&Path>) -> bool {
    applied_marker(alias, id, root).is_ok_and(|m| m.exists())
}

/// `<guide dir>/.applied/<id>`, the id reduced to a file name.
fn applied_marker(alias: &str, id: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    check_alias(alias)?;
    let safe = Path::new(id)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("invalid approval id {id:?}"))?;
    Ok(project_guide::guide_dir(alias, root)
        .join(".applied")
        .join(safe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::{self, NewApproval};
    use crate::guide_build::{self, BuildReport};
    use crate::guide_plan::{self, FeaturePlan, Plan};
    use crate::project_guide::{self, Kind, Page, Part};
    use crate::project_profile::{self, Origin, Profile, Section, SectionKey, Status};
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use std::path::Path;

    /// Split so the literal never looks like a real token to a scanner.
    fn fake_token() -> String {
        format!("{}{}", "glpat-", "AAAAAAAAAAAAAAAAAAAA")
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
    }

    /// Each test uses its own alias: the guide slot is process-wide.
    struct Fx {
        root: tempfile::TempDir,
        apr: tempfile::TempDir,
        alias: String,
    }

    impl Fx {
        fn new(alias: &str) -> Fx {
            Fx {
                root: tempfile::tempdir().unwrap(),
                apr: tempfile::tempdir().unwrap(),
                alias: alias.into(),
            }
        }
        fn root(&self) -> Option<&Path> {
            Some(self.root.path())
        }
        fn apr(&self) -> &Path {
            self.apr.path()
        }
        fn page(&self, slug: &str, status: Status) -> Page {
            Page {
                alias: self.alias.clone(),
                slug: slug.into(),
                kind: Kind::Feature,
                title: "Orders".into(),
                status,
                generated_at: Some(now()),
                head_sha: Some("abc123".into()),
                summary: "Taking orders.".into(),
                related: vec![],
                parts: vec![
                    Part {
                        key: "what".into(),
                        body: "Lets a customer place an order.".into(),
                        origin: Origin::Agent,
                    },
                    Part {
                        key: "key-points".into(),
                        body: "- Orders are immutable once paid.".into(),
                        origin: Origin::Agent,
                    },
                ],
                sources: vec![],
            }
        }
        /// An `Approved` feature page, saved.
        fn approved_page(&self, slug: &str) -> Page {
            let p = self.page(slug, Status::Approved);
            project_guide::save(&p, self.root()).unwrap();
            self.stored(slug)
        }
        fn stored(&self, slug: &str) -> Page {
            project_guide::load(&self.alias, slug, self.root())
                .unwrap()
                .unwrap()
        }
        fn history(&self, slug: &str) -> usize {
            project_guide::history(&self.alias, slug, self.root()).len()
        }
        fn raise(&self, held: &Page) -> Result<Option<String>, String> {
            raise(&self.alias, held, self.apr(), self.root(), now())
        }
        fn approve(&self, id: &str) -> Result<Value, String> {
            approve_and_apply(id, "tester", self.root(), self.apr(), now())
        }
        fn status(&self, id: &str) -> String {
            approvals::get(self.apr(), id).unwrap()["status"]
                .as_str()
                .unwrap()
                .to_string()
        }
        fn marker_dir(&self) -> std::path::PathBuf {
            project_guide::guide_dir(&self.alias, self.root()).join(".applied")
        }
    }

    /// The stored page as a fresh `Draft` with the `what` body replaced.
    fn held_of(p: &Page, new_what: &str) -> Page {
        let mut h = p.clone();
        h.status = Status::Draft;
        for part in &mut h.parts {
            if part.key == "what" {
                part.body = new_what.into();
            }
        }
        h
    }

    fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }

    fn is_hex64(v: &Value) -> bool {
        v.as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    #[test]
    fn raising_makes_a_pending_approval_with_the_texts_and_no_expiry() {
        let fx = Fx::new("gp-raise");
        let stored = fx.approved_page("feature-orders");
        let held = held_of(&stored, "Lets a customer place and cancel an order.");
        let id = fx.raise(&held).unwrap().expect("a proposal");
        let a = approvals::get(fx.apr(), &id).unwrap();
        assert_eq!(a["kind"], KIND);
        assert_eq!(a["status"], "pending");
        assert_eq!(a["project"], "gp-raise");
        assert_eq!(a["issue_iid"], Value::Null);
        assert_eq!(
            a["title"],
            "Update the feature-orders guide page of gp-raise"
        );
        assert_eq!(a["summary"], "Taking orders.");
        let t = &a["target"];
        assert_eq!(t["alias"], "gp-raise");
        assert_eq!(t["slug"], "feature-orders");
        assert_eq!(t["old"], project_guide::render(&stored));
        assert_eq!(t["new"], project_guide::render(&held));
        assert!(is_hex64(&t["old_hash"]), "{}", t["old_hash"]);
        assert!(is_hex64(&t["new_hash"]), "{}", t["new_hash"]);
        assert_ne!(t["old_hash"], t["new_hash"]);
        let expires = DateTime::parse_from_rfc3339(a["expires_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            expires > now() + chrono::Duration::days(365 * 9),
            "{expires}"
        );
        // Raising writes nothing to the page.
        assert_eq!(fx.stored("feature-orders"), stored);
        assert_eq!(fx.history("feature-orders"), 0);
    }

    #[test]
    fn an_identical_pending_dismissed_or_noop_proposal_is_not_raised_again() {
        let fx = Fx::new("gp-dedupe");
        let stored = fx.approved_page("feature-orders");
        let held = held_of(&stored, "A new what.");
        let id = fx.raise(&held).unwrap().expect("first proposal");
        assert_eq!(fx.raise(&held).unwrap(), None, "pending duplicate");
        approvals::decide(fx.apr(), &id, "deny", "tester", now()).unwrap();
        assert_eq!(fx.raise(&held).unwrap(), None, "dismissed duplicate");
        // A rebuild whose parts equal the stored page's is not a proposal.
        let noop = held_of(&stored, "Lets a customer place an order.");
        assert_eq!(fx.raise(&noop).unwrap(), None, "no-op rebuild");
        let other = held_of(&stored, "A different what.");
        let id2 = fx.raise(&other).unwrap().expect("different text");
        assert_ne!(id2, id);
        assert_eq!(approvals::all(fx.apr()).len(), 2);
    }

    #[test]
    fn raise_refuses_a_missing_draft_or_foreign_page() {
        let fx = Fx::new("gp-refuse");
        // Stored page missing.
        let missing = held_of(&fx.page("feature-orders", Status::Draft), "x");
        assert!(fx.raise(&missing).is_err());
        // Stored page is a draft (a build writes drafts directly).
        let draft = fx.page("feature-cart", Status::Draft);
        project_guide::save(&draft, fx.root()).unwrap();
        assert!(fx.raise(&held_of(&draft, "y")).is_err());
        // A held page for another project.
        let stored = fx.approved_page("feature-orders");
        let mut foreign = held_of(&stored, "z");
        foreign.alias = "someone-else".into();
        assert!(fx.raise(&foreign).is_err());
        // A held page with a secret in a part.
        let secret = held_of(&stored, &format!("token {}", fake_token()));
        let err = fx.raise(&secret).unwrap_err();
        assert!(!err.contains("glpat-"), "{err}");
        assert!(approvals::all(fx.apr()).is_empty());
    }

    #[test]
    fn approving_applies_the_new_text_snapshots_the_old_page_and_marks_it_applied() {
        let fx = Fx::new("gp-apply");
        let stored = fx.approved_page("feature-orders");
        let mut held = held_of(&stored, "Places, edits and cancels orders.");
        held.head_sha = Some("def456".into());
        let id = fx.raise(&held).unwrap().unwrap();
        let new_text = approvals::get(fx.apr(), &id).unwrap()["target"]["new"]
            .as_str()
            .unwrap()
            .to_string();
        let before = fx.history("feature-orders");
        let a = fx.approve(&id).unwrap();
        assert_eq!(a["status"], "approved");
        assert_eq!(a["decided_by"], "tester");
        let after = fx.stored("feature-orders");
        assert_eq!(after.status, Status::Approved);
        assert_eq!(after.head_sha.as_deref(), Some("def456"));
        let mut expected = project_guide::parse(&new_text).unwrap();
        expected.status = Status::Approved;
        assert_eq!(after, expected);
        assert_eq!(
            project_guide::render(&after),
            new_text.replace("status: draft", "status: approved")
        );
        assert_eq!(fx.history("feature-orders"), before + 1);
        assert!(is_applied(&fx.alias, &id, fx.root()));
        assert_eq!(fx.status(&id), "approved");
    }

    #[test]
    fn a_human_part_survives_the_application() {
        let fx = Fx::new("gp-human");
        let mut p = fx.page("feature-orders", Status::Approved);
        p.parts[1] = Part {
            key: "key-points".into(),
            body: "- Written by a person: never refund twice.".into(),
            origin: Origin::Human,
        };
        project_guide::save(&p, fx.root()).unwrap();
        let stored = fx.stored("feature-orders");
        let mut held = held_of(&stored, "The new what.");
        held.parts[1] = Part {
            key: "key-points".into(),
            body: "- Agent rewrite of the key points.".into(),
            origin: Origin::Agent,
        };
        let id = fx.raise(&held).unwrap().unwrap();
        fx.approve(&id).unwrap();
        let after = fx.stored("feature-orders");
        let kp = after.parts.iter().find(|x| x.key == "key-points").unwrap();
        assert_eq!(kp.body, "- Written by a person: never refund twice.");
        assert_eq!(kp.origin, Origin::Human);
        let what = after.parts.iter().find(|x| x.key == "what").unwrap();
        assert_eq!(what.body, "The new what.");
        assert_eq!(what.origin, Origin::Agent);
    }

    #[test]
    fn a_stale_proposal_is_refused_and_stays_pending() {
        let fx = Fx::new("gp-stale");
        let stored = fx.approved_page("feature-orders");
        let id = fx
            .raise(&held_of(&stored, "Proposed what."))
            .unwrap()
            .unwrap();
        let edited = project_guide::update_locked(&fx.alias, "feature-orders", fx.root(), |p| {
            let mut p = p.unwrap();
            p.parts[0].body = "Edited by hand meanwhile.".into();
            p.parts[0].origin = Origin::Human;
            Ok(p)
        })
        .unwrap();
        let edited = project_guide::load(&fx.alias, &edited.slug, fx.root())
            .unwrap()
            .unwrap();
        let err = fx.approve(&id).unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        assert!(!err.contains("Proposed what"), "{err}");
        assert_eq!(fx.status(&id), "pending");
        assert_eq!(fx.stored("feature-orders"), edited);
        assert!(!is_applied(&fx.alias, &id, fx.root()));
        assert!(!fx.marker_dir().exists());
    }

    #[test]
    fn approving_twice_applies_once() {
        let fx = Fx::new("gp-twice");
        let stored = fx.approved_page("feature-orders");
        let id = fx.raise(&held_of(&stored, "Once.")).unwrap().unwrap();
        fx.approve(&id).unwrap();
        let hist = fx.history("feature-orders");
        let page = fx.stored("feature-orders");
        let a = fx.approve(&id).unwrap();
        assert_eq!(a["status"], "approved");
        assert_eq!(fx.history("feature-orders"), hist);
        assert_eq!(fx.stored("feature-orders"), page);
        assert_eq!(
            apply_proposal(&id, fx.root(), fx.apr()).unwrap(),
            ApplyOutcome::AlreadyApplied
        );
        assert_eq!(fx.history("feature-orders"), hist);
    }

    #[test]
    fn a_busy_guide_slot_refuses_without_deciding() {
        let fx = Fx::new("gp-busy");
        let stored = fx.approved_page("feature-orders");
        let id = fx.raise(&held_of(&stored, "Busy.")).unwrap().unwrap();
        let slot = guide_build::try_acquire(&fx.alias).expect("slot free");
        let err = fx.approve(&id).unwrap_err();
        assert!(err.contains("in progress"), "{err}");
        assert_eq!(fx.status(&id), "pending");
        assert_eq!(fx.stored("feature-orders"), stored);
        drop(slot);
        fx.approve(&id).unwrap();
        assert_eq!(fx.status(&id), "approved");
    }

    fn hand_made(fx: &Fx, target: Value) -> String {
        let a = approvals::create(
            fx.apr(),
            &NewApproval {
                kind: KIND.into(),
                project: fx.alias.clone(),
                title: "t".into(),
                summary: "s".into(),
                timeout_seconds: 3600,
                target: Some(target),
                ..NewApproval::default()
            },
            now(),
        )
        .unwrap();
        a["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn a_tampered_target_is_refused_without_touching_disk() {
        let fx = Fx::new("gp-tamper");
        let stored = fx.approved_page("feature-orders");
        let old = project_guide::render(&stored);
        let old_hash = crate::sha256::hex(&crate::sha256::sha256(old.as_bytes()));
        let before = files_under(fx.root.path());

        // A slug that walks out of the guide directory.
        let id = hand_made(
            &fx,
            json!({"alias": fx.alias, "slug": "../x", "old_hash": old_hash,
                   "new_hash": "", "old": old, "new": old}),
        );
        assert!(fx.approve(&id).is_err());
        assert_eq!(fx.status(&id), "pending");
        approvals::decide(fx.apr(), &id, "approve", "tester", now()).unwrap();
        assert!(apply_proposal(&id, fx.root(), fx.apr()).is_err());

        // A real slug whose new text carries a secret.
        let mut bad = held_of(&stored, &format!("token {}", fake_token()));
        bad.status = Status::Draft;
        let new = project_guide::render(&bad);
        let id2 = hand_made(
            &fx,
            json!({"alias": fx.alias, "slug": "feature-orders", "old_hash": old_hash,
                   "new_hash": "", "old": old, "new": new}),
        );
        let err = fx.approve(&id2).unwrap_err();
        assert!(!err.contains("glpat-"), "{err}");
        assert_eq!(fx.status(&id2), "pending");
        approvals::decide(fx.apr(), &id2, "approve", "tester", now()).unwrap();
        let err = apply_proposal(&id2, fx.root(), fx.apr()).unwrap_err();
        assert!(!err.contains("glpat-"), "{err}");

        assert_eq!(fx.stored("feature-orders"), stored);
        assert!(!fx.marker_dir().exists());
        assert_eq!(files_under(fx.root.path()), before);
        assert!(!fx.root.path().join("x").exists());
    }

    #[test]
    fn raise_all_raises_for_held_pages_and_skips_the_ones_that_error() {
        let fx = Fx::new("gp-all");
        let stored = fx.approved_page("feature-orders");
        let good = held_of(&stored, "A fresh what.");
        let orphan = held_of(&fx.page("feature-ghost", Status::Draft), "Ghost.");
        let report = BuildReport {
            held: vec![good, orphan],
            ..BuildReport::default()
        };
        let ids = raise_all(&fx.alias, &report, fx.apr(), fx.root(), now());
        assert_eq!(ids.len(), 1);
        let a = approvals::get(fx.apr(), &ids[0]).unwrap();
        assert_eq!(a["target"]["slug"], "feature-orders");
        assert_eq!(approvals::all(fx.apr()).len(), 1);
    }

    #[test]
    fn applying_a_proposal_resyncs_the_profile_features_index() {
        let fx = Fx::new("gp-features");
        let plan = Plan {
            features: vec![FeaturePlan {
                slug: "feature-orders".into(),
                title: "Orders".into(),
                summary: "Taking orders.".into(),
                files: vec!["app/orders.rb".into()],
                related: vec![],
            }],
            ..Plan::default()
        };
        guide_plan::save(&fx.alias, &plan, fx.root()).unwrap();
        let mut sections = BTreeMap::new();
        for k in SectionKey::ALL {
            sections.insert(
                k,
                Section {
                    summary: format!("{} summary", k.heading()),
                    body: format!("{} body.", k.heading()),
                    last_verified: Some(now()),
                    sources: vec![],
                    origin: Origin::Agent,
                },
            );
        }
        let profile = Profile {
            alias: fx.alias.clone(),
            generated_at: Some(now()),
            head_sha: Some("abc".into()),
            status: Status::Approved,
            links: vec![],
            sections,
        };
        project_profile::save(&profile, fx.root()).unwrap();
        let features = |fx: &Fx| {
            project_profile::load(&fx.alias, fx.root())
                .unwrap()
                .unwrap()
                .sections[&SectionKey::Features]
                .body
                .clone()
        };
        assert!(!features(&fx).contains("feature-orders"));

        let stored = fx.approved_page("feature-orders");
        let id = fx.raise(&held_of(&stored, "Resynced.")).unwrap().unwrap();
        fx.approve(&id).unwrap();
        assert!(is_applied(&fx.alias, &id, fx.root()));
        let body = features(&fx);
        assert!(body.contains("(guide page `feature-orders`)"), "{body}");
        assert!(!body.contains(", draft"), "{body}");
    }
}

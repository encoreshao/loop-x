//! The guide plan: which feature pages to write and which files each page reads. The
//! model proposes it as JSON; this module is where every rule about it is enforced.
//! Errors name an item's position, never a value from the reply.

use crate::guide_inventory::Inventory;
use crate::project_guide::{check_slug, plan_path, Kind};
use crate::project_profile::{check_alias, contains_secret};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MAX_FEATURES: usize = 12;
pub const MAX_FILES_PER_PAGE: usize = 25;
const MAX_TITLE: usize = 80;
const MAX_SUMMARY: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeaturePlan {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagePlan {
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub features: Vec<FeaturePlan>,
    #[serde(default)]
    pub architecture: PagePlan,
    #[serde(default)]
    pub data_model: PagePlan,
}

/// The JSON object in `reply`, tolerating a code fence or a line of chatter around it.
fn json_slice(reply: &str) -> Option<&str> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    (end > start).then(|| &reply[start..=end])
}

fn one_line_ok(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.contains(['\n', '\r'])
}

/// `s` on one line, whitespace collapsed, cut to `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('\u{2026}');
    cut
}

/// Keep the first [`MAX_FILES_PER_PAGE`] distinct paths that are readable files of the project.
fn keep_readable(files: &mut Vec<String>, inv: &Inventory) {
    let mut seen: Vec<String> = Vec::new();
    files.retain(|f| {
        let keep = seen.len() < MAX_FILES_PER_PAGE && !seen.contains(f) && inv.readable(f);
        if keep {
            seen.push(f.clone());
        }
        keep
    });
}

/// Shape rules that need no inventory: used by `parse_plan` and by `load`.
fn check_shape(plan: &Plan) -> Result<(), String> {
    if plan.features.len() > MAX_FEATURES {
        return Err(format!("the plan has more than {MAX_FEATURES} features"));
    }
    let mut slugs: Vec<&str> = Vec::new();
    for (i, f) in plan.features.iter().enumerate() {
        let n = i + 1;
        if check_slug(&f.slug).ok() != Some(Kind::Feature) {
            return Err(format!("feature #{n} has an invalid slug"));
        }
        if slugs.contains(&f.slug.as_str()) {
            return Err(format!(
                "feature #{n} repeats a slug already used earlier (it would appear twice)"
            ));
        }
        slugs.push(&f.slug);
        if !one_line_ok(&f.title, MAX_TITLE) || !one_line_ok(&f.summary, MAX_SUMMARY) {
            return Err(format!("feature #{n} has a bad title or summary"));
        }
        if contains_secret(&f.title).is_some() || contains_secret(&f.summary).is_some() {
            return Err(format!(
                "feature #{n} title or summary looks like it holds a secret"
            ));
        }
    }
    for (i, f) in plan.features.iter().enumerate() {
        for r in &f.related {
            if r == &f.slug || !slugs.contains(&r.as_str()) {
                return Err(format!(
                    "feature #{} has a related slug that is not in the plan",
                    i + 1
                ));
            }
        }
    }
    Ok(())
}

/// The model's plan, repaired rather than refused: a feature with a bad or repeated
/// slug, an empty or secret-holding title or summary, or no readable file is dropped;
/// lists past their caps, unknown files and unknown related slugs are cut. `Err` only
/// when there is no JSON plan or no feature survives.
pub fn parse_plan(reply: &str, inv: &Inventory) -> Result<Plan, String> {
    let json = json_slice(reply).ok_or("the reply holds no plan")?;
    let raw: Plan = serde_json::from_str(json).map_err(|_| "the plan did not parse".to_string())?;
    let mut features: Vec<FeaturePlan> = Vec::new();
    for mut f in raw.features {
        if features.len() == MAX_FEATURES {
            break;
        }
        if check_slug(&f.slug).ok() != Some(Kind::Feature)
            || features.iter().any(|k| k.slug == f.slug)
        {
            continue;
        }
        f.title = one_line(&f.title, MAX_TITLE);
        f.summary = one_line(&f.summary, MAX_SUMMARY);
        if f.title.is_empty()
            || f.summary.is_empty()
            || contains_secret(&f.title).is_some()
            || contains_secret(&f.summary).is_some()
        {
            continue;
        }
        keep_readable(&mut f.files, inv);
        if !f.files.is_empty() {
            features.push(f);
        }
    }
    if features.is_empty() {
        return Err("the plan names no features".to_string());
    }
    let slugs: Vec<String> = features.iter().map(|f| f.slug.clone()).collect();
    for f in &mut features {
        let own = f.slug.clone();
        let mut seen: Vec<String> = Vec::new();
        f.related.retain(|r| {
            let keep = r != &own && slugs.contains(r) && !seen.contains(r);
            if keep {
                seen.push(r.clone());
            }
            keep
        });
    }
    let mut plan = Plan { features, ..raw };
    keep_readable(&mut plan.architecture.files, inv);
    keep_readable(&mut plan.data_model.files, inv);
    check_shape(&plan)?;
    Ok(plan)
}

pub fn save(alias: &str, plan: &Plan, root: Option<&Path>) -> Result<(), String> {
    check_alias(alias)?;
    check_shape(plan)?;
    let text = serde_json::to_string_pretty(plan).map_err(|e| e.to_string())?;
    crate::file_lock::write_atomic(&plan_path(alias, root), text.as_bytes())
        .map_err(|e| e.to_string())
}

pub fn load(alias: &str, root: Option<&Path>) -> Result<Option<Plan>, String> {
    check_alias(alias)?;
    let path = plan_path(alias, root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let plan: Plan = serde_json::from_str(&text)
        .map_err(|_| format!("{}: the plan did not parse", path.display()))?;
    check_shape(&plan).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(plan))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide_inventory::{Entry, Inventory};

    fn inv(paths: &[&str]) -> Inventory {
        Inventory {
            entries: paths
                .iter()
                .map(|p| Entry {
                    path: p.to_string(),
                    size: 10,
                    readable: true,
                })
                .collect(),
            truncated: false,
        }
    }

    fn reply(features: &str) -> String {
        format!(
            r#"{{"features":[{features}],"architecture":{{"files":["a.rb"]}},"data_model":{{"files":["db/schema.rb"]}}}}"#
        )
    }

    fn feat(slug: &str, files: &str, related: &str) -> String {
        format!(
            r#"{{"slug":"{slug}","title":"T {slug}","summary":"S.","files":[{files}],"related":[{related}]}}"#
        )
    }

    fn base() -> Inventory {
        inv(&["a.rb", "b.rb", "db/schema.rb", ".hidden/x", "c.rb"])
    }

    #[test]
    fn a_good_plan_parses_even_inside_a_code_fence() {
        let r = format!(
            "```json\n{}\n```",
            reply(&feat("feature-a", r#""a.rb","b.rb""#, ""))
        );
        let p = parse_plan(&r, &base()).unwrap();
        assert_eq!(p.features[0].slug, "feature-a");
        assert_eq!(p.architecture.files, ["a.rb"]);
        assert_eq!(p.data_model.files, ["db/schema.rb"]);
    }

    #[test]
    fn a_related_slug_not_in_the_plan_is_dropped() {
        let two = format!(
            "{},{}",
            feat(
                "feature-a",
                r#""a.rb""#,
                r#""feature-b","feature-zzz","feature-a""#
            ),
            feat("feature-b", r#""b.rb""#, "")
        );
        let p = parse_plan(&reply(&two), &base()).unwrap();
        assert_eq!(p.features[0].related, ["feature-b"]);
    }

    #[test]
    fn a_feature_with_a_bad_slug_or_a_secret_is_dropped_without_echoing_it() {
        let secret = "glpat-AAAAAAAAAAAAAAAAAAAA";
        let good = feat("feature-ok", r#""a.rb""#, "");
        for bad in [
            feat("Feature-A", r#""a.rb""#, ""),
            feat("feature-a", r#""a.rb""#, "").replace("\"S.\"", &format!("\"token {secret}\"")),
        ] {
            let p = parse_plan(&reply(&format!("{bad},{good}")), &base()).unwrap();
            let slugs: Vec<&str> = p.features.iter().map(|f| f.slug.as_str()).collect();
            assert_eq!(slugs, ["feature-ok"]);
            let err = parse_plan(&reply(&bad), &base()).unwrap_err();
            assert!(!err.contains(secret) && !err.contains("Feature-A"), "{err}");
        }
    }

    #[test]
    fn unknown_unreadable_and_env_paths_are_dropped_and_a_feature_left_with_none_is_dropped() {
        let mut i = base();
        i.entries.push(Entry {
            path: "big.rb".into(),
            size: 999_999,
            readable: false,
        });
        let r = reply(&format!(
            "{},{}",
            feat(
                "feature-a",
                r#""a.rb","big.rb",".env","../etc/passwd","not-in-inventory.rb""#,
                ""
            ),
            feat("feature-b", r#""big.rb",".env""#, "")
        ));
        let p = parse_plan(&r, &i).unwrap();
        assert_eq!(p.features.len(), 1);
        assert_eq!(p.features[0].files, ["a.rb"]);
        let only_bad = reply(&feat("feature-a", r#"".env""#, ""));
        assert!(parse_plan(&only_bad, &base()).is_err());
    }

    #[test]
    fn caps_keep_the_first_features_and_files() {
        let many: Vec<String> = (0..13)
            .map(|i| feat(&format!("feature-f{i}"), r#""a.rb""#, r#""feature-f12""#))
            .collect();
        let p = parse_plan(&reply(&many.join(",")), &base()).unwrap();
        assert_eq!(p.features.len(), MAX_FEATURES);
        assert_eq!(p.features[11].slug, "feature-f11");
        assert!(p.features.iter().all(|f| f.related.is_empty()));
        let files: Vec<String> = (0..26).map(|i| format!("\"f{i}.rb\"")).collect();
        let paths: Vec<String> = (0..26).map(|i| format!("f{i}.rb")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let r = reply(&feat("feature-a", &files.join(","), ""));
        let p = parse_plan(&r, &inv(&refs)).unwrap();
        assert_eq!(p.features[0].files.len(), MAX_FILES_PER_PAGE);
        assert_eq!(p.features[0].files[24], "f24.rb");
    }

    #[test]
    fn a_long_or_multiline_title_or_summary_is_cut_to_one_line() {
        let long = "word ".repeat(100);
        let r = reply(&feat("feature-a", r#""a.rb""#, ""))
            .replace("\"T feature-a\"", &format!("\"{long}\""))
            .replace("\"S.\"", "\"Line one.\\nLine two.\"");
        let p = parse_plan(&r, &base()).unwrap();
        assert!(p.features[0].title.chars().count() <= MAX_TITLE);
        assert_eq!(p.features[0].summary, "Line one. Line two.");
    }

    #[test]
    fn a_duplicate_slug_keeps_the_first_and_no_json_is_refused() {
        let dup = format!(
            "{},{}",
            feat("feature-a", r#""a.rb""#, ""),
            feat("feature-a", r#""b.rb""#, "")
        );
        let p = parse_plan(&reply(&dup), &base()).unwrap();
        assert_eq!(p.features.len(), 1);
        assert_eq!(p.features[0].files, ["a.rb"]);
        assert!(parse_plan("sorry, I cannot", &base()).is_err());
        assert!(parse_plan("", &base()).is_err());
    }

    #[test]
    fn duplicate_files_are_collapsed() {
        let p = parse_plan(&reply(&feat("feature-a", r#""a.rb","a.rb""#, "")), &base()).unwrap();
        assert_eq!(p.features[0].files, ["a.rb"]);
    }

    #[test]
    fn a_plan_with_no_features_is_refused() {
        let e = parse_plan(&reply(""), &base()).unwrap_err();
        assert_eq!(e, "the plan names no features");
    }

    #[test]
    fn save_and_load_round_trip_and_a_missing_plan_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = Some(dir.path());
        assert_eq!(load("shop", root).unwrap(), None);
        let p = parse_plan(&reply(&feat("feature-a", r#""a.rb""#, "")), &base()).unwrap();
        save("shop", &p, root).unwrap();
        assert_eq!(load("shop", root).unwrap(), Some(p));
        assert!(crate::project_guide::plan_path("shop", root).is_file());
    }

    #[test]
    fn load_refuses_a_hand_edited_plan_with_a_bad_slug_and_save_refuses_a_bad_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = Some(dir.path());
        let gd = crate::project_guide::guide_dir("shop", root);
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(gd.join("plan.json"), reply(&feat("../x", r#""a.rb""#, ""))).unwrap();
        assert!(load("shop", root).is_err());
        assert!(save("../shop", &Plan::default(), root).is_err());
    }
}

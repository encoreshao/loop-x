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
    pub summary: String,
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

fn check_files(label: &str, files: &mut Vec<String>, inv: &Inventory) -> Result<(), String> {
    let mut seen = Vec::new();
    files.retain(|f| {
        let keep = !seen.contains(f);
        seen.push(f.clone());
        keep
    });
    if files.len() > MAX_FILES_PER_PAGE {
        return Err(format!(
            "{label} names more than {MAX_FILES_PER_PAGE} files"
        ));
    }
    for (i, f) in files.iter().enumerate() {
        if !inv.readable(f) {
            return Err(format!(
                "{label} file #{} is not a readable file of the project",
                i + 1
            ));
        }
    }
    Ok(())
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

pub fn parse_plan(reply: &str, inv: &Inventory) -> Result<Plan, String> {
    let json = json_slice(reply).ok_or("the reply holds no plan")?;
    let mut plan: Plan =
        serde_json::from_str(json).map_err(|_| "the plan did not parse".to_string())?;
    check_shape(&plan)?;
    if plan.features.is_empty() {
        return Err("the plan names no features".to_string());
    }
    for (i, f) in plan.features.iter_mut().enumerate() {
        if f.files.is_empty() {
            return Err(format!("feature #{} names no files", i + 1));
        }
        check_files(&format!("feature #{}", i + 1), &mut f.files, inv)?;
    }
    check_files("architecture", &mut plan.architecture.files, inv)?;
    check_files("data model", &mut plan.data_model.files, inv)?;
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
    fn related_may_name_only_slugs_in_the_same_plan() {
        let two = format!(
            "{},{}",
            feat("feature-a", r#""a.rb""#, r#""feature-b""#),
            feat("feature-b", r#""b.rb""#, "")
        );
        assert!(parse_plan(&reply(&two), &base()).is_ok());
        let bad = feat("feature-a", r#""a.rb""#, r#""feature-zzz""#);
        assert!(parse_plan(&reply(&bad), &base())
            .unwrap_err()
            .contains("related"));
    }

    #[test]
    fn rejections_never_echo_the_offending_value() {
        let secret = "glpat-AAAAAAAAAAAAAAAAAAAA";
        for bad in [
            feat("Feature-A", r#""a.rb""#, ""),
            feat("feature-a", &format!(r#""{secret}""#), ""),
            feat("feature-a", r#""../etc/passwd""#, ""),
            feat("feature-a", r#""not-in-inventory.rb""#, ""),
            feat("feature-a", r#""a.rb""#, &format!(r#""{secret}""#)),
        ] {
            let err = parse_plan(&reply(&bad), &base()).unwrap_err();
            assert!(
                !err.contains(secret) && !err.contains("passwd") && !err.contains("Feature-A"),
                "{err}"
            );
        }
    }

    #[test]
    fn an_unreadable_or_env_path_is_refused() {
        let mut i = base();
        i.entries.push(Entry {
            path: "big.rb".into(),
            size: 999_999,
            readable: false,
        });
        let r = reply(&feat("feature-a", r#""big.rb""#, ""));
        assert!(parse_plan(&r, &i).is_err());
        let r = reply(&feat("feature-a", r#"".env""#, ""));
        assert!(parse_plan(&r, &base()).is_err());
    }

    #[test]
    fn caps_are_enforced() {
        let many: Vec<String> = (0..13)
            .map(|i| feat(&format!("feature-f{i}"), r#""a.rb""#, ""))
            .collect();
        assert!(parse_plan(&reply(&many.join(",")), &base())
            .unwrap_err()
            .contains("12"));
        let files: Vec<String> = (0..26).map(|i| format!("\"f{i}.rb\"")).collect();
        let paths: Vec<String> = (0..26).map(|i| format!("f{i}.rb")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let i = inv(&refs);
        let r = reply(&feat("feature-a", &files.join(","), ""));
        assert!(parse_plan(&r, &i).unwrap_err().contains("25"));
    }

    #[test]
    fn duplicate_slugs_a_feature_with_no_files_and_no_json_are_refused() {
        let dup = format!("{0},{0}", feat("feature-a", r#""a.rb""#, ""));
        assert!(parse_plan(&reply(&dup), &base())
            .unwrap_err()
            .contains("twice"));
        assert!(parse_plan(&reply(&feat("feature-a", "", "")), &base()).is_err());
        assert!(parse_plan("sorry, I cannot", &base()).is_err());
        assert!(parse_plan("", &base()).is_err());
    }

    #[test]
    fn a_title_or_summary_with_a_secret_is_refused() {
        let r = reply(&feat("feature-a", r#""a.rb""#, ""))
            .replace("\"S.\"", "\"token glpat-AAAAAAAAAAAAAAAAAAAA\"");
        assert!(parse_plan(&r, &base()).is_err());
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

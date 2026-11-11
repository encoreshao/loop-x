//! `loopx memory-store profile|graph`: real subprocesses against a scratch home.

use loopx_core::project_profile::{self, Link, Origin, Profile, Section, SectionKey, Status};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

fn loopx(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopx"))
        .args(args)
        .env("LOOP_ENGINEERING_HOME", home.join("loopx"))
        .env("HOME", home.join("home"))
        .env("LOOPX_ROOT", home.join("root"))
        .env("LOOP_UPDATE_DIR", home.join("update"))
        .env("LOOP_EVENTS_DIR", home.join("events"))
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn profile(alias: &str, links: Vec<Link>, extra: Option<&str>) -> Profile {
    let section = |body: &str| Section {
        summary: String::new(),
        body: body.into(),
        last_verified: None,
        sources: vec![],
        origin: Origin::Agent,
    };
    let mut sections = BTreeMap::new();
    sections.insert(SectionKey::Overview, section("A synthetic shop."));
    sections.insert(SectionKey::Secrets, section("STRIPE_KEY"));
    if let Some(text) = extra {
        sections.insert(SectionKey::Gotchas, section(text));
    }
    Profile {
        alias: alias.into(),
        generated_at: None,
        head_sha: None,
        status: Status::Approved,
        links,
        sections,
    }
}

fn save(home: &Path, p: &Profile) {
    let root = home.join("loopx").join("memory");
    project_profile::save(p, Some(&root)).unwrap();
}

fn write_projects(home: &Path, aliases: &[&str]) {
    let projects: serde_json::Map<String, serde_json::Value> = aliases
        .iter()
        .map(|a| {
            (
                a.to_string(),
                serde_json::json!({"project_id": format!("g/{a}"), "local_path": format!("/x/{a}")}),
            )
        })
        .collect();
    let dir = home.join("loopx");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("projects.json"),
        serde_json::json!({"gitlab_instance": "loop", "projects": projects}).to_string(),
    )
    .unwrap();
}

#[test]
fn cli_profile_prints_markdown() {
    let d = tempfile::tempdir().unwrap();
    save(d.path(), &profile("web", vec![], None));
    let o = loopx(d.path(), &["memory-store", "profile", "web"]);
    assert!(o.status.success());
    let text = out(&o);
    assert!(text.contains("## Overview") && text.contains("A synthetic shop."));
    assert!(text.contains("STRIPE_KEY"), "names only: {text}");
}

#[test]
fn cli_profile_absent_prints_placeholder() {
    let d = tempfile::tempdir().unwrap();
    let o = loopx(d.path(), &["memory-store", "profile", "web"]);
    assert!(o.status.success());
    assert_eq!(out(&o).trim(), "(no profile yet)");
}

#[test]
fn cli_graph_lists_neighbors() {
    let d = tempfile::tempdir().unwrap();
    write_projects(d.path(), &["web", "api"]);
    let link = Link {
        to: "api".into(),
        kind: "calls".into(),
        evidence: "web/client.rs calls api".into(),
    };
    save(d.path(), &profile("web", vec![link], None));
    save(d.path(), &profile("api", vec![], None));
    let o = loopx(d.path(), &["memory-store", "graph", "web"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["neighbors"], serde_json::json!(["api"]));
    assert_eq!(v["edges"][0]["kind"], "calls");
    // The whole graph, without an alias.
    let o = loopx(d.path(), &["memory-store", "graph"]);
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn cli_output_refuses_a_secret_value() {
    let d = tempfile::tempdir().unwrap();
    let token = format!("glpat-{}", "a1B2c3D4e5F6g7H8i9J0");
    save(
        d.path(),
        &profile("web", vec![], Some(&format!("token {token}"))),
    );
    let o = loopx(d.path(), &["memory-store", "profile", "web"]);
    assert_eq!(o.status.code(), Some(1));
    let all = format!("{}{}", out(&o), String::from_utf8_lossy(&o.stderr));
    assert!(!all.contains(&token), "{all}");
    assert!(all.contains("refused"), "{all}");
}

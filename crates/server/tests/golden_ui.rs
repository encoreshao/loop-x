//! Golden comparison of the Rust pages against the Python dashboard.
//!
//! `tests/golden_ui/capture.py` renders every page in `tests/golden_ui/pages.json`
//! (4 languages) from the Python dashboard in a sandboxed fixture and checks the
//! responses in under `tests/golden_ui/expected/`. This test builds the same
//! fixture (`tests/golden_ui/fixture` plus the page's overlay) in a temp dir,
//! runs each page through the Rust registry in-process, and compares.
//!
//! * A page no Rust route claims is reported as pending, not failed.
//! * Normalization (the only one): `127.0.0.1:<port>` becomes `127.0.0.1:PORT` on
//!   both sides. Everything else is pinned instead: the clock (`clock.txt`), the
//!   CSRF token, file mtimes (clock minus one hour) and the `@PID@` placeholder.
//! * `<style>`/`<script>` bodies over 1 KiB are stored once in `expected/_blobs`
//!   as `<!--@blob:HASH-->`; they are expanded before comparing.
//!
//! Env: `LOOPX_GOLDEN_FILTER=id1,id2` runs only those pages; `LOOPX_GOLDEN_VERBOSE=1`
//! lists every page's status.

use axum::http::Method;
use chrono::{DateTime, Utc};
use loopx_server::ui::badge::status_badge_markup;
use loopx_server::ui::routes::UiCtx;
use loopx_server::ui::shell::{render_shell, ShellOpts};
use loopx_server::ui::{UiRequest, UiState};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const LANGS: [&str; 4] = ["en", "ja", "zh", "fr"];

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden_ui")
}

fn copy_tree(src: &Path, dst: &Path) {
    let Ok(entries) = fs::read_dir(src) else {
        return;
    };
    fs::create_dir_all(dst).unwrap();
    for e in entries.flatten() {
        let (from, to) = (e.path(), dst.join(e.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

fn each_file(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            each_file(&p, f);
        } else {
            f(&p);
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    state: UiState,
}

fn build_fixture(overlay: Option<&str>, now: DateTime<Utc>, token: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let g = golden_dir();
    for name in ["checkout", "home", "userhome", "update", "events", "evals"] {
        let dst = tmp.path().join(name);
        fs::create_dir_all(&dst).unwrap();
        copy_tree(&g.join("fixture").join(name), &dst);
        if let Some(o) = overlay {
            copy_tree(&g.join("overlays").join(o).join(name), &dst);
        }
    }
    let stamp: SystemTime = (now - chrono::Duration::hours(1)).into();
    let pid = std::process::id().to_string();
    for name in ["checkout", "home", "userhome", "update", "events", "evals"] {
        each_file(&tmp.path().join(name), &mut |p| {
            if let Ok(text) = fs::read_to_string(p) {
                if text.contains("@PID@") {
                    fs::write(p, text.replace("@PID@", &pid)).unwrap();
                }
            }
            if name != "userhome" {
                let f = fs::OpenOptions::new().write(true).open(p).unwrap();
                f.set_modified(stamp).unwrap();
            }
        });
    }
    let state = UiState::explicit(
        &tmp.path().join("checkout"),
        &tmp.path().join("home"),
        &tmp.path().join("userhome"),
        &tmp.path().join("update"),
        token,
        Some(now),
    )
    .with_events_dir(&tmp.path().join("events"))
    .with_evals_dir(&tmp.path().join("evals"));
    Fixture { _tmp: tmp, state }
}

fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("127.0.0.1:") {
        out.push_str(&rest[..i + "127.0.0.1:".len()]);
        let after = &rest[i + "127.0.0.1:".len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            out.push_str("PORT");
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    pin_userhome(&out)
}

/// Mirror of capture.py's second rule: `<anything>/userhome` (the sandbox HOME)
/// becomes `/USERHOME`; the path starts after the last delimiter before it.
fn pin_userhome(text: &str) -> String {
    let is_delim = |c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '&');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("/userhome") {
        let start = rest[..i].rfind(is_delim).map_or(0, |d| {
            d + rest[d..].chars().next().map_or(1, char::len_utf8)
        });
        out.push_str(&rest[..start]);
        out.push_str("/USERHOME");
        rest = &rest[i + "/userhome".len()..];
    }
    out.push_str(rest);
    out
}

fn expand_blobs(text: &str, expected: &Path) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("<!--@blob:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + "<!--@blob:".len()..];
        let end = after.find("-->").expect("blob marker");
        let blob = fs::read_to_string(
            expected
                .join("_blobs")
                .join(format!("{}.txt", &after[..end])),
        )
        .expect("blob file");
        out.push_str(&blob);
        rest = &after[end + 3..];
    }
    out.push_str(rest);
    out
}

/// A readable first difference, with a little context.
fn first_difference(expected: &str, actual: &str) -> String {
    let (e, a): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    let n = e.len().max(a.len());
    for i in 0..n {
        let (el, al) = (
            e.get(i).copied().unwrap_or("<end of file>"),
            a.get(i).copied().unwrap_or("<end of file>"),
        );
        if el != al {
            let col = el
                .chars()
                .zip(al.chars())
                .take_while(|(x, y)| x == y)
                .count();
            let clip = |s: &str| -> String {
                let start = col.saturating_sub(60);
                s.chars().skip(start).take(200).collect()
            };
            let mut msg = format!("first difference at line {} column {}\n", i + 1, col + 1);
            for j in i.saturating_sub(2)..i {
                msg.push_str(&format!(
                    "   {:>5} | {}\n",
                    j + 1,
                    clip(e.get(j).copied().unwrap_or(""))
                ));
            }
            msg.push_str(&format!(
                "  -{:>5} | {}\n  +{:>5} | {}\n",
                i + 1,
                clip(el),
                i + 1,
                clip(al)
            ));
            return msg;
        }
    }
    format!(
        "bodies differ only in trailing whitespace/line endings ({} vs {} bytes)",
        expected.len(),
        actual.len()
    )
}

#[derive(Default)]
struct Tally {
    ported: usize,
    pending: usize,
    failed: usize,
}

#[test]
fn rust_pages_match_the_python_dashboard() {
    let g = golden_dir();
    let expected_dir = g.join("expected");
    let manifest: Value = serde_json::from_str(&fs::read_to_string(expected_dir.join("manifest.json")).expect(
        "tests/golden_ui/expected/manifest.json missing: run python3 tests/golden_ui/capture.py",
    ))
    .unwrap();
    let now = DateTime::parse_from_rfc3339(manifest["clock"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    let token = manifest["csrf_token"].as_str().unwrap();
    let filter: Option<Vec<String>> = std::env::var("LOOPX_GOLDEN_FILTER")
        .ok()
        .map(|v| v.split(',').map(str::to_string).collect());
    let verbose = std::env::var("LOOPX_GOLDEN_VERBOSE").is_ok();

    let mut fixtures: BTreeMap<String, Fixture> = BTreeMap::new();
    let mut tally: BTreeMap<&str, Tally> = LANGS.iter().map(|l| (*l, Tally::default())).collect();
    let mut failures = Vec::new();
    let mut pending_ids = std::collections::BTreeSet::new();

    for page in manifest["pages"].as_array().unwrap() {
        let id = page["id"].as_str().unwrap();
        if filter.as_ref().is_some_and(|f| !f.iter().any(|x| x == id)) {
            continue;
        }
        let overlay = page["overlay"].as_str();
        let fixture = fixtures
            .entry(overlay.unwrap_or("").to_string())
            .or_insert_with(|| build_fixture(overlay, now, token));
        let path = page["path"].as_str().unwrap();
        for (lang, rec) in page["results"].as_object().unwrap() {
            let t = tally.get_mut(lang.as_str()).unwrap();
            let path_only = path.split('?').next().unwrap();
            if !fixture
                .state
                .registry
                .serves(&fixture.state, &Method::GET, path_only)
            {
                t.pending += 1;
                pending_ids.insert(id.to_string());
                continue;
            }
            let req = UiRequest::get(path).with_header("cookie", &format!("loop_lang={lang}"));
            let resp = fixture
                .state
                .registry
                .dispatch(&fixture.state, &req)
                .expect("served route");
            let mut problems = Vec::new();
            if i64::from(resp.status) != rec["status"].as_i64().unwrap() {
                problems.push(format!(
                    "status {} != expected {}",
                    resp.status, rec["status"]
                ));
            }
            let want_ct = rec["content_type"].as_str().unwrap_or("");
            if resp.content_type().unwrap_or("") != want_ct {
                problems.push(format!(
                    "content-type {:?} != expected {want_ct:?}",
                    resp.content_type()
                ));
            }
            if resp.header("Location") != rec["location"].as_str() {
                problems.push(format!(
                    "location {:?} != expected {}",
                    resp.header("Location"),
                    rec["location"]
                ));
            }
            if let Some(file) = rec["file"].as_str() {
                let want_bytes = fs::read(expected_dir.join(file)).unwrap();
                if file.ends_with(".bin") {
                    if resp.body != want_bytes {
                        problems.push("binary body differs".to_string());
                    }
                } else {
                    let want = expand_blobs(&String::from_utf8(want_bytes).unwrap(), &expected_dir);
                    let got = normalize(&resp.body_text());
                    if want != got {
                        problems.push(first_difference(&want, &got));
                    }
                }
            } else if !resp.body.is_empty() {
                problems.push("expected an empty body".to_string());
            }
            if problems.is_empty() {
                t.ported += 1;
                if verbose {
                    println!("ok       {lang} {id}");
                }
            } else {
                t.failed += 1;
                failures.push(format!(
                    "FAIL {lang} {id} ({path})\n  {}",
                    problems.join("\n  ")
                ));
            }
        }
    }

    println!("\ngolden UI results (ported = byte-identical to Python):");
    for (lang, t) in &tally {
        println!(
            "  {lang}: {} ported, {} pending, {} failed",
            t.ported, t.pending, t.failed
        );
    }
    println!(
        "pending pages: {}",
        pending_ids.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n\n"));
}

#[test]
fn render_shell_matches_python_for_every_variant_and_language() {
    let g = golden_dir();
    let expected_dir = g.join("expected");
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(expected_dir.join("manifest.json")).unwrap())
            .unwrap();
    let now = DateTime::parse_from_rfc3339(manifest["clock"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    let fixture = build_fixture(None, now, manifest["csrf_token"].as_str().unwrap());
    let variants: Value =
        serde_json::from_str(&fs::read_to_string(g.join("shell_variants.json")).unwrap()).unwrap();
    let mut failures = Vec::new();
    let mut count = 0;
    for v in variants["variants"].as_array().unwrap() {
        let id = v["id"].as_str().unwrap();
        for lang in LANGS {
            let req = UiRequest::get("/").with_header("cookie", &format!("loop_lang={lang}"));
            let ctx = UiCtx::new(&fixture.state, &req);
            let badge = status_badge_markup(ctx.lang, &v["status"]);
            let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
            let got = normalize(&render_shell(
                &ctx,
                v["title"].as_str().unwrap(),
                v["active_page"].as_str().unwrap(),
                &badge,
                v["body"].as_str().unwrap(),
                ShellOpts {
                    refresh: flag("refresh"),
                    refresh_note: flag("refresh_note"),
                    lazy_refresh: flag("lazy_refresh"),
                },
            ));
            let want = expand_blobs(
                &fs::read_to_string(expected_dir.join("shell").join(format!("{id}.{lang}.html")))
                    .unwrap(),
                &expected_dir,
            );
            count += 1;
            if want != got {
                failures.push(format!(
                    "FAIL shell {id} {lang}\n  {}",
                    first_difference(&want, &got)
                ));
            }
        }
    }
    println!("shell variants compared: {count}");
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n\n"));
}

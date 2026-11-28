//! `loopx review config`: validate a `.loopx/review.yaml` and print the effective MR
//! review configuration with the source of each value. Reads local files only.

use loopx_core::config::{default_config_path, Config};
use loopx_core::loops_config;
use loopx_core::review::config::{self as review_config, Sources, REPO_FILE};
use serde_json::Value;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx review [--base <branch> | --uncommitted] [--dir <path>]
                    [--format text|ndjson] [--gate nit|minor|major|blocker]
                    [--no-tools] [--no-verify] [--budget-usd N]
       loopx review config [--file <path>] [--project <alias>]
       loopx review tools [refresh]

`loopx review` reviews the current branch (all changes since it left --base, default
origin's default branch) or only --uncommitted changes, with the MR Review loop's
prompt, .loopx/review.yaml, guidance files, .loopx/checks, installed linters and the
verification pass. Nothing is posted. Exit code: 0 done, 1 failed, 2 a finding at or
above --gate.

Prints the MR review configuration the MR Review loop would use, with where each
value comes from (default, loop settings, repo, projects.json). --file defaults to
.loopx/review.yaml in the current directory; --project adds that project's
personal `review` block from projects.json.

`tools` lists the linters MR reviews can run as hints and the age of the cached
semgrep rules; `tools refresh` downloads the semgrep rule packs now.";

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn tools(args: &[String]) -> ExitCode {
    use loopx_core::review::{semgrep_rules, tools};
    let runner = tools::SystemLintRunner;
    let dir = semgrep_rules::cache_dir();
    if args.get(1).map(String::as_str) == Some("refresh") {
        let mut failed = false;
        for pack in semgrep_rules::DEFAULT_PACKS {
            println!("Downloading {} ...", semgrep_rules::url_for(pack));
            match semgrep_rules::refresh(&dir, pack, &semgrep_rules::http_fetch) {
                Ok(()) => println!("Saved {}", semgrep_rules::path_for(&dir, pack).display()),
                Err(e) => {
                    failed = true;
                    eprintln!("{pack}: {e}");
                }
            }
        }
        return ExitCode::from(u8::from(failed));
    }
    for tool in tools::ALL {
        let state = if tools::LintRunner::installed(&runner, tool.name()) {
            "installed"
        } else {
            "not installed"
        };
        println!("{:<11} {state}", tool.name());
    }
    for pack in semgrep_rules::DEFAULT_PACKS {
        match semgrep_rules::age(&dir, pack) {
            Some(a) => println!(
                "semgrep {pack}: cached {} day(s) ago ({})",
                a.as_secs() / 86_400,
                semgrep_rules::path_for(&dir, pack).display()
            ),
            None => println!("semgrep {pack}: not cached yet (`loopx review tools refresh`)"),
        }
    }
    ExitCode::SUCCESS
}

fn sealed(model: Option<String>) -> impl Fn(&str, f64) -> Result<(String, Option<f64>), String> {
    move |prompt, budget| {
        let hooks = loopx_core::agents::sealed::SealedHooks {
            model: model.as_deref(),
            ..Default::default()
        };
        let mut sink = |_: &str| {};
        loopx_core::agents::sealed::sealed_call(
            prompt,
            loopx_core::review::local::DEFAULT_TIMEOUT.as_secs_f64(),
            &mut sink,
            Some(budget),
            &hooks,
        )
        .map(|r| (r.text, r.cost_usd))
        .map_err(|e| e.0)
    }
}

fn local(args: &[String]) -> ExitCode {
    use loopx_core::review::{local, taxonomy, verify};
    let format = flag(args, "--format").unwrap_or("text");
    let gate = flag(args, "--gate");
    if gate.is_some_and(|g| !taxonomy::SEVERITIES.contains(&g))
        || !["text", "ndjson"].contains(&format)
    {
        eprintln!("{USAGE}");
        return ExitCode::from(1);
    }
    let budget = flag(args, "--budget-usd")
        .and_then(|b| b.parse::<f64>().ok())
        .filter(|b| *b > 0.0)
        .unwrap_or(1.0);
    let dir = flag(args, "--dir").map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let opts = local::Options {
        dir: dir.clone(),
        base: flag(args, "--base").map(str::to_string),
        uncommitted: args.iter().any(|a| a == "--uncommitted"),
        no_tools: args.iter().any(|a| a == "--no-tools"),
        no_verify: args.iter().any(|a| a == "--no-verify"),
    };
    let Some(template) = local::template(None) else {
        eprintln!("the MR review prompt is missing from this build");
        return ExitCode::from(1);
    };
    let model = loopx_core::bundled::text("loops/mr-review/loop.yaml")
        .and_then(|y| {
            loopx_core::loop_definition::LoopDefinition::from_yaml_str(y, "mr-review").ok()
        })
        .and_then(|d| d.agent.model);
    let report = if opts.no_tools {
        None
    } else {
        local::collect(&opts)
            .ok()
            .map(|(changes, title, description, _)| {
                let (item, cfg, _, _) = local::build(&opts, &changes, &title, &description);
                local::tool_report(&dir, &item, &cfg)
            })
    };
    let main = sealed(model);
    let check = sealed(Some(verify::DEFAULT_MODEL.to_string()));
    let verify_fn: Option<local::ModelFn<'_>> = (!opts.no_verify).then_some(&check);
    match local::run(&opts, &template, budget, &main, verify_fn, report) {
        Ok(review) => {
            if format == "ndjson" {
                println!("{}", local::ndjson(&review, gate));
            } else {
                println!("{}", local::text(&review));
            }
            ExitCode::from(if local::blocked(&review, gate) { 2 } else { 0 })
        }
        Err(e) => {
            if format == "ndjson" {
                println!(
                    "{}",
                    serde_json::json!({"type": "complete", "outcome": "failed", "error": e})
                );
            } else {
                eprintln!("review failed: {e}");
            }
            ExitCode::from(1)
        }
    }
}

pub fn run(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) == Some("tools") {
        return tools(args);
    }
    if args.first().is_none_or(|a| a.starts_with("--")) && !args.iter().any(|a| a == "--help") {
        return local(args);
    }
    if args.first().map(String::as_str) != Some("config") || args.iter().any(|a| a == "--help") {
        println!("{USAGE}");
        return ExitCode::from(if args.iter().any(|a| a == "--help") {
            0
        } else {
            2
        });
    }
    let file = flag(args, "--file").map_or_else(|| PathBuf::from(REPO_FILE), PathBuf::from);
    let repo = match std::fs::read_to_string(&file) {
        Ok(text) => Some(review_config::parse_repo_file(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No {} found; showing defaults.", file.display());
            None
        }
        Err(e) => Some(Err(format!("could not read {}: {e}", file.display()))),
    };
    let personal = match flag(args, "--project") {
        None => None,
        Some(alias) => {
            let project = Config::load(&default_config_path())
                .map_err(|e| e.to_string())
                .and_then(|c| c.project(alias).map_err(|e| e.to_string()));
            match project {
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(1);
                }
                Ok(p) => p.get("review").map(review_config::parse_personal),
            }
        }
    };
    let loop_settings: Option<Value> =
        loops_config::get_loop("mr-review-loop", &loops_config::default_config_path(), None)
            .ok()
            .and_then(|l| l.get("settings").cloned());
    let loop_min = loop_settings
        .as_ref()
        .and_then(|s| s.get("min_severity"))
        .and_then(Value::as_str);
    let cfg = review_config::resolve(Sources {
        loop_min_severity: loop_min,
        repo,
        personal,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&cfg.explain()).unwrap_or_default()
    );
    if cfg.notes.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

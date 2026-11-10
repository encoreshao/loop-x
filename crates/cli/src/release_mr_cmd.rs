//! `loopx release-mr run <job_id>`: the runner the MR Review page's release panel launches.

use loopx_core::agents::sealed::SealedHooks;
use loopx_core::config::home_dir;
use loopx_core::release_mr::{api::GitlabApi, job, run};
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx release-mr run <job_id>";

/// The job id of `run <job_id>`; ids are filename components, never paths.
fn parse(args: &[String]) -> Result<&str, String> {
    match args {
        [sub, id]
            if sub == "run"
                && !id.is_empty()
                && id.len() <= 64
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            Ok(id)
        }
        _ => Err(USAGE.to_string()),
    }
}

pub fn run(args: &[String]) -> ExitCode {
    match run_job(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn run_job(args: &[String]) -> Result<(), String> {
    let id = parse(args)?;
    let dir = job::jobs_dir(&home_dir());
    let rec = job::get(&dir, id).ok_or_else(|| format!("no such release job: {id}"))?;
    let config = loopx_core::gitlab::load_config()
        .ok_or_else(|| "no GitLab configuration found (~/.gitlab/config.json)".to_string());
    let config = match config {
        Ok(c) => c,
        Err(e) => {
            let _ = job::update(&dir, id, |j| {
                j.state = "failed".into();
                j.summary = e.clone();
            });
            return Err(e);
        }
    };
    let fail = |e: String| {
        let _ = job::update(&dir, id, |j| {
            j.state = "failed".into();
            j.summary = e.clone();
        });
        e
    };
    let project = loopx_core::config::Config::load(&loopx_core::config::default_config_path())
        .and_then(|c| c.project(&rec.alias))
        .map_err(|e| fail(e.to_string()))?;
    let api = GitlabApi::for_project(&config, &project).map_err(fail)?;
    run::run(&api, &SealedHooks::default(), &dir, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_requires_a_job_id_and_rejects_unsafe_ids() {
        assert_eq!(
            parse(&args(&["run", "20261110T101500-ab12cd34"])).unwrap(),
            "20261110T101500-ab12cd34"
        );
        for bad in [
            args(&[]),
            args(&["run"]),
            args(&["go", "x"]),
            args(&["run", ""]),
            args(&["run", "../x"]),
            args(&["run", "a/b"]),
            args(&["run", "a", "b"]),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }
}

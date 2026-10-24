use loopx_core::config::{default_config_path, Config, ConfigError};
use loopx_core::pyjson;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx config <aliases|local-paths|worktree-root|assignee|instance>\n       loopx config project <alias>";

pub fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(1);
    };
    match dispatch(command, &args[1..]) {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(Failure::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
        Err(Failure::Config(e)) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Usage(String),
    Config(ConfigError),
}

impl From<ConfigError> for Failure {
    fn from(e: ConfigError) -> Self {
        Failure::Config(e)
    }
}

fn dispatch(command: &str, rest: &[String]) -> Result<String, Failure> {
    let load = || Config::load(&default_config_path());
    Ok(match command {
        "aliases" => load()?.aliases()?.join("\n"),
        "local-paths" => load()?.local_paths()?.join("\n"),
        "worktree-root" => load()?.worktree_root()?,
        "assignee" => load()?.assignee_username()?,
        "instance" => load()?.instance()?,
        "project" => {
            let alias = rest
                .first()
                .ok_or_else(|| Failure::Usage("Usage: loopx config project <alias>".into()))?;
            pyjson::to_string_pretty(&load()?.project(alias)?)
        }
        other => return Err(Failure::Usage(format!("Unknown command: {other}"))),
    })
}

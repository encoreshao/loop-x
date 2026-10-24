//! `loopx secrets`: move plaintext tokens out of `~/.gitlab/config.json` and
//! `~/.slack/config.json` into the secret store, and inspect the store (never values).

use loopx_core::keychain::{self, SecretStore, SERVICE_CONFIG};
use loopx_core::secrets::{self, Kind, Status};
use loopx_core::slack;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str =
    "Usage: loopx secrets migrate [--dry-run] [--yes] [--gitlab-config P] [--slack-config P]
       loopx secrets list [--gitlab-config P] [--slack-config P]
       loopx secrets set <account|keychain:ref> [--service S]     (value read from stdin)
       loopx secrets delete <account|keychain:ref> [--service S]

migrate moves plaintext GitLab tokens and Slack webhook URLs into the secret store (the macOS
Keychain, or the file store when LOOPX_KEYCHAIN=file:<dir>) and leaves keychain:<service>/<account>
references in the JSON. It backs each file up (0600) first, is idempotent, and refuses to change
anything without --yes (--dry-run only prints what it would do). Values are never printed.

WARNING: the external gitlab-config skill (gitlab_api.py) and bin/slack_notify.py read these files
directly and cannot resolve references: after a migration only `loopx` can use the config.";

pub fn run(args: &[String]) -> ExitCode {
    match dispatch(args) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn config_files(args: &[String]) -> Vec<(Kind, PathBuf)> {
    let gitlab = opt(args, "--gitlab-config")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".gitlab")
                .join("config.json")
        });
    let slack = opt(args, "--slack-config")
        .map(PathBuf::from)
        .unwrap_or_else(slack::default_config_path);
    vec![(Kind::Gitlab, gitlab), (Kind::Slack, slack)]
}

fn state_word(s: Status, applied: bool) -> &'static str {
    match (s, applied) {
        (Status::Plain, true) => "migrated",
        (Status::Plain, false) => "would migrate",
        (Status::Reference, _) => "already a reference",
        (Status::Empty, _) => "empty, skipped",
    }
}

fn dispatch(args: &[String]) -> Result<String, String> {
    let rest = &args[args.len().min(1)..];
    match args.first().map(String::as_str) {
        Some("migrate") => migrate(rest),
        Some("list") => list(rest),
        Some("set") => {
            let (store, account) = target(rest)?;
            let mut value = String::new();
            std::io::stdin()
                .read_to_string(&mut value)
                .map_err(|e| e.to_string())?;
            let value = value.trim_end_matches(['\n', '\r']);
            if value.is_empty() {
                return Err("no value on stdin".into());
            }
            store.set(&account, value).map_err(|e| e.to_string())?;
            Ok(format!(
                "stored {}",
                keychain::format_ref(&service_base(rest), &account)
            ))
        }
        Some("delete") => {
            let (store, account) = target(rest)?;
            store.delete(&account).map_err(|e| e.to_string())?;
            Ok(format!("deleted {account}"))
        }
        _ => Err(USAGE.to_string()),
    }
}

fn service_base(args: &[String]) -> String {
    if let Some(r) = args.iter().find_map(|a| keychain::parse_ref(a)) {
        return r.0.to_string();
    }
    opt(args, "--service").unwrap_or(SERVICE_CONFIG).to_string()
}

fn target(args: &[String]) -> Result<(Box<dyn SecretStore>, String), String> {
    let positional = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && (*i == 0 || args[i - 1] != "--service"))
        .map(|(_, a)| a.as_str())
        .ok_or_else(|| USAGE.to_string())?;
    let (service, account) = match keychain::parse_ref(positional) {
        Some((s, a)) => (s.to_string(), a.to_string()),
        None => (service_base(args), positional.to_string()),
    };
    if !service.starts_with(keychain::SERVICE_PREFIX) {
        return Err(format!(
            "service must start with {}",
            keychain::SERVICE_PREFIX
        ));
    }
    Ok((
        keychain::open_store(&service).map_err(|e| e.to_string())?,
        account,
    ))
}

fn migrate(args: &[String]) -> Result<String, String> {
    let dry_run = flag(args, "--dry-run");
    let yes = flag(args, "--yes");
    let files = config_files(args);
    let mut out = Vec::new();
    // Plan everything first so that a refusal still shows what would happen.
    let mut pending = 0usize;
    for (kind, path) in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            pending += secrets::plan(*kind, &v)
                .iter()
                .filter(|i| i.status == Status::Plain)
                .count();
        }
    }
    if !dry_run && !yes && pending > 0 {
        return Err(format!(
            "{pending} plaintext secret(s) would move into the secret store. Re-run with --dry-run \
             to see which, or --yes to migrate.\nNote: the external gitlab-config skill and \
             bin/slack_notify.py cannot read keychain: references."
        ));
    }
    let store = if dry_run || pending == 0 {
        None
    } else {
        Some(keychain::open_store(SERVICE_CONFIG).map_err(|e| e.to_string())?)
    };
    // A store is only needed to write; a dry run uses an empty in-memory stand-in.
    let dummy = keychain::MemoryStore::new("dry-run");
    let mut moved = 0usize;
    for (kind, path) in &files {
        if !path.exists() {
            out.push(format!(
                "{}: {} not found, skipped",
                kind.label(),
                path.display()
            ));
            continue;
        }
        let store_ref: &dyn SecretStore = match &store {
            Some(s) => s.as_ref(),
            None => &dummy,
        };
        let outcome = secrets::migrate_file(*kind, path, store_ref, dry_run || store.is_none())?;
        for item in &outcome.items {
            out.push(format!(
                "{}: {} -> {} [{}]",
                kind.label(),
                item.display_path(),
                item.reference,
                state_word(item.status, outcome.applied)
            ));
        }
        if outcome.applied {
            moved += outcome
                .items
                .iter()
                .filter(|i| i.status == Status::Plain)
                .count();
        }
        if let Some(b) = &outcome.backup {
            out.push(format!(
                "{}: backup written to {} (0600)",
                kind.label(),
                b.display()
            ));
        }
    }
    if dry_run {
        out.push("dry run: nothing was changed".into());
    } else if moved > 0 {
        out.push(format!(
            "migrated {moved} secret(s). The gitlab-config skill and bin/slack_notify.py can no \
             longer read these files; only loopx resolves keychain: references."
        ));
    } else {
        out.push("nothing to migrate".into());
    }
    Ok(out.join("\n"))
}

fn list(args: &[String]) -> Result<String, String> {
    let store = keychain::open_store(SERVICE_CONFIG).map_err(|e| e.to_string())?;
    let mut out = vec![format!("store {}:", store.service())];
    let accounts = store.list().map_err(|e| e.to_string())?;
    if accounts.is_empty() {
        out.push("  (no accounts)".into());
    }
    out.extend(accounts.iter().map(|a| format!("  {a}")));
    for (kind, path) in config_files(args) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for item in secrets::plan(kind, &v) {
            let status = match item.status {
                Status::Plain => "PLAINTEXT".to_string(),
                Status::Empty => "empty".to_string(),
                Status::Reference => {
                    let actual = item
                        .path
                        .iter()
                        .try_fold(&v, |n, k| n.get(k))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let found = keychain::resolve_value(actual).is_ok();
                    if found {
                        "reference (resolves)"
                    } else {
                        "reference (MISSING in store)"
                    }
                    .to_string()
                }
            };
            out.push(format!(
                "{}: {} [{}]",
                kind.label(),
                item.display_path(),
                status
            ));
        }
    }
    Ok(out.join("\n"))
}

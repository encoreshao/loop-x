//! Moving plaintext secrets out of `~/.gitlab/config.json` and `~/.slack/config.json`
//! into the secret store, leaving `keychain:<service>/<account>` references behind.
//!
//! Warning: the external Python `gitlab-config` skill (and `bin/slack_notify.py`) read these
//! files directly and know nothing about references, so they stop working on a migrated file.
//! Only the Rust `loopx` binary resolves them.

use crate::keychain::{self, SecretStore, SERVICE_CONFIG};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Gitlab,
    Slack,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Gitlab => "gitlab",
            Kind::Slack => "slack",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A plaintext secret that would be (dry run) or was moved.
    Plain,
    /// Already a `keychain:` reference.
    Reference,
    /// Empty or not a string: nothing to move.
    Empty,
}

/// One secret-bearing field. Never carries the secret itself.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    /// JSON path, e.g. `["instances", "work", "token"]`.
    pub path: Vec<String>,
    pub account: String,
    pub reference: String,
    pub status: Status,
}

impl Item {
    pub fn display_path(&self) -> String {
        self.path.join(".")
    }
}

struct Field {
    path: Vec<String>,
    account: String,
}

fn fields(kind: Kind, config: &Value) -> Vec<Field> {
    let mut out = Vec::new();
    let mut names = |section: &str, leaf: Option<&str>, account_root: &str| {
        if let Some(obj) = config.get(section).and_then(Value::as_object) {
            for name in obj.keys() {
                let mut path = vec![section.to_string(), name.clone()];
                let mut account = format!("{account_root}/{name}");
                if let Some(leaf) = leaf {
                    path.push(leaf.to_string());
                    account = format!("{account}/{leaf}");
                }
                out.push(Field { path, account });
            }
        }
    };
    match kind {
        Kind::Gitlab => {
            names("instances", Some("token"), "gitlab/instances");
            names("bundles", Some("token"), "gitlab/bundles");
        }
        Kind::Slack => {
            names("bundle_webhooks", None, "slack/bundle_webhooks");
        }
    }
    if kind == Kind::Slack && config.get("webhook_url").is_some() {
        out.insert(
            0,
            Field {
                path: vec!["webhook_url".into()],
                account: "slack/webhook_url".into(),
            },
        );
    }
    out
}

fn value_at<'a>(config: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(config, |v, k| v.get(k))
}

fn set_at(config: &mut Value, path: &[String], new: Value) {
    let mut cur = config;
    for k in &path[..path.len() - 1] {
        cur = &mut cur[k.as_str()];
    }
    cur[path[path.len() - 1].as_str()] = new;
}

/// What a migration of `config` would touch, without values.
pub fn plan(kind: Kind, config: &Value) -> Vec<Item> {
    fields(kind, config)
        .into_iter()
        .map(|f| {
            let status = match value_at(config, &f.path).and_then(Value::as_str) {
                Some(s) if keychain::is_ref(s) => Status::Reference,
                Some(s) if !s.is_empty() => Status::Plain,
                _ => Status::Empty,
            };
            Item {
                kind,
                reference: keychain::format_ref(SERVICE_CONFIG, &f.account),
                account: f.account,
                path: f.path,
                status,
            }
        })
        .collect()
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)
}

#[derive(Debug)]
pub struct Outcome {
    pub items: Vec<Item>,
    /// Where the 0600 backup went (None for a dry run or when nothing was plaintext).
    pub backup: Option<PathBuf>,
    pub applied: bool,
}

/// Migrate one config file. With `dry_run` nothing is written anywhere. Otherwise: back
/// the file up (0600), store each plaintext secret and read it back, then rewrite the
/// file with references. Idempotent: references and empty fields are left alone.
pub fn migrate_file(
    kind: Kind,
    path: &Path,
    store: &dyn SecretStore,
    dry_run: bool,
) -> Result<Outcome, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut config: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let items = plan(kind, &config);
    let todo: Vec<&Item> = items.iter().filter(|i| i.status == Status::Plain).collect();
    if dry_run || todo.is_empty() {
        return Ok(Outcome {
            items,
            backup: None,
            applied: false,
        });
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".bak-{stamp}"));
    let backup = PathBuf::from(backup);
    write_private(&backup, text.as_bytes())
        .map_err(|e| format!("cannot write backup {}: {e}", backup.display()))?;
    for item in &todo {
        let secret = value_at(&config, &item.path)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        store
            .set(&item.account, &secret)
            .map_err(|e| format!("{}: {e}", item.display_path()))?;
        if store.get(&item.account).ok().flatten().as_deref() != Some(secret.as_str()) {
            return Err(format!(
                "{}: the secret store did not return what was just written; {} left untouched",
                item.display_path(),
                path.display()
            ));
        }
    }
    for item in &todo {
        set_at(
            &mut config,
            &item.path,
            Value::String(item.reference.clone()),
        );
    }
    let mut out = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    out.push('\n');
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, out.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Outcome {
        items,
        backup: Some(backup),
        applied: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryStore;
    use serde_json::json;

    fn write(dir: &Path, name: &str, v: &Value) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap();
        p
    }

    #[test]
    fn gitlab_tokens_move_and_leave_references() {
        let d = tempfile::tempdir().unwrap();
        let p = write(
            d.path(),
            "g.json",
            &json!({"default":"work","instances":{"work":{"url":"https://gl","token":"glpat-AAA"}},
                    "bundles":{"ops":{"instance":"work","token":"glpat-BBB"}},"projects":{"a":{"id":"1"}}}),
        );
        let store = MemoryStore::new("svc");
        let out = migrate_file(Kind::Gitlab, &p, &store, false).unwrap();
        assert!(out.applied);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(!after.contains("glpat-"), "{after}");
        assert!(after.contains("keychain:loop-engineering.config/gitlab/instances/work/token"));
        assert_eq!(
            store.get("gitlab/bundles/ops/token").unwrap().unwrap(),
            "glpat-BBB"
        );
        let backup = std::fs::read_to_string(out.backup.unwrap()).unwrap();
        assert!(backup.contains("glpat-AAA"));
        let v: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v["instances"]["work"]["url"], "https://gl");
        assert_eq!(v["projects"]["a"]["id"], "1");
    }

    #[cfg(unix)]
    #[test]
    fn backup_and_rewritten_file_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = write(
            d.path(),
            "s.json",
            &json!({"webhook_url":"https://hooks/x"}),
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = migrate_file(Kind::Slack, &p, &MemoryStore::new("s"), false).unwrap();
        for f in [p, out.backup.unwrap()] {
            assert_eq!(
                std::fs::metadata(f).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn migration_is_idempotent_and_dry_run_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = write(
            d.path(),
            "s.json",
            &json!({"webhook_url":"https://hooks/default","bundle_webhooks":{"a/b":"https://hooks/b","e":""}}),
        );
        let store = MemoryStore::new("s");
        let before = std::fs::read_to_string(&p).unwrap();
        let dry = migrate_file(Kind::Slack, &p, &store, true).unwrap();
        assert!(!dry.applied && dry.backup.is_none());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        assert!(store.list().unwrap().is_empty());
        assert_eq!(
            dry.items
                .iter()
                .filter(|i| i.status == Status::Plain)
                .count(),
            2
        );
        migrate_file(Kind::Slack, &p, &store, false).unwrap();
        let once = std::fs::read_to_string(&p).unwrap();
        let again = migrate_file(Kind::Slack, &p, &store, false).unwrap();
        assert!(!again.applied && again.backup.is_none());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), once);
        assert_eq!(
            store.get("slack/bundle_webhooks/a/b").unwrap().unwrap(),
            "https://hooks/b"
        );
        assert!(once.contains(r#""e": """#));
    }

    #[test]
    fn a_store_that_loses_the_value_leaves_the_config_untouched() {
        struct Lossy;
        impl SecretStore for Lossy {
            fn service(&self) -> String {
                "x".into()
            }
            fn get(&self, _: &str) -> Result<Option<String>, keychain::SecretError> {
                Ok(None)
            }
            fn set(&self, _: &str, _: &str) -> Result<(), keychain::SecretError> {
                Ok(())
            }
            fn delete(&self, _: &str) -> Result<(), keychain::SecretError> {
                Ok(())
            }
            fn list(&self) -> Result<Vec<String>, keychain::SecretError> {
                Ok(vec![])
            }
        }
        let d = tempfile::tempdir().unwrap();
        let p = write(
            d.path(),
            "s.json",
            &json!({"webhook_url":"https://hooks/x"}),
        );
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(migrate_file(Kind::Slack, &p, &Lossy, false).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
    }
}

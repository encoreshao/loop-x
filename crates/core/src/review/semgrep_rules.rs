//! Local cache of semgrep registry rule packs, so reviews run semgrep offline.
//! A pack (`p/default`) is downloaded at most weekly into
//! `<loopx home>/cache/semgrep/<pack>.yml`, checked to parse as a rules file, and
//! written atomically. A failed refresh keeps the old file. Rules are fetched on the
//! user's machine and never bundled with LoopX (Semgrep Rules License).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DEFAULT_PACKS: &[&str] = &["p/default"];
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_BYTES: usize = 20 * 1024 * 1024;

pub fn cache_dir() -> PathBuf {
    crate::config::home_dir().join("cache").join("semgrep")
}

fn file_name(pack: &str) -> String {
    pack.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        + ".yml"
}

pub fn path_for(dir: &Path, pack: &str) -> PathBuf {
    dir.join(file_name(pack))
}

pub fn url_for(pack: &str) -> String {
    format!("https://semgrep.dev/c/{pack}")
}

/// Packs that have a cached file, as `--config` paths.
pub fn cached(dir: &Path, packs: &[&str]) -> Vec<PathBuf> {
    packs
        .iter()
        .map(|p| path_for(dir, p))
        .filter(|p| p.is_file())
        .collect()
}

/// Age of a pack's cached file, `None` when there is none.
pub fn age(dir: &Path, pack: &str) -> Option<Duration> {
    let modified = std::fs::metadata(path_for(dir, pack))
        .ok()?
        .modified()
        .ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn validate(text: &str) -> Result<(), String> {
    if text.len() > MAX_BYTES {
        return Err("rule pack too large".into());
    }
    let v: serde_json::Value =
        serde_norway::from_str(text).map_err(|e| format!("not a rules file: {e}"))?;
    match v.get("rules").and_then(|r| r.as_array()) {
        Some(r) if !r.is_empty() => Ok(()),
        _ => Err("no rules in the download".into()),
    }
}

/// Download `pack` with `fetch` and store it; `Ok(rule count)`.
pub fn refresh(
    dir: &Path,
    pack: &str,
    fetch: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let text = fetch(&url_for(pack))?;
    validate(&text)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let target = path_for(dir, pack);
    let tmp = target.with_extension("yml.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())
}

/// Refresh every pack older than [`MAX_AGE`] (or missing); returns one note per failure.
pub fn refresh_stale(
    dir: &Path,
    packs: &[&str],
    fetch: &dyn Fn(&str) -> Result<String, String>,
) -> Vec<String> {
    packs
        .iter()
        .filter(|p| age(dir, p).is_none_or(|a| a > MAX_AGE))
        .filter_map(|p| {
            refresh(dir, p, fetch)
                .err()
                .map(|e| format!("semgrep {p}: refresh failed ({e})"))
        })
        .collect()
}

/// The real download.
pub fn http_fetch(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT).build();
    agent
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_validates_and_keeps_the_old_file_on_failure() {
        let d = tempfile::tempdir().unwrap();
        assert!(cached(d.path(), DEFAULT_PACKS).is_empty());
        let good = |url: &str| {
            assert_eq!(url, "https://semgrep.dev/c/p/default");
            Ok("rules:\n- id: x\n  pattern: eval(...)\n  message: m\n  languages: [python]\n  severity: ERROR\n".to_string())
        };
        assert!(refresh_stale(d.path(), DEFAULT_PACKS, &good).is_empty());
        let file = path_for(d.path(), "p/default");
        assert_eq!(file.file_name().unwrap(), "p-default.yml");
        assert_eq!(cached(d.path(), DEFAULT_PACKS), std::slice::from_ref(&file));
        assert!(refresh_stale(d.path(), DEFAULT_PACKS, &|_| panic!("fresh, no fetch")).is_empty());
        let bad = |_: &str| Ok("<html>rate limited</html>".to_string());
        assert!(refresh(d.path(), "p/default", &bad).is_err());
        let offline = |_: &str| Err("offline".to_string());
        assert!(refresh(d.path(), "p/default", &offline).is_err());
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("eval(...)"),
            "old file kept"
        );
        let notes = refresh_stale(d.path(), &["p/python"], &offline);
        assert_eq!(notes, ["semgrep p/python: refresh failed (offline)"]);
    }
}

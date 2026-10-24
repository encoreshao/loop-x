//! The loop templates and scripted eval cases shipped inside the binary, so
//! `loopx loop init` and `loopx loop eval` work without a source checkout.
//! A unit test keeps these lists equal to `templates/*/loop.yaml` and
//! `evals/cases/*.yaml`.

/// `(name, loop.yaml text)`, sorted by name.
pub const TEMPLATES: &[(&str, &str)] = &[
    (
        "ci-fixer",
        include_str!("../../../templates/ci-fixer/loop.yaml"),
    ),
    (
        "daily-triage",
        include_str!("../../../templates/daily-triage/loop.yaml"),
    ),
    (
        "data-quality",
        include_str!("../../../templates/data-quality/loop.yaml"),
    ),
    (
        "dependency-updater",
        include_str!("../../../templates/dependency-updater/loop.yaml"),
    ),
    (
        "generic",
        include_str!("../../../templates/generic/loop.yaml"),
    ),
    (
        "gitlab-issue",
        include_str!("../../../templates/gitlab-issue/loop.yaml"),
    ),
    (
        "incident-response",
        include_str!("../../../templates/incident-response/loop.yaml"),
    ),
    (
        "issue-fixer",
        include_str!("../../../templates/issue-fixer/loop.yaml"),
    ),
    (
        "pr-review",
        include_str!("../../../templates/pr-review/loop.yaml"),
    ),
    (
        "research-monitor",
        include_str!("../../../templates/research-monitor/loop.yaml"),
    ),
];

/// `(file name, case yaml text)`, sorted by file name.
pub const EVAL_CASES: &[(&str, &str)] = &[
    (
        "ambiguous-task.yaml",
        include_str!("../../../evals/cases/ambiguous-task.yaml"),
    ),
    (
        "budget.yaml",
        include_str!("../../../evals/cases/budget.yaml"),
    ),
    (
        "no-progress.yaml",
        include_str!("../../../evals/cases/no-progress.yaml"),
    ),
    (
        "retry-then-pass.yaml",
        include_str!("../../../evals/cases/retry-then-pass.yaml"),
    ),
    (
        "retry.yaml",
        include_str!("../../../evals/cases/retry.yaml"),
    ),
    (
        "success.yaml",
        include_str!("../../../evals/cases/success.yaml"),
    ),
    (
        "unsafe-action.yaml",
        include_str!("../../../evals/cases/unsafe-action.yaml"),
    ),
    (
        "verification-gate.yaml",
        include_str!("../../../evals/cases/verification-gate.yaml"),
    ),
];

use std::path::Path;

fn read_dir_templates(root: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(root.join("templates"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("loop.yaml")).ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), text))
        })
        .collect();
    out.sort();
    out
}

/// `templates/<name>/loop.yaml` of the checkout named by `$LOOPX_ROOT` when
/// that has a `templates/` directory (so a new template there is picked up,
/// like the Python), else the copies compiled into the binary.
pub fn available_templates() -> Vec<(String, String)> {
    if let Some(root) = std::env::var_os("LOOPX_ROOT").filter(|v| !v.is_empty()) {
        let found = read_dir_templates(Path::new(&root));
        if !found.is_empty() {
            return found;
        }
    }
    TEMPLATES
        .iter()
        .map(|(n, t)| (n.to_string(), t.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn embedded_templates_match_the_checkout() {
        let mut on_disk: Vec<String> = std::fs::read_dir(root().join("templates"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().join("loop.yaml").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let embedded: Vec<&str> = TEMPLATES.iter().map(|(n, _)| *n).collect();
        assert_eq!(embedded, on_disk);
    }

    #[test]
    fn embedded_eval_cases_match_the_checkout() {
        let mut on_disk: Vec<String> = std::fs::read_dir(root().join("evals/cases"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".yaml"))
            .collect();
        on_disk.sort();
        let embedded: Vec<&str> = EVAL_CASES.iter().map(|(n, _)| *n).collect();
        assert_eq!(embedded, on_disk);
    }
}

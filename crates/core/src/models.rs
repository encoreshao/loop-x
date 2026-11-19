//! The Claude model each kind of work runs on unless its loop names one.

/// Writing, fixing and reviewing code.
pub const CODE: &str = "opus";
/// Everything else: triage, digests, summaries, chat.
pub const GENERAL: &str = "sonnet";

/// `declared` unless it is empty or `"default"`, else `fallback`.
pub fn pick<'a>(declared: Option<&'a str>, fallback: &'a str) -> &'a str {
    declared
        .map(str::trim)
        .filter(|m| !m.is_empty() && *m != "default")
        .unwrap_or(fallback)
}

/// `--model <model>` for a `claude` argv.
pub fn args(model: &str) -> [String; 2] {
    ["--model".to_string(), model.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_model_wins_and_default_or_blank_falls_back() {
        assert_eq!(pick(Some("haiku"), CODE), "haiku");
        assert_eq!(pick(Some(" default "), CODE), CODE);
        assert_eq!(pick(Some(""), GENERAL), GENERAL);
        assert_eq!(pick(None, GENERAL), GENERAL);
    }
}

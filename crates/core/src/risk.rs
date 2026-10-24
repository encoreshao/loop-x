//! Deterministic keyword-based risk scorer for issue text (mirrors `bin/risk.py`).
//! Pure: takes a string, no event log, no LLM. Advisory only.

use serde_json::{json, Value};

/// Keyword -> points, in definition order (the order `matched_keywords` uses).
pub const RISK_KEYWORDS: &[(&str, i64)] = &[
    ("database migration", 30),
    ("authentication", 30),
    ("security", 30),
    ("production configuration", 20),
    ("payment", 40),
    ("dependency update", 20),
    ("test-only change", -10),
    ("documentation", -20),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskScore {
    pub score: i64,
    pub level: &'static str,
    pub matched_keywords: Vec<&'static str>,
}

impl RiskScore {
    /// `{"score", "level", "matched_keywords"}` in the Python key order.
    pub fn to_value(&self) -> Value {
        json!({
            "score": self.score,
            "level": self.level,
            "matched_keywords": self.matched_keywords,
        })
    }
}

/// LOW (<30), MEDIUM (30..60), HIGH (60..80), CRITICAL (>=80).
pub fn level_for(value: i64) -> &'static str {
    match value {
        v if v >= 80 => "CRITICAL",
        v if v >= 60 => "HIGH",
        v if v >= 30 => "MEDIUM",
        _ => "LOW",
    }
}

/// Case-insensitive substring match against [`RISK_KEYWORDS`]; the score is the
/// sum of matched points clamped to a minimum of 0.
pub fn score(text: &str) -> RiskScore {
    let lowered = text.to_lowercase();
    let matched: Vec<(&'static str, i64)> = RISK_KEYWORDS
        .iter()
        .filter(|(kw, _)| lowered.contains(kw))
        .copied()
        .collect();
    let raw: i64 = matched.iter().map(|(_, pts)| pts).sum();
    let clamped = raw.max(0);
    RiskScore {
        score: clamped,
        level: level_for(clamped),
        matched_keywords: matched.into_iter().map(|(kw, _)| kw).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_keywords_is_zero_low() {
        let r = score("Fix a typo in the README");
        assert_eq!((r.score, r.level), (0, "LOW"));
        assert!(r.matched_keywords.is_empty());
    }

    #[test]
    fn single_keyword_case_insensitive() {
        let r = score("Add SECURITY headers to the login page");
        assert_eq!((r.score, r.level), (30, "MEDIUM"));
        assert_eq!(r.matched_keywords, vec!["security"]);
    }

    #[test]
    fn multiple_keywords_sum_in_definition_order() {
        let r = score("Payment authentication database migration for prod rollout");
        assert_eq!((r.score, r.level), (100, "CRITICAL"));
        assert_eq!(
            r.matched_keywords,
            vec!["database migration", "authentication", "payment"]
        );
    }

    #[test]
    fn negative_total_clamps_to_zero() {
        let r = score("Update the documentation for a test-only change");
        assert_eq!((r.score, r.level), (0, "LOW"));
    }

    #[test]
    fn level_thresholds() {
        for (v, l) in [
            (0, "LOW"),
            (29, "LOW"),
            (30, "MEDIUM"),
            (59, "MEDIUM"),
            (60, "HIGH"),
            (79, "HIGH"),
            (80, "CRITICAL"),
        ] {
            assert_eq!(level_for(v), l);
        }
    }
}

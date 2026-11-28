//! Review limits, severities, verdicts and areas shared by every review surface.

pub const DIFF_CAP_BYTES: usize = 150_000;
pub const MAX_FINDINGS: usize = 15;
pub const MAX_BODY: usize = 2000;
pub const MAX_SUMMARY: usize = 600;
pub const MAX_TITLE: usize = 120;
pub const MAX_OVERVIEW: usize = 4;
pub const MAX_RISKS: usize = 8;
pub const MAX_NOTE: usize = 300;
pub const SEVERITIES: [&str; 4] = ["nit", "minor", "major", "blocker"];
pub const VERDICTS: [&str; 3] = ["approve", "comment", "request_changes"];
pub const LEVELS: [&str; 3] = ["low", "medium", "high"];
/// `(key, label)`; the model answers with the key.
pub const AREAS: [(&str, &str); 8] = [
    ("correctness", "Correctness"),
    ("security", "Security"),
    ("performance", "Performance"),
    ("data", "Data & migrations"),
    ("reliability", "Reliability"),
    ("compatibility", "Compatibility"),
    ("tests", "Tests"),
    ("maintainability", "Maintainability"),
];

pub fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "Looks good",
        "request_changes" => "Changes requested",
        _ => "Comments",
    }
}

pub fn sev_rank(name: &str) -> usize {
    SEVERITIES.iter().position(|s| *s == name).unwrap_or(1)
}

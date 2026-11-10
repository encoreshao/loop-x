//! The title and description, rendered in code from the agent's structured summary.

use super::authors::Resolved;
use super::summarize::Summary;
use crate::mr_fix::notes::safe_text;

const TITLE_PREFIX: &str = "Release: ";
const TITLE_MAX: usize = 255;

/// Drops commit-hash-shaped words (7-40 hex characters with at least one digit).
fn strip_hashes(text: &str) -> String {
    let hash = regex::Regex::new(r"\b[0-9a-f]{7,40}\b").expect("hash regex");
    hash.replace_all(text, |m: &regex::Captures<'_>| {
        if m[0].chars().any(|c| c.is_ascii_digit()) {
            String::new()
        } else {
            m[0].to_string()
        }
    })
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

/// Breaks GitLab cross-references (`#123`, `!45`, `&9`) so a release MR cannot post
/// "mentioned in" notes on other issues and merge requests.
fn defang_references(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, c) in chars.iter().enumerate() {
        out.push(*c);
        if matches!(c, '#' | '!' | '&') && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric()) {
            out.push('\u{200b}');
        }
    }
    out
}

/// Untrusted agent text made safe for a GitLab description: no hashes, one line,
/// mentions, images and references defused, HTML escaped and Markdown links broken.
fn clean(text: &str) -> String {
    defang_references(&safe_text(&strip_hashes(text)))
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace("](", "]\u{200b}(")
}

/// The same for a title, which GitLab shows as plain text: angle brackets are dropped.
fn clean_title(text: &str) -> String {
    defang_references(&safe_text(&strip_hashes(text)))
        .replace(['<', '>'], "")
        .replace("](", "]\u{200b}(")
}

/// `(title, description)` for the release MR. Only `resolved` can add mentions.
pub fn render(summary: &Summary, resolved: &Resolved) -> (String, String) {
    let title: String = format!("{TITLE_PREFIX}{}", clean_title(&summary.title_brief))
        .chars()
        .take(TITLE_MAX)
        .collect();
    let mut body = String::from("## What's in this release\n\n");
    for f in &summary.features {
        let authors: Vec<String> = f
            .authors
            .iter()
            .map(|a| clean(&a.replace('@', "")))
            .filter(|a| !a.is_empty() && !a.to_ascii_lowercase().ends_with("[bot]"))
            .collect();
        body.push_str(&format!("- **{}**: {}", clean(&f.name), clean(&f.summary)));
        if !authors.is_empty() {
            body.push_str(&format!(" _({})_", authors.join(", ")));
        }
        body.push('\n');
    }
    let plain: Vec<String> = resolved
        .plain
        .iter()
        .map(|p| clean(&p.replace('@', "")))
        .collect();
    if !resolved.tags.is_empty() {
        let tags: Vec<String> = resolved.tags.iter().map(|t| format!("@{t}")).collect();
        body.push_str(&format!("\ncc {}", tags.join(" ")));
        if !plain.is_empty() {
            body.push_str(&format!(" (also: {})", plain.join(", ")));
        }
        body.push('\n');
    } else if !plain.is_empty() {
        body.push_str(&format!("\nAlso contributed: {}\n", plain.join(", ")));
    }
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_mr::summarize::Feature;

    fn summary() -> Summary {
        Summary {
            title_brief: "order export, faster search".into(),
            features: vec![
                Feature {
                    name: "Order export".into(),
                    summary: "Customers can download their orders as CSV.".into(),
                    authors: vec!["Ann Lee".into()],
                },
                Feature {
                    name: "Faster search".into(),
                    summary: "Search results load in under a second.".into(),
                    authors: vec!["Bo".into(), "Cy".into()],
                },
            ],
        }
    }

    #[test]
    fn title_is_release_colon_brief_trimmed_to_255() {
        let (t, _) = render(&summary(), &Resolved::default());
        assert_eq!(t, "Release: order export, faster search");
        let mut long = summary();
        long.title_brief = "x".repeat(400);
        let (t, _) = render(&long, &Resolved::default());
        assert_eq!(t.chars().count(), 255);
        assert!(t.starts_with("Release: "));
    }

    #[test]
    fn description_has_a_bullet_per_feature_with_its_authors() {
        let (_, d) = render(&summary(), &Resolved::default());
        assert!(
            d.contains(
                "- **Order export**: Customers can download their orders as CSV. _(Ann Lee)_"
            ),
            "{d}"
        );
        assert!(
            d.contains("- **Faster search**: Search results load in under a second. _(Bo, Cy)_"),
            "{d}"
        );
    }

    #[test]
    fn description_contains_no_commit_hash_shaped_tokens() {
        let mut s = summary();
        s.features[0].summary = "Fixed in 3f2a9bc1d4 and abcdef1234567.".into();
        let (_, d) = render(&s, &Resolved::default());
        assert!(
            !d.contains("3f2a9bc1d4") && !d.contains("abcdef1234567"),
            "{d}"
        );
    }

    #[test]
    fn cc_line_is_built_from_resolved_tags_only_even_if_the_agent_names_someone_else() {
        let mut s = summary();
        s.features[0].authors = vec!["@mallory".into()];
        let r = Resolved {
            tags: vec!["ann".into(), "bo".into()],
            plain: vec!["Zed".into()],
        };
        let (_, d) = render(&s, &r);
        assert!(d.trim_end().ends_with("cc @ann @bo (also: Zed)"), "{d}");
        assert_eq!(d.matches("@mallory").count(), 0, "{d}");
        assert_eq!(d.matches('@').count(), 2, "only the two resolved tags: {d}");
    }

    #[test]
    fn bot_authors_are_not_credited_on_a_feature() {
        let mut s = summary();
        s.features[0].authors = vec!["renovate[bot]".into(), "Ann Lee".into()];
        s.features[1].authors = vec!["dependabot[bot]".into()];
        let (_, d) = render(&s, &Resolved::default());
        assert!(!d.contains("[bot]"), "{d}");
        assert!(d.contains("_(Ann Lee)_"), "{d}");
        assert!(
            d.contains("- **Faster search**: Search results load in under a second.\n"),
            "no empty parentheses: {d}"
        );
    }

    #[test]
    fn html_links_and_references_in_agent_text_are_neutralised() {
        let mut s = summary();
        s.features[0].summary = "see <img src=\"https://t/p.png\"> and [click](http://e.com) fixes #123, !45 and &9 <b>x</b>".into();
        s.title_brief = "<script>x</script> #77".into();
        let (t, d) = render(&s, &Resolved::default());
        for text in [&t, &d] {
            assert!(!text.contains('<') || text.contains("&lt;"), "{text}");
            assert!(
                !text.contains("<img") && !text.contains("<script") && !text.contains("<b>"),
                "{text}"
            );
            assert!(!text.contains("]("), "{text}");
            assert!(
                !text.contains("#123")
                    && !text.contains("!45")
                    && !text.contains("&9")
                    && !text.contains("#77"),
                "{text}"
            );
        }
    }

    #[test]
    fn no_cc_line_when_nobody_is_known() {
        let (_, d) = render(&summary(), &Resolved::default());
        assert!(!d.contains("cc "), "{d}");
    }

    #[test]
    fn agent_text_is_defused() {
        let mut s = summary();
        s.features[0].summary =
            "line one\n/merge\n<!-- hidden --> ping @all ![x](http://e/i.png)".into();
        let (_, d) = render(&s, &Resolved::default());
        assert!(!d.lines().any(|l| l.trim_start().starts_with('/')), "{d}");
        assert!(
            !d.contains("<!--") && !d.contains("@all") && !d.contains("!["),
            "{d}"
        );
    }
}

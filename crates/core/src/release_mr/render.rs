//! The title and description, rendered in code from the agent's structured summary.

use super::authors::Resolved;
use super::summarize::Summary;
use crate::mr_fix::notes::safe_text;

const TITLE_PREFIX: &str = "Release: ";
/// A release title stays short enough to read in a list; the rest goes in the description.
const TITLE_MAX: usize = 72;
const MAX_HIGHLIGHTS: usize = 4;

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

/// Where a release lands, in words a reader knows: `production` for the usual main branches.
fn environment(target: &str) -> Option<&'static str> {
    match target.to_ascii_lowercase().as_str() {
        "master" | "main" | "production" | "prod" => Some("production"),
        "staging" => Some("staging"),
        _ => None,
    }
}

fn is_bot(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with("[bot]")
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// `Release: a, b +N more`: as many leading items as fit, the rest counted.
fn fit_title(brief: &str) -> String {
    let items: Vec<&str> = brief
        .split(',')
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .collect();
    let room = TITLE_MAX.saturating_sub(TITLE_PREFIX.chars().count());
    for keep in (1..=items.len()).rev() {
        let joined = items[..keep].join(", ");
        let rest = items.len() - keep;
        let text = if rest == 0 {
            joined
        } else {
            format!("{joined} +{rest} more")
        };
        if text.chars().count() <= room {
            return format!("{TITLE_PREFIX}{text}");
        }
    }
    let first = items.first().copied().unwrap_or("changes");
    let cut: String = first.chars().take(room.saturating_sub(1)).collect();
    format!("{TITLE_PREFIX}{cut}\u{2026}")
}

/// `(title, description)` for the release MR. Only `resolved` can add mentions.
/// `branches` is `(source, target)`.
pub fn render(summary: &Summary, resolved: &Resolved, branches: (&str, &str)) -> (String, String) {
    let title = fit_title(&clean_title(&summary.title_brief));
    let (source, target) = (branches.0.replace('`', ""), branches.1.replace('`', ""));
    let env = environment(&target);

    // Who is credited, by name, per feature (bots and empty names dropped).
    let credited: Vec<Vec<String>> = summary
        .features
        .iter()
        .map(|f| {
            f.authors
                .iter()
                .map(|a| a.replace('@', "").trim().to_string())
                .filter(|a| !a.is_empty() && !is_bot(a))
                .collect()
        })
        .collect();
    let mut people: Vec<String> = credited
        .iter()
        .flatten()
        .map(|a| a.to_ascii_lowercase())
        .collect();
    people.sort();
    people.dedup();

    let mut body = format!(
        "**Promoting `{source}` \u{2192} `{target}`{}**",
        env.map(|e| format!(" ({e})")).unwrap_or_default()
    );
    body.push_str(&format!(
        " \u{b7} {}",
        plural(summary.features.len(), "feature")
    ));
    if !people.is_empty() {
        body.push_str(&format!(" \u{b7} {}", plural(people.len(), "contributor")));
    }
    body.push_str("\n\n");
    let overview = clean(&summary.overview);
    if overview.is_empty() {
        let n = summary.features.len();
        body.push_str(&format!(
            "This release brings {} to {}: {}.\n\n",
            plural(n, "change"),
            env.unwrap_or(&target),
            clean(&summary.title_brief)
        ));
    } else {
        body.push_str(&format!("{overview}\n\n"));
    }

    let mut mentioned: Vec<&str> = Vec::new();
    let mut shown_plain: Vec<String> = Vec::new();
    for (f, names) in summary.features.iter().zip(&credited) {
        // Blank lines between the parts keep each one its own paragraph in any Markdown renderer.
        body.push_str(&format!(
            "### {}\n{}\n\n",
            clean(&f.name),
            clean(&f.summary)
        ));
        let highlights: Vec<String> = f
            .highlights
            .iter()
            .map(|h| clean(h))
            .filter(|h| !h.is_empty())
            .take(MAX_HIGHLIGHTS)
            .collect();
        for h in &highlights {
            body.push_str(&format!("- {h}\n"));
        }
        if !highlights.is_empty() {
            body.push('\n');
        }
        let by: Vec<String> = names
            .iter()
            .map(|n| {
                match resolved
                    .mentions
                    .iter()
                    .find(|(k, _)| *k == n.to_ascii_lowercase())
                {
                    Some((_, user)) => {
                        mentioned.push(user);
                        format!("@{user}")
                    }
                    None => {
                        shown_plain.push(n.to_ascii_lowercase());
                        clean(n)
                    }
                }
            })
            .collect();
        if !by.is_empty() {
            body.push_str(&format!("_By {}_\n\n", by.join(", ")));
        }
    }

    // Anyone already named in a feature is not tagged again at the end.
    let tags: Vec<String> = resolved
        .tags
        .iter()
        .filter(|t| !mentioned.contains(&t.as_str()))
        .map(|t| format!("@{t}"))
        .collect();
    let plain: Vec<String> = resolved
        .plain
        .iter()
        .filter(|p| !shown_plain.contains(&p.replace('@', "").trim().to_ascii_lowercase()))
        .map(|p| clean(&p.replace('@', "")))
        .collect();
    if !tags.is_empty() {
        body.push_str(&format!("cc {}", tags.join(" ")));
        if !plain.is_empty() {
            body.push_str(&format!(" (also: {})", plain.join(", ")));
        }
        body.push('\n');
    } else if !plain.is_empty() {
        body.push_str(&format!("Also contributed: {}\n", plain.join(", ")));
    }
    (title, body.trim_end().to_string() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_mr::summarize::Feature;

    fn feature(name: &str, summary: &str, highlights: &[&str], authors: &[&str]) -> Feature {
        Feature {
            name: name.into(),
            summary: summary.into(),
            highlights: highlights.iter().map(|h| h.to_string()).collect(),
            authors: authors.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn summary() -> Summary {
        Summary {
            title_brief: "order export, faster search".into(),
            overview: "Customers can export orders and search is much faster.".into(),
            features: vec![
                feature(
                    "Order export",
                    "Customers can download their orders as CSV.",
                    &[
                        "Export button on the orders page",
                        "Large exports run in the background",
                    ],
                    &["Ann Lee"],
                ),
                feature(
                    "Faster search",
                    "Search results load in under a second.",
                    &[],
                    &["Bo", "Cy"],
                ),
            ],
        }
    }

    fn resolved() -> Resolved {
        Resolved {
            tags: vec!["ann".into(), "bo".into()],
            plain: vec!["Cy".into()],
            mentions: vec![("ann lee".into(), "ann".into()), ("bo".into(), "bo".into())],
        }
    }

    const PROD: (&str, &str) = ("staging", "master");

    #[test]
    fn title_lists_only_the_leading_items_and_counts_the_rest() {
        let mut s = summary();
        s.title_brief = "order export, faster search, billing fixes, dependency updates, audit log, onboarding checklist".into();
        let (t, d) = render(&s, &resolved(), PROD);
        assert!(t.chars().count() <= 72, "{t}");
        assert!(t.starts_with("Release: order export, faster search"), "{t}");
        assert!(t.contains(" more"), "{t}");
        let kept = t.matches(", ").count() + 1;
        let shown_more: usize = t
            .rsplit_once('+')
            .and_then(|(_, n)| n.split_whitespace().next()?.parse().ok())
            .unwrap();
        assert_eq!(
            kept + shown_more,
            6,
            "every item is either listed or counted: {t}"
        );
        assert!(
            d.contains("Order export"),
            "the rest lives in the description"
        );
        let (short, _) = render(&summary(), &resolved(), PROD);
        assert_eq!(short, "Release: order export, faster search");
        let mut one = summary();
        one.title_brief = "x".repeat(200);
        let (t, _) = render(&one, &resolved(), PROD);
        assert!(
            t.chars().count() <= 72 && t.ends_with('\u{2026}') && t.starts_with("Release: "),
            "{t}"
        );
    }

    #[test]
    fn the_description_opens_with_where_it_goes_and_a_short_overview() {
        let (_, d) = render(&summary(), &resolved(), PROD);
        let first = d.lines().next().unwrap();
        assert_eq!(first, "**Promoting `staging` \u{2192} `master` (production)** \u{b7} 2 features \u{b7} 3 contributors", "{d}");
        assert!(
            d.contains("\nCustomers can export orders and search is much faster.\n"),
            "{d}"
        );
        let (_, st) = render(&summary(), &resolved(), ("develop", "staging"));
        assert!(
            st.starts_with("**Promoting `develop` \u{2192} `staging` (staging)**"),
            "{st}"
        );
        let (_, other) = render(&summary(), &resolved(), ("a", "b"));
        assert!(
            other.starts_with("**Promoting `a` \u{2192} `b`**"),
            "{other}"
        );
    }

    #[test]
    fn each_feature_is_a_group_with_highlights_and_clickable_authors() {
        let (_, d) = render(&summary(), &resolved(), PROD);
        assert!(
            d.contains("### Order export\nCustomers can download their orders as CSV.\n\n- Export button on the orders page\n- Large exports run in the background\n\n_By @ann_\n\n"),
            "{d}"
        );
        assert!(
            d.contains(
                "### Faster search\nSearch results load in under a second.\n\n_By @bo, Cy_\n"
            ),
            "an unmatched author stays a plain name: {d}"
        );
    }

    #[test]
    fn highlights_are_capped_at_four_per_feature() {
        let mut s = summary();
        s.features[0].highlights = (1..=7).map(|i| format!("item {i}")).collect();
        let (_, d) = render(&s, &resolved(), PROD);
        assert!(d.contains("- item 4\n") && !d.contains("item 5"), "{d}");
    }

    #[test]
    fn people_already_mentioned_in_a_feature_are_left_out_of_the_cc_line() {
        let mut r = resolved();
        r.tags.push("dee".into());
        r.plain = vec!["Cy".into(), "Zed".into()];
        let (_, d) = render(&summary(), &r, PROD);
        assert!(
            d.trim_end().ends_with("cc @dee (also: Zed)"),
            "ann and bo are in the features, Cy is shown by name: {d}"
        );
        assert_eq!(d.matches("cc ").count(), 1, "{d}");
    }

    #[test]
    fn nobody_left_to_tag_means_no_cc_line() {
        let mut r = resolved();
        r.plain.clear();
        let (_, d) = render(&summary(), &r, PROD);
        assert!(!d.contains("cc ") && !d.contains("Also contributed"), "{d}");
    }

    #[test]
    fn plain_leftovers_without_tags_are_named_without_a_mention() {
        let r = Resolved {
            plain: vec!["Zed".into()],
            ..Resolved::default()
        };
        let mut s = summary();
        s.features = vec![feature("Search", "Faster.", &[], &["Bo"])];
        let (_, d) = render(&s, &r, PROD);
        assert!(d.trim_end().ends_with("Also contributed: Zed"), "{d}");
        assert_eq!(d.matches('@').count(), 0, "{d}");
    }

    #[test]
    fn description_contains_no_commit_hash_shaped_tokens() {
        let mut s = summary();
        s.features[0].summary = "Fixed in 3f2a9bc1d4 and abcdef1234567.".into();
        let (_, d) = render(&s, &resolved(), PROD);
        assert!(
            !d.contains("3f2a9bc1d4") && !d.contains("abcdef1234567"),
            "{d}"
        );
    }

    #[test]
    fn the_agent_cannot_add_mentions_only_resolved_people_are_linked() {
        let mut s = summary();
        s.features[0].authors = vec!["@mallory".into(), "Ann Lee".into()];
        let (_, d) = render(&s, &resolved(), PROD);
        assert!(!d.contains("@mallory"), "{d}");
        assert!(
            d.contains("_By mallory, @ann_"),
            "an unknown name is plain text: {d}"
        );
        assert_eq!(
            d.matches('@').count(),
            2,
            "only ann (feature 1) and bo (feature 2) are linked: {d}"
        );
    }

    #[test]
    fn agent_text_is_defused_everywhere() {
        let mut s = summary();
        s.overview = "ping @all\n/merge <!-- hidden --> ![x](http://e/i.png)".into();
        s.features[0].summary = "line one\n/merge\n<!-- hidden --> ping @all".into();
        s.features[0].highlights = vec!["see <img src=\"https://t/p.png\"> and [click](http://e.com) fixes #123, !45 and &9 <b>x</b>".into()];
        s.title_brief = "<script>x</script> #77".into();
        let (t, d) = render(&s, &resolved(), PROD);
        for text in [&t, &d] {
            assert!(
                !text.contains("<img")
                    && !text.contains("<script")
                    && !text.contains("<b>")
                    && !text.contains("<!--"),
                "{text}"
            );
            assert!(
                !text.contains("](") && !text.contains("@all") && !text.contains("!["),
                "{text}"
            );
            assert!(
                !text.contains("#123")
                    && !text.contains("!45")
                    && !text.contains("&9")
                    && !text.contains("#77"),
                "{text}"
            );
        }
        assert!(!d.lines().any(|l| l.trim_start().starts_with('/')), "{d}");
    }

    #[test]
    fn bot_authors_are_not_credited_and_do_not_count_as_contributors() {
        let mut s = summary();
        s.features[0].authors = vec!["renovate[bot]".into(), "Ann Lee".into()];
        s.features[1].authors = vec!["dependabot[bot]".into()];
        let (_, d) = render(&s, &resolved(), PROD);
        assert!(!d.contains("[bot]"), "{d}");
        assert!(d.contains("\u{b7} 1 contributor\n"), "{d}");
        assert!(
            d.contains("### Faster search\nSearch results load in under a second.\n\n"),
            "no By line for a bot-only feature: {d}"
        );
    }

    #[test]
    fn a_missing_overview_falls_back_to_a_plain_sentence() {
        let mut s = summary();
        s.overview.clear();
        let (_, d) = render(&s, &resolved(), PROD);
        assert!(
            d.contains("This release brings 2 changes to production: order export, faster search."),
            "{d}"
        );
    }
}

//! The title and description, rendered in code from the agent's structured summary.

use super::authors::Resolved;
use super::collect::Change;
use super::summarize::{Feature, Summary};
use crate::mr_fix::notes::safe_text;

const TITLE_PREFIX: &str = "Release: ";
/// A release title stays short enough to read in a list; the rest goes in the description.
const TITLE_MAX: usize = 72;
/// A bullet cites at most this many merge requests.
const MAX_REFS: usize = 5;

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

/// `(title, description)` for the release MR. Only `resolved` can add mentions, and only
/// merge request numbers read from `changes` can appear as references.
///
/// The description follows the team's usual release MR: a `ccing` line, then numbered theme
/// headings, each with one bold-labelled bullet per feature and the MRs it came from.
pub fn render(summary: &Summary, resolved: &Resolved, changes: &[Change]) -> (String, String) {
    let title = fit_title(&clean_title(&summary.title_brief));
    let mut body = String::new();

    let tags: Vec<String> = resolved.tags.iter().map(|t| format!("@{t}")).collect();
    let plain: Vec<String> = resolved
        .plain
        .iter()
        .map(|p| clean(&p.replace('@', "")))
        .filter(|p| !p.is_empty())
        .collect();
    if !tags.is_empty() {
        body.push_str(&format!("ccing {}", tags.join(", ")));
        if !plain.is_empty() {
            body.push_str(&format!(" (also: {})", plain.join(", ")));
        }
        body.push_str("\n\n");
    } else if !plain.is_empty() {
        body.push_str(&format!("Also contributed: {}\n\n", plain.join(", ")));
    }
    body.push_str("Here is the high-level summary of changes organized by feature area:\n\n");

    // Themes in first-seen order; features without one go last under "Other".
    let theme_of = |f: &Feature| -> String {
        let g = clean(f.group.trim());
        if g.is_empty() {
            "Other".to_string()
        } else {
            g
        }
    };
    let mut themes: Vec<String> = Vec::new();
    for f in &summary.features {
        let t = theme_of(f);
        if t != "Other" && !themes.contains(&t) {
            themes.push(t);
        }
    }
    if summary.features.iter().any(|f| theme_of(f) == "Other") {
        themes.push("Other".into());
    }
    for (n, theme) in themes.iter().enumerate() {
        body.push_str(&format!("### **{}. {theme}**\n\n", n + 1));
        for f in summary.features.iter().filter(|f| theme_of(f) == *theme) {
            let mut refs: Vec<u64> = Vec::new();
            for r in f.refs.iter().filter_map(|r| r.checked_sub(1)) {
                let iid = changes.get(r).and_then(|c| c.mr.as_ref()).map(|m| m.iid);
                if let Some(iid) = iid.filter(|i| *i > 0 && !refs.contains(i)) {
                    refs.push(iid);
                }
            }
            let refs: Vec<String> = refs
                .iter()
                .take(MAX_REFS)
                .map(|i| format!("`#{i}`"))
                .collect();
            let cited = if refs.is_empty() {
                String::new()
            } else {
                format!(" ({})", refs.join(", "))
            };
            body.push_str(&format!(
                "* **{}:** {}{cited}\n",
                clean(&f.name),
                clean(&f.summary)
            ));
        }
        body.push('\n');
    }
    (title, body.trim_end().to_string() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_mr::api::MrInfo;

    fn feature(name: &str, summary: &str, group: &str, refs: &[usize]) -> Feature {
        Feature {
            name: name.into(),
            summary: summary.into(),
            refs: refs.to_vec(),
            group: group.into(),
        }
    }

    fn change(iid: u64) -> Change {
        Change {
            title: format!("change {iid}"),
            author_name: "Ann".into(),
            author_email: "a@x.io".into(),
            mr: (iid > 0).then(|| MrInfo {
                iid,
                title: format!("change {iid}"),
                author_username: "ann".into(),
            }),
        }
    }

    fn changes() -> Vec<Change> {
        vec![change(1351), change(1352), change(0)]
    }

    fn summary() -> Summary {
        Summary {
            title_brief: "order export, faster search".into(),
            features: vec![
                feature(
                    "Order export",
                    "Customers can download their orders as CSV.",
                    "Orders",
                    &[1, 2],
                ),
                feature(
                    "Faster search",
                    "Results load in under a second.",
                    "Orders",
                    &[3],
                ),
                feature("Billing", "Invoices show the right tax.", "Money", &[]),
            ],
        }
    }

    fn resolved() -> Resolved {
        Resolved {
            tags: vec!["ann".into(), "bo".into()],
            plain: vec!["Cy".into()],
            mentions: vec![("ann".into(), "ann".into()), ("bo".into(), "bo".into())],
        }
    }

    #[test]
    fn the_description_follows_the_team_release_format() {
        let (_, d) = render(&summary(), &resolved(), &changes());
        let want = "ccing @ann, @bo (also: Cy)\n\nHere is the high-level summary of changes organized by feature area:\n\n### **1. Orders**\n\n* **Order export:** Customers can download their orders as CSV. (`#1351`, `#1352`)\n* **Faster search:** Results load in under a second.\n\n### **2. Money**\n\n* **Billing:** Invoices show the right tax.\n";
        assert_eq!(d, want);
    }

    #[test]
    fn references_come_from_real_merge_requests_only() {
        let mut s = summary();
        s.features[0].refs = vec![0, 1, 1, 2, 3, 99];
        let (_, d) = render(&s, &resolved(), &changes());
        assert!(d.contains("CSV. (`#1351`, `#1352`)\n"), "{d}");
        assert_eq!(d.matches("`#").count(), 2, "{d}");
        s.features[0].refs = vec![1, 2];
        let many: Vec<Change> = (1..=9).map(change).collect();
        s.features[0].refs = (1..=9).collect();
        let (_, d) = render(&s, &resolved(), &many);
        assert!(
            d.contains("(`#1`, `#2`, `#3`, `#4`, `#5`)"),
            "capped at five: {d}"
        );
    }

    #[test]
    fn features_without_a_theme_go_last_under_other() {
        let mut s = summary();
        s.features[0].group.clear();
        let (_, d) = render(&s, &resolved(), &changes());
        let (a, b) = (d.find("### **1. Orders**"), d.find("### **3. Other**"));
        assert!(a.is_some() && b > a, "{d}");
        assert!(d.find("Order export") > b, "{d}");
    }

    #[test]
    fn nobody_to_tag_means_no_ccing_line() {
        let (_, d) = render(&summary(), &Resolved::default(), &changes());
        assert!(d.starts_with("Here is the high-level summary"), "{d}");
        let r = Resolved {
            plain: vec!["Zed".into()],
            ..Resolved::default()
        };
        let (_, d) = render(&summary(), &r, &changes());
        assert!(d.starts_with("Also contributed: Zed\n\n"), "{d}");
        assert_eq!(d.matches('@').count(), 0, "{d}");
    }

    #[test]
    fn title_lists_only_the_leading_items_and_counts_the_rest() {
        let mut s = summary();
        s.title_brief = "order export, faster search, billing fixes, dependency updates, audit log, onboarding checklist".into();
        let (t, _) = render(&s, &resolved(), &changes());
        assert!(t.chars().count() <= 72, "{t}");
        assert!(t.starts_with("Release: order export, faster search"), "{t}");
        assert!(t.contains(" more"), "{t}");
        let (short, _) = render(&summary(), &resolved(), &changes());
        assert_eq!(short, "Release: order export, faster search");
        let mut one = summary();
        one.title_brief = "x".repeat(200);
        let (t, _) = render(&one, &resolved(), &changes());
        assert!(
            t.chars().count() <= 72 && t.ends_with('\u{2026}') && t.starts_with("Release: "),
            "{t}"
        );
    }

    #[test]
    fn description_contains_no_commit_hash_shaped_tokens() {
        let mut s = summary();
        s.features[0].summary = "Fixed in 3f2a9bc1d4 and abcdef1234567.".into();
        let (_, d) = render(&s, &resolved(), &changes());
        assert!(
            !d.contains("3f2a9bc1d4") && !d.contains("abcdef1234567"),
            "{d}"
        );
    }

    #[test]
    fn agent_text_is_defused_everywhere() {
        let mut s = summary();
        s.features[0].summary = "line one\n/merge\n<!-- hidden --> ping @all see <img src=\"https://t/p.png\"> and [click](http://e.com) fixes #123, !45 and &9 <b>x</b> ![x](http://e/i.png)".into();
        s.features[0].name = "ping @all".into();
        s.features[0].group = "<b>Theme</b> @all".into();
        s.title_brief = "<script>x</script> #77".into();
        let (t, d) = render(&s, &resolved(), &changes());
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
        assert!(d.starts_with("ccing @ann, @bo (also: Cy)\n"), "{d}");
    }
}

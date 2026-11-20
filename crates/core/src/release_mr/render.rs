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
/// A feature credits at most this many authors.
const MAX_AUTHORS: usize = 4;

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

/// The opening lines a release description can start with. The `release-mr` skill in the
/// encore-skills repo (`skills/release-mr/SKILL.md`) keeps the same list and format rules
/// for hand-written release MRs; LoopX does not read it, so change both together.
/// The "focuses on" line is built in `opening_line` and only joins when the title brief
/// names exactly three features.
const OPENINGS: [&str; 7] = [
    "Here's what's shipping in this release, grouped by feature area:",
    "This release rounds up a handful of improvements, sorted by area:",
    "Quick tour of what's new, by feature area:",
    "A few things got better in this release. Here's the rundown by area:",
    "Fresh from the branch: this release's changes, grouped by feature area.",
    "Short version of what's changed, organized by area so you can jump to what matters to you:",
    "What you'll notice in this release, area by area:",
];

/// FNV-1a: a hash that never changes between Rust versions, so the same release always
/// gets the same opening line (a previewed text and its re-render agree).
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// One opening line, picked by the content of the release. The "focuses on a, b, and c"
/// variant joins the pool only when the title brief names three features.
fn opening_line(summary: &Summary) -> String {
    let brief = clean_title(&summary.title_brief);
    let themes: Vec<&str> = brief
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let mut pool: Vec<String> = OPENINGS.iter().map(|o| o.to_string()).collect();
    if let [a, b, c] = themes[..] {
        pool.push(format!(
            "This release focuses on {a}, {b}, and {c}. The details, by area:"
        ));
    }
    let seed: String = std::iter::once(brief.as_str())
        .chain(summary.features.iter().map(|f| f.name.as_str()))
        .collect::<Vec<_>>()
        .join("|");
    pool.swap_remove((fnv(&seed) % pool.len() as u64) as usize)
}

/// `[#N](url)` for a merge request whose page URL is a plain http(s) link; the bare
/// number in code formatting otherwise, so an odd URL can never break the Markdown.
fn link(iid: u64, url: &str) -> String {
    let plain = (url.starts_with("https://") || url.starts_with("http://"))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || "()[]<>`".contains(c));
    if plain {
        format!("[#{iid}]({url})")
    } else {
        format!("`#{iid}`")
    }
}

/// Who built a feature: the authors of the changes it cites, as `@mentions` for people
/// matched to one project member and plain names for the rest. Only people the resolver
/// already vetted can appear, so agent text can never add a mention.
fn credit(feature: &Feature, resolved: &Resolved, changes: &[Change]) -> String {
    let mut out: Vec<String> = Vec::new();
    for c in feature
        .refs
        .iter()
        .filter_map(|r| r.checked_sub(1))
        .filter_map(|r| changes.get(r))
    {
        let by_mr =
            c.mr.as_ref()
                .map(|m| m.author_username.as_str())
                .and_then(|u| {
                    resolved
                        .tags
                        .iter()
                        .find(|t| !u.is_empty() && t.eq_ignore_ascii_case(u))
                });
        let by_name = || {
            let n = c.author_name.to_ascii_lowercase();
            resolved
                .mentions
                .iter()
                .find(|(name, _)| *name == n)
                .map(|(_, user)| user)
        };
        let token = match by_mr.or_else(by_name) {
            Some(user) => format!("@{user}"),
            None if resolved.plain.contains(&c.author_name) => {
                clean(&c.author_name.replace('@', ""))
            }
            None => continue,
        };
        if !token.is_empty() && !out.contains(&token) {
            out.push(token);
        }
    }
    out.truncate(MAX_AUTHORS);
    out.join(" ")
}

/// `(title, description)` for the release MR. Only `resolved` can add mentions, and only
/// merge request numbers read from `changes` can appear as references.
///
/// The description opens with one casual line, then numbered theme headings, each with one
/// bold-labelled bullet per feature, linking the MRs it came from and crediting their authors
/// (`· @user`); there is no `ccing` line, each feature's author is the one tagged.
pub fn render(summary: &Summary, resolved: &Resolved, changes: &[Change]) -> (String, String) {
    let title = fit_title(&clean_title(&summary.title_brief));
    let mut body = String::new();

    body.push_str(&opening_line(summary));
    body.push_str("\n\n");

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
            let mut refs: Vec<(u64, &str)> = Vec::new();
            for r in f.refs.iter().filter_map(|r| r.checked_sub(1)) {
                let mr = changes.get(r).and_then(|c| c.mr.as_ref());
                if let Some(m) = mr.filter(|m| m.iid > 0 && !refs.iter().any(|(i, _)| *i == m.iid))
                {
                    refs.push((m.iid, m.web_url.as_str()));
                }
            }
            let refs: Vec<String> = refs
                .iter()
                .take(MAX_REFS)
                .map(|(iid, url)| link(*iid, url))
                .collect();
            let cited = if refs.is_empty() {
                String::new()
            } else {
                format!(" ({})", refs.join(", "))
            };
            let credit = credit(f, resolved, changes);
            let by = if credit.is_empty() {
                String::new()
            } else {
                format!(" \u{b7} {credit}")
            };
            body.push_str(&format!(
                "* **{}:** {}{cited}{by}\n",
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
                web_url: format!("https://g/app/-/merge_requests/{iid}"),
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
    fn the_description_follows_the_release_format() {
        let (_, d) = render(&summary(), &resolved(), &changes());
        let open = opening_line(&summary());
        let want = format!("{open}\n\n### **1. Orders**\n\n* **Order export:** Customers can download their orders as CSV. ([#1351](https://g/app/-/merge_requests/1351), [#1352](https://g/app/-/merge_requests/1352)) \u{b7} @ann\n* **Faster search:** Results load in under a second. \u{b7} @ann\n\n### **2. Money**\n\n* **Billing:** Invoices show the right tax.\n");
        assert_eq!(d, want);
        assert!(
            !d.contains("ccing") && !d.contains("Also contributed"),
            "{d}"
        );
    }

    #[test]
    fn the_opening_line_is_stable_per_release_and_varies_across_releases() {
        let s = summary();
        assert_eq!(opening_line(&s), opening_line(&s));
        let mut seen = std::collections::HashSet::new();
        for n in 0..40 {
            let mut other = summary();
            other.features[0].name = format!("Feature {n}");
            seen.insert(opening_line(&other));
        }
        assert!(seen.len() >= 4, "{seen:?}");
        assert!(
            seen.iter().all(|l| l.ends_with(':') || l.ends_with('.')),
            "{seen:?}"
        );
    }

    #[test]
    fn the_focus_line_joins_the_pool_only_for_three_featured_items() {
        let mut s = summary();
        s.title_brief = "order export, faster search".into();
        for n in 0..60 {
            s.features[0].name = format!("F{n}");
            assert!(!opening_line(&s).contains("focuses on"));
        }
        s.title_brief = "order export, faster search, billing fixes".into();
        let focus = (0..200).any(|n| {
            s.features[0].name = format!("F{n}");
            opening_line(&s)
                == "This release focuses on order export, faster search, and billing fixes. The details, by area:"
        });
        assert!(focus, "a three-item brief can produce the focus line");
    }

    #[test]
    fn each_feature_credits_its_authors_not_a_ccing_line() {
        let mut ch = changes();
        ch[1].mr.as_mut().unwrap().author_username = "bo".into();
        ch[1].author_name = "Bo".into();
        ch[2].author_name = "Cy".into();
        let r = Resolved {
            tags: vec!["ann".into(), "bo".into()],
            plain: vec!["Cy".into()],
            mentions: vec![("ann".into(), "ann".into()), ("bo".into(), "bo".into())],
        };
        let mut s = summary();
        s.features[1].refs = vec![3];
        let (_, d) = render(&s, &r, &ch);
        assert!(
            d.contains("CSV. ([#1351]") && d.contains(") \u{b7} @ann @bo\n"),
            "{d}"
        );
        assert!(
            d.contains("second. \u{b7} Cy\n"),
            "a plain name stays unmentioned: {d}"
        );
        assert_eq!(d.matches("@ann").count(), 1, "{d}");
    }

    #[test]
    fn an_unusable_page_url_falls_back_to_the_number() {
        assert_eq!(
            link(7, "https://g/a/-/merge_requests/7"),
            "[#7](https://g/a/-/merge_requests/7)"
        );
        for bad in [
            "",
            "javascript:alert(1)",
            "https://g/a b",
            "https://g/a)x",
            "https://g/<x>",
        ] {
            assert_eq!(link(7, bad), "`#7`", "{bad}");
        }
    }

    /// The worked example from the encore-skills `release-mr` skill
    /// (`examples/loopx-release.md`), rendered by this code. Authors are made up.
    #[test]
    fn a_loopx_release_renders_like_the_skill_example() {
        let url = "https://gitlab.example.com/acme/loop-x/-/merge_requests";
        let mr = |iid: u64, by: &str| Change {
            title: format!("change {iid}"),
            author_name: by.into(),
            author_email: String::new(),
            mr: Some(MrInfo {
                iid,
                title: format!("change {iid}"),
                author_username: by.into(),
                web_url: format!("{url}/{iid}"),
            }),
        };
        let changes = vec![
            mr(398, "priya"),
            mr(405, "jonas"),
            mr(409, "jonas"),
            mr(412, "mira"),
            mr(418, "tomas"),
            mr(421, "priya"),
        ];
        let s = Summary {
            title_brief: "slack agent, chat folders, project memory graph".into(),
            features: vec![
                feature("Slack Agent", "Mention LoopX in a channel, thread or DM and it starts a loop or an ad-hoc task on your Mac, then replies in the thread with progress and the result.", "Slack & Agents", &[4, 5]),
                feature("Cursor Agent for Creator", "Generate, Showcase and Retry now run on Cursor Agent when it's your selected AI CLI, and the job log follows its output.", "Slack & Agents", &[6]),
                feature("Chat History Folders", "File chats into folders by dragging them over or using the folder menu on each chat, and rename or delete a folder without losing its chats.", "Chat", &[2, 3]),
                feature("Project Memory Graph", "The Memory page now has a graph of projects, linked only where there is evidence, with zoom, filters and a one-page summary for each node.", "Insights & Memory", &[1]),
            ],
        };
        let r = Resolved {
            tags: ["mira", "tomas", "priya", "jonas"]
                .map(String::from)
                .to_vec(),
            ..Resolved::default()
        };
        let (title, d) = render(&s, &r, &changes);
        assert_eq!(
            title,
            "Release: slack agent, chat folders, project memory graph"
        );
        let (open, body) = d.split_once("\n\n").unwrap();
        assert_eq!(open, opening_line(&s));
        let want = format!("### **1. Slack & Agents**\n\n* **Slack Agent:** Mention LoopX in a channel, thread or DM and it starts a loop or an ad-hoc task on your Mac, then replies in the thread with progress and the result. ([#412]({url}/412), [#418]({url}/418)) \u{b7} @mira @tomas\n* **Cursor Agent for Creator:** Generate, Showcase and Retry now run on Cursor Agent when it's your selected AI CLI, and the job log follows its output. ([#421]({url}/421)) \u{b7} @priya\n\n### **2. Chat**\n\n* **Chat History Folders:** File chats into folders by dragging them over or using the folder menu on each chat, and rename or delete a folder without losing its chats. ([#405]({url}/405), [#409]({url}/409)) \u{b7} @jonas\n\n### **3. Insights & Memory**\n\n* **Project Memory Graph:** The Memory page now has a graph of projects, linked only where there is evidence, with zoom, filters and a one-page summary for each node. ([#398]({url}/398)) \u{b7} @priya\n");
        assert_eq!(body, want);
    }

    #[test]
    fn references_come_from_real_merge_requests_only() {
        let mut s = summary();
        s.features[0].refs = vec![0, 1, 1, 2, 3, 99];
        let (_, d) = render(&s, &resolved(), &changes());
        assert!(
            d.contains("[#1351](https://g/app/-/merge_requests/1351), [#1352]("),
            "{d}"
        );
        assert_eq!(d.matches("](https").count(), 2, "{d}");
        s.features[0].refs = vec![1, 2];
        let many: Vec<Change> = (1..=9).map(change).collect();
        s.features[0].refs = (1..=9).collect();
        let (_, d) = render(&s, &resolved(), &many);
        assert!(
            d.contains("[#1](https://g/app/-/merge_requests/1), [#2](https://g/app/-/merge_requests/2), [#3](https://g/app/-/merge_requests/3), [#4](https://g/app/-/merge_requests/4), [#5](https://g/app/-/merge_requests/5))"),
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
    fn nobody_to_tag_means_no_credit_and_no_mentions() {
        let (_, d) = render(&summary(), &Resolved::default(), &changes());
        assert!(d.starts_with(&opening_line(&summary())), "{d}");
        assert_eq!(d.matches('@').count(), 0, "{d}");
        assert!(!d.contains('\u{b7}'), "{d}");
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
                !text.contains("](http://e") && !text.contains("@all") && !text.contains("!["),
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
        assert!(!d.contains("ccing"), "{d}");
    }
}

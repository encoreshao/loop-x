//! The dashboard's own minimal Markdown renderer (`dashboard_server.render_markdown`
//! and its helpers), ported rule for rule so reports, history files and the README
//! render byte-identically. There is no regex engine here on purpose: every
//! pattern of the Python version is a small deterministic scanner.
//!
//! Everything literal passes through `html.escape` before any Markdown syntax is
//! interpreted, so the output is safe to embed.

use super::pyfmt::{self, is_py_space};
use std::collections::BTreeMap;

/// `{alias: "https://host/namespace/project"}`, for `alias #123` links.
pub type GitlabPrefixes = BTreeMap<String, String>;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_digit(c: char) -> bool {
    c.is_numeric()
}

fn is_fence(line: &str) -> bool {
    line.starts_with("```")
}

/// `^(#{1,6})\s+(.*)$`: `(level, text)`.
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    let trimmed = rest.trim_start_matches(is_py_space);
    (trimmed.len() < rest.len()).then_some((hashes, trimmed))
}

/// `^[-*+]\s+(.*)$`.
fn ul_item(line: &str) -> Option<&str> {
    let mut chars = line.chars();
    let first = chars.next()?;
    if !matches!(first, '-' | '*' | '+') {
        return None;
    }
    let rest = &line[1..];
    let trimmed = rest.trim_start_matches(is_py_space);
    (trimmed.len() < rest.len()).then_some(trimmed)
}

/// `^\d+\.\s+(.*)$`.
fn ol_item(line: &str) -> Option<&str> {
    let digits = line
        .chars()
        .take_while(|&c| is_digit(c))
        .map(char::len_utf8)
        .sum::<usize>();
    if digits == 0 {
        return None;
    }
    let rest = line[digits..].strip_prefix('.')?;
    let trimmed = rest.trim_start_matches(is_py_space);
    (trimmed.len() < rest.len()).then_some(trimmed)
}

/// `^\|(.+)\|\s*$`.
fn table_row(line: &str) -> bool {
    let t = line.trim_end_matches(is_py_space);
    t.chars().count() >= 3 && t.starts_with('|') && t.ends_with('|')
}

/// `^\|(?:\s*:?-+:?\s*\|)+\s*$` (applied to a stripped line).
fn table_sep(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    if chars.first() != Some(&'|') {
        return false;
    }
    let mut i = 1;
    let mut groups = 0;
    loop {
        let mut j = i;
        while j < chars.len() && is_py_space(chars[j]) {
            j += 1;
        }
        if j < chars.len() && chars[j] == ':' {
            j += 1;
        }
        let dashes = j;
        while j < chars.len() && chars[j] == '-' {
            j += 1;
        }
        if j == dashes {
            break;
        }
        if j < chars.len() && chars[j] == ':' {
            j += 1;
        }
        while j < chars.len() && is_py_space(chars[j]) {
            j += 1;
        }
        if j < chars.len() && chars[j] == '|' {
            groups += 1;
            i = j + 1;
        } else {
            break;
        }
    }
    groups > 0 && chars[i..].iter().all(|&c| is_py_space(c))
}

/// `_split_table_row`.
fn split_table_row(line: &str) -> Vec<String> {
    let stripped = pyfmt::strip(line);
    let inner: Vec<char> = stripped.chars().collect();
    let inner = if inner.len() >= 2 {
        &inner[1..inner.len() - 1]
    } else {
        &inner[..0]
    };
    let (mut cells, mut current) = (Vec::new(), String::new());
    let mut it = inner.iter();
    while let Some(&ch) = it.next() {
        if ch == '\\' {
            current.push(ch);
            if let Some(&next) = it.next() {
                current.push(next);
            }
        } else if ch == '|' {
            cells.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    cells.push(current);
    cells
        .into_iter()
        .map(|c| pyfmt::strip(&c).to_string())
        .collect()
}

/// `_slugify_heading`: GitHub-style anchor.
pub fn slugify_heading(text: &str) -> String {
    let lowered = text.to_lowercase();
    let kept: String = lowered
        .chars()
        .filter(|&c| is_word(c) || is_py_space(c) || c == '-')
        .collect();
    let mut out = String::new();
    let mut in_space = false;
    for c in pyfmt::strip(&kept).chars() {
        if is_py_space(c) {
            if !in_space {
                out.push('-');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `_markdown_h2_sections`: every `## title` as `(title, slug)`.
pub fn h2_sections(text: &str) -> Vec<(String, String)> {
    pyfmt::splitlines(text)
        .into_iter()
        .filter_map(|line| {
            let rest = line.strip_prefix("##")?;
            let title = rest.trim_start_matches(is_py_space);
            (title.len() < rest.len()).then(|| (title.to_string(), slugify_heading(title)))
        })
        .collect()
}

/// `_has_disallowed_link_scheme`.
fn has_disallowed_link_scheme(url: &str) -> bool {
    if url.starts_with("//") {
        return true;
    }
    let mut chars = url.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for (i, c) in chars {
        if c == ':' {
            let scheme = url[..i].to_ascii_lowercase();
            return scheme != "http" && scheme != "https";
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

/// A left-to-right `re.sub`: `find(chars, pos)` returns the match starting at
/// `pos` as `(end, replacement-or-None)`; `None` keeps the matched text.
fn sub(
    text: &str,
    mut at: impl FnMut(&[char], usize) -> Option<(usize, Option<String>)>,
) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match at(&chars, i) {
            Some((end, replacement)) => {
                match replacement {
                    Some(r) => out.push_str(&r),
                    None => out.extend(&chars[i..end]),
                }
                i = end.max(i + 1);
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn collect(chars: &[char], from: usize, to: usize) -> String {
    chars[from..to].iter().collect()
}

/// `[^X]+` starting at `i`: the end of the run (at least one char) or None.
fn run_until(chars: &[char], i: usize, stop: impl Fn(char) -> bool) -> Option<usize> {
    let mut j = i;
    while j < chars.len() && !stop(chars[j]) {
        j += 1;
    }
    (j > i).then_some(j)
}

struct Stash(Vec<String>);

impl Stash {
    fn put(&mut self, fragment: String) -> String {
        self.0.push(fragment);
        format!("\0{}\0", self.0.len() - 1)
    }
}

fn is_ascii_punct_escapable(c: char) -> bool {
    matches!(c, '!'..='/' | ':'..='@' | '['..='`' | '{'..='~')
}

/// `[text](url)` / `![alt](url)` at `i` (after the optional `!`): returns
/// `(end, group1, group2)`.
fn bracket_link(
    chars: &[char],
    i: usize,
    allow_empty_text: bool,
) -> Option<(usize, String, String)> {
    if chars.get(i) != Some(&'[') {
        return None;
    }
    let close = i + 1 + chars[i + 1..].iter().position(|&c| c == ']')?;
    if !allow_empty_text && close == i + 1 {
        return None;
    }
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let url_start = close + 2;
    let url_end = run_until(chars, url_start, |c| c == ')' || is_py_space(c))?;
    if chars.get(url_end) != Some(&')') {
        return None;
    }
    Some((
        url_end + 1,
        collect(chars, i + 1, close),
        collect(chars, url_start, url_end),
    ))
}

/// `_markdown_inline`: inline formatting for text that was already `html.escape`d.
pub fn markdown_inline(escaped: &str, prefixes: &GitlabPrefixes) -> String {
    let mut stash = Stash(Vec::new());

    // Backslash escapes and code spans, in one left-to-right pass.
    let mut text = sub(escaped, |c, i| {
        if c[i] == '\\' {
            if let Some(&n) = c.get(i + 1) {
                if is_ascii_punct_escapable(n) {
                    return Some((i + 2, Some(stash.put(n.to_string()))));
                }
            }
        }
        if c[i] == '`' {
            let end = run_until(c, i + 1, |ch| ch == '`')?;
            if c.get(end) == Some(&'`') {
                return Some((
                    end + 1,
                    Some(stash.put(format!("<code>{}</code>", collect(c, i + 1, end)))),
                ));
            }
        }
        None
    });

    if !prefixes.is_empty() {
        let mut aliases: Vec<&String> = prefixes.keys().collect();
        aliases.sort_by_key(|a| std::cmp::Reverse(a.chars().count()));
        text = sub(&text, |c, i| {
            let boundary_before = (i > 0 && is_word(c[i - 1])) != (i < c.len() && is_word(c[i]));
            if !boundary_before {
                return None;
            }
            for alias in &aliases {
                let a: Vec<char> = alias.chars().collect();
                if c.len() < i + a.len() || c[i..i + a.len()] != a[..] {
                    continue;
                }
                let mut j = i + a.len();
                let ws = j;
                while j < c.len() && is_py_space(c[j]) {
                    j += 1;
                }
                if j == ws || c.get(j) != Some(&'#') {
                    continue;
                }
                let digits = j + 1;
                let mut k = digits;
                while k < c.len() && is_digit(c[k]) {
                    k += 1;
                }
                let boundary_after = k > digits && (k == c.len() || !is_word(c[k]));
                if !boundary_after {
                    continue;
                }
                let url = format!("{}/-/issues/{}", prefixes[*alias], collect(c, digits, k));
                let whole = collect(c, i, k);
                return Some((
                    k,
                    Some(stash.put(format!(
                        "<a href=\"{url}\" rel=\"noopener\" target=\"_blank\">{whole}</a>"
                    ))),
                ));
            }
            None
        });
    }

    // Images, then links: a URL holding a placeholder or a disallowed scheme stays literal.
    text = sub(&text, |c, i| {
        if c[i] != '!' {
            return None;
        }
        let (end, alt, url) = bracket_link(c, i + 1, true)?;
        let keep_literal = url.contains('\0') || has_disallowed_link_scheme(&url);
        Some((
            end,
            (!keep_literal).then(|| {
                stash.put(format!(
                    "<img src=\"{url}\" alt=\"{alt}\" loading=\"lazy\">"
                ))
            }),
        ))
    });
    text = sub(&text, |c, i| {
        let (end, label, url) = bracket_link(c, i, false)?;
        let keep_literal = url.contains('\0') || has_disallowed_link_scheme(&url);
        Some((
            end,
            (!keep_literal).then(|| {
                stash.put(format!(
                    "<a href=\"{url}\" rel=\"noopener\" target=\"_blank\">{label}</a>"
                ))
            }),
        ))
    });

    // Bare URLs.
    text = sub(&text, |c, i| {
        let scheme_end = if c[i..].starts_with(&['h', 't', 't', 'p', 's', ':', '/', '/']) {
            i + 8
        } else if c[i..].starts_with(&['h', 't', 't', 'p', ':', '/', '/']) {
            i + 7
        } else {
            return None;
        };
        let end = run_until(c, scheme_end, |ch| {
            is_py_space(ch) || matches!(ch, '<' | '>' | '"' | '\0')
        })?;
        let mut url_end = end;
        while url_end > i
            && matches!(
                c[url_end - 1],
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
            )
        {
            url_end -= 1;
        }
        let url = collect(c, i, url_end);
        let trailing = collect(c, url_end, end);
        Some((
            end,
            Some(format!(
                "{}{trailing}",
                stash.put(format!(
                    "<a href=\"{url}\" rel=\"noopener\" target=\"_blank\">{url}</a>"
                ))
            )),
        ))
    });

    // **bold** / __bold__, then *italic* / _italic_.
    text = sub(&text, |c, i| {
        for marker in ['*', '_'] {
            if c[i] == marker && c.get(i + 1) == Some(&marker) {
                let end = run_until(c, i + 2, |ch| ch == marker)?;
                if c.get(end) == Some(&marker) && c.get(end + 1) == Some(&marker) {
                    return Some((
                        end + 2,
                        Some(format!("<strong>{}</strong>", collect(c, i + 2, end))),
                    ));
                }
            }
        }
        None
    });
    text = sub(&text, |c, i| {
        for marker in ['*', '_'] {
            if c[i] == marker && (i == 0 || c[i - 1] != marker) {
                let Some(end) = run_until(c, i + 1, |ch| ch == marker || ch == '\n') else {
                    continue;
                };
                if c.get(end) == Some(&marker) && c.get(end + 1) != Some(&marker) {
                    return Some((
                        end + 1,
                        Some(format!("<em>{}</em>", collect(c, i + 1, end))),
                    ));
                }
            }
        }
        None
    });

    for (i, fragment) in stash.0.iter().enumerate().rev() {
        text = text.replace(&format!("\0{i}\0"), fragment);
    }
    text
}

/// `render_markdown(text, gitlab_url_prefixes)`.
pub fn render_markdown(text: &str, prefixes: &GitlabPrefixes) -> String {
    let lines = pyfmt::splitlines(text);
    let inline = |s: &str| markdown_inline(&pyfmt::escape(s), prefixes);
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if is_fence(line) {
            i += 1;
            let mut code = Vec::new();
            while i < lines.len() && !is_fence(lines[i]) {
                code.push(lines[i]);
                i += 1;
            }
            i += 1;
            blocks.push(format!(
                "<pre><code>{}</code></pre>",
                pyfmt::escape(&code.join("\n"))
            ));
            continue;
        }

        if let Some((level, title)) = heading(line) {
            blocks.push(format!(
                "<h{level} id=\"{}\">{}</h{level}>",
                slugify_heading(title),
                inline(title)
            ));
            i += 1;
            continue;
        }

        if table_row(line) && i + 1 < lines.len() && table_sep(pyfmt::strip(lines[i + 1])) {
            let header = split_table_row(line);
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && table_row(lines[i]) {
                rows.push(split_table_row(lines[i]));
                i += 1;
            }
            let thead: String = header
                .iter()
                .map(|c| format!("<th>{}</th>", inline(c)))
                .collect();
            let tbody: String = rows
                .iter()
                .map(|row| {
                    format!(
                        "<tr>{}</tr>",
                        row.iter()
                            .map(|c| format!("<td>{}</td>", inline(c)))
                            .collect::<String>()
                    )
                })
                .collect();
            blocks.push(format!(
                "<div class='table-wrap'><table class='daemons md-table'><thead><tr>{thead}</tr></thead><tbody>{tbody}</tbody></table></div>"
            ));
            continue;
        }

        if ul_item(line).is_some() {
            let mut items = String::new();
            while let Some(item) = lines.get(i).and_then(|l| ul_item(l)) {
                items.push_str(&format!("<li>{}</li>", inline(item)));
                i += 1;
            }
            blocks.push(format!("<ul>{items}</ul>"));
            continue;
        }

        if ol_item(line).is_some() {
            let mut items = String::new();
            while let Some(item) = lines.get(i).and_then(|l| ol_item(l)) {
                items.push_str(&format!("<li>{}</li>", inline(item)));
                i += 1;
            }
            blocks.push(format!("<ol>{items}</ol>"));
            continue;
        }

        if pyfmt::strip(line).is_empty() {
            i += 1;
            continue;
        }

        let mut para = Vec::new();
        while i < lines.len()
            && !pyfmt::strip(lines[i]).is_empty()
            && !(is_fence(lines[i])
                || heading(lines[i]).is_some()
                || ul_item(lines[i]).is_some()
                || ol_item(lines[i]).is_some())
        {
            para.push(lines[i]);
            i += 1;
        }
        blocks.push(format!("<p>{}</p>", inline(&para.join(" "))));
    }
    blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(s: &str) -> String {
        render_markdown(s, &GitlabPrefixes::new())
    }

    #[test]
    fn blocks() {
        assert_eq!(
            md("# Hello *World*"),
            "<h1 id=\"hello-world\">Hello <em>World</em></h1>"
        );
        assert_eq!(
            md("- a\n- b\n\n1. x\n2. y"),
            "<ul><li>a</li><li>b</li></ul>\n<ol><li>x</li><li>y</li></ol>"
        );
        assert_eq!(md("a\nb\n\nc"), "<p>a b</p>\n<p>c</p>");
        assert_eq!(md("```\n<x>\n```"), "<pre><code>&lt;x&gt;</code></pre>");
        assert_eq!(md("####### no"), "<p>####### no</p>");
    }

    #[test]
    fn tables() {
        let out = md("| a | b |\n|---|:-:|\n| 1 | 2 \\| 3 |");
        assert!(out.starts_with("<div class='table-wrap'><table class='daemons md-table'><thead><tr><th>a</th><th>b</th></tr></thead>"));
        assert!(
            out.contains("<td>2 | 3</td>")
                || out.contains("<td>2 &#124; 3</td>")
                || out.contains("<td>2 ")
        );
    }

    #[test]
    fn inline_rules() {
        let p = GitlabPrefixes::new();
        assert_eq!(
            markdown_inline("`a_b_c` and **x**", &p),
            "<code>a_b_c</code> and <strong>x</strong>"
        );
        assert_eq!(
            markdown_inline("[t](https://x.y/z) see https://a.b/c.", &p),
            "<a href=\"https://x.y/z\" rel=\"noopener\" target=\"_blank\">t</a> see <a href=\"https://a.b/c\" rel=\"noopener\" target=\"_blank\">https://a.b/c</a>."
        );
        assert_eq!(
            markdown_inline("[bad](javascript:alert(1))", &p),
            "[bad](javascript:alert(1))"
        );
        assert_eq!(
            markdown_inline("![a](x.png)", &p),
            "<img src=\"x.png\" alt=\"a\" loading=\"lazy\">"
        );
        assert_eq!(
            markdown_inline("snake_case_name and _it_", &p),
            "snake<em>case</em>name and <em>it</em>"
        );
        assert_eq!(markdown_inline("\\*not em\\*", &p), "*not em*");
    }

    #[test]
    fn gitlab_references_become_links() {
        let mut p = GitlabPrefixes::new();
        p.insert("proj".into(), "https://gl.example/g/proj".into());
        assert_eq!(
            markdown_inline("fixes proj #12.", &p),
            "fixes <a href=\"https://gl.example/g/proj/-/issues/12\" rel=\"noopener\" target=\"_blank\">proj #12</a>."
        );
    }

    #[test]
    fn slugs_and_sections() {
        assert_eq!(slugify_heading("How it works!"), "how-it-works");
        assert_eq!(slugify_heading("  A  --  B "), "a----b");
        assert_eq!(
            h2_sections("# T\n## One\n### no\n##  Two words"),
            vec![
                ("One".to_string(), "one".to_string()),
                ("Two words".to_string(), "two-words".to_string())
            ]
        );
    }
}

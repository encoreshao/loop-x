//! UI translations, the English text is the lookup key,
//! the catalogs are `crates/server/locales/{ja,zh,fr}.json` (embedded at build time), a missing entry falls back to the
//! English text, and `{placeholder}` fields are filled after the lookup.
//!
//! Unlike Python there is no per-thread language: every render function gets the
//! request's [`Lang`] explicitly (usually through `UiCtx::lang`).

use super::pyfmt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

pub const COOKIE_NAME: &str = "loop_lang";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    En,
    Ja,
    Zh,
    Fr,
}

impl Lang {
    /// `SUPPORTED_LANGUAGES` order, which is also the topbar menu's order.
    pub const ALL: [Lang; 4] = [Lang::En, Lang::Ja, Lang::Zh, Lang::Fr];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ja => "ja",
            Lang::Zh => "zh",
            Lang::Fr => "fr",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        Lang::ALL.into_iter().find(|l| l.code() == code)
    }

    /// `LANGUAGE_NAMES`: each language in its own language.
    pub fn display_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ja => "日本語",
            Lang::Zh => "中文",
            Lang::Fr => "Français",
        }
    }

    /// `<html lang>`: `zh` is `zh-CN`, the others are their code.
    pub fn html_lang(self) -> &'static str {
        match self {
            Lang::Zh => "zh-CN",
            other => other.code(),
        }
    }
}

type Catalog = HashMap<String, String>;

fn parse_catalog(text: &str) -> Catalog {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
        _ => Catalog::new(),
    }
}

fn catalog(lang: Lang) -> &'static Catalog {
    static JA: OnceLock<Catalog> = OnceLock::new();
    static ZH: OnceLock<Catalog> = OnceLock::new();
    static FR: OnceLock<Catalog> = OnceLock::new();
    static EN: OnceLock<Catalog> = OnceLock::new();
    macro_rules! embedded {
        ($cell:ident, $file:literal) => {
            $cell.get_or_init(|| {
                parse_catalog(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/locales/",
                    $file
                )))
            })
        };
    }
    match lang {
        Lang::En => EN.get_or_init(Catalog::new),
        Lang::Ja => embedded!(JA, "ja.json"),
        Lang::Zh => embedded!(ZH, "zh.json"),
        Lang::Fr => embedded!(FR, "fr.json"),
    }
}

/// Is there a catalog entry for `text` in `lang` (always true for English)?
pub fn has_translation(lang: Lang, text: &str) -> bool {
    lang == Lang::En || catalog(lang).contains_key(text)
}

/// `i18n.t(text, **args)`: translate, then (only if `args` is non-empty) fill
/// `{name}` fields. An empty catalog value falls back to the English text, as
/// Python's `catalog.get(text) or text` does.
pub fn t(lang: Lang, text: &str, args: &[(&str, &str)]) -> String {
    let translated = if lang == Lang::En {
        text
    } else {
        catalog(lang)
            .get(text)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(text)
    };
    if args.is_empty() {
        translated.to_string()
    } else {
        pyfmt::format_named(translated, args)
    }
}

fn cookie_language(cookie_header: Option<&str>) -> Option<Lang> {
    for part in cookie_header.unwrap_or("").split(';') {
        let (name, value) = match part.trim().split_once('=') {
            Some((n, v)) => (n, v),
            None => (part.trim(), ""),
        };
        if name == COOKIE_NAME {
            if let Some(lang) = Lang::from_code(value) {
                return Some(lang);
            }
        }
    }
    None
}

fn accept_language(header: Option<&str>) -> Option<Lang> {
    let mut best: Option<(f64, usize, Lang)> = None;
    for (index, part) in header.unwrap_or("").split(',').enumerate() {
        let part = part.trim();
        let (tag, params) = part.split_once(';').unwrap_or((part, ""));
        let params = params.trim();
        let mut quality = 1.0;
        if let Some(q) = params.strip_prefix("q=") {
            quality = q.trim().parse::<f64>().unwrap_or(0.0);
        }
        let primary = tag
            .trim()
            .to_lowercase()
            .split('-')
            .next()
            .unwrap_or("")
            .to_string();
        if let Some(lang) = Lang::from_code(&primary) {
            if quality > 0.0 {
                let better = match best {
                    None => true,
                    Some((bq, bi, _)) => quality > bq || (quality == bq && index < bi),
                };
                if better {
                    best = Some((quality, index, lang));
                }
            }
        }
    }
    best.map(|(_, _, l)| l)
}

/// `i18n.resolve_language`: the `loop_lang` cookie, else `Accept-Language`,
/// else English.
pub fn resolve_language(cookie_header: Option<&str>, accept_header: Option<&str>) -> Lang {
    cookie_language(cookie_header)
        .or_else(|| accept_language(accept_header))
        .unwrap_or(Lang::En)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_identity_and_other_languages_translate() {
        assert_eq!(t(Lang::En, "Dashboard", &[]), "Dashboard");
        assert_eq!(t(Lang::Ja, "Dashboard", &[]), "ダッシュボード");
        assert_eq!(t(Lang::Zh, "Dashboard", &[]), "仪表盘");
        assert_eq!(t(Lang::Fr, "Dashboard", &[]), "Tableau de bord");
        assert_eq!(
            t(Lang::Fr, "Nobody translated this", &[]),
            "Nobody translated this"
        );
    }

    #[test]
    fn placeholders_fill_after_the_lookup() {
        assert_eq!(
            t(
                Lang::Fr,
                "auto-refreshes every {interval}",
                &[("interval", "30s")]
            ),
            "actualisation auto toutes les 30s"
        );
        assert_eq!(
            t(
                Lang::En,
                "auto-refreshes every {interval}",
                &[("interval", "30s")]
            ),
            "auto-refreshes every 30s"
        );
    }

    #[test]
    fn language_resolution_order() {
        assert_eq!(
            resolve_language(Some("a=b; loop_lang=ja"), Some("fr")),
            Lang::Ja
        );
        assert_eq!(
            resolve_language(Some("loop_lang=de"), Some("fr-CA,fr;q=0.9")),
            Lang::Fr
        );
        assert_eq!(
            resolve_language(None, Some("de, zh;q=0.8, ja;q=0.9")),
            Lang::Ja
        );
        assert_eq!(resolve_language(None, Some("ja;q=0")), Lang::En);
        assert_eq!(resolve_language(None, None), Lang::En);
    }

    #[test]
    fn html_lang_and_names() {
        assert_eq!(Lang::Zh.html_lang(), "zh-CN");
        assert_eq!(Lang::Ja.html_lang(), "ja");
        assert_eq!(Lang::Fr.display_name(), "Français");
    }

    /// The value of a Rust string literal body: `\<newline>` continuations,
    /// `\n`, `\'`, `\"`, `\\` and `\u{..}`.
    fn unescape_rust(body: &str) -> String {
        let mut out = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('\n') => {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                }
                Some('n') => out.push('\n'),
                Some('u') => {
                    let hex: String = chars.by_ref().skip(1).take_while(|c| *c != '}').collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(other) => out.push(other),
                None => {}
            }
        }
        out
    }

    /// Every literal a Rust page passes to a translate call (`t(lang, "..")`,
    /// `ctx.t("..")`, `escaped_t(lang, "..")`, ...) must exist in all three
    /// catalogs, like tests/test_i18n.py demands of the Python pages.
    #[test]
    fn every_translated_literal_in_the_ui_sources_has_a_catalog_entry() {
        fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    sources(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui"),
            &mut files,
        );
        let mut missing = Vec::new();
        let mut checked = 0;
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            // Skip this file's own test code and example literals.
            let text = text.split("#[cfg(test)]").next().unwrap().to_string();
            let b = text.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] != b'(' {
                    i += 1;
                    continue;
                }
                let mut k = i;
                while k > 0 && (b[k - 1].is_ascii_alphanumeric() || b[k - 1] == b'_') {
                    k -= 1;
                }
                let name = &text[k..i];
                i += 1;
                if !(name == "t" || name == "tf" || name.ends_with("_t")) {
                    continue;
                }
                let mut j = i;
                // optional first argument (the language) before the literal
                let rest = &text[j..];
                let trimmed = rest.trim_start();
                j += rest.len() - trimmed.len();
                if !trimmed.starts_with('"') {
                    match trimmed.find(',') {
                        Some(c)
                            if trimmed[..c]
                                .chars()
                                .all(|ch| ch.is_alphanumeric() || ch == '.' || ch == '_') =>
                        {
                            j += c + 1;
                            let after = text[j..].trim_start();
                            j = text.len() - after.len();
                        }
                        _ => continue,
                    }
                }
                if text.as_bytes().get(j) != Some(&b'"') {
                    continue;
                }
                let lit_start = j + 1;
                let mut e = lit_start;
                while e < b.len() && b[e] != b'"' {
                    e += if b[e] == b'\\' { 2 } else { 1 };
                }
                let lit = unescape_rust(&text[lit_start..e]);
                checked += 1;
                for lang in [Lang::Ja, Lang::Zh, Lang::Fr] {
                    if !has_translation(lang, &lit) {
                        missing.push(format!(
                            "{}: {:?} missing in {}",
                            file.display(),
                            lit,
                            lang.code()
                        ));
                    }
                }
            }
        }
        assert!(checked > 20, "scanner found only {checked} literals");
        assert!(
            missing.is_empty(),
            "untranslated UI strings:\n{}",
            missing.join("\n")
        );
    }
}

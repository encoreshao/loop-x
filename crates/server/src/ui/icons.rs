//! The dashboard's icon set: one inline SVG sprite, referenced by `icon()`.
//! Ids reuse the Material glyph names so data that carries a name needs no change.

pub const SPRITE: &str = include_str!("../../assets/icons/sprite.svg");
pub const FALLBACK: &str = "circle";

pub fn is_known(name: &str) -> bool {
    SPRITE.contains(&format!("id=\"i-{name}\""))
}

pub fn icon(name: &str) -> String {
    icon_with(name, "")
}

pub fn icon_with(name: &str, extra_class: &str) -> String {
    let name = if is_known(name) { name } else { FALLBACK };
    let class = if extra_class.is_empty() {
        "icon".to_string()
    } else {
        format!("icon {extra_class}")
    };
    format!("<svg class='{class}' aria-hidden='true'><use href='#i-{name}'/></svg>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_material_name_the_ui_used_has_a_symbol() {
        let names = crate::ui::consts::ds("_MATERIAL_SYMBOLS_ICON_NAMES");
        for n in names.split(',') {
            assert!(is_known(n), "sprite lacks {n}");
        }
    }

    #[test]
    fn icon_renders_a_use_reference() {
        assert_eq!(
            icon("add"),
            "<svg class='icon' aria-hidden='true'><use href='#i-add'/></svg>"
        );
    }

    #[test]
    fn unknown_name_falls_back_to_a_visible_glyph() {
        assert_eq!(icon("no_such_icon"), icon(FALLBACK));
    }

    #[test]
    fn extra_class_is_appended() {
        assert!(icon_with("add", "dash-stat-icon").contains("class='icon dash-stat-icon'"));
    }

    #[test]
    fn stylesheet_defines_the_icon_class() {
        assert!(crate::ui::consts::ds("_STYLE").contains(".icon {"));
    }

    /// Every glyph name written as a literal in a Rust source, template or
    /// constants file must resolve, since a dangling `use` renders nothing.
    #[test]
    fn every_hard_coded_glyph_name_resolves() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("rs" | "tpl" | "txt" | "json")
                ) && p.file_name().is_some_and(|n| n != "icons.rs")
                {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        walk(&root.join("assets"), &mut files);
        let mut checked = 0;
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            for marker in ["href='#i-", "icon(\"", "icon_with(\""] {
                for part in text.split(marker).skip(1) {
                    let end = part.find(['\'', '"']).unwrap_or(0);
                    let name = &part[..end];
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    {
                        continue;
                    }
                    assert!(is_known(name), "{} uses unknown icon {name}", f.display());
                    checked += 1;
                }
            }
        }
        assert!(checked > 50, "only {checked} literal icon names found");
    }
}

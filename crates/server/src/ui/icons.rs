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
}

//! The Python dashboard's module-level constants (icons, nav tables, prompts,
//! CSS, JS), read from `assets/constants.json` and `assets/blobs/` so nothing is
//! retyped by hand. See `tools/extract_ui_assets.py` for what is included.
//!
//! Modules: `dashboard_server`, `update_pages`, `brand_logos`, `inbox_pages`, `i18n`.
//! Tuples are JSON arrays, dicts keep their order.

use super::assets;
use serde_json::Value;
use std::sync::OnceLock;

fn all() -> &'static Value {
    static CONSTANTS: OnceLock<Value> = OnceLock::new();
    CONSTANTS.get_or_init(|| {
        let text = assets::text("constants.json").expect("assets/constants.json");
        serde_json::from_str(&assets::versioned_brand_urls(text))
            .expect("assets/constants.json is valid JSON")
    })
}

/// The JSON value of `module.name` (a blob reference is returned as-is; use
/// [`str`] for text).
pub fn value(module: &str, name: &str) -> Option<&'static Value> {
    all().get(module)?.get(name)
}

/// A string constant, following a `{"$blob": path}` reference to `assets/blobs`.
pub fn get_str(module: &str, name: &str) -> Option<&'static str> {
    match value(module, name)? {
        Value::String(s) => Some(s.as_str()),
        Value::Object(o) => assets::text(o.get("$blob")?.as_str()?),
        _ => None,
    }
}

/// [`get_str`], but a missing constant is a bug in the caller or a stale asset
/// tree, so it panics with the fix.
#[track_caller]
pub fn str(module: &str, name: &str) -> &'static str {
    get_str(module, name).unwrap_or_else(|| {
        panic!("UI constant {module}.{name} missing: run tools/extract_ui_assets.py")
    })
}

/// `dashboard_server.<name>`, the common case.
#[track_caller]
pub fn ds(name: &str) -> &'static str {
    str("dashboard_server", name)
}

/// A list constant (`tuple` in Python).
#[track_caller]
pub fn list(module: &str, name: &str) -> &'static [Value] {
    value(module, name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("UI list constant {module}.{name} missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_resolve_including_blobs() {
        assert!(ds("_STYLE").len() > 100_000);
        assert!(ds("_SPINNER_ICON").starts_with("<span class='md-spinner"));
        assert_eq!(list("dashboard_server", "_NAV_ITEMS").len(), 7);
        assert!(get_str("dashboard_server", "_NOPE").is_none());
    }

    #[test]
    fn stylesheet_uses_the_macos_palette_and_system_font() {
        let css = ds("_STYLE");
        assert!(
            css.contains("--md-primary: #0A84FF;"),
            "dark primary is system blue"
        );
        assert_eq!(
            css.matches("--md-primary: #007AFF;").count(),
            2,
            "light primary in both light blocks"
        );
        assert!(
            css.contains("--font-family-stack: -apple-system"),
            "SF is the default font"
        );
        assert!(!css.contains("--font-family-stack: 'Roboto', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;\n\n  --md-primary"));
        assert!(
            css.contains(":root[data-font=\"roboto\"]"),
            "Roboto stays selectable"
        );
        assert!(ds("_FONT_FACE_VARS").contains(":root[data-font=\"roboto\"]"));
        assert_eq!(list("dashboard_server", "_FONT_CHOICES")[0][0], "system");
    }

    #[test]
    fn stylesheet_has_macos_component_rules() {
        let css = ds("_STYLE");
        for needle in [
            "backdrop-filter: saturate(180%) blur(20px)",
            "box-shadow: 0 0 0 3px color-mix(in srgb, var(--md-primary) 60%, transparent)",
            ".tab-list, .hub-tabs, .pref-segmented {",
            "background: color-mix(in srgb, var(--seg-thumb) 14%, transparent); border-radius: 9px",
        ] {
            assert!(css.contains(needle), "missing: {needle}");
        }
        assert!(
            css.contains("@supports not (backdrop-filter"),
            "opaque fallback"
        );
        let a = css.find("a:focus-visible,").expect("global focus rule");
        let block = &css[a..a + css[a..].find('}').unwrap()];
        assert!(
            !block.contains("border-radius"),
            "global focus ring must not reshape pills/switches"
        );
        let ring = "inset 0 0 0 2px color-mix(in srgb, var(--md-primary) 60%, transparent)";
        assert!(
            css.contains(&format!(
                ".tab-button:focus-visible, .hub-tab:focus-visible, .pref-segmented-option:focus-visible {{ box-shadow: {ring}; }}"
            )),
            "inset tab ring"
        );
        assert!(
            css.contains(
                ".tab-button.is-active:focus-visible, .hub-tab.active:focus-visible, .pref-segmented-option.is-active:focus-visible { box-shadow: 0 1px 2px rgba(0, 0, 0, 0.25), inset 0 0 0 2px rgba(255, 255, 255, 0.6); }"
            ),
            "active tab keeps its raised shadow under focus"
        );
    }

    #[test]
    fn focus_rings_are_strong_and_survive_forced_colors() {
        let css = ds("_STYLE");
        assert!(!css.contains("var(--md-primary) 35%, transparent)"));
        assert!(!css.contains("var(--md-primary) 55%, transparent)"));
        assert!(!css.contains("var(--cg-accent) 35%, transparent)"));
        let a = css.find("a:focus-visible,").expect("global focus rule");
        let block = &css[a..a + css[a..].find('}').unwrap()];
        assert!(block.contains("outline: 2px solid transparent;"), "{block}");
        assert!(!block.contains("outline: none"));
    }

    #[test]
    fn form_fields_stand_out_inside_tinted_cards() {
        let css = ds("_STYLE");
        assert!(
            !css.contains("border: 0.5px solid var(--md-outline-variant);\n  border-radius: 8px; background: var(--md-surface-container-lowest)")
        );
        for line in css.lines().filter(|l| l.contains("border: 0.5px solid")) {
            assert!(
                !line.contains("var(--md-outline-variant)"),
                "field border collapses into the card tint: {line}"
            );
        }
        // fill must not be the colour the card itself remaps to
        let tinted = css.find(":root[data-accent=\"indigo\"] .card,").unwrap();
        let remap = &css[tinted..tinted + css[tinted..].find('}').unwrap()];
        assert!(remap.contains("--md-surface-container-lowest: var(--md-nav-surface)"));
        assert!(
            css.matches("background: var(--md-surface);").count() >= 3,
            "daemon, select and connector fields fill with --md-surface"
        );
    }

    #[test]
    fn default_accent_selected_nav_item_uses_the_primary_container() {
        let css = ds("_STYLE");
        let a = css.find(":root[data-accent=\"default\"] {").unwrap();
        let block = &css[a..a + css[a..].find('}').unwrap()];
        assert!(block.contains("--md-nav-active-surface: var(--md-primary-container);"));
        assert!(block.contains("--md-nav-active-on-surface: var(--md-on-primary-container);"));
    }

    #[test]
    fn segmented_selection_follows_the_theme_accent() {
        let css = ds("_STYLE");
        assert!(css.contains("background: var(--seg-thumb); color: #fff;"));
        for accent in ["default", "indigo", "blue", "green", "red", "gray", "dark"] {
            let a = css
                .find(&format!(":root[data-accent=\"{accent}\"] {{"))
                .unwrap();
            let block = &css[a..a + css[a..].find('}').unwrap()];
            assert!(
                block.contains("--seg-thumb:"),
                "{accent} sets the thumb colour"
            );
        }
    }

    #[test]
    fn topbar_icons_follow_the_theme_accent() {
        let css = ds("_STYLE");
        for accent in ["default", "indigo", "blue", "green", "red", "gray", "dark"] {
            let a = css
                .find(&format!(":root[data-accent=\"{accent}\"] {{"))
                .unwrap();
            let block = &css[a..a + css[a..].find('}').unwrap()];
            assert!(
                block.contains("--md-nav-accent:"),
                "{accent} sets the accent colour"
            );
        }
        for selector in [".ai-panel-trigger {", ".topbar-icon {"] {
            let a = css.find(selector).unwrap();
            let block = &css[a..a + css[a..].find('}').unwrap()];
            assert!(
                block.contains("color: var(--md-nav-accent);"),
                "{selector} uses the theme accent"
            );
        }
        assert!(css.contains(".topbar #queue-pill.pill-outline { color: var(--md-nav-accent); }"));
    }

    #[test]
    fn dark_theme_is_a_dark_wash_with_light_text_and_the_white_wordmark() {
        let css = ds("_STYLE");
        let a = css.find(":root[data-accent=\"dark\"] {").unwrap();
        let block = &css[a..a + css[a..].find('}').unwrap()];
        assert!(block.contains("--md-nav-surface: #1C1C1F;"), "{block}");
        assert!(block.contains("--md-nav-on-surface: #E4E4E8;"), "{block}");
        assert!(
            css.contains(":root[data-accent=\"dark\"] .card {"),
            "cards take the wash"
        );
        assert!(css.contains(
            ":root[data-accent=\"dark\"] .brand .brand-name-img.on-dark { display: block; }"
        ));
        assert!(css.contains(":root:not([data-accent=\"default\"]):not([data-accent=\"dark\"]) .brand .brand-name-img.on-light"));
        let choices = list("dashboard_server", "_ACCENT_CHOICES");
        assert!(choices
            .iter()
            .any(|c| c[0] == "dark" && c[1] == "Dark" && c[2] == "#1C1C1F"));
    }

    #[test]
    fn dark_theme_keeps_the_connector_gallery_dark_in_light_mode() {
        // The light-mode gallery tokens mix the theme wash with white, which only
        // works for a light wash; Dark resets them after those rules (same specificity).
        let css = ds("_STYLE");
        let light = css
            .rfind(":root[data-color-mode=\"light\"] .connector-gallery,")
            .unwrap();
        let a = css
            .find(":root[data-accent=\"dark\"] .connector-gallery,")
            .expect("dark theme gallery override");
        assert!(a > light, "the override comes after the light-mode rules");
        let block = &css[a..a + css[a..].find('}').unwrap()];
        assert!(
            block.contains("--cg-card: var(--md-surface-container-low);"),
            "{block}"
        );
        assert!(
            block.contains("--cg-chip-fg: var(--md-on-surface-variant);"),
            "{block}"
        );
    }

    #[test]
    fn stylesheet_comments_are_current() {
        let css = ds("_STYLE");
        assert!(!css.contains("Material Symbols glyph"));
        assert!(!css.contains("500 weight as every other heading"));
    }
}

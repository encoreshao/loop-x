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
        serde_json::from_str(assets::text("constants.json").expect("assets/constants.json"))
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
            "box-shadow: 0 0 0 3px color-mix(in srgb, var(--md-primary) 35%, transparent)",
            ".tab-list, .hub-tabs {",
            "background: var(--md-surface-container-high); border-radius: 9px",
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
        let ring = "inset 0 0 0 2px color-mix(in srgb, var(--md-primary) 55%, transparent)";
        assert!(
            css.contains(&format!(
                ".tab-button:focus-visible, .hub-tab:focus-visible {{ box-shadow: {ring}; }}"
            )),
            "inset tab ring"
        );
        assert!(
            css.contains(&format!(
                ".tab-button.is-active:focus-visible, .hub-tab.active:focus-visible {{ box-shadow: var(--shadow-1), {ring}; }}"
            )),
            "active tab keeps its raised shadow under focus"
        );
    }
}

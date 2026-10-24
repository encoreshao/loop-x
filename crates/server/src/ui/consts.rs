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
}

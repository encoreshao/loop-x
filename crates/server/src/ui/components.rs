//! Small reusable UI pieces shared by every page, so a look is defined once:
//! the LoopX wordmark and the floating pill controls (`.pill-btn`, styled in `_STYLE.txt`).

use crate::ui::pyfmt::escape as h;

/// Wraps every "LoopX" in already-escaped HTML so the "X" takes the brand accent
/// (`.brand-x`). Use it only for text content, never inside an attribute or `<title>`.
pub fn wordmark(escaped_html: &str) -> String {
    escaped_html.replace("LoopX", "Loop<span class='brand-x'>X</span>")
}

/// A floating pill button; `class` adds to `pill-btn`, `attrs` is raw extra attributes
/// (e.g. a `data-` hook).
pub fn pill_button(label: &str, class: &str, attrs: &str) -> String {
    format!(
        "<button type='button' class='pill-btn {class}' {attrs}>{}</button>",
        h(label)
    )
}

/// A round icon-only pill link with an accessible name; `icon_html` is an inline icon.
pub fn pill_icon_link(href: &str, label: &str, icon_html: &str, attrs: &str) -> String {
    format!(
        "<a class='pill-btn pill-btn-icon' href='{}' aria-label=\"{1}\" title=\"{1}\" {attrs}>{icon_html}</a>",
        h(href),
        h(label)
    )
}

/// A row of pill controls anchored to a corner of its positioned parent.
pub fn pill_bar(children: &str) -> String {
    format!("<div class='pill-bar'>{children}</div>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordmark_accents_the_x() {
        assert_eq!(
            wordmark("Ask LoopX"),
            "Ask Loop<span class='brand-x'>X</span>"
        );
        assert_eq!(wordmark("nothing"), "nothing");
    }

    #[test]
    fn pill_icon_link_escapes_and_labels() {
        let s = pill_icon_link("/?a=1&b=2", "Say \"hi\"", "<svg/>", "");
        assert!(
            s.contains("href='/?a=1&amp;b=2'") && s.contains("aria-label=\"Say &quot;hi&quot;\"")
        );
    }

    #[test]
    fn pill_bar_wraps_children() {
        assert_eq!(pill_bar("x"), "<div class='pill-bar'>x</div>");
    }
}

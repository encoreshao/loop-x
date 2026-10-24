//! Helpers shared by the loop pages (`/loops/<name>`): the tabbed loop page frame
//! (`render_loop_page`), the flash banner, the Run now / Stop / empty-state blocks,
//! `_relative_time`, `_custom_select` and `_mask_secret`. Registers no routes.

use crate::ui::badge::default_badge;
use crate::ui::hub::{self, Hub};
use crate::ui::i18n;
use crate::ui::pyfmt;
use crate::ui::routes::{Registry, UiCtx};
use crate::ui::shell::{render_shell, ShellOpts};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};

pub fn register(_r: &mut Registry) {}

/// `_flash_html`: the POST-redirect-GET banner (the text is always escaped).
pub fn flash_html(flash: Option<&str>, flash_ok: bool) -> String {
    match flash {
        Some(f) if !f.is_empty() => {
            let class = if flash_ok {
                "flash-success"
            } else {
                "flash-danger"
            };
            format!("<div class='flash {class}'>{}</div>", pyfmt::escape(f))
        }
        _ => String::new(),
    }
}

/// `render_loop_page`: title, tab strip and the view's body inside the shell, as
/// `loop:<name>`. `body` is the already rendered body of the view that
/// `resolve_view(hub, requested)` picks; `badge` overrides the default status pill.
pub fn render_loop_page(
    ctx: &UiCtx,
    page: &Hub,
    name: &str,
    requested_view: Option<&str>,
    body: &str,
    badge: Option<&str>,
) -> String {
    let current = hub::resolve_view(page, requested_view);
    let tabs = hub::tab_strip_html(ctx.lang, &page.path, &page.views, &current.key, "");
    let label = i18n::t(ctx.lang, &page.label, &[]);
    let title = format!("<div class='page-title'><h1>{}</h1></div>", pyfmt::escape(&label));
    let default = default_badge(ctx);
    render_shell(
        ctx,
        &format!("{label} · Loop X Engineering"),
        &format!("loop:{name}"),
        badge.unwrap_or(&default),
        &format!("{title}{tabs}{body}"),
        ShellOpts {
            refresh: current.refresh,
            refresh_note: current.refresh,
            lazy_refresh: current.lazy_refresh,
        },
    )
}

/// `_run_now_action_html`.
pub fn run_now_action_html(
    ctx: &UiCtx,
    action: &str,
    confirm_text: &str,
    csrf_input: &str,
    disabled_hint_html: Option<&str>,
) -> String {
    let run_now = pyfmt::escape(&ctx.t("Run now"));
    if let Some(hint) = disabled_hint_html {
        return format!(
            "\n<div class='run-now-action'>\n<button type='button' class='btn btn-primary' disabled>\n<span class='material-symbols-outlined' aria-hidden='true'>bolt</span> {run_now}\n</button>\n<p class='run-now-hint'>{hint}</p>\n</div>\n"
        );
    }
    let confirm_attr = pyfmt::escape(confirm_text);
    format!(
        "\n<div class='run-now-action'>\n<form method='post' action='{action}' class='daemon-action-form'>\n{csrf_input}\n<button type='submit' class='btn btn-primary' data-confirm=\"{confirm_attr}\">\n<span class='material-symbols-outlined' aria-hidden='true'>bolt</span> {run_now}\n</button>\n</form>\n</div>\n"
    )
}

/// `_stop_action_html`.
pub fn stop_action_html(ctx: &UiCtx, action: &str, confirm_text: &str, csrf_input: &str) -> String {
    let confirm_attr = pyfmt::escape(confirm_text);
    let stop = pyfmt::escape(&ctx.t("Stop"));
    format!(
        "\n<div class='run-now-action'>\n<form method='post' action='{action}' class='daemon-action-form'>\n{csrf_input}\n<button type='submit' class='btn btn-warning' data-confirm=\"{confirm_attr}\">\n<span class='material-symbols-outlined' aria-hidden='true'>warning</span> {stop}\n</button>\n</form>\n</div>\n"
    )
}

/// `_empty_state_html` (`message` and `action_label` are already escaped HTML).
pub fn empty_state_html(message: &str, action_href: &str, action_label: &str) -> String {
    format!(
        "\n<div class='empty-state'>\n<div class='empty-state-icon'><span class='material-symbols-outlined' aria-hidden='true'>folder_off</span></div>\n<p class='empty-state-message'>{message}</p>\n<a class='btn btn-primary empty-state-action' href='{action_href}'>\n<span class='material-symbols-outlined' aria-hidden='true'>settings</span> {action_label}\n</a>\n</div>\n"
    )
}

/// `_aware`: an ISO timestamp (a trailing `Z` allowed) as an offset-aware time;
/// naive ones are taken as UTC. `None` when it is not a timestamp.
pub fn aware(value: &str) -> Option<DateTime<FixedOffset>> {
    let text = value.trim().replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&text) {
        return Some(dt);
    }
    // Python's fromisoformat also takes a space separator, no offset, a date only.
    let text = text.replacen(' ', "T", 1);
    if let Ok(dt) = DateTime::parse_from_str(&text, "%Y-%m-%dT%H:%M:%S%.f%:z") {
        return Some(dt);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&text, fmt) {
            return Some(Utc.from_utc_datetime(&n).fixed_offset());
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(&text, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?).fixed_offset());
    }
    None
}

/// `html.escape(_t(text))`.
pub fn esc_t(ctx: &UiCtx, text: &str) -> String {
    pyfmt::escape(&ctx.t(text))
}

/// `_relative_time`: "2h ago" for an ISO timestamp, the raw text when it is not one.
pub fn relative_time(now: DateTime<Utc>, iso: &str) -> String {
    if iso.is_empty() {
        return String::new();
    }
    match aware(iso) {
        Some(dt) => relative_time_dt(now, dt),
        None => iso.to_string(),
    }
}

/// `_relative_time` for an already parsed timestamp.
pub fn relative_time_dt(now: DateTime<Utc>, dt: DateTime<FixedOffset>) -> String {
    let seconds = (now - dt.with_timezone(&Utc)).num_microseconds().unwrap_or(0) as f64 / 1e6;
    if seconds < 60.0 {
        return "just now".to_string();
    }
    let minutes = seconds / 60.0;
    if minutes < 60.0 {
        return format!("{}m ago", minutes as i64);
    }
    let hours = minutes / 60.0;
    if hours < 24.0 {
        return format!("{}h ago", hours as i64);
    }
    let days = hours / 24.0;
    if days < 30.0 {
        return format!("{}d ago", days as i64);
    }
    let months = days / 30.0;
    if months < 12.0 {
        return format!("{}mo ago", months as i64);
    }
    format!("{}y ago", (days / 365.0) as i64)
}

/// `_custom_select`. `options` are (value, label) pairs; `empty_label` prepends a
/// `value=''` option.
pub fn custom_select(
    name: &str,
    options: &[(String, String)],
    selected: Option<&str>,
    empty_label: Option<&str>,
) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(l) = empty_label {
        pairs.push((String::new(), l.to_string()));
    }
    pairs.extend(options.iter().cloned());
    let selected = selected.unwrap_or("");
    let option_tags: String = pairs
        .iter()
        .map(|(v, l)| {
            format!(
                "<option value='{}'{}>{}</option>",
                pyfmt::escape(v),
                if v == selected { " selected" } else { "" },
                pyfmt::escape(l)
            )
        })
        .collect();
    let menu_items: String = pairs
        .iter()
        .map(|(v, l)| {
            format!(
                "<div class='custom-select-option{}' role='option' tabindex='-1' data-value='{}'>{}</div>",
                if v == selected { " is-selected" } else { "" },
                pyfmt::escape(v),
                pyfmt::escape(l)
            )
        })
        .collect();
    let label = pairs
        .iter()
        .find(|(v, _)| v == selected)
        .or(pairs.first())
        .map(|(_, l)| l.as_str())
        .unwrap_or("");
    format!(
        "<div class='custom-select'><select name='{}' class='custom-select-native'>{option_tags}</select><button type='button' class='custom-select-trigger' aria-haspopup='listbox' aria-expanded='false'><span class='custom-select-value'>{}</span><span class='material-symbols-outlined' aria-hidden='true'>expand_more</span></button><div class='custom-select-menu' role='listbox' hidden>{menu_items}</div></div>",
        pyfmt::escape(name),
        pyfmt::escape(label)
    )
}

/// `custom_select` with values that double as labels.
pub fn custom_select_plain<S: AsRef<str>>(
    name: &str,
    options: &[S],
    selected: Option<&str>,
    empty_label: Option<&str>,
) -> String {
    let pairs: Vec<(String, String)> = options
        .iter()
        .map(|o| (o.as_ref().to_string(), o.as_ref().to_string()))
        .collect();
    custom_select(name, &pairs, selected, empty_label)
}

/// `_mask_secret`: bullets plus the last four characters.
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() >= 4 {
        format!("••••{}", chars[chars.len() - 4..].iter().collect::<String>())
    } else {
        "••••".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_buckets() {
        let now = DateTime::parse_from_rfc3339("2026-05-01T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(relative_time(now, ""), "");
        assert_eq!(relative_time(now, "nope"), "nope");
        assert_eq!(relative_time(now, "2026-05-01T11:59:30Z"), "just now");
        assert_eq!(relative_time(now, "2026-05-01T10:00:00Z"), "2h ago");
        assert_eq!(relative_time(now, "2026-04-28T11:59:59.5Z"), "3d ago");
        assert_eq!(relative_time(now, "2026-04-28"), "3d ago");
        assert_eq!(relative_time(now, "2025-01-01T00:00:00Z"), "1y ago");
    }

    #[test]
    fn masks_and_selects() {
        assert_eq!(mask_secret("abc"), "••••");
        assert_eq!(mask_secret("abcdefgh"), "••••efgh");
        let s = custom_select("x", &[("a".into(), "A".into())], Some("a"), Some("none"));
        assert!(s.contains("<option value='a' selected>A</option>"));
        assert!(s.contains("<span class='custom-select-value'>A</span>"));
        assert_eq!(flash_html(None, true), "");
        assert!(flash_html(Some("<b>"), false).contains("flash-danger'>&lt;b&gt;"));
    }
}

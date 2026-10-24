//! Connector brand marks (`bin/web/brand_logos.py`): inline Simple Icons SVG paths
//! (CC0), never loaded from outside. The path table is `brand_logos.LOGOS` from
//! the generated constants, so adding a brand means editing the Python file and
//! re-running `tools/extract_ui_assets.py`.

use super::{consts, pyfmt};

fn logo(key: &str) -> Option<(&'static str, &'static str)> {
    let entry = consts::value("brand_logos", "LOGOS")?
        .get(key)?
        .as_array()?;
    Some((entry.first()?.as_str()?, entry.get(1)?.as_str()?))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `brand_color`: the brand's hex, or the lettermark palette colour for an unknown key.
pub fn brand_color(key: &str) -> String {
    if let Some((_, color)) = logo(key) {
        return color.to_uppercase();
    }
    let palette = consts::list("brand_logos", "_LETTER_PALETTE");
    let idx = crc32(key.as_bytes()) as usize % palette.len();
    palette[idx].as_str().unwrap_or("#5B6CFF").to_string()
}

fn clamp_size(size: &str) -> u32 {
    let trimmed = size.trim_start();
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    let n = digits.parse::<u32>().unwrap_or(28);
    n.clamp(12, 96)
}

fn initials(label: &str) -> String {
    label
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .collect::<String>()
        .to_uppercase()
}

fn valid_class(cls: &str) -> bool {
    !cls.is_empty()
        && cls
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | ' ' | '-'))
}

/// `brand_logo_svg(key, label, size, cls)`.
pub fn brand_logo_svg(key: &str, label: &str, size: &str, cls: &str) -> String {
    let cls = if valid_class(cls) { cls } else { "brand-logo" };
    let size = clamp_size(size);
    if let Some((d, color)) = logo(key) {
        let current = consts::list("brand_logos", "_CURRENT_COLOR")
            .iter()
            .any(|v| v.as_str() == Some(key));
        let fill = if current { "currentColor" } else { color };
        return format!(
            "<svg class='{cls}' viewBox='0 0 24 24' width='{size}' height='{size}' fill='{fill}' aria-hidden='true' focusable='false'><path d='{d}'/></svg>"
        );
    }
    let color = brand_color(key);
    let shown = initials(if label.is_empty() { key } else { label });
    format!(
        "<span class='{cls} brand-lettermark' aria-hidden='true' style='--brand:{color};width:{size}px;height:{size}px'>{}</span>",
        pyfmt::escape(&shown)
    )
}

/// `_brand_nav_icon`: a brand mark as an 18px monochrome nav icon.
pub fn brand_nav_icon(key: &str) -> String {
    let (d, _) = logo(key).unwrap_or(("", ""));
    format!(
        "<svg class='brand-nav-mark' viewBox='-2 -2 28 28' width='18' height='18' fill='currentColor' aria-hidden='true' focusable='false'><path d='{}'/></svg>",
        pyfmt::escape(d)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_brand_is_an_svg_and_unknown_is_a_lettermark() {
        let svg = brand_logo_svg("gitlab", "", "28", "brand-logo");
        assert!(svg.starts_with("<svg class='brand-logo' viewBox='0 0 24 24' width='28'"));
        assert!(svg.contains("fill='#FC6D26'"));
        assert!(brand_logo_svg("github", "", "28", "brand-logo").contains("fill='currentColor'"));
        let m = brand_logo_svg("nope", "<b>Evil</b> Co", "9999", "x' onload='y");
        assert!(m.contains("brand-lettermark") && m.contains("&lt;C") && m.contains("width:96px"));
        assert!(m.starts_with("<span class='brand-logo "));
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}

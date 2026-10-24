//! Python-compatible timestamps for the mail code: `datetime.isoformat()`,
//! `datetime.fromisoformat()` and `datetime.fromtimestamp(ms / 1000, utc)`.

use chrono::{DateTime, FixedOffset, SecondsFormat, TimeZone, Timelike, Utc};

/// `dt.isoformat()`: fractional seconds only when non-zero (always 6 digits),
/// and the offset as `+HH:MM`.
pub fn isoformat(dt: &DateTime<FixedOffset>) -> String {
    let fmt = if dt.nanosecond() / 1000 == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    dt.to_rfc3339_opts(fmt, false)
}

pub fn isoformat_utc(dt: DateTime<Utc>) -> String {
    isoformat(&dt.fixed_offset())
}

/// `datetime.fromisoformat(value)` for timezone-aware values (a trailing `Z`
/// is accepted, like Python 3.11+).
pub fn parse_iso(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value.trim()).ok()
}

/// `datetime.fromtimestamp(ms / 1000, timezone.utc).isoformat()`.
pub fn iso_from_millis(ms: i64) -> String {
    let dt = Utc
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or(DateTime::UNIX_EPOCH);
    isoformat_utc(dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isoformat_matches_python() {
        let d = parse_iso("2026-09-27T08:00:00Z").unwrap();
        assert_eq!(isoformat(&d), "2026-09-27T08:00:00+00:00");
        assert_eq!(
            iso_from_millis(1_790_000_000_123),
            "2026-09-21T14:13:20.123000+00:00"
        );
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00+00:00");
        let z = parse_iso("2026-09-27T08:00:00.0000000Z").unwrap();
        assert_eq!(isoformat(&z), "2026-09-27T08:00:00+00:00");
        let o = parse_iso("2026-09-27T10:00:00+02:00").unwrap();
        assert_eq!(isoformat(&o), "2026-09-27T10:00:00+02:00");
        assert!(parse_iso("nonsense").is_none());
    }
}

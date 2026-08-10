//! Pure, dependency-free formatting helpers for byte counts and
//! timestamps — shared by `ui::dirview::list`'s size/date columns and
//! `ui::dialogs::properties`'s size/modified rows, so both surfaces render
//! the same "12.3 MB" / "2026-08-10 09:41" text for the same underlying
//! data rather than drifting into two near-identical formatters.
//!
//! Promoted from `ui::dirview::list`, where this stage's properties dialog
//! became the second consumer — see that module's `size_text`/`date_text`
//! for the one place a `FileEntry`'s absence-vs-presence (a directory has
//! no meaningful size in the list view; a `None` `modified` renders blank)
//! is still decided locally, since that's row-shape policy, not formatting.

use std::time::SystemTime;

/// `bytes` -> a human-scaled string ("512 B", "3.0 MB", …), one decimal
/// place from KB up, whole bytes below 1024 — not locale-aware, not
/// configurable (binary MiB/KiB-style 1024 steps, labeled with the
/// familiar "KB"/"MB" the style guide's mockups use, not the pedantically
/// correct "KiB"/"MiB").
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    let label = UNITS.get(unit).copied().unwrap_or("TB");
    format!("{size:.1} {label}")
}

/// A minimal, dependency-free "YYYY-MM-DD HH:MM" formatter for
/// `SystemTime`. Not a general calendar library — just enough to label a
/// list row or a properties dialog; a real date/time crate can replace
/// this if a later stage needs more (relative "2 days ago" phrasing,
/// locale-aware formats, …).
pub fn format_system_time(time: SystemTime) -> String {
    let Ok(duration) = time.duration_since(std::time::UNIX_EPOCH) else {
        return String::new(); // times before 1970 aren't worth a crate; blank is honest
    };
    let secs = duration.as_secs();
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (hour, minute) = (time_of_day / 3600, (time_of_day % 3600) / 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`: days-since-1970-01-01 (proleptic
/// Gregorian) -> `(year, month, day)`. A well-known, correct, allocation-
/// and dependency-free algorithm; see
/// <http://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 MB");
    }

    #[test]
    fn format_system_time_renders_year_month_day_hour_minute() {
        // 2000-01-01 00:00:00 UTC — `10_957` days after the epoch, per
        // `civil_from_days_matches_known_dates` above.
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10_957 * 86_400);
        assert_eq!(format_system_time(time), "2000-01-01 00:00");
    }
}

//! Persist and restore the user's choice of 12-hour (AM/PM) or 24-hour clock
//! for chat-message timestamps.

use std::path::PathBuf;

use gtk::glib;

/// 12-hour clock with AM/PM suffix, e.g. "01:30 PM".
///
/// ```rust
/// assert_eq!(time_format::format_time(ms, time_format::TimeFormat::AmPm), "01:30 PM");
/// ```
///
/// 24-hour clock, e.g. "13:30".
///
/// ```rust
/// assert_eq!(time_format::format_time(ms, time_format::TimeFormat::H24), "13:30");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum TimeFormat {
    /// 12-hour with AM/PM suffix — the default.
    #[default]
    AmPm,
    /// 24-hour, zero-padded hour.
    H24,
}


/// Default time format: 12-hour AM/PM.
pub const DEFAULT: TimeFormat = TimeFormat::AmPm;

#[cfg(not(test))]
const STATE_FILE: &str = "time_format.txt";

/// Data directory for bubbles. `pub(crate)` for test access.
pub(crate) fn data_dir() -> PathBuf {
    glib::user_data_dir().join("bubbles")
}

/// Path to the time-format state file. `pub(crate)` for test access.
pub(crate) fn state_path() -> PathBuf {
    #[cfg(test)]
    return data_dir().join(format!(
        "time_format-{:?}.txt",
        std::thread::current().id()
    ));
    #[cfg(not(test))]
    data_dir().join(STATE_FILE)
}

/// Read the saved time format, or [`DEFAULT`] if nothing is saved yet.
pub fn get() -> TimeFormat {
    let data = match std::fs::read_to_string(state_path()) {
        Ok(d) => d,
        Err(_) => return DEFAULT,
    };
    match data.trim() {
        "1" => TimeFormat::H24,
        _ => DEFAULT,
    }
}

/// Save a time format to disk. Creates the parent directory if needed.
pub fn set(mode: TimeFormat) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, match mode {
        TimeFormat::AmPm => "0",
        TimeFormat::H24 => "1",
    });
}

/// Format a unix-epoch millisecond timestamp as a local-time clock string.
///
/// - `AmPm`: `"%I:%M %p"` — leading zero on hour, e.g. "01:30 PM", "12:00 AM"
/// - `H24`:   `"%H:%M"`   — leading zero on hour, e.g. "13:30", "00:00"
pub fn format_time(ms: i64, mode: TimeFormat) -> String {
    let format_str = match mode {
        TimeFormat::AmPm => "%I:%M %p",
        TimeFormat::H24 => "%H:%M",
    };
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|dt| dt.format(format_str))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Full local date + time string, suitable for hover tooltips on message
/// timestamps.
///
/// - `AmPm`: e.g. `"June 15, 2024 01:30 PM"`
/// - `H24`:  e.g. `"June 15, 2024 13:30"`
///
/// Used by message timestamp tooltips.
pub fn format_full_timestamp(ms: i64, mode: TimeFormat) -> String {
    let format_str = match mode {
        TimeFormat::AmPm => "%B %e, %Y %I:%M %p",
        TimeFormat::H24 => "%B %e, %Y %H:%M",
    };
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|dt| dt.format(format_str))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Stable human-readable local-date label, e.g. `"June 15, 2024"`.
///
/// The output is deterministic (no time-of-day or relative components) so it
/// is suitable for date-dividers in the chat history.
///
/// Used by date-divider widgets in the timeline.
pub fn format_date_label(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|dt| dt.format("%B %e, %Y"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Decide whether a date divider should be shown before a message.
///
/// * `prev` — timestamp of the previous message in the list, or `None` for
///   the first message.
/// * `current` — timestamp of the message being considered.
/// * `now` — the current wall-clock time (for determining "today").
///
/// Returns `true` when a divider is needed:
///   - The message is the first in the list (`prev` is `None`) and it is
///     **not** from today.
///   - The message's calendar date is different from the previous message's
///     calendar date, **and** it is not from today.
///
/// Returns `false` when no divider is needed:
///   - The message is from today (regardless of `prev`).
///   - The message's calendar date matches the previous message's date (when
///     both are non-today; consecutive same-date messages share one divider).
///
/// Used by the message-list renderer to insert date dividers.
pub fn should_show_date_divider(prev: Option<i64>, current: i64, now: i64) -> bool {
    let current_date = calendar_date(current);
    let today_date = calendar_date(now);

    // Today's messages never get a date divider.
    if current_date == today_date {
        return false;
    }

    match prev {
        // First message, non-today → divider.
        None => true,
        Some(prev_ms) => {
            let prev_date = calendar_date(prev_ms);
            // Divider when calendar dates differ (both non-today, since we
            // already handled the today case above).
            prev_date != current_date
        }
    }
}

/// Extract the local calendar date `(year, month, day)` from a unix-epoch
/// millisecond timestamp.
fn calendar_date(ms: i64) -> (i32, i32, i32) {
    glib::DateTime::from_unix_local(ms / 1000)
        .map(|dt| (dt.year(), dt.month(), dt.day_of_month()))
        .unwrap_or((0, 0, 0))
}

// --- tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the timezone and XDG data dir once per process so that
    /// `format_time` assertions are deterministic regardless of where CI runs.
    fn setup_isolated_data_dir() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            // TZ=UTC ensures from_unix_local agrees with our datetime_ms helper.
            std::env::set_var("TZ", "UTC");
            let dir = std::env::temp_dir().join(format!(
                "openbubbles-time-format-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            // g_get_user_data_dir reads XDG_DATA_HOME.
            std::env::set_var("XDG_DATA_HOME", &dir);
        });
    }

    /// Convert a (year, month, day, hour, minute, second) tuple in *local* time
    /// to a unix-epoch millisecond timestamp.  Assumes TZ=UTC so local==UTC.
    fn datetime_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        (days_from_civil(y, mo as i32, d as i32) * 86_400
            + h as i64 * 3600
            + mi as i64 * 60
            + s as i64) * 1000
    }

    /// Howard Hinnant's inverse-ymd-to-days algorithm.
    fn days_from_civil(y: i32, m: i32, d: i32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as i64; // [0, 399]
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as i64; // [0, 146096]
        era as i64 * 146097 + doe - 719468
    }

    /// Path to the state file, delegating to the module's helper.
    fn state_file_path() -> PathBuf {
        super::state_path()
    }

    // --- format_time tests ---

    #[test]
    fn format_time_ampm_afternoon_uses_leading_zero_and_pm() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 13, 30, 0);
        assert_eq!(
            format_time(ms, TimeFormat::AmPm),
            "01:30 PM"
        );
    }

    #[test]
    fn format_time_ampm_morning() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 9, 5, 0);
        assert_eq!(
            format_time(ms, TimeFormat::AmPm),
            "09:05 AM"
        );
    }

    #[test]
    fn format_time_ampm_midnight_is_12_am() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 0, 0, 0);
        assert_eq!(
            format_time(ms, TimeFormat::AmPm),
            "12:00 AM"
        );
    }

    #[test]
    fn format_time_ampm_noon_is_12_pm() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 12, 0, 0);
        assert_eq!(
            format_time(ms, TimeFormat::AmPm),
            "12:00 PM"
        );
    }

    #[test]
    fn format_time_24h_afternoon() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 13, 30, 0);
        assert_eq!(
            format_time(ms, TimeFormat::H24),
            "13:30"
        );
    }

    #[test]
    fn format_time_24h_midnight() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 0, 0, 0);
        assert_eq!(
            format_time(ms, TimeFormat::H24),
            "00:00"
        );
    }

    #[test]
    fn format_time_24h_noon() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 1, 15, 12, 0, 0);
        assert_eq!(
            format_time(ms, TimeFormat::H24),
            "12:00"
        );
    }

    // --- state persistence tests ---

    #[test]
    fn default_is_ampm_when_no_file() {
        setup_isolated_data_dir();
        let _ = std::fs::remove_file(state_file_path());
        assert_eq!(get(), TimeFormat::AmPm);
    }

    #[test]
    fn set_then_get_roundtrips_ampm() {
        setup_isolated_data_dir();
        // Start clean so prior test in this process doesn't bleed into this one.
        let _ = std::fs::remove_file(state_file_path());
        set(TimeFormat::AmPm);
        assert_eq!(get(), TimeFormat::AmPm);
    }

    #[test]
    fn set_then_get_roundtrips_h24() {
        setup_isolated_data_dir();
        let _ = std::fs::remove_file(state_file_path());
        set(TimeFormat::H24);
        assert_eq!(get(), TimeFormat::H24);
    }

    #[test]
    fn corrupted_file_falls_back_to_default() {
        setup_isolated_data_dir();
        std::fs::create_dir_all(state_file_path().parent().unwrap()).unwrap();
        std::fs::write(state_file_path(), "garbage not a 0 or 1").unwrap();
        assert_eq!(get(), TimeFormat::AmPm);
    }

    // --- full timestamp formatter (hover tooltip) ---

    /// Hover tooltip on a message: local date + time, AmPm mode.
    /// Pinned to a single stable format so the tooltip text is deterministic.
    #[test]
    fn format_full_timestamp_ampm() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 6, 15, 13, 30, 0);
        assert_eq!(
            format_full_timestamp(ms, TimeFormat::AmPm),
            "June 15, 2024 01:30 PM"
        );
    }

    /// Hover tooltip on a message: local date + time, H24 mode.
    #[test]
    fn format_full_timestamp_h24() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 6, 15, 13, 30, 0);
        assert_eq!(
            format_full_timestamp(ms, TimeFormat::H24),
            "June 15, 2024 13:30"
        );
    }

    // --- date divider formatter ---

    /// Date label for a non-today message date: stable, human-readable.
    /// Same input must always produce the same string (no random/time-of-day
    /// components).
    #[test]
    fn format_date_label_is_stable_human_readable() {
        setup_isolated_data_dir();
        let ms = datetime_ms(2024, 6, 15, 13, 30, 0);
        assert_eq!(format_date_label(ms), "June 15, 2024");
    }

    // --- date divider decision helper ---
    //
    // Pin: no divider for today's messages; divider for the first non-today
    // message; divider when crossing between two different non-today dates;
    // no duplicate divider for consecutive messages on the same non-today
    // date. The "now" timestamp is passed explicitly so the test never depends
    // on real wall-clock time.

    /// today=2024-06-15; current message is also today → no divider.
    #[test]
    fn should_show_date_divider_false_for_today_message() {
        setup_isolated_data_dir();
        let now = datetime_ms(2024, 6, 15, 12, 0, 0);
        let current = datetime_ms(2024, 6, 15, 8, 0, 0);
        let prev = Some(datetime_ms(2024, 6, 15, 9, 0, 0));
        assert!(!should_show_date_divider(prev, current, now));
    }

    /// First message in a chat (no previous) and it is non-today → divider.
    #[test]
    fn should_show_date_divider_true_for_first_non_today_message() {
        setup_isolated_data_dir();
        let now = datetime_ms(2024, 6, 15, 12, 0, 0);
        let current = datetime_ms(2024, 6, 14, 10, 0, 0);
        assert!(should_show_date_divider(None, current, now));
    }

    /// Crossing from one non-today date to a different non-today date → divider.
    #[test]
    fn should_show_date_divider_true_when_crossing_non_today_dates() {
        setup_isolated_data_dir();
        let now = datetime_ms(2024, 6, 15, 12, 0, 0);
        let prev = Some(datetime_ms(2024, 6, 13, 10, 0, 0));
        let current = datetime_ms(2024, 6, 14, 10, 0, 0);
        assert!(should_show_date_divider(prev, current, now));
    }

    /// Two consecutive messages on the same non-today date → no duplicate
    /// divider on the second one.
    #[test]
    fn should_show_date_divider_false_for_consecutive_same_non_today_date() {
        setup_isolated_data_dir();
        let now = datetime_ms(2024, 6, 15, 12, 0, 0);
        let prev = Some(datetime_ms(2024, 6, 14, 9, 0, 0));
        let current = datetime_ms(2024, 6, 14, 18, 0, 0);
        assert!(!should_show_date_divider(prev, current, now));
    }
}
//! Time helpers shared by the pipeline stages, the CLI and the orchestrator.
//!
//! **Clock convention (HB-058).** The engine has exactly one wall clock: the
//! server's **local** timezone. The systemd timers that drive every stage fire
//! on that clock (`OnCalendar=` resolves in the server timezone and is
//! DST-aware), and so do the publishing and engagement windows. Only quota
//! *boundaries* are counted on a UTC day ([`utc_day`]), because a quota does not
//! care what the wall clock says. Mixing the two — a UTC minute compared against
//! a local window — is what silently closed the engagement gate, so any new
//! wall-clock comparison must go through [`local_minute_of_day`].

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};

/// Seconds in one day.
const SECONDS_PER_DAY: u64 = 86_400;

/// Current time as Unix seconds.
///
/// Falls back to `0` if the system clock is set before the Unix epoch, so a
/// misconfigured clock can never panic a run.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The UTC day (days since 1970-01-01) a Unix timestamp falls in.
///
/// The frequency policy counts posts per day and per week; a UTC day keeps that
/// accounting deterministic and testable (the publishing *windows* stay local,
/// but a quota boundary does not need to be).
pub fn utc_day(now_unix: u64) -> i64 {
    (now_unix / SECONDS_PER_DAY) as i64
}

/// Minute of the day (`0..=1439`) for a Unix timestamp at a fixed UTC offset,
/// in seconds east of UTC.
///
/// This is the **pure** core of the local-clock conversion: the offset is an
/// argument, so a test can pin it and stay independent of the machine's
/// timezone. The live path ([`local_minute_of_day`]) feeds it the system offset,
/// so the arithmetic here *is* the production arithmetic.
pub fn minute_of_day_at(now_unix: u64, utc_offset_secs: i64) -> u32 {
    let shifted = now_unix as i64 + utc_offset_secs;
    (shifted.rem_euclid(SECONDS_PER_DAY as i64) / 60) as u32
}

/// The system's UTC offset (seconds east of UTC) **at the given instant**.
///
/// Resolved through the local timezone database, so a DST transition is applied
/// for that exact instant rather than for "now". Falls back to `0` (UTC) only if
/// the instant is out of the representable range, which a live run never hits.
pub fn local_utc_offset_secs(now_unix: u64) -> i64 {
    Local
        .timestamp_opt(now_unix as i64, 0)
        .single()
        .map(|dt| i64::from(dt.offset().local_minus_utc()))
        .unwrap_or(0)
}

/// Minute of the day (`0..=1439`) in the system's **local** timezone.
///
/// The one conversion every wall-clock window comparison must use. It honours
/// DST: the offset is taken at `now_unix`, not at "now", so a fire during a
/// transition is judged on the clock it actually fired on. Brazil has no DST
/// today, but nothing here assumes that.
pub fn local_minute_of_day(now_unix: u64) -> u32 {
    minute_of_day_at(now_unix, local_utc_offset_secs(now_unix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_counts_whole_utc_days() {
        assert_eq!(utc_day(0), 0);
        assert_eq!(utc_day(86_399), 0);
        assert_eq!(utc_day(86_400), 1);
    }

    #[test]
    fn a_local_minute_is_shifted_from_utc_by_the_offset() {
        // 12:35 UTC == 09:35 at -03 (the engagement timer's first fire).
        let at_1235_utc = 12 * 3600 + 35 * 60;
        assert_eq!(minute_of_day_at(at_1235_utc, -3 * 3600), 9 * 60 + 35);
        // The same instant read in UTC is the minute the old code used.
        assert_eq!(minute_of_day_at(at_1235_utc, 0), 12 * 60 + 35);
    }

    #[test]
    fn the_offset_math_wraps_across_midnight() {
        // 01:00 UTC == 22:00 the previous day at -03.
        assert_eq!(minute_of_day_at(3600, -3 * 3600), 22 * 60);
        // 23:30 UTC == 02:30 the next day at +03.
        assert_eq!(minute_of_day_at(23 * 3600 + 30 * 60, 3 * 3600), 2 * 60 + 30);
    }

    #[test]
    fn a_negative_timestamp_shift_cannot_underflow() {
        // The first second of the epoch at -03 is the previous day's 21:00.
        assert_eq!(minute_of_day_at(0, -3 * 3600), 21 * 60);
    }
}

//! Time helpers shared by the pipeline stages, the CLI and the orchestrator.

use std::time::{SystemTime, UNIX_EPOCH};

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

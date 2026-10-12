//! Time helpers shared by the pipeline stages, the CLI and the orchestrator.

use std::time::{SystemTime, UNIX_EPOCH};

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

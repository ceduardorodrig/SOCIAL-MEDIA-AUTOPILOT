//! Engagement configuration: caps, pacing and execution windows.
//!
//! This is **data**, never code, and carries no secret (only the *name* of the
//! kill-switch environment variable). The engagement windows are deliberately
//! separate from the persona's publishing windows, so interactions never ride a
//! publishing burst.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

fn default_kill_switch_env() -> String {
    "AUTOPILOT_ENGAGE_KILL_SWITCH".to_string()
}

fn default_windows() -> Vec<String> {
    vec![
        "09:30".to_string(),
        "13:30".to_string(),
        "20:30".to_string(),
    ]
}

fn default_window_minutes() -> u32 {
    45
}

fn default_notification_reasons() -> Vec<String> {
    vec!["mention".to_string(), "reply".to_string()]
}

fn default_notification_limit() -> usize {
    50
}

fn default_own_posts_to_scan() -> usize {
    5
}

fn default_thread_depth() -> u32 {
    2
}

/// Everything the engagement stage reads to pace itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngageConfig {
    /// Environment variable that, when truthy, stops the stage immediately.
    #[serde(default = "default_kill_switch_env")]
    pub kill_switch_env: String,
    /// Execution windows (24h clock), separate from the publishing windows.
    #[serde(default = "default_windows")]
    pub windows: Vec<String>,
    /// Length of each window, in minutes.
    #[serde(default = "default_window_minutes")]
    pub window_minutes: u32,
    #[serde(default)]
    pub caps: Caps,
    #[serde(default)]
    pub pacing: Pacing,
    #[serde(default)]
    pub interests: Interests,
    /// Notification reasons treated as "someone talked to us".
    #[serde(default = "default_notification_reasons")]
    pub notification_reasons: Vec<String>,
    #[serde(default = "default_notification_limit")]
    pub notification_limit: usize,
    /// How many of our own recent posts to scan for comments.
    #[serde(default = "default_own_posts_to_scan")]
    pub own_posts_to_scan: usize,
    /// Reply-tree depth when reading comments on our own posts.
    #[serde(default = "default_thread_depth")]
    pub thread_depth: u32,
}

/// Hard per-day (and per-week) caps, plus a per-run budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caps {
    #[serde(default = "default_replies_per_day")]
    pub replies_per_day: u32,
    #[serde(default = "default_likes_per_day")]
    pub likes_per_day: u32,
    #[serde(default = "default_follows_per_day")]
    pub follows_per_day: u32,
    #[serde(default = "default_follows_per_week")]
    pub follows_per_week: u32,
    /// Total writes (reply + like + follow) allowed in a single run.
    #[serde(default = "default_actions_per_run")]
    pub max_actions_per_run: u32,
}

fn default_replies_per_day() -> u32 {
    4
}

fn default_likes_per_day() -> u32 {
    6
}

fn default_follows_per_day() -> u32 {
    1
}

fn default_follows_per_week() -> u32 {
    3
}

fn default_actions_per_run() -> u32 {
    6
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            replies_per_day: default_replies_per_day(),
            likes_per_day: default_likes_per_day(),
            follows_per_day: default_follows_per_day(),
            follows_per_week: default_follows_per_week(),
            max_actions_per_run: default_actions_per_run(),
        }
    }
}

/// Randomized pacing knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pacing {
    /// Probability that a scheduled run does nothing at all (sporadic behaviour).
    #[serde(default = "default_skip_probability")]
    pub run_skip_probability: f64,
    #[serde(default = "default_delay_min")]
    pub delay_min_secs: u64,
    #[serde(default = "default_delay_max")]
    pub delay_max_secs: u64,
    /// Follows are spaced much further apart than replies: never a burst.
    #[serde(default = "default_follow_delay_min")]
    pub follow_delay_min_secs: u64,
    #[serde(default = "default_follow_delay_max")]
    pub follow_delay_max_secs: u64,
}

fn default_skip_probability() -> f64 {
    0.35
}

fn default_delay_min() -> u64 {
    45
}

fn default_delay_max() -> u64 {
    240
}

fn default_follow_delay_min() -> u64 {
    600
}

fn default_follow_delay_max() -> u64 {
    1800
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            run_skip_probability: default_skip_probability(),
            delay_min_secs: default_delay_min(),
            delay_max_secs: default_delay_max(),
            follow_delay_min_secs: default_follow_delay_min(),
            follow_delay_max_secs: default_follow_delay_max(),
        }
    }
}

/// Accounts of interest the owner curates for this persona.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Interests {
    /// Curated handles (Linux / FOSS / AI). Resolved to DIDs only when a follow
    /// is actually planned, and only ever followed slowly.
    #[serde(default)]
    pub accounts: Vec<String>,
}

impl EngageConfig {
    /// Load the configuration from a YAML file.
    pub async fn load(path: &Path) -> Result<Self> {
        let raw = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading engagement config {}", path.display()))?;
        Self::parse_yaml(&raw)
    }

    /// Parse the configuration from a YAML string.
    pub fn parse_yaml(raw: &str) -> Result<Self> {
        serde_yaml_ng::from_str(raw).context("parsing engagement config")
    }

    /// Whether the kill switch is currently engaged.
    pub fn kill_switch_active(&self) -> bool {
        std::env::var(&self.kill_switch_env).is_ok_and(|value| is_truthy(&value))
    }

    /// Minute of the day (UTC) for a Unix timestamp.
    pub fn minute_of_day(now_unix: u64) -> u32 {
        ((now_unix % 86_400) / 60) as u32
    }

    /// Whether `now_unix` falls inside one of the configured windows.
    pub fn in_window(&self, now_unix: u64) -> bool {
        let minute = Self::minute_of_day(now_unix);
        self.windows.iter().any(|window| {
            parse_clock(window).is_some_and(|start| {
                let offset = (minute + 1_440 - start) % 1_440;
                offset < self.window_minutes
            })
        })
    }

    /// Reject a configuration that would make the pacing unsafe or nonsensical.
    pub fn validate(&self) -> Result<()> {
        if self.windows.is_empty() {
            bail!("engagement config needs at least one execution window");
        }
        for window in &self.windows {
            if parse_clock(window).is_none() {
                bail!("`{window}` is not a valid 24h clock window (expected HH:MM)");
            }
        }
        if self.window_minutes == 0 {
            bail!("window_minutes must be greater than zero");
        }
        if self.caps.max_actions_per_run == 0 {
            bail!("caps.max_actions_per_run must be at least 1");
        }
        if !(0.0..=1.0).contains(&self.pacing.run_skip_probability) {
            bail!("pacing.run_skip_probability must be within 0.0..=1.0");
        }
        if self.pacing.delay_min_secs > self.pacing.delay_max_secs {
            bail!("pacing.delay_min_secs must not exceed delay_max_secs");
        }
        if self.pacing.follow_delay_min_secs > self.pacing.follow_delay_max_secs {
            bail!("pacing.follow_delay_min_secs must not exceed follow_delay_max_secs");
        }
        Ok(())
    }
}

/// Truthy strings for the kill switch (case-insensitive).
pub fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "enabled"
    )
}

/// Parse a `HH:MM` 24h clock into minutes since midnight.
pub fn parse_clock(value: &str) -> Option<u32> {
    let (hours, minutes) = value.trim().split_once(':')?;
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    if hours < 24 && minutes < 60 {
        Some(hours * 60 + minutes)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_validate() {
        let config = EngageConfig::parse_yaml("{}").expect("empty config parses to defaults");
        config.validate().expect("defaults are valid");
        assert_eq!(config.caps.max_actions_per_run, 6);
        assert_eq!(config.caps.follows_per_week, 3);
        assert!(config.pacing.run_skip_probability > 0.0);
    }

    #[test]
    fn windows_are_separate_and_matched_by_offset() {
        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        // 09:40 UTC is inside the 09:30 window (45 minutes long).
        let at_0940 = 9 * 3600 + 40 * 60;
        assert!(config.in_window(at_0940));
        // 09:20 is before the window and 10:30 is after it.
        assert!(!config.in_window(9 * 3600 + 20 * 60));
        assert!(!config.in_window(10 * 3600 + 30 * 60));
    }

    #[test]
    fn a_window_wrapping_midnight_is_matched() {
        let config = EngageConfig::parse_yaml("windows: [\"23:50\"]\nwindow_minutes: 30\n")
            .expect("config parses");
        assert!(config.in_window(23 * 3600 + 55 * 60));
        assert!(config.in_window(10 * 60)); // 00:10 next day
        assert!(!config.in_window(12 * 3600));
    }

    #[test]
    fn invalid_clock_is_rejected() {
        let config =
            EngageConfig::parse_yaml("windows: [\"9h30\"]").expect("still parses as a string");
        assert!(config.validate().is_err());
        assert!(parse_clock("24:00").is_none());
        assert!(parse_clock("12:60").is_none());
        assert_eq!(parse_clock("09:30"), Some(9 * 60 + 30));
    }

    #[test]
    fn inconsistent_pacing_is_rejected() {
        let config = EngageConfig::parse_yaml("pacing: {delay_min_secs: 300, delay_max_secs: 10}")
            .expect("parses");
        assert!(config.validate().is_err());
    }

    #[test]
    fn kill_switch_truthiness() {
        assert!(is_truthy("1"));
        assert!(is_truthy("TRUE"));
        assert!(is_truthy(" on "));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
    }
}

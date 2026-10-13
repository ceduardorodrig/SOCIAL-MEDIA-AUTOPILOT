//! Engagement configuration: caps, pacing and execution windows.
//!
//! This is **data**, never code, and carries no secret (only the *name* of the
//! kill-switch environment variable). The engagement windows are deliberately
//! separate from the persona's publishing windows, so interactions never ride a
//! publishing burst.
//!
//! The windows are **local** wall-clock times (`HH:MM`), the same clock the
//! systemd timer fires on and the publishing stage uses. The comparison is done
//! with [`autopilot_core::local_minute_of_day`]; never mix a UTC minute into a
//! local window (HB-058).

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
    /// Execution windows (`HH:MM`, **local** 24h clock), separate from the
    /// publishing windows. The systemd timer fires on the same local clock.
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
    /// Proactive commenting on the curated accounts of interest.
    #[serde(default)]
    pub proactive: Proactive,
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

/// Proactive commenting: joining the conversation on curated accounts.
///
/// This is the growth mechanism the owner asked for, and the most dangerous one
/// for a bot's reputation. Every default here is deliberately tight: few
/// replies, one per account per week, a long minimum gap and a strict quality
/// bar. It never advertises: see `quality.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proactive {
    /// Master switch for the whole proactive source.
    #[serde(default = "default_proactive_enabled")]
    pub enabled: bool,
    /// How many curated accounts to probe per run.
    #[serde(default = "default_proactive_accounts")]
    pub accounts_per_run: usize,
    /// How many recent posts to read per account.
    #[serde(default = "default_proactive_posts")]
    pub posts_per_account: usize,
    /// Skip posts older than this (the conversation is already cold).
    #[serde(default = "default_proactive_max_age_hours")]
    pub max_post_age_hours: u64,
    /// Candidates kept per account in a single gather (never a wall of posts).
    #[serde(default = "default_proactive_candidates")]
    pub candidates_per_account_per_run: usize,
    /// Global ceiling on proactive replies per day, on top of `caps.replies_per_day`.
    #[serde(default = "default_proactive_replies_per_day")]
    pub replies_per_day: u32,
    /// Hard weekly ceiling per account: never become that account's reply-guy.
    #[serde(default = "default_proactive_replies_per_account_week")]
    pub replies_per_account_per_week: u32,
    /// Minimum hours before replying to the same account again.
    #[serde(default = "default_proactive_min_gap_hours")]
    pub min_gap_hours: u64,
    /// Quality bar: minimum characters of a proactive reply.
    #[serde(default = "default_quality_min_chars")]
    pub min_chars: usize,
    /// Quality bar: minimum words of a proactive reply.
    #[serde(default = "default_quality_min_words")]
    pub min_words: usize,
}

fn default_proactive_enabled() -> bool {
    true
}

fn default_proactive_accounts() -> usize {
    5
}

fn default_proactive_posts() -> usize {
    5
}

fn default_proactive_max_age_hours() -> u64 {
    36
}

fn default_proactive_candidates() -> usize {
    1
}

fn default_proactive_replies_per_day() -> u32 {
    2
}

fn default_proactive_replies_per_account_week() -> u32 {
    1
}

fn default_proactive_min_gap_hours() -> u64 {
    72
}

fn default_quality_min_chars() -> usize {
    40
}

fn default_quality_min_words() -> usize {
    8
}

impl Default for Proactive {
    fn default() -> Self {
        Self {
            enabled: default_proactive_enabled(),
            accounts_per_run: default_proactive_accounts(),
            posts_per_account: default_proactive_posts(),
            max_post_age_hours: default_proactive_max_age_hours(),
            candidates_per_account_per_run: default_proactive_candidates(),
            replies_per_day: default_proactive_replies_per_day(),
            replies_per_account_per_week: default_proactive_replies_per_account_week(),
            min_gap_hours: default_proactive_min_gap_hours(),
            min_chars: default_quality_min_chars(),
            min_words: default_quality_min_words(),
        }
    }
}

impl Proactive {
    /// The freshness cut-off in seconds.
    pub fn max_age_secs(&self) -> u64 {
        self.max_post_age_hours.saturating_mul(3_600)
    }

    /// The minimum gap before replying to the same account again, in seconds.
    pub fn min_gap_secs(&self) -> u64 {
        self.min_gap_hours.saturating_mul(3_600)
    }
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

    /// Whether `minute_of_day` falls inside one of the configured windows.
    ///
    /// `minute_of_day` is the **local** minute of the day (`0..=1439`) — the same
    /// clock the systemd timer fires on (`OnCalendar=09,13,20:35` resolves in the
    /// server timezone) and the publishing stage uses. The window list and
    /// `window_minutes` are local too; callers pass
    /// [`autopilot_core::local_minute_of_day`].
    ///
    /// HB-058: the old code computed a **UTC** minute here and compared it against
    /// these local windows, so the 09:35 local fire was read as 12:35 UTC — 185
    /// minutes past the 09:30 window — and every scheduled engagement run ended
    /// `window : outside` with `0 action(s) executed`.
    pub fn in_window(&self, minute_of_day: u32) -> bool {
        self.windows.iter().any(|window| {
            parse_clock(window).is_some_and(|start| {
                let offset = (minute_of_day + 1_440 - start) % 1_440;
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
        if self.proactive.max_post_age_hours == 0 {
            bail!("proactive.max_post_age_hours must be greater than zero");
        }
        if self.proactive.candidates_per_account_per_run == 0 {
            bail!("proactive.candidates_per_account_per_run must be at least 1");
        }
        if self.proactive.min_chars == 0 || self.proactive.min_words == 0 {
            bail!("proactive quality thresholds must be greater than zero");
        }
        if self.proactive.replies_per_account_per_week == 0 && self.proactive.enabled {
            bail!("proactive is enabled but replies_per_account_per_week is 0");
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
        // 09:40 local is inside the 09:30 window (45 minutes long).
        assert!(config.in_window(9 * 60 + 40));
        // 09:20 is before the window and 10:30 is after it.
        assert!(!config.in_window(9 * 60 + 20));
        assert!(!config.in_window(10 * 60 + 30));
    }

    #[test]
    fn a_fire_inside_the_window_engages_and_one_outside_does_not() {
        // HB-058: the scheduler fires at 09:35 **local**, five minutes into the
        // 09:30 window — the case the old UTC arithmetic called "outside" (at
        // -03 that instant is 12:35 UTC, 185 minutes past the window). The
        // decision function is exercised for real, with the local minute the
        // live path computes. Reverting `in_window` to a UTC `now_unix` read
        // makes the first assertion fail (575 would be read as 00:09 UTC).
        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        assert!(config.in_window(9 * 60 + 35), "09:35 local must engage");
        assert!(config.in_window(9 * 60 + 40), "09:40 local must engage");
        assert!(!config.in_window(9 * 60 + 20), "09:20 local is before");
        assert!(!config.in_window(10 * 60 + 30), "10:30 local is after");
    }

    #[test]
    fn a_local_fire_is_judged_on_the_local_clock_not_utc() {
        // The conversion the runner uses: the 09:35 local fire arrives as the
        // 12:35 UTC instant, and `local_minute_of_day` (offset -03) must turn it
        // back into 575 — inside the window. A UTC read would give 755, outside.
        let at_1235_utc = 12 * 3600 + 35 * 60;
        let local = autopilot_core::minute_of_day_at(at_1235_utc, -3 * 3600);
        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        assert!(config.in_window(local));
        assert!(!config.in_window(autopilot_core::minute_of_day_at(at_1235_utc, 0)));
    }

    #[test]
    fn a_window_wrapping_midnight_is_matched() {
        let config = EngageConfig::parse_yaml("windows: [\"23:50\"]\nwindow_minutes: 30\n")
            .expect("config parses");
        assert!(config.in_window(23 * 60 + 55));
        assert!(config.in_window(10)); // 00:10 next day
        assert!(!config.in_window(12 * 60));
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
    fn proactive_defaults_are_conservative() {
        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        config.validate().expect("defaults are valid");
        assert!(config.proactive.enabled);
        assert_eq!(config.proactive.replies_per_day, 2);
        assert_eq!(config.proactive.replies_per_account_per_week, 1);
        assert_eq!(config.proactive.min_gap_secs(), 72 * 3_600);
        assert_eq!(config.proactive.max_age_secs(), 36 * 3_600);
    }

    #[test]
    fn proactive_quality_thresholds_are_validated() {
        let config = EngageConfig::parse_yaml("proactive: {min_chars: 0}").expect("still parses");
        assert!(config.validate().is_err());

        let config =
            EngageConfig::parse_yaml("proactive: {enabled: true, replies_per_account_per_week: 0}")
                .expect("still parses");
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

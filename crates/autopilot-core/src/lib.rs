//! Domain model for the Social Media Autopilot harness.
//!
//! The harness is a single engine; a **persona pack** is the data package that
//! configures it. One persona drives one or many platform channels, always
//! adapting the master content per channel.

pub mod persona;
pub mod store;
pub mod time;

pub use persona::{
    Channel, DEFAULT_PERSONAS_DIR, HarvestPolicy, PersonaPack, Platform, PublishingPolicy, Rubric,
    SelectionRule, ValidationIssue, Voice, humanize_niche, load_dir, load_persona,
    resolve_persona_path,
};
pub use time::now_unix;

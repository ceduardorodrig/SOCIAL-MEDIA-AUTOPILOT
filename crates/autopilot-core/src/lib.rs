//! Domain model for the Social Media Autopilot harness.
//!
//! The harness is a single engine; a **persona pack** is the data package that
//! configures it. One persona drives one or many platform channels, always
//! adapting the master content per channel.

pub mod persona;
pub mod store;
pub mod time;

pub use persona::{
    Channel, Palette, PersonaPack, Platform, Rubric, TextCase, Typography, ValidationIssue, Visual,
    Voice, is_hex_color, load_dir, load_persona, resolve_visual_template, visual_dir,
};
pub use time::now_unix;

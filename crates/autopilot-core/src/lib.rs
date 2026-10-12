//! Domain model for the Social Media Autopilot harness.
//!
//! The harness is a single engine; a **persona pack** is the data package that
//! configures it. One persona drives one or many platform channels, always
//! adapting the master content per channel.

pub mod persona;

pub use persona::{
    Channel, PersonaPack, Platform, Rubric, ValidationIssue, Voice, load_dir, load_persona,
};

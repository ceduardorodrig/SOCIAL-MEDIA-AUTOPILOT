//! Shared prompt fragments for the stage prompts.
//!
//! The creation ([3]) and the editorial review ([3.2]) prompts describe the same
//! persona and the same per-platform limit, so that rendering lives here once
//! (the repository enforces DRY across prompt builders).

use autopilot_core::{PersonaPack, Platform};

/// Render the persona's style guidelines as a bullet list (or `- none`).
pub fn render_guidelines(persona: &PersonaPack) -> String {
    if persona.voice.guidelines.is_empty() {
        return "- none".to_string();
    }
    persona
        .voice
        .guidelines
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the persona's banned phrases as one `;`-separated line (or `none`).
pub fn render_banned_phrases(persona: &PersonaPack) -> String {
    if persona.voice.banned_phrases.is_empty() {
        return "none".to_string();
    }
    persona.voice.banned_phrases.join("; ")
}

/// Conservative per-platform character limit, mirrored from the guardrail.
///
/// Kept in the prompt layer (instead of depending on `autopilot-guardrail`) so
/// the prompt builders stay free of the validation crate; the guardrail remains
/// the single authority that actually rejects an over-long draft.
pub fn platform_limit(platform: Platform) -> usize {
    match platform {
        Platform::Bluesky => 300,
        Platform::X => 280,
        Platform::Linkedin => 3_000,
        Platform::Instagram => 2_200,
        Platform::Tiktok => 2_200,
        Platform::Kwai => 2_200,
    }
}

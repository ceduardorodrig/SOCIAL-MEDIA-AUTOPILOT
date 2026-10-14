//! Shared test fixtures for the pipeline stage modules.
//!
//! Every stage module needs a valid persona pack to build its prompt. The
//! fixture lives here once; a test overrides only the fields it cares about
//! (the repository enforces DRY across test code too).

use autopilot_core::{
    Channel, HarvestPolicy, PersonaPack, Platform, PublishingPolicy, Rubric, Voice,
};

/// A minimal, valid persona pack for the stage tests.
pub(crate) fn sample_persona() -> PersonaPack {
    PersonaPack {
        persona: "ciencia".into(),
        display_name: "Ciência & Curiosidades".into(),
        niche: "science-curiosities".into(),
        language: "pt-BR".into(),
        active: false,
        voice: Voice {
            tone: "curioso".into(),
            audience: "adultos curiosos".into(),
            guidelines: vec![],
            banned_phrases: vec![],
            max_hashtags: 4,
            emojis_allowed: true,
        },
        few_shot: vec![],
        blocklist: vec!["terraplanismo".into()],
        rubric: Rubric {
            min_score: 0.75,
            criteria: vec![],
        },
        channels: vec![Channel {
            platform: Platform::Bluesky,
            handle: "x".into(),
            enabled: true,
            posts_per_day: 1,
            windows: vec![],
        }],
        publishing: PublishingPolicy::default(),
        harvest: HarvestPolicy::default(),
        visual: None,
    }
}

/// The same fixture with the voice rules the creation and review tests assert.
pub(crate) fn sample_persona_with_voice() -> PersonaPack {
    let mut persona = sample_persona();
    persona.voice.guidelines = vec!["comece com um gancho".into()];
    persona.voice.banned_phrases = vec!["você não vai acreditar".into()];
    persona.voice.max_hashtags = 3;
    persona
}

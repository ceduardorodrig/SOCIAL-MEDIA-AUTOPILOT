//! Deterministic guardrail: brand safety and format validation before publish.
//!
//! Safety is **not** delegated to an LLM. This stage is pure Rust and
//! deterministic: the same draft always produces the same verdict, which is
//! what makes an autonomous loop auditable.

use autopilot_core::{PersonaPack, Platform};
use serde::{Deserialize, Serialize};

/// Conservative per-platform character limits.
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

/// A single rule violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub rule: String,
    pub detail: String,
}

/// The guardrail verdict for one draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub approved: bool,
    pub issues: Vec<Issue>,
}

impl Verdict {
    /// Start from an approved verdict.
    pub fn new() -> Self {
        Self {
            approved: true,
            issues: Vec::new(),
        }
    }

    fn reject(&mut self, rule: &str, detail: impl Into<String>) {
        self.issues.push(Issue {
            rule: rule.to_string(),
            detail: detail.into(),
        });
        self.approved = false;
    }
}

impl Default for Verdict {
    fn default() -> Self {
        Self::new()
    }
}

/// Evaluate a draft against the persona pack and the target platform.
pub fn evaluate(persona: &PersonaPack, platform: Platform, text: &str) -> Verdict {
    let mut verdict = Verdict::new();
    let haystack = normalize(text);

    for term in &persona.blocklist {
        let needle = normalize(term);
        if !needle.trim().is_empty() && haystack.contains(needle.trim()) {
            verdict.reject("GUARD-BLOCKLIST", format!("blocked term present: {term}"));
        }
    }

    for phrase in &persona.voice.banned_phrases {
        let needle = normalize(phrase);
        if !needle.trim().is_empty() && haystack.contains(needle.trim()) {
            verdict.reject(
                "GUARD-BANNED-PHRASE",
                format!("banned phrase present: {phrase}"),
            );
        }
    }

    let hashtags = count_hashtags(text);
    if hashtags > persona.voice.max_hashtags as usize {
        verdict.reject(
            "GUARD-HASHTAGS",
            format!(
                "{hashtags} hashtags exceed the limit of {}",
                persona.voice.max_hashtags
            ),
        );
    }

    let length = text.chars().count();
    let limit = platform_limit(platform);
    if length > limit {
        verdict.reject(
            "GUARD-LENGTH",
            format!(
                "{length} characters exceed the {limit}-character limit for {}",
                platform.as_str()
            ),
        );
    }

    if !persona.voice.emojis_allowed && contains_emoji(text) {
        verdict.reject("GUARD-EMOJI", "emojis are not allowed for this persona");
    }

    verdict
}

fn count_hashtags(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.starts_with('#'))
        .count()
}

fn contains_emoji(text: &str) -> bool {
    text.chars().any(|c| {
        let cp = c as u32;
        (0x1F000..=0x1FAFF).contains(&cp)
            || (0x2600..=0x27BF).contains(&cp)
            || (0x2B00..=0x2BFF).contains(&cp)
            || (0xFE00..=0xFE0F).contains(&cp)
    })
}

/// Lowercase and fold pt-BR accents so matching is robust for Portuguese copy.
fn normalize(input: &str) -> String {
    input
        .chars()
        .flat_map(char::to_lowercase)
        .map(fold_accent)
        .collect()
}

fn fold_accent(character: char) -> char {
    match character {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopilot_core::{Channel, HarvestPolicy, PublishingPolicy, Rubric, Voice};

    fn persona() -> PersonaPack {
        PersonaPack {
            persona: "test".into(),
            display_name: "Test".into(),
            niche: "science".into(),
            language: "pt-BR".into(),
            active: false,
            voice: Voice {
                tone: "curious".into(),
                audience: "adults".into(),
                guidelines: vec![],
                banned_phrases: vec!["você não vai acreditar".into()],
                max_hashtags: 3,
                emojis_allowed: true,
            },
            few_shot: vec![],
            blocklist: vec!["terraplanismo".into(), "discurso de ódio".into()],
            rubric: Rubric {
                min_score: 0.75,
                criteria: vec![],
            },
            channels: vec![Channel {
                platform: Platform::Bluesky,
                handle: "x".into(),
                app_password_env: None,
                x_env: None,
                enabled: true,
                posts_per_day: 1,
                windows: vec![],
            }],
            publishing: PublishingPolicy::default(),
            harvest: HarvestPolicy::default(),
            visual: None,
        }
    }

    #[test]
    fn clean_draft_is_approved() {
        let verdict = evaluate(
            &persona(),
            Platform::Bluesky,
            "O cérebro gasta 20% da energia do corpo. Você sabia? #ciencia",
        );
        assert!(verdict.approved, "issues: {:?}", verdict.issues);
    }

    #[test]
    fn blocklist_match_is_rejected_case_and_accent_insensitive() {
        let verdict = evaluate(
            &persona(),
            Platform::Bluesky,
            "Sobre o TERRAPLANISMO e a ciência moderna...",
        );
        assert!(!verdict.approved);
        assert!(verdict.issues.iter().any(|i| i.rule == "GUARD-BLOCKLIST"));
    }

    #[test]
    fn banned_phrase_is_rejected() {
        let verdict = evaluate(
            &persona(),
            Platform::Bluesky,
            "Você não vai acreditar no que a ciência descobriu.",
        );
        assert!(
            verdict
                .issues
                .iter()
                .any(|i| i.rule == "GUARD-BANNED-PHRASE")
        );
    }

    #[test]
    fn too_many_hashtags_is_rejected() {
        let verdict = evaluate(&persona(), Platform::Bluesky, "#a #b #c #d");
        assert!(verdict.issues.iter().any(|i| i.rule == "GUARD-HASHTAGS"));
    }

    #[test]
    fn over_platform_limit_is_rejected() {
        let long = "a".repeat(301);
        let verdict = evaluate(&persona(), Platform::Bluesky, &long);
        assert!(verdict.issues.iter().any(|i| i.rule == "GUARD-LENGTH"));
    }

    #[test]
    fn emoji_rejected_when_not_allowed() {
        let mut pack = persona();
        pack.voice.emojis_allowed = false;
        let verdict = evaluate(&pack, Platform::Bluesky, "Boa notícia 🚀 para a ciência");
        assert!(verdict.issues.iter().any(|i| i.rule == "GUARD-EMOJI"));
    }

    #[test]
    fn accumulated_issues_keep_all_reasons() {
        let verdict = evaluate(
            &persona(),
            Platform::X,
            "Você não vai acreditar no terraplanismo #a #b #c #d",
        );
        assert!(!verdict.approved);
        assert!(verdict.issues.len() >= 3);
    }
}

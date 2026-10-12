//! Stage [3] — creation: draft A/B variants adapted to one platform.

use anyhow::{Context, Result};
use autopilot_core::{PersonaPack, Platform};
use serde::{Deserialize, Serialize};

use crate::json::extract_json;

/// One content variant (A or B) for a platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub label: String,
    pub text: String,
    #[serde(default)]
    pub hashtags: Vec<String>,
    #[serde(default)]
    pub image_prompt: String,
}

#[derive(Debug, Deserialize)]
struct Wrapper {
    variants: Vec<Variant>,
}

/// Build the creation prompt for one platform and topic.
pub fn build_creation_prompt(persona: &PersonaPack, platform: Platform, topic: &str) -> String {
    let guidelines = if persona.voice.guidelines.is_empty() {
        "- none".to_string()
    } else {
        persona
            .voice
            .guidelines
            .iter()
            .map(|rule| format!("- {rule}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let banned = if persona.voice.banned_phrases.is_empty() {
        "none".to_string()
    } else {
        persona.voice.banned_phrases.join("; ")
    };

    format!(
        "You are the creation stage of an autonomous social-media engine.\n\
         Write in {language} for the platform `{platform}`.\n\n\
         Persona: {display_name} ({niche})\n\
         Tone: {tone}\n\
         Audience: {audience}\n\
         Style rules:\n{guidelines}\n\
         Never use these phrases: {banned}\n\
         Never touch these topics: {blocklist}\n\n\
         Topic to write about: {topic}\n\n\
         Produce exactly 2 alternative variants (A and B) that could be A/B tested.\n\
         Each caption must respect the platform limit of {limit} characters and use at most\n\
         {max_tags} hashtags. Also propose a short image prompt for each variant.\n\n\
         Reply with ONLY a JSON array (no prose, no markdown) of objects with the keys\n\
         \"label\" (\"A\" or \"B\"), \"text\", \"hashtags\" (array of strings) and \"image_prompt\".\n",
        language = persona.language,
        platform = platform.as_str(),
        display_name = persona.display_name,
        niche = persona.niche,
        tone = persona.voice.tone,
        audience = persona.voice.audience,
        guidelines = guidelines,
        banned = banned,
        blocklist = persona.blocklist.join("; "),
        topic = topic,
        limit = autopilot_guardrail_limit(platform),
        max_tags = persona.voice.max_hashtags,
    )
}

/// Character limit mirrored from the guardrail, kept local to avoid a dependency cycle.
fn autopilot_guardrail_limit(platform: Platform) -> usize {
    match platform {
        Platform::Bluesky => 300,
        Platform::X => 280,
        Platform::Linkedin => 3_000,
        Platform::Instagram => 2_200,
        Platform::Tiktok => 2_200,
        Platform::Kwai => 2_200,
    }
}

/// Parse the model response into variants.
pub fn parse_variants(raw: &str) -> Result<Vec<Variant>> {
    let json = extract_json(raw)?;
    let variants: Vec<Variant> = match serde_json::from_str(json) {
        Ok(variants) => variants,
        Err(_) => {
            let wrapper: Wrapper = serde_json::from_str(json)
                .context("invalid variants JSON (expected an array or {\"variants\": [...]})")?;
            wrapper.variants
        }
    };

    if variants.is_empty() {
        anyhow::bail!("the model returned no variants");
    }
    Ok(variants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopilot_core::{Channel, Rubric, Voice};

    fn persona() -> PersonaPack {
        PersonaPack {
            persona: "ciencia".into(),
            display_name: "Ciência & Curiosidades".into(),
            niche: "science-curiosities".into(),
            language: "pt-BR".into(),
            active: false,
            voice: Voice {
                tone: "curioso".into(),
                audience: "adultos".into(),
                guidelines: vec!["comece com um gancho".into()],
                banned_phrases: vec!["você não vai acreditar".into()],
                max_hashtags: 3,
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
        }
    }

    #[test]
    fn prompt_carries_constraints() {
        let prompt =
            build_creation_prompt(&persona(), Platform::Bluesky, "o cérebro gasta energia");
        assert!(prompt.contains("pt-BR"));
        assert!(prompt.contains("bluesky"));
        assert!(prompt.contains("300"));
        assert!(prompt.contains("você não vai acreditar"));
        assert!(prompt.contains("comece com um gancho"));
    }

    #[test]
    fn parses_fenced_variants() {
        let raw = "```json\n[{\"label\":\"A\",\"text\":\"t\",\"hashtags\":[\"#a\"],\
                   \"image_prompt\":\"p\"},{\"label\":\"B\",\"text\":\"u\"}]\n```";
        let variants = parse_variants(raw).expect("parses");
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].label, "A");
        assert_eq!(variants[0].hashtags, vec!["#a".to_string()]);
        assert!(variants[1].image_prompt.is_empty());
    }

    #[test]
    fn empty_variant_list_is_an_error() {
        assert!(parse_variants("[]").is_err());
    }
}

//! Stage [3] — creation: draft A/B variants adapted to one platform.

use anyhow::{Context, Result};
use autopilot_core::{PersonaPack, Platform};
use serde::{Deserialize, Serialize};

use crate::grounding::Source;
use crate::json::extract_json;
use crate::prompt::{platform_limit, render_banned_phrases, render_guidelines};

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
pub fn build_creation_prompt(
    persona: &PersonaPack,
    platform: Platform,
    topic: &str,
    source: Option<Source<'_>>,
) -> String {
    let guidelines = render_guidelines(persona);
    let banned = render_banned_phrases(persona);

    // The harvest already read the page: that text — not the model's memory — is
    // the only thing a post may cite. Without it, say so instead of inviting an
    // invention.
    let grounding = match source {
        Some(source) => format!(
            "The ONLY admissible source (url: {url}):\n\"{content}\"\n\n\
             Ground every claim in that source. Never add a number, date, version, name,\n\
             price, quote or comparison it does not contain: a wrong detail is worse than a\n\
             plainer sentence.",
            url = source.url,
            content = source.content,
        ),
        None => "No source text is available: state only what the topic itself supports, and\n\
                 never invent a specific detail."
            .to_string(),
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
         {grounding}\n\n\
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
        grounding = grounding,
        limit = platform_limit(platform),
        max_tags = persona.voice.max_hashtags,
    )
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

    fn persona() -> PersonaPack {
        crate::test_support::sample_persona_with_voice()
    }

    #[test]
    fn prompt_carries_constraints() {
        let prompt = build_creation_prompt(
            &persona(),
            Platform::Bluesky,
            "o cérebro gasta energia",
            None,
        );
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

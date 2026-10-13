//! Stage [3] — creation: draft A/B variants adapted to one platform.

use anyhow::{Context, Result};
use autopilot_core::{PersonaPack, Platform};
use autopilot_media::VideoPlan;
use serde::{Deserialize, Serialize};

use crate::grounding::Source;
use crate::json::extract_json;
use crate::prompt::{platform_limit, render_banned_phrases, render_guidelines};

/// The information a post's card carries.
///
/// The card is **not** the post text: the post *introduces* and comments, the
/// card *states* the information (a number, a name, a concrete detail from the
/// source). Rendering the post text into the image — which is what happened
/// until 2026-10-13 — makes the picture repeat the post.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CardContent {
    /// Headline for the card. Short: the card wraps it over two lines.
    pub title: String,
    /// The fact the card states, taken from the source.
    pub body: String,
}

impl CardContent {
    /// True when the card has something to show.
    pub fn is_usable(&self) -> bool {
        !self.title.trim().is_empty() && !self.body.trim().is_empty()
    }
}

/// One content variant (A or B) for a platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub label: String,
    pub text: String,
    #[serde(default)]
    pub hashtags: Vec<String>,
    #[serde(default)]
    pub image_prompt: String,
    /// What the card (if one is attached) should say. Absent for a text-only
    /// post, and absent in older responses — hence the default.
    #[serde(default)]
    pub card: Option<CardContent>,
    /// A short-video plan, present only when the topic's angle suits a video and
    /// the persona's policy releases the format (HB-035). Absent in older
    /// responses and in every text/card response.
    #[serde(default)]
    pub video_plan: Option<VideoPlan>,
}

#[derive(Debug, Deserialize)]
struct Wrapper {
    variants: Vec<Variant>,
}

/// Build the creation prompt for one platform and topic.
///
/// No avoid memory: the plain creation path. Use
/// [`build_creation_prompt_with_avoid`] when the learning stage has warnings.
pub fn build_creation_prompt(
    persona: &PersonaPack,
    platform: Platform,
    topic: &str,
    source: Option<Source<'_>>,
) -> String {
    build_creation_prompt_with_avoid(persona, platform, topic, source, &[])
}

/// Build the creation prompt, warned off claims the grounding review refused.
///
/// `avoid` carries the learning stage's "avoid" memory (HB-046): claims the
/// [3.7] reviewer refused because the source did not support them, plus the
/// shape/openings the craft score penalised. The creation stage is told never
/// to repeat them, so the same unsupported claim is not rediscovered every slot.
pub fn build_creation_prompt_with_avoid(
    persona: &PersonaPack,
    platform: Platform,
    topic: &str,
    source: Option<Source<'_>>,
    avoid: &[String],
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

    // The [3.7] reviewer's refusals are learned, not just gated: a claim the
    // source did not support must not be rediscovered on the next slot.
    let avoid_block = if avoid.is_empty() {
        String::new()
    } else {
        format!(
            "\nClaims and shapes a reviewer already refused — never repeat them, or anything\n\
             like them (each was either unsupported by a source or penalised as slop):\n{}\n",
            avoid
                .iter()
                .map(|claim| format!("- {claim}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    // HB-035: a short-video plan is asked for only when the policy budgets one,
    // so a text/card-only pack keeps a byte-identical prompt (video defaults to
    // a zero budget).
    let video_block = if persona.publishing.formats.video > 0 {
        crate::video::VIDEO_PLAN_INSTRUCTION.to_string()
    } else {
        String::new()
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
         {grounding}\n\
         {avoid_block}\n\
         Produce exactly 2 alternative variants (A and B) that could be A/B tested.\n\
         Each caption must respect the platform limit of {limit} characters and use at most\n\
         {max_tags} hashtags. Also propose a short image prompt for each variant.\n\n\
         Each variant also carries a \"card\": the information the image will state — a short\n\
         headline plus one concrete fact from the source (a number, a name, a detail). The\n\
         caption is an INTRODUCTION to that card — your angle, why it matters — and never a\n\
         restatement of it: the reader sees both, so they must not say the same thing.\n\n\
         {video_block}Reply with ONLY a JSON array (no prose, no markdown) of objects with the keys\n\
         \"label\" (\"A\" or \"B\"), \"text\", \"hashtags\" (array of strings), \"image_prompt\",\n\
         and \"card\" (an object with \"title\" and \"body\").\n",
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
        avoid_block = avoid_block,
        video_block = video_block,
        limit = platform_limit(platform),
        max_tags = persona.voice.max_hashtags,
    )
}

/// Parse the model response into variants.
pub fn parse_variants(raw: &str) -> Result<Vec<Variant>> {
    let json = extract_json(raw)?;
    let mut variants: Vec<Variant> = match serde_json::from_str(json) {
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
    // HB-035: a `video_plan` that cannot be rendered is dropped here (the caption
    // and its card survive), so a malformed plan never costs the slot.
    crate::video::drop_invalid_video_plans(&mut variants);
    Ok(variants)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_parses_a_card_and_tolerates_its_absence() {
        // The card is what the image states; an older response without one must
        // still parse (hence `#[serde(default)]` on the field).
        let raw = r#"[{"label":"A","text":"t","card":{"title":"7 GB","body":"mostly models"}},
                       {"label":"B","text":"u"}]"#;
        let variants = parse_variants(raw).expect("parses");
        let card = variants[0].card.as_ref().expect("A carries a card");
        assert_eq!(card.title, "7 GB");
        assert_eq!(card.body, "mostly models");
        assert!(card.is_usable());
        assert!(variants[1].card.is_none());
    }

    #[test]
    fn an_empty_card_is_not_usable() {
        let card = CardContent {
            title: "  ".into(),
            body: "fact".into(),
        };
        assert!(!card.is_usable());
    }

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

    #[test]
    fn a_video_plan_is_parsed_and_a_broken_one_is_dropped() {
        // HB-035: the creation may carry a `video_plan`; one that cannot render
        // is dropped here so its caption still ships (as a card or text).
        let raw = r#"[{"label":"A","text":"t","video_plan":{"hook":"oi","scenes":[{"narration":"n","image_prompt":"p"}]}},
                       {"label":"B","text":"u","video_plan":{"hook":"x","scenes":[]}}]"#;
        let variants = parse_variants(raw).expect("parses");
        assert!(variants[0].video_plan.is_some());
        assert!(
            variants[1].video_plan.is_none(),
            "the scene-less plan is dropped, the caption kept"
        );
        assert_eq!(variants[1].text, "u");
    }

    #[test]
    fn the_video_instruction_appears_only_when_the_policy_budgets_video() {
        let mut pack = persona();
        let plain = build_creation_prompt(&pack, Platform::Bluesky, "a topic", None);
        assert!(
            !plain.contains("video_plan"),
            "a zero video budget asks for no plan"
        );

        pack.publishing.formats.video = 1;
        let with_video = build_creation_prompt(&pack, Platform::Bluesky, "a topic", None);
        assert!(with_video.contains("video_plan"));
        assert!(with_video.contains("\"scenes\""));
    }

    #[test]
    fn the_prompt_warns_off_claims_the_reviewer_refused() {
        let avoid = vec![
            "grounding-rejected: Claims the agent is 'inspectable, not someone's cloud API' — \
             the source never says whether KylinBot/Xiao K is local/inspectable or cloud-based."
                .to_string(),
        ];
        let prompt = build_creation_prompt_with_avoid(
            &persona(),
            Platform::Bluesky,
            "o cérebro gasta energia",
            None,
            &avoid,
        );
        assert!(prompt.contains("never repeat them"));
        assert!(prompt.contains("inspectable, not someone's cloud API"));
    }

    #[test]
    fn the_plain_prompt_carries_no_avoid_block() {
        let prompt = build_creation_prompt(
            &persona(),
            Platform::Bluesky,
            "o cérebro gasta energia",
            None,
        );
        assert!(!prompt.contains("never repeat them"));
        // The original constraint block is untouched.
        assert!(prompt.contains("Produce exactly 2 alternative variants"));
    }
}

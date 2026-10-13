//! Short-video plan production for the creation stage (HB-035).
//!
//! The creation stage decides, per topic, whether the angle suits a short video.
//! When the persona's format policy releases video (`publishing.formats.video`
//! above zero), the creation prompt asks for a `video_plan` alongside the
//! caption, and this module owns both halves of that contract: the prompt block
//! and the parser.
//!
//! The plan model itself lives in `autopilot-media` — the crate that renders it —
//! so there is exactly one definition, never a copy.

use anyhow::Result;
use autopilot_media::VideoPlan;

use crate::create::Variant;

/// The creation-prompt block that asks for a short-video plan.
///
/// It is additive: when the persona's policy budgets no video, the block is not
/// appended at all, so a text/card-only prompt is byte-identical to before.
pub const VIDEO_PLAN_INSTRUCTION: &str = "\
If the topic's angle is a story, a demo or a sequence of steps, also add a\n\
\"video_plan\" for a short vertical video: an object with \"hook\" (one spoken\n\
line), \"scenes\" (an array of {\"narration\", \"image_prompt\"} — one spoken\n\
sentence and one still per scene) and \"call_to_action\" (one spoken line).\n\
Omit \"video_plan\" for a topic that reads better as text or a card.\n";

/// Parse a standalone video plan from a model response.
///
/// Delegates to the media model's tolerant parser (`extract_json` + validation),
/// so a fenced or prose-wrapped plan is accepted and an unrenderable one is
/// rejected.
pub fn parse_video_plan(raw: &str) -> Result<VideoPlan> {
    VideoPlan::parse(raw)
}

/// Drop any `video_plan` that cannot be rendered, keeping the variant's caption.
///
/// A malformed plan must never cost the slot: with the plan gone the format
/// falls back to the caption's own signal (card or text). Returns how many plans
/// were dropped, so a caller can observe it.
pub fn drop_invalid_video_plans(variants: &mut [Variant]) -> usize {
    let mut dropped = 0;
    for variant in variants.iter_mut() {
        if variant
            .video_plan
            .as_ref()
            .is_some_and(|plan| plan.validate().is_err())
        {
            variant.video_plan = None;
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant() -> Variant {
        Variant {
            label: "A".into(),
            text: "a caption".into(),
            hashtags: vec![],
            image_prompt: String::new(),
            card: None,
            video_plan: None,
        }
    }

    #[test]
    fn parses_a_prose_wrapped_video_plan() {
        let raw = "Here is the plan:\n```json\n{\"video_plan\": {\"hook\": \"oi\", \
                   \"scenes\": [{\"narration\": \"uma cena\", \"image_prompt\": \"um cerebro\"}], \
                   \"cta\": \"siga\"}}\n```";
        let plan = parse_video_plan(raw).expect("plan parses");
        assert_eq!(plan.hook, "oi");
        assert_eq!(plan.scenes.len(), 1);
        assert_eq!(plan.call_to_action, "siga");
    }

    #[test]
    fn rejects_an_unrenderable_plan() {
        // No scenes: nothing to speak over.
        assert!(parse_video_plan("{\"scenes\":[]}").is_err());
    }

    #[test]
    fn an_invalid_plan_is_dropped_not_a_slot_lost() {
        let mut variants = vec![
            Variant {
                video_plan: Some(VideoPlan {
                    hook: "hook".into(),
                    scenes: vec![],
                    call_to_action: String::new(),
                }),
                ..variant()
            },
            Variant {
                video_plan: Some(VideoPlan {
                    hook: String::new(),
                    scenes: vec![autopilot_media::Scene {
                        narration: "uma cena".into(),
                        image_prompt: "um cerebro".into(),
                    }],
                    call_to_action: String::new(),
                }),
                ..variant()
            },
        ];

        let dropped = drop_invalid_video_plans(&mut variants);
        assert_eq!(dropped, 1, "only the scene-less plan is dropped");
        assert!(variants[0].video_plan.is_none());
        assert!(variants[1].video_plan.is_some());
        // The caption survives either way.
        assert_eq!(variants[0].text, "a caption");
    }
}

//! Short-video plan model (creation stage -> media rendering).
//!
//! The creation stage emits a structured plan for one vertical short video: a
//! spoken **hook**, an ordered list of **scenes** (each with a narration line and
//! a still-image prompt) and a spoken **call to action**.
//!
//! This module parses that plan tolerantly and exposes the ordered **beats** the
//! renderer consumes. A beat is one narrated unit: the hook and the call to
//! action reuse the first/last scene's still image, so the number of *images*
//! equals the number of scenes while every spoken line still gets its own audio.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::json::extract_json;

/// One narrated scene and the still-image prompt it is spoken over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scene {
    pub narration: String,
    #[serde(default)]
    pub image_prompt: String,
}

/// A structured short-video plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPlan {
    #[serde(default)]
    pub hook: String,
    pub scenes: Vec<Scene>,
    #[serde(default, alias = "cta")]
    pub call_to_action: String,
}

/// The role of a beat in the final video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatRole {
    Hook,
    Scene,
    CallToAction,
}

impl BeatRole {
    /// Lowercase label used in logs and CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            BeatRole::Hook => "hook",
            BeatRole::Scene => "scene",
            BeatRole::CallToAction => "cta",
        }
    }
}

/// One renderable unit: a narration line and the still image it is spoken over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beat {
    pub role: BeatRole,
    /// Index into [`VideoPlan::scenes`] whose image this beat is spoken over.
    pub scene_index: usize,
    pub narration: String,
    pub image_prompt: String,
}

/// Tolerated top-level wrapper, e.g. `{"video_plan": {...}}` or `{"plan": {...}}`.
#[derive(Debug, Deserialize)]
struct PlanWrapper {
    #[serde(alias = "plan")]
    video_plan: VideoPlan,
}

impl VideoPlan {
    /// Parse a model response into a validated plan.
    pub fn parse(raw: &str) -> Result<Self> {
        let json = extract_json(raw)?;
        let plan = match serde_json::from_str::<VideoPlan>(json) {
            Ok(plan) => plan,
            Err(direct) => serde_json::from_str::<PlanWrapper>(json)
                .map(|wrapper| wrapper.video_plan)
                .with_context(|| format!("invalid video plan JSON: {direct}"))?,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Reject plans that cannot be rendered.
    pub fn validate(&self) -> Result<()> {
        if self.scenes.is_empty() {
            bail!("the video plan has no scenes");
        }
        for (index, scene) in self.scenes.iter().enumerate() {
            if scene.narration.trim().is_empty() {
                bail!("scene {index} has an empty narration");
            }
        }
        Ok(())
    }

    /// Expand the plan into the ordered beats the renderer consumes.
    ///
    /// Empty hook/CTA fields are skipped, so a minimal plan renders exactly its
    /// scenes. An invalid (empty) plan yields no beats; call [`Self::validate`]
    /// first to get a precise error.
    pub fn beats(&self) -> Vec<Beat> {
        let Some(first) = self.scenes.first() else {
            return Vec::new();
        };
        let last = self.scenes.last().unwrap_or(first);

        let mut beats = Vec::with_capacity(self.scenes.len() + 2);
        if !self.hook.trim().is_empty() {
            beats.push(Beat {
                role: BeatRole::Hook,
                scene_index: 0,
                narration: self.hook.trim().to_string(),
                image_prompt: first.image_prompt.clone(),
            });
        }
        for (index, scene) in self.scenes.iter().enumerate() {
            beats.push(Beat {
                role: BeatRole::Scene,
                scene_index: index,
                narration: scene.narration.trim().to_string(),
                image_prompt: scene.image_prompt.clone(),
            });
        }
        if !self.call_to_action.trim().is_empty() {
            beats.push(Beat {
                role: BeatRole::CallToAction,
                scene_index: self.scenes.len() - 1,
                narration: self.call_to_action.trim().to_string(),
                image_prompt: last.image_prompt.clone(),
            });
        }
        beats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = include_str!("../tests/fixtures/plan-basic.json");
    const FENCED: &str = include_str!("../tests/fixtures/plan-fenced.json");
    const WRAPPED: &str = include_str!("../tests/fixtures/plan-wrapped.json");
    const CTA_ALIAS: &str = include_str!("../tests/fixtures/plan-cta-alias.json");
    const EMPTY_SCENES: &str = include_str!("../tests/fixtures/plan-empty-scenes.json");

    #[test]
    fn parses_a_basic_plan() {
        let plan = VideoPlan::parse(BASIC).expect("basic plan");
        assert_eq!(plan.scenes.len(), 2);
        assert!(plan.hook.starts_with("Seu cérebro"));
        assert_eq!(plan.scenes[0].image_prompt, "a glowing brain at night");
        assert!(plan.call_to_action.contains("Siga"));
    }

    #[test]
    fn parses_a_fenced_plan_embedded_in_prose() {
        let plan = VideoPlan::parse(FENCED).expect("fenced plan");
        assert_eq!(plan.scenes.len(), 1);
    }

    #[test]
    fn parses_a_wrapped_plan() {
        let plan = VideoPlan::parse(WRAPPED).expect("wrapped plan");
        assert_eq!(plan.scenes.len(), 1);
    }

    #[test]
    fn accepts_the_cta_alias() {
        let plan = VideoPlan::parse(CTA_ALIAS).expect("cta alias");
        assert!(plan.call_to_action.contains("Compartilhe"));
        assert!(plan.hook.is_empty());
    }

    #[test]
    fn beats_order_hook_scenes_then_cta() {
        let plan = VideoPlan::parse(BASIC).expect("basic plan");
        let beats = plan.beats();
        assert_eq!(beats.len(), 4);
        assert_eq!(beats[0].role, BeatRole::Hook);
        assert_eq!(beats[0].scene_index, 0);
        assert_eq!(beats[1].role, BeatRole::Scene);
        assert_eq!(beats[2].role, BeatRole::Scene);
        assert_eq!(beats[3].role, BeatRole::CallToAction);
        assert_eq!(beats[3].scene_index, 1);
        assert_eq!(beats[3].image_prompt, plan.scenes[1].image_prompt);
    }

    #[test]
    fn skips_an_empty_hook() {
        let plan = VideoPlan::parse(CTA_ALIAS).expect("cta alias");
        let beats = plan.beats();
        assert_eq!(beats.len(), 3);
        assert_eq!(beats[0].role, BeatRole::Scene);
        assert_eq!(beats[1].role, BeatRole::Scene);
        assert_eq!(beats[2].role, BeatRole::CallToAction);
        assert_eq!(beats[2].scene_index, 1);
    }

    #[test]
    fn rejects_an_empty_scene_list() {
        assert!(VideoPlan::parse(EMPTY_SCENES).is_err());
    }

    #[test]
    fn rejects_an_empty_narration() {
        let raw = "{\"scenes\":[{\"narration\":\"  \",\"image_prompt\":\"x\"}]}";
        assert!(VideoPlan::parse(raw).is_err());
    }

    #[test]
    fn renders_exactly_the_scenes_without_hook_or_cta() {
        let raw = "{\"scenes\":[{\"narration\":\"a\"},{\"narration\":\"b\"}]}";
        let plan = VideoPlan::parse(raw).expect("minimal plan");
        let beats = plan.beats();
        assert_eq!(beats.len(), 2);
        assert!(beats.iter().all(|beat| beat.role == BeatRole::Scene));
    }

    #[test]
    fn beats_is_empty_for_an_empty_plan() {
        let plan = VideoPlan {
            hook: "x".into(),
            scenes: Vec::new(),
            call_to_action: "y".into(),
        };
        assert!(plan.beats().is_empty());
    }
}

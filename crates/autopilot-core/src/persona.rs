//! Persona pack: the configurable data package the harness loads per profile.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_language() -> String {
    "pt-BR".to_string()
}

fn default_max_hashtags() -> u8 {
    5
}

fn default_true() -> bool {
    true
}

fn default_min_score() -> f32 {
    0.75
}

/// A complete persona definition: identity, voice, guardrails and channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaPack {
    /// Stable slug, unique across the workspace (used as the primary key).
    pub persona: String,
    #[serde(default)]
    pub display_name: String,
    /// Content niche this persona covers.
    pub niche: String,
    #[serde(default = "default_language")]
    pub language: String,
    /// A draft pack (`active: false`) is validated but never executed by the engine.
    #[serde(default)]
    pub active: bool,
    pub voice: Voice,
    /// Reference posts the engine may use as few-shot examples.
    #[serde(default)]
    pub few_shot: Vec<String>,
    /// Topics/keywords that must never be produced (brand safety).
    #[serde(default)]
    pub blocklist: Vec<String>,
    pub rubric: Rubric,
    /// One persona drives one or many platform channels.
    pub channels: Vec<Channel>,
}

/// Tone of voice and writing constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Voice {
    pub tone: String,
    pub audience: String,
    #[serde(default)]
    pub guidelines: Vec<String>,
    /// Phrases that trigger an automatic reject before publishing.
    #[serde(default)]
    pub banned_phrases: Vec<String>,
    #[serde(default = "default_max_hashtags")]
    pub max_hashtags: u8,
    #[serde(default = "default_true")]
    pub emojis_allowed: bool,
}

/// Automated grading rubric applied to every draft before publishing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rubric {
    #[serde(default = "default_min_score")]
    pub min_score: f32,
    #[serde(default)]
    pub criteria: Vec<Criterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Criterion {
    pub name: String,
    pub weight: f32,
    #[serde(default)]
    pub description: String,
}

/// A platform channel bound to this persona.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub platform: Platform,
    pub handle: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cadence")]
    pub posts_per_day: u8,
    /// Fixed execution windows (24h clock) chosen for predictability.
    #[serde(default)]
    pub windows: Vec<String>,
}

fn default_cadence() -> u8 {
    1
}

/// Supported publishing platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Bluesky,
    X,
    Linkedin,
    Instagram,
    Tiktok,
    Kwai,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Bluesky => "bluesky",
            Platform::X => "x",
            Platform::Linkedin => "linkedin",
            Platform::Instagram => "instagram",
            Platform::Tiktok => "tiktok",
            Platform::Kwai => "kwai",
        }
    }
}

/// A single validation problem found in a persona pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: String,
    pub problem: String,
}

impl PersonaPack {
    /// Validate invariants the engine relies on before running any persona.
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        if self.persona.trim().is_empty() {
            issues.push(ValidationIssue {
                field: "persona".into(),
                problem: "persona slug is empty".into(),
            });
        }
        if self.niche.trim().is_empty() {
            issues.push(ValidationIssue {
                field: "niche".into(),
                problem: "niche is empty".into(),
            });
        }
        if self.voice.tone.trim().is_empty() {
            issues.push(ValidationIssue {
                field: "voice.tone".into(),
                problem: "tone is empty".into(),
            });
        }
        if self.blocklist.is_empty() {
            issues.push(ValidationIssue {
                field: "blocklist".into(),
                problem: "blocklist must not be empty (brand safety is mandatory)".into(),
            });
        }
        if !(0.0..=1.0).contains(&self.rubric.min_score) {
            issues.push(ValidationIssue {
                field: "rubric.min_score".into(),
                problem: "min_score must be within 0.0..=1.0".into(),
            });
        }
        let weight_sum: f32 = self.rubric.criteria.iter().map(|c| c.weight).sum();
        if !self.rubric.criteria.is_empty() && (weight_sum - 1.0).abs() > 0.01 {
            issues.push(ValidationIssue {
                field: "rubric.criteria".into(),
                problem: format!("criteria weights must sum to 1.0 (got {weight_sum:.2})"),
            });
        }
        if !self.channels.iter().any(|c| c.enabled) {
            issues.push(ValidationIssue {
                field: "channels".into(),
                problem: "at least one enabled channel is required".into(),
            });
        }
        if self.active {
            for channel in &self.channels {
                if channel.enabled && channel.handle.trim().is_empty() {
                    issues.push(ValidationIssue {
                        field: format!("channels[{}].handle", channel.platform.as_str()),
                        problem: "an active pack requires a handle on every enabled channel".into(),
                    });
                }
            }
        }

        issues
    }
}

/// Load a single persona pack from a YAML file.
pub async fn load_persona(path: &Path) -> Result<PersonaPack> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading persona pack {}", path.display()))?;
    let pack: PersonaPack = serde_yaml_ng::from_str(&raw)
        .with_context(|| format!("parsing persona pack {}", path.display()))?;
    Ok(pack)
}

/// Load every `*.yaml` persona pack found in a directory.
pub async fn load_dir(dir: &Path) -> Result<Vec<PersonaPack>> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("reading personas directory {}", dir.display()))?;

    let mut packs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let is_yaml = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
        if is_yaml {
            packs.push(load_persona(&path).await?);
        }
    }

    packs.sort_by(|a, b| a.persona.cmp(&b.persona));
    Ok(packs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_pack() -> PersonaPack {
        PersonaPack {
            persona: "demo".into(),
            display_name: "Demo".into(),
            niche: "science".into(),
            language: "pt-BR".into(),
            active: true,
            voice: Voice {
                tone: "curious".into(),
                audience: "young adults".into(),
                guidelines: vec![],
                banned_phrases: vec![],
                max_hashtags: 5,
                emojis_allowed: true,
            },
            few_shot: vec![],
            blocklist: vec!["extreme-right".into()],
            rubric: Rubric {
                min_score: 0.75,
                criteria: vec![Criterion {
                    name: "hook".into(),
                    weight: 1.0,
                    description: String::new(),
                }],
            },
            channels: vec![Channel {
                platform: Platform::Bluesky,
                handle: "demo.bsky.social".into(),
                enabled: true,
                posts_per_day: 1,
                windows: vec![],
            }],
        }
    }

    #[test]
    fn valid_pack_has_no_issues() {
        assert!(base_pack().validate().is_empty());
    }

    #[test]
    fn active_pack_requires_handle() {
        let mut pack = base_pack();
        pack.channels[0].handle.clear();
        let issues = pack.validate();
        assert!(issues.iter().any(|i| i.field == "channels[bluesky].handle"));
    }

    #[test]
    fn draft_pack_does_not_require_handle() {
        let mut pack = base_pack();
        pack.active = false;
        pack.channels[0].handle.clear();
        assert!(pack.validate().is_empty());
    }

    #[test]
    fn empty_blocklist_is_rejected() {
        let mut pack = base_pack();
        pack.blocklist.clear();
        let issues = pack.validate();
        assert!(issues.iter().any(|i| i.field == "blocklist"));
    }

    #[test]
    fn weights_must_sum_to_one() {
        let mut pack = base_pack();
        pack.rubric.criteria[0].weight = 0.5;
        let issues = pack.validate();
        assert!(issues.iter().any(|i| i.field == "rubric.criteria"));
    }
}

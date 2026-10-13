//! Persona pack: the configurable data package the harness loads per profile.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default directory where persona packs live, relative to the workspace root.
pub const DEFAULT_PERSONAS_DIR: &str = "personas";

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

fn default_variants_per_slot() -> usize {
    1
}

/// How a slot chooses among a platform's approved variants.
///
/// The A/B comparison is **sequential** (`scoring-model.md`, HB-040): a slot
/// releases one variant and the alternative waits for a future slot. How the
/// slot picks is data, not a constant, so a persona can change it without a
/// code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionRule {
    /// Rotate per slot so consecutive slots alternate between the variants.
    #[default]
    Alternate,
    /// Always take the first approved variant (deterministic, no rotation).
    First,
}

/// How many variants a slot releases per platform and how it picks them.
///
/// The default is the owner-approved sequential policy: **one** variant per
/// slot, rotating. Raising `variants_per_slot` publishes several look-alike
/// posts in the same minute (the behaviour the redesign removed), so it is an
/// explicit, deliberate opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishingPolicy {
    /// How many approved variants one platform publishes per slot (at least 1).
    #[serde(default = "default_variants_per_slot")]
    pub variants_per_slot: usize,
    /// How the slot picks among the approved variants.
    #[serde(default)]
    pub selection: SelectionRule,
}

impl Default for PublishingPolicy {
    fn default() -> Self {
        Self {
            variants_per_slot: default_variants_per_slot(),
            selection: SelectionRule::default(),
        }
    }
}

/// The search queries a persona harvests in rotation (HB-033).
///
/// Deriving the query from the `niche` alone (`astronomy space curiosities`) is
/// too thin to tell two personas apart: they would all harvest the same
/// subjects. A persona that cares about specific sources declares them here and
/// the harvest stage rotates through the list; a persona that declares nothing
/// keeps the old behaviour (the humanised niche), so older packs never break.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarvestPolicy {
    /// Search queries used in rotation. Empty falls back to the humanised niche.
    #[serde(default)]
    pub queries: Vec<String>,
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
    /// How a slot selects and how many variants it releases (sequential A/B).
    #[serde(default)]
    pub publishing: PublishingPolicy,
    /// Per-persona harvest queries (HB-033); absent means "use the niche".
    #[serde(default)]
    pub harvest: HarvestPolicy,
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

/// Human-readable harvest query derived from a persona niche.
///
/// `science-curiosities` and `astrology_horoscope` become `science curiosities`
/// and `astrology horoscope`: a niche is a slug, a search engine wants words.
pub fn humanize_niche(niche: &str) -> String {
    niche
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl PersonaPack {
    /// The harvest queries this persona rotates through (HB-033).
    ///
    /// A persona that configures `harvest.queries` harvests the subjects it
    /// declares; a persona that does not falls back to the **humanised niche**,
    /// so an older pack keeps its previous single-query behaviour. Blank entries
    /// are ignored, so a stray line in the list never becomes an empty search.
    pub fn harvest_queries(&self) -> Vec<String> {
        let configured: Vec<String> = self
            .harvest
            .queries
            .iter()
            .map(|query| query.trim().to_string())
            .filter(|query| !query.is_empty())
            .collect();
        if configured.is_empty() {
            vec![humanize_niche(&self.niche)]
        } else {
            configured
        }
    }

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
        if self.publishing.variants_per_slot == 0 {
            issues.push(ValidationIssue {
                field: "publishing.variants_per_slot".into(),
                problem: "variants_per_slot must be at least 1".into(),
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

/// Resolve a `--persona` value that may be a slug or a path into the pack path.
///
/// HB-031: the flag used to mean two things — `metrics collect` took a slug
/// while `learn run` took a file path — so the same spelling behaved
/// differently. The convention is now one: every `--persona` accepts **both** a
/// bare slug (`space-oddities`, resolved under [`DEFAULT_PERSONAS_DIR`]) and a
/// path (`personas/space-oddities.yaml`). A value with a `.yaml`/`.yml`
/// extension or a path separator is taken as a path; anything else is a slug.
pub fn resolve_persona_path(value: &str) -> PathBuf {
    let trimmed = value.trim();
    let path = Path::new(trimmed);
    let has_pack_extension = matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("yaml" | "yml")
    );
    let has_separator = trimmed.contains('/') || trimmed.contains(std::path::MAIN_SEPARATOR);
    if has_pack_extension || has_separator {
        PathBuf::from(trimmed)
    } else {
        Path::new(DEFAULT_PERSONAS_DIR).join(format!("{trimmed}.yaml"))
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
            publishing: PublishingPolicy::default(),
            harvest: HarvestPolicy::default(),
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

    #[test]
    fn publishing_policy_defaults_to_one_alternating_variant() {
        let policy = PublishingPolicy::default();
        assert_eq!(policy.variants_per_slot, 1);
        assert_eq!(policy.selection, SelectionRule::Alternate);
    }

    #[test]
    fn a_slot_must_release_at_least_one_variant() {
        let mut pack = base_pack();
        pack.publishing.variants_per_slot = 0;
        let issues = pack.validate();
        assert!(
            issues
                .iter()
                .any(|i| i.field == "publishing.variants_per_slot")
        );
    }

    #[test]
    fn an_absent_publishing_block_uses_the_sequential_default() {
        // The policy is data with a safe default: an older pack without the
        // block must still parse as one variant per slot, alternating.
        let yaml = "persona: demo\nniche: science\nvoice:\n  tone: curious\n  audience: adults\nrubric:\n  min_score: 0.75\nchannels:\n  - platform: bluesky\n    handle: demo\n    enabled: true\n";
        let pack: PersonaPack = serde_yaml_ng::from_str(yaml).expect("parses");
        assert_eq!(pack.publishing, PublishingPolicy::default());
        // HB-033: a pack with no harvest block keeps the niche fallback.
        assert_eq!(pack.harvest, HarvestPolicy::default());
        assert_eq!(pack.harvest_queries(), vec!["science".to_string()]);
    }

    #[test]
    fn a_pack_without_harvest_queries_falls_back_to_the_humanised_niche() {
        // The pre-HB-033 behaviour, preserved for older packs.
        let mut pack = base_pack();
        pack.niche = "astronomy-space-curiosities".into();
        pack.harvest.queries = vec!["   ".into()];
        assert_eq!(
            pack.harvest_queries(),
            vec!["astronomy space curiosities".to_string()]
        );
    }

    #[test]
    fn configured_harvest_queries_win_over_the_niche() {
        // HB-033: two personas with the same kind of niche must be able to
        // harvest different sources; the pack decides, not the niche alone.
        let mut pack = base_pack();
        pack.niche = "astronomy-space-curiosities".into();
        pack.harvest.queries = vec![
            "space mission anomalies".into(),
            "".into(),
            "weird telescope data".into(),
        ];
        assert_eq!(
            pack.harvest_queries(),
            vec![
                "space mission anomalies".to_string(),
                "weird telescope data".to_string(),
            ]
        );
    }

    #[test]
    fn a_bare_slug_resolves_under_the_personas_directory() {
        // HB-031: the same `--persona` flag takes a slug or a path.
        assert_eq!(
            resolve_persona_path("space-oddities"),
            PathBuf::from("personas/space-oddities.yaml")
        );
        assert_eq!(
            resolve_persona_path("  space-oddities  "),
            PathBuf::from("personas/space-oddities.yaml")
        );
    }

    #[test]
    fn anything_that_looks_like_a_path_is_kept_verbatim() {
        assert_eq!(
            resolve_persona_path("personas/space-oddities.yaml"),
            PathBuf::from("personas/space-oddities.yaml")
        );
        assert_eq!(
            resolve_persona_path("/tmp/pack.yml"),
            PathBuf::from("/tmp/pack.yml")
        );
        // A separator without an extension is still a path, not a slug.
        assert_eq!(
            resolve_persona_path("some/dir/pack"),
            PathBuf::from("some/dir/pack")
        );
    }

    #[test]
    fn humanizes_the_niche_into_a_query() {
        assert_eq!(humanize_niche("science-curiosities"), "science curiosities");
        assert_eq!(humanize_niche("astrology_horoscope"), "astrology horoscope");
        assert_eq!(humanize_niche("  spaced  out "), "spaced out");
    }
}

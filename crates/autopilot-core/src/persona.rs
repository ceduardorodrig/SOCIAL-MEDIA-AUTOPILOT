//! Persona pack: the configurable data package the harness loads per profile.

use std::path::{Path, PathBuf};

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
    /// Per-persona visual identity (template, palette, typography, motif).
    /// Absent means the engine uses its built-in default card.
    #[serde(default)]
    pub visual: Option<Visual>,
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

/// Fallback background colour of a persona that declares no palette.
const DEFAULT_BACKGROUND: &str = "#0b1220";
/// Fallback accent colour.
const DEFAULT_ACCENT: &str = "#f59e0b";
/// Fallback secondary colour.
const DEFAULT_SECONDARY: &str = "#34d399";
/// Fallback text colour.
const DEFAULT_TEXT: &str = "#f8fafc";
/// Fallback font family.
const DEFAULT_FONT_FAMILY: &str = "DejaVu Sans";
/// Fallback title font weight.
const DEFAULT_FONT_WEIGHT: u16 = 700;

fn default_background() -> String {
    DEFAULT_BACKGROUND.to_string()
}

fn default_accent() -> String {
    DEFAULT_ACCENT.to_string()
}

fn default_secondary() -> String {
    DEFAULT_SECONDARY.to_string()
}

fn default_text() -> String {
    DEFAULT_TEXT.to_string()
}

fn default_font_family() -> String {
    DEFAULT_FONT_FAMILY.to_string()
}

fn default_font_weight() -> u16 {
    DEFAULT_FONT_WEIGHT
}

/// The colours one persona's cards are painted with.
///
/// Every field has a sane default, so a pack may declare only the colours it
/// overrides. The palette is data, never a constant in the renderer: two packs
/// with different palettes must never render the same card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    #[serde(default = "default_background")]
    pub background: String,
    #[serde(default = "default_accent")]
    pub accent: String,
    #[serde(default = "default_secondary")]
    pub secondary: String,
    #[serde(default = "default_text")]
    pub text: String,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: default_background(),
            accent: default_accent(),
            secondary: default_secondary(),
            text: default_text(),
        }
    }
}

/// Letter case applied to the labels a template renders (motif, footer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextCase {
    /// Keep the text exactly as authored.
    #[default]
    None,
    Upper,
    Lower,
}

impl TextCase {
    /// Apply the case transform to `text`.
    pub fn apply(self, text: &str) -> String {
        match self {
            TextCase::None => text.to_string(),
            TextCase::Upper => text.to_uppercase(),
            TextCase::Lower => text.to_lowercase(),
        }
    }
}

/// The typography one persona's cards use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typography {
    #[serde(default = "default_font_family")]
    pub family: String,
    #[serde(default = "default_font_weight")]
    pub weight: u16,
    #[serde(default)]
    pub case: TextCase,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            family: default_font_family(),
            weight: default_font_weight(),
            case: TextCase::None,
        }
    }
}

/// Per-persona visual identity: how the cards of *this* persona look.
///
/// Identity separation is an invariant (HB-032): a persona never shares a
/// template, palette, motif or footer with another one. The whole block is
/// optional — a pack without it keeps working and falls back to the engine's
/// built-in default card.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Visual {
    /// Template name or path, resolved under `personas/<slug>/visual/`.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub palette: Palette,
    #[serde(default)]
    pub typography: Typography,
    /// Structural motif name (e.g. `terminal`, `orbit`).
    #[serde(default)]
    pub motif: String,
    /// Small line rendered at the bottom of the card.
    #[serde(default)]
    pub footer: String,
}

/// True when `value` looks like a `#rgb` or `#rrggbb` hex colour.
pub fn is_hex_color(value: &str) -> bool {
    let Some(digits) = value.trim().strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 6) && digits.chars().all(|c| c.is_ascii_hexdigit())
}

/// Directory holding one persona's visual assets (`personas/<slug>/visual/`).
pub fn visual_dir(personas_root: &Path, slug: &str) -> PathBuf {
    personas_root.join(slug).join("visual")
}

/// Resolve a `visual.template` value to the file it points at.
///
/// * an absolute path is used as-is;
/// * a value with a path separator or an `.svg` extension is taken relative to
///   the persona's `visual/` directory;
/// * a bare name (`card-terminal-v1`) becomes `<name>.svg` inside it.
pub fn resolve_visual_template(visual_dir: &Path, template: &str) -> PathBuf {
    let template = template.trim();
    let candidate = Path::new(template);
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    if template.ends_with(".svg") || template.contains('/') || template.contains('\\') {
        return visual_dir.join(candidate);
    }
    visual_dir.join(format!("{template}.svg"))
}

/// A single validation problem found in a persona pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: String,
    pub problem: String,
}

impl PersonaPack {
    /// Validate invariants the engine relies on before running any persona.
    ///
    /// Only checks that need no filesystem: the `visual.template` is validated
    /// syntactically. Use [`PersonaPack::validate_with_base`] to also assert
    /// that the template resolves to a real file.
    pub fn validate(&self) -> Vec<ValidationIssue> {
        self.validate_with_base(None)
    }

    /// Validate the pack and, when `personas_root` is given, assert that the
    /// declared visual template resolves to a file under
    /// `personas/<slug>/visual/`.
    pub fn validate_with_base(&self, personas_root: Option<&Path>) -> Vec<ValidationIssue> {
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

        issues.extend(self.visual_issues(personas_root));

        issues
    }

    /// Validate the optional `visual` block and return its issues.
    ///
    /// When `personas_root` is given, a declared template must resolve to a
    /// file: a template that cannot be loaded is an error, not a silent
    /// fallback, so two personas can never accidentally share a look.
    fn visual_issues(&self, personas_root: Option<&Path>) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        let Some(visual) = &self.visual else {
            return issues;
        };

        for (field, value) in [
            ("visual.palette.background", &visual.palette.background),
            ("visual.palette.accent", &visual.palette.accent),
            ("visual.palette.secondary", &visual.palette.secondary),
            ("visual.palette.text", &visual.palette.text),
        ] {
            if !is_hex_color(value) {
                issues.push(ValidationIssue {
                    field: field.into(),
                    problem: format!("`{value}` is not a hex colour (#rgb or #rrggbb)"),
                });
            }
        }

        if visual.typography.family.trim().is_empty() {
            issues.push(ValidationIssue {
                field: "visual.typography.family".into(),
                problem: "font family is empty".into(),
            });
        }
        if !(100..=900).contains(&visual.typography.weight) {
            issues.push(ValidationIssue {
                field: "visual.typography.weight".into(),
                problem: format!(
                    "weight must be within 100..=900 (got {})",
                    visual.typography.weight
                ),
            });
        }

        if let Some(template) = &visual.template {
            if template.trim().is_empty() {
                issues.push(ValidationIssue {
                    field: "visual.template".into(),
                    problem: "template is declared but empty".into(),
                });
            } else if let Some(root) = personas_root {
                let dir = visual_dir(root, &self.persona);
                let path = resolve_visual_template(&dir, template);
                if !path.is_file() {
                    issues.push(ValidationIssue {
                        field: "visual.template".into(),
                        problem: format!(
                            "template `{template}` does not resolve to a file (expected {})",
                            path.display()
                        ),
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
            visual: None,
        }
    }

    #[test]
    fn valid_pack_has_no_issues() {
        assert!(base_pack().validate().is_empty());
    }

    #[test]
    fn a_pack_without_visual_stays_valid() {
        let mut pack = base_pack();
        pack.visual = None;
        assert!(pack.validate().is_empty());
    }

    #[test]
    fn visual_defaults_fill_a_partial_block() {
        let visual: Visual = serde_yaml_ng::from_str("palette:\n  accent: \"#ff0000\"\n")
            .expect("a partial visual block deserializes");
        assert_eq!(visual.palette.accent, "#ff0000");
        assert_eq!(visual.palette.background, DEFAULT_BACKGROUND);
        assert_eq!(visual.typography.family, DEFAULT_FONT_FAMILY);
        assert_eq!(visual.typography.weight, DEFAULT_FONT_WEIGHT);
        assert_eq!(visual.typography.case, TextCase::None);
        assert!(visual.template.is_none());
        assert!(visual.motif.is_empty());
    }

    #[test]
    fn visual_block_parses_from_a_persona_pack() {
        let yaml = r##"
persona: demo
niche: science
voice: { tone: curious, audience: peers }
rubric: { min_score: 0.75 }
channels:
  - { platform: bluesky, handle: demo.bsky.social, enabled: true }
blocklist: [extremism]
visual:
  template: card-orbit-v1
  palette: { background: "#05060a", accent: "#7c3aed", secondary: "#22d3ee", text: "#e2e8f0" }
  typography: { family: "Inter", weight: 800, case: upper }
  motif: orbit
  footer: space.example
"##;
        let pack: PersonaPack = serde_yaml_ng::from_str(yaml).expect("pack parses");
        let visual = pack.visual.as_ref().expect("visual block present");
        assert_eq!(visual.template.as_deref(), Some("card-orbit-v1"));
        assert_eq!(visual.motif, "orbit");
        assert_eq!(visual.typography.case, TextCase::Upper);
        assert_eq!(visual.typography.case.apply("orbit"), "ORBIT");
        assert!(pack.validate().is_empty());
    }

    #[test]
    fn non_hex_palette_colour_is_rejected() {
        let mut pack = base_pack();
        pack.visual = Some(Visual {
            palette: Palette {
                accent: "amber".into(),
                ..Palette::default()
            },
            ..Visual::default()
        });
        let issues = pack.validate();
        assert!(
            issues.iter().any(|i| i.field == "visual.palette.accent"),
            "expected a palette issue, got {issues:?}"
        );
    }

    #[test]
    fn out_of_range_font_weight_is_rejected() {
        let mut pack = base_pack();
        pack.visual = Some(Visual {
            typography: Typography {
                weight: 1000,
                ..Typography::default()
            },
            ..Visual::default()
        });
        let issues = pack.validate();
        assert!(issues.iter().any(|i| i.field == "visual.typography.weight"));
    }

    #[test]
    fn empty_template_declaration_is_rejected() {
        let mut pack = base_pack();
        pack.visual = Some(Visual {
            template: Some("  ".into()),
            ..Visual::default()
        });
        let issues = pack.validate();
        assert!(issues.iter().any(|i| i.field == "visual.template"));
    }

    #[test]
    fn hex_colour_detection() {
        assert!(is_hex_color("#fff"));
        assert!(is_hex_color("#0b1220"));
        assert!(!is_hex_color("0b1220"));
        assert!(!is_hex_color("#12345"));
        assert!(!is_hex_color("#gggggg"));
        assert!(!is_hex_color(""));
    }

    #[test]
    fn template_names_and_paths_resolve_under_the_visual_dir() {
        let dir = Path::new("/personas/demo/visual");
        assert_eq!(
            resolve_visual_template(dir, "card-orbit-v1"),
            dir.join("card-orbit-v1.svg")
        );
        assert_eq!(
            resolve_visual_template(dir, "card-orbit-v1.svg"),
            dir.join("card-orbit-v1.svg")
        );
        assert_eq!(
            resolve_visual_template(dir, "nested/card.svg"),
            dir.join("nested/card.svg")
        );
        assert_eq!(
            resolve_visual_template(dir, "/abs/card.svg"),
            PathBuf::from("/abs/card.svg")
        );
    }

    #[test]
    fn visual_dir_is_scoped_to_the_persona() {
        assert_eq!(
            visual_dir(Path::new("/personas"), "demo"),
            PathBuf::from("/personas/demo/visual")
        );
    }

    #[test]
    fn a_declared_template_that_does_not_exist_is_an_issue() {
        let mut pack = base_pack();
        pack.visual = Some(Visual {
            template: Some("card-absent-v1".into()),
            ..Visual::default()
        });
        // Without a base dir only the syntax is checked.
        assert!(pack.validate().is_empty());
        // With the personas root, the absent template file is reported.
        let issues = pack.validate_with_base(Some(Path::new("/nonexistent/personas")));
        assert!(
            issues.iter().any(|i| i.field == "visual.template"),
            "expected a template issue, got {issues:?}"
        );
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

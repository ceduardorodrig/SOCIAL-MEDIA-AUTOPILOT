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

fn default_text_budget() -> u16 {
    12
}

fn default_card_budget() -> u16 {
    6
}

fn default_video_budget() -> u16 {
    0
}

/// The medium a post uses, one per slot (`content-formats.md` §2).
///
/// The format is decided per topic by the creation stage (a filled
/// `image_prompt` means a card, an empty one means text), but the *frequency*
/// policy constrains which formats a slot may release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Text only — the baseline, always available once the account is warm.
    Text,
    /// A post with a generated card (image).
    Card,
    /// A short video.
    Video,
}

impl Format {
    /// Stable, lower-case label used by the journal and the console.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Text => "text",
            Format::Card => "card",
            Format::Video => "video",
        }
    }

    /// Read a format back from its journal label. An unknown or absent label is
    /// treated as text: the baseline format, so an older journal line never
    /// disappears from the weekly accounting (HB-034).
    pub fn from_label(label: Option<&str>) -> Format {
        match label.map(str::trim) {
            Some("card") => Format::Card,
            Some("video") => Format::Video,
            _ => Format::Text,
        }
    }
}

/// The formats a slot may release this week, after the warming ramp and the
/// weekly per-format budget (`content-formats.md` §3, HB-034).
///
/// The creation model is told which formats the budget and the ramp leave
/// available so **it chooses** the format for the topic instead of the engine
/// imposing one; the engine keeps only a deterministic safety net
/// ([`crate`]-level `effective_format`). This is pure data so both the prompt
/// (in `autopilot-pipeline`) and the publish gate (in `autopilot-run`) share one
/// shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatAvailability {
    pub text: bool,
    pub card: bool,
    pub video: bool,
}

impl FormatAvailability {
    /// Every format released (the state a pack with no accounting starts from).
    pub fn all() -> Self {
        Self {
            text: true,
            card: true,
            video: true,
        }
    }

    /// Whether one format is available this slot.
    pub fn allows(self, format: Format) -> bool {
        match format {
            Format::Text => self.text,
            Format::Card => self.card,
            Format::Video => self.video,
        }
    }

    /// Whether any format is available at all.
    pub fn any(self) -> bool {
        self.text || self.card || self.video
    }

    /// Whether every format is available (nothing to constrain the model about).
    pub fn unrestricted(self) -> bool {
        self.text && self.card && self.video
    }
}

impl Default for FormatAvailability {
    fn default() -> Self {
        Self::all()
    }
}

/// Weekly budget per format (`content-formats.md` §3), engine defaults.
///
/// The budget is **not rigid**: it exists to avoid monotony, not to impose a
/// single ceiling — text is cheap and conversational, so it carries more volume
/// than card or video, which cost production and attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatBudgets {
    /// Text posts allowed per week (the baseline; the highest volume).
    #[serde(default = "default_text_budget")]
    pub text: u16,
    /// Card (image) posts allowed per week.
    #[serde(default = "default_card_budget")]
    pub card: u16,
    /// Short-video posts allowed per week (`0` until the pipeline is validated).
    #[serde(default = "default_video_budget")]
    pub video: u16,
}

impl FormatBudgets {
    /// The weekly budget for one format.
    pub fn budget(&self, format: Format) -> u16 {
        match format {
            Format::Text => self.text,
            Format::Card => self.card,
            Format::Video => self.video,
        }
    }
}

impl Default for FormatBudgets {
    fn default() -> Self {
        Self {
            text: default_text_budget(),
            card: default_card_budget(),
            video: default_video_budget(),
        }
    }
}

/// Warming-ramp activation for a new account (`content-formats.md` §1).
///
/// A new account does not post at volume: the engine holds it to week 1
/// (text only) and releases more formats as it warms. The ramp is **autonomous**:
/// the engine detects the account's birth from its own history (the first
/// publication recorded in the run journals), and `started_on` is only an
/// explicit **override** for the rare case the operator wants to force a start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarmingRamp {
    /// Explicit ISO `YYYY-MM-DD` override for the ramp start. When absent the
    /// engine auto-detects the birth from the first recorded publication.
    #[serde(default)]
    pub started_on: Option<String>,
}

impl WarmingRamp {
    /// The **declared** ramp start as days since 1970-01-01 (UTC), or `None`.
    pub fn start_day(&self) -> Option<i64> {
        self.started_on.as_deref().and_then(parse_iso_date_days)
    }

    /// Resolve the ramp start day: the explicit override wins, else the
    /// auto-detected birth (`earliest_record_day`).
    ///
    /// Returns `None` when neither exists — an **empty base** (no journal and no
    /// override) is treated as a mature account with **no ramp**, never as an
    /// eternal "week 1": an old account whose journals were cleared must not be
    /// throttled, and a brand-new account starts ramping from its first recorded
    /// post onward.
    pub fn resolve_start_day(&self, earliest_record_day: Option<i64>) -> Option<i64> {
        self.start_day().or(earliest_record_day)
    }
}

/// How many variants a slot releases per platform and how it picks them, plus
/// the **format/frequency policy** the slot must respect (HB-034).
///
/// The default is the owner-approved sequential policy: **one** variant per
/// slot, rotating. Raising `variants_per_slot` publishes several look-alike
/// posts in the same minute (the behaviour the redesign removed), so it is an
/// explicit, deliberate opt-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishingPolicy {
    /// How many approved variants one platform publishes per slot (at least 1).
    #[serde(default = "default_variants_per_slot")]
    pub variants_per_slot: usize,
    /// How the slot picks among the approved variants.
    #[serde(default)]
    pub selection: SelectionRule,
    /// Weekly budget per format (`content-formats.md` §3).
    #[serde(default)]
    pub formats: FormatBudgets,
    /// Warming ramp for a new account (`content-formats.md` §1).
    #[serde(default)]
    pub warming: WarmingRamp,
}

impl Default for PublishingPolicy {
    fn default() -> Self {
        Self {
            variants_per_slot: default_variants_per_slot(),
            selection: SelectionRule::default(),
            formats: FormatBudgets::default(),
            warming: WarmingRamp::default(),
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

/// Per-persona **card** visual identity (HB-032).
///
/// A feed must not look like one initiative: two personas run by the same engine
/// must be visually distinguishable, or a reader cannot tell them apart. The
/// card crate already ships several visually distinct templates and rotates them
/// per post; this block scopes **which** templates belong to *this* persona — a
/// **pool**, not one template — so the card still rotates inside the persona's
/// own set (variety within a feed is kept, HB-055b) while two personas with
/// different pools provably never render the same card.
///
/// The block is optional and purely additive: an absent or empty pool inherits
/// every template (the global rotation), so an older pack behaves exactly as
/// before. A pool that names a template outside the catalogue is rejected where
/// the catalogue is known (`personas validate`, the run gate) — never a silent
/// fallback, which is how two personas come to share a look.
///
/// The avatar is deliberately **not** part of this block: it already lives per
/// persona under `personas/<slug>/assets/` (its own path and pipeline).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualIdentity {
    /// Card template names (`card-v1` … `card-vN`) this persona may use, drawn
    /// from the engine's catalogue. Empty inherits every template. Duplicates
    /// and blank entries are allowed in YAML but flagged by validation; the
    /// accessor drops the blanks.
    #[serde(default)]
    pub templates: Vec<String>,
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
    /// Per-persona card visual identity (HB-032); absent inherits every template.
    #[serde(default)]
    pub visual: Option<VisualIdentity>,
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

    /// The card template pool this persona owns (HB-032).
    ///
    /// Blank entries are dropped; an empty result means "inherit every template"
    /// (the global rotation), which is exactly what a pack without a `visual`
    /// block gets — so the previous behaviour is preserved byte-for-byte.
    pub fn visual_templates(&self) -> Vec<String> {
        self.visual
            .as_ref()
            .map(|visual| {
                visual
                    .templates
                    .iter()
                    .map(|template| template.trim().to_string())
                    .filter(|template| !template.is_empty())
                    .collect()
            })
            .unwrap_or_default()
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
        if let Some(started_on) = &self.publishing.warming.started_on
            && parse_iso_date_days(started_on).is_none()
        {
            issues.push(ValidationIssue {
                field: "publishing.warming.started_on".into(),
                problem: format!("`{started_on}` is not a valid YYYY-MM-DD date"),
            });
        }
        // HB-032: the card pool is checked for blank and duplicate names here.
        // Membership against the real catalogue is checked where the catalogue is
        // known (`personas validate`, the run gate) — never a silent fallback.
        if let Some(visual) = &self.visual {
            let mut seen: Vec<&str> = Vec::new();
            for name in &visual.templates {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    issues.push(ValidationIssue {
                        field: "visual.templates".into(),
                        problem: "a card template name must not be blank".into(),
                    });
                } else if seen.contains(&trimmed) {
                    issues.push(ValidationIssue {
                        field: "visual.templates".into(),
                        problem: format!("card template `{trimmed}` is listed twice"),
                    });
                } else {
                    seen.push(trimmed);
                }
            }
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

/// Parse an ISO `YYYY-MM-DD` date into days since 1970-01-01 (UTC).
///
/// Returns `None` for anything that is not a real calendar date, so a typo in
/// `publishing.warming.started_on` can never silently disable the ramp (it is a
/// validation issue instead). The date is a *day number*, not a formatted
/// string, because the run only needs to know how old the account is.
pub fn parse_iso_date_days(value: &str) -> Option<i64> {
    let mut parts = value.trim().split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let year: i64 = year.parse().ok()?;
    let month: i64 = month.parse().ok()?;
    let day: i64 = day.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// Days in `month` of `year` (1-based month).
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days since the Unix epoch for a proleptic-Gregorian date
/// (Howard Hinnant's `days_from_civil`), valid for any year.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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
            visual: None,
        }
    }

    #[test]
    fn valid_pack_has_no_issues() {
        assert!(base_pack().validate().is_empty());
    }

    #[test]
    fn a_pack_without_a_visual_block_stays_valid_and_inherits_every_template() {
        // HB-032 back-compat: no `visual` block validates and inherits the whole
        // catalogue (an empty pool), so an older pack is untouched.
        let mut pack = base_pack();
        pack.visual = None;
        assert!(pack.validate().is_empty());
        assert!(pack.visual_templates().is_empty());
    }

    #[test]
    fn a_visual_block_scopes_the_card_pool() {
        let yaml = "persona: demo\nniche: science\nvoice:\n  tone: curious\n  audience: adults\nblocklist:\n  - forbidden\nrubric:\n  min_score: 0.75\nchannels:\n  - platform: bluesky\n    handle: demo\n    enabled: true\nvisual:\n  templates: [card-v3, card-v7]\n";
        let pack: PersonaPack = serde_yaml_ng::from_str(yaml).expect("parses");
        assert_eq!(
            pack.visual_templates(),
            vec!["card-v3".to_string(), "card-v7".to_string()]
        );
        assert!(pack.validate().is_empty(), "{:?}", pack.validate());
    }

    #[test]
    fn a_blank_or_duplicate_template_name_is_a_validation_issue() {
        let mut pack = base_pack();
        pack.visual = Some(VisualIdentity {
            templates: vec!["card-v1".into(), "card-v1".into(), "  ".into()],
        });
        let issues = pack.validate();
        assert!(
            issues.iter().any(|i| i.field == "visual.templates"),
            "expected a visual.templates issue, got {issues:?}"
        );
        // The accessor still yields the usable names (blanks dropped).
        assert_eq!(
            pack.visual_templates(),
            vec!["card-v1".to_string(), "card-v1".to_string()]
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

    #[test]
    fn the_format_budget_defaults_match_the_content_policy() {
        // content-formats.md §3: text 12, card 6, video 0 until validated.
        let budgets = FormatBudgets::default();
        assert_eq!(budgets.budget(Format::Text), 12);
        assert_eq!(budgets.budget(Format::Card), 6);
        assert_eq!(budgets.budget(Format::Video), 0);
    }

    #[test]
    fn the_warming_ramp_is_inert_until_a_pack_declares_its_start() {
        assert_eq!(WarmingRamp::default().start_day(), None);
        assert_eq!(
            WarmingRamp {
                started_on: Some("2026-10-13".into())
            }
            .start_day(),
            parse_iso_date_days("2026-10-13")
        );
    }

    #[test]
    fn the_ramp_resolves_the_override_first_then_the_detected_birth() {
        // HB-034: the ramp is autonomous. The explicit `started_on` is an
        // override; without it the engine uses the auto-detected birth.
        let declared = WarmingRamp {
            started_on: Some("2026-10-13".into()),
        };
        assert_eq!(
            declared.resolve_start_day(Some(100)),
            parse_iso_date_days("2026-10-13"),
            "an explicit override wins over the detected birth"
        );

        // No override: the detected birth is the ramp start.
        assert_eq!(
            WarmingRamp::default().resolve_start_day(Some(100)),
            Some(100)
        );

        // Empty base (no override, no record): no ramp — never an eternal week 1.
        assert_eq!(WarmingRamp::default().resolve_start_day(None), None);
        // An invalid override falls through to the detected birth (it never
        // silently disables the ramp; validation flags the bad date).
        let bad = WarmingRamp {
            started_on: Some("13/10/2026".into()),
        };
        assert_eq!(bad.resolve_start_day(Some(42)), Some(42));
    }

    #[test]
    fn format_availability_reports_and_defaults_to_all() {
        assert_eq!(FormatAvailability::default(), FormatAvailability::all());
        assert!(FormatAvailability::all().allows(Format::Video));
        assert!(FormatAvailability::all().any());
        assert!(FormatAvailability::all().unrestricted());

        let restricted = FormatAvailability {
            text: true,
            card: false,
            video: false,
        };
        assert!(restricted.allows(Format::Text));
        assert!(!restricted.allows(Format::Card));
        assert!(!restricted.allows(Format::Video));
        assert!(restricted.any());
        assert!(!restricted.unrestricted());

        let none = FormatAvailability {
            text: false,
            card: false,
            video: false,
        };
        assert!(!none.any());
    }

    #[test]
    fn a_pack_with_a_bad_warming_date_is_rejected() {
        let mut pack = base_pack();
        pack.publishing.warming.started_on = Some("13/10/2026".into());
        let issues = pack.validate();
        assert!(
            issues
                .iter()
                .any(|issue| issue.field == "publishing.warming.started_on"),
            "issues: {issues:?}"
        );
    }

    #[test]
    fn iso_dates_parse_to_real_calendar_days_only() {
        assert_eq!(parse_iso_date_days("1970-01-01"), Some(0));
        assert_eq!(parse_iso_date_days("1970-01-02"), Some(1));
        // 1970-01-08 is a Thursday exactly seven days later.
        assert_eq!(parse_iso_date_days("1970-01-08"), Some(7));
        // A leap day is real in a leap year and impossible otherwise.
        assert_eq!(parse_iso_date_days("2024-02-29"), Some(19782));
        assert_eq!(parse_iso_date_days("2023-02-29"), None);
        // Anything that is not a strict YYYY-MM-DD date is not a date.
        for junk in [
            "2026-13-01",
            "2026-00-10",
            "2026-1-1",
            "13/10/2026",
            "",
            "2026-10",
        ] {
            assert_eq!(parse_iso_date_days(junk), None, "junk: {junk}");
        }
    }

    #[test]
    fn a_format_label_round_trips_and_falls_back_to_text() {
        assert_eq!(Format::Text.as_str(), "text");
        assert_eq!(Format::from_label(Some("card")), Format::Card);
        assert_eq!(Format::from_label(Some("video")), Format::Video);
        // An unknown or absent label is the baseline, never an error.
        assert_eq!(Format::from_label(Some("gif")), Format::Text);
        assert_eq!(Format::from_label(None), Format::Text);
    }

    #[test]
    fn the_publishing_policy_carries_the_format_and_warming_defaults() {
        let policy = PublishingPolicy::default();
        assert_eq!(policy.formats, FormatBudgets::default());
        assert_eq!(policy.warming, WarmingRamp::default());
    }

    #[test]
    fn a_pack_can_override_the_budgets_and_declare_a_warming_start() {
        // The engine's policy is data: a pack opts into a different cadence
        // without a code change (HB-034). The block parses from YAML as the
        // owner will write it.
        let yaml = "persona: demo\nniche: science\nvoice:\n  tone: curious\n  audience: adults\nblocklist:\n  - forbidden\nrubric:\n  min_score: 0.75\nchannels:\n  - platform: bluesky\n    handle: demo\n    enabled: true\npublishing:\n  formats:\n    text: 9\n    card: 2\n    video: 1\n  warming:\n    started_on: 2026-10-13\n";
        let pack: PersonaPack = serde_yaml_ng::from_str(yaml).expect("parses");
        assert_eq!(
            pack.publishing.formats,
            FormatBudgets {
                text: 9,
                card: 2,
                video: 1,
            }
        );
        assert_eq!(
            pack.publishing.warming.started_on.as_deref(),
            Some("2026-10-13")
        );
        assert!(pack.validate().is_empty(), "{:?}", pack.validate());
    }
}

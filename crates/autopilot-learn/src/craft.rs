//! Deterministic craft scoring: the persona rubric **minus** named slop penalties.
//!
//! The craft score never blocks anything — that is the guardrail's job. It only
//! lowers a post's score so the learning stage can refuse to promote a post that
//! won by luck with a sloppy template (see `scoring-model.md`, §2 and §3), and so
//! the penalised shapes can be recorded in an "avoid" memory.
//!
//! Every heuristic is pure Rust and returns its own **named** penalty together
//! with the evidence that triggered it, so a human can see *why* a score dropped.
//! No LLM is involved anywhere in this module.
//!
//! Honest limitation: the persona rubric (hook/clarity/accuracy/voice/freshness)
//! is graded by the LLM at creation time and is **not** persisted in the metrics
//! store, so the base defaults to the neutral maximum ([`DEFAULT_RUBRIC_BASE`])
//! and only the penalties move the number. A caller that holds the real graded
//! rubric value can inject it through [`CraftConfig::rubric_base`].

use serde::{Deserialize, Serialize};

/// Base score used when no graded rubric value is available.
///
/// This is the maximum of the 0..1 scale; the penalties are what lower it.
pub const DEFAULT_RUBRIC_BASE: f64 = 1.0;

/// A post must clear this craft score to be promoted to a few-shot example.
///
/// A single moderate slop tick (≤ 0.15) is tolerated; a high-severity template
/// penalty (0.25) or two moderate ones drop below the floor.
pub const DEFAULT_CRAFT_FLOOR: f64 = 0.8;

/// How many recent posts the repetition heuristics look back at.
pub const DEFAULT_RECENT_WINDOW: usize = 10;

/// Maximum hashtags tolerated before the overload penalty applies.
pub const DEFAULT_MAX_HASHTAGS: usize = 2;

/// Maximum emojis tolerated before the overload penalty applies.
pub const DEFAULT_MAX_EMOJIS: usize = 2;

/// Words per em dash below which the density is considered a stylistic tic.
pub const DEFAULT_EM_DASH_WORDS: usize = 40;

/// Minimum coefficient of variation of sentence lengths (higher = more human).
pub const DEFAULT_MIN_SENTENCE_VARIATION: f64 = 0.25;

/// n-gram overlap at or above which two posts read as the same structure.
pub const DEFAULT_STRUCTURE_SIMILARITY: f64 = 0.5;

/// How many distinct avoid entries the memory proposal carries.
pub const DEFAULT_MAX_AVOID: usize = 8;

// Penalty magnitudes, named per the scoring model (`scoring-model.md`, §2).
// "high" penalties target the strongest bot signals (template reuse), "medium"
// target stylistic habits and "low" target noise.

/// High: the post reuses a recent post's structure (prefix/n-gram similarity).
pub const PENALTY_REPEATED_STRUCTURE: f64 = 0.25;
/// High: the post opens with the same words as a recent post.
pub const PENALTY_REPEATED_OPENING: f64 = 0.25;
/// Medium: em-dash density above ~1 per 40 words.
pub const PENALTY_EM_DASH_DENSITY: f64 = 0.15;
/// Medium: sentence lengths are too uniform, which reads like a machine.
pub const PENALTY_SENTENCE_VARIANCE: f64 = 0.12;
/// Medium: every post in the window ends in a question (a formula).
pub const PENALTY_ALWAYS_QUESTION: f64 = 0.15;
/// Medium: the listicle shape ("5 things about X") repeats.
pub const PENALTY_REPEATED_LISTICLE: f64 = 0.12;
/// Low: more than the tolerated number of hashtags.
pub const PENALTY_HASHTAG_OVERLOAD: f64 = 0.05;
/// Low: emoji overload.
pub const PENALTY_EMOJI_OVERLOAD: f64 = 0.05;

/// Tunables for the craft heuristics. All values have safe defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CraftConfig {
    /// Base rubric score before penalties are subtracted.
    pub rubric_base: f64,
    /// Minimum score a winning post needs to become a few-shot example.
    pub craft_floor: f64,
    /// How many recent posts the repetition heuristics inspect.
    pub recent_window: usize,
    /// Maximum hashtags tolerated.
    pub max_hashtags: usize,
    /// Maximum emojis tolerated.
    pub max_emojis: usize,
    /// Words per em dash below which the density is a tic.
    pub em_dash_words: usize,
    /// Minimum coefficient of variation of sentence lengths.
    pub min_sentence_variation: f64,
    /// n-gram overlap at or above which structures are considered repeated.
    pub structure_similarity: f64,
}

impl Default for CraftConfig {
    fn default() -> Self {
        Self {
            rubric_base: DEFAULT_RUBRIC_BASE,
            craft_floor: DEFAULT_CRAFT_FLOOR,
            recent_window: DEFAULT_RECENT_WINDOW,
            max_hashtags: DEFAULT_MAX_HASHTAGS,
            max_emojis: DEFAULT_MAX_EMOJIS,
            em_dash_words: DEFAULT_EM_DASH_WORDS,
            min_sentence_variation: DEFAULT_MIN_SENTENCE_VARIATION,
            structure_similarity: DEFAULT_STRUCTURE_SIMILARITY,
        }
    }
}

impl CraftConfig {
    /// Build from environment overrides, falling back to the defaults.
    ///
    /// Only the policy knobs are overridable (floor, base and window); malformed
    /// or absent values are ignored, never a panic. This keeps the pure
    /// functions deterministic while letting the operator tune the gate without
    /// new CLI flags (the CLI is out of scope for this module).
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(value) = env_number("AUTOPILOT_CRAFT_FLOOR") {
            config.craft_floor = value;
        }
        if let Some(value) = env_number("AUTOPILOT_CRAFT_RUBRIC_BASE") {
            config.rubric_base = value;
        }
        if let Some(value) = env_number("AUTOPILOT_CRAFT_RECENT_WINDOW") {
            config.recent_window = value.max(0.0) as usize;
        }
        config
    }
}

/// Read a numeric environment override, ignoring anything unparseable.
fn env_number(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

/// One named slop penalty with the evidence that triggered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CraftPenalty {
    /// Stable rule name (e.g. `repeated-opening`).
    pub rule: String,
    /// Magnitude subtracted from the base score.
    pub penalty: f64,
    /// Human-readable evidence explaining why the score dropped.
    pub evidence: String,
}

/// The craft verdict for one post.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CraftScore {
    /// Rubric base the penalties were subtracted from.
    pub base: f64,
    /// Sum of every penalty applied.
    pub total_penalty: f64,
    /// Final score, clamped to 0..1.
    pub score: f64,
    /// Every penalty that fired, in heuristic order.
    pub penalties: Vec<CraftPenalty>,
}

impl CraftScore {
    /// True when no heuristic fired.
    pub fn is_clean(&self) -> bool {
        self.penalties.is_empty()
    }

    /// True when the score clears `floor`.
    pub fn meets(&self, floor: f64) -> bool {
        self.score >= floor
    }
}

/// A shape or opening pattern the creation stage should avoid reusing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvoidEntry {
    /// Rule name that produced the penalty.
    pub rule: String,
    /// Magnitude of the penalty, so the reader sees how bad it is.
    pub penalty: f64,
    /// Evidence describing the offending shape.
    pub evidence: String,
}

/// Score one post against the craft heuristics.
///
/// `recent` is the persona's recent posts, **most recent first**; only the first
/// `config.recent_window` are inspected. Pass texts that are *not* exact copies
/// of `text` so the repetition heuristics measure variation, not duplicates
/// (duplicates are handled separately by de-duplication).
pub fn craft_score(text: &str, recent: &[&str], config: &CraftConfig) -> CraftScore {
    let window = recent
        .iter()
        .take(config.recent_window)
        .copied()
        .collect::<Vec<_>>();

    let mut penalties: Vec<CraftPenalty> = Vec::new();
    for candidate in [
        penalty_repeated_structure(text, &window, config),
        penalty_repeated_opening(text, &window),
        penalty_em_dash_density(text, config),
        penalty_sentence_variance(text, config),
        penalty_always_question(text, &window),
        penalty_repeated_listicle(text, &window),
        penalty_hashtag_overload(text, config),
        penalty_emoji_overload(text, config),
    ]
    .into_iter()
    .flatten()
    {
        penalties.push(candidate);
    }

    let total_penalty: f64 = penalties.iter().map(|penalty| penalty.penalty).sum();
    let score = (config.rubric_base - total_penalty).clamp(0.0, 1.0);

    CraftScore {
        base: config.rubric_base,
        total_penalty,
        score,
        penalties,
    }
}

/// Collect the distinct avoid entries across a corpus of posts.
///
/// `texts` is the corpus, **most recent first**.
pub fn avoid_entries(texts: &[&str], config: &CraftConfig) -> Vec<AvoidEntry> {
    let mut entries: Vec<AvoidEntry> = Vec::new();
    for (index, text) in texts.iter().enumerate() {
        let recent = others(texts, index, text);
        for penalty in craft_score(text, &recent, config).penalties {
            let duplicate = entries
                .iter()
                .any(|seen| seen.rule == penalty.rule && seen.evidence == penalty.evidence);
            if !duplicate {
                entries.push(AvoidEntry {
                    rule: penalty.rule,
                    penalty: penalty.penalty,
                    evidence: penalty.evidence,
                });
            }
            if entries.len() >= DEFAULT_MAX_AVOID {
                return entries;
            }
        }
    }
    entries
}

/// Every corpus text except the one at `index` and its exact copies.
fn others<'a>(texts: &[&'a str], index: usize, text: &str) -> Vec<&'a str> {
    texts
        .iter()
        .enumerate()
        .filter(|(other, candidate)| *other != index && **candidate != text)
        .map(|(_, candidate)| *candidate)
        .collect()
}

// --- Heuristics ------------------------------------------------------------

/// Repeated structure vs. the recent window (prefix/n-gram similarity).
fn penalty_repeated_structure(
    text: &str,
    recent: &[&str],
    config: &CraftConfig,
) -> Option<CraftPenalty> {
    let current = signature(&tokenize(text));
    if current.is_empty() {
        return None;
    }

    let mut best = 0.0_f64;
    let mut best_text = "";
    for candidate in recent {
        let similarity = jaccard(&current, &signature(&tokenize(candidate)));
        if similarity > best {
            best = similarity;
            best_text = candidate;
        }
    }

    if best >= config.structure_similarity {
        Some(CraftPenalty {
            rule: "repeated-structure".to_string(),
            penalty: PENALTY_REPEATED_STRUCTURE,
            evidence: format!(
                "{:.0}% n-gram overlap with a recent post: \"{}\"",
                best * 100.0,
                snippet(best_text)
            ),
        })
    } else {
        None
    }
}

/// A repeated opening ("The trick…", "Did you know…") is a template signature.
fn penalty_repeated_opening(text: &str, recent: &[&str]) -> Option<CraftPenalty> {
    let opening_tokens = opening(&tokenize(text));
    if opening_tokens.len() < OPENING_WORDS {
        return None;
    }
    if recent
        .iter()
        .any(|candidate| opening(&tokenize(candidate)) == opening_tokens)
    {
        return Some(CraftPenalty {
            rule: "repeated-opening".to_string(),
            penalty: PENALTY_REPEATED_OPENING,
            evidence: format!(
                "opening \"{}\" already used in a recent post",
                opening_tokens.join(" ")
            ),
        });
    }
    None
}

/// Em-dash density above ~1 per 40 words is an LLM stylistic tic.
///
/// A single em dash is normal prose, so the penalty only fires from two upward.
fn penalty_em_dash_density(text: &str, config: &CraftConfig) -> Option<CraftPenalty> {
    let dashes = text.matches('—').count();
    let words = text.split_whitespace().count();
    if dashes < 2 || words == 0 {
        return None;
    }
    let per = config.em_dash_words.max(1);
    let density = dashes as f64 / words as f64;
    if density > 1.0 / per as f64 {
        Some(CraftPenalty {
            rule: "em-dash-density".to_string(),
            penalty: PENALTY_EM_DASH_DENSITY,
            evidence: format!(
                "{dashes} em-dashes in {words} words (1 per ~{:.0} words, limit 1 per {per})",
                words as f64 / dashes as f64
            ),
        })
    } else {
        None
    }
}

/// Uniform sentence lengths read like a machine; humans vary their rhythm.
fn penalty_sentence_variance(text: &str, config: &CraftConfig) -> Option<CraftPenalty> {
    let lengths: Vec<f64> = sentences(text)
        .iter()
        .map(|sentence| sentence.split_whitespace().count() as f64)
        .filter(|length| *length > 0.0)
        .collect();
    if lengths.len() < 3 {
        return None;
    }

    let mean = lengths.iter().sum::<f64>() / lengths.len() as f64;
    if mean == 0.0 {
        return None;
    }
    let variance = lengths
        .iter()
        .map(|length| (length - mean).powi(2))
        .sum::<f64>()
        / lengths.len() as f64;
    let variation = variance.sqrt() / mean;

    if variation < config.min_sentence_variation {
        Some(CraftPenalty {
            rule: "flat-sentence-length".to_string(),
            penalty: PENALTY_SENTENCE_VARIANCE,
            evidence: format!(
                "{} sentences vary by only {variation:.2} (min {:.2})",
                lengths.len(),
                config.min_sentence_variation
            ),
        })
    } else {
        None
    }
}

/// Every post in the window ending in a question is a formula.
fn penalty_always_question(text: &str, recent: &[&str]) -> Option<CraftPenalty> {
    let mut posts: Vec<&str> = Vec::with_capacity(recent.len() + 1);
    posts.push(text);
    posts.extend(recent.iter().copied());
    if posts.len() < 2 || !posts.iter().all(|post| ends_with_question(post)) {
        return None;
    }
    Some(CraftPenalty {
        rule: "always-a-question".to_string(),
        penalty: PENALTY_ALWAYS_QUESTION,
        evidence: format!("all {} posts in the window end in a question", posts.len()),
    })
}

/// A repeated listicle shape ("5 things about X") exhausts the format.
fn penalty_repeated_listicle(text: &str, recent: &[&str]) -> Option<CraftPenalty> {
    if !is_listicle(text) {
        return None;
    }
    let repeats = recent
        .iter()
        .filter(|candidate| is_listicle(candidate))
        .count();
    if repeats == 0 {
        return None;
    }
    Some(CraftPenalty {
        rule: "repeated-listicle".to_string(),
        penalty: PENALTY_REPEATED_LISTICLE,
        evidence: format!(
            "listicle shape already used by {repeats} of {} recent post(s)",
            recent.len()
        ),
    })
}

/// Hashtags beyond the tolerated count are noise.
fn penalty_hashtag_overload(text: &str, config: &CraftConfig) -> Option<CraftPenalty> {
    let count = text
        .split_whitespace()
        .filter(|word| word.starts_with('#'))
        .count();
    if count > config.max_hashtags {
        Some(CraftPenalty {
            rule: "hashtag-overload".to_string(),
            penalty: PENALTY_HASHTAG_OVERLOAD,
            evidence: format!("{count} hashtags (limit {})", config.max_hashtags),
        })
    } else {
        None
    }
}

/// Emoji overload is noise.
fn penalty_emoji_overload(text: &str, config: &CraftConfig) -> Option<CraftPenalty> {
    let count = text
        .chars()
        .filter(|character| is_emoji(*character))
        .count();
    if count > config.max_emojis {
        Some(CraftPenalty {
            rule: "emoji-overload".to_string(),
            penalty: PENALTY_EMOJI_OVERLOAD,
            evidence: format!("{count} emojis (limit {})", config.max_emojis),
        })
    } else {
        None
    }
}

// --- Text helpers ----------------------------------------------------------

/// Number of leading words that define a post's "opening".
const OPENING_WORDS: usize = 3;

/// n-gram size used for the structure signature.
const SIGNATURE_GRAM: usize = 3;

/// Nouns that mark an English listicle headline.
const LISTICLE_NOUNS: [&str; 8] = [
    "things", "ways", "tips", "reasons", "facts", "lessons", "tricks", "steps",
];

/// Lowercased alphanumeric word tokens (accents preserved).
fn tokenize(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The n-gram signature of a post; short posts collapse to one whole-text gram.
fn signature(tokens: &[String]) -> Vec<Vec<String>> {
    match tokens.len() {
        0 | 1 => Vec::new(),
        2 => vec![tokens.to_vec()],
        _ => tokens
            .windows(SIGNATURE_GRAM)
            .map(<[String]>::to_vec)
            .collect(),
    }
}

/// Jaccard similarity between two n-gram sets.
fn jaccard(left: &[Vec<String>], right: &[Vec<String>]) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let shared = left.iter().filter(|gram| right.contains(gram)).count();
    let union = left.len() + right.len() - shared;
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

/// The first [`OPENING_WORDS`] tokens.
fn opening(tokens: &[String]) -> Vec<String> {
    tokens.iter().take(OPENING_WORDS).cloned().collect()
}

/// Split into sentences on terminal punctuation and newlines.
fn sentences(text: &str) -> Vec<String> {
    text.split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when the post's meaningful tail is a question mark.
fn ends_with_question(text: &str) -> bool {
    text.trim_end_matches(|character: char| {
        character.is_whitespace() || matches!(character, '"' | '\'' | ')' | '”' | '’')
    })
    .ends_with('?')
}

/// True when the post is shaped like a listicle.
fn is_listicle(text: &str) -> bool {
    let tokens = tokenize(text);
    if let Some(first) = tokens.first() {
        if first.parse::<u32>().is_ok() {
            return true;
        }
        if let Some(second) = tokens.get(1)
            && LISTICLE_NOUNS.contains(&second.as_str())
        {
            return true;
        }
    }
    text.lines()
        .filter(|line| is_numbered_marker(line.trim()))
        .count()
        >= 2
}

/// True when a line starts with a numbered marker ("1.", "2)" or "3-").
fn is_numbered_marker(line: &str) -> bool {
    let digits = line
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digits == 0 {
        return false;
    }
    matches!(line.chars().nth(digits), Some('.') | Some(')') | Some('-'))
}

/// A short, single-line preview of a post for evidence strings.
fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    let mut preview: String = trimmed.chars().take(48).collect();
    if trimmed.chars().count() > 48 {
        preview.push('…');
    }
    preview
}

/// Broad emoji ranges, mirroring the guardrail's policy.
fn is_emoji(character: char) -> bool {
    let code = character as u32;
    (0x1F000..=0x1FAFF).contains(&code)
        || (0x2600..=0x27BF).contains(&code)
        || (0x2B00..=0x2BFF).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CraftConfig {
        CraftConfig::default()
    }

    #[test]
    fn clean_post_has_no_penalties() {
        let score = craft_score(
            "The kernel scheduler got a rewrite this cycle. Latency dropped on my box.",
            &[],
            &config(),
        );
        assert!(score.is_clean(), "penalties: {:?}", score.penalties);
        assert_eq!(score.score, 1.0);
    }

    #[test]
    fn repeated_structure_is_penalised() {
        let recent = ["Rust 1.85 ships async closures for everyone in stable now"];
        let score = craft_score(
            "Rust 1.85 ships async closures for everyone in stable today",
            &recent,
            &config(),
        );
        assert!(
            score
                .penalties
                .iter()
                .any(|p| p.rule == "repeated-structure"),
            "penalties: {:?}",
            score.penalties
        );
    }

    #[test]
    fn repeated_opening_is_penalised_with_evidence() {
        let recent = ["The trick is to read the man page before the blog post"];
        let score = craft_score(
            "The trick is to benchmark before trusting the microbenchmark",
            &recent,
            &config(),
        );
        let penalty = score
            .penalties
            .iter()
            .find(|p| p.rule == "repeated-opening")
            .expect("opening penalty");
        assert!(penalty.evidence.contains("the trick is"));
    }

    #[test]
    fn em_dash_density_is_penalised_above_the_limit() {
        let text = "one — two — three — four — five";
        let score = craft_score(text, &[], &config());
        assert!(
            score.penalties.iter().any(|p| p.rule == "em-dash-density"),
            "penalties: {:?}",
            score.penalties
        );
    }

    #[test]
    fn a_single_em_dash_is_tolerated() {
        let score = craft_score("a single dash — here is fine", &[], &config());
        assert!(!score.penalties.iter().any(|p| p.rule == "em-dash-density"));
    }

    #[test]
    fn flat_sentence_lengths_are_penalised() {
        let text = "one two three. four five six. seven eight nine.";
        let score = craft_score(text, &[], &config());
        assert!(
            score
                .penalties
                .iter()
                .any(|p| p.rule == "flat-sentence-length"),
            "penalties: {:?}",
            score.penalties
        );
    }

    #[test]
    fn varied_sentence_lengths_are_clean() {
        let text = "Short. Then a much longer sentence that keeps going for a while. Tiny.";
        let score = craft_score(text, &[], &config());
        assert!(
            !score
                .penalties
                .iter()
                .any(|p| p.rule == "flat-sentence-length")
        );
    }

    #[test]
    fn always_a_question_is_penalised() {
        let recent = ["Is the new scheduler faster?", "Did you try the rewrite?"];
        let score = craft_score("Should I upgrade today?", &recent, &config());
        assert!(
            score
                .penalties
                .iter()
                .any(|p| p.rule == "always-a-question")
        );
    }

    #[test]
    fn mixed_endings_are_not_penalised() {
        let recent = ["Is the new scheduler faster?", "The rewrite landed today."];
        let score = craft_score("Should I upgrade today?", &recent, &config());
        assert!(
            !score
                .penalties
                .iter()
                .any(|p| p.rule == "always-a-question")
        );
    }

    #[test]
    fn hashtag_overload_is_penalised() {
        let score = craft_score("hello #a #b #c", &[], &config());
        let penalty = score
            .penalties
            .iter()
            .find(|p| p.rule == "hashtag-overload")
            .expect("hashtag penalty");
        assert!(penalty.evidence.contains("3 hashtags"));
    }

    #[test]
    fn emoji_overload_is_penalised() {
        let score = craft_score("ship it 🚀🔥🎉", &[], &config());
        assert!(score.penalties.iter().any(|p| p.rule == "emoji-overload"));
    }

    #[test]
    fn repeated_listicle_is_penalised() {
        let recent = ["5 things about the Linux scheduler"];
        let score = craft_score("3 ways to profile a Rust program", &recent, &config());
        assert!(
            score
                .penalties
                .iter()
                .any(|p| p.rule == "repeated-listicle")
        );
    }

    #[test]
    fn a_lone_listicle_is_not_penalised() {
        let score = craft_score("3 ways to profile a Rust program", &[], &config());
        assert!(
            !score
                .penalties
                .iter()
                .any(|p| p.rule == "repeated-listicle")
        );
    }

    #[test]
    fn penalties_stack() {
        let recent = ["The trick is to read the docs before the blog post"];
        let text =
            "The trick is to read the docs before the blog post — always — again #a #b #c 🚀🔥🎉";
        let score = craft_score(text, &recent, &config());
        assert!(
            score.penalties.len() >= 3,
            "penalties: {:?}",
            score.penalties
        );
        assert!(score.total_penalty > 0.5);
        assert!(score.score < 0.5);
    }

    #[test]
    fn score_clamps_at_zero() {
        let config = CraftConfig {
            rubric_base: 0.5,
            ..CraftConfig::default()
        };
        let recent = ["The trick is to read the docs before the blog post"];
        let text =
            "The trick is to read the docs before the blog post — always — again #a #b #c 🚀🔥🎉";
        let score = craft_score(text, &recent, &config);
        assert_eq!(score.score, 0.0);
        assert!(score.total_penalty > config.rubric_base);
    }

    #[test]
    fn avoid_entries_deduplicate_and_carry_evidence() {
        let texts = [
            "5 things about the Linux scheduler",
            "5 things about the Rust borrow checker",
        ];
        let entries = avoid_entries(&texts, &config());
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|entry| !entry.evidence.is_empty()));
    }

    #[test]
    fn craft_floor_gates_high_penalties() {
        let recent = ["The trick is to read the man page first"];
        let sloppy = craft_score(
            "The trick is to always benchmark before trusting numbers",
            &recent,
            &config(),
        );
        assert!(!sloppy.meets(config().craft_floor));
        assert!(craft_score("A clean short note.", &[], &config()).meets(config().craft_floor));
    }
}

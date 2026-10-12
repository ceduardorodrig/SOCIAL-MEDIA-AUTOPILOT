//! Stage [3.2] — editorial review: a sceptical editor rewrites the drafts.
//!
//! This stage is the **human-like counterpart of the deterministic slop
//! penalties** documented in `scoring-model.md` (§2): the learning stage
//! measures the AI tells, this stage rewrites them away. It runs after creation
//! ([3]) and before the guardrail ([3.5]), so the guardrail always validates the
//! *polished* text and never the first draft.
//!
//! The editor is a **different role from the creator**: it does not invent a
//! post, it edits one. It receives the persona's voice rules, the source topic
//! and the draft, and must preserve the facts — no new claims, no invented
//! numbers — while removing the shapes that read as machine-written.
//!
//! As with the other stages, the fragile contract (the JSON shape we ask the
//! model for) is split into a pure part — prompt building and response parsing —
//! and an effectful part (the LLM call, which lives in `autopilot-providers`),
//! so the whole contract is unit-tested without ever calling an LLM.

use anyhow::{Context, Result};
use autopilot_core::{PersonaPack, Platform};
use serde::{Deserialize, Serialize};

use crate::create::Variant;
use crate::json::extract_json;
use crate::prompt::{platform_limit, render_banned_phrases, render_guidelines};

/// One per-variant review as returned by the editor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditedVariant {
    /// Label of the draft this review belongs to (`A`/`B`).
    pub label: String,
    /// Rewritten caption; empty when the draft was rejected.
    #[serde(default)]
    pub text: String,
    /// Short list of what the editor changed (for the journal's before/after).
    #[serde(default)]
    pub changes: Vec<String>,
    /// One-line reason when the editor could not salvage the draft.
    #[serde(default)]
    pub rejected_reason: String,
}

impl EditedVariant {
    /// True when the editor could not salvage the draft.
    pub fn is_rejected(&self) -> bool {
        !self.rejected_reason.trim().is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct Wrapper {
    edits: Vec<EditedVariant>,
}

/// The review outcome for one original draft.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorOutcome {
    /// The editor rewrote the draft; this variant is what the guardrail must see.
    Polished {
        variant: Variant,
        changes: Vec<String>,
    },
    /// The editor could not salvage the draft without inventing facts.
    Rejected { label: String, reason: String },
    /// No usable rewrite for this draft: the original is kept unchanged.
    Unchanged { variant: Variant },
}

impl EditorOutcome {
    /// Label of the draft this outcome refers to.
    pub fn label(&self) -> &str {
        match self {
            EditorOutcome::Polished { variant, .. } | EditorOutcome::Unchanged { variant } => {
                &variant.label
            }
            EditorOutcome::Rejected { label, .. } => label,
        }
    }

    /// What the editor changed, when the draft was polished.
    pub fn changes(&self) -> &[String] {
        match self {
            EditorOutcome::Polished { changes, .. } => changes,
            _ => &[],
        }
    }

    /// The rejection reason, when the draft was rejected.
    pub fn rejection(&self) -> Option<&str> {
        match self {
            EditorOutcome::Rejected { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

/// Keep only the variants the guardrail must see (rejected drafts are dropped).
pub fn kept_variants(outcomes: Vec<EditorOutcome>) -> Vec<Variant> {
    outcomes
        .into_iter()
        .filter_map(|outcome| match outcome {
            EditorOutcome::Polished { variant, .. } | EditorOutcome::Unchanged { variant } => {
                Some(variant)
            }
            EditorOutcome::Rejected { .. } => None,
        })
        .collect()
}

/// Build the editorial review prompt for one platform and its drafts.
///
/// The editor is told to hunt the same tells the deterministic craft score
/// penalises (repeated openings, flat sentence length, always-a-question,
/// em-dash density, listicles, hashtag/emoji overload), and is bound by two hard
/// rules: preserve the facts and respect the platform limit.
pub fn build_editor_prompt(
    persona: &PersonaPack,
    platform: Platform,
    topic: &str,
    variants: &[Variant],
) -> String {
    let drafts = variants
        .iter()
        .map(|variant| format!("- label {}: {}", variant.label, variant.text.trim()))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are the editorial review stage of an autonomous social-media engine.\n\
         You are NOT the writer: you are a sceptical editor. Rewrite each draft so it reads\n\
         like a person wrote it, without changing what it says.\n\n\
         Persona: {display_name} ({niche})\n\
         Language: {language}\n\
         Platform: {platform} (hard limit {limit} characters, at most {max_tags} hashtags)\n\
         Tone: {tone}\n\
         Audience: {audience}\n\
         Voice rules to preserve:\n{guidelines}\n\
         Never use these phrases: {banned}\n\
         Never touch these topics: {blocklist}\n\n\
         Source topic: {topic}\n\n\
         Drafts to review:\n{drafts}\n\n\
         Hunt every AI tell and rewrite it away:\n\
         - templated openings (\"Did you know...\", \"Here's the trick...\", \"Let's dive in\")\n\
         - uniform sentence length: vary the rhythm, mix short and long sentences\n\
         - every post ending with a question: land on a statement when it is stronger\n\
         - em-dash density above one per 40 words\n\
         - listicle shape (\"5 things about X\") unless the topic really is a list\n\
         - hype words (unbelievable, mind-blowing, revolutionary, secret)\n\
         - over-hashtagging (more than {max_tags}) and emoji spam\n\n\
         Non-negotiable rules:\n\
         - Preserve the facts: no new claims, no invented numbers, no new sources.\n\
         - Keep the same language and the same core message.\n\
         - Respect the platform limit of {limit} characters.\n\
         - When a voice rule above conflicts with a tell, keep the voice rule and vary how\n\
           you satisfy it instead of dropping it.\n\
         - If a draft cannot be salvaged without inventing facts, reject it.\n\n\
         Reply with ONLY a JSON array (no prose, no markdown) of objects with the keys\n\
         \"label\" (matching the draft), \"text\" (the rewritten caption, empty when rejected),\n\
         \"changes\" (short array of what you changed) and \"rejected_reason\" (one line, empty\n\
         when the draft was rewritten).\n",
        display_name = persona.display_name,
        niche = persona.niche,
        language = persona.language,
        platform = platform.as_str(),
        limit = platform_limit(platform),
        max_tags = persona.voice.max_hashtags,
        tone = persona.voice.tone,
        audience = persona.voice.audience,
        guidelines = render_guidelines(persona),
        banned = render_banned_phrases(persona),
        blocklist = persona.blocklist.join("; "),
        topic = topic,
        drafts = drafts,
    )
}

/// Parse the editor response into per-variant reviews.
///
/// An empty list is an error rather than "everything was rejected": it is far
/// more likely a truncated answer, and the caller then degrades to the original
/// drafts instead of dropping every post.
pub fn parse_editor_response(raw: &str) -> Result<Vec<EditedVariant>> {
    let json = extract_json(raw)?;
    let edits: Vec<EditedVariant> = match serde_json::from_str(json) {
        Ok(edits) => edits,
        Err(_) => {
            let wrapper: Wrapper = serde_json::from_str(json)
                .context("invalid editor JSON (expected an array or {\"edits\": [...]})")?;
            wrapper.edits
        }
    };

    if edits.is_empty() {
        anyhow::bail!("the editor returned no edits");
    }
    Ok(edits)
}

/// Fold the editor's response into the original drafts.
///
/// `edits` is `None` when the editor was unavailable (call or parse failure):
/// every draft is then kept unchanged, so a transient error never costs a post
/// and the guardrail still runs on the original text. A draft the editor did not
/// mention, or mentioned without a usable rewrite, is also kept unchanged —
/// a partial answer never silently drops a post.
pub fn review_or_keep(
    originals: &[Variant],
    edits: Option<&[EditedVariant]>,
) -> Vec<EditorOutcome> {
    originals
        .iter()
        .map(|original| match find_edit(edits, &original.label) {
            Some(edit) if edit.is_rejected() => EditorOutcome::Rejected {
                label: original.label.clone(),
                reason: edit.rejected_reason.trim().to_string(),
            },
            Some(edit) if !edit.text.trim().is_empty() => EditorOutcome::Polished {
                variant: rewrite(original, &edit.text),
                changes: edit.changes.clone(),
            },
            _ => EditorOutcome::Unchanged {
                variant: original.clone(),
            },
        })
        .collect()
}

/// Find the review for one draft label (trimmed, case-insensitive).
fn find_edit<'a>(edits: Option<&'a [EditedVariant]>, label: &str) -> Option<&'a EditedVariant> {
    let wanted = label.trim();
    edits?
        .iter()
        .find(|edit| edit.label.trim().eq_ignore_ascii_case(wanted))
}

/// Rebuild a variant from the polished text, preserving everything else.
///
/// The label, the hashtags and the image prompt are carried over: only the
/// caption is the editor's job, and the image prompt still drives the post card.
fn rewrite(original: &Variant, text: &str) -> Variant {
    Variant {
        label: original.label.clone(),
        text: text.trim().to_string(),
        hashtags: original.hashtags.clone(),
        image_prompt: original.image_prompt.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona() -> PersonaPack {
        crate::test_support::sample_persona_with_voice()
    }

    fn draft(label: &str, text: &str) -> Variant {
        Variant {
            label: label.into(),
            text: text.into(),
            hashtags: vec!["#ciencia".into()],
            image_prompt: "a brain".into(),
        }
    }

    #[test]
    fn prompt_carries_voice_facts_and_the_ai_tells() {
        let prompt = build_editor_prompt(
            &persona(),
            Platform::Bluesky,
            "o cérebro gasta energia",
            &[draft("A", "Did you know the brain uses energy?")],
        );
        assert!(prompt.contains("editorial review"));
        assert!(prompt.contains("NOT the writer"));
        assert!(prompt.contains("pt-BR"));
        assert!(prompt.contains("bluesky"));
        assert!(prompt.contains("300"));
        assert!(prompt.contains("comece com um gancho"));
        assert!(prompt.contains("você não vai acreditar"));
        assert!(prompt.contains("terraplanismo"));
        assert!(prompt.contains("o cérebro gasta energia"));
        assert!(prompt.contains("label A: Did you know"));
        assert!(prompt.contains("Did you know..."));
        assert!(prompt.contains("Preserve the facts"));
        assert!(prompt.contains("no invented numbers"));
    }

    #[test]
    fn parses_a_polished_review() {
        let raw = "```json\n[{\"label\":\"A\",\"text\":\"O cérebro gasta energia.\",\
                   \"changes\":[\"removed the templated opener\"],\"rejected_reason\":\"\"}]\n```";
        let edits = parse_editor_response(raw).expect("parses");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].text, "O cérebro gasta energia.");
        assert_eq!(
            edits[0].changes,
            vec!["removed the templated opener".to_string()]
        );
        assert!(!edits[0].is_rejected());
    }

    #[test]
    fn parses_a_rejected_review() {
        let raw = "[{\"label\":\"B\",\"text\":\"\",\"changes\":[],\
                   \"rejected_reason\":\"every fact is invented\"}]";
        let edits = parse_editor_response(raw).expect("parses");
        assert!(edits[0].is_rejected());
        assert_eq!(edits[0].rejected_reason, "every fact is invented");
    }

    #[test]
    fn accepts_a_wrapper_object() {
        let raw = "{\"edits\":[{\"label\":\"A\",\"text\":\"polished\"}]}";
        let edits = parse_editor_response(raw).expect("parses");
        assert_eq!(edits.len(), 1);
        assert!(edits[0].changes.is_empty());
    }

    #[test]
    fn malformed_or_empty_responses_are_errors() {
        assert!(parse_editor_response("no structured output here").is_err());
        assert!(parse_editor_response("[]").is_err());
    }

    #[test]
    fn review_replaces_the_text_and_preserves_the_rest() {
        let originals = vec![draft("A", "Did you know?")];
        let edits = vec![EditedVariant {
            label: "a".into(),
            text: "  O cérebro gasta energia.  ".into(),
            changes: vec!["cut the hook".into()],
            rejected_reason: String::new(),
        }];
        let outcomes = review_or_keep(&originals, Some(&edits));
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].label(), "A");
        assert_eq!(outcomes[0].changes(), ["cut the hook".to_string()]);
        let kept = kept_variants(outcomes);
        assert_eq!(kept[0].text, "O cérebro gasta energia.");
        assert_eq!(kept[0].hashtags, vec!["#ciencia".to_string()]);
        assert_eq!(kept[0].image_prompt, "a brain");
    }

    #[test]
    fn a_rejected_draft_is_dropped() {
        let originals = vec![draft("A", "first"), draft("B", "second")];
        let edits = vec![EditedVariant {
            label: "A".into(),
            text: String::new(),
            changes: vec![],
            rejected_reason: "unsalvageable".into(),
        }];
        let outcomes = review_or_keep(&originals, Some(&edits));
        assert_eq!(outcomes[0].rejection(), Some("unsalvageable"));
        let kept = kept_variants(outcomes);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].label, "B");
        assert_eq!(kept[0].text, "second");
    }

    #[test]
    fn a_missing_or_empty_edit_keeps_the_original() {
        let originals = vec![draft("A", "original A"), draft("B", "original B")];
        let edits = vec![EditedVariant {
            label: "A".into(),
            text: "   ".into(),
            changes: vec![],
            rejected_reason: String::new(),
        }];
        let kept = kept_variants(review_or_keep(&originals, Some(&edits)));
        assert_eq!(kept[0].text, "original A");
        assert_eq!(kept[1].text, "original B");
    }

    #[test]
    fn an_unavailable_editor_keeps_every_original() {
        let originals = vec![draft("A", "original A"), draft("B", "original B")];
        let outcomes = review_or_keep(&originals, None);
        assert!(outcomes.iter().all(|outcome| outcome.changes().is_empty()));
        let kept = kept_variants(outcomes);
        assert_eq!(kept, originals);
    }
}

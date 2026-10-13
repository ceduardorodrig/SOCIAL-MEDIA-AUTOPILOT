//! Claim-vs-source grounding review for the drafts about to be published.
//!
//! The creation stage writes from a harvested topic, and until now it wrote from
//! the **title alone** — so the body came out of the model's memory. That is the
//! same invention risk the engagement stage showed when it shipped a fabricated
//! command on 2026-10-12, but on the publishing path, where it is far more
//! visible: a post is permanent and public.
//!
//! Two changes close it:
//!
//! 1. The source **content** travels into the creation prompt as the only
//!    citable context (see [`crate::build_creation_prompt`]).
//! 2. This layer asks a **different role on the same chain** — a sceptical
//!    fact-checker, never the writer — whether every claim in a draft is
//!    supported by that source.
//!
//! It is **fail-closed**: if the review cannot run, nothing is published. Silence
//! costs one slot; a fabrication costs credibility. It is also *generic* — it
//! judges the claim, not the wording, so no word list can go stale.

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::create::Variant;
use crate::json::extract_json;

/// The harvested source a post must be grounded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source<'a> {
    /// Where the topic came from (the harvest result).
    pub url: &'a str,
    /// What the harvest actually read — the only context a post may cite.
    pub content: &'a str,
}

/// One draft's verdict from the grounding review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundingVerdict {
    pub label: String,
    /// Whether every claim in the draft is supported by the source.
    pub supported: bool,
    /// One line: the unsupported claim, or why the draft passes.
    pub reason: String,
}

#[derive(Deserialize)]
struct RawVerdict {
    label: String,
    supported: bool,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Wrapper {
    verdicts: Vec<RawVerdict>,
}

/// Build the prompt for the grounding reviewer.
///
/// The reviewer is told it is not the writer, is given the source as the *only*
/// admissible evidence, and must judge each draft separately so one bad variant
/// does not cost the slot.
pub fn build_grounding_prompt(source: Source<'_>, variants: &[Variant]) -> String {
    let drafts = variants
        .iter()
        .map(|variant| format!("[{}] {}", variant.label, variant.text))
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "You are the fact-checking layer of an autonomous social-media engine. You are NOT\n\
         the writer and NOT a copy-editor: your only job is to decide, for each draft below,\n\
         whether every claim it makes is SUPPORTED by the source.\n\n\
         The ONLY admissible source (url: {url}):\n\"{content}\"\n\n\
         The drafts:\n{drafts}\n\n\
         A draft is NOT supported if it asserts anything the source does not contain: a\n\
         number, a date, a version, a name, a price, a quote, a comparison, or a causal\n\
         claim. General knowledge is NOT admissible evidence — if the source does not say\n\
         it, the draft may not either. A draft that stays inside the source, or that only\n\
         states an opinion about it, is supported.\n\
         Judge each draft separately. When in doubt, mark it NOT supported: silence costs\n\
         one slot, a fabrication costs credibility.\n\n\
         Answer with JSON only: [{{\"label\": \"A\", \"supported\": true|false, \"reason\": \"<one line>\"}}]",
        url = source.url,
        content = source.content,
        drafts = drafts,
    )
}

/// Parse the reviewer's answer into per-draft verdicts.
pub fn parse_grounding(raw: &str) -> Result<Vec<GroundingVerdict>> {
    let json = extract_json(raw)?;
    let raw_verdicts: Vec<RawVerdict> = match serde_json::from_str(json) {
        Ok(verdicts) => verdicts,
        Err(_) => serde_json::from_str::<Wrapper>(json)
            .map(|wrapper| wrapper.verdicts)
            .map_err(|_| anyhow::anyhow!("no grounding verdicts in the answer"))?,
    };

    if raw_verdicts.is_empty() {
        bail!("the reviewer returned no verdicts");
    }

    let mut verdicts = Vec::new();
    for raw_verdict in raw_verdicts {
        let reason = raw_verdict.reason.trim().to_string();
        if !raw_verdict.supported && reason.is_empty() {
            bail!("a rejection must carry a reason");
        }
        verdicts.push(GroundingVerdict {
            label: raw_verdict.label.trim().to_string(),
            supported: raw_verdict.supported,
            reason,
        });
    }
    Ok(verdicts)
}

/// The labels the review says are safe to publish.
///
/// A draft the reviewer never mentioned is **not** safe: a truncated answer must
/// never be read as approval.
pub fn supported_labels(verdicts: &[GroundingVerdict]) -> Vec<&str> {
    verdicts
        .iter()
        .filter(|verdict| verdict.supported)
        .map(|verdict| verdict.label.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(label: &str, text: &str) -> Variant {
        Variant {
            label: label.to_string(),
            text: text.to_string(),
            hashtags: Vec::new(),
            image_prompt: String::new(),
        }
    }

    fn source() -> Source<'static> {
        Source {
            url: "https://example.org/gnome-fingerprints",
            content: "GNOME 51 improved the user interface for managing fingerprints.",
        }
    }

    #[test]
    fn the_prompt_gives_the_source_as_the_only_evidence() {
        let prompt = build_grounding_prompt(source(), &[variant("A", "GNOME 51 tidied it up.")]);
        assert!(prompt.contains("ONLY admissible source"));
        assert!(prompt.contains("https://example.org/gnome-fingerprints"));
        assert!(prompt.contains("[A] GNOME 51 tidied it up."));
        // The reviewer is not the writer: it must never be asked to rewrite.
        assert!(prompt.contains("the writer and NOT a copy-editor"));
        assert!(prompt.contains("When in doubt, mark it NOT supported"));
    }

    #[test]
    fn verdicts_parse_from_a_plain_array() {
        let raw = r#"[{"label":"A","supported":true,"reason":"inside the source"},
                       {"label":"B","supported":false,"reason":"invents a version number"}]"#;
        let verdicts = parse_grounding(raw).expect("parses");
        assert_eq!(verdicts.len(), 2);
        assert_eq!(supported_labels(&verdicts), vec!["A"]);
    }

    #[test]
    fn a_wrapped_answer_is_tolerated() {
        let raw = r#"{"verdicts":[{"label":"A","supported":false,"reason":"adds a date"}]}"#;
        let verdicts = parse_grounding(raw).expect("parses");
        assert_eq!(verdicts.len(), 1);
        assert!(supported_labels(&verdicts).is_empty());
    }

    #[test]
    fn an_unmentioned_draft_is_not_safe() {
        // A truncated answer must never be read as approval.
        let verdicts =
            parse_grounding(r#"[{"label":"A","supported":true,"reason":"ok"}]"#).expect("parses");
        assert_eq!(supported_labels(&verdicts), vec!["A"]);
        assert!(!supported_labels(&verdicts).contains(&"B"));
    }

    #[test]
    fn a_rejection_without_a_reason_is_an_error() {
        assert!(parse_grounding(r#"[{"label":"A","supported":false}]"#).is_err());
    }

    #[test]
    fn an_empty_or_chatty_answer_is_an_error() {
        assert!(parse_grounding("[]").is_err());
        assert!(parse_grounding("looks fine to me").is_err());
    }
}

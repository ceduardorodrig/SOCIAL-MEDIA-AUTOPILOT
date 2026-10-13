//! The generic grounding review for a reply we are about to send a stranger.
//!
//! The deterministic layers can only refuse a shape they already know: a banned
//! phrase, a character count, a URL. A model that is unsure *invents* a
//! plausible specific — a command, a package, a version, a date — and no word
//! list can enumerate what does not exist. Hardcoding those patterns makes the
//! gate brittle in both directions: it misses the next invention and rejects
//! honest prose that happens to look like one.
//!
//! So the last layer before we speak is a **different role on the same chain**:
//! a sceptical fact-checker that judges whether the reply is *grounded in the
//! post it answers*. It judges the claim, not the wording — which is what makes
//! it generic. It is deliberately **fail-closed**: if the review cannot run, the
//! reply is withheld, because silence costs nothing and a fabrication costs
//! credibility.

use anyhow::{Result, bail};
use serde::Deserialize;

/// The reviewer's verdict on one reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// Whether the reply may be sent.
    pub approved: bool,
    /// One line explaining the verdict (journaled, never posted).
    pub reason: String,
}

#[derive(Deserialize)]
struct RawReview {
    approved: bool,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Wrapper {
    review: RawReview,
}

/// Build the prompt for the grounding reviewer.
///
/// The reviewer is told exactly what "grounded" means and, crucially, that it is
/// not the writer or a copy-editor: its only job is the claim, so it never
/// rewrites and never judges style.
pub fn build_review_prompt(parent: &str, reply: &str) -> String {
    format!(
        "You are the fact-checking layer of an autonomous social-media engine. You are NOT\n\
         the writer and NOT a copy-editor: your only job is to decide whether the reply is\n\
         GROUNDED in the post it answers.\n\n\
         The post being answered:\n\"{parent}\"\n\n\
         The reply:\n\"{reply}\"\n\n\
         REJECT the reply if it asserts any of these:\n\
         - a specific the post does not contain and that is not widely known general\n\
           knowledge: a command, flag, package, file path, version, date, statistic or\n\
           proper noun;\n\
         - a claim about the author, their intent or their history that the post does\n\
           not state;\n\
         - a paraphrase that misrepresents what the post says;\n\
         - generic praise or bare agreement with nothing added.\n\
         APPROVE a reply that stays inside what the post says, or that speaks only from\n\
         personal experience without asserting checkable facts about the world.\n\
         When in doubt, REJECT: silence costs nothing, a fabrication costs credibility.\n\n\
         Answer with JSON only: {{\"approved\": true|false, \"reason\": \"<one line>\"}}"
    )
}

/// Parse the reviewer's answer into a verdict.
pub fn parse_review(raw: &str) -> Result<ReviewOutcome> {
    let json = extract_json(raw);
    let review: RawReview = match serde_json::from_str(json) {
        Ok(review) => review,
        Err(_) => serde_json::from_str::<Wrapper>(json)
            .map(|wrapper| wrapper.review)
            .map_err(|_| anyhow::anyhow!("no review verdict in the answer"))?,
    };
    let reason = review.reason.trim().to_string();
    if !review.approved && reason.is_empty() {
        bail!("a rejection must carry a reason");
    }
    Ok(ReviewOutcome {
        approved: review.approved,
        reason,
    })
}

/// Take the JSON object out of a fenced or chatty answer.
fn extract_json(raw: &str) -> &str {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let body = body.trim().trim_end_matches("```").trim();
    match (body.find('{'), body.rfind('}')) {
        (Some(start), Some(end)) if end > start => &body[start..=end],
        _ => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_states_the_grounding_rule_and_the_fail_closed_default() {
        let prompt = build_review_prompt("In GNOME 51 we improved the fingerprint UI.", "Neat.");
        assert!(prompt.contains("GROUNDED"));
        assert!(prompt.contains("command, flag, package, file path, version"));
        assert!(prompt.contains("When in doubt, REJECT"));
        // The reviewer is not the writer: it must never be asked to rewrite.
        assert!(prompt.contains("the writer and NOT a copy-editor"));
    }

    #[test]
    fn a_clean_approval_parses() {
        let outcome = parse_review(r#"{"approved": true, "reason": "stays inside the post"}"#)
            .expect("parses");
        assert!(outcome.approved);
        assert_eq!(outcome.reason, "stays inside the post");
    }

    #[test]
    fn a_rejection_parses_from_a_fenced_answer() {
        let raw = "```json\n{\"approved\": false, \"reason\": \"invents a command name\"}\n```";
        let outcome = parse_review(raw).expect("parses");
        assert!(!outcome.approved);
        assert_eq!(outcome.reason, "invents a command name");
    }

    #[test]
    fn a_wrapped_verdict_is_tolerated() {
        let outcome =
            parse_review(r#"{"review": {"approved": false, "reason": "misreads the post"}}"#)
                .expect("parses");
        assert!(!outcome.approved);
    }

    #[test]
    fn a_rejection_without_a_reason_is_an_error() {
        assert!(parse_review(r#"{"approved": false}"#).is_err());
    }

    #[test]
    fn a_chatty_answer_with_no_verdict_is_an_error() {
        assert!(parse_review("I think it is probably fine?").is_err());
    }
}

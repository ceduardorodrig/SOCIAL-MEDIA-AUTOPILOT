//! The deterministic, **structural** floor for a proactive reply.
//!
//! A proactive reply is the riskiest thing this stage does: we are answering a
//! stranger nobody asked us to answer. Two layers guard it, and they are kept
//! deliberately separate:
//!
//! - **This module** refuses only what is exact and cheap to see: a reply too
//!   short or too thin to say anything, one that carries a link, or one that
//!   names our own account. Those are structural facts about the text — no word
//!   list, no judgement, nothing that goes stale when a model changes.
//! - **[`crate::review`]** judges what needs judgement: whether the reply is
//!   *grounded* in the post, whether it adds something, whether it invents a
//!   specific. That is a semantic question, so it is asked of a sceptical
//!   reviewer rather than encoded as patterns — a hardcoded list would miss the
//!   next invention and reject honest prose that merely resembles one.
//!
//! It runs after the shared guardrail (blocklist, banned phrases, platform
//! limits) and never replaces it.

use autopilot_guardrail::{Issue, Verdict};

/// Screen a proactive reply against the structural floor.
///
/// `our_tokens` are the strings that must never appear (our handles, DID and
/// display name), so the reply can never turn into a plug.
pub fn screen_proactive_reply(
    text: &str,
    our_tokens: &[String],
    min_chars: usize,
    min_words: usize,
) -> Verdict {
    let mut verdict = Verdict::new();
    let normalized = normalize(text);
    let words: Vec<&str> = normalized.split_whitespace().collect();

    if text.chars().count() < min_chars {
        verdict = reject(
            verdict,
            "QUALITY-TOO-SHORT",
            format!("fewer than {min_chars} characters to add anything"),
        );
    }
    if words.len() < min_words {
        verdict = reject(
            verdict,
            "QUALITY-TOO-THIN",
            format!("fewer than {min_words} words to make a point"),
        );
    }
    if contains_link(text) {
        verdict = reject(
            verdict,
            "QUALITY-LINK",
            "a proactive reply must never carry a link",
        );
    }
    if let Some(token) = names_our_account(text, our_tokens) {
        verdict = reject(
            verdict,
            "QUALITY-SELF-MENTION",
            format!("the reply names our own account (`{token}`) — never a plug"),
        );
    }

    verdict
}

fn reject(mut verdict: Verdict, rule: &str, detail: impl Into<String>) -> Verdict {
    verdict.approved = false;
    verdict.issues.push(Issue {
        rule: rule.to_string(),
        detail: detail.into(),
    });
    verdict
}

/// Lowercase, collapse whitespace and drop punctuation (keeping `@`, `.`, `-`
/// and digits, which matter for handles and versions).
fn normalize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() || matches!(c, '@' | '.' | '-' | '_' | '\'')
            {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A URL or an obvious link shortener/domain.
fn contains_link(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("www.")
        || lower.contains("://")
        || lower.contains("bsky.app")
        || lower.contains("t.co/")
}

/// The first of our own tokens that appears in the reply.
fn names_our_account(text: &str, our_tokens: &[String]) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    our_tokens.iter().find_map(|token| {
        let token = token.trim().trim_start_matches('@').to_ascii_lowercase();
        if token.chars().count() < 4 {
            return None;
        }
        lower.contains(&token).then_some(token)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> Vec<String> {
        vec![
            "example.bsky.social".to_string(),
            "Tech Curiosities".to_string(),
        ]
    }

    fn screen(text: &str) -> Verdict {
        screen_proactive_reply(text, &tokens(), 40, 8)
    }

    #[test]
    fn a_substantive_reply_passes() {
        let verdict = screen(
            "I ran that scheduler on an old ThinkPad and idle latency improved, but throughput was flat.",
        );
        assert!(verdict.approved, "issues: {:?}", verdict.issues);
    }

    #[test]
    fn a_short_or_thin_reply_is_rejected() {
        let verdict = screen("Try the 6.9 patch.");
        assert!(!verdict.approved);
        assert!(
            verdict
                .issues
                .iter()
                .any(|issue| issue.rule == "QUALITY-TOO-SHORT" || issue.rule == "QUALITY-TOO-THIN")
        );
    }

    #[test]
    fn a_link_is_rejected() {
        let verdict = screen("The benchmark is here: https://example.com/bench and it is telling.");
        assert!(!verdict.approved);
        assert!(
            verdict
                .issues
                .iter()
                .any(|issue| issue.rule == "QUALITY-LINK")
        );
    }

    #[test]
    fn naming_our_own_account_is_rejected() {
        let verdict =
            screen("You should follow @example.bsky.social for more on this kernel change.");
        assert!(!verdict.approved);
        assert!(
            verdict
                .issues
                .iter()
                .any(|issue| issue.rule == "QUALITY-SELF-MENTION")
        );
    }

    #[test]
    fn the_verdict_is_deterministic() {
        let text = "The 6.9 scheduler mainly helps idle latency, not throughput.";
        assert_eq!(screen(text), screen(text));
    }

    #[test]
    fn semantics_are_left_to_the_grounding_review() {
        // This module is the *structural* floor: praise and ungrounded claims
        // are judgement calls and belong to `review.rs`. Keeping them out here is
        // the point — a word list here would be exactly the hardcoding that
        // misses the next invention.
        let praise = screen("Great post, thanks for sharing this with everyone today!");
        assert!(
            praise.approved,
            "the floor does not judge meaning: {:?}",
            praise.issues
        );
    }
}

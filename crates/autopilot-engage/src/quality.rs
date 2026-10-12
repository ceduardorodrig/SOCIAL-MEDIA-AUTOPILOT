//! Deterministic quality bar for a **proactive** reply.
//!
//! A proactive reply is the riskiest thing this stage does: we are answering a
//! stranger nobody asked us to answer. The owner's rule is blunt — it must
//! *contribute to the conversation*, never look like a plug. The prompt asks the
//! model for that; this module is the deterministic half that refuses a reply
//! that only flatters, only agrees, links out, or names our own account.
//!
//! It is intentionally heuristic and cheap. It runs after the shared guardrail
//! (blocklist, banned phrases, length) and never replaces it.

use autopilot_guardrail::{Issue, Verdict};

/// Generic praise and filler that reads as a bot's "great post".
const LOW_EFFORT: &[&str] = &[
    "great post",
    "great point",
    "nice post",
    "good post",
    "great share",
    "thanks for sharing",
    "thank you for sharing",
    "well said",
    "well put",
    "so true",
    "spot on",
    "love this",
    "this is great",
    "this is amazing",
    "amazing work",
    "awesome",
    "exactly this",
    "could not agree more",
    "couldn't agree more",
    "couldn't have said it better",
    "totally agree",
    "agree 100%",
    "following for more",
];

/// Words that carry no substance on their own. A reply whose content words are
/// all in here is agreement-only, however long it is.
const FILLER: &[&str] = &[
    "a",
    "an",
    "the",
    "i",
    "you",
    "we",
    "they",
    "he",
    "she",
    "it",
    "this",
    "that",
    "these",
    "those",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "am",
    "do",
    "does",
    "did",
    "have",
    "has",
    "had",
    "and",
    "or",
    "but",
    "so",
    "if",
    "of",
    "to",
    "in",
    "on",
    "at",
    "for",
    "with",
    "about",
    "from",
    "my",
    "your",
    "our",
    "their",
    "his",
    "her",
    "its",
    "me",
    "us",
    "them",
    "very",
    "really",
    "just",
    "also",
    "too",
    "much",
    "totally",
    "completely",
    "agree",
    "agreed",
    "yes",
    "yep",
    "true",
    "exactly",
    "indeed",
    "absolutely",
    "definitely",
    "correct",
    "right",
    "same",
    "post",
    "comment",
    "take",
    "point",
    "thought",
    "opinion",
    "thing",
    "stuff",
    "here",
    "there",
    "well",
    "good",
    "great",
    "nice",
    "thanks",
    "thank",
    "welcome",
];

/// Screen a proactive reply against the quality bar.
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
    if is_low_effort(&normalized) {
        verdict = reject(
            verdict,
            "QUALITY-LOW-EFFORT",
            "generic praise — the reply adds nothing to the conversation",
        );
    }
    if is_agreement_only(&normalized) {
        verdict = reject(
            verdict,
            "QUALITY-AGREEMENT-ONLY",
            "the reply only agrees; it must add a fact, a correction or a real question",
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

/// The reply is (or opens with) generic praise and little else.
fn is_low_effort(normalized: &str) -> bool {
    let words = normalized.split_whitespace().count();
    LOW_EFFORT.iter().any(|phrase| {
        normalized == *phrase
            || (normalized.starts_with(phrase)
                && normalized[phrase.len()..].starts_with(' ')
                && words <= 12)
    })
}

/// Every content word is filler (agreement, pronouns, articles): nothing added.
fn is_agreement_only(normalized: &str) -> bool {
    let content = normalized
        .split_whitespace()
        .filter(|word| !FILLER.contains(word))
        .count();
    content < 3
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
    fn generic_praise_is_rejected() {
        for text in ["Great post!", "great post", "So true, thanks for sharing!"] {
            let verdict = screen(text);
            assert!(!verdict.approved, "should reject: {text}");
            assert!(
                verdict
                    .issues
                    .iter()
                    .any(|issue| issue.rule.starts_with("QUALITY-LOW-EFFORT")
                        || issue.rule == "QUALITY-AGREEMENT-ONLY"),
                "unexpected issues for {text}: {:?}",
                verdict.issues
            );
        }
    }

    #[test]
    fn agreement_only_is_rejected_even_when_long() {
        let verdict = screen("I totally agree with this take, it is really very true and correct.");
        assert!(!verdict.approved);
        assert!(
            verdict
                .issues
                .iter()
                .any(|issue| issue.rule == "QUALITY-AGREEMENT-ONLY")
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
    fn the_verdict_is_deterministic() {
        let text = "The 6.9 scheduler mainly helps idle latency, not throughput.";
        assert_eq!(screen(text), screen(text));
    }
}

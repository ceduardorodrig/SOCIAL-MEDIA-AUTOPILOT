//! Reply drafting: build an in-context prompt, parse the model's answer, and
//! screen it through the deterministic guardrail.
//!
//! A reply is content. It is generated through the shared LLM chain and it must
//! pass the same deterministic guardrail as a published post — the blocklist,
//! banned phrases, hashtag policy and platform limit are never bypassed.

use anyhow::{Result, bail};
use autopilot_core::{PersonaPack, Platform};
use autopilot_guardrail::{Verdict, evaluate, platform_limit};

use crate::candidates::{Candidate, CandidateKind};

/// Build the prompt that asks the chain for one in-context reply.
pub fn build_reply_prompt(persona: &PersonaPack, candidate: &Candidate) -> String {
    let guidelines = if persona.voice.guidelines.is_empty() {
        "- none".to_string()
    } else {
        persona
            .voice
            .guidelines
            .iter()
            .map(|rule| format!("- {rule}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let banned = if persona.voice.banned_phrases.is_empty() {
        "none".to_string()
    } else {
        persona.voice.banned_phrases.join("; ")
    };
    let limit = platform_limit(Platform::Bluesky);

    let proactive = match candidate.kind {
        CandidateKind::Proactive => {
            "\n\
             You are joining a stranger's conversation to CONTRIBUTE, never to advertise.\n\
             Reply only if you can add something specific about the post above.\n\
             Anchor the reply to what the post actually says, so it reads as a reply and not\n\
             as a fragment: do not open with a bare assertion about the topic.\n\
             Never state a specific you cannot ground in the post or in general knowledge you\n\
             are certain of. A wrong detail costs more than a plainer sentence.\n\
             Write plain text only: no code, no markdown.\n"
        }
        _ => "",
    };

    format!(
        "You are the engagement stage of an autonomous social-media engine.\n\
         Write ONE short reply in {language} on Bluesky, as {display_name} ({niche}).\n\n\
         Tone: {tone}\n\
         Audience: {audience}\n\
         Style rules:\n{guidelines}\n\
         Never use these phrases: {banned}\n\
         Never touch these topics: {blocklist}\n\n\
         {author} wrote ({kind}):\n\"{parent}\"\n\n\
         Reply to THAT specific post: engage with its actual point and add something\n\
         (a fact, a correction, a concrete experience or a real question), staying in context.\n\
         Quality bar: never send a generic acknowledgement (\"Thanks!\", \"Great post!\"),\n\
         never agree without adding anything, and never turn the reply into a plug.\n\
         Never include a link, never name or mention our own account ({display_name}),\n\
         never add hashtags. Reply with the plain text only — no quotes, no markdown —\n\
         at most {limit} characters.{proactive}",
        language = persona.language,
        display_name = persona.display_name,
        niche = persona.niche,
        tone = persona.voice.tone,
        audience = persona.voice.audience,
        guidelines = guidelines,
        banned = banned,
        blocklist = persona.blocklist.join("; "),
        author = candidate.author_handle,
        kind = candidate.kind.as_str(),
        parent = candidate.text,
        limit = limit,
        proactive = proactive,
    )
}

/// Parse the model's answer into a clean, single-line reply.
pub fn parse_reply(raw: &str) -> Result<String> {
    let cleaned = clean(raw);
    if cleaned.is_empty() {
        bail!("the model returned an empty reply");
    }
    Ok(cleaned)
}

/// Screen a candidate reply through the deterministic guardrail.
pub fn screen_reply(persona: &PersonaPack, text: &str) -> Verdict {
    evaluate(persona, Platform::Bluesky, text)
}

fn clean(raw: &str) -> String {
    let body = strip_fences(raw.trim());
    let body = strip_quotes(body.trim());
    let body = strip_leading_mention(body.trim());
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop a ```` ```json ```` fence around the answer, if present.
fn strip_fences(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix("```") else {
        return raw;
    };
    let body = match rest.find('\n') {
        Some(index) => &rest[index + 1..],
        None => rest,
    };
    body.strip_suffix("```").unwrap_or(body)
}

/// Strip matched surrounding quotes (straight and typographic).
fn strip_quotes(raw: &str) -> &str {
    let mut text = raw;
    loop {
        let before = text;
        for (open, close) in [('"', '"'), ('\'', '\''), ('“', '”'), ('‘', '’')] {
            if let Some(inner) = text
                .strip_prefix(open)
                .and_then(|rest| rest.strip_suffix(close))
            {
                text = inner.trim();
            }
        }
        if text == before {
            return text;
        }
    }
}

/// Drop a leading `@handle` token; the reply ref already targets the author.
fn strip_leading_mention(raw: &str) -> &str {
    match raw.strip_prefix('@') {
        Some(rest) => match rest.find(char::is_whitespace) {
            Some(index) => rest[index..].trim_start(),
            None => "",
        },
        None => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::records::StrongRef;

    /// Build the persona from YAML so this test does not break when the persona
    /// pack grows a new (defaulted) field — the pack is data, not a struct literal.
    fn persona() -> PersonaPack {
        let yaml = r#"
persona: tech-curiosities
display_name: Tech Curiosities
niche: computing-technology-ai-linux-foss
language: en
voice:
  tone: a curious engineer talking to peers
  audience: developers and sysadmins
  guidelines: ["write like a person on a forum"]
  banned_phrases: ["game-changer"]
  max_hashtags: 2
  emojis_allowed: true
blocklist: ["crypto/token shilling"]
rubric:
  min_score: 0.75
channels:
  - platform: bluesky
    handle: example.bsky.social
    enabled: true
    posts_per_day: 2
"#;
        serde_yaml_ng::from_str(yaml).expect("persona fixture parses")
    }

    fn candidate() -> Candidate {
        Candidate {
            kind: crate::candidates::CandidateKind::Mention,
            parent: StrongRef::new("at://p", "cid"),
            root: StrongRef::new("at://p", "cid"),
            author_did: "did:plc:them".into(),
            author_handle: "them.bsky.social".into(),
            text: "What do you think about the new kernel scheduler?".into(),
            score: 30,
        }
    }

    #[test]
    fn prompt_carries_the_parent_context_and_constraints() {
        let prompt = build_reply_prompt(&persona(), &candidate());
        assert!(prompt.contains("them.bsky.social"));
        assert!(prompt.contains("new kernel scheduler"));
        assert!(prompt.contains("en"));
        assert!(prompt.contains("game-changer"));
        assert!(prompt.contains("crypto/token shilling"));
        assert!(prompt.contains("mention"));
    }

    #[test]
    fn the_proactive_prompt_states_the_no_plug_rule() {
        let mut proactive = candidate();
        proactive.kind = CandidateKind::Proactive;
        let prompt = build_reply_prompt(&persona(), &proactive);
        assert!(prompt.contains("proactive"));
        assert!(prompt.contains("never to advertise"));
        assert!(prompt.contains("never name or mention our own account"));

        // A mention reply keeps the quality bar but not the stranger wording.
        let mention = build_reply_prompt(&persona(), &candidate());
        assert!(!mention.contains("never to advertise"));
        assert!(mention.contains("Quality bar"));
    }

    #[test]
    fn parse_reply_strips_fences_quotes_and_mentions() {
        assert_eq!(
            parse_reply("```\n\"@them.bsky.social  I doubt it.\"\n```").expect("parses"),
            "I doubt it."
        );
        assert_eq!(
            parse_reply("“That benchmark is old.”").expect("parses"),
            "That benchmark is old."
        );
        assert_eq!(
            parse_reply("@x hello there").expect("parses"),
            "hello there"
        );
        assert_eq!(parse_reply("plain reply").expect("parses"), "plain reply");
    }

    #[test]
    fn parse_reply_rejects_empty_output() {
        assert!(parse_reply("   ").is_err());
        assert!(parse_reply("@handle").is_err());
    }

    #[test]
    fn guardrail_screens_a_blocked_reply() {
        let verdict = screen_reply(&persona(), "buy this token now, crypto/token shilling");
        assert!(!verdict.approved);
        assert!(
            verdict
                .issues
                .iter()
                .any(|issue| issue.rule == "GUARD-BLOCKLIST")
        );
    }

    #[test]
    fn guardrail_approves_a_clean_reply() {
        let verdict = screen_reply(
            &persona(),
            "The new scheduler is interesting, but the benchmarks I saw were on older hardware.",
        );
        assert!(verdict.approved, "issues: {:?}", verdict.issues);
    }

    #[test]
    fn guardrail_rejects_a_reply_over_the_platform_limit() {
        let long = "a".repeat(platform_limit(Platform::Bluesky) + 1);
        let verdict = screen_reply(&persona(), &long);
        assert!(
            verdict
                .issues
                .iter()
                .any(|issue| issue.rule == "GUARD-LENGTH")
        );
    }
}

//! Stage [2] — triage: score harvested topics against the persona.

use anyhow::{Context, Result};
use autopilot_core::PersonaPack;
use autopilot_harvest::Topic;
use serde::{Deserialize, Serialize};

use crate::json::extract_json;

/// A trend candidate scored against the persona.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicScore {
    pub title: String,
    #[serde(default)]
    pub url: String,
    pub score: f32,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
struct Wrapper {
    topics: Vec<TopicScore>,
}

/// Build the triage prompt for the persona and a batch of topics.
pub fn build_triage_prompt(persona: &PersonaPack, topics: &[Topic]) -> String {
    let mut candidates = String::new();
    for (index, topic) in topics.iter().enumerate() {
        let content: String = topic.content.chars().take(200).collect();
        candidates.push_str(&format!(
            "{}. title: {}\n   url: {}\n   excerpt: {}\n",
            index + 1,
            topic.title,
            topic.url,
            content
        ));
    }

    format!(
        "You are the triage stage of an autonomous social-media engine.\n\
         Persona niche: {niche}\n\
         Audience: {audience}\n\
         Tone: {tone}\n\
         Forbidden topics (reject any candidate touching them): {blocklist}\n\n\
         Score each candidate for how well it fits this persona and how likely it is to\n\
         perform on social media right now. Use a score between 0.0 and 1.0.\n\n\
         Candidates:\n{candidates}\n\
         Reply with ONLY a JSON array (no prose, no markdown) of objects with the keys\n\
         \"title\", \"url\", \"score\" and \"reason\". Sort by score descending and keep at\n\
         most {max} entries.\n",
        niche = persona.niche,
        audience = persona.voice.audience,
        tone = persona.voice.tone,
        blocklist = persona.blocklist.join("; "),
        candidates = candidates,
        max = 5,
    )
}

/// Parse the model response into a shortlist sorted by score.
pub fn parse_shortlist(raw: &str) -> Result<Vec<TopicScore>> {
    let json = extract_json(raw)?;
    let mut scores: Vec<TopicScore> = match serde_json::from_str(json) {
        Ok(scores) => scores,
        Err(_) => {
            let wrapper: Wrapper = serde_json::from_str(json)
                .context("invalid shortlist JSON (expected an array or {\"topics\": [...]})")?;
            wrapper.topics
        }
    };

    for score in &scores {
        if !(0.0..=1.0).contains(&score.score) {
            anyhow::bail!("score out of range (0.0..=1.0): {}", score.score);
        }
    }
    scores.sort_by(|left, right| right.score.total_cmp(&left.score));
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopilot_core::{Channel, Platform, Rubric, Voice};

    fn persona() -> PersonaPack {
        PersonaPack {
            persona: "ciencia".into(),
            display_name: "Ciência".into(),
            niche: "science-curiosities".into(),
            language: "pt-BR".into(),
            active: false,
            voice: Voice {
                tone: "curioso".into(),
                audience: "adultos curiosos".into(),
                guidelines: vec![],
                banned_phrases: vec![],
                max_hashtags: 4,
                emojis_allowed: true,
            },
            few_shot: vec![],
            blocklist: vec!["terraplanismo".into()],
            rubric: Rubric {
                min_score: 0.75,
                criteria: vec![],
            },
            channels: vec![Channel {
                platform: Platform::Bluesky,
                handle: "x".into(),
                enabled: true,
                posts_per_day: 1,
                windows: vec![],
            }],
        }
    }

    fn topics() -> Vec<Topic> {
        vec![
            Topic {
                title: "Descoberta sobre o cérebro".into(),
                url: "https://a.example".into(),
                content: "excerpt".into(),
                engine: "google".into(),
            },
            Topic {
                title: "Outro tema".into(),
                url: "https://b.example".into(),
                content: "excerpt".into(),
                engine: "google".into(),
            },
        ]
    }

    #[test]
    fn prompt_carries_persona_and_candidates() {
        let prompt = build_triage_prompt(&persona(), &topics());
        assert!(prompt.contains("science-curiosities"));
        assert!(prompt.contains("terraplanismo"));
        assert!(prompt.contains("Descoberta sobre o cérebro"));
        assert!(prompt.contains("JSON"));
    }

    #[test]
    fn parses_fenced_array_and_sorts_desc() {
        let raw = "```json\n[\
            {\"title\":\"low\",\"url\":\"u\",\"score\":0.2,\"reason\":\"r\"},\
            {\"title\":\"high\",\"url\":\"u\",\"score\":0.9,\"reason\":\"r\"}]\n```";
        let shortlist = parse_shortlist(raw).expect("parses");
        assert_eq!(shortlist.len(), 2);
        assert_eq!(shortlist[0].title, "high");
        assert_eq!(shortlist[1].title, "low");
    }

    #[test]
    fn accepts_wrapper_object() {
        let raw = "{\"topics\":[{\"title\":\"a\",\"url\":\"u\",\"score\":0.5,\"reason\":\"r\"}]}";
        let shortlist = parse_shortlist(raw).expect("parses");
        assert_eq!(shortlist.len(), 1);
    }

    #[test]
    fn rejects_out_of_range_scores() {
        let raw = "[{\"title\":\"a\",\"url\":\"u\",\"score\":7.5,\"reason\":\"r\"}]";
        assert!(parse_shortlist(raw).is_err());
    }
}

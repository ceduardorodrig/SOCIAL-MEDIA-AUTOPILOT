//! Tolerant JSON extraction from model responses.

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Extract the first JSON value (object or array) from a model response.
///
/// Tolerates ```` ```json ```` fences and surrounding prose, which is how models
/// usually wrap structured output despite being told not to.
pub fn extract_json(raw: &str) -> Result<&str> {
    let body = strip_fences(raw);
    let start = body.find(['{', '[']);
    let end = body.rfind(['}', ']']);
    match (start, end) {
        (Some(start), Some(end)) if end > start => Ok(&body[start..=end]),
        _ => bail!(
            "no JSON value found in the model response; answer: {}",
            answer_preview(body)
        ),
    }
}

/// How much of a model answer an error message keeps.
///
/// The raw answer is persisted nowhere else, so without this a contract breach is
/// undiagnosable: the operator reads the serde complaint and learns nothing about
/// the shape that produced it (HB-126, 2026-11-19 — four occurrences of one
/// breach read as four different bugs for that reason).
const ANSWER_PREVIEW: usize = 240;

/// The first [`ANSWER_PREVIEW`] characters of `raw`, whitespace-collapsed.
pub fn answer_preview(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= ANSWER_PREVIEW {
        return collapsed;
    }
    let head: String = collapsed.chars().take(ANSWER_PREVIEW).collect();
    format!("{head}…")
}

fn strip_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // Drop the opening fence line (```json) and the closing fence.
    let body = match rest.find('\n') {
        Some(index) => &rest[index + 1..],
        None => rest,
    };
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// A member that carries the label it answers to.
///
/// A model told to answer `[{"label":"A",…}]` also answers `{"A":{…},"B":{…}}`;
/// the label then lives in the map key, and adopting it is what lets one
/// normalizer serve every stage that joins on labels.
pub trait LabelSlot {
    fn label_slot(&mut self) -> &mut String;
}

/// Adopt the map key as the member's own label when it arrived keyed.
///
/// A key that contradicts the member's own field is an error, never a silent
/// rewrite: `label` is the join key for the editor, the grounding review and
/// publication, so guessing here would drift exactly what this exists to pin.
pub fn adopt_labels<T: LabelSlot>(members: Vec<(Option<String>, T)>) -> Result<Vec<T>> {
    members
        .into_iter()
        .map(|(key, mut member)| {
            let Some(key) = key else {
                return Ok(member);
            };
            let key = key.trim().to_string();
            let current = member.label_slot().trim().to_string();
            let slot = member.label_slot();
            if current.is_empty() {
                *slot = key;
            } else if !current.eq_ignore_ascii_case(&key) {
                bail!("the answer keyed a member `{key}` but its own label says `{current}`");
            } else {
                *slot = current;
            }
            Ok(member)
        })
        .collect()
}

/// The members of the list the prompt asked for, resolved by **shape**.
///
/// THE REASON THIS EXISTS (HB-126, 2026-11-19): every stage parser tried exactly
/// two envelopes — a bare array and `{key: [...]}` — and discarded the first
/// serde error. A model answering `[{ "A": {…}, "B": {…} }]` failed both, and the
/// journal blamed "a map where a sequence was expected" at the wrong coordinate
/// while the publishing slot was lost. The accepted shapes now live in one named
/// function, tried in order:
///
/// 1. `{ "<key>": … }` — the documented envelope, unwrapped;
/// 2. an array whose every item is a `T` — the members, in the model's order;
/// 3. an array holding exactly one other value — the accidental wrapper;
/// 4. an object whose every value is a `T` — the same members keyed by label, in
///    key order, so the A/B order never depends on the model's key order;
/// 5. anything else — an error naming the kind that arrived and a bounded
///    preview of the answer.
///
/// Rule 2 precedes rule 3, so a legitimate single-member array is never
/// unwrapped by mistake.
pub fn list_members<T: DeserializeOwned>(
    value: Value,
    key: &str,
) -> Result<Vec<(Option<String>, T)>> {
    members_by_shape(value, key, 2)
}

fn members_by_shape<T: DeserializeOwned>(
    value: Value,
    key: &str,
    depth: usize,
) -> Result<Vec<(Option<String>, T)>> {
    match value {
        Value::Array(items) => members_of_array(items, key, depth),
        Value::Object(map) => members_of_object(map, key, depth),
        other => bail!(
            "the answer is {} where an array or an object was expected; answer: {}",
            kind_of(&other),
            answer_preview(&other.to_string()),
        ),
    }
}

fn members_of_array<T: DeserializeOwned>(
    items: Vec<Value>,
    key: &str,
    depth: usize,
) -> Result<Vec<(Option<String>, T)>> {
    let len = items.len();
    if let Some(members) = every_item_is_a_member(&items) {
        return Ok(members);
    }
    if len == 1
        && depth > 0
        && let Some(only) = items.first().cloned()
    {
        // The accidental wrapper: `[{ "A": {…}, "B": {…} }]`.
        return members_by_shape(only, key, depth - 1);
    }
    bail!(
        "the answer is an array of {len} item(s), none of them a member of the expected shape; answer: {}",
        answer_preview(&Value::Array(items).to_string()),
    )
}

fn members_of_object<T: DeserializeOwned>(
    map: serde_json::Map<String, Value>,
    key: &str,
    depth: usize,
) -> Result<Vec<(Option<String>, T)>> {
    if let Some(inner) = map.get(key).cloned() {
        if depth == 0 {
            bail!("the answer nests `{key}` inside `{key}` — the envelope cannot be resolved");
        }
        return members_by_shape(inner, key, depth - 1);
    }

    let mut names = map.keys().cloned().collect::<Vec<_>>();
    names.sort();

    let mut members = Vec::with_capacity(names.len());
    for name in names {
        let Some(value) = map.get(&name).cloned() else {
            continue;
        };
        match serde_json::from_value::<T>(value) {
            Ok(member) => members.push((Some(name), member)),
            Err(err) => bail!(
                "the answer is an object keyed by `{name}`, but that value is not a member: {err}"
            ),
        }
    }
    if members.is_empty() {
        bail!("the answer is an empty object; expected at least one member");
    }
    Ok(members)
}

fn every_item_is_a_member<T: DeserializeOwned>(
    items: &[Value],
) -> Option<Vec<(Option<String>, T)>> {
    let mut members = Vec::with_capacity(items.len());
    for item in items {
        let member = serde_json::from_value::<T>(item.clone()).ok()?;
        members.push((None, member));
    }
    Some(members)
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn extracts_bare_object() {
        assert_eq!(extract_json("{\"a\":1}").expect("object"), "{\"a\":1}");
    }

    #[test]
    fn extracts_from_prose_and_fences() {
        let raw = "Sure! Here is the JSON:\n```json\n[{\"a\": 1}]\n```\nHope it helps.";
        assert_eq!(extract_json(raw).expect("array"), "[{\"a\": 1}]");
    }

    #[test]
    fn fails_when_no_json_present() {
        assert!(extract_json("no structured output here").is_err());
    }

    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Member {
        label: String,
        text: String,
    }

    impl LabelSlot for Member {
        fn label_slot(&mut self) -> &mut String {
            &mut self.label
        }
    }

    fn members(raw: &str) -> Result<Vec<Member>> {
        let value: Value = serde_json::from_str(raw).context("fixture is JSON")?;
        adopt_labels(list_members::<Member>(value, "variants")?)
    }

    #[test]
    fn accepts_the_documented_envelope() {
        let parsed = members(r#"{"variants":[{"label":"A","text":"t"},{"label":"B","text":"u"}]}"#)
            .expect("envelope");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].label, "B");
    }

    #[test]
    fn accepts_a_bare_array() {
        let parsed = members(r#"[{"label":"A","text":"t"}]"#).expect("array");
        assert_eq!(parsed.len(), 1, "a one-member array is not an envelope");
        assert_eq!(parsed[0].label, "A");
    }

    #[test]
    fn accepts_the_map_keyed_by_label() {
        // The real 2026-11-19 answer: an array wrapping the A/B map.
        let parsed = members(r#"[{"B":{"label":"B","text":"u"},"A":{"label":"A","text":"t"}}]"#)
            .expect("keyed map inside an array");
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed
                .iter()
                .map(|member| member.label.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"],
            "key order decides, not the model's key order"
        );
    }

    #[test]
    fn accepts_a_flat_map_keyed_by_label() {
        let parsed = members(r#"{"B":{"label":"B","text":"u"},"A":{"label":"A","text":"t"}}"#)
            .expect("flat keyed map");
        assert_eq!(parsed[0].label, "A");
    }

    #[test]
    fn the_map_key_fills_an_empty_label_and_never_contradicts_one() {
        let filled = members(r#"{"A":{"label":"","text":"t"}}"#).expect("filled from the key");
        assert_eq!(filled[0].label, "A");

        let contradictory = members(r#"{"A":{"label":"B","text":"t"}}"#);
        assert!(
            contradictory.is_err(),
            "a key that disagrees with the field is a parse error, not a guess"
        );
    }

    #[test]
    fn an_incoherent_object_is_named_in_the_error() {
        let err = members(r#"{"label":"A","text":"t"}"#)
            .err()
            .map(|err| format!("{err:#}"))
            .unwrap_or_default();
        assert!(err.contains("keyed by `label`"), "got: {err}");
    }

    #[test]
    fn the_error_carries_a_bounded_preview_of_the_answer() {
        let long = "x".repeat(ANSWER_PREVIEW * 2);
        let err = members(&format!(
            r#"[{{"label":"A","text":"{long}"}}, {{"nope":1}}]"#
        ))
        .err()
        .map(|err| format!("{err:#}"))
        .unwrap_or_default();
        assert!(err.contains("array of 2 item(s)"), "got: {err}");
        assert!(
            err.chars().count() < ANSWER_PREVIEW + 200,
            "the preview is bounded: {} chars",
            err.chars().count()
        );
    }
}

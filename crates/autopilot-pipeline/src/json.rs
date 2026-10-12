//! Tolerant JSON extraction from model responses.

use anyhow::{Result, bail};

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
        _ => bail!("no JSON value found in the model response"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

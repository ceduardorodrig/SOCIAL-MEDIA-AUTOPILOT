//! Minimal tolerant JSON extraction for model responses.
//!
//! The creation stage is an LLM: even when told to answer with bare JSON it
//! tends to wrap the payload in ```` ```json ```` fences or in a sentence. This
//! helper recovers the first complete JSON object/array from such a response.
//!
//! It is a small local reimplementation on purpose: depending on
//! `autopilot-pipeline` would couple the media module to the text pipeline.

use anyhow::{Result, bail};

/// Extract the first complete JSON object or array from a model response.
///
/// Unlike a naive `find('{')..rfind('}')` slice, this walks the text and matches
/// brackets while ignoring brackets that live inside JSON strings, so trailing
/// prose (which may itself contain braces) cannot corrupt the extracted slice.
pub fn extract_json(raw: &str) -> Result<&str> {
    let body = strip_code_fence(raw);
    let bytes = body.as_bytes();

    let Some(start) = bytes.iter().position(|byte| *byte == b'{' || *byte == b'[') else {
        bail!("no JSON object or array found in the response");
    };

    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&body[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }

    bail!("unbalanced JSON in the response")
}

/// Drop an optional ```` ```json ```` code fence and surrounding whitespace.
fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = rest.split_once('\n').map_or(rest, |(_, after)| after);
    body.strip_suffix("```").map_or(body, str::trim).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_bare_object() {
        assert_eq!(extract_json("{\"a\":1}").expect("object"), "{\"a\":1}");
    }

    #[test]
    fn ignores_trailing_prose_containing_braces() {
        let raw = "{\"a\":\"}\"}\n\nThat is the object {I promise}.";
        assert_eq!(extract_json(raw).expect("object"), "{\"a\":\"}\"}");
    }

    #[test]
    fn extracts_from_a_fenced_block() {
        let raw = "Sure!\n```json\n[{\"a\": 1}]\n```\nDone.";
        assert_eq!(extract_json(raw).expect("array"), "[{\"a\": 1}]");
    }

    #[test]
    fn fails_without_json() {
        assert!(extract_json("nothing structured here").is_err());
    }

    #[test]
    fn fails_on_unbalanced_json() {
        assert!(extract_json("{\"a\": 1").is_err());
    }
}

//! Fetch the source article a post is grounded in.
//!
//! The harvest returns a SearXNG **snippet** — one or two sentences. A post
//! written against that has to invent the rest, and the [3.7] grounding review
//! then refuses it (verified live on 2026-10-13: both drafts rejected, nothing
//! published). The snippet is too thin to be a source; the *article* is.
//!
//! So the runner fetches the selected topic's page once per run and hands its
//! text to the creation prompt and to the grounding review. The conversion is a
//! pure function ([`html_to_text`]) so it is unit-tested without any network.

use std::time::Duration;

use anyhow::{Context, Result};

/// How long to wait for one article fetch.
pub const ARTICLE_TIMEOUT: Duration = Duration::from_secs(20);

/// How much article text to keep. The prompt is the constraint, not the page:
/// beyond a few thousand characters the model stops reading and starts skimming.
pub const MAX_SOURCE_CHARS: usize = 6000;

/// Fetch `url` and return its readable text, truncated to `max_chars`.
pub async fn fetch_article_text(url: &str, timeout: Duration, max_chars: usize) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent("autopilot/1.0 (+homelab; grounding review)")
        .build()
        .context("building the article HTTP client")?;

    let body = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} answered with an error status"))?
        .text()
        .await
        .with_context(|| format!("reading the body of {url}"))?;

    let text = html_to_text(&body, max_chars);
    if text.trim().is_empty() {
        anyhow::bail!("{url} produced no readable text");
    }
    Ok(text)
}

/// Extract readable text from an HTML document.
///
/// Deliberately a small state machine rather than a parser crate: it only has to
/// be good enough to ground a post, and being pure keeps it unit-testable. It
/// drops `<script>`/`<style>`/`<noscript>`/`<svg>` and the comments, turns the
/// block-level tags into line breaks, strips the remaining tags, decodes the
/// common entities, collapses the whitespace and truncates.
pub fn html_to_text(html: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut chars = html.chars().peekable();
    // Tag name of the block we are skipping, if any.
    let mut skipping: Option<&'static str> = None;

    while let Some(c) = chars.next() {
        if c != '<' {
            if skipping.is_none() {
                // Whitespace in an HTML text node is insignificant — the browser
                // collapses it. Only the block tags below may break a line.
                out.push(if c.is_whitespace() { ' ' } else { c });
            }
            continue;
        }

        // A comment: `<!-- ... -->`.
        if chars.clone().take(3).collect::<String>() == "!--" {
            for _ in 0..3 {
                chars.next();
            }
            let mut window = [' '; 3];
            for c in chars.by_ref() {
                window.rotate_left(1);
                window[2] = c;
                if window == ['-', '-', '>'] {
                    break;
                }
            }
            continue;
        }

        // A tag: read its name and whether it closes.
        let mut closing = false;
        if chars.peek() == Some(&'/') {
            closing = true;
            chars.next();
        }
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() {
                name.push(c.to_ascii_lowercase());
                chars.next();
            } else {
                break;
            }
        }

        // Skip to the end of the tag, remembering a block we must ignore.
        for c in chars.by_ref() {
            if c == '>' {
                break;
            }
        }

        match (closing, name.as_str()) {
            (false, "script") => skipping = Some("script"),
            (false, "style") => skipping = Some("style"),
            (false, "noscript") => skipping = Some("noscript"),
            (false, "svg") => skipping = Some("svg"),
            (true, "script") if skipping == Some("script") => skipping = None,
            (true, "style") if skipping == Some("style") => skipping = None,
            (true, "noscript") if skipping == Some("noscript") => skipping = None,
            (true, "svg") if skipping == Some("svg") => skipping = None,
            _ => {}
        }

        // Block-level tags become a break so the text keeps its shape.
        if !closing
            && matches!(
                name.as_str(),
                "p" | "div"
                    | "br"
                    | "li"
                    | "ul"
                    | "ol"
                    | "tr"
                    | "td"
                    | "th"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "article"
                    | "section"
                    | "header"
                    | "footer"
                    | "pre"
            )
        {
            out.push('\n');
        }
    }

    truncate(&collapse_whitespace(&decode_entities(&out)), max_chars)
}

/// Decode the entities an article body actually uses.
fn decode_entities(text: &str) -> String {
    let named = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        ("&mdash;", "—"),
        ("&ndash;", "–"),
        ("&hellip;", "…"),
        ("&rsquo;", "'"),
        ("&lsquo;", "'"),
        ("&rdquo;", "\""),
        ("&ldquo;", "\""),
    ];

    let mut out = text.to_string();
    for (entity, replacement) in named {
        out = out.replace(entity, replacement);
    }
    out
}

/// Collapse runs of spaces and blank lines while keeping paragraph breaks.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_newline = false;
    let mut started = false;

    for line in text.lines() {
        let words = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if words.is_empty() {
            if started {
                pending_newline = true;
            }
            continue;
        }
        if pending_newline {
            out.push('\n');
            pending_newline = false;
        }
        if started {
            out.push('\n');
        }
        out.push_str(&words);
        started = true;
    }
    out
}

/// Truncate on a character boundary, at a word boundary when one is near.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    match cut.rfind(' ') {
        Some(index) if index > max_chars / 2 => cut[..index].to_string(),
        _ => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_drops_scripts_styles_and_comments() {
        let html = "<p>Real text</p><script>var x = 1; steal();</script><style>p{color:red}</style><!-- a comment --><p>More</p>";
        let text = html_to_text(html, 1000);
        assert!(text.contains("Real text"));
        assert!(text.contains("More"));
        assert!(!text.contains("steal"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("a comment"));
    }

    #[test]
    fn it_keeps_paragraph_shape_and_decodes_entities() {
        let html = "<div><p>First &amp; second</p><p>Third &mdash; done</p></div>";
        let text = html_to_text(html, 1000);
        assert_eq!(text, "First & second\nThird — done");
    }

    #[test]
    fn it_collapses_whitespace() {
        let html = "<p>  lots\n\n   of\t\tspace  </p>";
        assert_eq!(html_to_text(html, 1000), "lots of space");
    }

    #[test]
    fn it_truncates_on_a_word_boundary() {
        let html = format!("<p>{}</p>", "word ".repeat(50));
        let text = html_to_text(&html, 40);
        assert!(text.chars().count() <= 40);
        assert!(!text.ends_with("wor"), "must not cut mid-word: {text:?}");
    }

    #[test]
    fn an_empty_or_tag_only_document_is_empty() {
        assert_eq!(html_to_text("", 100), "");
        assert_eq!(html_to_text("<div><span></span></div>", 100), "");
    }

    #[test]
    fn an_unclosed_script_does_not_leak_its_body() {
        // A truncated page must not dump minified JavaScript into the source.
        let html = "<p>Intro</p><script>var a=1; function b(){";
        let text = html_to_text(html, 1000);
        assert_eq!(text, "Intro");
    }
}

//! Deterministic post-card generator (no GPU, no heavy image crate).
//!
//! A "post card" is the classic tech-account visual: the post's topic as a title,
//! the post text as body copy, rendered over the persona's palette. It is built
//! from a **versioned SVG template** ([`CARD_TEMPLATE_VERSION`], shipped in
//! `assets/card-v1.svg`) whose placeholders are filled with escaped, wrapped text
//! and then rasterised to PNG by `rsvg-convert`.
//!
//! Why an SVG template + `rsvg-convert` instead of a Rust image crate?
//!
//! * the visual is data (a reviewable, diffable, versioned file), not code;
//! * `rsvg-convert` is already installed on the host and does the rasterisation
//!   (cairo/pango/harfbuzz), so we add **no** dependency for font shaping or PNG
//!   encoding — `resvg`/`image`/`ab_glyph` would each be a large tree for a job
//!   the system tool already does;
//! * the same filled SVG is written next to the PNG, so a re-render is diffable
//!   even when the PNG bytes are not byte-identical across tool versions.
//!
//! Everything that decides the output (wrapping, truncation, escaping, template
//! filling, PNG header parsing) is a pure function and unit-tested; only
//! [`render_png`] spawns a process.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Version of the SVG template. Bump it when `assets/card-vN.svg` changes.
pub const CARD_TEMPLATE_VERSION: &str = "card-v1";

/// The template, embedded at compile time so the binary is self-contained.
const CARD_TEMPLATE: &str = include_str!("../assets/card-v1.svg");

/// Card width in pixels. Square 1:1: it is the aspect the Bluesky app renders
/// without letterboxing and the one that keeps the same asset usable on the
/// square-crop channels that follow (Instagram feed). Bluesky takes the real
/// dimensions from `aspectRatio`, so 1:1 is a deliberate framing choice, not a
/// constraint of the platform.
pub const CARD_WIDTH: u32 = 1200;

/// Card height in pixels (square — see [`CARD_WIDTH`]).
pub const CARD_HEIGHT: u32 = 1200;

/// MIME type the card is uploaded as.
pub const CARD_MIME_TYPE: &str = "image/png";

/// Default rasteriser binary.
pub const DEFAULT_RSVG_BINARY: &str = "rsvg-convert";

/// Timeout for one `rsvg-convert` invocation.
pub const DEFAULT_RSVG_TIMEOUT: Duration = Duration::from_secs(30);

/// Characters per title line. Tuned for DejaVu Sans at 68px inside a 1008px
/// content box (the card is 1200px wide with a 96px safe margin): ~0.55em per
/// glyph keeps a 24-character line under the margin even for wide glyphs.
const TITLE_MAX_CHARS: usize = 24;
/// Maximum title lines (the template reserves two).
const TITLE_MAX_LINES: usize = 2;
/// Characters per body line (same heuristic at 54px).
const BODY_MAX_CHARS: usize = 32;
/// Maximum body lines before the text is truncated with an ellipsis.
const BODY_MAX_LINES: usize = 8;
/// Horizontal inset of the text columns, mirroring the template.
const TEXT_X: u32 = 96;
/// Baseline-to-baseline distance of the title lines, mirroring the template.
const TITLE_LINE_HEIGHT: u32 = 84;
/// Baseline-to-baseline distance of the body lines, mirroring the template.
const BODY_LINE_HEIGHT: u32 = 74;
/// Soft cap for alt text. The Lexicon declares no maximum, but the Bluesky app
/// truncates very long alt strings; we stay well inside that.
const MAX_ALT_CHARS: usize = 1000;

/// The text content of one card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSpec {
    /// Headline — the post topic.
    pub title: String,
    /// Body copy — the post text.
    pub body: String,
    /// Small monospace line at the bottom (persona handle, project name).
    pub footer: String,
}

impl CardSpec {
    /// Build a spec from a post's topic, text and footer.
    pub fn from_post(topic: &str, text: &str, footer: &str) -> Self {
        Self {
            title: topic.trim().to_string(),
            body: text.trim().to_string(),
            footer: footer.trim().to_string(),
        }
    }
}

/// Fill the versioned template with a spec's escaped, wrapped text.
///
/// Pure and deterministic: the same spec always yields the same SVG string.
pub fn render_svg(spec: &CardSpec) -> String {
    let title_lines = wrap_text(&spec.title, TITLE_MAX_CHARS, TITLE_MAX_LINES);
    let body_lines = wrap_text(&spec.body, BODY_MAX_CHARS, BODY_MAX_LINES);
    let aria = escape_xml(&alt_text(spec));

    CARD_TEMPLATE
        .replace("{{ARIA}}", &aria)
        .replace(
            "{{TITLE_LINES}}",
            &tspans(&title_lines, TEXT_X, TITLE_LINE_HEIGHT),
        )
        .replace(
            "{{BODY_LINES}}",
            &tspans(&body_lines, TEXT_X, BODY_LINE_HEIGHT),
        )
        .replace("{{FOOTER}}", &escape_xml(&spec.footer))
}

/// A text alternative for the card (Bluesky alt text is mandatory).
///
/// The card is text on a background, so the alt carries the same information a
/// sighted reader gets: the title and the body, whitespace-collapsed.
pub fn alt_text(spec: &CardSpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    let title = collapse_whitespace(&spec.title);
    let body = collapse_whitespace(&spec.body);
    if !title.is_empty() {
        parts.push(title);
    }
    if !body.is_empty() {
        parts.push(body);
    }
    let joined = parts.join(" — ");
    if joined.chars().count() > MAX_ALT_CHARS {
        let kept: String = joined
            .chars()
            .take(MAX_ALT_CHARS.saturating_sub(1))
            .collect();
        return format!("{kept}…");
    }
    joined
}

/// Wrap `text` into at most `max_lines` lines of at most `max_chars` characters.
///
/// Greedy word wrapping: whitespace is collapsed, words longer than a line are
/// hard-split, and once `max_lines` is reached the last line is truncated with an
/// ellipsis so nothing can overflow the card. Pure and deterministic.
pub fn wrap_text(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let max_lines = max_lines.max(1);

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        for piece in split_long_word(word, max_chars) {
            let current_len = current.chars().count();
            let piece_len = piece.chars().count();
            if current.is_empty() {
                current = piece;
            } else if current_len + 1 + piece_len <= max_chars {
                current.push(' ');
                current.push_str(&piece);
            } else {
                lines.push(std::mem::take(&mut current));
                current = piece;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.pop() {
            lines.push(truncate_with_ellipsis(&last, max_chars));
        }
    }
    lines
}

/// Read the pixel dimensions from a PNG's IHDR chunk.
///
/// Pure: it only inspects the header, so the bin can report the real size of the
/// rendered file without pulling in an image-decoding dependency.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Some((width, height))
}

/// Rasterise a spec to a PNG, writing the filled SVG next to it for auditing.
///
/// The SVG path is the PNG path with an `.svg` extension, so a re-render can be
/// diffed at the template level. `rsvg_binary` is injectable for tests.
pub async fn render_png(spec: &CardSpec, png_path: &Path, rsvg_binary: &str) -> Result<PathBuf> {
    if let Some(dir) = png_path.parent()
        && !dir.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating card directory {}", dir.display()))?;
    }

    let svg_path = png_path.with_extension("svg");
    tokio::fs::write(&svg_path, render_svg(spec).as_bytes())
        .await
        .with_context(|| format!("writing card SVG {}", svg_path.display()))?;

    let output = tokio::process::Command::new(rsvg_binary)
        .args([
            "-w",
            &CARD_WIDTH.to_string(),
            "-h",
            &CARD_HEIGHT.to_string(),
            "-o",
        ])
        .arg(png_path)
        .arg(&svg_path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {rsvg_binary} (is it installed?)"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{rsvg_binary} failed: {}", stderr.trim());
    }
    Ok(svg_path)
}

/// Escape the five XML predefined entities.
pub fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Collapse every run of whitespace into a single space and trim the ends.
fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build the `<tspan>` runs for one text element. `dy` is relative, so the first
/// line sits on the element's `y` and every following line is offset by
/// `line_height`.
fn tspans(lines: &[String], x: u32, line_height: u32) -> String {
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        let dy = if index == 0 { 0 } else { line_height };
        let _ = write!(
            out,
            "<tspan x=\"{x}\" dy=\"{dy}\">{}</tspan>",
            escape_xml(line)
        );
    }
    out
}

/// Split a word that does not fit one line into `max_chars` chunks.
fn split_long_word(word: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() <= max_chars {
        return vec![word.to_string()];
    }
    chars
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Return `line` trimmed so that it plus an ellipsis still fits `max_chars`.
fn truncate_with_ellipsis(line: &str, max_chars: usize) -> String {
    let mut out: String = if line.chars().count() >= max_chars {
        line.chars().take(max_chars.saturating_sub(1)).collect()
    } else {
        line.to_string()
    };
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CardSpec {
        CardSpec::from_post(
            "A rust crate that renders cards",
            "We ship a versioned SVG template and let rsvg-convert do the pixels.",
            "example.bsky.social",
        )
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = wrap_text("alpha beta gamma delta", 11, 4);
        assert_eq!(lines, vec!["alpha beta", "gamma delta"]);
    }

    #[test]
    fn hard_splits_a_word_longer_than_a_line() {
        let lines = wrap_text("abcdefghijklmnop", 5, 4);
        assert_eq!(lines, vec!["abcde", "fghij", "klmno", "p"]);
    }

    #[test]
    fn truncates_with_an_ellipsis_when_lines_run_out() {
        let lines = wrap_text("one two three four five six", 7, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].chars().count() <= 7);
        assert!(lines[1].ends_with('…'), "got {:?}", lines[1]);
    }

    #[test]
    fn ellipsis_keeps_the_line_within_the_limit() {
        let lines = wrap_text("aaaaaaaaaa bbbbbbbbbb cccccccccc", 10, 2);
        assert!(lines.iter().all(|line| line.chars().count() <= 10));
        assert!(lines[1].ends_with('…'));
    }

    #[test]
    fn empty_text_yields_no_lines() {
        assert!(wrap_text("   \n\t ", 20, 3).is_empty());
    }

    #[test]
    fn wrapping_is_deterministic() {
        let once = wrap_text("deterministic output matters here", 12, 5);
        let twice = wrap_text("deterministic output matters here", 12, 5);
        assert_eq!(once, twice);
    }

    #[test]
    fn svg_is_deterministic_and_carries_escaped_text() {
        let spec = CardSpec::from_post("a < b & c", "x > y", "me");
        let first = render_svg(&spec);
        let second = render_svg(&spec);
        assert_eq!(first, second);
        assert!(first.contains("a &lt; b &amp; c"));
        assert!(first.contains("x &gt; y"));
        assert!(!first.contains("{{TITLE_LINES}}"), "template fully filled");
        assert!(first.contains("card-v1") || first.contains("viewBox"));
    }

    #[test]
    fn svg_never_leaves_unfilled_placeholders() {
        let svg = render_svg(&spec());
        assert!(!svg.contains("{{"));
        assert!(svg.contains("<tspan"));
    }

    #[test]
    fn alt_text_joins_title_and_body() {
        let alt = alt_text(&spec());
        assert!(alt.starts_with("A rust crate that renders cards — "));
        assert!(alt.contains("rsvg-convert"));
    }

    #[test]
    fn alt_text_is_capped() {
        let long = CardSpec::from_post("t", &"word ".repeat(600), "f");
        let alt = alt_text(&long);
        assert_eq!(alt.chars().count(), MAX_ALT_CHARS);
        assert!(alt.ends_with('…'));
    }

    #[test]
    fn escapes_the_xml_entities() {
        assert_eq!(escape_xml("a&b<c>\"d'e"), "a&amp;b&lt;c&gt;&quot;d&apos;e");
    }

    #[test]
    fn parses_png_dimensions_from_a_minimal_header() {
        let mut png = Vec::from(*b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1200u32.to_be_bytes());
        png.extend_from_slice(&1200u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((1200, 1200)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }
}

//! Deterministic post-card generator (no GPU, no heavy image crate).
//!
//! A "post card" is the classic tech-account visual: the post's topic as a title,
//! the post text as body copy, rendered over the persona's palette. It is built
//! from a **versioned SVG template** (the [`CARD_TEMPLATES`] list, shipped as
//! `assets/card-vN.svg`) whose placeholders are filled with escaped, wrapped text
//! and then rasterised to PNG by `rsvg-convert`.
//!
//! A feed must not look like one template, so the crate ships **eleven** visually
//! distinct templates (layout, palette, typography and motif all differ) and picks
//! one per post from the post's own identity: a deterministic FNV-1a hash over the
//! identity fields ([`CardIdentity`], or the spec's own content by default). A
//! re-render of the same post therefore reproduces the same card, while consecutive
//! posts land on different templates.
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

/// Version of the default SVG template. Kept for the journal line; the per-post
/// template name comes from [`CardTemplate::name`].
pub const CARD_TEMPLATE_VERSION: &str = "card-v1";

/// The text geometry a template needs to place its three placeholders. Each
/// template owns its own numbers, so the layout can differ without touching the
/// renderer: it is data, not code.
#[derive(Debug, Clone, Copy)]
struct CardLayout {
    /// Inset of both text columns (the SVG file mirrors it on the `<text>` element).
    text_x: u32,
    /// Baseline-to-baseline distance of the title lines.
    title_line_height: u32,
    /// Characters per title line for this title size.
    title_max_chars: usize,
    /// Maximum title lines the template reserves.
    title_max_lines: usize,
    /// Baseline-to-baseline distance of the body lines.
    body_line_height: u32,
    /// Characters per body line for this body size.
    body_max_chars: usize,
    /// Maximum body lines before the text is truncated with an ellipsis.
    body_max_lines: usize,
}

/// One visual identity: a versioned SVG template plus the geometry to fill it.
///
/// This is the unit the rotation selects. It is deliberately data — adding a
/// template is a new `assets/card-vN.svg` plus one entry in [`CARD_TEMPLATES`].
#[derive(Debug)]
pub struct CardTemplate {
    /// Versioned template name (`card-v1` … `card-v11`), journalled with the card.
    pub name: &'static str,
    /// The template source, embedded at compile time so the binary is self-contained.
    svg: &'static str,
    /// Where and how this template places the text.
    layout: CardLayout,
}

impl CardTemplate {
    /// The raw SVG source, before placeholders are filled.
    pub fn source(&self) -> &'static str {
        self.svg
    }
}

/// All card templates, in rotation order — the single source of truth.
///
/// The rotation walks this list, so a new identity is a data edit here (never a
/// per-service or per-persona branch in code). The list is non-empty by
/// construction; the first entry is the default.
pub const CARD_TEMPLATES: &[CardTemplate] = &[
    CardTemplate {
        name: "card-v1",
        svg: include_str!("../assets/card-v1.svg"),
        layout: CardLayout {
            text_x: 96,
            title_line_height: 84,
            title_max_chars: 24,
            title_max_lines: 2,
            body_line_height: 74,
            body_max_chars: 32,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v2",
        svg: include_str!("../assets/card-v2.svg"),
        layout: CardLayout {
            text_x: 120,
            title_line_height: 88,
            title_max_chars: 21,
            title_max_lines: 2,
            body_line_height: 72,
            body_max_chars: 33,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v3",
        svg: include_str!("../assets/card-v3.svg"),
        layout: CardLayout {
            text_x: 600,
            title_line_height: 84,
            title_max_chars: 22,
            title_max_lines: 2,
            body_line_height: 70,
            body_max_chars: 34,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v4",
        svg: include_str!("../assets/card-v4.svg"),
        layout: CardLayout {
            text_x: 120,
            title_line_height: 82,
            title_max_chars: 24,
            title_max_lines: 2,
            body_line_height: 74,
            body_max_chars: 33,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v5",
        svg: include_str!("../assets/card-v5.svg"),
        layout: CardLayout {
            text_x: 1104,
            title_line_height: 84,
            title_max_chars: 23,
            title_max_lines: 2,
            body_line_height: 74,
            body_max_chars: 33,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v6",
        svg: include_str!("../assets/card-v6.svg"),
        layout: CardLayout {
            text_x: 96,
            title_line_height: 96,
            title_max_chars: 18,
            title_max_lines: 2,
            body_line_height: 68,
            body_max_chars: 36,
            body_max_lines: 4,
        },
    },
    CardTemplate {
        name: "card-v7",
        svg: include_str!("../assets/card-v7.svg"),
        layout: CardLayout {
            text_x: 600,
            title_line_height: 84,
            title_max_chars: 22,
            title_max_lines: 2,
            body_line_height: 70,
            body_max_chars: 34,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v8",
        svg: include_str!("../assets/card-v8.svg"),
        layout: CardLayout {
            text_x: 96,
            title_line_height: 80,
            title_max_chars: 22,
            title_max_lines: 2,
            body_line_height: 74,
            body_max_chars: 33,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v9",
        svg: include_str!("../assets/card-v9.svg"),
        layout: CardLayout {
            text_x: 96,
            title_line_height: 78,
            title_max_chars: 24,
            title_max_lines: 2,
            body_line_height: 70,
            body_max_chars: 32,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v10",
        svg: include_str!("../assets/card-v10.svg"),
        layout: CardLayout {
            text_x: 1104,
            title_line_height: 84,
            title_max_chars: 24,
            title_max_lines: 2,
            body_line_height: 74,
            body_max_chars: 33,
            body_max_lines: 8,
        },
    },
    CardTemplate {
        name: "card-v11",
        svg: include_str!("../assets/card-v11.svg"),
        layout: CardLayout {
            text_x: 150,
            title_line_height: 84,
            title_max_chars: 23,
            title_max_lines: 2,
            body_line_height: 74,
            body_max_chars: 33,
            body_max_lines: 8,
        },
    },
];

/// FNV-1a offset basis, mixed with a SplitMix64 finaliser for a flat low-bit
/// distribution (the raw FNV tail is biased modulo a small, non-power-of-two list).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Field separator for identity keys: the ASCII unit separator cannot appear in a
/// run id, variant label or persona handle, so `("a","bc")` and `("ab","c")`
/// cannot collide by concatenation.
const IDENTITY_SEPARATOR: char = '\u{1f}';

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

/// The post's own identity, used to rotate the card template.
///
/// `run_id`, variant label and persona are the fields that are stable for a post
/// and differ between posts, so hashing them makes the card reproducible on a
/// re-run while a feed varies. Built by the caller (the run orchestrator owns
/// those values); the card crate only needs them as a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardIdentity {
    /// Identifier of the run that produced the post.
    pub run_id: String,
    /// A/B variant label within the run.
    pub variant_label: String,
    /// Persona handle the post is published under.
    pub persona: String,
}

impl CardIdentity {
    /// Build an identity from the three stable post fields.
    pub fn new(
        run_id: impl Into<String>,
        variant_label: impl Into<String>,
        persona: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            variant_label: variant_label.into(),
            persona: persona.into(),
        }
    }

    /// Stable key mixing the fields with a separator that cannot occur in them.
    pub fn key(&self) -> String {
        format!(
            "{}{IDENTITY_SEPARATOR}{}{IDENTITY_SEPARATOR}{}",
            self.run_id, self.variant_label, self.persona
        )
    }
}

/// Deterministic template index for a stable string key.
///
/// FNV-1a over the key bytes, finished with SplitMix64 so the low bits are flat
/// (the raw FNV tail is biased modulo a small, non-power-of-two list). No
/// `RandomState`, no time, no allocation: identical on every run and platform.
pub fn template_index(key: &str) -> usize {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^= hash >> 31;

    let len = CARD_TEMPLATES.len().max(1) as u128;
    // Multiply-shift instead of `%`: maps the 64-bit hash onto `len` buckets with
    // no modulo bias.
    ((u128::from(hash) * len) >> 64) as usize
}

/// The template selected for a stable key.
///
/// [`CARD_TEMPLATES`] is a non-empty constant and `template_index` always maps
/// into range, so the fallback is only there to keep the function total.
pub fn template_for_key(key: &str) -> &'static CardTemplate {
    match CARD_TEMPLATES.get(template_index(key)) {
        Some(template) => template,
        None => &CARD_TEMPLATES[0],
    }
}

/// The template selected for a post's identity.
pub fn template_for(identity: &CardIdentity) -> &'static CardTemplate {
    template_for_key(&identity.key())
}

/// Stable key for a spec: the card's own content is its identity, so a re-render
/// of the same post reproduces the same template with no call-site change.
fn spec_key(spec: &CardSpec) -> String {
    format!(
        "{}{IDENTITY_SEPARATOR}{}{IDENTITY_SEPARATOR}{}",
        spec.title, spec.body, spec.footer
    )
}

/// The template the default rotation picks for a spec's own content.
pub fn template_for_spec(spec: &CardSpec) -> &'static CardTemplate {
    template_for_key(&spec_key(spec))
}

/// The default (first) template.
pub fn default_template() -> &'static CardTemplate {
    &CARD_TEMPLATES[0]
}

/// Fill the template selected from a spec's own content with escaped, wrapped text.
///
/// Pure and deterministic: the same spec always yields the same SVG string.
pub fn render_svg(spec: &CardSpec) -> String {
    render_svg_with(spec, template_for_spec(spec))
}

/// Fill the template selected for a post's identity.
pub fn render_svg_for(spec: &CardSpec, identity: &CardIdentity) -> String {
    render_svg_with(spec, template_for(identity))
}

/// Fill a specific template with a spec's escaped, wrapped text.
///
/// Pure and deterministic: the same spec and template always yield the same SVG
/// string. The template's [`CardLayout`] decides the column position, the line
/// heights and the wrap limits, so templates can differ in layout without a
/// branch here.
pub fn render_svg_with(spec: &CardSpec, template: &CardTemplate) -> String {
    let layout = &template.layout;
    let title_lines = wrap_text(&spec.title, layout.title_max_chars, layout.title_max_lines);
    let body_lines = wrap_text(&spec.body, layout.body_max_chars, layout.body_max_lines);
    let aria = escape_xml(&alt_text(spec));

    template
        .svg
        .replace("{{ARIA}}", &aria)
        .replace(
            "{{TITLE_LINES}}",
            &tspans(&title_lines, layout.text_x, layout.title_line_height),
        )
        .replace(
            "{{BODY_LINES}}",
            &tspans(&body_lines, layout.text_x, layout.body_line_height),
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

/// Rasterise a spec to a PNG using the template its own content selects.
pub async fn render_png(spec: &CardSpec, png_path: &Path, rsvg_binary: &str) -> Result<PathBuf> {
    render_png_with(spec, template_for_spec(spec), png_path, rsvg_binary).await
}

/// Rasterise a spec to a PNG using the template selected for its identity.
pub async fn render_png_for(
    spec: &CardSpec,
    identity: &CardIdentity,
    png_path: &Path,
    rsvg_binary: &str,
) -> Result<PathBuf> {
    render_png_with(spec, template_for(identity), png_path, rsvg_binary).await
}

/// Rasterise a spec to a PNG with a specific template, writing the filled SVG
/// next to it for auditing.
///
/// The SVG path is the PNG path with an `.svg` extension, so a re-render can be
/// diffed at the template level. `rsvg_binary` is injectable for tests.
pub async fn render_png_with(
    spec: &CardSpec,
    template: &CardTemplate,
    png_path: &Path,
    rsvg_binary: &str,
) -> Result<PathBuf> {
    if let Some(dir) = png_path.parent()
        && !dir.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating card directory {}", dir.display()))?;
    }

    let svg_path = png_path.with_extension("svg");
    tokio::fs::write(&svg_path, render_svg_with(spec, template).as_bytes())
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

    #[test]
    fn every_template_fills_all_placeholders() {
        let spec = spec();
        for template in CARD_TEMPLATES {
            let svg = render_svg_with(&spec, template);
            assert!(!svg.is_empty(), "{} rendered nothing", template.name);
            assert!(
                svg.contains("A rust crate that renders cards"),
                "{} lost the title",
                template.name
            );
            assert!(
                svg.contains("rsvg-convert do the pixels"),
                "{} lost the body",
                template.name
            );
            assert!(
                svg.contains("example.bsky.social"),
                "{} lost the footer",
                template.name
            );
            assert!(!svg.contains("{{"), "{} left a placeholder", template.name);
        }
    }

    #[test]
    fn every_template_declares_the_placeholder_contract() {
        for template in CARD_TEMPLATES {
            let source = template.source();
            for placeholder in [
                "{{ARIA}}",
                "{{TITLE_LINES}}",
                "{{BODY_LINES}}",
                "{{FOOTER}}",
            ] {
                assert!(
                    source.contains(placeholder),
                    "{} is missing {placeholder}",
                    template.name
                );
            }
        }
    }

    #[test]
    fn every_template_is_a_square_1200_card() {
        for template in CARD_TEMPLATES {
            let source = template.source();
            assert!(
                source.contains("width=\"1200\""),
                "{} is not 1200 wide",
                template.name
            );
            assert!(
                source.contains("height=\"1200\""),
                "{} is not 1200 tall",
                template.name
            );
        }
    }

    #[test]
    fn there_are_at_least_ten_uniquely_named_templates() {
        assert!(
            CARD_TEMPLATES.len() >= 10,
            "a feed needs ten identities, got {}",
            CARD_TEMPLATES.len()
        );
        let mut names: Vec<&str> = CARD_TEMPLATES
            .iter()
            .map(|template| template.name)
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CARD_TEMPLATES.len(), "duplicate template name");
    }

    #[test]
    fn templates_render_distinct_outputs() {
        let spec = spec();
        let mut outputs: Vec<String> = CARD_TEMPLATES
            .iter()
            .map(|template| render_svg_with(&spec, template))
            .collect();
        let total = outputs.len();
        outputs.sort_unstable();
        outputs.dedup();
        assert_eq!(outputs.len(), total, "two templates render identically");
        assert!(outputs.len() >= 10);
    }

    #[test]
    fn rotation_is_deterministic() {
        let identity = CardIdentity::new("run-42", "A", "example.bsky.social");
        let first = template_for(&identity).name;
        assert_eq!(first, template_for(&identity).name);
        assert_eq!(first, template_for_key(&identity.key()).name);
    }

    #[test]
    fn identity_key_separates_fields() {
        let left = CardIdentity::new("a", "bc", "p");
        let right = CardIdentity::new("ab", "c", "p");
        assert_ne!(left.key(), right.key(), "fields must not concatenate");
    }

    #[test]
    fn rotation_covers_every_template() {
        let mut seen: Vec<&str> = Vec::new();
        for index in 0..400 {
            for label in ["A", "B", "C"] {
                let identity =
                    CardIdentity::new(format!("run-{index:04}"), label, "example.bsky.social");
                let name = template_for(&identity).name;
                if !seen.contains(&name) {
                    seen.push(name);
                }
            }
        }
        assert_eq!(
            seen.len(),
            CARD_TEMPLATES.len(),
            "rotation never reached {:?}",
            CARD_TEMPLATES
                .iter()
                .map(|template| template.name)
                .filter(|name| !seen.contains(name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn consecutive_posts_use_more_than_one_template() {
        let mut names: Vec<&str> = Vec::new();
        for index in 0..24 {
            let identity = CardIdentity::new(format!("run-{index:04}"), "A", "persona");
            names.push(template_for(&identity).name);
        }
        let mut distinct = names.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(distinct.len() >= 4, "rotation collapsed to {distinct:?}");
        let identical_neighbours = names.windows(2).filter(|pair| pair[0] == pair[1]).count();
        assert!(
            identical_neighbours < names.len() - 1,
            "consecutive posts repeated one template"
        );
    }

    #[test]
    fn render_svg_is_deterministic_and_follows_the_content_key() {
        let spec = spec();
        assert_eq!(render_svg(&spec), render_svg(&spec));
        assert_eq!(
            render_svg(&spec),
            render_svg_with(&spec, template_for_spec(&spec))
        );
    }

    #[test]
    fn same_identity_reproduces_the_same_card() {
        let spec = spec();
        let identity = CardIdentity::new("run-7", "B", "example.bsky.social");
        assert_eq!(
            render_svg_for(&spec, &identity),
            render_svg_for(&spec, &identity)
        );
    }
}

//! Deterministic, **per-persona** post-card generator (no GPU, no image crate).
//!
//! A "post card" is the classic tech-account visual: the post's topic as a title,
//! the post text as body copy, painted with **the persona's own** template and
//! palette. The look is data, never a constant: the template and the palette are
//! resolved from the persona pack (`visual.template` + `visual.palette`, see
//! [`CardTheme::from_visual`]) and the template lives under
//! `personas/<slug>/visual/`. Two personas therefore never render the same card —
//! a single global template was an architecture bug (HB-032).
//!
//! The template is a **versioned SVG file** whose placeholders are filled with
//! escaped, wrapped text and the palette colours; it is then rasterised to PNG by
//! `rsvg-convert`. Why SVG + `rsvg-convert` instead of a Rust image crate?
//!
//! * the visual is data (a reviewable, diffable, versioned file), not code;
//! * `rsvg-convert` is already installed on the host and does the rasterisation
//!   (cairo/pango/harfbuzz), so we add **no** dependency for font shaping or PNG
//!   encoding — `resvg`/`image`/`ab_glyph` would each be a large tree for a job
//!   the system tool already does;
//! * the same filled SVG is written next to the PNG, so a re-render is diffable
//!   even when the PNG bytes are not byte-identical across tool versions.
//!
//! Everything that decides the output (wrapping, truncation, escaping, placeholder
//! filling, palette derivation, PNG header parsing) is a pure function and
//! unit-tested; only [`render_png`] spawns a process.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use autopilot_core::{Palette, Typography, Visual, resolve_visual_template};

/// Version of the built-in fallback template. Bump it when `default-card.svg`
/// changes; a persona template is versioned by its own filename.
pub const CARD_TEMPLATE_VERSION: &str = "card-v1";

/// Name reported for the built-in fallback template.
pub const DEFAULT_TEMPLATE_NAME: &str = "default-card";

/// The built-in fallback template, embedded at compile time so the binary is
/// self-contained. It is used **only** when a persona declares no template.
const DEFAULT_CARD_TEMPLATE: &str = include_str!("../assets/default-card.svg");

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

/// Percentage of each channel kept when deriving a deeper background tone.
const DARKEN_PERCENT: u16 = 70;

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

/// The resolved look of one card: template source plus the persona's palette,
/// typography and motif.
///
/// A theme is always resolved **from a persona** ([`CardTheme::from_visual`]);
/// the only exception is [`CardTheme::default_fallback`], used when a pack
/// declares no `visual` block. It is data — never a global constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardTheme {
    /// Template name as declared by the pack (or the fallback name).
    pub name: String,
    /// SVG source with `{{...}}` placeholders.
    pub template: String,
    pub palette: Palette,
    pub typography: Typography,
    /// Structural motif name, rendered by the template.
    pub motif: String,
    /// Footer the persona declares for its cards (empty when it declares none).
    /// The caller may prefer it over a per-post footer.
    pub footer: String,
}

impl CardTheme {
    /// The built-in fallback theme, used only when a persona declares no
    /// `visual` block.
    pub fn default_fallback() -> Self {
        Self {
            name: DEFAULT_TEMPLATE_NAME.to_string(),
            template: DEFAULT_CARD_TEMPLATE.to_string(),
            palette: Palette::default(),
            typography: Typography::default(),
            motif: "terminal".to_string(),
            footer: String::new(),
        }
    }

    /// Resolve a theme from a persona's `visual` block.
    ///
    /// `visual_dir` is the persona's `personas/<slug>/visual/` directory. A
    /// declared template that cannot be read is a hard error — never a silent
    /// fallback, so two personas can never accidentally share a look. With no
    /// template declared, the persona's palette and typography still repaint the
    /// built-in structure.
    pub fn from_visual(visual: Option<&Visual>, visual_dir: &Path) -> Result<Self> {
        let Some(visual) = visual else {
            return Ok(Self::default_fallback());
        };

        let (name, template) = match visual.template.as_deref() {
            Some(declared) if !declared.trim().is_empty() => {
                let path = resolve_visual_template(visual_dir, declared);
                let source = std::fs::read_to_string(&path).with_context(|| {
                    format!(
                        "reading card template `{declared}` for the persona visual \
                         (expected at {})",
                        path.display()
                    )
                })?;
                (declared.trim().to_string(), source)
            }
            _ => (
                DEFAULT_TEMPLATE_NAME.to_string(),
                DEFAULT_CARD_TEMPLATE.to_string(),
            ),
        };

        Ok(Self {
            name,
            template,
            palette: visual.palette.clone(),
            typography: visual.typography.clone(),
            motif: visual.motif.clone(),
            footer: visual.footer.clone(),
        })
    }
}

/// Fill a resolved theme's template with a spec's escaped, wrapped text and the
/// theme's palette/typography.
///
/// Pure and deterministic: the same theme and spec always yield the same SVG.
pub fn render_svg(theme: &CardTheme, spec: &CardSpec) -> String {
    let title_lines = wrap_text(&spec.title, TITLE_MAX_CHARS, TITLE_MAX_LINES);
    let body_lines = wrap_text(&spec.body, BODY_MAX_CHARS, BODY_MAX_LINES);
    let case = theme.typography.case;

    let replacements = [
        ("{{ARIA}}", escape_xml(&alt_text(spec))),
        (
            "{{TITLE_LINES}}",
            tspans(&title_lines, TEXT_X, TITLE_LINE_HEIGHT),
        ),
        (
            "{{BODY_LINES}}",
            tspans(&body_lines, TEXT_X, BODY_LINE_HEIGHT),
        ),
        ("{{FOOTER}}", escape_xml(&case.apply(&spec.footer))),
        ("{{BG}}", theme.palette.background.clone()),
        ("{{BG_DEEP}}", darken(&theme.palette.background)),
        ("{{ACCENT}}", theme.palette.accent.clone()),
        ("{{SECONDARY}}", theme.palette.secondary.clone()),
        ("{{TEXT}}", theme.palette.text.clone()),
        ("{{FONT_FAMILY}}", escape_xml(&theme.typography.family)),
        ("{{TITLE_WEIGHT}}", theme.typography.weight.to_string()),
        ("{{MOTIF}}", escape_xml(&case.apply(&theme.motif))),
    ];

    let mut svg = theme.template.clone();
    for (key, value) in replacements {
        svg = svg.replace(key, &value);
    }
    svg
}

/// Darken a `#rgb`/`#rrggbb` colour to a deeper tone (used for gradients).
///
/// Pure; a value that is not a valid hex colour is returned unchanged.
pub fn darken(hex: &str) -> String {
    let trimmed = hex.trim();
    let Some(digits) = trimmed.strip_prefix('#') else {
        return hex.to_string();
    };
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return hex.to_string();
    }
    let expanded: Vec<char> = match digits.len() {
        3 => digits.chars().flat_map(|c| [c, c]).collect(),
        6 => digits.chars().collect(),
        _ => return hex.to_string(),
    };

    let mut channels = [0u8; 3];
    for (index, channel) in channels.iter_mut().enumerate() {
        let pair: String = expanded[index * 2..index * 2 + 2].iter().collect();
        let Ok(byte) = u8::from_str_radix(&pair, 16) else {
            return hex.to_string();
        };
        *channel = (u16::from(byte) * DARKEN_PERCENT / 100) as u8;
    }
    format!("#{:02x}{:02x}{:02x}", channels[0], channels[1], channels[2])
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

/// Rasterise a themed spec to a PNG, writing the filled SVG next to it for auditing.
///
/// The SVG path is the PNG path with an `.svg` extension, so a re-render can be
/// diffed at the template level. `rsvg_binary` is injectable for tests.
pub async fn render_png(
    theme: &CardTheme,
    spec: &CardSpec,
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
    tokio::fs::write(&svg_path, render_svg(theme, spec).as_bytes())
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
    use autopilot_core::TextCase;

    fn spec() -> CardSpec {
        CardSpec::from_post(
            "A rust crate that renders cards",
            "We ship a versioned SVG template and let rsvg-convert do the pixels.",
            "example.bsky.social",
        )
    }

    /// A minimal template used to prove structural differences.
    const MINIMAL_TEMPLATE: &str =
        "<svg aria-label=\"{{ARIA}}\"><text fill=\"{{ACCENT}}\">{{TITLE_LINES}}</text></svg>";

    fn theme() -> CardTheme {
        CardTheme::default_fallback()
    }

    fn themed_visual() -> Visual {
        Visual {
            template: None,
            palette: Palette {
                background: "#05060a".into(),
                accent: "#7c3aed".into(),
                secondary: "#22d3ee".into(),
                text: "#e2e8f0".into(),
            },
            typography: Typography {
                family: "Inter".into(),
                weight: 800,
                case: TextCase::Upper,
            },
            motif: "orbit".into(),
            footer: "space.example".into(),
        }
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
        let first = render_svg(&theme(), &spec);
        let second = render_svg(&theme(), &spec);
        assert_eq!(first, second);
        assert!(first.contains("a &lt; b &amp; c"));
        assert!(first.contains("x &gt; y"));
        assert!(!first.contains("{{TITLE_LINES}}"), "template fully filled");
        assert!(first.contains("viewBox"));
    }

    #[test]
    fn svg_never_leaves_unfilled_placeholders() {
        let svg = render_svg(&theme(), &spec());
        assert!(!svg.contains("{{"), "no placeholder may survive: {svg}");
        assert!(svg.contains("<tspan"));
    }

    #[test]
    fn the_palette_is_substituted_into_the_template() {
        let theme = CardTheme {
            palette: themed_visual().palette,
            ..CardTheme::default_fallback()
        };
        let svg = render_svg(&theme, &spec());
        assert!(svg.contains("#7c3aed"), "accent must reach the SVG");
        assert!(svg.contains("#22d3ee"), "secondary must reach the SVG");
        assert!(svg.contains("#05060a"), "background must reach the SVG");
        assert!(!svg.contains("{{ACCENT}}"));
    }

    #[test]
    fn two_palettes_produce_different_svg() {
        let spec = spec();
        let one = CardTheme::default_fallback();
        let mut other = one.clone();
        other.palette.accent = "#ff0000".into();
        assert_ne!(render_svg(&one, &spec), render_svg(&other, &spec));
    }

    #[test]
    fn two_templates_produce_structurally_different_svg() {
        let spec = spec();
        let default = render_svg(&theme(), &spec);
        let minimal = CardTheme {
            template: MINIMAL_TEMPLATE.to_string(),
            ..CardTheme::default_fallback()
        };
        let minimal_svg = render_svg(&minimal, &spec);
        assert_ne!(default, minimal_svg);
        assert!(!minimal_svg.contains("<rect"));
    }

    #[test]
    fn typography_case_is_applied_to_labels() {
        let theme = CardTheme {
            typography: themed_visual().typography,
            motif: "orbit".into(),
            ..CardTheme::default_fallback()
        };
        let svg = render_svg(&theme, &spec());
        assert!(svg.contains(">ORBIT<"), "motif must be uppercased: {svg}");
        assert!(svg.contains("font-family=\"'Inter'"));
        assert!(svg.contains("font-weight=\"800\""));
    }

    #[test]
    fn darken_derives_a_deeper_tone() {
        assert_eq!(darken("#ffffff"), "#b2b2b2");
        assert_eq!(darken("#fff"), "#b2b2b2");
        assert_eq!(darken("#0b1220"), "#070c16");
        // Invalid input is returned unchanged (never a panic).
        assert_eq!(darken("amber"), "amber");
        assert_eq!(darken("#12345"), "#12345");
        assert_eq!(darken("#zzzzzz"), "#zzzzzz");
    }

    #[test]
    fn from_visual_without_a_block_returns_the_fallback() {
        let dir = Path::new("/nonexistent");
        let theme = CardTheme::from_visual(None, dir).expect("no visual is not an error");
        assert_eq!(theme, CardTheme::default_fallback());
    }

    #[test]
    fn from_visual_keeps_the_declared_palette_with_the_default_structure() {
        let visual = themed_visual();
        let theme = CardTheme::from_visual(Some(&visual), Path::new("/nonexistent"))
            .expect("no template declared is not an error");
        assert_eq!(theme.name, DEFAULT_TEMPLATE_NAME);
        assert_eq!(theme.palette.accent, "#7c3aed");
        assert_eq!(theme.motif, "orbit");
        assert_eq!(theme.footer, "space.example");
    }

    #[test]
    fn from_visual_reads_the_template_file_and_fails_when_missing() {
        let base = std::env::temp_dir().join(format!("autopilot-card-{}", std::process::id()));
        let visual_dir = base.join("demo").join("visual");
        std::fs::create_dir_all(&visual_dir).expect("temp visual dir");

        let mut visual = themed_visual();
        visual.template = Some("card-orbit-v1".into());

        // Missing file: a clear error, never a silent fallback.
        let missing = CardTheme::from_visual(Some(&visual), &visual_dir);
        assert!(missing.is_err());

        std::fs::write(visual_dir.join("card-orbit-v1.svg"), MINIMAL_TEMPLATE)
            .expect("write temp template");
        let theme = CardTheme::from_visual(Some(&visual), &visual_dir).expect("template loads");
        assert_eq!(theme.name, "card-orbit-v1");
        assert_eq!(theme.template, MINIMAL_TEMPLATE);

        std::fs::remove_dir_all(&base).expect("cleanup temp visual dir");
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

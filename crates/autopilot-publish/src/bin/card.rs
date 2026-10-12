//! `card` — offline, **per-persona** post-card generator for Bluesky.
//!
//! Resolves the card theme from a persona pack (`--persona`, which carries the
//! `visual` block: template + palette + typography + motif), fills that
//! template from a post's topic/text and rasterises it with `rsvg-convert`. No
//! GPU, no network, no image-generation model: the card is typography over the
//! persona's own palette. `--dry-run` prints the filled SVG and the alt text and
//! writes nothing.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use autopilot_core::{load_persona, visual_dir};
use autopilot_publish::card::{
    CARD_HEIGHT, CARD_MIME_TYPE, CARD_WIDTH, CardSpec, CardTheme, DEFAULT_RSVG_BINARY, alt_text,
    png_dimensions, render_png, render_svg,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "card",
    version,
    about = "Deterministic per-persona post-card generator (SVG template + rsvg-convert)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render a post card to PNG (or print the SVG with --dry-run).
    Render {
        /// Headline — the post topic.
        #[arg(long)]
        topic: String,
        /// Body copy — the post text.
        #[arg(long)]
        text: String,
        /// Footer line (persona handle, project name).
        #[arg(long, default_value = "")]
        footer: String,
        /// Output PNG path.
        #[arg(long)]
        out: PathBuf,
        /// Rasteriser binary.
        #[arg(long, default_value = DEFAULT_RSVG_BINARY)]
        rsvg: String,
        /// Persona pack to resolve the visual identity from (template + palette).
        /// Without it the built-in fallback card is used.
        #[arg(long)]
        persona: Option<PathBuf>,
        /// Explicit SVG template path, overriding the one the pack declares.
        #[arg(long)]
        template: Option<PathBuf>,
        /// Print the filled SVG and the alt text without rasterising.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let Command::Render {
        topic,
        text,
        footer,
        out,
        rsvg,
        persona,
        template,
        dry_run,
    } = cli.command;

    let theme = resolve_theme(persona.as_deref(), template.as_deref()).await?;
    // The persona's declared footer is the default; `--footer` overrides it.
    let footer = if footer.trim().is_empty() {
        theme.footer.clone()
    } else {
        footer
    };
    let spec = CardSpec::from_post(&topic, &text, &footer);

    println!("template : {}", theme.name);
    println!("mime     : {CARD_MIME_TYPE}");
    println!("size     : {CARD_WIDTH}x{CARD_HEIGHT}");
    println!("alt      : {}", alt_text(&spec));

    if dry_run {
        println!("--- filled SVG (dry run, nothing written) ---");
        println!("{}", render_svg(&theme, &spec));
        return Ok(());
    }

    let svg = render_png(&theme, &spec, &out, &rsvg).await?;
    let bytes = tokio::fs::read(&out)
        .await
        .with_context(|| format!("reading the rendered card {}", out.display()))?;
    let (width, height) = png_dimensions(&bytes).context("the renderer did not produce a PNG")?;
    println!(
        "png      : {} ({} bytes, {width}x{height})",
        out.display(),
        bytes.len()
    );
    println!("svg      : {}", svg.display());
    Ok(())
}

/// Resolve the card theme from a persona pack, an explicit template, or the
/// built-in fallback.
///
/// The persona pack is the source of the visual identity: its `visual.template`
/// resolves under `personas/<slug>/visual/` and its palette/typography repaint
/// the card. `--template` overrides only the structure.
async fn resolve_theme(persona: Option<&Path>, template: Option<&Path>) -> Result<CardTheme> {
    let mut theme = match persona {
        Some(path) => {
            let pack = load_persona(path).await?;
            let root = path.parent().unwrap_or_else(|| Path::new("."));
            let dir = visual_dir(root, &pack.persona);
            CardTheme::from_visual(pack.visual.as_ref(), &dir)?
        }
        None => CardTheme::default_fallback(),
    };

    if let Some(template) = template {
        let source = tokio::fs::read_to_string(template)
            .await
            .with_context(|| format!("reading card template {}", template.display()))?;
        theme.name = template.display().to_string();
        theme.template = source;
    }

    Ok(theme)
}

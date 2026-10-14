//! `card` — offline post-card generator for Bluesky.
//!
//! Fills the versioned SVG template the post's own content selects (one of the
//! [`autopilot_publish::card::CARD_TEMPLATES`] identities) from a post's
//! topic/text and rasterises it with `rsvg-convert`. No GPU, no network, no
//! image-generation model: the card is typography over the persona palette.
//! `--dry-run` prints the filled SVG and the alt text and writes nothing.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use autopilot_core::load_persona;
use autopilot_publish::card::{
    CARD_HEIGHT, CARD_MIME_TYPE, CARD_WIDTH, CardSpec, DEFAULT_RSVG_BINARY, alt_text,
    png_dimensions, render_png_in_pool, render_png_with, render_svg_in_pool, render_svg_with,
    resolve_pool, template_for_spec, template_for_spec_in_pool,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "card",
    version,
    about = "Deterministic post-card generator (SVG template + rsvg-convert)"
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
        /// Persona pack whose `visual` pool scopes the card identity (HB-032).
        /// Without it the card uses the whole template catalogue (the global
        /// rotation) — the behaviour a pack with no `visual` block always had.
        #[arg(long)]
        persona: Option<PathBuf>,
        /// Rasteriser binary.
        #[arg(long, default_value = DEFAULT_RSVG_BINARY)]
        rsvg: String,
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
        persona,
        rsvg,
        dry_run,
    } = cli.command;

    // HB-032: a persona pack narrows the catalogue to its own pool; without one
    // the card keeps the global rotation (spec-content key over every template).
    let persona_pool = match persona.as_deref() {
        Some(path) => Some(resolve_pool(&load_persona(path).await?.visual_templates())?),
        None => None,
    };

    let spec = CardSpec::from_post(&topic, &text, &footer);
    let template = match &persona_pool {
        Some(pool) => {
            if let Some(path) = &persona {
                println!("persona : {}", path.display());
            }
            println!(
                "pool    : {}",
                pool.iter()
                    .map(|template| template.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            template_for_spec_in_pool(&spec, pool)
        }
        None => template_for_spec(&spec),
    };
    println!("template : {}", template.name);
    println!("mime     : {CARD_MIME_TYPE}");
    println!("size     : {CARD_WIDTH}x{CARD_HEIGHT}");
    println!("alt      : {}", alt_text(&spec));

    if dry_run {
        println!("--- filled SVG (dry run, nothing written) ---");
        match &persona_pool {
            Some(pool) => println!("{}", render_svg_in_pool(&spec, pool)),
            None => println!("{}", render_svg_with(&spec, template)),
        }
        return Ok(());
    }

    let svg = match &persona_pool {
        Some(pool) => render_png_in_pool(&spec, pool, &out, &rsvg).await?,
        None => render_png_with(&spec, template, &out, &rsvg).await?,
    };
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

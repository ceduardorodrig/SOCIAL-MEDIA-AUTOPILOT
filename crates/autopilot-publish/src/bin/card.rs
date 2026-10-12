//! `card` — offline post-card generator for Bluesky.
//!
//! Fills the versioned SVG template from a post's topic/text and rasterises it
//! with `rsvg-convert`. No GPU, no network, no image-generation model: the card
//! is typography over the persona palette. `--dry-run` prints the filled SVG and
//! the alt text and writes nothing.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use autopilot_publish::card::{
    CARD_HEIGHT, CARD_MIME_TYPE, CARD_TEMPLATE_VERSION, CARD_WIDTH, CardSpec, DEFAULT_RSVG_BINARY,
    alt_text, png_dimensions, render_png, render_svg,
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
        rsvg,
        dry_run,
    } = cli.command;

    let spec = CardSpec::from_post(&topic, &text, &footer);
    println!("template : {CARD_TEMPLATE_VERSION}");
    println!("mime     : {CARD_MIME_TYPE}");
    println!("size     : {CARD_WIDTH}x{CARD_HEIGHT}");
    println!("alt      : {}", alt_text(&spec));

    if dry_run {
        println!("--- filled SVG (dry run, nothing written) ---");
        println!("{}", render_svg(&spec));
        return Ok(());
    }

    let svg = render_png(&spec, &out, &rsvg).await?;
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

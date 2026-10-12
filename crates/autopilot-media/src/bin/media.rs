//! `media` — offline-verifiable demo binary for the media module.
//!
//! It exposes `plan` (parse a plan and print its ordered beats) and `render
//! --dry-run` (print the exact Piper/ffmpeg commands without executing
//! anything). The real wiring into `autopilot` belongs to `autopilot-cli`; this
//! binary exists so the module is demonstrable without touching that crate.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use autopilot_media::{
    DEFAULT_PIPER_BINARY, ImageSource, Motion, PiperConfig, PlaceholderImageSource, RenderOptions,
    SceneImageDir, VideoPlan, probe_piper, render,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "media",
    version,
    about = "Short-video media module (offline demo)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a video plan and print its ordered beats.
    Plan {
        #[arg(long)]
        plan: PathBuf,
    },
    /// Render a plan to a vertical MP4, or print the commands with --dry-run.
    Render {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "var/media")]
        work_dir: PathBuf,
        /// Piper voice model (`.onnx`); required for a real render.
        #[arg(long)]
        piper_model: Option<PathBuf>,
        /// Directory holding one PNG per scene, named `<scene_index>.png`.
        #[arg(long)]
        images: Option<PathBuf>,
        #[arg(long, value_parser = parse_motion, default_value = "static")]
        motion: Motion,
        /// Print the commands without executing Piper or ffmpeg.
        #[arg(long)]
        dry_run: bool,
    },
}

fn parse_motion(value: &str) -> Result<Motion, String> {
    value.parse()
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Plan { plan } => run_plan(&plan).await,
        Command::Render {
            plan,
            work_dir,
            piper_model,
            images,
            motion,
            dry_run,
        } => run_render(&plan, &work_dir, piper_model, images, motion, dry_run).await,
    }
}

async fn run_plan(path: &Path) -> Result<ExitCode> {
    let plan = read_plan(path).await?;
    println!("hook  : {}", display_or(&plan.hook, "(none)"));
    println!("scenes: {}", plan.scenes.len());
    println!("cta   : {}", display_or(&plan.call_to_action, "(none)"));
    for (index, beat) in plan.beats().iter().enumerate() {
        println!(
            "{index:>2}. [{:<5}] scene={} image={} :: {}",
            beat.role.as_str(),
            beat.scene_index,
            display_or(&beat.image_prompt, "-"),
            beat.narration
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_render(
    plan_path: &Path,
    work_dir: &Path,
    piper_model: Option<PathBuf>,
    images: Option<PathBuf>,
    motion: Motion,
    dry_run: bool,
) -> Result<ExitCode> {
    let plan = read_plan(plan_path).await?;

    if !dry_run && piper_model.is_none() {
        bail!("a real render needs --piper-model <voice.onnx> (use --dry-run to preview)");
    }
    if !dry_run {
        let probe = probe_piper(DEFAULT_PIPER_BINARY).await;
        if !probe.available {
            eprintln!("piper is not on PATH — install it or use --dry-run");
            return Ok(ExitCode::FAILURE);
        }
        if let Some(version) = &probe.version {
            println!("piper : {version}");
        }
    }

    let source: Box<dyn ImageSource> = match images {
        Some(dir) => Box::new(SceneImageDir::new(dir)),
        None => Box::new(PlaceholderImageSource),
    };
    let model = piper_model.unwrap_or_else(|| PathBuf::from("<piper-model>"));
    let options = RenderOptions::new(
        work_dir.to_path_buf(),
        PiperConfig::new(model),
        motion,
        dry_run,
    );

    let output = render(&plan, source.as_ref(), &options).await?;

    let suffix = if dry_run {
        " (dry run, nothing executed)"
    } else {
        ""
    };
    println!("{} command(s){suffix}", output.commands.len());
    for rendered in &output.commands {
        println!("--- {} ---", rendered.label);
        println!("{}", rendered.command.display_line());
    }
    println!(
        "{}: {}",
        if dry_run { "would write" } else { "video" },
        output.video.display()
    );
    Ok(ExitCode::SUCCESS)
}

async fn read_plan(path: &Path) -> Result<VideoPlan> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    VideoPlan::parse(&raw)
}

fn display_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

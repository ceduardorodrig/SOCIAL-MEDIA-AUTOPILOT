//! `autopilot` CLI — Phase 0 tooling for the Social Media Autopilot harness.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use autopilot_core::{ValidationIssue, load_dir};
use autopilot_providers::{ProbeOutcome, load_config, probe, resolve_chain};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "autopilot", version, about = "Social Media Autopilot harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// LLM provider registry operations.
    Providers {
        #[command(subcommand)]
        command: ProvidersCommand,
    },
    /// Persona pack operations.
    Personas {
        #[command(subcommand)]
        command: PersonasCommand,
    },
}

#[derive(Subcommand)]
enum ProvidersCommand {
    /// Probe every provider and print availability.
    Check {
        #[arg(long, default_value = "config/providers.yaml")]
        config: PathBuf,
    },
    /// Print the resolved fallback chain (available providers by priority).
    Chain {
        #[arg(long, default_value = "config/providers.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum PersonasCommand {
    /// List the persona packs found in a directory.
    List {
        #[arg(long, default_value = "personas")]
        dir: PathBuf,
    },
    /// Validate persona packs against engine invariants.
    Validate {
        #[arg(long, default_value = "personas")]
        dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Providers { command } => run_providers(command).await,
        Command::Personas { command } => run_personas(command).await,
    }
}

async fn probe_all(config: &Path) -> Result<Vec<ProbeOutcome>> {
    let registry = load_config(config).await?;
    let mut set = tokio::task::JoinSet::new();
    for provider in registry.providers {
        set.spawn(async move { probe(&provider).await });
    }

    let mut outcomes = Vec::new();
    while let Some(result) = set.join_next().await {
        outcomes.push(result?);
    }
    outcomes.sort_by_key(|o| o.priority);
    Ok(outcomes)
}

async fn run_providers(command: ProvidersCommand) -> Result<ExitCode> {
    match command {
        ProvidersCommand::Check { config } => {
            let outcomes = probe_all(&config).await?;
            println!(
                "{:<10} {:<8} {:<34} {:<4} {:>8}  DETAIL",
                "ID", "KIND", "MODEL", "UP", "LAT(ms)"
            );
            for o in &outcomes {
                println!(
                    "{:<10} {:<8} {:<34} {:<4} {:>8}  {}",
                    o.id,
                    o.kind.as_str(),
                    o.model,
                    if o.available { "yes" } else { "no" },
                    o.latency_ms,
                    o.detail
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        ProvidersCommand::Chain { config } => {
            let outcomes = probe_all(&config).await?;
            let chain = resolve_chain(&outcomes);
            if chain.is_empty() {
                eprintln!("no provider is available — the pipeline must pause this window");
                return Ok(ExitCode::FAILURE);
            }
            for (index, o) in chain.iter().enumerate() {
                println!("{}. {:<10} {:<34} {}", index + 1, o.id, o.model, o.detail);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_personas(command: PersonasCommand) -> Result<ExitCode> {
    match command {
        PersonasCommand::List { dir } => {
            let packs = load_dir(&dir).await?;
            println!("{:<30} {:<30} {:>8}", "PERSONA", "NICHE", "CHANNELS");
            for p in &packs {
                println!("{:<30} {:<30} {:>8}", p.persona, p.niche, p.channels.len());
            }
            Ok(ExitCode::SUCCESS)
        }
        PersonasCommand::Validate { dir } => {
            let packs = load_dir(&dir).await?;
            let mut issues: Vec<(String, ValidationIssue)> = Vec::new();
            for p in &packs {
                for issue in p.validate() {
                    issues.push((p.persona.clone(), issue));
                }
            }
            if issues.is_empty() {
                println!("all {} persona pack(s) valid", packs.len());
                Ok(ExitCode::SUCCESS)
            } else {
                for (persona, issue) in &issues {
                    println!("fail {} :: {} — {}", persona, issue.field, issue.problem);
                }
                eprintln!("{} validation issue(s) found", issues.len());
                Ok(ExitCode::FAILURE)
            }
        }
    }
}

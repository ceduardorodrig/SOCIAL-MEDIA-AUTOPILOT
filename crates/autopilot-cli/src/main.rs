//! `autopilot` CLI — Phase 0 tooling for the Social Media Autopilot harness.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use autopilot_core::{ValidationIssue, load_dir};
use autopilot_harvest::{DEFAULT_SEARXNG_ENDPOINT, SearxngClient};
use autopilot_providers::{
    ProbeOutcome, append_ledger, check_budget, ledger_entry, load_config, opencode_usage, probe,
    resolve_chain, usage_for,
};
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
    /// LLM quota instrumentation.
    Quota {
        #[command(subcommand)]
        command: QuotaCommand,
    },
    /// Trend harvesting.
    Harvest {
        #[command(subcommand)]
        command: HarvestCommand,
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

#[derive(Subcommand)]
enum QuotaCommand {
    /// Show OpenCode usage for a model and check it against a token budget.
    Check {
        #[arg(long, default_value = "opencode/mimo-v2.6-flash-free")]
        model: String,
        /// Maximum tokens allowed in the window (omit to only report).
        #[arg(long)]
        budget_tokens: Option<u64>,
        /// Window size in days (`0` = today).
        #[arg(long, default_value_t = 0)]
        days: u32,
        /// Append the snapshot to a JSONL ledger.
        #[arg(long)]
        ledger: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HarvestCommand {
    /// Search trend candidates via the SearXNG JSON API.
    Search {
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value = DEFAULT_SEARXNG_ENDPOINT)]
        endpoint: String,
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
        Command::Quota { command } => run_quota(command).await,
        Command::Harvest { command } => run_harvest(command).await,
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

/// How long to wait for the `opencode stats` subprocess.
const STATS_TIMEOUT: Duration = Duration::from_secs(120);

async fn run_quota(command: QuotaCommand) -> Result<ExitCode> {
    match command {
        QuotaCommand::Check {
            model,
            budget_tokens,
            days,
            ledger,
        } => {
            let usages = opencode_usage(days, STATS_TIMEOUT).await?;
            let Some(usage) = usage_for(&usages, &model) else {
                let seen = usages.iter().map(|u| u.id()).collect::<Vec<_>>().join(", ");
                println!("no usage recorded for {model} in the last {days} day(s)");
                println!("models seen: {seen}");
                return Ok(ExitCode::SUCCESS);
            };

            let check = check_budget(usage, budget_tokens);
            println!("model        : {}", check.model_id);
            println!("window       : last {days} day(s)");
            println!("steps        : {}", usage.steps);
            println!("input tokens : {}", usage.input_tokens);
            println!("output tokens: {}", usage.output_tokens);
            println!("reasoning    : {}", usage.reasoning_tokens);
            println!("cache read   : {}", usage.cache_read_tokens);
            println!("total tokens : {}", check.used_tokens);
            println!("cost (usd)   : {:.4}", usage.cost_usd);
            match check.budget_tokens {
                Some(0) => println!("budget       : 0 (configured)"),
                Some(budget) => println!(
                    "budget       : {budget} ({:.1}% used)",
                    check.used_tokens as f64 * 100.0 / budget as f64
                ),
                None => println!("budget       : not set"),
            }

            if let Some(path) = ledger {
                append_ledger(&path, &ledger_entry(usage, days)).await?;
                println!("ledger       : appended to {}", path.display());
            }

            if check.within_budget {
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("token budget exceeded for {}", check.model_id);
                Ok(ExitCode::FAILURE)
            }
        }
    }
}

async fn run_harvest(command: HarvestCommand) -> Result<ExitCode> {
    match command {
        HarvestCommand::Search {
            query,
            limit,
            endpoint,
        } => {
            let client = SearxngClient::new(endpoint);
            let topics = client.search(&query, limit).await?;
            println!(
                "{} topic(s) for \"{query}\" via {}",
                topics.len(),
                client.endpoint()
            );
            for (index, topic) in topics.iter().enumerate() {
                println!(
                    "{:>2}. [{}] {}\n    {}",
                    index + 1,
                    topic.engine,
                    topic.title,
                    topic.url
                );
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

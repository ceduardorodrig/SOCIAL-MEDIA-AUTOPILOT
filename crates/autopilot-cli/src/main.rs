//! `autopilot` CLI — Phase 0 tooling for the Social Media Autopilot harness.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use autopilot_core::{
    Platform, ValidationIssue, load_dir, load_persona, now_unix, resolve_persona_path,
};
use autopilot_engage::{EngageOptions, EngageReport};
use autopilot_guardrail::{Verdict, evaluate};
use autopilot_harvest::{DEFAULT_SEARXNG_ENDPOINT, SearxngClient};
use autopilot_learn::{
    CraftConfig, DEFAULT_MAX_FEW_SHOT, DEFAULT_MIN_SAMPLES, ProposalContext, compare_variants,
    propose_memory_with_context, render_memory, write_memory,
};
use autopilot_metrics::{
    BlueskyMetrics, DEFAULT_BLUESKY_READ_URL, DEFAULT_METRICS_TIMEOUT, MetricsRecord,
    MetricsSource, RecordContext, append_metrics, collect, read_metrics,
};
use autopilot_pipeline::{
    build_creation_prompt, build_triage_prompt, parse_shortlist, parse_variants,
};
use autopilot_providers::{
    JobKind, ModelUsage, ProbeOutcome, append_ledger, check_budget, complete_with_chain, delta,
    ledger_entry, load_config, opencode_usage, probe, read_baseline, resolve_chain, usage_for,
    write_baseline,
};
use autopilot_publish::x::create_post as create_x_post;
use autopilot_publish::{
    BlueskyCredentials, DEFAULT_BLUESKY_BASE_URL, DEFAULT_X_API_BASE_URL, X_ENV_VARS, XCredentials,
    build_post_record, build_tweet_body, create_post, create_session, missing_env_vars,
    now_rfc3339_millis, post_endpoint, post_url,
};
use autopilot_run::{CardAttachment, RunOptions, RunReport, StageOutcome};
use clap::{Args, Parser, Subcommand};
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
    /// Execute the full pipeline for one persona pack.
    Run(RunArgs),
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
    /// Deterministic pre-publish validation.
    Guardrail {
        #[command(subcommand)]
        command: GuardrailCommand,
    },
    /// Stage [2] — score harvested topics against a persona.
    Triage {
        #[command(subcommand)]
        command: TriageCommand,
    },
    /// Stage [3] — draft A/B variants for a platform.
    Create {
        #[command(subcommand)]
        command: CreateCommand,
    },
    /// Stage [4] — publish a validated draft.
    Publish {
        #[command(subcommand)]
        command: PublishCommand,
    },
    /// Stage [5] — collect engagement metrics for published posts.
    Metrics {
        #[command(subcommand)]
        command: MetricsCommand,
    },
    /// Stage [6] — learn from A/B performance and propose a memory update.
    Learn {
        #[command(subcommand)]
        command: LearnCommand,
    },
    /// Engagement — reply, like contextually and follow slowly, on its own schedule.
    Engage {
        #[command(subcommand)]
        command: EngageCommand,
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

#[derive(Args)]
struct RunArgs {
    /// Persona slug (`space-oddities`) or pack path.
    #[arg(long, value_parser = parse_persona)]
    persona: PathBuf,
    /// Harvest query (defaults to the persona's `harvest.queries`, or its niche).
    #[arg(long)]
    query: Option<String>,
    /// Maximum number of harvested topics.
    #[arg(long, default_value_t = 10)]
    limit: usize,
    /// SearXNG endpoint used by the harvest stage.
    #[arg(long, default_value = DEFAULT_SEARXNG_ENDPOINT)]
    endpoint: String,
    /// Restrict the run to one platform channel (default: every enabled one).
    #[arg(long, value_parser = parse_platform)]
    platform: Option<Platform>,
    /// Provider registry (read only in a live run).
    #[arg(long, default_value = "config/providers.yaml")]
    config: PathBuf,
    /// Journal file (defaults to `var/runs/<run-id>.jsonl`).
    #[arg(long)]
    journal: Option<PathBuf>,
    /// Persisted memory proposal to read the "avoid" warnings from (defaults to
    /// `var/learn/<persona>.json`): the creation stage is warned off claims the
    /// grounding review already refused.
    #[arg(long)]
    memory: Option<PathBuf>,
    /// Attach a generated post card to Bluesky posts: off, prompted (only when
    /// the variant carries an image prompt) or always.
    #[arg(long, value_parser = parse_card_attachment, default_value = "off")]
    attach_card: CardAttachment,
    /// Media configuration used only when the policy releases a short video
    /// (Piper binary/voice, motion, scene images). A missing file uses defaults.
    #[arg(long, default_value = "config/media.yaml")]
    media_config: PathBuf,
    /// Describe every stage without any LLM call, GPU work, network or publish.
    #[arg(long)]
    dry_run: bool,
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
        /// Baseline file: report consumption since it and refresh it.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Scope the statistics to a project (`.` = current directory), so the
        /// pipeline's consumption is isolated from interactive sessions.
        #[arg(long)]
        project: Option<String>,
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

#[derive(Subcommand)]
enum GuardrailCommand {
    /// Validate a draft against a persona pack (deterministic, no LLM).
    Check {
        /// Persona slug (`space-oddities`) or pack path.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long, value_parser = parse_platform)]
        platform: Platform,
        /// Draft text (mutually exclusive with `--file`).
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        /// Read the draft from a file (mutually exclusive with `--text`).
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

/// Accept both persona spellings on every `--persona` (HB-031).
///
/// A bare slug (`space-oddities`) resolves under `personas/`; anything that
/// looks like a path (`personas/space-oddities.yaml`) is used verbatim. The
/// flag previously meant a slug in `metrics collect` and a path in `learn run`,
/// so the same spelling behaved differently. One convention now: both work.
fn parse_persona(value: &str) -> Result<PathBuf, std::convert::Infallible> {
    Ok(resolve_persona_path(value))
}

fn parse_platform(value: &str) -> Result<Platform, String> {
    match value.to_ascii_lowercase().as_str() {
        "bluesky" => Ok(Platform::Bluesky),
        "x" => Ok(Platform::X),
        "linkedin" => Ok(Platform::Linkedin),
        "instagram" => Ok(Platform::Instagram),
        "tiktok" => Ok(Platform::Tiktok),
        "kwai" => Ok(Platform::Kwai),
        other => Err(format!(
            "unknown platform `{other}` (expected: bluesky, x, linkedin, instagram, tiktok, kwai)"
        )),
    }
}

fn parse_card_attachment(value: &str) -> Result<CardAttachment, String> {
    match value.to_ascii_lowercase().as_str() {
        "off" | "never" => Ok(CardAttachment::Off),
        "prompted" | "prompt" => Ok(CardAttachment::WhenPrompted),
        "always" => Ok(CardAttachment::Always),
        other => Err(format!(
            "unknown card mode `{other}` (expected: off, prompted, always)"
        )),
    }
}

#[derive(Subcommand)]
enum TriageCommand {
    /// Harvest topics and score them against a persona.
    Run {
        /// Persona slug (`space-oddities`) or pack path.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value = DEFAULT_SEARXNG_ENDPOINT)]
        endpoint: String,
        #[arg(long, default_value = "config/providers.yaml")]
        config: PathBuf,
        /// Print the prompt without calling any LLM (no quota spent).
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum CreateCommand {
    /// Draft A/B variants for a platform.
    Run {
        /// Persona slug (`space-oddities`) or pack path.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long, value_parser = parse_platform)]
        platform: Platform,
        #[arg(long)]
        topic: String,
        #[arg(long, default_value = "config/providers.yaml")]
        config: PathBuf,
        /// Print the prompt without calling any LLM (no quota spent).
        #[arg(long)]
        dry_run: bool,
        /// Run every variant through the deterministic guardrail.
        #[arg(long)]
        guard: bool,
    },
}

#[derive(Subcommand)]
enum PublishCommand {
    /// Verify publishing credentials without creating any post.
    Auth {
        /// Platform whose credentials to check.
        #[arg(long, value_parser = parse_platform, default_value = "bluesky")]
        platform: Platform,
        /// Base URL of the platform API (defaults to the platform's entry point).
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Publish a guardrail-approved draft to Bluesky.
    Bluesky {
        /// Persona slug (`space-oddities`) or pack path.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long)]
        text: String,
        #[arg(long, default_value = DEFAULT_BLUESKY_BASE_URL)]
        base_url: String,
        /// Print the record that would be posted without calling the API.
        #[arg(long)]
        dry_run: bool,
    },
    /// Publish a guardrail-approved draft to X (Twitter).
    X {
        /// Persona slug (`space-oddities`) or pack path.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long)]
        text: String,
        #[arg(long, default_value = DEFAULT_X_API_BASE_URL)]
        base_url: String,
        /// Print the request that would be sent without calling the API.
        #[arg(long)]
        dry_run: bool,
    },
}

/// A published post as `VARIANT=URI` (for example `A=at://did:plc:.../3kxyz`).
#[derive(Debug, Clone)]
struct PostArg {
    variant: String,
    uri: String,
}

/// One post to sample: explicit (`--post`) or discovered from the journal.
struct Target {
    variant: String,
    uri: String,
    published_at: Option<u64>,
}

/// The metrics checkpoints, in hours. `learn` compares variants, so a post is
/// sampled once it has lived a day and again once it has lived two — the same
/// two points the playbook promises.
const METRICS_CHECKPOINTS_HOURS: [u64; 2] = [24, 48];

/// The posts whose next checkpoint has passed and that have no sample for it.
///
/// This is what makes the metrics stage schedulable at all: a caller cannot know
/// the AT URIs, so the journal (what went out and when) and the store (what we
/// already sampled) decide. Without it the timer would have nothing to pass.
async fn due_targets(
    persona: &str,
    platform: Platform,
    store: &Path,
    runs_dir: &Path,
    now: u64,
) -> Result<Vec<Target>> {
    let published = autopilot_run::read_published(runs_dir).await?;
    let samples = read_metrics(store).await?;

    let mut due = Vec::new();
    for entry in published {
        if entry.persona != persona || entry.platform != platform.as_str() {
            continue;
        }
        let age_hours = now.saturating_sub(entry.published_at_unix) / 3_600;
        let wanted = METRICS_CHECKPOINTS_HOURS
            .iter()
            .filter(|checkpoint| age_hours >= **checkpoint)
            .count();
        let have = samples
            .iter()
            .filter(|sample| sample.uri == entry.uri)
            .count();
        if have < wanted {
            due.push(Target {
                variant: entry.variant,
                uri: entry.uri,
                published_at: Some(entry.published_at_unix),
            });
        }
    }
    Ok(due)
}

fn parse_post_arg(value: &str) -> Result<PostArg, String> {
    let (variant, uri) = value
        .split_once('=')
        .ok_or_else(|| format!("expected VARIANT=URI, got `{value}`"))?;
    if variant.trim().is_empty() || uri.trim().is_empty() {
        return Err(format!("both variant and URI are required in `{value}`"));
    }
    Ok(PostArg {
        variant: variant.trim().to_string(),
        uri: uri.trim().to_string(),
    })
}

#[derive(Subcommand)]
enum MetricsCommand {
    /// Collect engagement for published posts and append them to the store.
    Collect {
        #[arg(long, value_parser = parse_platform)]
        platform: Platform,
        /// Persona slug (`space-oddities`) or pack path that produced the posts.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long, default_value = "")]
        topic: String,
        /// A published post as `VARIANT=URI` (repeat for A/B).
        #[arg(long = "post", value_parser = parse_post_arg)]
        posts: Vec<PostArg>,
        /// JSONL store for the samples (under `var/`, which is gitignored).
        #[arg(long, default_value = "var/metrics.jsonl")]
        store: PathBuf,
        /// Run journals used to discover what is due when no `--post` is given.
        #[arg(long, default_value = "var/runs")]
        runs_dir: PathBuf,
        /// Base URL of the platform's public read API.
        #[arg(long)]
        base_url: Option<String>,
        /// Read the payload from a local file instead of the network (offline).
        #[arg(long, conflicts_with = "dry_run")]
        fixture: Option<PathBuf>,
        /// When the posts were published (Unix seconds), for schedule hints.
        #[arg(long)]
        published_at: Option<u64>,
        /// Print the request without calling the API or writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum LearnCommand {
    /// Analyse A/B metrics and propose a persona memory update.
    Run {
        /// Persona slug (`space-oddities`) or pack path.
        #[arg(long, value_parser = parse_persona)]
        persona: PathBuf,
        #[arg(long, value_parser = parse_platform)]
        platform: Platform,
        /// JSONL store written by the metrics stage.
        #[arg(long, default_value = "var/metrics.jsonl")]
        metrics: PathBuf,
        /// Run journals read for the [3.7] grounding refusals to feed the avoid
        /// memory (defaults to `var/runs`).
        #[arg(long, default_value = "var/runs")]
        runs_dir: PathBuf,
        /// Minimum samples per variant before the comparison is conclusive.
        #[arg(long, default_value_t = DEFAULT_MIN_SAMPLES)]
        min_samples: usize,
        /// How many winning posts to keep as few-shot examples.
        #[arg(long, default_value_t = DEFAULT_MAX_FEW_SHOT)]
        max_few_shot: usize,
        /// Persist the proposal here (omit to only print it).
        #[arg(long)]
        write: Option<PathBuf>,
        /// Print the proposal and never write, even if `--write` is given.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum EngageCommand {
    /// Run the engagement stage for one persona (separate from publishing).
    Run(EngageRunArgs),
    /// Print the local engagement state (what was already answered/liked/followed).
    State {
        #[arg(long, default_value = "var/engage/state.jsonl")]
        state: PathBuf,
    },
}

#[derive(Args)]
struct EngageRunArgs {
    /// Persona slug (`space-oddities`) or pack path to engage as.
    #[arg(long, value_parser = parse_persona)]
    persona: PathBuf,
    /// Engagement configuration (caps, windows, pacing).
    #[arg(long, default_value = "config/engage.yaml")]
    config: PathBuf,
    /// JSONL state under `var/` (gitignored) so nothing repeats.
    #[arg(long, default_value = "var/engage/state.jsonl")]
    state: PathBuf,
    /// Provider registry used to draft replies.
    #[arg(long, default_value = "config/providers.yaml")]
    providers: PathBuf,
    /// Offline fixture for `--dry-run` (notifications, threads, profiles).
    #[arg(long)]
    fixture: Option<PathBuf>,
    /// Describe the plan with no network call and no write.
    #[arg(long)]
    dry_run: bool,
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
        Command::Run(args) => run_autopilot(args).await,
        Command::Quota { command } => run_quota(command).await,
        Command::Harvest { command } => run_harvest(command).await,
        Command::Guardrail { command } => run_guardrail(command).await,
        Command::Triage { command } => run_triage(command).await,
        Command::Create { command } => run_create(command).await,
        Command::Publish { command } => run_publish(command).await,
        Command::Metrics { command } => run_metrics(command).await,
        Command::Learn { command } => run_learn(command).await,
        Command::Engage { command } => run_engage(command).await,
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

/// Execute the full pipeline for one persona pack (`autopilot run`).
///
/// The stage chaining lives in `autopilot-run`; this command only maps the CLI
/// arguments and renders the resulting report.
async fn run_autopilot(args: RunArgs) -> Result<ExitCode> {
    let options = RunOptions {
        persona_path: args.persona,
        query: args.query,
        limit: args.limit,
        endpoint: args.endpoint,
        platform: args.platform,
        config_path: args.config,
        journal_path: args.journal,
        memory_path: args.memory,
        dry_run: args.dry_run,
        card: args.attach_card,
        media_path: args.media_config,
    };

    let report = autopilot_run::run(options).await?;
    print_run_report(&report);
    if report.failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Render a run report as structured console output.
fn print_run_report(report: &RunReport) {
    println!("persona  : {}", report.persona);
    println!("run-id   : {}", report.run_id);
    println!(
        "mode     : {}",
        if report.dry_run {
            "dry-run (no LLM, no GPU, no network, no publish)"
        } else {
            "live"
        }
    );
    println!("journal  : {}", report.journal.display());
    for stage in &report.stages {
        println!(
            "[{:<9}] {:<9} {:<14} {}",
            stage.stage,
            stage.status.as_str(),
            stage_scope(stage),
            stage.detail
        );
    }
    for post in &report.published {
        println!(
            "published: {} variant {} -> {}",
            post.platform, post.variant, post.url
        );
    }
    println!(
        "result   : {} published, {}",
        report.published.len(),
        if report.failed { "FAILED" } else { "ok" }
    );
}

/// `platform/variant`, `platform` or `-` for a stage line.
fn stage_scope(stage: &StageOutcome) -> String {
    match (&stage.platform, &stage.variant) {
        (Some(platform), Some(variant)) => format!("{platform}/{variant}"),
        (Some(platform), None) => platform.clone(),
        _ => "-".to_string(),
    }
}

/// How long to wait for the `opencode stats` subprocess.
const STATS_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for one LLM completion.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a publishing API call.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);

async fn run_quota(command: QuotaCommand) -> Result<ExitCode> {
    match command {
        QuotaCommand::Check {
            model,
            budget_tokens,
            days,
            ledger,
            baseline,
            project,
        } => {
            let account = opencode_usage(days, None, STATS_TIMEOUT).await?;
            let attributed = match project.as_deref() {
                Some(project) => Some(opencode_usage(days, Some(project), STATS_TIMEOUT).await?),
                None => None,
            };

            let primary = attributed.as_ref().unwrap_or(&account);
            let Some(usage) = usage_for(primary, &model) else {
                let seen = primary
                    .iter()
                    .map(ModelUsage::id)
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("no usage recorded for {model} in the last {days} day(s)");
                println!("models seen: {seen}");
                return Ok(ExitCode::SUCCESS);
            };

            println!("model  : {}", usage.id());
            println!("window : last {days} day(s)");

            if attributed.is_some() {
                print_usage("attributed — pipeline project", usage);
                if let Some(all) = usage_for(&account, &model) {
                    print_usage("account — every session", all);
                }
            } else {
                print_usage("usage", usage);
            }

            let billable = usage_for(&account, &model).unwrap_or(usage);
            let check = check_budget(billable, budget_tokens);
            match check.budget_tokens {
                Some(0) => println!("budget : 0 (configured)"),
                Some(budget) => println!(
                    "budget : {budget} ({:.1}% of the account window)",
                    check.used_tokens as f64 * 100.0 / budget as f64
                ),
                None => println!("budget : not set"),
            }

            if let Some(path) = ledger {
                append_ledger(&path, &ledger_entry(usage, days)).await?;
                println!("ledger : appended to {}", path.display());
            }

            if let Some(path) = baseline {
                let previous = read_baseline(&path).await?;
                match previous {
                    Some(previous) => {
                        let consumed = delta(&previous, usage);
                        println!("--- since baseline ---");
                        println!("steps        : {}", consumed.steps);
                        println!("input tokens : {}", consumed.input_tokens);
                        println!("output tokens: {}", consumed.output_tokens);
                        println!("reasoning    : {}", consumed.reasoning_tokens);
                        println!("cache read   : {}", consumed.cache_read_tokens);
                        println!("total tokens : {}", consumed.total_tokens());
                        println!("cost (usd)   : {:.4}", consumed.cost_usd);
                    }
                    None => println!("--- baseline created (no previous snapshot) ---"),
                }
                write_baseline(&path, &ledger_entry(usage, days)).await?;
                println!("baseline : refreshed at {}", path.display());
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

fn print_usage(label: &str, usage: &ModelUsage) {
    println!("{label}");
    println!("  steps        : {}", usage.steps);
    println!("  input tokens : {}", usage.input_tokens);
    println!("  output tokens: {}", usage.output_tokens);
    println!("  reasoning    : {}", usage.reasoning_tokens);
    println!("  cache read   : {}", usage.cache_read_tokens);
    println!("  total tokens : {}", usage.total_tokens());
    println!("  cost (usd)   : {:.4}", usage.cost_usd);
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

async fn run_guardrail(command: GuardrailCommand) -> Result<ExitCode> {
    match command {
        GuardrailCommand::Check {
            persona,
            platform,
            text,
            file,
        } => {
            let pack = load_persona(&persona).await?;
            let draft = match (text, file) {
                (Some(text), _) => text,
                (None, Some(path)) => tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("reading draft {}", path.display()))?,
                (None, None) => {
                    eprintln!("provide either --text or --file");
                    return Ok(ExitCode::FAILURE);
                }
            };

            let verdict: Verdict = evaluate(&pack, platform, &draft);
            let name = persona
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("-");
            println!("persona  : {name}");
            println!("platform : {}", platform.as_str());
            println!("checks   : {} issue(s)", verdict.issues.len());
            for issue in &verdict.issues {
                println!("  x [{}] {}", issue.rule, issue.detail);
            }

            if verdict.approved {
                println!("verdict  : APPROVED");
                Ok(ExitCode::SUCCESS)
            } else {
                println!("verdict  : REJECTED");
                Ok(ExitCode::FAILURE)
            }
        }
    }
}

async fn run_triage(command: TriageCommand) -> Result<ExitCode> {
    match command {
        TriageCommand::Run {
            persona,
            query,
            limit,
            endpoint,
            config,
            dry_run,
        } => {
            let pack = load_persona(&persona).await?;
            let topics = SearxngClient::new(endpoint).search(&query, limit).await?;
            println!("harvest  : {} topic(s) for \"{query}\"", topics.len());

            let prompt = build_triage_prompt(&pack, &topics);
            if dry_run {
                println!("--- prompt (dry run, no LLM called) ---");
                println!("{prompt}");
                return Ok(ExitCode::SUCCESS);
            }

            let registry = load_config(&config).await?;
            let completion =
                complete_with_chain(&registry, &prompt, JobKind::Triage, COMPLETION_TIMEOUT)
                    .await?;
            println!(
                "provider : {} ({})",
                completion.provider_id, completion.model
            );

            let shortlist = parse_shortlist(&completion.text)?;
            println!("shortlist: {} topic(s)", shortlist.len());
            for (index, entry) in shortlist.iter().enumerate() {
                println!("{:>2}. {:.2}  {}", index + 1, entry.score, entry.title);
                if !entry.reason.is_empty() {
                    println!("      {}", entry.reason);
                }
                if !entry.url.is_empty() {
                    println!("      {}", entry.url);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_create(command: CreateCommand) -> Result<ExitCode> {
    match command {
        CreateCommand::Run {
            persona,
            platform,
            topic,
            config,
            dry_run,
            guard,
        } => {
            let pack = load_persona(&persona).await?;
            let prompt = build_creation_prompt(&pack, platform, &topic, None);
            if dry_run {
                println!("--- prompt (dry run, no LLM called) ---");
                println!("{prompt}");
                return Ok(ExitCode::SUCCESS);
            }

            let registry = load_config(&config).await?;
            let completion =
                complete_with_chain(&registry, &prompt, JobKind::Create, COMPLETION_TIMEOUT)
                    .await?;
            println!(
                "provider : {} ({})",
                completion.provider_id, completion.model
            );

            let variants = parse_variants(&completion.text)?;
            let mut rejected = 0usize;
            for variant in &variants {
                println!("--- variant {} ---", variant.label);
                println!("{}", variant.text);
                if !variant.hashtags.is_empty() {
                    println!("hashtags : {}", variant.hashtags.join(" "));
                }
                if !variant.image_prompt.is_empty() {
                    println!("image    : {}", variant.image_prompt);
                }
                if guard {
                    let verdict = evaluate(&pack, platform, &variant.text);
                    if verdict.approved {
                        println!("guardrail: APPROVED");
                    } else {
                        rejected += 1;
                        println!("guardrail: REJECTED");
                        for issue in &verdict.issues {
                            println!("  x [{}] {}", issue.rule, issue.detail);
                        }
                    }
                }
            }

            if guard && rejected > 0 {
                eprintln!("{rejected} variant(s) rejected by the guardrail");
                Ok(ExitCode::FAILURE)
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

async fn run_publish(command: PublishCommand) -> Result<ExitCode> {
    match command {
        PublishCommand::Auth { platform, base_url } => match platform {
            Platform::Bluesky => {
                let base_url = base_url.unwrap_or_else(|| DEFAULT_BLUESKY_BASE_URL.to_string());
                let credentials = BlueskyCredentials::from_env()?;
                let session = create_session(&base_url, &credentials, PUBLISH_TIMEOUT).await?;
                println!("authenticated : {}", session.handle);
                println!("did           : {}", session.did);
                Ok(ExitCode::SUCCESS)
            }
            Platform::X => {
                let missing = missing_env_vars();
                if !missing.is_empty() {
                    eprintln!("missing X credential variable(s): {}", missing.join(", "));
                    return Ok(ExitCode::FAILURE);
                }
                println!("platform      : x");
                println!("credentials   : present (OAuth 1.0a user context)");
                for name in X_ENV_VARS {
                    println!("  {name}: set");
                }
                println!(
                    "note          : presence check only — a live round-trip needs HB-026 credentials"
                );
                Ok(ExitCode::SUCCESS)
            }
            other => bail!(
                "credential check for `{}` is not implemented yet",
                other.as_str()
            ),
        },
        PublishCommand::Bluesky {
            persona,
            text,
            base_url,
            dry_run,
        } => {
            let pack = load_persona(&persona).await?;

            // The guardrail is mandatory: an unvalidated draft is never published.
            let verdict = evaluate(&pack, Platform::Bluesky, &text);
            if !verdict.approved {
                for issue in &verdict.issues {
                    println!("  x [{}] {}", issue.rule, issue.detail);
                }
                eprintln!("refusing to publish: the guardrail rejected the draft");
                return Ok(ExitCode::FAILURE);
            }
            println!("guardrail : APPROVED");

            let record = build_post_record(&text, &now_rfc3339_millis());
            if dry_run {
                println!("--- would post (dry run, no API call) ---");
                println!("{}", serde_json::to_string_pretty(&record)?);
                return Ok(ExitCode::SUCCESS);
            }

            let credentials = BlueskyCredentials::from_env()?;
            let session = create_session(&base_url, &credentials, PUBLISH_TIMEOUT).await?;
            let uri = create_post(&base_url, &session, &record, PUBLISH_TIMEOUT).await?;
            println!("published : {}", post_url(&session.handle, &uri));
            Ok(ExitCode::SUCCESS)
        }
        PublishCommand::X {
            persona,
            text,
            base_url,
            dry_run,
        } => {
            let pack = load_persona(&persona).await?;

            // The guardrail is mandatory: an unvalidated draft is never published.
            let verdict = evaluate(&pack, Platform::X, &text);
            if !verdict.approved {
                for issue in &verdict.issues {
                    println!("  x [{}] {}", issue.rule, issue.detail);
                }
                eprintln!("refusing to publish: the guardrail rejected the draft");
                return Ok(ExitCode::FAILURE);
            }
            println!("guardrail : APPROVED");

            let body = build_tweet_body(&text);
            if dry_run {
                println!("--- would post (dry run, no API call) ---");
                println!("POST {}", post_endpoint(&base_url));
                println!("{}", serde_json::to_string_pretty(&body)?);
                return Ok(ExitCode::SUCCESS);
            }

            let credentials = XCredentials::from_env()?;
            let post = create_x_post(&base_url, &credentials, &text, PUBLISH_TIMEOUT).await?;
            println!("published : {}", post.url);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Build the metrics adapter for a platform (Bluesky is the only one so far).
fn build_metrics_source(
    platform: Platform,
    base_url: Option<String>,
) -> Result<Box<dyn MetricsSource>> {
    match platform {
        Platform::Bluesky => Ok(Box::new(BlueskyMetrics::new(
            base_url.unwrap_or_else(|| DEFAULT_BLUESKY_READ_URL.to_string()),
        ))),
        other => bail!(
            "metrics collection for `{}` is not implemented yet",
            other.as_str()
        ),
    }
}

async fn run_metrics(command: MetricsCommand) -> Result<ExitCode> {
    match command {
        MetricsCommand::Collect {
            platform,
            persona,
            topic,
            posts,
            store,
            base_url,
            fixture,
            published_at,
            dry_run,
            runs_dir,
        } => {
            let source = build_metrics_source(platform, base_url)?;

            // HB-031: `--persona` takes a slug or a path here too. The canonical
            // slug comes from the pack, so the store's `persona` field always
            // matches the slug the run journals wrote.
            let pack = load_persona(&persona).await?;
            let persona = pack.persona;

            // What to sample. The scheduled case passes no `--post`: a caller
            // cannot know the AT URIs, so the journal (what went out and when)
            // and the store (what we already sampled) decide what is due.
            let targets: Vec<Target> = if posts.is_empty() {
                due_targets(&persona, platform, &store, &runs_dir, now_unix()).await?
            } else {
                posts
                    .into_iter()
                    .map(|post| Target {
                        variant: post.variant,
                        uri: post.uri,
                        published_at,
                    })
                    .collect()
            };

            if targets.is_empty() {
                println!("nothing due for a metrics sample");
                return Ok(ExitCode::SUCCESS);
            }

            if dry_run {
                println!("--- request (dry run, no API call, nothing written) ---");
                for target in &targets {
                    let request = source.request(std::slice::from_ref(&target.uri))?;
                    println!("GET {}", request.url);
                    for (key, value) in &request.query {
                        println!("  {key}={value}");
                    }
                }
                return Ok(ExitCode::SUCCESS);
            }

            let mut stored = 0usize;
            for target in &targets {
                let ids = vec![target.uri.clone()];
                let parsed = match &fixture {
                    Some(path) => {
                        let raw = tokio::fs::read_to_string(path)
                            .await
                            .with_context(|| format!("reading fixture {}", path.display()))?;
                        source.parse(&raw)?
                    }
                    None => collect(&*source, &ids, DEFAULT_METRICS_TIMEOUT).await?,
                };

                let Some(metrics) = parsed.iter().find(|item| item.uri == target.uri) else {
                    println!("missing : {} (no metrics returned)", target.uri);
                    continue;
                };
                let context = RecordContext {
                    captured_at_unix: now_unix(),
                    persona: persona.clone(),
                    variant: target.variant.clone(),
                    topic: topic.clone(),
                    published_at_unix: target.published_at,
                };
                let record = MetricsRecord::from_post(platform, metrics, context);
                append_metrics(&store, &record).await?;
                stored += 1;
                println!(
                    "variant {}: {} like(s), {} reply(ies), {} repost(s), {} quote(s) -> {}",
                    record.variant,
                    record.likes,
                    record.replies,
                    record.reposts,
                    record.quotes,
                    record.uri
                );
            }
            println!("stored  : {stored} sample(s) in {}", store.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The samples that belong to one persona and platform.
///
/// The metrics store is shared across personas, and sequential A/B compares a
/// variant over time — so filtering by variant alone must never merge two
/// personas' posts into one comparison (`scoring-model.md` §3, HB-040).
fn records_for(
    records: Vec<MetricsRecord>,
    persona: &str,
    platform: Platform,
) -> Vec<MetricsRecord> {
    records
        .into_iter()
        .filter(|record| record.persona == persona && record.platform == platform)
        .collect()
}

async fn run_learn(command: LearnCommand) -> Result<ExitCode> {
    match command {
        LearnCommand::Run {
            persona,
            platform,
            metrics,
            runs_dir,
            min_samples,
            max_few_shot,
            write,
            dry_run,
        } => {
            let pack = load_persona(&persona).await?;
            let records = records_for(read_metrics(&metrics).await?, &pack.persona, platform);
            if records.is_empty() {
                println!(
                    "no metrics for {} in {} — nothing to learn yet",
                    platform.as_str(),
                    metrics.display()
                );
            }

            // HB-046: the [3.7] grounding reviewer's refusals must teach, not
            // just gate. Read them from the run journals (the metrics store does
            // not carry them) and feed the *claims* into the avoid memory, so the
            // creation stage is warned off what the source did not support.
            let rejections = autopilot_run::read_grounding_rejections(&runs_dir).await?;
            let claims: Vec<String> = rejections
                .iter()
                .filter(|rejection| {
                    rejection.persona == pack.persona && rejection.platform == platform.as_str()
                })
                .map(|rejection| rejection.reason.clone())
                .collect();
            let claim_refs: Vec<&str> = claims.iter().map(String::as_str).collect();
            let context = ProposalContext {
                config: CraftConfig::default(),
                guardrail_rejected: &[],
                grounding_rejected: &claim_refs,
            };

            let outcome = compare_variants(&records, min_samples);
            let update = propose_memory_with_context(
                &records,
                &pack.persona,
                platform,
                &outcome,
                max_few_shot,
                now_unix(),
                &context,
            );

            println!("--- proposed memory update ---");
            print!("{}", render_memory(&update)?);
            println!(
                "grounding: {} refusal(s) fed into the avoid memory",
                claim_refs.len()
            );
            println!("outcome : {}", update.reason);

            match (dry_run, write) {
                (false, Some(path)) => {
                    write_memory(&path, &update).await?;
                    println!("memory  : written to {}", path.display());
                }
                _ => println!("memory  : proposal only — pass --write <path> to persist it"),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Run the engagement stage (`autopilot engage run`) or print its state.
async fn run_engage(command: EngageCommand) -> Result<ExitCode> {
    match command {
        EngageCommand::Run(args) => {
            let options = EngageOptions {
                persona_path: args.persona,
                config_path: args.config,
                state_path: args.state,
                providers_path: args.providers,
                fixture: args.fixture,
                dry_run: args.dry_run,
            };
            let report = autopilot_engage::run(options).await?;
            print_engage_report(&report);
            if report.failed {
                Ok(ExitCode::FAILURE)
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        EngageCommand::State { state } => {
            let log = autopilot_engage::ActionLog::load(&state).await?;
            println!("state   : {}", state.display());
            println!("entries : {}", log.entries().len());
            for kind in [
                autopilot_engage::ActionKind::Reply,
                autopilot_engage::ActionKind::Like,
                autopilot_engage::ActionKind::Follow,
            ] {
                println!("  {:<7} {}", kind.as_str(), log.count_since(kind, 0));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Render an engagement report as structured console output.
fn print_engage_report(report: &EngageReport) {
    println!("persona  : {}", report.persona);
    println!(
        "mode     : {}",
        if report.dry_run {
            "dry-run (no network, no write)"
        } else {
            "live"
        }
    );
    println!(
        "window   : {}",
        if report.in_window {
            "inside"
        } else {
            "outside"
        }
    );
    println!(
        "kill-switch: {}",
        if report.kill_switch { "ENGAGED" } else { "off" }
    );
    println!("state    : {}", report.state_path.display());
    if report.skipped_run {
        println!("result   : sporadic skip — nothing to do this run");
    }
    for note in &report.notes {
        println!("note     : {note}");
    }
    for line in &report.planned {
        println!("planned  : {line}");
    }
    for action in &report.executed {
        println!(
            "[{:<9}] {:<8} {:<45} {}",
            action.action, action.status, action.target, action.detail
        );
    }
    println!("result   : {} action(s) executed", report.executed.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(persona: &str, platform: Platform) -> MetricsRecord {
        MetricsRecord::from_post(
            platform,
            &autopilot_metrics::PostMetrics {
                uri: format!("at://{persona}"),
                text: "corpo".into(),
                likes: 1,
                replies: 0,
                reposts: 0,
                quotes: 0,
            },
            RecordContext {
                captured_at_unix: 1,
                persona: persona.into(),
                variant: "A".into(),
                topic: "t".into(),
                published_at_unix: None,
            },
        )
    }

    #[test]
    fn the_persona_flag_accepts_a_slug_or_a_path() {
        // HB-031: every `--persona` shares one convention — `metrics collect`
        // (which used a slug) and `learn run` (which demanded a path) now both
        // take either spelling.
        assert_eq!(
            parse_persona("space-oddities").expect("infallible"),
            PathBuf::from("personas/space-oddities.yaml")
        );
        assert_eq!(
            parse_persona("personas/space-oddities.yaml").expect("infallible"),
            PathBuf::from("personas/space-oddities.yaml")
        );
    }

    #[test]
    fn records_for_keeps_only_one_persona_and_platform() {
        // Sequential A/B compares a variant over time, so two personas' posts
        // must never land in the same comparison (the store is shared).
        let records = vec![
            sample("a", Platform::Bluesky),
            sample("b", Platform::Bluesky),
            sample("a", Platform::X),
        ];
        let kept = records_for(records, "a", Platform::Bluesky);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].persona, "a");
        assert_eq!(kept[0].platform, Platform::Bluesky);
    }
}

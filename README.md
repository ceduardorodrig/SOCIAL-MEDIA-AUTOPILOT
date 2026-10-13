# SUMAENIMA-SOCIALMEDIA-AUTOPILOT

> **Private repository.** Autonomous social-media engine: it detects trends, writes
> content, publishes across channels and learns from performance — in a loop, with no
> manual step. Phase 0 (harness foundation) is implemented here.

## What this is

A **harness engine** with **persona packs**. The engine is written once; a persona pack is
the data package that configures it. One persona drives **many** platform channels, always
adapting the master content per channel — the voice stays the same, the format changes.

```
PERSONA PACKS (YAML)              HARNESS ENGINE                 CHANNELS
ciencia-curiosidades ──┐      [1] harvest trends                bluesky · x (phase 1)
horoscopo-diario ──────┴──►   [2] triage   (LLM chain)          instagram · linkedin
                              [3] create   (LLM chain)          tiktok (phase 2)
                              [3.5] guardrail (deterministic)   kwai (phase 3, experimental)
                              [4] publish  (per-channel adapter)
                              [5] metrics  (24/48h collection)
                              [6] learn    → writes back into the persona pack
```

## LLM provider chain (fail-safe)

Cloud first, local as the last resort. The chain is **data**, resolved at run time by probing
every provider's availability:

| Priority | Provider | Role |
|---|---|---|
| 100 | **OpenCode** (`opencode/mimo-v2.6-flash-free`) | Primary — free tier, already paid |
| 200 | **Gemini** Developer API free tier | Fail-safe 1 |
| 300 | **Ollama** (local, psicopompo RTX 5050) | Fail-safe 2 — GPU idle windows only, VRAM released after each job |

Because the OpenCode catalogue changes often, the model id lives in
[`config/providers.yaml`](config/providers.yaml) — swapping a model is a one-line edit, never a
code change.

> Local inference is subject to the homelab GPU policy: the GPU is a scarce resource. Jobs that
> cannot get VRAM **wait** for it (no CPU fallback — ADR-020), and VRAM is always released after
> finishing. This is centralised by the `hl-infer` gateway on psicopompo.

## Layout

```
crates/
  autopilot-core/       persona pack model + validation
  autopilot-guardrail/  deterministic pre-publish validation
  autopilot-harvest/    trend collection (SearXNG JSON API)
  autopilot-pipeline/   triage (score topics) and creation (A/B variants) prompts + parsers
  autopilot-providers/  provider registry, availability probes, fallback chain, quota, completion
  autopilot-publish/    platform adapters (Bluesky / AT Protocol, X / OAuth 1.0a)
  autopilot-metrics/    stage [5] — engagement collection (adapter trait) + JSONL store
  autopilot-learn/      stage [6] — pure A/B scoring + persona memory proposal
  autopilot-run/        `autopilot run` — chains the stages for one persona
  autopilot-cli/        `autopilot` binary
config/providers.yaml   LLM chain (data, no secrets — env var names only)
personas/*.yaml         persona packs
var/                    local stores (gitignored): run journals, metrics JSONL, memory proposals
```

## Usage

```bash
cargo build --release

# Persona packs
autopilot personas list
autopilot personas validate

# `--persona` takes a slug or a path, in every subcommand (HB-031):
#   --persona space-oddities   ==   --persona personas/space-oddities.yaml
#
# A pack may declare its own harvest subjects, walked in rotation (HB-033) so
# different personas harvest different sources. Without the block, the engine
# falls back to the humanised niche (`astronomy-space-curiosities` becomes
# `astronomy space curiosities`).
#   harvest:
#     queries:
#       - "space mission anomalies"
#       - "strange telescope observations"

# One full run for a persona: harvest → triage → create → guardrail → publish.
# --dry-run describes every stage with no LLM call, no GPU work, no network and
# no publish. A live run needs the publishing credentials in the environment and
# respects the hl-infer batch windows; metrics/learning are scheduled separately.
autopilot run --persona personas/ciencia-curiosidades.yaml --dry-run
autopilot run --persona personas/ciencia-curiosidades.yaml --platform bluesky
# Every run appends an auditable JSONL journal under var/runs/ (gitignored).

# Provider availability (all three layers must answer)
autopilot providers check

# Resolved fallback chain for the next run
autopilot providers chain

# Trend harvesting (SearXNG JSON API on kavure)
autopilot harvest search --query "descobertas científicas curiosas" --limit 10

# Free-tier quota instrumentation
# --project scopes the statistics to this repository, isolating the pipeline's
# own consumption from interactive sessions.
autopilot quota check \
  --model opencode/mimo-v2.6-flash-free \
  --budget-tokens 5000000 \
  --project . \
  --ledger var/quota-ledger.jsonl \
  --baseline var/quota-baseline.json   # reports consumption since the last snapshot

# Deterministic pre-publish validation (no LLM)
autopilot guardrail check \
  --persona personas/ciencia-curiosidades.yaml \
  --platform bluesky \
  --text "Draft to validate #tag"

# Stage [2] — triage: harvest and score topics (--dry-run prints the prompt only)
autopilot triage run --persona personas/ciencia-curiosidades.yaml --query "descobertas científicas 2026" --limit 5

# Stage [3] — creation: A/B variants for a platform, validated by the guardrail
autopilot create run \
  --persona personas/ciencia-curiosidades.yaml \
  --platform bluesky \
  --topic "O cérebro gasta ~20% da energia do corpo em repouso" \
  --guard

# Stage [4] — publish to Bluesky (the guardrail runs first and may refuse)
autopilot publish auth                                         # verify Bluesky credentials
autopilot publish bluesky --persona personas/ciencia-curiosidades.yaml \
  --text "Draft" --dry-run                                    # print the record only

# Stage [4] — attach a generated post card to Bluesky posts. `off` (default) keeps
# text-only posting; `prompted` attaches a card only when the variant carries an
# image prompt; `always` attaches one to every post. The card is an SVG template
# rasterised by rsvg-convert — no GPU and no image-generation model.
autopilot run --persona personas/tech-curiosities.yaml --attach-card prompted

# Generate a card offline (no network, no model): prints the dimensions and the
# alt text; `--dry-run` prints the filled SVG and writes nothing.
cargo run -p autopilot-publish --bin card -- render \
  --topic "Rust 1.95 ships let-chains" \
  --text "No more nesting: `if let Some(x) = y && x > 0`." \
  --footer example.bsky.social --out /tmp/card.png

# Stage [4] — publish to X (Twitter). OAuth 1.0a signing is pure Rust and
# unit-tested against the published reference vectors; `--dry-run` prints the
# request with no API call and needs no credentials.
autopilot publish auth --platform x                            # check the X credentials (offline)
autopilot publish x --persona personas/ciencia-curiosidades.yaml \
  --text "Draft" --dry-run                                     # print the request only

# Stage [5] — metrics: collect engagement for published posts into a JSONL store.
# `--fixture <file>` replays a saved payload with no network call (offline mode);
# `--dry-run` only prints the request that would be made.
autopilot metrics collect \
  --platform bluesky --persona ciencia-curiosidades --topic "cérebro" \
  --post A=at://did:plc:.../3ka --post B=at://did:plc:.../3kb \
  --store var/metrics.jsonl

# Stage [6] — learning: score the A/B variants and propose a persona memory update.
# It also reads the run journals (`--runs-dir`, default `var/runs`) so the [3.7]
# grounding refusals feed the "avoid" memory, and `autopilot run` reads that
# proposal back (default `var/learn/<slug>.json`) to warn the creation stage off
# the claims it already refused. Without `--write` it only prints the proposal;
# the persona pack is never rewritten silently.
#
# HB-034 §4: the same run joins each post's journalled format with its metrics
# and, with enough samples (`--format-min-samples`, default 8), proposes moving
# the weekly per-format budget by at most one post a week, normalised against the
# persona's own median. `autopilot run` consumes that proposal.
autopilot learn run \
  --persona personas/ciencia-curiosidades.yaml \
  --platform bluesky \
  --metrics var/metrics.jsonl \
  --min-samples 2 \
  --write var/learn/ciencia-curiosidades.json
```

Format/frequency policy (HB-034) is data, not code: a pack sets `publishing.formats`
(weekly text/card/video budgets) and may override the **warming ramp** with
`publishing.warming.started_on`. The ramp is otherwise **autonomous** — the engine
detects the account's birth from the first publication in its journals, and an empty
base is a mature account with no ramp. A slot is an *opportunity*, not an obligation:
the model is told which formats the budget leaves available and chooses the format per
topic, with the engine keeping only a deterministic safety net.

Publishing credentials come from the environment (SOPS store), never from the repository:

```bash
export BLUESKY_HANDLE=...        # handle
export BLUESKY_APP_PASSWORD=...  # an app password, not the account password

export X_API_KEY=...             # OAuth 1.0a consumer key
export X_API_SECRET=...          # OAuth 1.0a consumer secret
export X_ACCESS_TOKEN=...        # OAuth 1.0a access token (user context)
export X_ACCESS_TOKEN_SECRET=... # OAuth 1.0a access token secret
```

Provider credentials are read from the environment, never from the repo:

```bash
export GEMINI_API_KEY=...   # from the SOPS store (never committed)
```

## Status

**Phase 0 — harness foundation.** Implemented: persona pack model + validation, provider
registry with live health probes, fallback-chain resolution and completion across the chain,
trend harvesting (SearXNG), free-tier quota instrumentation (snapshot, budget, JSONL ledger,
delta and project-scoped attribution), the deterministic guardrail, the triage (stage 2)
and creation (stage 3) prompts + parsers, the **Bluesky publisher** (stage 4, AT Protocol)
and the **X (Twitter) publisher** (stage 4, API v2 with pure-Rust OAuth 1.0a signing,
offline-tested against the RFC 5849 and classic Twitter reference vectors), both with a
mandatory guardrail gate, and the **metrics** (stage 5, platform adapter trait +
JSONL store) and **learning** (stage 6, pure A/B scoring + persona memory proposal) stages. The
Bluesky publisher now also carries **images**: it uploads a blob with
`com.atproto.repo.uploadBlob` and attaches an `app.bsky.embed.images` embed with mandatory
alt text, and it can generate the image itself as a deterministic **post card** (a versioned
SVG template rasterised by `rsvg-convert`, no GPU) through `autopilot run --attach-card`.
The **`autopilot run` orchestrator** now chains harvest → triage → creation → guardrail →
publish for one persona, with a safe `--dry-run` and a per-run JSONL journal under
`var/runs/`; a non-admitted `hl-infer` lease aborts a stage instead of running the
GPU unarbitrated. Next: the remaining channels (LinkedIn, Instagram, TikTok, Kwai) and the
live credentials (HB-026) that let the X path run for real.

| Phase | Scope |
|---|---|
| 0 | Harness foundation *(current)* |
| 0.5 | `hl-infer` — centralised GPU gateway (VRAM ledger, queue, fixed windows) |
| 1 | Autonomous loop on Bluesky + X |
| 2 | Instagram / LinkedIn / TikTok + short video (Piper TTS) |
| 3 | Kwai (experimental) + ComfyUI image generation |
| 4 | SUMÆNIMÁ persona on the same engine |

## Governance

- Part of the **Mnemocine** homelab ecosystem; product naming follows the SUMÆNIMÁ (`sae-` / brand)
  convention, municipality services follow `hl-`, hosts carry Tupi-Guarani names.
- **No plaintext secrets** in this repo — SOPS store only.
- Rust-first: no Python; errors are handled with `?`/`match`, never `unwrap()`/`expect()`.

<div align="center">

### 🛡️ Human-in-the-Loop Agentic Engineering & Deterministic Governance

> **Architected by an Anthropologist, Built with Autonomous AI Agents, Governed by Deterministic Code.**
> 
> This project was developed through rigorous human-AI pair programming led by **Carlos Eduardo Rodrigues** ([@ceduardorodrig](https://github.com/ceduardorodrig)) — an anthropologist and product architect using autonomous coding agents under strict, sub-millisecond static governance.
>
> Every commit, driver, and system architecture is continuously audited and enforced by 🤖 **[StenioSentinel](https://github.com/ceduardorodrig/STENIO-SENTINEL)** (our native Rust quality gate) with zero tolerance for hallucinated tests, blind merges, or bypassed checks.

</div>


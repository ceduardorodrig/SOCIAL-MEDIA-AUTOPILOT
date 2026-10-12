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
  autopilot-publish/    platform adapters (Bluesky / AT Protocol)
  autopilot-cli/        `autopilot` binary
config/providers.yaml   LLM chain (data, no secrets — env var names only)
personas/*.yaml         persona packs
```

## Usage

```bash
cargo build --release

# Persona packs
autopilot personas list
autopilot personas validate

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
autopilot publish auth                                         # verify credentials
autopilot publish bluesky --persona personas/ciencia-curiosidades.yaml \
  --text "Draft" --dry-run                                    # print the record only
```

Publishing credentials come from the environment (SOPS store), never from the repository:

```bash
export BLUESKY_HANDLE=...        # handle
export BLUESKY_APP_PASSWORD=...  # an app password, not the account password
```

Provider credentials are read from the environment, never from the repo:

```bash
export GEMINI_API_KEY=...   # from the SOPS store (never committed)
```

## Status

**Phase 0 — harness foundation.** Implemented: persona pack model + validation, provider
registry with live health probes, fallback-chain resolution and completion across the chain,
trend harvesting (SearXNG), free-tier quota instrumentation (snapshot, budget, JSONL ledger,
delta and project-scoped attribution), the deterministic guardrail, and the triage (stage 2)
and creation (stage 3) prompts + parsers, and the **Bluesky publisher** (stage 4, AT Protocol)
with a mandatory guardrail gate. Next: metrics + learning loop, then X and the remaining channels.

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


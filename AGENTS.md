---
tags: [meta, agents, governance]
---

# AGENTS.md — Rules for AI Agents (SUMAENIMA-SOCIALMEDIA-AUTOPILOT)

**Language tier:** A (English) — docs, logs and CLI strings in English; see
`governance/language-policy.md` in the vault. The machine-readable half of this configuration lives in
`steniocheck.toml` (tier, security, architecture and the `GOV-LOG-EN` custom rule).

This repository is part of the **Mnemocine** homelab ecosystem and is governed by
**StenioSentinel**. Every AI agent working here must follow the rules below.

## Absolute Laws

1. **Deterministic governance.** The repository must pass the audit at all times:
   ```bash
   stenio --path .
   ```
   Suppression directives (`#[allow(...)]` to dodge a real rule, `// stenio-ignore`,
   `--no-verify`) are **forbidden**. If a rule cannot be satisfied, stop and escalate
   with the file, line, the accused rule and what was tried.

2. **Rust-first.** No Python in this repository (`ARCH-NO-PYTHON`). If a third-party
   capability only exists in another language, it belongs in an isolated container,
   never in this codebase.

3. **No panics.** Production code never uses `unwrap()`/`expect()`. Handle errors with
   `?`, `match` or safe fallbacks (`unwrap_or_default()`). `expect()` is not a fix for
   `unwrap()` — it is the same panic.

4. **No hardcoded secrets.** Credentials live in the SOPS store and are referenced by
   environment-variable **name** only (e.g. `api_key_env: GEMINI_API_KEY`).

5. **DRY.** Shared logic is extracted (crate or module), never duplicated across
   personas, providers or adapters.

## Architecture Rules

- **One engine, many personas.** The harness is stateless; behaviour is configured by
  **persona packs** (a tenant's `personas/*.yaml`; the repo ships `examples/personas/`).
  Never hardcode persona-specific logic in the
  engine.
- **LLM providers are data.** The chain (`opencode` → `gemini` → `ollama`) lives in
  `config/providers.yaml`. Swapping a model is a config edit, never a code change.
- **No CPU fallback for GPU work.** GPU jobs that cannot obtain VRAM **wait** for it
  (ADR-020). VRAM is scarce and is always released after each job.
- **Cloud first.** Prefer the cloud providers; local inference is the last resort.

## Workflow

1. Read the relevant persona pack / provider config before changing behaviour.
2. Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all`.
3. Run `stenio --path .` before concluding.
4. Document any homelab-affecting change in the `mnemocine` vault in the same step.

<div align="center">

### 🛡️ Human-in-the-Loop Agentic Engineering & Deterministic Governance

> **Architected by an Anthropologist, Built with Autonomous AI Agents, Governed by Deterministic Code.**
>
> This project was developed through rigorous human-AI pair programming led by **Carlos Eduardo Rodrigues** ([@ceduardorodrig](https://github.com/ceduardorodrig)) — an anthropologist and product architect using autonomous coding agents under strict, sub-millisecond static governance.
>
> Every commit, driver, and system architecture is continuously audited and enforced by 🤖 **[StenioSentinel](https://github.com/ceduardorodrig/STENIO-SENTINEL)** (our native Rust quality gate) with zero tolerance for hallucinated tests, blind merges, or bypassed checks.

</div>

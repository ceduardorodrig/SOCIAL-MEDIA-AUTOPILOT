# Changelog

All notable changes to the engine are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-21

### Added

- The autonomous loop: harvest → triage → create → editor → guardrail → publish →
  metrics → learn, with a separate engagement stage (reply / like / follow at a
  human pace).
- **Tenant model**: the engine is code; a tenant owns the persona packs, the
  identities (`channel.handle`), the credential **names** (`app_password_env`,
  `x_env`) and the state (`var/`). A session for a different account is rejected.
- Deterministic guardrail (blocklist, banned phrases, hashtags, platform limit,
  emoji), A/B variants, the warming ramp and the per-format weekly budget.
- Bluesky (AT Protocol) and X (OAuth 1.0a) adapters, the SVG card renderer
  (`rsvg-convert`) and the short-video pipeline (Piper + ffmpeg, validated).
- `examples/` template tenant, `docs/tenancy.md`, `SECURITY.md` and the
  `AGPL-3.0-only` license with a commercial option (`COMMERCIAL.md`).

### Changed

- The repository is the **engine** only: the production persona packs live in a
  private tenant and are not part of the public tree or its history.
- The legacy global `BLUESKY_HANDLE` is no longer used by the pipeline; the
  channel handle is the identity and the app password is read from the variable
  the pack names.

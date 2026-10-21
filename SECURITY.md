# Security policy

## Reporting a vulnerability

Please report vulnerabilities **privately** through GitHub Security Advisories
("Security" → "Report a vulnerability"). Do not open a public issue for a
security problem.

## Scope

This engine publishes to social accounts. The repository contains **no
credentials**: persona packs name environment variables, and `var/` (state) is
ignored. A leaked app password must be revoked in the platform — patching the
repository is not enough.

## Design guarantees

- The guardrail is deterministic and runs before every publish.
- A draft pack (`active: false`) is validated but never acts.
- A login session for a different account is rejected
  (`create_session_for_channel`).
- Production code has no `unwrap()`/`expect()` panic paths (governance gate).
- Secrets are referenced by name only; the repository is scanned by the
  governance gate for accidental credentials.

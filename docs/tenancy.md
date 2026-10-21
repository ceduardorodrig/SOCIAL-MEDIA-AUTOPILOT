# Tenancy — engine, tenant, identity

This repository is the **engine**. A **tenant** is the unit that owns one or
more persona packs, their account identities, their engagement policy and their
state. The engine contains no handle and no secret, so a tenant can be a
directory, a private repository or a customer's deployment.

## The split

| Engine (this repository) | Tenant (private, per customer) |
|---|---|
| `crates/`, `examples/` | `personas/<slug>.yaml` |
| persona schema, guardrail, publishers | voice, niche, schedule, rubric, assets |
| `publish` / `engage` / `metrics` / `learn` | `config/engage/<slug>.yaml` |
| `docs/` and tests | `config/providers.yaml` |
| CI and the governance gate | `var/` (journals, cards, metrics, state) |

The engine is stateless. The tenant is the state. Two tenants never share a
`var/`: the wrapper runs each one with its own working directory.

## Layout

```
tenant/
├── personas/
│   ├── tech-curiosities.yaml
│   └── tech-curiosities/
│       └── assets/              # avatar, card art (optional)
├── config/
│   ├── providers.yaml
│   ├── media.yaml
│   └── engage/
│       └── tech-curiosities.yaml
└── var/                         # created by the engine; gitignored
    ├── runs/
    ├── learn/
    ├── cards/
    ├── media/
    └── engage/state.jsonl
```

`examples/` is a **template tenant**: copy it, edit the pack, set the variables.
The example pack is a draft (`active: false`) and uses `example.bsky.social`, so
it never posts until you change both.

## Identity and credentials

The pack declares **names**, never values:

```yaml
channels:
  - platform: bluesky
    handle: "your.account.bsky.social"   # the identity: login and card footer
    app_password_env: "BSKY_APP_PASSWORD"
    enabled: true
  - platform: x
    handle: "your_handle"
    enabled: false
    x_env:
      api_key: "X_API_KEY"
      api_secret: "X_API_SECRET"
      access_token: "X_ACCESS_TOKEN"
      access_token_secret: "X_ACCESS_TOKEN_SECRET"
```

- The **handle is the identity**. The publisher logs in with it and rejects a
  session that belongs to another account (`create_session_for_channel`).
- The **secret is named**. `app_password_env` / `x_env` point at variables the
  deployment exports (SOPS, `EnvironmentFile`, a secret manager). Without the
  block, the legacy `BLUESKY_APP_PASSWORD` / `X_*` names are used.
- Two channels in the same process can use different accounts. Nothing is
  global, so one tenant can run many personas with distinct identities.

## Running

```bash
# build the engine once
cargo build --release --locked

# run one persona from a tenant
export BSKY_APP_PASSWORD='...'          # from your secret store
export GEMINI_API_KEY='...'
target/release/autopilot run \
  --persona /srv/autopilot/tenants/acme/personas/tech-curiosities.yaml \
  --config  /srv/autopilot/tenants/acme/config/providers.yaml \
  --dry-run
```

The scheduled wrapper (`MNEMOCINE-OPS`) sets `AUTOPILOT_TENANT_DIR` and `cd`s
into the tenant, so `var/` and the relative defaults are tenant-scoped. The
engine stays in `AUTOPILOT_DIR`.

## Security

- No secret in the repository: `.env`, `var/`, `secrets/` and `*.pem` are
  ignored.
- The pack is shareable: it names variables, not values.
- The guardrail is deterministic and runs before every publish.
- A draft (`active: false`) is validated but never acts.
- The app password is an **app password**, never the account password; revoke it
  in the platform if it leaks.

## Migrating an existing deployment

1. Create the tenant directory with `personas/`, `config/` and `var/`.
2. Move the packs, the engage config and `var/` (use `rsync -aHAX` and compare
   checksums; `var/` holds the caps and the journals).
3. Set `AUTOPILOT_TENANT_DIR` and the `AUTOPILOT_SECRETS` list in the unit.
4. Dry run, then a real run in the next window.
5. Rollback: point `AUTOPILOT_TENANT_DIR` back to the old layout; the old `var/`
   stays frozen until the first green run.

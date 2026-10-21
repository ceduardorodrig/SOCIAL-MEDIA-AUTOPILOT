# Example tenant

This directory is a **template tenant**. Copy it, edit the pack and the configs,
and run the engine against it — the engine itself stays in the repository.

```bash
cp -r examples /srv/autopilot/tenants/acme
export BSKY_APP_PASSWORD='...'
target/release/autopilot run \
  --persona /srv/autopilot/tenants/acme/personas/tech-curiosities.yaml \
  --config  /srv/autopilot/tenants/acme/config/providers.yaml \
  --dry-run
```

The example pack is a **draft** (`active: false`) and uses
`example.bsky.social`: it never posts until you set `active: true`, a real
handle and the environment variables the pack names.

The layout and the credential contract are documented in
[`../docs/tenancy.md`](../docs/tenancy.md).

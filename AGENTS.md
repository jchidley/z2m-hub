# Before starting work

- Run `lat search` for the user's intent before reading code or docs.
- Read the relevant sections with `lat section`.
- Use `lat expand` if the prompt contains `[[refs]]`.

# Post-task checklist

- Update `lat.md/` if behaviour, architecture, constraints, or tests changed.
- Run `lat check` before finishing.

## What This Repo Is

`z2m-hub` is a Rust service that replaces Home Assistant for a LAN-only Zigbee + heat-pump setup.

## Read next

Use these `lat.md/` files as the canonical source of truth for current behaviour:

- `lat.md/lat.md` — graph entrypoint
- `lat.md/architecture.md` — runtime structure and shared-state model
- `lat.md/automations.md` — motion-light rules and manual override behaviour
- `lat.md/dhw.md` — DHW tracking model and invariants
- `lat.md/interfaces.md` — HTTP, Z2M, eBUS, PostgreSQL, heating proxy contracts
- `lat.md/infrastructure.md` — hosts, config ownership, deployment shape
- `lat.md/constraints.md` — hard operational boundaries

Use `README.md` for human-facing signposting.

## Commands

- Build check: `cargo check`
- Build for Pi: `cargo build --release --target aarch64-unknown-linux-gnu`
- Deploy: `scp target/aarch64-unknown-linux-gnu/release/z2m-hub jack@pi5data:/tmp/z2m-hub && ssh jack@pi5data 'sudo mv /tmp/z2m-hub /usr/local/bin/z2m-hub && sudo systemctl restart z2m-hub'`
- Logs: `ssh jack@pi5data 'sudo journalctl -u z2m-hub -f'`
- Status: `ssh jack@pi5data 'sudo systemctl status z2m-hub'`

## Key boundaries

- Do not modify Zigbee2MQTT config directly; use its API.
- Do not use `tokio::process` for runtime integrations.
- Keep the dashboard usable on a small phone over LAN.
- Cross-compile for `aarch64-unknown-linux-gnu` when building for `pi5data`.
- Treat `lat.md/` as canonical for current architecture and operating rules.

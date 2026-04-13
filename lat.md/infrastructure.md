# Infrastructure

This file records the deployment topology, config ownership, and live host relationships around z2m-hub.

## Deployment and configuration

z2m-hub runs on the LAN as a Raspberry Pi service and depends on neighbouring hosts for Zigbee, telemetry, and heating data.

## Hosts and roles

The deployed system spans a few fixed LAN nodes with clear ownership boundaries.

- `pi5data` (`10.0.1.230`) runs z2m-hub (systemd service), plus Docker containers: TimescaleDB (`timescale/timescaledb:latest-pg17`), Grafana, Telegraf, Mosquitto, and ebusd
- `emonpi` (`10.0.1.117`) runs Zigbee2MQTT and the Zigbee coordinator
- `emondhw` (`10.0.1.46`) publishes Multical DHW heat-meter data
- the router provides local DNS and DHCP under the `chidley.home` domain

The desired steady state is PostgreSQL-first. If an InfluxDB v2 container still exists on `pi5data`, treat it as a temporary migration artefact rather than part of the target architecture; its retirement is tracked in [[tsdb-migration]].

The dashboard is intended to be reached as `http://10.0.1.230:3030` because some Android clients do not append the LAN search domain reliably.

## Config ownership

Runtime configuration is intentionally small and split between code constants and one TOML file.

`/etc/z2m-hub.toml` is loaded at startup with built-in defaults as fallback. That file carries DHW model parameters and a `[database]` section for the PostgreSQL connection (host, port, dbname, user). Network endpoints, HTTP port, light lists, and motion thresholds remain code constants.

## Secret management

Secrets follow device class and trust boundary.

Pi/Linux services hold stronger runtime secrets and must use systemd encrypted credentials via `systemd-creds encrypt` + `LoadCredentialEncrypted=`. Do not store secrets in TOML, pass them on command lines, check them into the repo, or use `LoadCredential=` from plaintext files.

Dev/test may use one-shot `ak`-sourced environment injection on the trusted machine only, e.g. `PGPASSWORD=$(ak get timescaledb) cargo run`. This is a local operator convenience for verification, not a production secret-distribution mechanism.

MCUs should prefer a gateway pattern via MQTT or a Pi-owned API and should not hold database or cloud secrets unless unavoidable. Any device that must access PostgreSQL, MQTT, or another backend directly gets its own least-privilege credential. Assume MCU secrets may be extractable, so use per-device rotation and revocation.

Many field devices already publish to Pi-side services over MQTT, so stronger secrets should stay on the Pi side.

For z2m-hub specifically, the PostgreSQL password is the only runtime secret the service needs.

Password resolution order in [[src/main.rs#DatabaseConfig]]:

1. **systemd credential** — `$CREDENTIALS_DIRECTORY/pgpassword`, decrypted at runtime by systemd from the encrypted blob at `/etc/z2m-hub/pgpassword.encrypted`. This is the production path.
2. **`PGPASSWORD` env var** — for trusted dev/test-machine use: `PGPASSWORD=$(ak get timescaledb) cargo run`

No plaintext password fields exist in the config struct.

Provisioning on pi5data (one-time):

```bash
ak get timescaledb | ssh pi5data "sudo systemd-creds encrypt --name=pgpassword - /etc/z2m-hub/pgpassword.encrypted"
```

The systemd unit uses `LoadCredentialEncrypted=pgpassword:/etc/z2m-hub/pgpassword.encrypted` to decrypt it into a private tmpfs at service start. The secret never appears in env, process listings, or normal config files.

To rotate: re-run the encrypt command with the new password and restart the service.

## Device roles

Only a subset of Zigbee devices are semantically significant to this service.

Important named devices are:

- motion sensors: `landing_motion`, `hall_motion`
- motion-linked lights: `landing`, `hall`
- dashboard-only light: `top_landing`
- tracked-but-not-dashboarded examples: room temperature sensors plus `washing_machine`, `plusnet_router`, and `pi4_router` smart plugs

The service can cache any retained Zigbee topic, but only the named lights and motion sensors currently drive behaviour.

## Build and deploy path

The project is cross-compiled on x86_64 and deployed as a single ARM64 binary.

Build with `cargo build --release --target aarch64-unknown-linux-gnu`. Deploy by copying `target/aarch64-unknown-linux-gnu/release/z2m-hub` to `pi5data`, moving it to `/usr/local/bin/z2m-hub`, and restarting the `z2m-hub` systemd service.

## Reference submodule

The Zigbee2MQTT source tree is vendored as a reference, not as a build dependency.

`vendor/zigbee2mqtt` is pinned to tag `2.9.1` and exists so protocol details can be checked locally when the WebSocket API or bridge behaviour needs to be confirmed.

# Infrastructure

This file records the deployment topology, config ownership, and live host relationships around z2m-hub.

## Deployment and configuration

z2m-hub runs on the LAN as a Raspberry Pi service and depends on neighbouring hosts for Zigbee, telemetry, and heating data.

## Hosts and roles

The deployed system spans a few fixed LAN nodes with clear ownership boundaries.

- `pi5data` (`10.0.1.230`) runs z2m-hub, InfluxDB, Grafana, Telegraf, Mosquitto, and ebusd
- `emonpi` (`10.0.1.117`) runs Zigbee2MQTT and the Zigbee coordinator
- `emondhw` (`10.0.1.46`) publishes Multical DHW heat-meter data
- the router provides local DNS and DHCP under the `chidley.home` domain

The dashboard is intended to be reached as `http://10.0.1.230:3030` because some Android clients do not append the LAN search domain reliably.

## Config ownership

Runtime configuration is intentionally small and split between code constants and one TOML file.

`/etc/z2m-hub.toml` is loaded at startup with built-in defaults as fallback. That file currently only carries DHW model parameters. Network endpoints, HTTP port, InfluxDB token, light lists, and motion thresholds remain code constants.

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

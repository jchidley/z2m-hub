# AGENTS.md

## What This Is

Rust server that acts as a Zigbee2MQTT automation hub and SPA server. Replaces Home Assistant for a Zigbee-only setup.

Runs on pi5data (10.0.1.230), connects to Z2M on emonpi (10.0.1.117) via WebSocket.

## Architecture

```
Browser ←──HTTP──→ Rust server (pi5data:3000) ←──WS──→ Z2M (emonpi:8080/api)
         ←──WS──→       │
                         ├── automation engine (rules in TOML)
                         ├── static file server (SPA)
                         └── device state cache
```

### Communication Paths

- **Z2M WebSocket** (`ws://emonpi:8080/api`) — primary API. Z2M pushes all device state on connect, commands sent back as `{topic, payload}` JSON. No auth required.
- **Z2M MQTT** (`emonpi:1883`, user `emonpi`, pass `emonpimqtt2016`) — also available. Mosquitto on emonpi is open to the network with password auth. Bridge to pi5data is bidirectional for `zigbee2mqtt/#`.
- **SPA WebSocket** — server pushes device state updates to connected browsers, receives commands.

### Z2M WebSocket Protocol

Messages are JSON: `{"topic": "<topic>", "payload": <object>}`

**On connect**, Z2M pushes retained state:
- `bridge/state` — `{"state": "online"}`
- `bridge/info` — version, config, coordinator info
- `bridge/devices` — full device list with definitions, endpoints, exposes
- `bridge/groups`, `bridge/extensions`, `bridge/converters`, `bridge/definitions`
- `<device_name>` — cached state for each device (including sleeping battery devices)

**Sending commands** (same format):
- `{"topic": "<device>/set", "payload": {"state": "ON"}}` — control devices
- `{"topic": "bridge/request/<action>", "payload": {...}}` — bridge API

**Receiving updates**:
- `{"topic": "<device>", "payload": {...}}` — device state changes
- `{"topic": "bridge/logging", "payload": {"level": "info", "message": "..."}}` — logs
- `{"topic": "bridge/event", "payload": {"type": "device_joined", ...}}` — events
- `{"topic": "bridge/response/<action>", "payload": {"status": "ok", "data": {...}}}` — request responses

### Bridge Request/Response API

Send to `bridge/request/<action>`, response on `bridge/response/<action>`.
Response always has `status` ("ok"|"error") and `data`. Optional `transaction` property for matching.

Key actions:
- `permit_join` — `{"time": 254}` (open) / `{"time": 0}` (close)
- `device/remove` — `{"id": "name", "force": false}`
- `device/rename` — `{"from": "old", "to": "new"}`
- `device/options` — `{"id": "name", "options": {...}}`
- `device/configure` — `{"id": "name"}` (re-configure)
- `restart` — restart Z2M
- `health_check` — `{"healthy": true}`
- `networkmap` — `{"type": "raw", "routes": false}`
- `options` — change Z2M config (e.g. `{"options": {"advanced": {"last_seen": "epoch"}}}`)

## Current Zigbee Devices

| Device | Model | Type | Status |
|--------|-------|------|--------|
| landing | ZBMINI (SONOFF) | Router/Switch | ✅ Active |
| hall | ZBMINI (SONOFF) | Router/Switch | ✅ Active |
| landing_motion | RTCGQ14LM (Aqara) | Motion sensor | ✅ Active |
| kitchen | ZBMINI (SONOFF) | Router/Switch | ❌ Dead since Nov 2024 |
| bathroom_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ❌ Dead since Nov 2024 |
| front_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ❌ Dead since Nov 2024 |
| conservatory_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ❌ Dead since Nov 2024 |
| shower_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ❌ Dead since Nov 2024 |

Dead devices need re-pairing after March 2026 emonpi rebuild.

## Automations

Currently one automation running as a shell script (`z2m-automations.service` on pi5data):
- **landing_motion → landing**: motion detected → light ON, off after 60s, timer resets on re-trigger

This will be replaced by the Rust server's automation engine.

## Commands

| Task | Command |
|------|---------|
| Build | `cargo build` |
| Run | `cargo run` |
| Build for pi5data | `cross build --release --target aarch64-unknown-linux-gnu` |
| Deploy | `scp target/aarch64-unknown-linux-gnu/release/z2m-hub jack@pi5data:/usr/local/bin/` |

## Tech Stack

- `axum` — HTTP server + WebSocket server (for SPA clients)
- `tokio-tungstenite` — WebSocket client (to Z2M)
- `tower-http` — static file serving
- `serde`/`serde_json` — JSON serialization
- `toml` — config/rules

## Related Infrastructure

See `~/projects/heatpump-analysis/AGENTS.md` for full monitoring network details.

Key points:
- **emonpi** (10.0.1.117) — EmonPi2, Z2M in Docker (Sonoff USB 3.0 dongle), Mosquitto (open on 0.0.0.0:1883 with auth)
- **pi5data** (10.0.1.230) — central hub, Docker (Mosquitto, InfluxDB, Telegraf, Grafana, ebusd), systemd services
- All hostnames resolve via local DNS (dnsmasq on router 10.0.0.1)

## Boundaries

- Don't modify Z2M config directly — use the bridge request API
- Don't store MQTT credentials in source — use config file or environment
- Cross-compile for aarch64 (pi5data is ARM64)
- SPA should work on any modern browser on the LAN, no internet required

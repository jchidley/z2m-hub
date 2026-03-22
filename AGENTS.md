# AGENTS.md

## What This Is

Rust server that acts as a Zigbee2MQTT automation hub, eBUS heat pump controller, and mobile-friendly dashboard. Replaces Home Assistant for a Zigbee + heat pump setup.

Runs on pi5data (10.0.1.230:3030), connects to Z2M on emonpi (10.0.1.117) via WebSocket.

## Architecture

```
Browser ←──HTTP──→ Rust server (pi5data:3030) ←──WS──→ Z2M (emonpi:8080/api)
                         │
                         ├── motion → light automations
                         ├── DHW tracking (remaining litres)
                         ├── eBUS heat pump control (TCP :8888)
                         ├── InfluxDB queries + writes (HTTP :8086)
                         └── device state cache (from Z2M WebSocket)
```

### Communication Paths

- **Z2M WebSocket** (`ws://emonpi:8080/api`) — primary API for Zigbee devices. Z2M pushes all device state on connect, commands sent back as `{topic, payload}` JSON. No auth required.
- **eBUS TCP** (`localhost:8888`) — direct TCP to ebusd Docker container for heat pump control. Send commands like `write -c 700 HwcSFMode load`, receive `done`.
- **InfluxDB HTTP** (`localhost:8086`) — Flux queries for sensor data (DHW T1, volume register, remaining litres). Writes remaining_litres back for Grafana/history.
- **Z2M MQTT** (`emonpi:1883`, user `emonpi`, pass `emonpimqtt2016`) — not used by z2m-hub directly. Used by Telegraf for logging all device data to InfluxDB.

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

### eBUS Commands (via TCP to ebusd)

```
read -f -c 700 HwcSFMode          → "auto" / "load"
write -c 700 HwcSFMode load       → "done" (trigger DHW boost)
read -f -c 700 HwcTempDesired     → "45"
read -f -c 700 HwcStorageTemp     → "37.5"
read -f -c hmu Status01           → "43.0;39.5;-;-;-;hwc"
  (flow;return;outside;dhw;storage;pumpstate — pumpstate: off/on/overrun/hwc)
```

## Current Zigbee Devices

| Device | Model | Type | Status |
|--------|-------|------|--------|
| landing | ZBMINI (SONOFF) | Router/Switch | ✅ Active (debounce 0.5s) |
| hall | ZBMINI (SONOFF) | Router/Switch | ✅ Active (debounce 0.5s) |
| kitchen | ZBMINI (SONOFF) | Router/Switch | ✅ Active |
| landing_motion | RTCGQ14LM (Aqara) | Motion sensor | ✅ Active (62-77% batt) |
| hall_motion | RTCGQ14LM (Aqara) | Motion sensor | ✅ Active (100% batt) |
| bathroom_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ✅ Re-paired Mar 2026 |
| conservatory_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ✅ Active |
| shower_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ✅ Active |
| front_temp_humid | SNZB-02P (SONOFF) | Temp/humidity | ❌ Dead battery |

## Automations

All automations run in the Rust server (z2m-hub.service on pi5data). Previous shell scripts removed.

### Motion → Lights
- **Sensors**: landing_motion, hall_motion
- **Lights**: landing, hall (both triggered by either sensor)
- **Illuminance thresholds**: landing_motion ≤ 15 lx, hall_motion ≤ 15 lx
- **Behaviour**: motion ON → lights ON, 60s auto-off timer, re-trigger resets timer
- **Light-aware**: illuminance only sampled when lights are off (avoids self-inflation from switched-on lights boosting sensor readings by ~5-6 lx)

### DHW Tracking
- Polls ebusd every 10s for charge status (HwcSFMode + Status01 pumpstate)
- **Scheduled charge completes** → reset to 161L (full tank)
- **Manual boost completes** → +50% (80.5L), capped at 161L
- **Water usage** → tracked via Multical volume register (emon/multical/dhw_volume_V1)
- Writes `remaining_litres` to InfluxDB measurement `dhw` for Grafana
- Previous InfluxDB Flux task ("DHW Remaining Litres") disabled — had null crash edge case

## HTTP API (port 3030)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/` | GET | Mobile dashboard (hot water gauge, boost button, light toggles) |
| `/api/hot-water` | GET | `{remaining_litres, ok}` |
| `/api/dhw/boost` | POST | Trigger DHW boost (HwcSFMode=load) |
| `/api/dhw/status` | GET | `{charging, sfmode, t1, return_temp, target_temp, ok}` |
| `/api/lights` | GET | `{lights: {name: {on: bool}}, ok}` |
| `/api/lights/{name}/toggle` | POST | Toggle light, returns new state |
| `/api/lights/{name}/on` | POST | Turn light on |
| `/api/lights/{name}/off` | POST | Turn light off |

### Mobile Dashboard
- Optimised for iPhone SE (320px) portrait
- Hot water: red tank gauge, litres remaining, status (Empty/Low/OK/Full)
- DHW boost: one-tap button, shows "Boosting…" with pulsing animation while active, return temp while charging, T1 when idle
- Lights: toggle switches with live state from Z2M (polls every 5s)
- Access via `http://10.0.1.230:3030` (use IP, not hostname — Android ignores DHCP search domains)

## Commands

| Task | Command |
|------|---------|
| Build (check) | `cargo check` |
| Build for pi5data | `cargo build --release --target aarch64-unknown-linux-gnu` |
| Deploy | `scp target/aarch64-unknown-linux-gnu/release/z2m-hub jack@pi5data:/tmp/z2m-hub && ssh jack@pi5data 'sudo mv /tmp/z2m-hub /usr/local/bin/z2m-hub && sudo systemctl restart z2m-hub'` |
| Logs | `ssh jack@pi5data 'sudo journalctl -u z2m-hub -f'` |
| Status | `ssh jack@pi5data 'sudo systemctl status z2m-hub'` |

### Cross-compilation Setup
- Target: `aarch64-unknown-linux-gnu` (pi5data is ARM64, Debian 12, glibc 2.36)
- Linker: `aarch64-linux-gnu-gcc` (configured in `.cargo/config.toml`)
- **No TLS needed** — all connections are LAN-only (ws://, http://)
- **No `tokio::process`** — causes GLIBC_2.39 dependency (pidfd). Use TCP/HTTP instead of shelling out.
- `cross` tool installed but not used (requires Docker). Native cross-compile works.

## Tech Stack

- `axum` — HTTP server (API + embedded HTML dashboard)
- `tokio-tungstenite` — WebSocket client (to Z2M)
- `reqwest` (rustls) — HTTP client (InfluxDB queries/writes)
- `serde`/`serde_json` — JSON serialization
- `tokio` — async runtime, TCP (ebusd), timers

## Submodules

- `vendor/zigbee2mqtt` — pinned to tag 2.9.1 (reference for Z2M protocol/API)

## Related Infrastructure

See `~/projects/heatpump-analysis/AGENTS.md` for full monitoring network details.

Key points:
- **emonpi** (10.0.1.117) — EmonPi2, Z2M 2.9.1 in Docker (Sonoff ZBDongle-P USB coordinator), Mosquitto (0.0.0.0:1883 with auth)
- **pi5data** (10.0.1.230) — central hub, Docker (Mosquitto, InfluxDB, Telegraf, Grafana, ebusd), z2m-hub systemd service
- **emondhw** (10.0.1.46) — Multical DHW heat meter, publishes to `emon/multical/#` via MQTT
- **Router** (10.0.0.1) — Alpine Linux, Unbound (port 53) → dnsmasq (port 35353), DHCP with static reservations
- Hostnames resolve via `chidley.home` domain (dnsmasq expand-hosts). Android devices may not append search domain — use IP addresses.

## Data Flow

```
Zigbee devices → Z2M (emonpi) → WebSocket → z2m-hub (automations + API)
                                    ↓
                              MQTT publish
                                    ↓
                              Mosquitto (pi5data, bridged)
                                    ↓
                              Telegraf → InfluxDB ← z2m-hub (DHW writes)
                                                        ↓
                                                    Grafana / Mobile dashboard
```

## Boundaries

- Don't modify Z2M config directly — use the bridge request API
- Don't use `tokio::process::Command` — causes glibc version mismatch on pi5data
- Don't use TLS/native-tls — unnecessary on LAN, adds OpenSSL cross-compile pain
- Cross-compile for aarch64 (pi5data is ARM64, glibc 2.36)
- Dashboard must work on iPhone SE (320px) over LAN, no internet required
- InfluxDB token is hardcoded (LAN-only, not exposed to internet)

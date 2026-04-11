# Repository Overview & Map

<!-- code-truth: d33cd13 -->

## Purpose

Single-binary Rust server replacing Home Assistant for a LAN-only Zigbee + heat pump setup. Three responsibilities:

1. **Zigbee automations** — motion sensors → lights, with illuminance gating and manual override
2. **Heat pump DHW control** — boost button + remaining hot water tracking
3. **Mobile dashboard** — embedded HTML served on port 3030

## Technologies

- Rust (2021 edition), async via Tokio
- axum (HTTP server), tokio-tungstenite (Z2M WebSocket client), reqwest/rustls (InfluxDB HTTP)
- Cross-compiled to `aarch64-unknown-linux-gnu` for Raspberry Pi 5 (pi5data)
- No MQTT dependency — talks Z2M WebSocket and ebusd TCP directly

## File Organisation

```
src/main.rs           ← entire application (~1520 lines, single file)
Cargo.toml            ← dependencies
.cargo/config.toml    ← aarch64 cross-compile linker config
vendor/zigbee2mqtt/   ← submodule pinned to 2.9.1 (reference only)
AGENTS.md             ← LLM context (comprehensive, kept current)
docs/code-truth/      ← code-derived documentation (this directory)
```

### Navigating main.rs

The file is organised in labelled sections:

| Section | What it contains | Change X → look here |
|---------|-----------------|---------------------|
| Top: structs + constants | `Z2mMessage`, `AutomationState`, `DhwState`, `AppState`, all `const` | Add a device, change thresholds, add config |
| `main()` | Wires everything up: state init, axum router, `tokio::select!` for concurrent loops | Add a new background loop or HTTP route |
| HTTP handlers (`api_*`) | `api_hot_water`, `api_dhw_boost`, `api_dhw_status`, `api_light_*`, `api_lights_state` | Add/modify API endpoints |
| `query_influxdb()` | Flux query → CSV parse → (f64, String) | Change what's read from InfluxDB |
| `write_dhw_to_influxdb()` | Line protocol write | Change what's written to InfluxDB |
| `ebusd_command()` | TCP connect → send command → read response | Change heat pump communication |
| `HOME_PAGE` const | Entire HTML/CSS/JS dashboard as a string literal | Change the UI |
| `dhw_tracking_loop()` | Polls ebusd every 10s, detects charge transitions, tracks volume | Change DHW logic |
| `timer_loop()` | 1s tick, checks light off timers | Change light timing |
| `z2m_connection_loop()` | WebSocket connect/reconnect, message dispatch | Change Z2M connection handling |
| `handle_z2m_message()` | Routes Z2M messages to automation logic + manual override detection | Add new automations, handle new device types |

### Where things are configured

| Setting | Location | Current value |
|---------|----------|---------------|
| Z2M URL | `const Z2M_WS_URL` | `ws://emonpi:8080/api` |
| HTTP port | `const HTTP_PORT` | `3030` |
| All toggleable lights | `const LIGHTS` | `["landing", "hall", "top_landing"]` |
| Motion-triggered lights | `const MOTION_LIGHTS` | `["landing", "hall"]` |
| Motion sensors + thresholds | `const MOTION_SENSORS` | `[("landing_motion", 15.0), ("hall_motion", 15.0)]` |
| Light off delay | `const OFF_DELAY` | `300s` (5 minutes) |
| DHW config | `HubConfig` loaded from `/etc/z2m-hub.toml` | `full_litres=177`, decay, thresholds (see `z2m-hub.toml`) |
| InfluxDB URL/token/org | `const INFLUXDB_*` | Hardcoded (LAN-only) |
| ebusd host/port | `const EBUSD_HOST/PORT` | `localhost:8888` |
| Heating MVP proxy | `const HEATING_MVP_URL` | `http://127.0.0.1:3031` |

### Key distinction: LIGHTS vs MOTION_LIGHTS

`LIGHTS` = all lights with dashboard toggles (landing, hall, top_landing).
`MOTION_LIGHTS` = subset triggered by motion sensors (landing, hall only).

top_landing has a dashboard toggle but is not linked to any motion sensor. To add a new light to the dashboard only, add to `LIGHTS`. To also link it to motion, add to `MOTION_LIGHTS`.

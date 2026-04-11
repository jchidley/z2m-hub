# Repository Overview & Map

Code-derived navigation aid. For canonical architecture and domain rules, see [`lat.md/`](../../lat.md/lat.md).

## File Organisation

```
src/main.rs           ← entire application (single-file service)
Cargo.toml            ← dependencies
.cargo/config.toml    ← aarch64 cross-compile linker config
vendor/zigbee2mqtt/   ← submodule pinned to 2.9.1 (reference only)
lat.md/               ← canonical current-state knowledge graph
AGENTS.md             ← agent workflow and repo execution rules
README.md             ← human-facing signposting and build/deploy basics
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

See [`lat.md/infrastructure.md`](../../lat.md/infrastructure.md) for config ownership and deployment shape. Quick reference for code constants:

| Setting | Location |
|---------|----------|
| Z2M URL | `const Z2M_WS_URL` |
| HTTP port | `const HTTP_PORT` |
| All toggleable lights | `const LIGHTS` |
| Motion-triggered lights | `const MOTION_LIGHTS` |
| Motion sensors + thresholds | `const MOTION_SENSORS` |
| Light off delay | `const OFF_DELAY` |
| DHW config | `HubConfig` loaded from `/etc/z2m-hub.toml` |
| InfluxDB URL/token/org | `const INFLUXDB_*` |
| ebusd host/port | `const EBUSD_HOST/PORT` |
| Heating MVP proxy | `const HEATING_MVP_URL` |

For the LIGHTS vs MOTION_LIGHTS distinction, see [`lat.md/automations.md`](../../lat.md/automations.md).

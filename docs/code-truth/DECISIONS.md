# Decisions

<!-- code-truth: 8313d95 -->

## Structural (don't touch without good reason)

### No MQTT dependency
z2m-hub connects to Z2M via WebSocket and ebusd via TCP. It never touches MQTT. This is deliberate — MQTT is used by Telegraf for data logging (a separate concern). Adding MQTT to z2m-hub would create a circular dependency and an unnecessary broker hop for real-time control.

### No TLS
All connections are LAN-only (`ws://`, `http://`, raw TCP). Using `reqwest` with `rustls` (not `native-tls`) avoids OpenSSL cross-compilation pain. The `tokio-tungstenite` dependency has no TLS features enabled.

### No `tokio::process`
Shelling out to `docker exec` for ebusd commands caused a GLIBC_2.39 dependency (`pidfd_spawnp`), which pi5data (Debian 12, glibc 2.36) doesn't have. Replaced with direct TCP to ebusd port 8888. This constraint applies to any future feature — don't spawn subprocesses.

### Single-file architecture
Everything is in `src/main.rs`. For a ~850 line server with clear section labels, this is simpler than module splitting. Reconsider if the file exceeds ~1500 lines or if independent subsystems emerge.

### Embedded HTML dashboard
The entire mobile UI is a `const` string literal in main.rs (`HOME_PAGE`). No separate static files, no build step, no SPA framework. This keeps deployment as a single binary. The tradeoff is that UI changes require recompilation.

### InfluxDB token hardcoded
The token is in source code. Acceptable because: LAN-only, not exposed to internet, InfluxDB is only accessible from pi5data's Docker network + localhost. If this ever gets internet-exposed, move to environment variables.

## Pragmatic (could change)

### Illuminance frozen while lights on
When the motion automation turns on lights, it stops updating the illuminance cache for that sensor. This prevents the lights themselves from inflating the lux reading (measured +5-6 lx bump). The cache unfreezes when lights turn off. This works but means if ambient light changes while lights are on (e.g., sunrise), the cached value is stale. Acceptable because the lights auto-off after 60s anyway.

### DHW boost = +50%
Manual boost adds 50% of tank capacity (80.5L) capped at 161L. This is an approximation — a boost from empty won't fill the tank completely (heat pump may not run long enough), and a boost from nearly full will be capped. The 50% figure can be tuned via `DHW_BOOST_PERCENT`.

### DHW tracking via volume register
Usage is tracked by Multical `dhw_volume_V1` — a cumulative register in 10L steps. This is coarse but reliable. The old Flux task attempted sub-register interpolation using flow integration, which was fragile (null crashes when no flow). z2m-hub just uses the register directly. Accuracy is ±10L, which is fine for "how much hot water is left" on a phone.

### Optimistic light toggle
The toggle API returns the intended new state immediately, before Z2M confirms the device actually switched. The UI updates instantly from this response, then background polling (5s) catches the real state. This gives responsive feel but means a failed toggle won't be noticed for up to 5 seconds.

### Device state lost on restart
`z2m_state` (device cache), `AutomationState` (light timers), and `DhwState` (remaining litres, volume baseline) are all in-memory. On restart:
- Z2M repushes all device state on reconnect → `z2m_state` recovers immediately
- Light timers reset → no issue, lights just turn off after 60s naturally  
- DHW remaining initialises from last InfluxDB value → minor accuracy gap possible
- `boost_initiated` flag resets to false → if server restarts mid-boost, it'll be treated as a scheduled charge (reset to full instead of +50%)

## Open Questions

- **Should motion sensors have independent off-timers per sensor?** Currently both sensors share one timer. If landing_motion triggers, then hall_motion retriggers 30s later, the timer resets to 60s from the hall event. This seems fine but could be surprising.
- **Should the dashboard show more device info?** Temperature sensors, kitchen switch, etc. are tracked by Z2M and logged by Telegraf but not exposed in the dashboard.
- **Should z2m-hub subscribe to MQTT for emon data instead of querying InfluxDB?** Currently reads T1 and volume from InfluxDB (via Flux query, CSV parse). Subscribing to `emon/multical/dhw_t1` via MQTT would give real-time data without the query overhead. But adds MQTT as a dependency.

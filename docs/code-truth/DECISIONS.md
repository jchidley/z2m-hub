# Decisions

<!-- code-truth: 3c35351 -->

## Structural (don't touch without good reason)

### No MQTT dependency
z2m-hub connects to Z2M via WebSocket and ebusd via TCP. It never touches MQTT. This is deliberate — MQTT is used by Telegraf for data logging (a separate concern). Adding MQTT would create a circular dependency and an unnecessary broker hop for real-time control.

### No TLS
All connections are LAN-only (`ws://`, `http://`, raw TCP). Using `reqwest` with `rustls` (not `native-tls`) avoids OpenSSL cross-compilation pain. `tokio-tungstenite` has no TLS features enabled.

### No `tokio::process`
Shelling out to `docker exec` caused a GLIBC_2.39 dependency (`pidfd_spawnp`), which pi5data (Debian 12, glibc 2.36) doesn't have. Replaced with direct TCP to ebusd port 8888. This constraint applies to any future feature — don't spawn subprocesses.

### Single-file architecture
Everything is in `src/main.rs`. For a ~870 line server with clear section labels, this is simpler than module splitting. Reconsider if the file exceeds ~1500 lines or if independent subsystems emerge.

### Embedded HTML dashboard
The entire mobile UI is a `const` string literal (`HOME_PAGE`). No separate static files, no build step, no SPA framework. Keeps deployment as a single binary. Tradeoff: UI changes require recompilation.

### InfluxDB token hardcoded
Acceptable because: LAN-only, not exposed to internet, InfluxDB only accessible from pi5data's Docker network + localhost. If this ever gets internet-exposed, move to environment variables.

### LIGHTS vs MOTION_LIGHTS split
`LIGHTS` lists all lights with dashboard toggles. `MOTION_LIGHTS` is the subset triggered by motion sensors. This separation lets `top_landing` appear on the dashboard without being linked to motion. Adding a light to `LIGHTS` only gives it a toggle; adding to `MOTION_LIGHTS` also links it to the motion automation.

## Pragmatic (could change)

### Illuminance frozen while lights on
When motion turns on lights, illuminance cache stops updating for that sensor. Prevents the lights inflating the lux reading (+5-6 lx measured). Cache unfreezes when lights turn off. Means ambient light changes during the 5min window are missed — acceptable because the timer handles it.

### 5-minute motion timeout
`OFF_DELAY = 300s`. Long enough that you don't get plunged into darkness while on the stairs, short enough to not waste electricity. Re-trigger resets the full 5 minutes.

### Manual off cancels automation
If a MOTION_LIGHT is switched OFF (physical switch or dashboard) while the motion timer is active, the timer is cancelled. The automation won't fight the user. This is detected by watching Z2M state updates for MOTION_LIGHTS going to OFF while `lights_off_at` is set.

### DHW boost = +50%
Manual boost adds 50% of tank capacity (80.5L) capped at 161L. Approximation — a boost from empty won't fill completely, boost from nearly full gets capped. Tunable via `DHW_BOOST_PERCENT`.

### DHW tracking via volume register
Usage tracked by Multical `dhw_volume_V1` — cumulative register in 10L steps. Coarse but reliable. The old InfluxDB Flux task attempted sub-register interpolation using flow integration, which was fragile (null crashes). z2m-hub just uses the register directly. Accuracy ±10L.

### Optimistic light toggle
Toggle API returns intended new state immediately, before Z2M confirms. UI updates instantly, background poll (5s) catches real state. Failed toggle not noticed for up to 5 seconds.

### Device state lost on restart
All in-memory state resets on restart:
- `z2m_state` recovers immediately (Z2M repushes on reconnect)
- Light timers reset (lights just turn off naturally)
- DHW remaining initialises from last InfluxDB value (minor gap possible)
- `boost_initiated` resets to false (mid-boost restart → treated as scheduled charge = 161L instead of +50%)

## Open Questions

- **Per-sensor off-timers?** Currently both motion sensors share one timer. If landing_motion triggers, then hall_motion retriggers 30s later, the timer resets to 5min from the hall event. Seems fine but could be surprising.
- **Dashboard for more devices?** Temperature sensors, kitchen switch, smart plugs are tracked by Z2M and logged by Telegraf but not exposed in the dashboard.
- **MQTT for emon data?** Currently reads T1 and volume from InfluxDB (Flux query, CSV parse). Subscribing to `emon/multical/dhw_t1` via MQTT would give real-time data without query overhead. But adds MQTT as a dependency.
- **top_landing motion link?** Currently manual-only. Could be linked to a future top_landing_motion sensor with its own illuminance threshold.

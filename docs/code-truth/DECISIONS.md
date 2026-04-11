# Decisions

<!-- code-truth: d33cd13 -->

## Structural (don't touch without good reason)

### No MQTT dependency
z2m-hub connects to Z2M via WebSocket and ebusd via TCP. It never touches MQTT. This is deliberate — MQTT is used by Telegraf for data logging (a separate concern). Adding MQTT would create a circular dependency and an unnecessary broker hop for real-time control.

### No TLS
All connections are LAN-only (`ws://`, `http://`, raw TCP). Using `reqwest` with `rustls` (not `native-tls`) avoids OpenSSL cross-compilation pain. `tokio-tungstenite` has no TLS features enabled.

### No `tokio::process`
Shelling out to `docker exec` caused a GLIBC_2.39 dependency (`pidfd_spawnp`), which pi5data (Debian 12, glibc 2.36) doesn't have. Replaced with direct TCP to ebusd port 8888. This constraint applies to any future feature — don't spawn subprocesses.

### Single-file architecture
Everything is in `src/main.rs`. For a server with clear section labels, this is simpler than module splitting. The file is currently ~1520 lines — past the original ~1500 line threshold. Module splitting may be worth considering if further features are added.

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

### Manual off cancels and suppresses automation
If a MOTION_LIGHT is switched OFF (physical switch or dashboard) while the motion timer is active, the timer is cancelled and re-triggering is suppressed for 5 minutes (`suppressed_until`). The automation won't fight the user. This is detected by watching Z2M state updates for MOTION_LIGHTS going to OFF while `lights_off_at` is set.

### DHW boost triggers a charge cycle
Manual boost sets `HwcSFMode=load` via eBUS. When the resulting charge completes, remaining litres are determined by the crossover/thermocline model (same as any scheduled charge). The old +50% heuristic was replaced by the v0.2.0 physics-based model.

### DHW tracking: physics-based model (v0.2.0)
Usage tracked by multiple signals: Multical volume register (`dhw_volume_V1`), T1 temperature, HwcStorageTemp, and dhw_flow. Crossover detection (HwcStorage reaching T1_at_charge_start) determines full vs partial charge. Draw tracking uses volume subtraction with temperature-based overrides (HwcStorage crash caps, T1 drop caps). Standby decay models T1 cooling at 0.25°C/h. Config in `z2m-hub.toml`, capacity autoloaded from InfluxDB. Replaced the earlier simple volume-register-only approach.

### Draw tracking runs during charging
The Multical tap-side meter measures actual hot water draws independently of the heat pump circuit. Draws during a charge cycle still deplete the cylinder, so draw detection is always active — not gated on `!charging`. This replaced an earlier design that ignored draws while charging.

### Optimistic light toggle
Toggle API returns intended new state immediately, before Z2M confirms. UI updates instantly, background poll (5s) catches real state. Failed toggle not noticed for up to 5 seconds.

### Device state lost on restart
All in-memory state resets on restart:
- `z2m_state` recovers immediately (Z2M repushes on reconnect)
- Light timers reset (lights just turn off naturally)
- DHW remaining initialises from last InfluxDB value (minor gap possible)
- Mid-boost restart → charge completion treated as scheduled (crossover/thermocline model applies normally)

## Open Questions

- **Per-sensor off-timers?** Currently both motion sensors share one timer. If landing_motion triggers, then hall_motion retriggers 30s later, the timer resets to 5min from the hall event. Seems fine but could be surprising.
- **Dashboard for more devices?** Temperature sensors, kitchen switch, smart plugs are tracked by Z2M and logged by Telegraf but not exposed in the dashboard.
- **MQTT for emon data?** Currently reads T1 and volume from InfluxDB (Flux query, CSV parse). Subscribing to `emon/multical/dhw_t1` via MQTT would give real-time data without query overhead. But adds MQTT as a dependency.
- **top_landing motion link?** Currently manual-only. Could be linked to a future top_landing_motion sensor with its own illuminance threshold.

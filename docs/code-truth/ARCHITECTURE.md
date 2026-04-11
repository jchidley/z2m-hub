# Architecture

<!-- code-truth: d33cd13 -->

## Runtime Structure

`main()` launches five concurrent tasks via `tokio::select!`:

```
tokio::select! {
    timer_loop           — 1s tick, checks light-off timers
    z2m_connection_loop  — WebSocket to Z2M, auto-reconnect
    dhw_tracking_loop    — 10s tick, polls ebusd, tracks DHW
    axum::serve          — HTTP server on port 3030
}
```

All tasks share state through `Arc<Mutex<T>>`:
- `AutomationState` — light timers + illuminance cache (used by timer_loop + z2m handler)
- `DhwState` — remaining litres, charging state, volume baseline (used by dhw_tracking_loop + HTTP handlers)
- `HashMap<String, Value>` — Z2M device state cache (used by z2m handler + light toggle API)
- `broadcast::Sender<Z2mMessage>` — command channel from HTTP handlers/automations → Z2M WebSocket writer

## External Dependencies (runtime)

| System | Protocol | Direction | What for |
|--------|----------|-----------|----------|
| Z2M (emonpi:8080) | WebSocket | Bidirectional | Device state + commands |
| ebusd (localhost:8888) | Raw TCP | Request/response | Heat pump control (HwcSFMode, Status01, temps) |
| InfluxDB (localhost:8086) | HTTP | Read + write | DHW T1 temp, volume register, remaining_litres |

### Implicit Contracts

- **Z2M pushes all device state on WebSocket connect.** The server depends on this to populate `z2m_state` cache. If Z2M changes this behaviour, light toggles won't know current state.
- **ebusd TCP protocol is line-based.** Send `command\n`, read until EOF. The `ebusd_command()` function calls `shutdown()` after writing to signal end-of-request. If ebusd changes to keep connections alive, this will break.
- **InfluxDB CSV response format.** `query_influxdb()` parses CSV with `_value` and `_time` columns. If InfluxDB changes column ordering or naming, parsing breaks silently (returns 0.0).
- **Multical volume register (`dhw_volume_V1`) is monotonically increasing.** DHW tracking subtracts `volume_at_reset` from current. If the register wraps or resets, remaining will go negative (clamped to 0).

## Data Flow: Motion → Lights

```
Aqara sensor (Zigbee) → Z2M → WebSocket message
  → handle_z2m_message()
    → topic in MOTION_SENSORS? yes
    → lights already on (lights_off_at set)? → just reset timer to 5min
    → illuminance ≤ threshold? (only sampled when lights off)
    → send ON to each MOTION_LIGHT via broadcast channel → Z2M → Zigbee → ZBMINI
    → set lights_off_at = now + 300s
```

### Manual Override

If a MOTION_LIGHT reports state OFF while `lights_off_at` is set (timer active), the automation cancels and suppresses re-triggering:

```
Physical switch or dashboard toggle → light OFF
  → Z2M pushes state update → handle_z2m_message()
    → topic in MOTION_LIGHTS? yes
    → state == "OFF" && lights_off_at is Some?
    → lights_off_at = None (timer cancelled)
    → suppressed_until = now + 5min (motion events ignored)
```

During suppression, motion events are silently dropped. Suppression expires after 5 minutes (`OFF_DELAY`). This prevents the automation from immediately re-triggering after a manual off.

The timer loop sets `lights_off_at = None` before sending its own OFF commands, so the Z2M confirmation arriving after a timer-triggered OFF won't re-cancel (it's already None) — and won't set `suppressed_until` either, since `lights_off_at` is already None by the time the confirmation arrives.

## Data Flow: DHW Tracking (v0.2.0 physics-based model)

```
dhw_tracking_loop (every 10s):
  → is_charging(): ebusd HwcSFMode + Status01 → sfmode=="load" || pumpstate=="hwc"
  → get_current_volume(), get_current_t1(), get_hwc_storage_temp(), get_current_dhw_flow()
  
  Charge start (charging && !was_charging):
    → snapshot t1_at_charge_start, begin crossover detection
  
  During charge:
    → if HwcStorage ≥ t1_at_charge_start → crossover_achieved = true
  
  Charge end (was_charging && !charging):
    → if crossover_achieved → remaining = full_litres (full charge)
    → else → gap-based thermocline model (gap_dissolved < 1.5°C → full,
             gap_sharp > 3.5°C → unchanged, else interpolated)
    → snapshot volume_at_reset, write to InfluxDB
  
  Draw detection (dhw_flow > draw_flow_min):
    → volume subtraction: remaining = min(remaining, full - volume_drawn)
    → HwcStorage crash (>5°C drop) → cap at vol_above_hwc (148L)
    → T1 drop >0.5°C → cap at 20L; >1.5°C → remaining = 0
  
  Standby (not charging, not drawing):
    → effective_t1 = t1_at_charge_end − 0.25°C/h elapsed
```

## Data Flow: HTTP → Light Toggle

```
POST /api/lights/landing/toggle
  → read z2m_state["landing"]["state"] → "OFF"
  → new_state = "ON"
  → send Z2mMessage via broadcast channel
  → return {"ok": true, "state": "ON"} (immediate, optimistic)
  → Z2M WebSocket writer sends to Z2M → device
  → Z2M pushes state update back → stored in z2m_state
  → next /api/lights poll (5s) picks up confirmed state
```

**Note:** Toggle response is optimistic — returns intended state before Z2M confirms. If device is unreachable, the next poll (5s) shows the old state.

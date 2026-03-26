# Architecture

<!-- code-truth: 3c35351 -->

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

If a MOTION_LIGHT reports state OFF while `lights_off_at` is set (timer active), the automation cancels:

```
Physical switch or dashboard toggle → light OFF
  → Z2M pushes state update → handle_z2m_message()
    → topic in MOTION_LIGHTS? yes
    → state == "OFF" && lights_off_at is Some?
    → lights_off_at = None (automation cancelled)
```

This works because the timer loop sets `lights_off_at = None` before sending its own OFF commands, so the Z2M confirmation arriving after a timer-triggered OFF won't re-cancel (it's already None).

## Data Flow: DHW Tracking

```
dhw_tracking_loop (every 10s):
  → ebusd_command("read -f -c 700 HwcSFMode")     → "auto" or "load"
  → ebusd_command("read -f -c hmu Status01")       → "...;hwc" or "...;off"
  → charging = sfmode=="load" || pumpstate=="hwc"
  
  If was_charging && !charging:
    → boost_initiated? → remaining += 50%, cap 161
    → else (scheduled) → remaining = 161
    → write to InfluxDB
    → snapshot volume register
  
  If !charging && volume increased:
    → remaining -= (volume_now - volume_at_reset)
    → write to InfluxDB
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

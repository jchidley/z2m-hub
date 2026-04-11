use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tokio::time::Instant;
use tracing::{error, info, warn};

/// DHW configuration loaded from z2m-hub.toml
#[derive(Debug, Clone, Deserialize)]
struct DhwConfig {
    /// Usable litres from a full charge at 45°C
    full_litres: f64,
    /// T1 decay rate in standby (°C/h)
    t1_decay_rate: f64,
    /// Below this T1, capacity starts scaling down
    reduced_t1: f64,
    /// HwcStorage crash threshold during draw (°C drop)
    hwc_crash_threshold: f64,
    /// Volume above HwcStorage sensor (litres)
    vol_above_hwc: f64,
    /// Minimum draw flow rate (L/h)
    draw_flow_min: f64,
    /// Gap threshold for sharp thermocline (°C)
    gap_sharp: f64,
    /// Gap threshold for dissolved thermocline (°C)
    gap_dissolved: f64,
    /// Minimum sane full_litres from InfluxDB autoload
    #[serde(default = "default_full_litres_min")]
    full_litres_min: f64,
    /// Maximum sane full_litres from InfluxDB autoload
    #[serde(default = "default_full_litres_max")]
    full_litres_max: f64,
}

fn default_full_litres_min() -> f64 { 160.0 }
fn default_full_litres_max() -> f64 { 220.0 }

#[derive(Debug, Clone, Deserialize)]
struct HubConfig {
    dhw: DhwConfig,
}

impl HubConfig {
    fn load(path: &str) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => {
                    info!("Loaded config from {path}");
                    config
                }
                Err(e) => {
                    error!("Failed to parse {path}: {e}, using defaults");
                    Self::default()
                }
            },
            Err(e) => {
                warn!("Config file {path} not found ({e}), using defaults");
                Self::default()
            }
        }
    }

    fn default() -> Self {
        Self {
            dhw: DhwConfig {
                full_litres: 177.0,
                t1_decay_rate: 0.25,
                reduced_t1: 42.0,
                hwc_crash_threshold: 5.0,
                vol_above_hwc: 148.0,
                draw_flow_min: 100.0,
                gap_sharp: 3.5,
                gap_dissolved: 1.5,
                full_litres_min: 160.0,
                full_litres_max: 220.0,
            },
        }
    }
}

const CONFIG_PATH: &str = "/etc/z2m-hub.toml";

/// Z2M WebSocket message format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Z2mMessage {
    topic: String,
    payload: serde_json::Value,
}

/// Shared automation state
struct AutomationState {
    /// When the lights should turn off (None = not scheduled)
    lights_off_at: Option<Instant>,
    /// When manual override expires (None = automation active)
    suppressed_until: Option<Instant>,
    /// Last known illuminance per motion sensor
    illuminance: std::collections::HashMap<String, f64>,
}

/// DHW charge/discharge state label
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DhwChargeState {
    /// Fully charged (crossover achieved, T1 ≥ 43°C)
    Full,
    /// Partially charged (no crossover or low T1)
    Partial,
    /// Standing idle, decaying
    Standby,
    /// Currently charging — below-T1 heating phase
    ChargingBelow,
    /// Currently charging — uniform heating phase (post-crossover)
    ChargingUniform,
}

impl std::fmt::Display for DhwChargeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::Partial => write!(f, "partial"),
            Self::Standby => write!(f, "standby"),
            Self::ChargingBelow => write!(f, "charging_below"),
            Self::ChargingUniform => write!(f, "charging_uniform"),
        }
    }
}

/// DHW tracking state — physics-based model per dhw-cylinder-analysis.md
struct DhwState {
    /// Runtime full capacity (config value, possibly upgraded by InfluxDB autoload)
    full_litres: f64,
    /// Current remaining usable litres
    remaining: f64,
    /// Volume register at last charge completion (for tracking usage)
    volume_at_reset: f64,

    // ── Crossover tracking ──
    /// T1 when charge started (the threshold HwcStorage must reach)
    t1_at_charge_start: f64,
    /// Whether HwcStorage crossed T1_at_charge_start during this charge
    crossover_achieved: bool,

    // ── Post-charge state ──
    /// T1 at the moment charging ended
    t1_at_charge_end: f64,
    /// HwcStorageTemp at the moment charging ended
    hwc_at_charge_end: f64,
    /// When the last charge ended (for standby decay)
    charge_end_time: Option<std::time::Instant>,
    /// Temperature-decayed effective T1 (drops 0.25°C/h in standby)
    effective_t1: f64,

    // ── Draw tracking ──
    /// HwcStorageTemp at the start of the current draw sequence
    hwc_pre_draw: f64,
    /// Whether we've detected the HwcStorage crash (>5°C drop during draw)
    hwc_crash_detected: bool,
    /// T1 at the start of the current draw sequence
    t1_pre_draw: f64,
    /// Whether a draw is currently active
    drawing: bool,

    // ── Cached sensor readings ──
    current_t1: f64,
    current_hwc: f64,

    // ── Flags ──
    was_charging: bool,
    charge_state: DhwChargeState,
}

/// Shared app state for axum handlers
#[derive(Clone)]
struct AppState {
    http_client: reqwest::Client,
    cmd_tx: broadcast::Sender<Z2mMessage>,
    z2m_state: Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    dhw_state: Arc<Mutex<DhwState>>,
    config: Arc<HubConfig>,
}

const Z2M_WS_URL: &str = "ws://emonpi:8080/api";
const LIGHTS: &[&str] = &["landing", "hall", "top_landing"];
const MOTION_LIGHTS: &[&str] = &["landing", "hall"];
const OFF_DELAY: Duration = Duration::from_secs(300);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const HTTP_PORT: u16 = 3030;

const INFLUXDB_URL: &str = "http://localhost:8086";
const INFLUXDB_TOKEN: &str =
    "jPTPrwcprKfDzt8IFr7gkn6shpBy15j8hFeyjLaBIaJ0IwcgQeXJ4LtrvVBJ5aIPYuzEfeDw5e-cmtAuvZ-Xmw==";
const INFLUXDB_ORG: &str = "home";


/// Motion sensor config: (name, illuminance threshold)
const MOTION_SENSORS: &[(&str, f64)] = &[("landing_motion", 15.0), ("hall_motion", 15.0)];

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "z2m_hub=info".into()),
        )
        .init();

    info!("z2m-hub starting");

    let automation_state = Arc::new(Mutex::new(AutomationState {
        lights_off_at: None,
        suppressed_until: None,
        illuminance: std::collections::HashMap::new(),
    }));

    // Channel for sending commands to Z2M
    let (cmd_tx, _) = broadcast::channel::<Z2mMessage>(64);

    let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::<
        String,
        serde_json::Value,
    >::new()));

    let config = Arc::new(HubConfig::load(CONFIG_PATH));
    info!("DHW config: full_litres={}", config.dhw.full_litres);

    let dhw_state = Arc::new(Mutex::new(DhwState {
        full_litres: config.dhw.full_litres,
        remaining: 0.0,
        volume_at_reset: 0.0,
        t1_at_charge_start: 0.0,
        crossover_achieved: false,
        t1_at_charge_end: 0.0,
        hwc_at_charge_end: 0.0,
        charge_end_time: None,
        effective_t1: 0.0,
        hwc_pre_draw: 0.0,
        hwc_crash_detected: false,
        t1_pre_draw: 0.0,
        drawing: false,
        current_t1: 0.0,
        current_hwc: 0.0,
        was_charging: false,
        charge_state: DhwChargeState::Standby,
    }));

    let app_state = AppState {
        http_client: reqwest::Client::new(),
        cmd_tx: cmd_tx.clone(),
        z2m_state: z2m_state.clone(),
        dhw_state: dhw_state.clone(),
        config: config.clone(),
    };

    let timer_state = automation_state.clone();
    let timer_cmd_tx = cmd_tx.clone();

    let dhw_client = app_state.http_client.clone();
    let dhw_state_loop = dhw_state.clone();

    // Build axum router
    let app = axum::Router::new()
        .route("/", get(page_home))
        .route("/api/hot-water", get(api_hot_water))
        .route("/api/lights/{name}/on", axum::routing::post(api_light_on))
        .route("/api/lights/{name}/off", axum::routing::post(api_light_off))
        .route(
            "/api/lights/{name}/toggle",
            axum::routing::post(api_light_toggle),
        )
        .route("/api/lights", get(api_lights_state))
        .route("/api/dhw/boost", axum::routing::post(api_dhw_boost))
        .route("/api/dhw/status", get(api_dhw_status))
        .route("/api/heating/status", get(api_heating_status))
        .route(
            "/api/heating/mode/{mode}",
            axum::routing::post(api_heating_mode),
        )
        .route("/api/heating/away", axum::routing::post(api_heating_away))
        .route("/api/heating/kill", axum::routing::post(api_heating_kill))
        .with_state(app_state);

    tokio::select! {
        _ = timer_loop(timer_state, timer_cmd_tx) => {
            error!("Timer loop exited unexpectedly");
        }
        _ = z2m_connection_loop(automation_state, cmd_tx, z2m_state) => {
            error!("Z2M connection loop exited unexpectedly");
        }
        _ = dhw_tracking_loop(dhw_state_loop, dhw_client, config.clone()) => {
            error!("DHW tracking loop exited unexpectedly");
        }
        result = async {
            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{HTTP_PORT}")).await.unwrap();
            info!("HTTP server listening on port {HTTP_PORT}");
            axum::serve(listener, app).await
        } => {
            error!("HTTP server exited: {result:?}");
        }
    }
}

// ── HTTP handlers ───────────────────────────────────────────────────────────

async fn api_hot_water(State(state): State<AppState>) -> Json<serde_json::Value> {
    let dhw = state.dhw_state.lock().await;
    Json(serde_json::json!({
        "remaining_litres": dhw.remaining,
        "full_litres": dhw.full_litres,
        "effective_t1": dhw.effective_t1,
        "charge_state": dhw.charge_state,
        "crossover_achieved": dhw.crossover_achieved,
        "t1": dhw.current_t1,
        "hwc_storage": dhw.current_hwc,
        "ok": true
    }))
}

async fn query_influxdb(
    client: &reqwest::Client,
    query: &str,
) -> Result<(f64, String), Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .post(format!("{INFLUXDB_URL}/api/v2/query?org={INFLUXDB_ORG}"))
        .header("Authorization", format!("Token {INFLUXDB_TOKEN}"))
        .header("Content-Type", "application/vnd.flux")
        .header("Accept", "application/csv")
        .body(query.to_string())
        .send()
        .await?;

    let body = resp.text().await?;

    // Parse CSV response — find the last row with _value
    let mut litres = 0.0;
    let mut timestamp = String::new();

    let mut headers: Vec<String> = Vec::new();
    for line in body.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if headers.is_empty() {
            headers = fields.iter().map(|s| s.to_string()).collect();
            continue;
        }
        // Find column indices
        if let (Some(val_idx), Some(time_idx)) = (
            headers.iter().position(|h| h == "_value"),
            headers.iter().position(|h| h == "_time"),
        ) {
            if let Some(val_str) = fields.get(val_idx) {
                if let Ok(val) = val_str.parse::<f64>() {
                    litres = val;
                }
            }
            if let Some(ts) = fields.get(time_idx) {
                timestamp = ts.to_string();
            }
        }
    }

    Ok((litres, timestamp))
}

async fn api_light_on(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    if !LIGHTS.contains(&name.as_str()) {
        return Json(serde_json::json!({"ok": false, "error": "unknown light"}));
    }
    let msg = Z2mMessage {
        topic: format!("{name}/set"),
        payload: serde_json::json!({"state": "ON"}),
    };
    let _ = state.cmd_tx.send(msg);
    info!("HTTP: turning ON {name}");
    Json(serde_json::json!({"ok": true, "light": name, "state": "ON"}))
}

async fn api_light_off(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    if !LIGHTS.contains(&name.as_str()) {
        return Json(serde_json::json!({"ok": false, "error": "unknown light"}));
    }
    let msg = Z2mMessage {
        topic: format!("{name}/set"),
        payload: serde_json::json!({"state": "OFF"}),
    };
    let _ = state.cmd_tx.send(msg);
    info!("HTTP: turning OFF {name}");
    Json(serde_json::json!({"ok": true, "light": name, "state": "OFF"}))
}

async fn api_light_toggle(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    if !LIGHTS.contains(&name.as_str()) {
        return Json(serde_json::json!({"ok": false, "error": "unknown light"}));
    }
    let current = {
        let zs = state.z2m_state.lock().await;
        zs.get(&name)
            .and_then(|v| v.get("state"))
            .and_then(|v| v.as_str())
            .unwrap_or("OFF")
            .to_string()
    };
    let new_state = if current == "ON" { "OFF" } else { "ON" };
    let msg = Z2mMessage {
        topic: format!("{name}/set"),
        payload: serde_json::json!({"state": new_state}),
    };
    let _ = state.cmd_tx.send(msg);
    info!("HTTP: toggling {name} {current} → {new_state}");
    Json(serde_json::json!({"ok": true, "light": name, "state": new_state}))
}

async fn api_lights_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    let zs = state.z2m_state.lock().await;
    let mut lights = serde_json::Map::new();
    for &name in LIGHTS {
        let on = zs
            .get(name)
            .and_then(|v| v.get("state"))
            .and_then(|v| v.as_str())
            == Some("ON");
        lights.insert(name.to_string(), serde_json::json!({"on": on}));
    }
    Json(serde_json::json!({"ok": true, "lights": lights}))
}

const EBUSD_HOST: &str = "localhost";
const EBUSD_PORT: u16 = 8888;

async fn ebusd_command(cmd: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect((EBUSD_HOST, EBUSD_PORT)).await?;
    stream.write_all(format!("{cmd}\n").as_bytes()).await?;
    stream.shutdown().await?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await?;
    Ok(buf.trim().to_string())
}

async fn api_dhw_boost(State(state): State<AppState>) -> Json<serde_json::Value> {
    // HwcSFMode "load" = one-shot cylinder charge, reverts to auto when done
    match ebusd_command("write -c 700 HwcSFMode load").await {
        Ok(resp) if resp == "done" => {
            info!("HTTP: DHW boost (HwcSFMode=load) requested");
            Json(serde_json::json!({"ok": true}))
        }
        Ok(resp) => {
            error!("DHW boost unexpected response: {resp}");
            Json(serde_json::json!({"ok": false, "error": resp}))
        }
        Err(e) => {
            error!("DHW boost failed: {e}");
            Json(serde_json::json!({"ok": false, "error": e.to_string()}))
        }
    }
}

async fn api_dhw_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let sfmode = ebusd_command("read -f -c 700 HwcSFMode")
        .await
        .unwrap_or_default();
    let status = ebusd_command("read -f -c hmu Status01")
        .await
        .unwrap_or_default();
    // Status01 format: "flow;return;outside;dhw;storage;pumpstate"
    // pumpstate: off/on/overrun/hwc
    let charging = status.ends_with(";hwc") || sfmode == "load";
    let parts: Vec<&str> = status.split(';').collect();
    let return_temp: f64 = parts.get(1).unwrap_or(&"0").parse().unwrap_or(0.0);
    let target_temp: f64 = ebusd_command("read -f -c 700 HwcTempDesired")
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or(0.0);

    // T1 (hot out) from emondhw Multical via InfluxDB
    let t1_query = r#"from(bucket: "energy")
  |> range(start: -1h)
  |> filter(fn: (r) => r._measurement == "emon" and r._field == "value" and r.field == "dhw_t1")
  |> last()"#;
    let t1 = query_influxdb(&state.http_client, t1_query)
        .await
        .map(|(v, _)| v)
        .unwrap_or(0.0);

    // HwcStorageTemp — VR 10 NTC in cylinder dry pocket, above bottom coil
    let cylinder_temp: f64 = ebusd_command("read -f -c 700 HwcStorageTemp")
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or(0.0);

    Json(serde_json::json!({
        "ok": true,
        "charging": charging,
        "sfmode": sfmode,
        "t1_hot": t1,
        "cylinder_temp": cylinder_temp,
        "return_temp": return_temp,
        "target_temp": target_temp
    }))
}

async fn page_home() -> Html<&'static str> {
    Html(HOME_PAGE)
}

const HOME_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Home</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: #1a1a2e;
    color: #eee;
    display: flex;
    flex-direction: column;
    align-items: center;
    min-height: 100vh;
    padding: 12px;
  }
  .section {
    width: 100%;
    max-width: 400px;
    margin-bottom: 20px;
  }
  .section h2 {
    font-size: 13px;
    color: #666;
    text-transform: uppercase;
    letter-spacing: 1px;
    margin-bottom: 10px;
  }

  /* Hot water */
  .hw-row {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  .tank {
    position: relative;
    width: 60px;
    height: 120px;
    border: 3px solid #555;
    border-radius: 10px;
    overflow: hidden;
    background: #222;
    flex-shrink: 0;
  }
  .water {
    position: absolute;
    bottom: 0;
    width: 100%;
    background: linear-gradient(to top, #cc2200, #ff4433);
    transition: height 1s ease, background 1s ease;
    border-radius: 0 0 7px 7px;
  }
  .water.cool {
    background: linear-gradient(to top, #2255aa, #4488cc);
  }
  .water.warm {
    background: linear-gradient(to top, #cc8800, #ffaa33);
  }
  .hw-info { display: flex; flex-direction: column; }
  .litres { font-size: 40px; font-weight: 700; }
  .hw-label { font-size: 14px; color: #999; }
  .hw-status { font-size: 14px; font-weight: 600; margin-top: 2px; }
  .hw-status.empty { color: #ff4444; }
  .hw-status.low { color: #ffaa00; }
  .hw-status.ok { color: #44cc44; }
  .hw-status.full { color: #ff4433; }
  .hw-updated { font-size: 11px; color: #666; margin-top: 2px; }

  /* Lights */
  .light-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    background: #222;
    border-radius: 12px;
    padding: 12px 14px;
    margin-bottom: 6px;
    cursor: pointer;
    -webkit-tap-highlight-color: transparent;
    user-select: none;
    transition: background 0.2s;
  }
  .light-row:active { background: #333; }
  .light-name {
    font-size: 16px;
    font-weight: 600;
    text-transform: capitalize;
  }
  .toggle {
    width: 46px;
    height: 26px;
    border-radius: 13px;
    background: #444;
    position: relative;
    transition: background 0.3s;
    flex-shrink: 0;
  }
  .toggle.on { background: #f0c040; }
  .toggle::after {
    content: '';
    position: absolute;
    top: 3px;
    left: 3px;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    background: #fff;
    transition: transform 0.3s;
  }
  .toggle.on::after { transform: translateX(20px); }

  /* Boost button */
  .boost-btn {
    width: 100%;
    border: none;
    border-radius: 12px;
    padding: 12px;
    font-size: 16px;
    font-weight: 600;
    cursor: pointer;
    background: #222;
    color: #ff6633;
    transition: background 0.2s;
    -webkit-tap-highlight-color: transparent;
  }
  .boost-btn:active { background: #333; }
  .boost-btn.sent { color: #44cc44; }
  .boost-btn.charging {
    color: #ff6633;
    animation: pulse 2s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.5; }
  }
  .dhw-info {
    font-size: 13px;
    color: #666;
    margin-top: 6px;
  }

  /* Heating */
  .heating-mode {
    font-size: 28px;
    font-weight: 700;
    text-transform: capitalize;
    margin-bottom: 10px;
  }
  .heating-mode.occupied { color: #44cc44; }
  .heating-mode.short_absence { color: #ffaa00; }
  .heating-mode.away_until { color: #ff6633; }
  .heating-mode.disabled { color: #666; }
  .heating-mode.monitor_only { color: #6699ff; }
  .heating-btns {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .mode-btn {
    flex: 1;
    min-width: 80px;
    border: none;
    border-radius: 12px;
    padding: 10px 8px;
    font-size: 14px;
    font-weight: 600;
    cursor: pointer;
    background: #222;
    color: #999;
    transition: background 0.2s;
    -webkit-tap-highlight-color: transparent;
  }
  .mode-btn:active { background: #333; }
  .mode-btn.active { background: #2a4a2a; color: #44cc44; }
  .heating-away-info {
    font-size: 13px;
    color: #666;
    margin-top: 6px;
  }
</style>
</head>
<body>
  <div class="section">
    <h2>💧 Hot Water</h2>
    <div class="hw-row">
      <div class="tank"><div class="water" id="water"></div></div>
      <div class="hw-info">
        <div class="litres" id="litres">—</div>
        <div class="hw-label">litres remaining</div>
        <div class="hw-status" id="hw-status"></div>
        <div class="hw-updated" id="hw-updated"></div>
      </div>
    </div>
  </div>

  <div class="section">
    <h2>🔥 Hot Water Boost</h2>
    <button class="boost-btn" id="boost-btn" onclick="boostDhw()">Boost</button>
    <div class="dhw-info" id="dhw-info"></div>
  </div>

  <div class="section">
    <h2>🏠 Heating</h2>
    <div class="heating-mode" id="heating-mode">—</div>
    <div class="heating-btns">
      <button class="mode-btn" id="hm-occupied" onclick="setHeatingMode('occupied')">Occupied</button>
      <button class="mode-btn" id="hm-short-absence" onclick="setHeatingMode('short-absence')">Short Absence</button>
      <button class="mode-btn" id="hm-disabled" onclick="setHeatingMode('disabled')">Disabled</button>
    </div>
    <div class="heating-away-info" id="heating-away-info"></div>
    <button class="boost-btn" style="margin-top:8px;color:#ff4444" onclick="killHeating()">🛑 Kill / Restore Baseline</button>
  </div>

  <div class="section">
    <h2>💡 Lights</h2>
    <div id="lights"></div>
  </div>

<script>
let TANK_MAX = 177; // updated from API on first poll
const LIGHTS = ['landing', 'hall', 'top_landing'];

async function updateHotWater() {
  try {
    const r = await fetch('/api/hot-water');
    const d = await r.json();
    if (!d.ok) return;
    if (d.full_litres) TANK_MAX = d.full_litres;
    const litres = Math.round(d.remaining_litres);
    const pct = Math.min(100, Math.max(0, (litres / TANK_MAX) * 100));
    document.getElementById('litres').textContent = litres;
    const waterEl = document.getElementById('water');
    waterEl.style.height = pct + '%';
    // Colour by effective temperature: hot (≥42) → warm (38–42) → cool (<38)
    const et = d.effective_t1 || 0;
    waterEl.classList.remove('cool', 'warm');
    if (et > 0 && et < 38) waterEl.classList.add('cool');
    else if (et >= 38 && et < 42) waterEl.classList.add('warm');
    const el = document.getElementById('hw-status');
    const cs = d.charge_state || '';
    if (cs === 'charging_below') { el.textContent = 'Heating below'; el.className = 'hw-status low'; }
    else if (cs === 'charging_uniform') { el.textContent = 'Heating uniformly'; el.className = 'hw-status ok'; }
    else if (litres <= 0) { el.textContent = 'Empty'; el.className = 'hw-status empty'; }
    else if (litres < 40 || (d.effective_t1 && d.effective_t1 < 42)) { el.textContent = 'Low'; el.className = 'hw-status low'; }
    else if (litres < 150) { el.textContent = 'OK'; el.className = 'hw-status ok'; }
    else if (cs === 'partial') { el.textContent = 'Partial'; el.className = 'hw-status ok'; }
    else { el.textContent = 'Full'; el.className = 'hw-status full'; }
    // Stale indicator: standby with ~ prefix
    if (cs === 'standby' && litres > 0) {
      document.getElementById('litres').textContent = '~' + litres;
    }
    if (d.timestamp) {
      const t = new Date(d.timestamp);
      document.getElementById('hw-updated').textContent = 'Updated ' + t.toLocaleTimeString();
    }
  } catch(e) { console.error(e); }
}

function buildLights() {
  const container = document.getElementById('lights');
  LIGHTS.forEach(name => {
    const row = document.createElement('div');
    row.className = 'light-row';
    row.id = 'light-' + name;
    row.innerHTML = `
      <span class="light-name">${name}</span>
      <div class="toggle" id="toggle-${name}"></div>`;
    row.addEventListener('click', () => toggleLight(name));
    container.appendChild(row);
  });
}

async function toggleLight(name) {
  try {
    const r = await fetch('/api/lights/' + name + '/toggle', { method: 'POST' });
    const d = await r.json();
    if (d.ok) {
      const el = document.getElementById('toggle-' + name);
      el.className = d.state === 'ON' ? 'toggle on' : 'toggle';
    }
  } catch(e) { console.error(e); }
}

async function updateLights() {
  try {
    const r = await fetch('/api/lights');
    const d = await r.json();
    if (!d.ok) return;
    for (const [name, state] of Object.entries(d.lights)) {
      const el = document.getElementById('toggle-' + name);
      if (el) el.className = state.on ? 'toggle on' : 'toggle';
    }
  } catch(e) { console.error(e); }
}

async function boostDhw() {
  const btn = document.getElementById('boost-btn');
  try {
    const r = await fetch('/api/dhw/boost', { method: 'POST' });
    const d = await r.json();
    if (d.ok) {
      btn.textContent = 'Boost sent';
      btn.classList.add('sent');
      setTimeout(() => { updateDhwStatus(); }, 2000);
    }
  } catch(e) { console.error(e); }
}

async function updateDhwStatus() {
  try {
    const r = await fetch('/api/dhw/status');
    const d = await r.json();
    if (!d.ok) return;
    const btn = document.getElementById('boost-btn');
    const info = document.getElementById('dhw-info');
    if (d.charging) {
      btn.textContent = 'Boosting…';
      btn.className = 'boost-btn charging';
      let parts = [];
      if (d.cylinder_temp > 0) parts.push('Lower ' + d.cylinder_temp.toFixed(1) + '°C');
      if (d.t1_hot > 0) parts.push('Top ' + d.t1_hot.toFixed(1) + '°C');
      info.textContent = parts.join(' · ') || d.return_temp + '°C';
    } else {
      btn.textContent = 'Boost';
      btn.className = 'boost-btn';
      let parts = [];
      if (d.t1_hot > 0) parts.push('Top ' + d.t1_hot.toFixed(1) + '°C');
      if (d.cylinder_temp > 0) parts.push('Lower ' + d.cylinder_temp.toFixed(1) + '°C');
      info.textContent = parts.join(' · ');
    }
  } catch(e) { console.error(e); }
}

// ── Heating MVP ──
const HEATING_MODES = ['occupied', 'short-absence', 'disabled'];

async function updateHeating() {
  try {
    const r = await fetch('/api/heating/status');
    const d = await r.json();
    if (!d.mode) return;
    const el = document.getElementById('heating-mode');
    const awayEl = document.getElementById('heating-away-info');
    el.textContent = d.mode.replace('_', ' ');
    el.className = 'heating-mode ' + d.mode;
    if (d.away_until) {
      const t = new Date(d.away_until);
      awayEl.textContent = 'Return: ' + t.toLocaleString();
    } else {
      awayEl.textContent = d.last_reason || '';
    }
    HEATING_MODES.forEach(m => {
      const btn = document.getElementById('hm-' + m);
      if (btn) btn.className = (d.mode === m || d.mode === m.replace('-','_')) ? 'mode-btn active' : 'mode-btn';
    });
  } catch(e) { console.error(e); }
}

async function setHeatingMode(mode) {
  await fetch('/api/heating/mode/' + mode, { method: 'POST' });
  setTimeout(updateHeating, 500);
}

async function killHeating() {
  if (!confirm('Restore baseline and disable controller?')) return;
  await fetch('/api/heating/kill', { method: 'POST' });
  setTimeout(updateHeating, 500);
}

buildLights();
updateHotWater();
updateLights();
updateDhwStatus();
updateHeating();
setInterval(updateHotWater, 30000);
setInterval(updateLights, 5000);
setInterval(updateDhwStatus, 10000);
setInterval(updateHeating, 15000);
</script>
</body>
</html>"#;

// ── Heating MVP proxy ────────────────────────────────────────────────────────

const HEATING_MVP_URL: &str = "http://127.0.0.1:3031";

async fn api_heating_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state
        .http_client
        .get(format!("{HEATING_MVP_URL}/status"))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        },
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn api_heating_mode(
    State(state): State<AppState>,
    axum::extract::Path(mode): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let url = format!("{HEATING_MVP_URL}/mode/{mode}");
    match state.http_client.post(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        },
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_heating_away(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    match state
        .http_client
        .post(format!("{HEATING_MVP_URL}/mode/away"))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        },
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_heating_kill(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state
        .http_client
        .post(format!("{HEATING_MVP_URL}/kill"))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        },
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

// ── DHW tracking loop (physics-based model) ────────────────────────────────

async fn get_current_volume(client: &reqwest::Client) -> f64 {
    let query = r#"from(bucket: "energy")
  |> range(start: -1h)
  |> filter(fn: (r) => r._measurement == "emon" and r._field == "value" and r.field == "dhw_volume_V1")
  |> last()"#;
    query_influxdb(client, query)
        .await
        .map(|(v, _)| v)
        .unwrap_or(0.0)
}

async fn get_current_t1(client: &reqwest::Client) -> f64 {
    let query = r#"from(bucket: "energy")
  |> range(start: -1h)
  |> filter(fn: (r) => r._measurement == "emon" and r._field == "value" and r.field == "dhw_t1")
  |> last()"#;
    query_influxdb(client, query)
        .await
        .map(|(v, _)| v)
        .unwrap_or(0.0)
}

async fn get_current_dhw_flow(client: &reqwest::Client) -> f64 {
    let query = r#"from(bucket: "energy")
  |> range(start: -5m)
  |> filter(fn: (r) => r._measurement == "emon" and r._field == "value" and r.field == "dhw_flow")
  |> last()"#;
    query_influxdb(client, query)
        .await
        .map(|(v, _)| v)
        .unwrap_or(0.0)
}

async fn get_hwc_storage_temp() -> f64 {
    ebusd_command("read -f -c 700 HwcStorageTemp")
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or(0.0)
}

async fn is_charging() -> bool {
    let sfmode = ebusd_command("read -f -c 700 HwcSFMode")
        .await
        .unwrap_or_default();
    let status = ebusd_command("read -f -c hmu Status01")
        .await
        .unwrap_or_default();
    status.ends_with(";hwc") || sfmode == "load"
}

async fn write_dhw_to_influxdb(client: &reqwest::Client, s: &DhwState) {
    let line = format!(
        "dhw remaining_litres={:.1},model_version=2i,t1={:.2},hwc_storage={:.2},\
         effective_t1={:.2},charge_state=\"{}\",crossover={},bottom_zone_hot={}",
        s.remaining,
        s.current_t1,
        s.current_hwc,
        s.effective_t1,
        s.charge_state,
        s.crossover_achieved,
        // Bottom zone is "hot" when HwcStorage is significantly above mains (~20°C)
        s.current_hwc > 30.0,
    );
    let result = client
        .post(format!(
            "{INFLUXDB_URL}/api/v2/write?org={INFLUXDB_ORG}&bucket=energy&precision=s"
        ))
        .header("Authorization", format!("Token {INFLUXDB_TOKEN}"))
        .header("Content-Type", "text/plain")
        .body(line)
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => error!("InfluxDB write failed: {}", resp.status()),
        Err(e) => error!("InfluxDB write error: {e}"),
    }
}

/// Apply standby decay: T1 drops at configured rate, mark standby below reduced_t1
fn apply_standby_decay(s: &mut DhwState, cfg: &DhwConfig) {
    if let Some(end_time) = s.charge_end_time {
        let hours = end_time.elapsed().as_secs_f64() / 3600.0;
        s.effective_t1 = s.t1_at_charge_end - cfg.t1_decay_rate * hours;

        // Below reduced_t1, don't reduce remaining — the water is still there,
        // just lukewarm. The SPA shows colour (blue/amber/red) for temperature.
        // Only mark as standby.
        if s.effective_t1 < cfg.reduced_t1 {
            s.charge_state = DhwChargeState::Standby;
        }

        // Mark as standby after 2h
        if hours > 2.0
            && !matches!(
                s.charge_state,
                DhwChargeState::ChargingBelow | DhwChargeState::ChargingUniform
            )
        {
            s.charge_state = DhwChargeState::Standby;
        }
    }
}

/// Determine remaining litres after a no-crossover charge ends.
/// Uses the gap between T1 and HwcStorage to estimate thermocline state.
fn apply_no_crossover_charge(s: &mut DhwState, cfg: &DhwConfig, t1_now: f64, hwc_now: f64) {
    let gap = t1_now - hwc_now;
    let full = s.full_litres;
    info!(
        "DHW no-crossover charge ended: T1={t1_now:.1}, HwcS={hwc_now:.1}, gap={gap:.1}"
    );

    if gap < cfg.gap_dissolved {
        // Thermocline dissolved — effectively full but at a lower temperature
        s.remaining = full;
        s.charge_state = DhwChargeState::Full;
        info!("  Gap <{:.1}°C → thermocline dissolved, full at lower temp", cfg.gap_dissolved);
    } else if gap > cfg.gap_sharp {
        // Sharp thermocline — remaining unchanged from before charge
        s.charge_state = DhwChargeState::Partial;
        info!(
            "  Gap >{:.1}°C → sharp thermocline, remaining unchanged at {:.0}L",
            cfg.gap_sharp, s.remaining
        );
    } else {
        // Intermediate: interpolate between unchanged and full
        let frac = (cfg.gap_sharp - gap) / (cfg.gap_sharp - cfg.gap_dissolved);
        let interpolated = s.remaining + frac * (full - s.remaining);
        s.remaining = interpolated;
        s.charge_state = DhwChargeState::Partial;
        info!(
            "  Gap {gap:.1}°C → interpolated frac={frac:.2}, remaining={interpolated:.0}L"
        );
    }
}

fn apply_charge_completion(s: &mut DhwState, cfg: &DhwConfig, t1_now: f64, hwc_now: f64) {
    let full = s.full_litres;
    s.t1_at_charge_end = t1_now;
    s.hwc_at_charge_end = hwc_now;
    s.charge_end_time = Some(std::time::Instant::now());
    s.effective_t1 = t1_now;

    if s.crossover_achieved {
        s.remaining = full;
        s.charge_state = DhwChargeState::Full;
        info!(
            "DHW charge complete (crossover): T1={t1_now:.1}, HwcS={hwc_now:.1} \
             → {full:.0}L"
        );
    } else {
        apply_no_crossover_charge(s, cfg, t1_now, hwc_now);
    }
}

fn apply_draw_tracking(
    s: &mut DhwState,
    cfg: &DhwConfig,
    volume_now: f64,
    t1_now: f64,
    hwc_now: f64,
) {
    if volume_now <= s.volume_at_reset {
        return;
    }

    let drawn = volume_now - s.volume_at_reset;
    let remaining_by_volume = (s.full_litres - drawn).max(0.0);
    s.remaining = s.remaining.min(remaining_by_volume);

    if s.drawing {
        let hwc_drop = s.hwc_pre_draw - hwc_now;
        if hwc_drop > cfg.hwc_crash_threshold && !s.hwc_crash_detected {
            s.hwc_crash_detected = true;
            let cap = cfg.vol_above_hwc;
            if s.remaining > cap {
                info!(
                    "DHW HwcS crash ({hwc_drop:.1}°C): capping remaining \
                     {:.0} → {cap:.0}L",
                    s.remaining
                );
                s.remaining = cap;
            }
        }

        let t1_drop = s.t1_pre_draw - t1_now;
        if t1_drop > 1.5 {
            if s.remaining > 0.0 {
                info!("DHW T1 crashed {t1_drop:.1}°C: remaining → 0");
            }
            s.remaining = 0.0;
        } else if t1_drop > 0.5 {
            let cap = 20.0;
            if s.remaining > cap {
                info!(
                    "DHW T1 dropping {t1_drop:.1}°C: capping remaining \
                     {:.0} → {cap:.0}L",
                    s.remaining
                );
                s.remaining = cap;
            }
        }
    }
}

async fn dhw_tracking_loop(
    state: Arc<Mutex<DhwState>>,
    client: reqwest::Client,
    config: Arc<HubConfig>,
) {
    let cfg = &config.dhw;

    // Autoload recommended capacity from InfluxDB (written by dhw-inflection-detector.py)
    {
        let query = r#"from(bucket: "energy")
  |> range(start: -90d)
  |> filter(fn: (r) => r._measurement == "dhw_capacity" and r._field == "recommended_full_litres")
  |> last()"#;
        if let Ok((recommended, _)) = query_influxdb(&client, query).await {
            if recommended >= cfg.full_litres_min && recommended <= cfg.full_litres_max {
                let mut s = state.lock().await;
                let prev = s.full_litres;
                s.full_litres = s.full_litres.max(recommended);
                info!(
                    "DHW autoload: recommended={recommended:.0}L, \
                     config={prev:.0}L → using {:.0}L",
                    s.full_litres
                );
            } else if recommended > 0.0 {
                warn!(
                    "DHW autoload: recommended={recommended:.0}L outside sane range \
                     [{:.0}, {:.0}], ignoring",
                    cfg.full_litres_min, cfg.full_litres_max
                );
            }
        }
    }

    // Initialise remaining from InfluxDB
    {
        let query = r#"from(bucket: "energy")
  |> range(start: -24h)
  |> filter(fn: (r) => r._measurement == "dhw" and r._field == "remaining_litres")
  |> last()"#;
        let remaining = query_influxdb(&client, query)
            .await
            .map(|(v, _)| v)
            .unwrap_or(0.0);
        let volume = get_current_volume(&client).await;
        let charging = is_charging().await;
        let t1 = get_current_t1(&client).await;
        let hwc = get_hwc_storage_temp().await;

        let mut s = state.lock().await;
        s.remaining = remaining;
        // Reconstruct volume_at_reset: if 66L drawn from full, the reset
        // point was 66L before the current register reading
        let already_drawn = (s.full_litres - remaining).max(0.0);
        s.volume_at_reset = volume - already_drawn;
        s.was_charging = charging;
        s.current_t1 = t1;
        s.current_hwc = hwc;
        s.effective_t1 = t1; // Best guess on startup
        s.t1_at_charge_end = t1;
        // If we're already charging on startup, capture T1 for crossover detection
        if charging {
            s.t1_at_charge_start = t1;
            s.charge_state = DhwChargeState::ChargingBelow;
        }
        info!(
            "DHW init: remaining={remaining:.1}L, full={:.0}L, volume={volume:.1}, \
             T1={t1:.1}, HwcS={hwc:.1}, charging={charging}",
            s.full_litres
        );
    }

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        // Read all sensors
        let charging = is_charging().await;
        let volume_now = get_current_volume(&client).await;
        let t1_now = get_current_t1(&client).await;
        let hwc_now = get_hwc_storage_temp().await;
        let dhw_flow = get_current_dhw_flow(&client).await;

        let mut s = state.lock().await;

        // Update cached sensor values
        s.current_t1 = t1_now;
        s.current_hwc = hwc_now;

        // ── Charging state machine ──────────────────────────────────────

        if charging && !s.was_charging {
            // Charge just started
            s.t1_at_charge_start = t1_now;
            s.crossover_achieved = false;
            s.charge_state = DhwChargeState::ChargingBelow;
            info!(
                "DHW charge started: T1={t1_now:.1}, HwcS={hwc_now:.1}, \
                 crossover target={t1_now:.1}"
            );
        }

        if charging {
            // Monitor for crossover: HwcStorage reaches T1 at charge start
            if !s.crossover_achieved && hwc_now >= s.t1_at_charge_start {
                s.crossover_achieved = true;
                s.charge_state = DhwChargeState::ChargingUniform;
                info!(
                    "DHW CROSSOVER achieved: HwcS={hwc_now:.1} ≥ T1_start={:.1}",
                    s.t1_at_charge_start
                );
            }
        }

        if s.was_charging && !charging {
            // ── Charge just ended ───────────────────────────────────────
            apply_charge_completion(&mut s, cfg, t1_now, hwc_now);
            s.volume_at_reset = volume_now;
            s.hwc_crash_detected = false;
            write_dhw_to_influxdb(&client, &s).await;
        }

        // ── Draw detection and tracking ─────────────────────────────────

        // dhw_flow is from the Multical tap-side meter — independent of HP circuit.
        // Draws during charging still deplete the cylinder and must be tracked.
        let is_drawing = dhw_flow > cfg.draw_flow_min;

        if is_drawing && !s.drawing {
            // Draw just started
            s.drawing = true;
            s.hwc_pre_draw = hwc_now;
            s.t1_pre_draw = t1_now;
            s.hwc_crash_detected = false;
            info!("DHW draw started: T1={t1_now:.1}, HwcS={hwc_now:.1}");
        }

        if volume_now > s.volume_at_reset {
            apply_draw_tracking(&mut s, cfg, volume_now, t1_now, hwc_now);
            write_dhw_to_influxdb(&client, &s).await;
        }

        if s.drawing && !is_drawing {
            // Draw ended
            s.drawing = false;
            info!(
                "DHW draw ended: remaining={:.0}L, T1={t1_now:.1}, HwcS={hwc_now:.1}",
                s.remaining
            );
            write_dhw_to_influxdb(&client, &s).await;
        }

        // ── Standby decay ───────────────────────────────────────────────

        if !charging && !s.drawing {
            apply_standby_decay(&mut s, cfg);
        }

        s.was_charging = charging;
    }
}

// ── Timer loop ──────────────────────────────────────────────────────────────

async fn timer_loop(state: Arc<Mutex<AutomationState>>, cmd_tx: broadcast::Sender<Z2mMessage>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let mut s = state.lock().await;
        if let Some(off_at) = s.lights_off_at {
            if Instant::now() >= off_at {
                info!("Timer expired — turning OFF {MOTION_LIGHTS:?}");
                s.lights_off_at = None;
                for light in MOTION_LIGHTS {
                    let msg = Z2mMessage {
                        topic: format!("{light}/set"),
                        payload: serde_json::json!({"state": "OFF"}),
                    };
                    let _ = cmd_tx.send(msg);
                }
            }
        }
    }
}

// ── Z2M WebSocket ───────────────────────────────────────────────────────────

async fn z2m_connection_loop(
    state: Arc<Mutex<AutomationState>>,
    cmd_tx: broadcast::Sender<Z2mMessage>,
    z2m_state: Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
) {
    loop {
        info!("Connecting to Z2M at {Z2M_WS_URL}");
        match tokio_tungstenite::connect_async(Z2M_WS_URL).await {
            Ok((ws_stream, _)) => {
                info!("Connected to Z2M");
                let (write, read) = ws_stream.split();
                let write = Arc::new(Mutex::new(write));

                let write_clone = write.clone();
                let mut cmd_rx = cmd_tx.subscribe();
                let writer_handle = tokio::spawn(async move {
                    while let Ok(msg) = cmd_rx.recv().await {
                        let text = serde_json::to_string(&msg).unwrap();
                        info!("Sending to Z2M: {text}");
                        let mut w = write_clone.lock().await;
                        if let Err(e) = w
                            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                            .await
                        {
                            error!("WS write error: {e}");
                            break;
                        }
                    }
                });

                let mut read = read;
                loop {
                    match read.next().await {
                        Some(Ok(msg)) => {
                            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                                handle_z2m_message(&text, &state, &cmd_tx, &z2m_state).await;
                            }
                        }
                        Some(Err(e)) => {
                            error!("WS read error: {e}");
                            break;
                        }
                        None => {
                            warn!("Z2M WebSocket closed");
                            break;
                        }
                    }
                }

                writer_handle.abort();
            }
            Err(e) => {
                error!("Failed to connect to Z2M: {e}");
            }
        }

        warn!("Reconnecting in {RECONNECT_DELAY:?}...");
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

// ── Z2M message handler ────────────────────────────────────────────────────

async fn handle_z2m_message(
    text: &str,
    state: &Arc<Mutex<AutomationState>>,
    cmd_tx: &broadcast::Sender<Z2mMessage>,
    z2m_state: &Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
) {
    let msg: Z2mMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to parse Z2M message: {e}");
            return;
        }
    };

    // Cache device state for any non-bridge topic
    if !msg.topic.starts_with("bridge/") && !msg.topic.contains('/') {
        let mut zs = z2m_state.lock().await;
        zs.insert(msg.topic.clone(), msg.payload.clone());
    }

    match msg.topic.as_str() {
        "bridge/state" => {
            info!("Z2M bridge state: {}", msg.payload);
        }
        "bridge/info" => {
            if let Some(version) = msg.payload.get("version") {
                info!("Z2M version: {version}");
            }
        }
        topic if MOTION_SENSORS.iter().any(|(name, _)| *name == topic) => {
            let threshold = MOTION_SENSORS
                .iter()
                .find(|(name, _)| *name == topic)
                .unwrap()
                .1;
            let mut s = state.lock().await;

            if s.lights_off_at.is_none() {
                if let Some(lux) = msg.payload.get("illuminance").and_then(|v| v.as_f64()) {
                    s.illuminance.insert(topic.to_string(), lux);
                }
            }

            // Clear expired suppression
            if let Some(until) = s.suppressed_until {
                if Instant::now() >= until {
                    s.suppressed_until = None;
                }
            }

            if let Some(occupancy) = msg.payload.get("occupancy").and_then(|v| v.as_bool()) {
                if occupancy {
                    if s.suppressed_until.is_some() {
                        // Automation suppressed by manual override — ignore motion
                    } else if s.lights_off_at.is_some() {
                        s.lights_off_at = Some(Instant::now() + OFF_DELAY);
                        info!("Motion on {topic} — lights already on, reset timer");
                    } else {
                        let lux = s.illuminance.get(topic).copied().unwrap_or(0.0);
                        if lux <= threshold {
                            info!("Motion on {topic} (lux={lux}, threshold={threshold}) — turning ON {MOTION_LIGHTS:?}");

                            for light in MOTION_LIGHTS {
                                let on_msg = Z2mMessage {
                                    topic: format!("{light}/set"),
                                    payload: serde_json::json!({"state": "ON"}),
                                };
                                let _ = cmd_tx.send(on_msg);
                            }

                            s.lights_off_at = Some(Instant::now() + OFF_DELAY);
                            info!("Scheduled {MOTION_LIGHTS:?} OFF in {OFF_DELAY:?}");
                        } else {
                            info!("Motion on {topic} (lux={lux}, threshold={threshold}) — too bright, skipping");
                        }
                    }
                }
            }
        }
        // Detect manual off on a motion light — cancel automation + suppress re-trigger
        topic if MOTION_LIGHTS.contains(&topic) => {
            if let Some(state_val) = msg.payload.get("state").and_then(|v| v.as_str()) {
                if state_val == "OFF" {
                    let mut s = state.lock().await;
                    if s.lights_off_at.is_some() {
                        s.lights_off_at = None;
                        s.suppressed_until = Some(Instant::now() + OFF_DELAY);
                        info!("Manual OFF on {topic} — automation suppressed for {OFF_DELAY:?}");
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Build a DhwConfig with the production defaults (matching HubConfig::default)
    fn test_cfg() -> DhwConfig {
        DhwConfig {
            full_litres: 177.0,
            t1_decay_rate: 0.25,
            reduced_t1: 42.0,
            hwc_crash_threshold: 5.0,
            vol_above_hwc: 148.0,
            draw_flow_min: 100.0,
            gap_sharp: 3.5,
            gap_dissolved: 1.5,
            full_litres_min: 160.0,
            full_litres_max: 220.0,
        }
    }

    /// Build a DhwState with sensible defaults for testing
    fn test_state() -> DhwState {
        DhwState {
            full_litres: 177.0,
            remaining: 100.0,
            volume_at_reset: 0.0,
            t1_at_charge_start: 40.0,
            crossover_achieved: false,
            t1_at_charge_end: 50.0,
            hwc_at_charge_end: 48.0,
            charge_end_time: None,
            effective_t1: 50.0,
            hwc_pre_draw: 48.0,
            hwc_crash_detected: false,
            t1_pre_draw: 50.0,
            drawing: false,
            current_t1: 50.0,
            current_hwc: 48.0,
            was_charging: false,
            charge_state: DhwChargeState::Full,
        }
    }

    fn test_automation_state() -> Arc<Mutex<AutomationState>> {
        Arc::new(Mutex::new(AutomationState {
            lights_off_at: None,
            suppressed_until: None,
            illuminance: std::collections::HashMap::new(),
        }))
    }

    // ── apply_no_crossover_charge tests ─────────────────────────────────

    // @lat: [[tests#DHW no crossover#Dissolved thermocline resets to full]]
    #[test]
    fn no_crossover_dissolved_thermocline_sets_full() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 80.0;

        // gap = 45 - 44 = 1.0 < gap_dissolved(1.5) → full
        apply_no_crossover_charge(&mut s, &cfg, 45.0, 44.0);

        assert_eq!(s.remaining, 177.0);
        assert_eq!(s.charge_state, DhwChargeState::Full);
    }

    // @lat: [[tests#DHW no crossover#Sharp thermocline preserves prior remaining]]
    #[test]
    fn no_crossover_sharp_thermocline_preserves_remaining() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 80.0;

        // gap = 48 - 44 = 4.0 > gap_sharp(3.5) → remaining unchanged
        apply_no_crossover_charge(&mut s, &cfg, 48.0, 44.0);

        assert_eq!(s.remaining, 80.0);
        assert_eq!(s.charge_state, DhwChargeState::Partial);
    }

    // @lat: [[tests#DHW no crossover#Intermediate gap interpolates between prior and full]]
    #[test]
    fn no_crossover_intermediate_interpolates() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 80.0;

        // gap = 46 - 43.5 = 2.5, between dissolved(1.5) and sharp(3.5)
        // frac = (3.5 - 2.5) / (3.5 - 1.5) = 1.0 / 2.0 = 0.5
        // interpolated = 80 + 0.5 * (177 - 80) = 80 + 48.5 = 128.5
        apply_no_crossover_charge(&mut s, &cfg, 46.0, 43.5);

        assert!((s.remaining - 128.5).abs() < 0.01);
        assert_eq!(s.charge_state, DhwChargeState::Partial);
    }

    // @lat: [[tests#DHW no crossover#Dissolved boundary stays on interpolation path]]
    #[test]
    fn no_crossover_at_dissolved_boundary() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 60.0;

        // gap = exactly gap_dissolved (1.5) → should be full (gap < is strict, so 1.5 is NOT < 1.5)
        // Actually gap < 1.5 is false when gap == 1.5, so it falls to intermediate
        // frac = (3.5 - 1.5) / (3.5 - 1.5) = 1.0
        // interpolated = 60 + 1.0 * (177 - 60) = 177
        apply_no_crossover_charge(&mut s, &cfg, 45.0, 43.5);

        assert!((s.remaining - 177.0).abs() < 0.01);
        assert_eq!(s.charge_state, DhwChargeState::Partial);
    }

    // @lat: [[tests#DHW no crossover#Sharp boundary keeps prior litres without full reset]]
    #[test]
    fn no_crossover_at_sharp_boundary() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 60.0;

        // gap = exactly gap_sharp (3.5) → not > 3.5, falls to intermediate
        // frac = (3.5 - 3.5) / (3.5 - 1.5) = 0.0
        // interpolated = 60 + 0.0 * (177 - 60) = 60
        apply_no_crossover_charge(&mut s, &cfg, 47.0, 43.5);

        assert!((s.remaining - 60.0).abs() < 0.01);
        assert_eq!(s.charge_state, DhwChargeState::Partial);
    }

    // @lat: [[tests#DHW no crossover#Dissolved gap can recover from zero remaining]]
    #[test]
    fn no_crossover_zero_remaining_dissolved_resets_to_full() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 0.0;

        // Dissolved thermocline should set to full even from 0
        apply_no_crossover_charge(&mut s, &cfg, 44.0, 43.0);

        assert_eq!(s.remaining, 177.0);
        assert_eq!(s.charge_state, DhwChargeState::Full);
    }

    // ── apply_charge_completion tests ───────────────────────────────────

    // @lat: [[tests#DHW charge completion#Crossover completion restores full litres and full state]]
    #[test]
    fn charge_completion_with_crossover_restores_full_litres_and_full_state() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 45.0;
        s.full_litres = 177.0;
        s.crossover_achieved = true;
        s.charge_state = DhwChargeState::ChargingUniform;

        apply_charge_completion(&mut s, &cfg, 49.0, 46.0);

        assert_eq!(s.remaining, 177.0);
        assert_eq!(s.charge_state, DhwChargeState::Full);
        assert_eq!(s.t1_at_charge_end, 49.0);
        assert_eq!(s.hwc_at_charge_end, 46.0);
        assert_eq!(s.effective_t1, 49.0);
        assert!(s.charge_end_time.is_some());
    }

    // @lat: [[tests#DHW charge completion#Charge completion without crossover falls back to the gap model]]
    #[test]
    fn charge_completion_without_crossover_uses_gap_model() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.remaining = 80.0;
        s.crossover_achieved = false;
        s.charge_state = DhwChargeState::ChargingBelow;

        apply_charge_completion(&mut s, &cfg, 46.0, 43.5);

        assert!((s.remaining - 128.5).abs() < 0.01);
        assert_eq!(s.charge_state, DhwChargeState::Partial);
        assert_eq!(s.t1_at_charge_end, 46.0);
        assert_eq!(s.hwc_at_charge_end, 43.5);
        assert_eq!(s.effective_t1, 46.0);
        assert!(s.charge_end_time.is_some());
    }

    // ── apply_draw_tracking tests ───────────────────────────────────────

    // @lat: [[tests#DHW draw tracking#Volume draw alone reduces remaining litres]]
    #[test]
    fn draw_tracking_volume_only_reduces_remaining() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 120.0;
        s.volume_at_reset = 1000.0;
        s.drawing = false;

        apply_draw_tracking(&mut s, &cfg, 1025.0, 50.0, 48.0);

        assert_eq!(s.remaining, 120.0_f64.min(177.0 - 25.0));
        assert!(!s.hwc_crash_detected);
    }

    // @lat: [[tests#DHW draw tracking#Hwc storage crash caps remaining at the upper sensor volume]]
    #[test]
    fn draw_tracking_hwc_crash_caps_remaining_at_upper_sensor_volume() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 170.0;
        s.volume_at_reset = 1000.0;
        s.drawing = true;
        s.hwc_pre_draw = 50.0;
        s.hwc_crash_detected = false;
        s.t1_pre_draw = 50.0;

        apply_draw_tracking(&mut s, &cfg, 1010.0, 49.8, 44.0);

        assert_eq!(s.remaining, cfg.vol_above_hwc);
        assert!(s.hwc_crash_detected);
    }

    // @lat: [[tests#DHW draw tracking#A repeated Hwc storage crash does not reapply the cap logic]]
    #[test]
    fn draw_tracking_repeated_hwc_crash_does_not_reapply_cap_logic() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 160.0;
        s.volume_at_reset = 1000.0;
        s.drawing = true;
        s.hwc_pre_draw = 50.0;
        s.hwc_crash_detected = true;
        s.t1_pre_draw = 50.0;

        apply_draw_tracking(&mut s, &cfg, 1010.0, 49.8, 44.0);

        assert_eq!(s.remaining, 160.0_f64.min(177.0 - 10.0));
        assert!(s.hwc_crash_detected);
    }

    // @lat: [[tests#DHW draw tracking#A moderate T1 drop caps remaining at twenty litres]]
    #[test]
    fn draw_tracking_moderate_t1_drop_caps_remaining_at_twenty_litres() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 150.0;
        s.volume_at_reset = 1000.0;
        s.drawing = true;
        s.hwc_pre_draw = 48.0;
        s.t1_pre_draw = 50.0;

        apply_draw_tracking(&mut s, &cfg, 1010.0, 49.0, 47.5);

        assert_eq!(s.remaining, 20.0);
    }

    // @lat: [[tests#DHW draw tracking#A severe T1 drop forces remaining to zero]]
    #[test]
    fn draw_tracking_severe_t1_drop_forces_remaining_to_zero() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 150.0;
        s.volume_at_reset = 1000.0;
        s.drawing = true;
        s.hwc_pre_draw = 48.0;
        s.t1_pre_draw = 50.0;

        apply_draw_tracking(&mut s, &cfg, 1010.0, 48.4, 47.5);

        assert_eq!(s.remaining, 0.0);
    }

    // @lat: [[tests#DHW draw tracking#A T1 drop exactly at one point five degrees stays on the twenty litre cap]]
    #[test]
    fn draw_tracking_t1_drop_at_one_point_five_degrees_stays_on_twenty_litre_cap() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 150.0;
        s.volume_at_reset = 1000.0;
        s.drawing = true;
        s.hwc_pre_draw = 48.0;
        s.t1_pre_draw = 50.0;

        apply_draw_tracking(&mut s, &cfg, 1010.0, 48.5, 47.5);

        assert_eq!(s.remaining, 20.0);
    }

    // @lat: [[tests#DHW draw tracking#A severe T1 drop overrides a Hwc crash cap]]
    #[test]
    fn draw_tracking_severe_t1_drop_overrides_hwc_crash_cap() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 170.0;
        s.volume_at_reset = 1000.0;
        s.drawing = true;
        s.hwc_pre_draw = 50.0;
        s.hwc_crash_detected = false;
        s.t1_pre_draw = 50.0;

        apply_draw_tracking(&mut s, &cfg, 1010.0, 48.0, 44.0);

        assert_eq!(s.remaining, 0.0);
        assert!(s.hwc_crash_detected);
    }

    // ── apply_standby_decay tests ───────────────────────────────────────

    // @lat: [[tests#DHW standby decay#No charge end time leaves state unchanged]]
    #[test]
    fn standby_decay_no_charge_end_time_is_noop() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.charge_end_time = None;
        s.effective_t1 = 50.0;
        s.charge_state = DhwChargeState::Full;

        apply_standby_decay(&mut s, &cfg);

        assert_eq!(s.effective_t1, 50.0);
        assert_eq!(s.charge_state, DhwChargeState::Full);
    }

    // @lat: [[tests#DHW standby decay#Two hour decay cools top temperature and marks standby]]
    #[test]
    fn standby_decay_reduces_effective_t1() {
        let cfg = test_cfg();
        let mut s = test_state();
        // Simulate charge ended 2 hours ago
        s.charge_end_time = Some(Instant::now() - Duration::from_secs(7200));
        s.t1_at_charge_end = 50.0;
        s.charge_state = DhwChargeState::Full;

        apply_standby_decay(&mut s, &cfg);

        // effective_t1 = 50 - 0.25 * 2 = 49.5 (approximately)
        assert!(s.effective_t1 < 50.0);
        assert!(s.effective_t1 > 49.0);
        // After 2h, should transition to Standby
        assert_eq!(s.charge_state, DhwChargeState::Standby);
    }

    // @lat: [[tests#DHW standby decay#Cooling below reduced temperature marks standby]]
    #[test]
    fn standby_decay_below_reduced_t1_sets_standby() {
        let cfg = test_cfg();
        let mut s = test_state();
        // Simulate charge ended long ago — T1 has decayed below reduced_t1 (42°C)
        s.charge_end_time = Some(Instant::now() - Duration::from_secs(3600 * 40));
        s.t1_at_charge_end = 50.0;
        s.charge_state = DhwChargeState::Full;

        apply_standby_decay(&mut s, &cfg);

        // effective_t1 = 50 - 0.25 * 40 = 40.0, which is < reduced_t1(42)
        assert!(s.effective_t1 < cfg.reduced_t1);
        assert_eq!(s.charge_state, DhwChargeState::Standby);
    }

    // @lat: [[tests#DHW standby decay#Short standby keeps full state]]
    #[test]
    fn standby_decay_under_2h_preserves_full_state() {
        let cfg = test_cfg();
        let mut s = test_state();
        // Charge ended 1 hour ago, T1 still above reduced_t1
        s.charge_end_time = Some(Instant::now() - Duration::from_secs(3600));
        s.t1_at_charge_end = 50.0;
        s.charge_state = DhwChargeState::Full;

        apply_standby_decay(&mut s, &cfg);

        // effective_t1 ≈ 49.75, still above reduced_t1(42) and under 2h
        assert!(s.effective_t1 > cfg.reduced_t1);
        assert_eq!(s.charge_state, DhwChargeState::Full);
    }

    // @lat: [[tests#DHW standby decay#Decay never overwrites active charging states]]
    #[test]
    fn standby_decay_does_not_override_charging_state() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.charge_end_time = Some(Instant::now() - Duration::from_secs(3600 * 3));
        s.t1_at_charge_end = 50.0;
        s.charge_state = DhwChargeState::ChargingBelow;

        apply_standby_decay(&mut s, &cfg);

        // >2h but charging state should be preserved by the second condition
        // Wait — the code checks: if hours > 2.0 && !matches!(charging states)
        // ChargingBelow IS matched, so the 2h rule doesn't apply
        // But the reduced_t1 check at 49.25 > 42 doesn't trigger either
        // So charging state should be preserved
        assert_eq!(s.charge_state, DhwChargeState::ChargingBelow);
    }

    // ── Motion automation tests ─────────────────────────────────────────

    // @lat: [[tests#Motion lighting automation#Dark motion turns on both motion lights and arms the timer]]
    #[tokio::test]
    async fn dark_motion_turns_on_both_motion_lights_and_arms_timer() {
        let state = test_automation_state();
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"hall_motion","payload":{"occupancy":true,"illuminance":10.0}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        let first = cmd_rx.try_recv().expect("first ON command");
        let second = cmd_rx.try_recv().expect("second ON command");
        assert_eq!(first.topic, "landing/set");
        assert_eq!(first.payload, serde_json::json!({"state": "ON"}));
        assert_eq!(second.topic, "hall/set");
        assert_eq!(second.payload, serde_json::json!({"state": "ON"}));
        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.lights_off_at.is_some());
        assert_eq!(s.illuminance.get("hall_motion"), Some(&10.0));
        drop(s);

        let zs = z2m_state.lock().await;
        assert_eq!(zs.get("hall_motion"), Some(&serde_json::json!({"occupancy": true, "illuminance": 10.0})));
    }

    // @lat: [[tests#Motion lighting automation#Motion at the darkness threshold still triggers the lights]]
    #[tokio::test]
    async fn motion_at_darkness_threshold_still_triggers_lights() {
        let state = test_automation_state();
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"landing_motion","payload":{"occupancy":true,"illuminance":15.0}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert_eq!(cmd_rx.try_recv().expect("first ON command").topic, "landing/set");
        assert_eq!(cmd_rx.try_recv().expect("second ON command").topic, "hall/set");
        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.lights_off_at.is_some());
        assert_eq!(s.illuminance.get("landing_motion"), Some(&15.0));
    }

    // @lat: [[tests#Motion lighting automation#Bright motion only refreshes cached lux and does not switch lights]]
    #[tokio::test]
    async fn bright_motion_only_refreshes_cached_lux_and_does_not_switch_lights() {
        let state = test_automation_state();
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"landing_motion","payload":{"occupancy":true,"illuminance":30.0}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.lights_off_at.is_none());
        assert_eq!(s.illuminance.get("landing_motion"), Some(&30.0));
    }

    // @lat: [[tests#Motion lighting automation#Manual off cancels the timer and suppresses retriggering]]
    #[tokio::test]
    async fn manual_off_cancels_timer_and_suppresses_retriggering() {
        let state = test_automation_state();
        {
            let mut s = state.lock().await;
            s.lights_off_at = Some(tokio::time::Instant::now() + OFF_DELAY);
        }
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"hall","payload":{"state":"OFF"}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());
        let s = state.lock().await;
        assert!(s.lights_off_at.is_none());
        assert!(s.suppressed_until.is_some());
    }

    // @lat: [[tests#Motion lighting automation#Active suppression blocks dark motion retriggering]]
    #[tokio::test]
    async fn active_suppression_blocks_dark_motion_retriggering() {
        let state = test_automation_state();
        {
            let mut s = state.lock().await;
            s.suppressed_until = Some(tokio::time::Instant::now() + Duration::from_secs(60));
        }
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"landing_motion","payload":{"occupancy":true,"illuminance":10.0}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.suppressed_until.is_some());
        assert!(s.lights_off_at.is_none());
    }

    // @lat: [[tests#Motion lighting automation#Expired suppression is cleared before a fresh dark motion trigger]]
    #[tokio::test]
    async fn expired_suppression_is_cleared_before_fresh_dark_motion_trigger() {
        let state = test_automation_state();
        {
            let mut s = state.lock().await;
            s.suppressed_until = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        }
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"landing_motion","payload":{"occupancy":true,"illuminance":10.0}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert_eq!(cmd_rx.try_recv().expect("first ON command").topic, "landing/set");
        assert_eq!(cmd_rx.try_recv().expect("second ON command").topic, "hall/set");

        let s = state.lock().await;
        assert!(s.suppressed_until.is_none());
        assert!(s.lights_off_at.is_some());
    }

    // ── Property tests ──────────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        /// Config strategy with sane ranges
        fn arb_cfg() -> impl Strategy<Value = DhwConfig> {
            (
                100.0..300.0_f64,  // full_litres
                0.1..1.0_f64,      // t1_decay_rate
                35.0..45.0_f64,    // reduced_t1
                2.0..10.0_f64,     // hwc_crash_threshold
                50.0..200.0_f64,   // vol_above_hwc
                50.0..200.0_f64,   // draw_flow_min
            )
                .prop_flat_map(
                    |(full, decay, reduced, crash, vol, flow)| {
                        // gap_dissolved must be < gap_sharp
                        (0.5..5.0_f64).prop_flat_map(move |gap_d| {
                            ((gap_d + 0.1)..10.0_f64).prop_map(move |gap_s| DhwConfig {
                                full_litres: full,
                                t1_decay_rate: decay,
                                reduced_t1: reduced,
                                hwc_crash_threshold: crash,
                                vol_above_hwc: vol,
                                draw_flow_min: flow,
                                gap_sharp: gap_s,
                                gap_dissolved: gap_d,
                                full_litres_min: 160.0,
                                full_litres_max: 220.0,
                            })
                        })
                    },
                )
        }

        proptest! {
            /// Remaining is always bounded in [0, full_litres] after no-crossover charge
            // @lat: [[tests#DHW no crossover#Remaining stays within zero and full capacity]]
            #[test]
            fn no_crossover_remaining_bounded(
                cfg in arb_cfg(),
                prior_remaining in 0.0..300.0_f64,
                t1 in 30.0..60.0_f64,
                hwc in 20.0..60.0_f64,
            ) {
                let mut s = test_state();
                s.full_litres = cfg.full_litres;
                s.remaining = prior_remaining.min(cfg.full_litres);

                apply_no_crossover_charge(&mut s, &cfg, t1, hwc);

                prop_assert!(s.remaining >= 0.0,
                    "remaining {} < 0", s.remaining);
                prop_assert!(s.remaining <= cfg.full_litres,
                    "remaining {} > full {}", s.remaining, cfg.full_litres);
            }

            /// Monotonicity: increasing gap should never increase remaining
            /// (for same prior state and config)
            // @lat: [[tests#DHW no crossover#Larger temperature gaps never increase remaining litres]]
            #[test]
            fn no_crossover_monotonic_in_gap(
                cfg in arb_cfg(),
                prior_remaining in 0.0..300.0_f64,
                t1_base in 35.0..55.0_f64,
                hwc_hi in 30.0..50.0_f64,
            ) {
                // Two gaps: small gap (hwc close to t1) and large gap (hwc further from t1)
                let hwc_lo = hwc_hi - 2.0; // larger gap
                if hwc_lo < 0.0 || hwc_hi >= t1_base {
                    return Ok(());  // skip degenerate cases
                }

                let mut s1 = test_state();
                s1.full_litres = cfg.full_litres;
                s1.remaining = prior_remaining.min(cfg.full_litres);

                let mut s2 = test_state();
                s2.full_litres = cfg.full_litres;
                s2.remaining = prior_remaining.min(cfg.full_litres);

                apply_no_crossover_charge(&mut s1, &cfg, t1_base, hwc_hi); // small gap
                apply_no_crossover_charge(&mut s2, &cfg, t1_base, hwc_lo); // large gap

                prop_assert!(s1.remaining >= s2.remaining,
                    "small gap remaining {} < large gap remaining {} (gaps: {}, {})",
                    s1.remaining, s2.remaining,
                    t1_base - hwc_hi, t1_base - hwc_lo);
            }

            /// Standby decay: effective_t1 never increases with time
            // @lat: [[tests#DHW standby decay#Effective top temperature never rises during standby]]
            #[test]
            fn standby_decay_t1_never_increases(
                cfg in arb_cfg(),
                t1_end in 40.0..55.0_f64,
                secs_elapsed in 0u64..200_000,
            ) {
                let mut s = test_state();
                s.t1_at_charge_end = t1_end;
                s.effective_t1 = t1_end;
                s.charge_end_time = Some(Instant::now() - Duration::from_secs(secs_elapsed));
                s.charge_state = DhwChargeState::Full;

                apply_standby_decay(&mut s, &cfg);

                prop_assert!(s.effective_t1 <= t1_end,
                    "effective_t1 {} > t1_at_charge_end {}", s.effective_t1, t1_end);
            }
        }
    }
}

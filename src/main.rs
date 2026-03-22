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
    /// Last known illuminance per motion sensor
    illuminance: std::collections::HashMap<String, f64>,
}

/// DHW tracking state
struct DhwState {
    /// Current remaining litres
    remaining: f64,
    /// Whether the heat pump is currently charging DHW
    was_charging: bool,
    /// Whether we initiated the current charge (via boost button)
    boost_initiated: bool,
    /// Volume register at last charge completion (for tracking usage)
    volume_at_reset: f64,
}

/// Shared app state for axum handlers
#[derive(Clone)]
struct AppState {
    http_client: reqwest::Client,
    cmd_tx: broadcast::Sender<Z2mMessage>,
    z2m_state: Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    dhw_state: Arc<Mutex<DhwState>>,
}

const Z2M_WS_URL: &str = "ws://emonpi:8080/api";
const LIGHTS: &[&str] = &["landing", "hall"];
const OFF_DELAY: Duration = Duration::from_secs(60);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const HTTP_PORT: u16 = 3030;

const INFLUXDB_URL: &str = "http://localhost:8086";
const INFLUXDB_TOKEN: &str = "jPTPrwcprKfDzt8IFr7gkn6shpBy15j8hFeyjLaBIaJ0IwcgQeXJ4LtrvVBJ5aIPYuzEfeDw5e-cmtAuvZ-Xmw==";
const INFLUXDB_ORG: &str = "home";
const DHW_FULL_LITRES: f64 = 161.0;
const DHW_BOOST_PERCENT: f64 = 0.5;

/// Motion sensor config: (name, illuminance threshold)
const MOTION_SENSORS: &[(&str, f64)] = &[
    ("landing_motion", 15.0),
    ("hall_motion", 15.0),
];

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
        illuminance: std::collections::HashMap::new(),
    }));

    // Channel for sending commands to Z2M
    let (cmd_tx, _) = broadcast::channel::<Z2mMessage>(64);

    let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::<String, serde_json::Value>::new()));

    let dhw_state = Arc::new(Mutex::new(DhwState {
        remaining: 0.0, // Will be initialised from InfluxDB on first DHW poll
        was_charging: false,
        boost_initiated: false,
        volume_at_reset: 0.0,
    }));

    let app_state = AppState {
        http_client: reqwest::Client::new(),
        cmd_tx: cmd_tx.clone(),
        z2m_state: z2m_state.clone(),
        dhw_state: dhw_state.clone(),
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
        .route("/api/lights/{name}/toggle", axum::routing::post(api_light_toggle))
        .route("/api/lights", get(api_lights_state))
        .route("/api/dhw/boost", axum::routing::post(api_dhw_boost))
        .route("/api/dhw/status", get(api_dhw_status))
        .with_state(app_state);

    tokio::select! {
        _ = timer_loop(timer_state, timer_cmd_tx) => {
            error!("Timer loop exited unexpectedly");
        }
        _ = z2m_connection_loop(automation_state, cmd_tx, z2m_state) => {
            error!("Z2M connection loop exited unexpectedly");
        }
        _ = dhw_tracking_loop(dhw_state_loop, dhw_client) => {
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

#[derive(Serialize)]
struct HotWaterResponse {
    remaining_litres: f64,
    timestamp: String,
}

async fn api_hot_water(State(state): State<AppState>) -> Json<serde_json::Value> {
    let dhw = state.dhw_state.lock().await;
    Json(serde_json::json!({
        "remaining_litres": dhw.remaining,
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
        let on = zs.get(name)
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
            {
                let mut dhw = state.dhw_state.lock().await;
                dhw.boost_initiated = true;
            }
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
    let sfmode = ebusd_command("read -f -c 700 HwcSFMode").await.unwrap_or_default();
    let status = ebusd_command("read -f -c hmu Status01").await.unwrap_or_default();
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
    let t1 = query_influxdb(&state.http_client, t1_query).await
        .map(|(v, _)| v)
        .unwrap_or(0.0);

    Json(serde_json::json!({
        "ok": true,
        "charging": charging,
        "sfmode": sfmode,
        "t1": t1,
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
    background: linear-gradient(to top, #0066cc, #3399ff);
    transition: height 1s ease;
    border-radius: 0 0 7px 7px;
  }
  .hw-info { display: flex; flex-direction: column; }
  .litres { font-size: 40px; font-weight: 700; }
  .hw-label { font-size: 14px; color: #999; }
  .hw-status { font-size: 14px; font-weight: 600; margin-top: 2px; }
  .hw-status.empty { color: #ff4444; }
  .hw-status.low { color: #ffaa00; }
  .hw-status.ok { color: #44cc44; }
  .hw-status.full { color: #3399ff; }
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
    <h2>💡 Lights</h2>
    <div id="lights"></div>
  </div>

<script>
const TANK_MAX = 161;
const LIGHTS = ['landing', 'hall'];

async function updateHotWater() {
  try {
    const r = await fetch('/api/hot-water');
    const d = await r.json();
    if (!d.ok) return;
    const litres = Math.round(d.remaining_litres);
    const pct = Math.min(100, Math.max(0, (litres / TANK_MAX) * 100));
    document.getElementById('litres').textContent = litres;
    document.getElementById('water').style.height = pct + '%';
    const el = document.getElementById('hw-status');
    if (litres <= 0) { el.textContent = 'Empty'; el.className = 'hw-status empty'; }
    else if (litres < 40) { el.textContent = 'Low'; el.className = 'hw-status low'; }
    else if (litres < 150) { el.textContent = 'OK'; el.className = 'hw-status ok'; }
    else { el.textContent = 'Full'; el.className = 'hw-status full'; }
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
      info.textContent = d.return_temp + '°C';
    } else {
      btn.textContent = 'Boost';
      btn.className = 'boost-btn';
      info.textContent = d.t1 > 0 ? d.t1.toFixed(1) + '°C' : '';
    }
  } catch(e) { console.error(e); }
}

buildLights();
updateHotWater();
updateLights();
updateDhwStatus();
setInterval(updateHotWater, 30000);
setInterval(updateLights, 5000);
setInterval(updateDhwStatus, 10000);
</script>
</body>
</html>"#;

// ── DHW tracking loop ───────────────────────────────────────────────────────

async fn get_current_volume(client: &reqwest::Client) -> f64 {
    let query = r#"from(bucket: "energy")
  |> range(start: -1h)
  |> filter(fn: (r) => r._measurement == "emon" and r._field == "value" and r.field == "dhw_volume_V1")
  |> last()"#;
    query_influxdb(client, query).await.map(|(v, _)| v).unwrap_or(0.0)
}

async fn write_remaining_to_influxdb(client: &reqwest::Client, litres: f64) {
    let line = format!("dhw remaining_litres={litres}");
    let result = client
        .post(format!("{INFLUXDB_URL}/api/v2/write?org={INFLUXDB_ORG}&bucket=energy&precision=s"))
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

async fn is_charging() -> bool {
    let sfmode = ebusd_command("read -f -c 700 HwcSFMode").await.unwrap_or_default();
    let status = ebusd_command("read -f -c hmu Status01").await.unwrap_or_default();
    status.ends_with(";hwc") || sfmode == "load"
}

async fn dhw_tracking_loop(state: Arc<Mutex<DhwState>>, client: reqwest::Client) {
    // Initialise from InfluxDB
    {
        let query = r#"from(bucket: "energy")
  |> range(start: -24h)
  |> filter(fn: (r) => r._measurement == "dhw" and r._field == "remaining_litres")
  |> last()"#;
        let remaining = query_influxdb(&client, query).await.map(|(v, _)| v).unwrap_or(0.0);
        let volume = get_current_volume(&client).await;
        let charging = is_charging().await;

        let mut s = state.lock().await;
        s.remaining = remaining;
        s.volume_at_reset = volume;
        s.was_charging = charging;
        info!("DHW init: remaining={remaining:.1}L, volume={volume:.1}, charging={charging}");
    }

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        let charging = is_charging().await;
        let volume_now = get_current_volume(&client).await;

        let mut s = state.lock().await;

        if s.was_charging && !charging {
            // Charging just ended
            if s.boost_initiated {
                // Manual boost: add 50%, cap at max
                let add = DHW_FULL_LITRES * DHW_BOOST_PERCENT;
                s.remaining = (s.remaining + add).min(DHW_FULL_LITRES);
                info!("DHW boost complete: +{add:.0}L → {:.0}L", s.remaining);
                s.boost_initiated = false;
            } else {
                // Scheduled charge: full tank
                s.remaining = DHW_FULL_LITRES;
                info!("DHW charge complete: reset to {DHW_FULL_LITRES:.0}L");
            }
            s.volume_at_reset = volume_now;
            write_remaining_to_influxdb(&client, s.remaining).await;
        } else if !charging && volume_now > s.volume_at_reset {
            // Water being used (volume register increased)
            let drawn = volume_now - s.volume_at_reset;
            s.remaining = (DHW_FULL_LITRES - drawn).max(0.0).min(s.remaining);
            // Only write periodically when usage changes
            write_remaining_to_influxdb(&client, s.remaining).await;
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
                info!("Timer expired — turning OFF {LIGHTS:?}");
                s.lights_off_at = None;
                for light in LIGHTS {
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
                        if let Err(e) = w.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await {
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
            let threshold = MOTION_SENSORS.iter()
                .find(|(name, _)| *name == topic)
                .unwrap().1;
            let mut s = state.lock().await;

            if s.lights_off_at.is_none() {
                if let Some(lux) = msg.payload.get("illuminance").and_then(|v| v.as_f64()) {
                    s.illuminance.insert(topic.to_string(), lux);
                }
            }

            if let Some(occupancy) = msg.payload.get("occupancy").and_then(|v| v.as_bool()) {
                if occupancy {
                    if s.lights_off_at.is_some() {
                        s.lights_off_at = Some(Instant::now() + OFF_DELAY);
                        info!("Motion on {topic} — lights already on, reset timer");
                    } else {
                        let lux = s.illuminance.get(topic).copied().unwrap_or(0.0);
                        if lux <= threshold {
                            info!("Motion on {topic} (lux={lux}, threshold={threshold}) — turning ON {LIGHTS:?}");

                            for light in LIGHTS {
                                let on_msg = Z2mMessage {
                                    topic: format!("{light}/set"),
                                    payload: serde_json::json!({"state": "ON"}),
                                };
                                let _ = cmd_tx.send(on_msg);
                            }

                            s.lights_off_at = Some(Instant::now() + OFF_DELAY);
                            info!("Scheduled {LIGHTS:?} OFF in {OFF_DELAY:?}");
                        } else {
                            info!("Motion on {topic} (lux={lux}, threshold={threshold}) — too bright, skipping");
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

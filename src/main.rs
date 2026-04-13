use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
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
    /// Minimum sane full_litres from database autoload
    #[serde(default = "default_full_litres_min")]
    full_litres_min: f64,
    /// Maximum sane full_litres from database autoload
    #[serde(default = "default_full_litres_max")]
    full_litres_max: f64,
}

fn default_full_litres_min() -> f64 {
    160.0
}
fn default_full_litres_max() -> f64 {
    220.0
}

fn default_db_host() -> String {
    "10.0.1.230".to_string()
}
fn default_db_port() -> u16 {
    5432
}
fn default_db_dbname() -> String {
    "energy".to_string()
}
fn default_db_user() -> String {
    "energy".to_string()
}

/// PostgreSQL/TimescaleDB connection configuration
#[derive(Debug, Clone, Deserialize)]
struct DatabaseConfig {
    #[serde(default = "default_db_host")]
    host: String,
    #[serde(default = "default_db_port")]
    port: u16,
    #[serde(default = "default_db_dbname")]
    dbname: String,
    #[serde(default = "default_db_user")]
    user: String,
}

/// Name of the systemd credential containing the PostgreSQL password.
/// Provisioned via `systemd-creds encrypt` and loaded by `LoadCredentialEncrypted=`
/// in the systemd unit file.
const PG_CREDENTIAL_NAME: &str = "pgpassword";

#[async_trait]
trait PgAccess: Send + Sync {
    async fn query_f64(&self, query: &str) -> (f64, String);
    async fn write_dhw(&self, s: &DhwState);
}

#[derive(Debug)]
struct ReconnectingPg {
    config: DatabaseConfig,
}

impl ReconnectingPg {
    fn new(config: DatabaseConfig) -> Self {
        Self { config }
    }

    async fn connect(&self) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let conn_str = self.config.to_connection_string();
        let (client, connection) =
            tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await?;
        let host = self.config.host.clone();
        let port = self.config.port;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!("PostgreSQL connection error at {host}:{port}: {e}");
            }
        });
        Ok(client)
    }
}

#[async_trait]
impl PgAccess for ReconnectingPg {
    async fn query_f64(&self, query: &str) -> (f64, String) {
        match self.connect().await {
            Ok(client) => query_pg_f64(&client, query, &[]).await,
            Err(e) => {
                error!(
                    "PostgreSQL connect error at {}:{}: {e}",
                    self.config.host, self.config.port
                );
                (0.0, String::new())
            }
        }
    }

    async fn write_dhw(&self, s: &DhwState) {
        match self.connect().await {
            Ok(client) => write_dhw_to_pg(&client, s).await,
            Err(e) => error!(
                "PostgreSQL connect error at {}:{}: {e}",
                self.config.host, self.config.port
            ),
        }
    }
}

impl DatabaseConfig {
    /// Resolve the password in priority order:
    /// 1. systemd credential ($CREDENTIALS_DIRECTORY/pgpassword) — encrypted at rest
    /// 2. PGPASSWORD env var — for dev and CI
    ///
    /// No plaintext password fields exist in the config struct — secrets must
    /// not live in TOML files that could be committed or left world-readable.
    fn resolve_password(&self) -> Option<String> {
        // 1. systemd credential (encrypted at rest via systemd-creds)
        if let Ok(creds_dir) = std::env::var("CREDENTIALS_DIRECTORY") {
            let path = format!("{creds_dir}/{PG_CREDENTIAL_NAME}");
            if let Ok(pw) = std::fs::read_to_string(&path) {
                let pw = pw.trim().to_string();
                if !pw.is_empty() {
                    return Some(pw);
                }
            }
        }
        // 2. PGPASSWORD env var
        std::env::var("PGPASSWORD").ok()
    }

    fn to_connection_string(&self) -> String {
        let mut s = format!(
            "host={} port={} dbname={} user={}",
            self.host, self.port, self.dbname, self.user
        );
        if let Some(pw) = self.resolve_password() {
            s.push_str(&format!(" password={pw}"));
        }
        s
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: default_db_host(),
            port: default_db_port(),
            dbname: default_db_dbname(),
            user: default_db_user(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct HubConfig {
    dhw: DhwConfig,
    #[serde(default)]
    database: DatabaseConfig,
}

impl HubConfig {
    fn parse(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }

    fn load(path: &str) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match Self::parse(&contents) {
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
            database: DatabaseConfig::default(),
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
    /// Runtime full capacity (config value, possibly upgraded by database autoload)
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
    pg: Arc<dyn PgAccess>,
    cmd_tx: broadcast::Sender<Z2mMessage>,
    z2m_state: Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    dhw_state: Arc<Mutex<DhwState>>,
}

const Z2M_WS_URL: &str = "ws://emonpi:8080/api";
const LIGHTS: &[&str] = &["landing", "hall", "top_landing"];
const MOTION_LIGHTS: &[&str] = &["landing", "hall"];
const OFF_DELAY: Duration = Duration::from_secs(300);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const HTTP_PORT: u16 = 3030;

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

    let pg: Arc<dyn PgAccess> = Arc::new(ReconnectingPg::new(config.database.clone()));
    info!(
        "Configured PostgreSQL target at {}:{} (connect on demand)",
        config.database.host, config.database.port
    );

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
        pg: pg.clone(),
        cmd_tx: cmd_tx.clone(),
        z2m_state: z2m_state.clone(),
        dhw_state: dhw_state.clone(),
    };

    let timer_state = automation_state.clone();
    let timer_cmd_tx = cmd_tx.clone();

    let dhw_pg = pg.clone();
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
        _ = dhw_tracking_loop(dhw_state_loop, dhw_pg, config.clone()) => {
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

/// Query PostgreSQL for a single f64 value. Returns `(0.0, "")` on any error
/// or empty result — preserving the safety-critical zero-default fallback
/// contract so the DHW model starts from zeros and recovers naturally.
async fn query_pg_f64<C>(
    pg: &C,
    query: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> (f64, String)
where
    C: tokio_postgres::GenericClient + Sync,
{
    match pg.query_opt(query, params).await {
        Ok(Some(row)) => {
            let value: f64 = row.try_get(0).unwrap_or(0.0);
            let timestamp: String = row
                .try_get::<_, chrono::DateTime<chrono::Utc>>(1)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            (value, timestamp)
        }
        Ok(None) => (0.0, String::new()),
        Err(e) => {
            error!("PostgreSQL query error: {e}");
            (0.0, String::new())
        }
    }
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

fn parse_status01(status: &str, sfmode: &str) -> (bool, f64) {
    // Status01 format: "flow;return;outside;dhw;storage;pumpstate"
    // pumpstate: off/on/overrun/hwc
    let charging = status.ends_with(";hwc") || sfmode == "load";
    let return_temp = status
        .split(';')
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    (charging, return_temp)
}

async fn api_dhw_boost(State(_state): State<AppState>) -> Json<serde_json::Value> {
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
    let (charging, return_temp) = parse_status01(&status, &sfmode);
    let target_temp: f64 = ebusd_command("read -f -c 700 HwcTempDesired")
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or(0.0);

    // T1 (hot out) from emondhw Multical via PostgreSQL
    let t1 = state
        .pg
        .query_f64(
            "SELECT dhw_t1, time FROM multical \
             WHERE time >= now() - interval '1 hour' \
             ORDER BY time DESC LIMIT 1",
        )
        .await
        .0;

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

fn heating_proxy_json(
    result: Result<serde_json::Value, String>,
    include_ok_false: bool,
) -> serde_json::Value {
    match result {
        Ok(value) => value,
        Err(error) => {
            if include_ok_false {
                serde_json::json!({"ok": false, "error": error})
            } else {
                serde_json::json!({"error": error})
            }
        }
    }
}

async fn api_heating_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let result = match state
        .http_client
        .get(format!("{HEATING_MVP_URL}/status"))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    Json(heating_proxy_json(result, false))
}

async fn api_heating_mode(
    State(state): State<AppState>,
    axum::extract::Path(mode): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let url = format!("{HEATING_MVP_URL}/mode/{mode}");
    let result = match state.http_client.post(&url).send().await {
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    Json(heating_proxy_json(result, true))
}

async fn api_heating_away(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let result = match state
        .http_client
        .post(format!("{HEATING_MVP_URL}/mode/away"))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    Json(heating_proxy_json(result, true))
}

async fn api_heating_kill(State(state): State<AppState>) -> Json<serde_json::Value> {
    let result = match state
        .http_client
        .post(format!("{HEATING_MVP_URL}/kill"))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    Json(heating_proxy_json(result, true))
}

// ── DHW tracking loop (physics-based model) ────────────────────────────────

async fn get_current_volume(pg: &dyn PgAccess) -> f64 {
    pg.query_f64(
        "SELECT dhw_volume_v1, time FROM multical \
         WHERE time >= now() - interval '1 hour' \
         ORDER BY time DESC LIMIT 1",
    )
    .await
    .0
}

async fn get_current_t1(pg: &dyn PgAccess) -> f64 {
    pg.query_f64(
        "SELECT dhw_t1, time FROM multical \
         WHERE time >= now() - interval '1 hour' \
         ORDER BY time DESC LIMIT 1",
    )
    .await
    .0
}

async fn get_current_dhw_flow(pg: &dyn PgAccess) -> f64 {
    pg.query_f64(
        "SELECT dhw_flow, time FROM multical \
         WHERE time >= now() - interval '5 minutes' \
         ORDER BY time DESC LIMIT 1",
    )
    .await
    .0
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

#[derive(Debug, Clone, PartialEq)]
struct DhwWriteRow {
    remaining_litres: f64,
    model_version: i32,
    t1: f64,
    hwc_storage: f64,
    effective_t1: f64,
    charge_state: String,
    crossover: bool,
    bottom_zone_hot: bool,
}

fn dhw_write_row(s: &DhwState) -> DhwWriteRow {
    DhwWriteRow {
        remaining_litres: s.remaining,
        model_version: 2,
        t1: s.current_t1,
        hwc_storage: s.current_hwc,
        effective_t1: s.effective_t1,
        charge_state: s.charge_state.to_string(),
        crossover: s.crossover_achieved,
        // Bottom zone is "hot" when HwcStorage is significantly above mains (~20°C)
        bottom_zone_hot: s.current_hwc > 30.0,
    }
}

/// Write current DHW state to PostgreSQL. Fire-and-forget: logs errors but
/// never propagates them — a flaky database must not stop the DHW automation loop.
async fn write_dhw_to_pg<C>(pg: &C, s: &DhwState)
where
    C: tokio_postgres::GenericClient + Sync,
{
    let row = dhw_write_row(s);
    let result = pg
        .execute(
            "INSERT INTO dhw (time, remaining_litres, model_version, t1, hwc_storage, \
             effective_t1, charge_state, crossover, bottom_zone_hot) \
             VALUES (now(), $1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &row.remaining_litres,
                &row.model_version,
                &row.t1,
                &row.hwc_storage,
                &row.effective_t1,
                &row.charge_state,
                &row.crossover,
                &row.bottom_zone_hot,
            ],
        )
        .await;
    match result {
        Ok(_) => {}
        Err(e) => error!("PostgreSQL write error: {e}"),
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
    info!("DHW no-crossover charge ended: T1={t1_now:.1}, HwcS={hwc_now:.1}, gap={gap:.1}");

    if gap < cfg.gap_dissolved {
        // Thermocline dissolved — effectively full but at a lower temperature
        s.remaining = full;
        s.charge_state = DhwChargeState::Full;
        info!(
            "  Gap <{:.1}°C → thermocline dissolved, full at lower temp",
            cfg.gap_dissolved
        );
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
        info!("  Gap {gap:.1}°C → interpolated frac={frac:.2}, remaining={interpolated:.0}L");
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

/// Decide the runtime full_litres given the config value and a recommended
/// value from the database.  Returns `None` when the recommendation is
/// outside sane bounds or non-positive (caller should keep the current value).
fn apply_autoload(current: f64, recommended: f64, min: f64, max: f64) -> Option<f64> {
    if recommended >= min && recommended <= max {
        Some(current.max(recommended))
    } else {
        None
    }
}

/// Reconstruct the Multical volume register reading that corresponded to
/// the last charge-completion reset, given current sensor and persisted state.
fn reconstruct_volume_at_reset(full_litres: f64, remaining: f64, volume_now: f64) -> f64 {
    let already_drawn = (full_litres - remaining).max(0.0);
    volume_now - already_drawn
}

#[derive(Debug, Clone, Copy)]
struct LiveDhwTick {
    charging: bool,
    volume_now: f64,
    t1_now: f64,
    hwc_now: f64,
    dhw_flow: f64,
}

fn apply_startup_recovery(
    s: &mut DhwState,
    remaining: f64,
    volume_now: f64,
    charging: bool,
    t1_now: f64,
    hwc_now: f64,
) {
    s.remaining = remaining;
    s.volume_at_reset = reconstruct_volume_at_reset(s.full_litres, remaining, volume_now);
    s.was_charging = charging;
    s.current_t1 = t1_now;
    s.current_hwc = hwc_now;
    s.effective_t1 = t1_now;
    s.t1_at_charge_end = t1_now;
    if charging {
        s.t1_at_charge_start = t1_now;
        s.charge_state = DhwChargeState::ChargingBelow;
    }
}

fn apply_live_dhw_tick(s: &mut DhwState, cfg: &DhwConfig, tick: LiveDhwTick) -> bool {
    let mut should_write = false;

    s.current_t1 = tick.t1_now;
    s.current_hwc = tick.hwc_now;

    if tick.charging && !s.was_charging {
        s.t1_at_charge_start = tick.t1_now;
        s.crossover_achieved = false;
        s.charge_state = DhwChargeState::ChargingBelow;
    }

    if tick.charging && !s.crossover_achieved && tick.hwc_now >= s.t1_at_charge_start {
        s.crossover_achieved = true;
        s.charge_state = DhwChargeState::ChargingUniform;
    }

    if s.was_charging && !tick.charging {
        apply_charge_completion(s, cfg, tick.t1_now, tick.hwc_now);
        s.volume_at_reset = tick.volume_now;
        s.hwc_crash_detected = false;
        should_write = true;
    }

    let is_drawing = tick.dhw_flow > cfg.draw_flow_min;

    if is_drawing && !s.drawing {
        s.drawing = true;
        s.hwc_pre_draw = tick.hwc_now;
        s.t1_pre_draw = tick.t1_now;
        s.hwc_crash_detected = false;
    }

    if tick.volume_now > s.volume_at_reset {
        apply_draw_tracking(s, cfg, tick.volume_now, tick.t1_now, tick.hwc_now);
        should_write = true;
    }

    if s.drawing && !is_drawing {
        s.drawing = false;
        should_write = true;
    }

    if !tick.charging && !s.drawing {
        apply_standby_decay(s, cfg);
    }

    s.was_charging = tick.charging;
    should_write
}

async fn dhw_tracking_loop(
    state: Arc<Mutex<DhwState>>,
    pg: Arc<dyn PgAccess>,
    config: Arc<HubConfig>,
) {
    let cfg = &config.dhw;

    // Autoload recommended capacity from database (written by dhw-inflection-detector.py)
    {
        let (recommended, _) = pg
            .query_f64(
                "SELECT recommended_full_litres, time FROM dhw_capacity \
                 WHERE time >= now() - interval '90 days' \
                 ORDER BY time DESC LIMIT 1",
            )
            .await;
        if recommended > 0.0 {
            let mut s = state.lock().await;
            if let Some(new_full) = apply_autoload(
                s.full_litres,
                recommended,
                cfg.full_litres_min,
                cfg.full_litres_max,
            ) {
                let prev = s.full_litres;
                s.full_litres = new_full;
                info!(
                    "DHW autoload: recommended={recommended:.0}L, \
                     config={prev:.0}L → using {:.0}L",
                    s.full_litres
                );
            } else {
                warn!(
                    "DHW autoload: recommended={recommended:.0}L outside sane range \
                     [{:.0}, {:.0}], ignoring",
                    cfg.full_litres_min, cfg.full_litres_max
                );
            }
        }
    }

    // Initialise remaining from database
    {
        let (remaining, _) = pg
            .query_f64(
                "SELECT remaining_litres, time FROM dhw \
                 WHERE time >= now() - interval '24 hours' \
                 ORDER BY time DESC LIMIT 1",
            )
            .await;
        let volume = get_current_volume(pg.as_ref()).await;
        let charging = is_charging().await;
        let t1 = get_current_t1(pg.as_ref()).await;
        let hwc = get_hwc_storage_temp().await;

        let mut s = state.lock().await;
        apply_startup_recovery(&mut s, remaining, volume, charging, t1, hwc);
        info!(
            "DHW init: remaining={remaining:.1}L, full={:.0}L, volume={volume:.1}, \
             T1={t1:.1}, HwcS={hwc:.1}, charging={charging}",
            s.full_litres
        );
    }

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        let tick = LiveDhwTick {
            charging: is_charging().await,
            volume_now: get_current_volume(pg.as_ref()).await,
            t1_now: get_current_t1(pg.as_ref()).await,
            hwc_now: get_hwc_storage_temp().await,
            dhw_flow: get_current_dhw_flow(pg.as_ref()).await,
        };

        let mut s = state.lock().await;
        let should_write = apply_live_dhw_tick(&mut s, cfg, tick);
        if should_write {
            pg.write_dhw(&s).await;
        }
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
    use std::{
        sync::{Mutex as StdMutex, OnceLock},
        time::{Duration, Instant},
    };

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

    fn env_lock() -> &'static StdMutex<()> {
        static ENV_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| StdMutex::new(()))
    }

    fn with_env_var_removed<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let original = std::env::var(name).ok();
        std::env::remove_var(name);
        let result = f();
        match original {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        result
    }

    fn with_password_env<T>(
        credentials_directory: Option<&str>,
        pgpassword: Option<&str>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let original_credentials_directory = std::env::var("CREDENTIALS_DIRECTORY").ok();
        let original_pgpassword = std::env::var("PGPASSWORD").ok();

        match credentials_directory {
            Some(value) => std::env::set_var("CREDENTIALS_DIRECTORY", value),
            None => std::env::remove_var("CREDENTIALS_DIRECTORY"),
        }
        match pgpassword {
            Some(value) => std::env::set_var("PGPASSWORD", value),
            None => std::env::remove_var("PGPASSWORD"),
        }

        let result = f();

        match original_credentials_directory {
            Some(value) => std::env::set_var("CREDENTIALS_DIRECTORY", value),
            None => std::env::remove_var("CREDENTIALS_DIRECTORY"),
        }
        match original_pgpassword {
            Some(value) => std::env::set_var("PGPASSWORD", value),
            None => std::env::remove_var("PGPASSWORD"),
        }

        result
    }

    /// Create a PG client whose connection task is immediately dropped.
    /// Every query will fail — perfect for testing zero-default fallback
    /// and fire-and-forget write contracts.
    async fn dead_pg_client() -> Arc<tokio_postgres::Client> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port for PG mock");
        let addr = listener.local_addr().expect("get local addr");
        // Spawn a minimal PG handshake acceptor so connect() succeeds
        let accept_handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.expect("accept PG connection");
            // Read the startup message
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            // Send AuthenticationOk + ReadyForQuery
            let auth_ok: &[u8] = &[b'R', 0, 0, 0, 8, 0, 0, 0, 0]; // AuthenticationOk
            let ready: &[u8] = &[b'Z', 0, 0, 0, 5, b'I']; // ReadyForQuery(Idle)
            stream.write_all(auth_ok).await.expect("send auth ok");
            stream.write_all(ready).await.expect("send ready");
            // Drop immediately — connection dies, all future queries fail
            drop(stream);
        });
        let (client, connection) = tokio_postgres::connect(
            &format!("host=127.0.0.1 port={} user=test dbname=test", addr.port()),
            tokio_postgres::NoTls,
        )
        .await
        .expect("connect to mock PG");
        // Spawn connection but it will die when the mock drops
        tokio::spawn(connection);
        // Wait for the mock acceptor to finish
        let _ = accept_handle.await;
        // Small delay so the connection task notices the drop
        tokio::time::sleep(Duration::from_millis(10)).await;
        Arc::new(client)
    }

    #[derive(Default)]
    struct FakePg {
        query_results: StdMutex<std::collections::HashMap<String, (f64, String)>>,
        queries: StdMutex<Vec<String>>,
    }

    impl FakePg {
        fn with_query_result(self, query: &str, value: f64, timestamp: &str) -> Self {
            self.query_results
                .lock()
                .expect("fake pg query results mutex")
                .insert(query.to_string(), (value, timestamp.to_string()));
            self
        }

        fn recorded_queries(&self) -> Vec<String> {
            self.queries
                .lock()
                .expect("fake pg queries mutex")
                .clone()
        }
    }

    #[async_trait]
    impl PgAccess for FakePg {
        async fn query_f64(&self, query: &str) -> (f64, String) {
            self.queries
                .lock()
                .expect("fake pg queries mutex")
                .push(query.to_string());
            self.query_results
                .lock()
                .expect("fake pg query results mutex")
                .get(query)
                .cloned()
                .unwrap_or((0.0, String::new()))
        }

        async fn write_dhw(&self, _s: &DhwState) {}
    }

    fn test_app_state(pg: Arc<dyn PgAccess>) -> AppState {
        let (cmd_tx, _) = broadcast::channel(8);
        AppState {
            http_client: reqwest::Client::new(),
            pg,
            cmd_tx,
            z2m_state: Arc::new(Mutex::new(std::collections::HashMap::new())),
            dhw_state: Arc::new(Mutex::new(test_state())),
        }
    }

    fn dead_test_pg() -> Arc<dyn PgAccess> {
        Arc::new(FakePg::default())
    }

    fn unreachable_pg_config() -> DatabaseConfig {
        let addr = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("bind ephemeral port for unreachable pg config")
            .local_addr()
            .expect("read unreachable pg config addr");
        DatabaseConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            dbname: "test".to_string(),
            user: "test".to_string(),
        }
    }

    fn heating_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn dhw_http_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    async fn spawn_heating_test_server(
        requests: Arc<std::sync::Mutex<Vec<(String, Option<serde_json::Value>)>>>,
    ) -> tokio::task::JoinHandle<()> {
        let app = axum::Router::new()
            .route(
                "/status",
                axum::routing::get({
                    let requests = requests.clone();
                    move || {
                        let requests = requests.clone();
                        async move {
                            requests
                                .lock()
                                .expect("recorded requests mutex")
                                .push(("GET /status".to_string(), None));
                            Json(serde_json::json!({"ok": true, "status": "idle"}))
                        }
                    }
                }),
            )
            .route(
                "/mode/{mode}",
                axum::routing::post({
                    let requests = requests.clone();
                    move |axum::extract::Path(mode): axum::extract::Path<String>| {
                        let requests = requests.clone();
                        async move {
                            requests
                                .lock()
                                .expect("recorded requests mutex")
                                .push((format!("POST /mode/{mode}"), None));
                            Json(serde_json::json!({"ok": true, "mode": mode}))
                        }
                    }
                }),
            )
            .route(
                "/kill",
                axum::routing::post({
                    let requests = requests.clone();
                    move || {
                        let requests = requests.clone();
                        async move {
                            requests
                                .lock()
                                .expect("recorded requests mutex")
                                .push(("POST /kill".to_string(), None));
                            Json(serde_json::json!({"ok": true, "killed": true}))
                        }
                    }
                }),
            );
        let app = app.route(
            "/mode/away",
            axum::routing::post({
                let requests = requests.clone();
                move |Json(body): Json<serde_json::Value>| {
                    let requests = requests.clone();
                    async move {
                        requests
                            .lock()
                            .expect("recorded requests mutex")
                            .push(("POST /mode/away".to_string(), Some(body.clone())));
                        Json(serde_json::json!({"ok": true, "until": body["until"].clone()}))
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 3031))
            .await
            .expect("bind heating test server");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("run heating test server");
        })
    }

    async fn spawn_heating_invalid_json_server(
        requests: Arc<std::sync::Mutex<Vec<(String, Option<serde_json::Value>)>>>,
    ) -> tokio::task::JoinHandle<()> {
        use axum::{
            body::Body,
            http::{Response, StatusCode},
            response::IntoResponse,
        };

        let app = axum::Router::new()
            .route(
                "/status",
                axum::routing::get({
                    let requests = requests.clone();
                    move || {
                        let requests = requests.clone();
                        async move {
                            requests
                                .lock()
                                .expect("recorded requests mutex")
                                .push(("GET /status".to_string(), None));
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Body::from("not-json"))
                                .expect("build invalid json response")
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/mode/{mode}",
                axum::routing::post({
                    let requests = requests.clone();
                    move |axum::extract::Path(mode): axum::extract::Path<String>| {
                        let requests = requests.clone();
                        async move {
                            requests
                                .lock()
                                .expect("recorded requests mutex")
                                .push((format!("POST /mode/{mode}"), None));
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Body::from("not-json"))
                                .expect("build invalid json response")
                                .into_response()
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 3031))
            .await
            .expect("bind heating invalid json test server");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("run heating invalid json test server");
        })
    }

    async fn spawn_ebusd_test_server(
        responses: Arc<std::collections::HashMap<String, String>>,
        commands: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 8888))
            .await
            .expect("bind ebusd test server");
        let expected_connections = responses.len();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            for _ in 0..expected_connections {
                let (mut stream, _) = listener.accept().await.expect("accept ebusd client");
                let mut cmd = String::new();
                stream
                    .read_to_string(&mut cmd)
                    .await
                    .expect("read ebusd command");
                let cmd = cmd.trim().to_string();
                commands
                    .lock()
                    .expect("recorded commands mutex")
                    .push(cmd.clone());
                let response = responses.get(&cmd).cloned().unwrap_or_default();
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write ebusd response");
            }
        })
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

    // ── Config and eBUS helper tests ─────────────────────────────────────

    // @lat: [[tests#Config loading#Missing config file falls back to built in defaults]]
    #[test]
    fn missing_config_file_falls_back_to_built_in_defaults() {
        let missing = format!(
            "/tmp/z2m-hub-missing-{}-{}.toml",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );

        let cfg = HubConfig::load(&missing);

        assert_eq!(cfg.dhw.full_litres, 177.0);
        assert_eq!(cfg.dhw.t1_decay_rate, 0.25);
        assert_eq!(cfg.dhw.full_litres_min, 160.0);
        assert_eq!(cfg.dhw.full_litres_max, 220.0);
    }

    // @lat: [[tests#Config loading#Partial config uses serde defaults for sane bounds]]
    #[test]
    fn partial_config_uses_serde_defaults_for_sane_bounds() {
        let cfg = HubConfig::parse(
            r#"[dhw]
full_litres = 190.0
t1_decay_rate = 0.4
reduced_t1 = 41.5
hwc_crash_threshold = 6.0
vol_above_hwc = 150.0
draw_flow_min = 120.0
gap_sharp = 4.0
gap_dissolved = 2.0
"#,
        )
        .expect("config should parse");

        assert_eq!(cfg.dhw.full_litres, 190.0);
        assert_eq!(cfg.dhw.full_litres_min, 160.0);
        assert_eq!(cfg.dhw.full_litres_max, 220.0);
    }

    // @lat: [[tests#Config loading#Invalid config falls back to built in defaults]]
    #[test]
    fn invalid_config_falls_back_to_built_in_defaults() {
        let path = format!(
            "/tmp/z2m-hub-invalid-{}-{}.toml",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );
        std::fs::write(&path, "not = [valid toml").expect("write invalid config");

        let cfg = HubConfig::load(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(cfg.dhw.full_litres, 177.0);
        assert_eq!(cfg.dhw.gap_dissolved, 1.5);
        assert_eq!(cfg.dhw.full_litres_min, 160.0);
        assert_eq!(cfg.dhw.full_litres_max, 220.0);
    }

    // ── Password resolution order ──────────────────────────────────────

    // @lat: [[tests#Password resolution#Systemd credential is used when available]]
    #[test]
    fn systemd_credential_is_used_when_available() {
        with_env_var_removed("PGPASSWORD", || {
            let dir = format!("/tmp/z2m-creds-{}", std::process::id());
            std::fs::create_dir_all(&dir).expect("create temp creds dir");
            std::fs::write(format!("{dir}/pgpassword"), "cred-secret\n").expect("write cred file");

            std::env::set_var("CREDENTIALS_DIRECTORY", &dir);

            let cfg = DatabaseConfig::default();
            let pw = cfg.resolve_password();

            std::env::remove_var("CREDENTIALS_DIRECTORY");
            let _ = std::fs::remove_dir_all(&dir);

            assert_eq!(pw.as_deref(), Some("cred-secret"));
        });
    }

    // @lat: [[tests#Password resolution#Systemd credential takes precedence over PGPASSWORD]]
    #[test]
    fn systemd_credential_takes_precedence_over_pgpassword() {
        let dir = format!("/tmp/z2m-creds-priority-{}", std::process::id());
        std::fs::create_dir_all(&dir).expect("create temp creds dir");
        std::fs::write(format!("{dir}/pgpassword"), "cred-secret\n").expect("write cred file");

        with_password_env(Some(&dir), Some("env-secret"), || {
            let cfg = DatabaseConfig::default();
            let pw = cfg.resolve_password();
            assert_eq!(pw.as_deref(), Some("cred-secret"));
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    // @lat: [[tests#Password resolution#Connection string includes resolved password]]
    #[test]
    fn connection_string_includes_resolved_password() {
        with_env_var_removed("PGPASSWORD", || {
            let dir = format!("/tmp/z2m-creds-conn-{}", std::process::id());
            std::fs::create_dir_all(&dir).expect("create temp creds dir");
            std::fs::write(format!("{dir}/pgpassword"), "test-pw").expect("write cred file");

            std::env::set_var("CREDENTIALS_DIRECTORY", &dir);

            let cfg = DatabaseConfig::default();
            let conn = cfg.to_connection_string();

            std::env::remove_var("CREDENTIALS_DIRECTORY");
            let _ = std::fs::remove_dir_all(&dir);

            assert!(
                conn.contains("password=test-pw"),
                "conn string must include password: {conn}"
            );
        });
    }

    // @lat: [[tests#Password resolution#Blank systemd credential falls back to PGPASSWORD]]
    #[test]
    fn blank_systemd_credential_falls_back_to_pgpassword() {
        let dir = format!("/tmp/z2m-creds-blank-{}", std::process::id());
        std::fs::create_dir_all(&dir).expect("create temp creds dir");
        std::fs::write(format!("{dir}/pgpassword"), "   \n").expect("write blank cred file");

        with_password_env(Some(&dir), Some("env-secret"), || {
            let cfg = DatabaseConfig::default();
            let pw = cfg.resolve_password();
            assert_eq!(pw.as_deref(), Some("env-secret"));
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    // @lat: [[tests#Password resolution#Connection string omits password when none resolved]]
    #[test]
    fn connection_string_omits_password_when_none_resolved() {
        with_env_var_removed("PGPASSWORD", || {
            std::env::remove_var("CREDENTIALS_DIRECTORY");

            let cfg = DatabaseConfig::default();
            let conn = cfg.to_connection_string();
            assert!(
                !conn.contains("password"),
                "conn string must not include password field: {conn}"
            );
        });
    }

    // @lat: [[tests#eBUS interface#Status01 hwc suffix marks charging]]
    #[test]
    fn status01_hwc_suffix_marks_charging() {
        let (charging, return_temp) = parse_status01("50.0;37.5;8.0;55.0;49.0;hwc", "auto");

        assert!(charging);
        assert_eq!(return_temp, 37.5);
    }

    // @lat: [[tests#eBUS interface#Sfmode load marks charging without hwc suffix]]
    #[test]
    fn sfmode_load_marks_charging_without_hwc_suffix() {
        let (charging, return_temp) = parse_status01("50.0;36.0;8.0;55.0;49.0;off", "load");

        assert!(charging);
        assert_eq!(return_temp, 36.0);
    }

    // @lat: [[tests#eBUS interface#Malformed Status01 falls back to zero return temperature]]
    #[test]
    fn malformed_status01_falls_back_to_zero_return_temperature() {
        let (charging, return_temp) = parse_status01("garbage", "auto");

        assert!(!charging);
        assert_eq!(return_temp, 0.0);
    }

    // @lat: [[tests#eBUS interface#Hwc storage helper parses numeric replies and defaults to zero]]
    #[tokio::test]
    async fn hwc_storage_helper_parses_numeric_reply_and_defaults_to_zero() {
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([(
            "read -f -c 700 HwcStorageTemp".to_string(),
            "47.25".to_string(),
        )]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;

        let temp = get_hwc_storage_temp().await;

        ebusd.await.expect("ebusd task");
        assert_eq!(temp, 47.25);
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &["read -f -c 700 HwcStorageTemp".to_string()]
        );

        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([(
            "read -f -c 700 HwcStorageTemp".to_string(),
            "not-a-number".to_string(),
        )]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;

        let temp = get_hwc_storage_temp().await;

        ebusd.await.expect("ebusd task");
        assert_eq!(temp, 0.0);
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &["read -f -c 700 HwcStorageTemp".to_string()]
        );
    }

    // @lat: [[tests#eBUS interface#Charging helper treats either sfmode load or Status01 hwc as charging]]
    #[tokio::test]
    async fn charging_helper_treats_either_sfmode_load_or_status01_hwc_as_charging() {
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([
            (
                "read -f -c 700 HwcSFMode".to_string(),
                "auto".to_string(),
            ),
            (
                "read -f -c hmu Status01".to_string(),
                "50.0;37.5;8.0;55.0;49.0;hwc".to_string(),
            ),
        ]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;

        assert!(is_charging().await);

        ebusd.await.expect("ebusd task");
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &[
                "read -f -c 700 HwcSFMode".to_string(),
                "read -f -c hmu Status01".to_string(),
            ]
        );

        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([
            (
                "read -f -c 700 HwcSFMode".to_string(),
                "idle".to_string(),
            ),
            (
                "read -f -c hmu Status01".to_string(),
                "50.0;37.5;8.0;55.0;49.0;off".to_string(),
            ),
        ]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;

        assert!(!is_charging().await);

        ebusd.await.expect("ebusd task");
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &[
                "read -f -c 700 HwcSFMode".to_string(),
                "read -f -c hmu Status01".to_string(),
            ]
        );
    }

    // ── PostgreSQL write-row mapping ─────────────────────────────────

    // @lat: [[tests#PostgreSQL interface#Write row maps all dhw columns from state]]
    #[test]
    fn dhw_write_row_maps_all_dhw_columns_from_state() {
        let mut s = test_state();
        s.remaining = 134.5;
        s.current_t1 = 51.23;
        s.current_hwc = 47.89;
        s.effective_t1 = 49.67;
        s.charge_state = DhwChargeState::Full;
        s.crossover_achieved = true;

        let row = dhw_write_row(&s);

        assert_eq!(
            row,
            DhwWriteRow {
                remaining_litres: 134.5,
                model_version: 2,
                t1: 51.23,
                hwc_storage: 47.89,
                effective_t1: 49.67,
                charge_state: "full".to_string(),
                crossover: true,
                bottom_zone_hot: true,
            }
        );
    }

    // @lat: [[tests#PostgreSQL interface#Bottom zone hot threshold at thirty degrees]]
    #[test]
    fn bottom_zone_hot_threshold_at_thirty_degrees() {
        let mut s = test_state();

        s.current_hwc = 30.1;
        assert!(dhw_write_row(&s).bottom_zone_hot);

        s.current_hwc = 30.0;
        assert!(!dhw_write_row(&s).bottom_zone_hot);

        s.current_hwc = 20.0;
        assert!(!dhw_write_row(&s).bottom_zone_hot);
    }

    // @lat: [[tests#PostgreSQL interface#Charge state strings match dhw schema values]]
    #[test]
    fn dhw_write_row_encodes_all_charge_states_correctly() {
        let mut s = test_state();
        for (state, expected) in [
            (DhwChargeState::Full, "full"),
            (DhwChargeState::Partial, "partial"),
            (DhwChargeState::Standby, "standby"),
            (DhwChargeState::ChargingBelow, "charging_below"),
            (DhwChargeState::ChargingUniform, "charging_uniform"),
        ] {
            s.charge_state = state;
            let row = dhw_write_row(&s);
            assert_eq!(row.charge_state, expected, "wrong string for {state:?}");
        }
    }

    // @lat: [[tests#PostgreSQL interface#Query fallback returns zero defaults on transport failure]]
    #[tokio::test]
    async fn query_fallback_returns_zero_defaults_on_transport_failure() {
        let pg = dead_pg_client().await;

        let (value, timestamp) = query_pg_f64(
            pg.as_ref(),
            "SELECT dhw_t1, time FROM multical ORDER BY time DESC LIMIT 1",
            &[],
        )
        .await;

        assert_eq!(value, 0.0);
        assert_eq!(timestamp, "");
    }

    // @lat: [[tests#PostgreSQL interface#Reconnecting reader returns zero defaults when connect fails]]
    #[tokio::test]
    async fn reconnecting_reader_returns_zero_defaults_when_connect_fails() {
        let pg = ReconnectingPg::new(unreachable_pg_config());

        let (value, timestamp) = pg
            .query_f64("SELECT dhw_t1, time FROM multical ORDER BY time DESC LIMIT 1")
            .await;

        assert_eq!(value, 0.0);
        assert_eq!(timestamp, "");
    }

    // @lat: [[tests#PostgreSQL interface#Reconnecting writer ignores connect failures before a session exists]]
    #[tokio::test]
    async fn reconnecting_writer_ignores_connect_failures_before_a_session_exists() {
        let pg = ReconnectingPg::new(unreachable_pg_config());
        let s = test_state();

        pg.write_dhw(&s).await;
    }

    // @lat: [[tests#PostgreSQL interface#DHW polling helpers query the intended columns and windows]]
    #[tokio::test]
    async fn dhw_polling_helpers_query_the_intended_columns_and_windows() {
        let fake_pg = Arc::new(
            FakePg::default()
                .with_query_result(
                    "SELECT dhw_volume_v1, time FROM multical \
                     WHERE time >= now() - interval '1 hour' \
                     ORDER BY time DESC LIMIT 1",
                    123.4,
                    "2026-01-01T00:00:00Z",
                )
                .with_query_result(
                    "SELECT dhw_t1, time FROM multical \
                     WHERE time >= now() - interval '1 hour' \
                     ORDER BY time DESC LIMIT 1",
                    54.5,
                    "2026-01-01T00:00:00Z",
                )
                .with_query_result(
                    "SELECT dhw_flow, time FROM multical \
                     WHERE time >= now() - interval '5 minutes' \
                     ORDER BY time DESC LIMIT 1",
                    8.75,
                    "2026-01-01T00:00:00Z",
                ),
        );

        assert_eq!(get_current_volume(fake_pg.as_ref()).await, 123.4);
        assert_eq!(get_current_t1(fake_pg.as_ref()).await, 54.5);
        assert_eq!(get_current_dhw_flow(fake_pg.as_ref()).await, 8.75);
        assert_eq!(
            fake_pg.recorded_queries(),
            vec![
                "SELECT dhw_volume_v1, time FROM multical \
                 WHERE time >= now() - interval '1 hour' \
                 ORDER BY time DESC LIMIT 1"
                    .to_string(),
                "SELECT dhw_t1, time FROM multical \
                 WHERE time >= now() - interval '1 hour' \
                 ORDER BY time DESC LIMIT 1"
                    .to_string(),
                "SELECT dhw_flow, time FROM multical \
                 WHERE time >= now() - interval '5 minutes' \
                 ORDER BY time DESC LIMIT 1"
                    .to_string(),
            ]
        );
    }

    // @lat: [[tests#PostgreSQL interface#DHW polling helpers default to zero when PostgreSQL returns no row]]
    #[tokio::test]
    async fn dhw_polling_helpers_default_to_zero_when_postgres_returns_no_row() {
        let fake_pg = FakePg::default();

        assert_eq!(get_current_volume(&fake_pg).await, 0.0);
        assert_eq!(get_current_t1(&fake_pg).await, 0.0);
        assert_eq!(get_current_dhw_flow(&fake_pg).await, 0.0);
    }

    // @lat: [[tests#PostgreSQL interface#Write failure does not stop the caller]]
    #[tokio::test]
    async fn write_failure_does_not_stop_the_caller() {
        // Dead PG client — connection has been dropped, every query fails
        let pg = dead_pg_client().await;
        let s = test_state();

        // This must return normally — not panic, not propagate an error
        write_dhw_to_pg(pg.as_ref(), &s).await;
    }

    // @lat: [[tests#PostgreSQL interface#Write to unreachable server does not stop the caller]]
    #[tokio::test]
    async fn write_to_unreachable_server_does_not_stop_the_caller() {
        // Dead PG client — simulates unreachable server
        let pg = dead_pg_client().await;
        let s = test_state();

        // This must return normally despite the transport error
        write_dhw_to_pg(pg.as_ref(), &s).await;
    }

    // ── Autoload bounds logic (pre-migration regression) ────────────

    // @lat: [[tests#DHW autoload#Autoload applies max of config and recommended when in sane range]]
    #[test]
    fn autoload_applies_max_when_in_sane_range() {
        // recommended > current → upgrade
        assert_eq!(apply_autoload(150.0, 177.0, 100.0, 250.0), Some(177.0));
        // recommended < current → keep current
        assert_eq!(apply_autoload(177.0, 150.0, 100.0, 250.0), Some(177.0));
        // recommended == current → no change
        assert_eq!(apply_autoload(177.0, 177.0, 100.0, 250.0), Some(177.0));
    }

    // @lat: [[tests#DHW autoload#Autoload rejects values outside sane bounds]]
    #[test]
    fn autoload_rejects_outside_sane_bounds() {
        // Too high
        assert_eq!(apply_autoload(177.0, 300.0, 100.0, 250.0), None);
        // Too low
        assert_eq!(apply_autoload(177.0, 50.0, 100.0, 250.0), None);
    }

    // @lat: [[tests#DHW autoload#Autoload accepts values at exact boundaries]]
    #[test]
    fn autoload_accepts_at_exact_boundaries() {
        // At min boundary
        assert_eq!(apply_autoload(80.0, 100.0, 100.0, 250.0), Some(100.0));
        // At max boundary
        assert_eq!(apply_autoload(177.0, 250.0, 100.0, 250.0), Some(250.0));
    }

    // ── Volume-at-reset reconstruction (pre-migration regression) ───

    // @lat: [[tests#DHW startup#Volume at reset reconstructs from drawn volume]]
    #[test]
    fn volume_at_reset_reconstructs_from_drawn_volume() {
        // 66L drawn from 177L full → volume_at_reset should be 66L before current reading
        assert_eq!(reconstruct_volume_at_reset(177.0, 111.0, 1000.0), 934.0);
    }

    // @lat: [[tests#DHW startup#Volume at reset at full capacity gives current volume]]
    #[test]
    fn volume_at_reset_at_full_capacity_gives_current_volume() {
        // remaining == full → nothing drawn → reset == current register
        assert_eq!(reconstruct_volume_at_reset(177.0, 177.0, 1000.0), 1000.0);
    }

    // @lat: [[tests#DHW startup#Volume at reset clamps negative drawn to zero]]
    #[test]
    fn volume_at_reset_clamps_negative_drawn_to_zero() {
        // remaining > full (shouldn't happen but defensive) → already_drawn clamped to 0
        assert_eq!(reconstruct_volume_at_reset(177.0, 200.0, 1000.0), 1000.0);
    }

    // @lat: [[tests#DHW startup#Startup recovery hydrates cached sensors and volume offset]]
    #[test]
    fn startup_recovery_hydrates_cached_sensors_and_volume_offset() {
        let mut s = test_state();
        s.full_litres = 177.0;
        s.remaining = 5.0;
        s.volume_at_reset = 0.0;
        s.was_charging = false;
        s.current_t1 = 0.0;
        s.current_hwc = 0.0;
        s.effective_t1 = 0.0;
        s.t1_at_charge_end = 0.0;
        s.charge_state = DhwChargeState::Standby;

        apply_startup_recovery(&mut s, 111.0, 1000.0, false, 49.5, 46.0);

        assert_eq!(s.remaining, 111.0);
        assert_eq!(s.volume_at_reset, 934.0);
        assert!(!s.was_charging);
        assert_eq!(s.current_t1, 49.5);
        assert_eq!(s.current_hwc, 46.0);
        assert_eq!(s.effective_t1, 49.5);
        assert_eq!(s.t1_at_charge_end, 49.5);
        assert_eq!(s.charge_state, DhwChargeState::Standby);
    }

    // @lat: [[tests#DHW startup#Startup recovery while charging captures the charge start baseline]]
    #[test]
    fn startup_recovery_while_charging_captures_the_charge_start_baseline() {
        let mut s = test_state();
        s.t1_at_charge_start = 0.0;
        s.charge_state = DhwChargeState::Standby;

        apply_startup_recovery(&mut s, 90.0, 1200.0, true, 47.0, 43.0);

        assert!(s.was_charging);
        assert_eq!(s.t1_at_charge_start, 47.0);
        assert_eq!(s.charge_state, DhwChargeState::ChargingBelow);
    }

    // @lat: [[tests#DHW loop orchestration#Charge end resets volume and requests a write]]
    #[test]
    fn live_tick_charge_end_resets_volume_and_requests_a_write() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.was_charging = true;
        s.crossover_achieved = true;
        s.hwc_crash_detected = true;
        s.volume_at_reset = 900.0;
        s.charge_state = DhwChargeState::ChargingUniform;

        let should_write = apply_live_dhw_tick(
            &mut s,
            &cfg,
            LiveDhwTick {
                charging: false,
                volume_now: 1040.0,
                t1_now: 51.0,
                hwc_now: 49.0,
                dhw_flow: 0.0,
            },
        );

        assert!(should_write);
        assert_eq!(s.remaining, s.full_litres);
        assert_eq!(s.volume_at_reset, 1040.0);
        assert!(!s.hwc_crash_detected);
        assert_eq!(s.charge_state, DhwChargeState::Full);
        assert!(!s.was_charging);
    }

    // @lat: [[tests#DHW loop orchestration#Draw start snapshots temperatures and clears prior crash state]]
    #[test]
    fn live_tick_draw_start_snapshots_temperatures_and_clears_prior_crash_state() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.drawing = false;
        s.hwc_crash_detected = true;

        let volume_at_reset = s.volume_at_reset;
        let should_write = apply_live_dhw_tick(
            &mut s,
            &cfg,
            LiveDhwTick {
                charging: false,
                volume_now: volume_at_reset,
                t1_now: 48.5,
                hwc_now: 44.5,
                dhw_flow: cfg.draw_flow_min + 1.0,
            },
        );

        assert!(!should_write);
        assert!(s.drawing);
        assert_eq!(s.t1_pre_draw, 48.5);
        assert_eq!(s.hwc_pre_draw, 44.5);
        assert!(!s.hwc_crash_detected);
    }

    // @lat: [[tests#DHW loop orchestration#Draw end clears drawing and requests a write]]
    #[test]
    fn live_tick_draw_end_clears_drawing_and_requests_a_write() {
        let cfg = test_cfg();
        let mut s = test_state();
        s.drawing = true;
        s.volume_at_reset = 1000.0;
        s.remaining = 150.0;

        let should_write = apply_live_dhw_tick(
            &mut s,
            &cfg,
            LiveDhwTick {
                charging: false,
                volume_now: 1000.0,
                t1_now: 49.0,
                hwc_now: 47.0,
                dhw_flow: 0.0,
            },
        );

        assert!(should_write);
        assert!(!s.drawing);
    }

    // @lat: [[tests#Heating proxy#Heating proxy passes success JSON through unchanged]]
    #[test]
    fn heating_proxy_passes_success_json_through_unchanged() {
        let payload = serde_json::json!({"ok": true, "mode": "away"});

        let relayed = heating_proxy_json(Ok(payload.clone()), true);

        assert_eq!(relayed, payload);
    }

    // @lat: [[tests#Heating proxy#Heating mode style errors include ok false]]
    #[test]
    fn heating_proxy_mode_errors_include_ok_false() {
        let relayed = heating_proxy_json(Err("transport failed".to_string()), true);

        assert_eq!(
            relayed,
            serde_json::json!({"ok": false, "error": "transport failed"})
        );
    }

    // @lat: [[tests#Heating proxy#Heating status style errors omit ok false]]
    #[test]
    fn heating_proxy_status_errors_omit_ok_false() {
        let relayed = heating_proxy_json(Err("bad json".to_string()), false);

        assert_eq!(relayed, serde_json::json!({"error": "bad json"}));
    }

    // @lat: [[tests#Heating proxy#Heating status calls upstream status with GET]]
    #[tokio::test]
    async fn heating_status_calls_upstream_status_with_get() {
        let _guard = heating_test_lock().lock().await;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = spawn_heating_test_server(requests.clone()).await;
        let state = test_app_state(dead_test_pg());

        let Json(resp) = api_heating_status(State(state)).await;

        assert_eq!(resp, serde_json::json!({"ok": true, "status": "idle"}));
        assert_eq!(
            requests.lock().expect("recorded requests mutex").as_slice(),
            &[("GET /status".to_string(), None)]
        );

        server.abort();
        let _ = server.await;
    }

    // @lat: [[tests#Heating proxy#Heating status invalid JSON keeps status error shape]]
    #[tokio::test]
    async fn heating_status_invalid_json_keeps_status_error_shape() {
        let _guard = heating_test_lock().lock().await;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = spawn_heating_invalid_json_server(requests.clone()).await;
        let state = test_app_state(dead_test_pg());

        let Json(resp) = api_heating_status(State(state)).await;

        assert!(resp.get("ok").is_none(), "status errors must omit ok: {resp}");
        assert!(resp["error"].as_str().is_some(), "status error text missing: {resp}");
        assert_eq!(
            requests.lock().expect("recorded requests mutex").as_slice(),
            &[("GET /status".to_string(), None)]
        );

        server.abort();
        let _ = server.await;
    }

    // @lat: [[tests#Heating proxy#Heating mode and kill call their upstream POST endpoints]]
    #[tokio::test]
    async fn heating_mode_and_kill_call_their_upstream_post_endpoints() {
        let _guard = heating_test_lock().lock().await;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = spawn_heating_test_server(requests.clone()).await;
        let state = test_app_state(dead_test_pg());

        let Json(mode_resp) = api_heating_mode(
            State(state.clone()),
            axum::extract::Path("comfort".to_string()),
        )
        .await;
        let Json(kill_resp) = api_heating_kill(State(state)).await;

        assert_eq!(
            mode_resp,
            serde_json::json!({"ok": true, "mode": "comfort"})
        );
        assert_eq!(kill_resp, serde_json::json!({"ok": true, "killed": true}));
        assert_eq!(
            requests.lock().expect("recorded requests mutex").as_slice(),
            &[
                ("POST /mode/comfort".to_string(), None),
                ("POST /kill".to_string(), None),
            ]
        );

        server.abort();
        let _ = server.await;
    }

    // @lat: [[tests#Heating proxy#Heating away forwards request JSON body unchanged]]
    #[tokio::test]
    async fn heating_away_forwards_request_json_body_unchanged() {
        let _guard = heating_test_lock().lock().await;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = spawn_heating_test_server(requests.clone()).await;
        let state = test_app_state(dead_test_pg());
        let body = serde_json::json!({"until": "2026-04-11T18:30:00Z", "reason": "school run"});

        let Json(resp) = api_heating_away(State(state), Json(body.clone())).await;

        assert_eq!(
            resp,
            serde_json::json!({"ok": true, "until": "2026-04-11T18:30:00Z"})
        );
        assert_eq!(
            requests.lock().expect("recorded requests mutex").as_slice(),
            &[("POST /mode/away".to_string(), Some(body))]
        );

        server.abort();
        let _ = server.await;
    }

    // @lat: [[tests#Heating proxy#Heating mode invalid JSON keeps ok false error shape]]
    #[tokio::test]
    async fn heating_mode_invalid_json_keeps_ok_false_error_shape() {
        let _guard = heating_test_lock().lock().await;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = spawn_heating_invalid_json_server(requests.clone()).await;
        let state = test_app_state(dead_test_pg());

        let Json(resp) = api_heating_mode(
            State(state),
            axum::extract::Path("comfort".to_string()),
        )
        .await;

        assert_eq!(resp["ok"], serde_json::json!(false));
        assert!(resp["error"].as_str().is_some(), "mode error text missing: {resp}");
        assert_eq!(
            requests.lock().expect("recorded requests mutex").as_slice(),
            &[("POST /mode/comfort".to_string(), None)]
        );

        server.abort();
        let _ = server.await;
    }

    // ── HTTP handler tests ───────────────────────────────────────────────

    // @lat: [[tests#HTTP API#Light toggle uses cached ON state to send OFF]]
    #[tokio::test]
    async fn light_toggle_uses_cached_on_state_to_send_off() {
        let state = test_app_state(dead_test_pg());
        let mut cmd_rx = state.cmd_tx.subscribe();
        {
            let mut zs = state.z2m_state.lock().await;
            zs.insert("hall".to_string(), serde_json::json!({"state": "ON"}));
        }

        let Json(resp) = api_light_toggle(
            State(state.clone()),
            axum::extract::Path("hall".to_string()),
        )
        .await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["light"], serde_json::json!("hall"));
        assert_eq!(resp["state"], serde_json::json!("OFF"));

        let msg = cmd_rx.try_recv().expect("toggle should publish a command");
        assert_eq!(msg.topic, "hall/set");
        assert_eq!(msg.payload, serde_json::json!({"state": "OFF"}));
    }

    // @lat: [[tests#HTTP API#Light toggle assumes OFF when cache is missing]]
    #[tokio::test]
    async fn light_toggle_assumes_off_when_cache_is_missing() {
        let state = test_app_state(dead_test_pg());
        let mut cmd_rx = state.cmd_tx.subscribe();

        let Json(resp) = api_light_toggle(
            State(state.clone()),
            axum::extract::Path("landing".to_string()),
        )
        .await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["light"], serde_json::json!("landing"));
        assert_eq!(resp["state"], serde_json::json!("ON"));

        let msg = cmd_rx.try_recv().expect("toggle should publish a command");
        assert_eq!(msg.topic, "landing/set");
        assert_eq!(msg.payload, serde_json::json!({"state": "ON"}));
    }

    // @lat: [[tests#HTTP API#Light toggle treats malformed cached state as OFF]]
    #[tokio::test]
    async fn light_toggle_treats_malformed_cached_state_as_off() {
        let state = test_app_state(dead_test_pg());
        let mut cmd_rx = state.cmd_tx.subscribe();
        {
            let mut zs = state.z2m_state.lock().await;
            zs.insert("landing".to_string(), serde_json::json!({"state": true}));
        }

        let Json(resp) = api_light_toggle(
            State(state.clone()),
            axum::extract::Path("landing".to_string()),
        )
        .await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["light"], serde_json::json!("landing"));
        assert_eq!(resp["state"], serde_json::json!("ON"));

        let msg = cmd_rx.try_recv().expect("toggle should publish a command");
        assert_eq!(msg.topic, "landing/set");
        assert_eq!(msg.payload, serde_json::json!({"state": "ON"}));
    }

    // @lat: [[tests#HTTP API#Unknown light commands fail without publishing Zigbee traffic]]
    #[tokio::test]
    async fn unknown_light_commands_fail_without_publishing() {
        let state = test_app_state(dead_test_pg());
        let mut cmd_rx = state.cmd_tx.subscribe();

        let Json(on_resp) = api_light_on(
            State(state.clone()),
            axum::extract::Path("kitchen".to_string()),
        )
        .await;
        assert_eq!(
            on_resp,
            serde_json::json!({"ok": false, "error": "unknown light"})
        );

        let Json(off_resp) = api_light_off(
            State(state.clone()),
            axum::extract::Path("kitchen".to_string()),
        )
        .await;
        assert_eq!(
            off_resp,
            serde_json::json!({"ok": false, "error": "unknown light"})
        );

        let Json(toggle_resp) = api_light_toggle(
            State(state.clone()),
            axum::extract::Path("kitchen".to_string()),
        )
        .await;
        assert_eq!(
            toggle_resp,
            serde_json::json!({"ok": false, "error": "unknown light"})
        );

        assert!(cmd_rx.try_recv().is_err());
    }

    // @lat: [[tests#HTTP API#Light on and off publish the requested state for known lights]]
    #[tokio::test]
    async fn light_on_and_off_publish_requested_state_for_known_lights() {
        let state = test_app_state(dead_test_pg());
        let mut cmd_rx = state.cmd_tx.subscribe();

        let Json(on_resp) = api_light_on(
            State(state.clone()),
            axum::extract::Path("hall".to_string()),
        )
        .await;
        assert_eq!(
            on_resp,
            serde_json::json!({"ok": true, "light": "hall", "state": "ON"})
        );
        let on_msg = cmd_rx
            .try_recv()
            .expect("light on should publish a command");
        assert_eq!(on_msg.topic, "hall/set");
        assert_eq!(on_msg.payload, serde_json::json!({"state": "ON"}));

        let Json(off_resp) = api_light_off(
            State(state.clone()),
            axum::extract::Path("top_landing".to_string()),
        )
        .await;
        assert_eq!(
            off_resp,
            serde_json::json!({"ok": true, "light": "top_landing", "state": "OFF"})
        );
        let off_msg = cmd_rx
            .try_recv()
            .expect("light off should publish a command");
        assert_eq!(off_msg.topic, "top_landing/set");
        assert_eq!(off_msg.payload, serde_json::json!({"state": "OFF"}));

        assert!(cmd_rx.try_recv().is_err());
    }

    // @lat: [[tests#HTTP API#Lights state reports missing cache entries as off]]
    #[tokio::test]
    async fn lights_state_reports_missing_cache_entries_as_off() {
        let state = test_app_state(dead_test_pg());
        {
            let mut zs = state.z2m_state.lock().await;
            zs.insert("hall".to_string(), serde_json::json!({"state": "ON"}));
        }

        let Json(resp) = api_lights_state(State(state)).await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["lights"]["hall"]["on"], serde_json::json!(true));
        assert_eq!(resp["lights"]["landing"]["on"], serde_json::json!(false));
        assert_eq!(
            resp["lights"]["top_landing"]["on"],
            serde_json::json!(false)
        );
    }

    // @lat: [[tests#HTTP API#Lights state treats malformed cached state as off]]
    #[tokio::test]
    async fn lights_state_treats_malformed_cached_state_as_off() {
        let state = test_app_state(dead_test_pg());
        {
            let mut zs = state.z2m_state.lock().await;
            zs.insert("hall".to_string(), serde_json::json!({"state": true}));
            zs.insert("landing".to_string(), serde_json::json!({"brightness": 128}));
        }

        let Json(resp) = api_lights_state(State(state)).await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["lights"]["hall"]["on"], serde_json::json!(false));
        assert_eq!(resp["lights"]["landing"]["on"], serde_json::json!(false));
        assert_eq!(resp["lights"]["top_landing"]["on"], serde_json::json!(false));
    }

    // @lat: [[tests#HTTP API#Hot water endpoint returns the current DHW snapshot]]
    #[tokio::test]
    async fn hot_water_endpoint_returns_the_current_dhw_snapshot() {
        let state = test_app_state(dead_test_pg());
        {
            let mut dhw = state.dhw_state.lock().await;
            dhw.remaining = 91.5;
            dhw.full_litres = 177.0;
            dhw.effective_t1 = 47.25;
            dhw.current_t1 = 48.0;
            dhw.current_hwc = 43.5;
            dhw.charge_state = DhwChargeState::Partial;
            dhw.crossover_achieved = false;
        }

        let Json(resp) = api_hot_water(State(state)).await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["remaining_litres"], serde_json::json!(91.5));
        assert_eq!(resp["full_litres"], serde_json::json!(177.0));
        assert_eq!(resp["effective_t1"], serde_json::json!(47.25));
        assert_eq!(resp["charge_state"], serde_json::json!("partial"));
        assert_eq!(resp["crossover_achieved"], serde_json::json!(false));
        assert_eq!(resp["t1"], serde_json::json!(48.0));
        assert_eq!(resp["hwc_storage"], serde_json::json!(43.5));
    }

    // @lat: [[tests#HTTP API#DHW boost returns ok true only for done]]
    #[tokio::test]
    async fn dhw_boost_returns_ok_true_only_for_done() {
        let _guard = dhw_http_test_lock().lock().await;
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([(
            "write -c 700 HwcSFMode load".to_string(),
            "done".to_string(),
        )]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;
        let state = test_app_state(dead_test_pg());

        let Json(resp) = api_dhw_boost(State(state)).await;

        assert_eq!(resp, serde_json::json!({"ok": true}));
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &["write -c 700 HwcSFMode load".to_string()]
        );

        let _ = ebusd.await;
    }

    // @lat: [[tests#HTTP API#DHW boost unexpected replies include ok false and the reply text]]
    #[tokio::test]
    async fn dhw_boost_unexpected_replies_include_ok_false_and_reply_text() {
        let _guard = dhw_http_test_lock().lock().await;
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([(
            "write -c 700 HwcSFMode load".to_string(),
            "busy".to_string(),
        )]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;
        let state = test_app_state(dead_test_pg());

        let Json(resp) = api_dhw_boost(State(state)).await;

        assert_eq!(resp, serde_json::json!({"ok": false, "error": "busy"}));
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &["write -c 700 HwcSFMode load".to_string()]
        );

        let _ = ebusd.await;
    }

    // @lat: [[tests#HTTP API#DHW status combines ebusd and database readings into one snapshot]]
    #[tokio::test]
    async fn dhw_status_combines_ebusd_and_db_readings_into_one_snapshot() {
        let _guard = dhw_http_test_lock().lock().await;
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responses = Arc::new(std::collections::HashMap::from([
            ("read -f -c 700 HwcSFMode".to_string(), "load".to_string()),
            (
                "read -f -c hmu Status01".to_string(),
                "55.0;38.5;10.0;60.0;48.2;off".to_string(),
            ),
            (
                "read -f -c 700 HwcTempDesired".to_string(),
                "52.0".to_string(),
            ),
            (
                "read -f -c 700 HwcStorageTemp".to_string(),
                "47.25".to_string(),
            ),
        ]));
        let ebusd = spawn_ebusd_test_server(responses, commands.clone()).await;
        let pg = Arc::new(FakePg::default().with_query_result(
            "SELECT dhw_t1, time FROM multical \
             WHERE time >= now() - interval '1 hour' \
             ORDER BY time DESC LIMIT 1",
            49.75,
            "2026-04-11T11:15:00Z",
        ));
        let state = test_app_state(pg);

        let Json(resp) = api_dhw_status(State(state)).await;

        assert_eq!(resp["ok"], serde_json::json!(true));
        assert_eq!(resp["charging"], serde_json::json!(true));
        assert_eq!(resp["sfmode"], serde_json::json!("load"));
        assert_eq!(resp["t1_hot"], serde_json::json!(49.75));
        assert_eq!(resp["cylinder_temp"], serde_json::json!(47.25));
        assert_eq!(resp["return_temp"], serde_json::json!(38.5));
        assert_eq!(resp["target_temp"], serde_json::json!(52.0));
        assert_eq!(
            commands.lock().expect("recorded commands mutex").as_slice(),
            &[
                "read -f -c 700 HwcSFMode".to_string(),
                "read -f -c hmu Status01".to_string(),
                "read -f -c 700 HwcTempDesired".to_string(),
                "read -f -c 700 HwcStorageTemp".to_string(),
            ]
        );

        let _ = ebusd.await;
    }

    // @lat: [[tests#HTTP API#DHW status falls back to safe defaults when upstream reads fail]]
    #[tokio::test]
    async fn dhw_status_falls_back_to_safe_defaults_when_upstream_reads_fail() {
        let _guard = dhw_http_test_lock().lock().await;
        let state = test_app_state(dead_test_pg());

        let Json(resp) = api_dhw_status(State(state)).await;

        assert_eq!(
            resp,
            serde_json::json!({
                "ok": true,
                "charging": false,
                "sfmode": "",
                "t1_hot": 0.0,
                "cylinder_temp": 0.0,
                "return_temp": 0.0,
                "target_temp": 0.0,
            })
        );
    }

    // ── Motion automation tests ─────────────────────────────────────────

    // @lat: [[tests#HTTP API#Retained slashless Zigbee topics are cached for dashboard decisions]]
    #[tokio::test]
    async fn slashless_non_bridge_topics_are_cached_for_dashboard_decisions() {
        let state = test_automation_state();
        let (cmd_tx, _cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"top_landing","payload":{"state":"ON","brightness":180}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        let zs = z2m_state.lock().await;
        assert_eq!(
            zs.get("top_landing"),
            Some(&serde_json::json!({"state": "ON", "brightness": 180}))
        );
    }

    // @lat: [[tests#HTTP API#Bridge and nested Zigbee topics are not cached as device state]]
    #[tokio::test]
    async fn bridge_and_nested_topics_are_not_cached_as_device_state() {
        let state = test_automation_state();
        let (cmd_tx, _cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"bridge/state","payload":{"state":"online"}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;
        handle_z2m_message(
            r#"{"topic":"hall/set","payload":{"state":"ON"}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        let zs = z2m_state.lock().await;
        assert!(zs.get("bridge/state").is_none());
        assert!(zs.get("hall/set").is_none());
        assert!(zs.is_empty());
    }

    // @lat: [[tests#HTTP API#Retained slashless Zigbee topics overwrite older cached state]]
    #[tokio::test]
    async fn slashless_non_bridge_topics_overwrite_older_cached_state() {
        let state = test_automation_state();
        let (cmd_tx, _cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"top_landing","payload":{"state":"OFF","brightness":20}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;
        handle_z2m_message(
            r#"{"topic":"top_landing","payload":{"state":"ON","brightness":180}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        let zs = z2m_state.lock().await;
        assert_eq!(
            zs.get("top_landing"),
            Some(&serde_json::json!({"state": "ON", "brightness": 180}))
        );
    }

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
        assert_eq!(
            zs.get("hall_motion"),
            Some(&serde_json::json!({"occupancy": true, "illuminance": 10.0}))
        );
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

        assert_eq!(
            cmd_rx.try_recv().expect("first ON command").topic,
            "landing/set"
        );
        assert_eq!(
            cmd_rx.try_recv().expect("second ON command").topic,
            "hall/set"
        );
        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.lights_off_at.is_some());
        assert_eq!(s.illuminance.get("landing_motion"), Some(&15.0));
    }

    // @lat: [[tests#Motion lighting automation#Motion during an active timer refreshes the deadline]]
    #[tokio::test]
    async fn active_timer_motion_refreshes_the_deadline_without_duplicate_on_commands() {
        let state = test_automation_state();
        let previous_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        {
            let mut s = state.lock().await;
            s.lights_off_at = Some(previous_deadline);
            s.illuminance.insert("landing_motion".to_string(), 10.0);
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
        let refreshed_deadline = s.lights_off_at.expect("timer should stay armed");
        assert!(refreshed_deadline > previous_deadline);
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

    // @lat: [[tests#Motion lighting automation#Occupancy false refreshes cached lux without switching lights]]
    #[tokio::test]
    async fn occupancy_false_refreshes_cached_lux_without_switching_lights() {
        let state = test_automation_state();
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"landing_motion","payload":{"occupancy":false,"illuminance":7.5}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.lights_off_at.is_none());
        assert_eq!(s.illuminance.get("landing_motion"), Some(&7.5));
        drop(s);

        let zs = z2m_state.lock().await;
        assert_eq!(
            zs.get("landing_motion"),
            Some(&serde_json::json!({"occupancy": false, "illuminance": 7.5}))
        );
    }

    // @lat: [[tests#Motion lighting automation#Illuminance-only reports refresh lux without switching lights]]
    #[tokio::test]
    async fn illuminance_only_reports_refresh_lux_without_switching_lights() {
        let state = test_automation_state();
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"hall_motion","payload":{"illuminance":11.0}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert!(s.lights_off_at.is_none());
        assert_eq!(s.illuminance.get("hall_motion"), Some(&11.0));
        drop(s);

        let zs = z2m_state.lock().await;
        assert_eq!(
            zs.get("hall_motion"),
            Some(&serde_json::json!({"illuminance": 11.0}))
        );
    }

    // @lat: [[tests#Motion lighting automation#Active timer motion keeps the pre light lux sample]]
    #[tokio::test]
    async fn active_timer_motion_keeps_the_pre_light_lux_sample() {
        let state = test_automation_state();
        {
            let mut s = state.lock().await;
            s.lights_off_at = Some(tokio::time::Instant::now() + OFF_DELAY);
            s.illuminance.insert("landing_motion".to_string(), 9.0);
        }
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
        assert!(s.lights_off_at.is_some());
        assert_eq!(s.illuminance.get("landing_motion"), Some(&9.0));
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

    // @lat: [[tests#Motion lighting automation#Non motion light off does not suppress automation]]
    #[tokio::test]
    async fn non_motion_light_off_does_not_suppress_automation() {
        let state = test_automation_state();
        let original_deadline = tokio::time::Instant::now() + OFF_DELAY;
        {
            let mut s = state.lock().await;
            s.lights_off_at = Some(original_deadline);
        }
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"top_landing","payload":{"state":"OFF"}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert_eq!(s.lights_off_at, Some(original_deadline));
        assert!(s.suppressed_until.is_none());
        drop(s);

        let zs = z2m_state.lock().await;
        assert_eq!(zs.get("top_landing"), Some(&serde_json::json!({"state": "OFF"})));
    }

    // @lat: [[tests#Motion lighting automation#Motion light ON does not suppress automation]]
    #[tokio::test]
    async fn motion_light_on_does_not_suppress_automation() {
        let state = test_automation_state();
        let original_deadline = tokio::time::Instant::now() + OFF_DELAY;
        {
            let mut s = state.lock().await;
            s.lights_off_at = Some(original_deadline);
        }
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        handle_z2m_message(
            r#"{"topic":"hall","payload":{"state":"ON"}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        assert!(cmd_rx.try_recv().is_err());

        let s = state.lock().await;
        assert_eq!(s.lights_off_at, Some(original_deadline));
        assert!(s.suppressed_until.is_none());
        drop(s);

        let zs = z2m_state.lock().await;
        assert_eq!(zs.get("hall"), Some(&serde_json::json!({"state": "ON"})));
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

        assert_eq!(
            cmd_rx.try_recv().expect("first ON command").topic,
            "landing/set"
        );
        assert_eq!(
            cmd_rx.try_recv().expect("second ON command").topic,
            "hall/set"
        );

        let s = state.lock().await;
        assert!(s.suppressed_until.is_none());
        assert!(s.lights_off_at.is_some());
    }

    // @lat: [[tests#Motion lighting automation#Timer expiry off does not create manual suppression]]
    #[tokio::test(start_paused = true)]
    async fn timer_expiry_off_does_not_create_manual_suppression() {
        let state = test_automation_state();
        {
            let mut s = state.lock().await;
            s.lights_off_at = Some(tokio::time::Instant::now() + Duration::from_secs(1));
        }
        let (cmd_tx, mut cmd_rx) = broadcast::channel(8);
        let z2m_state = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let timer_task = tokio::spawn(timer_loop(state.clone(), cmd_tx.clone()));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            cmd_rx.try_recv().expect("first OFF command").topic,
            "landing/set"
        );
        assert_eq!(
            cmd_rx.try_recv().expect("second OFF command").topic,
            "hall/set"
        );
        assert!(cmd_rx.try_recv().is_err());

        {
            let s = state.lock().await;
            assert!(s.lights_off_at.is_none());
            assert!(s.suppressed_until.is_none());
        }

        handle_z2m_message(
            r#"{"topic":"hall","payload":{"state":"OFF"}}"#,
            &state,
            &cmd_tx,
            &z2m_state,
        )
        .await;

        let s = state.lock().await;
        assert!(s.lights_off_at.is_none());
        assert!(s.suppressed_until.is_none());

        timer_task.abort();
        let _ = timer_task.await;
    }

    // ── Property tests ──────────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        /// Config strategy with sane ranges
        fn arb_cfg() -> impl Strategy<Value = DhwConfig> {
            (
                100.0..300.0_f64, // full_litres
                0.1..1.0_f64,     // t1_decay_rate
                35.0..45.0_f64,   // reduced_t1
                2.0..10.0_f64,    // hwc_crash_threshold
                50.0..200.0_f64,  // vol_above_hwc
                50.0..200.0_f64,  // draw_flow_min
            )
                .prop_flat_map(|(full, decay, reduced, crash, vol, flow)| {
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
                })
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

            /// Autoload result is always >= current (capacity can only increase)
            // @lat: [[tests#DHW autoload#Autoload never decreases current capacity]]
            #[test]
            fn autoload_never_decreases_current(
                current in 50.0..300.0_f64,
                recommended in 50.0..300.0_f64,
                min in 50.0..150.0_f64,
                max_offset in 50.0..200.0_f64,
            ) {
                let max = min + max_offset;
                if let Some(result) = apply_autoload(current, recommended, min, max) {
                    prop_assert!(result >= current,
                        "autoload {} < current {}", result, current);
                }
            }

            /// Volume-at-reset increases monotonically with volume_now
            // @lat: [[tests#DHW startup#Volume at reset increases with register reading]]
            #[test]
            fn volume_at_reset_monotonic_in_volume(
                full in 100.0..300.0_f64,
                remaining in 0.0..300.0_f64,
                vol_a in 0.0..10000.0_f64,
                delta in 0.0..1000.0_f64,
            ) {
                let remaining = remaining.min(full);
                let vol_b = vol_a + delta;
                let reset_a = reconstruct_volume_at_reset(full, remaining, vol_a);
                let reset_b = reconstruct_volume_at_reset(full, remaining, vol_b);
                prop_assert!(reset_b >= reset_a,
                    "reset_b {} < reset_a {} for volumes {}, {}",
                    reset_b, reset_a, vol_a, vol_b);
            }

            /// Volume-at-reset increases monotonically with remaining (less drawn = higher reset)
            // @lat: [[tests#DHW startup#Volume at reset increases with remaining litres]]
            #[test]
            fn volume_at_reset_monotonic_in_remaining(
                full in 100.0..300.0_f64,
                rem_a in 0.0..300.0_f64,
                delta in 0.0..100.0_f64,
                volume in 0.0..10000.0_f64,
            ) {
                let rem_a = rem_a.min(full);
                let rem_b = (rem_a + delta).min(full);
                let reset_a = reconstruct_volume_at_reset(full, rem_a, volume);
                let reset_b = reconstruct_volume_at_reset(full, rem_b, volume);
                prop_assert!(reset_b >= reset_a,
                    "reset_b {} < reset_a {} for remaining {}, {}",
                    reset_b, reset_a, rem_a, rem_b);
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

    // ── Real PostgreSQL integration tests ─────────────────────────────
    //
    // Gated with #[ignore] — run with `cargo test -- --ignored` on a
    // machine that can reach TimescaleDB at 10.0.1.230:5432.

    /// Connect to the real PostgreSQL instance using z2m-hub.toml config.
    /// Panics if unreachable — these tests are #[ignore]d for exactly that reason.
    async fn real_pg_client() -> tokio_postgres::Client {
        let config = HubConfig::load(CONFIG_PATH);
        let conn_str = config.database.to_connection_string();
        let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
            .await
            .expect("connect to real PostgreSQL");
        tokio::spawn(connection);
        client
    }

    // @lat: [[tests#Real PostgreSQL integration#Row decoding returns f64 and timestamp string]]
    #[tokio::test]
    #[ignore]
    async fn pg_row_decoding_returns_f64_and_timestamp() {
        let pg = real_pg_client().await;
        let (value, ts) = query_pg_f64(
            &pg,
            "SELECT dhw_t1, time FROM multical \
             WHERE time >= now() - interval '1 hour' \
             ORDER BY time DESC LIMIT 1",
            &[],
        )
        .await;
        // If there's recent data, value should be a plausible temperature
        // and timestamp should be a non-empty RFC3339 string.
        // If no data in the last hour, both default to zero — still valid.
        assert!(
            value >= 0.0,
            "temperature must be non-negative, got {value}"
        );
        if value > 0.0 {
            assert!(!ts.is_empty(), "non-zero value must have a timestamp");
            assert!(value < 100.0, "temperature must be plausible, got {value}");
        }
    }

    // @lat: [[tests#Real PostgreSQL integration#INSERT includes explicit time column]]
    #[tokio::test]
    #[ignore]
    async fn pg_insert_includes_explicit_time_column() {
        let mut pg = real_pg_client().await;
        let tx = pg.transaction().await.expect("start transaction");

        let mut s = test_state();
        s.remaining = 111.111; // distinctive value to find our row inside this transaction
        write_dhw_to_pg(&tx, &s).await;

        // Find our specific row by its distinctive remaining value
        let row = tx
            .query_one(
                "SELECT time FROM dhw \
                 WHERE remaining_litres = 111.111 \
                 ORDER BY time DESC LIMIT 1",
                &[],
            )
            .await
            .expect("read back inserted row");

        let time: chrono::DateTime<chrono::Utc> = row.get(0);
        let age = chrono::Utc::now() - time;
        assert!(
            age.num_seconds() < 10,
            "inserted row must have a recent timestamp, age={age}"
        );

        tx.rollback().await.expect("rollback transaction");
    }

    // @lat: [[tests#Real PostgreSQL integration#INSERT column types match dhw table schema]]
    #[tokio::test]
    #[ignore]
    async fn pg_insert_column_types_match_schema() {
        let mut pg = real_pg_client().await;
        let tx = pg.transaction().await.expect("start transaction");

        let mut s = test_state();
        s.remaining = 122.222; // distinctive value to find our row inside this transaction
        write_dhw_to_pg(&tx, &s).await;

        // Find our specific row by its distinctive remaining value
        let row = tx
            .query_one(
                "SELECT time, remaining_litres, model_version, t1, hwc_storage, \
                 effective_t1, charge_state, crossover, bottom_zone_hot \
                 FROM dhw WHERE remaining_litres = 122.222 \
                 ORDER BY time DESC LIMIT 1",
                &[],
            )
            .await
            .expect("read back all columns");

        // Type assertions — these will panic if the column type doesn't match
        let _time: chrono::DateTime<chrono::Utc> = row.get(0);
        let remaining: f64 = row.get(1);
        let model_version: i32 = row.get(2);
        let t1: f64 = row.get(3);
        let hwc_storage: f64 = row.get(4);
        let effective_t1: f64 = row.get(5);
        let charge_state: String = row.get(6);
        let crossover: bool = row.get(7);
        let bottom_zone_hot: bool = row.get(8);

        assert!((remaining - 122.222).abs() < f64::EPSILON);
        assert_eq!(model_version, 2);
        assert!((t1 - s.current_t1).abs() < f64::EPSILON);
        assert!((hwc_storage - s.current_hwc).abs() < f64::EPSILON);
        assert!((effective_t1 - s.effective_t1).abs() < f64::EPSILON);
        assert_eq!(charge_state, s.charge_state.to_string());
        assert_eq!(crossover, s.crossover_achieved);
        assert_eq!(bottom_zone_hot, s.current_hwc > 30.0);

        tx.rollback().await.expect("rollback transaction");
    }

    // @lat: [[tests#Real PostgreSQL integration#Consecutive writes produce distinct rows]]
    #[tokio::test]
    #[ignore]
    async fn pg_consecutive_writes_produce_distinct_rows() {
        let mut pg = real_pg_client().await;
        let tx = pg.transaction().await.expect("start transaction");

        // Use distinctive values that no other test uses
        let mut s1 = test_state();
        s1.remaining = 133.331;
        write_dhw_to_pg(&tx, &s1).await;

        let mut s2 = test_state();
        s2.remaining = 133.332;
        write_dhw_to_pg(&tx, &s2).await;

        // Find both rows by their distinctive values
        let rows = tx
            .query(
                "SELECT time, remaining_litres FROM dhw \
                 WHERE remaining_litres IN (133.331, 133.332) \
                 ORDER BY time ASC",
                &[],
            )
            .await
            .expect("read back test rows");

        assert!(
            rows.len() >= 2,
            "two consecutive writes must produce at least 2 rows, got {}",
            rows.len()
        );

        let t1: chrono::DateTime<chrono::Utc> = rows[0].get(0);
        let t2: chrono::DateTime<chrono::Utc> = rows[rows.len() - 1].get(0);
        assert!(t2 >= t1, "second write must have equal or later timestamp");

        tx.rollback().await.expect("rollback transaction");
    }

    // @lat: [[tests#Real PostgreSQL integration#End-to-end read and write against seeded tables]]
    #[tokio::test]
    #[ignore]
    async fn pg_end_to_end_seeded_integration() {
        let mut pg = real_pg_client().await;
        let tx = pg.transaction().await.expect("start transaction");

        // Verify we can read from all three tables the service depends on
        let multical = query_pg_f64(
            &tx,
            "SELECT dhw_volume_v1, time FROM multical \
             WHERE time >= now() - interval '1 hour' \
             ORDER BY time DESC LIMIT 1",
            &[],
        )
        .await;
        assert!(multical.0 >= 0.0, "volume must be non-negative");

        let dhw = query_pg_f64(
            &tx,
            "SELECT remaining_litres, time FROM dhw \
             WHERE time >= now() - interval '24 hours' \
             ORDER BY time DESC LIMIT 1",
            &[],
        )
        .await;
        assert!(dhw.0 >= 0.0, "remaining_litres must be non-negative");

        let capacity = query_pg_f64(
            &tx,
            "SELECT recommended_full_litres, time FROM dhw_capacity \
             WHERE time >= now() - interval '90 days' \
             ORDER BY time DESC LIMIT 1",
            &[],
        )
        .await;
        assert!(
            capacity.0 >= 0.0,
            "recommended_full_litres must be non-negative"
        );

        // Write and read back — round-trip verification
        let mut s = test_state();
        s.remaining = 123.456;
        write_dhw_to_pg(&tx, &s).await;

        let (readback, ts) = query_pg_f64(
            &tx,
            "SELECT remaining_litres, time FROM dhw \
             WHERE time >= now() - interval '10 seconds' \
             ORDER BY time DESC LIMIT 1",
            &[],
        )
        .await;

        assert!(
            (readback - 123.456).abs() < f64::EPSILON,
            "round-trip remaining must match: got {readback}"
        );
        assert!(!ts.is_empty(), "round-trip must have a timestamp");

        tx.rollback().await.expect("rollback transaction");
    }
}

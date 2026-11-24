//! End-to-end tests: the whole service against PostGIS and a fake vehicle.
//!
//! Run with `cargo test -p server --features db-tests` and `DATABASE_URL` set
//! (e.g. `docker compose up -d postgis`). Each test uses its own database.
#![cfg(feature = "db-tests")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use mavlink::{AsyncConnection, AsyncMavConnection, MavHeader};
use serde_json::{Value, json};
use server::{Config, Running};
use telemetry::common::{
    GLOBAL_POSITION_INT_DATA, HEARTBEAT_DATA, MISSION_ACK_DATA, MISSION_REQUEST_INT_DATA,
    MavAutopilot, MavMessage, MavMissionResult, MavModeFlag, MavType,
};
use tiff::encoder::{TiffEncoder, colortype::Gray32Float};
use tiff::tags::Tag;
use tokio_tungstenite::tungstenite::Message;

const VEHICLE: MavHeader = MavHeader {
    system_id: 1,
    component_id: 1,
    sequence: 0,
};

// ---------------------------------------------------------------- helpers

fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for db-tests")
}

fn url_for(db: &str) -> String {
    let admin = admin_url();
    let (base, _) = admin
        .rsplit_once('/')
        .expect("DATABASE_URL needs a database name");
    format!("{base}/{db}")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

async fn admin_exec(sql: &str) {
    use sqlx::Connection;
    eprintln!("[e2e] admin connect for: {sql}");
    let mut conn = tokio::time::timeout(
        Duration::from_secs(10),
        sqlx::PgConnection::connect(&admin_url()),
    )
    .await
    .expect("admin connect timed out")
    .unwrap_or_else(|e| panic!("connecting to {}: {e}", admin_url()));
    eprintln!("[e2e] admin connected");
    sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    conn.close().await.unwrap();
}

async fn create_db(name: &str) {
    admin_exec(&format!("CREATE DATABASE {name}")).await;
}

async fn drop_db(name: &str) {
    admin_exec(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await;
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(unique_name("uav_e2e"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 3×3 DEM, 0.1° pixels covering lon 11.0–11.3, lat 48.0–48.3. Flat 500 m
/// with a 900 m hill in the centre pixel.
fn write_dem(path: &Path) {
    let mut data = vec![500.0_f32; 9];
    data[4] = 900.0;
    let file = std::fs::File::create(path).unwrap();
    let mut enc = TiffEncoder::new(file).unwrap();
    let mut img = enc.new_image::<Gray32Float>(3, 3).unwrap();
    let tags = img.encoder();
    tags.write_tag(Tag::ModelPixelScaleTag, &[0.1, 0.1, 0.0][..])
        .unwrap();
    tags.write_tag(Tag::ModelTiepointTag, &[0.0, 0.0, 0.0, 11.0, 48.3, 0.0][..])
        .unwrap();
    let keys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    tags.write_tag(Tag::GeoKeyDirectoryTag, &keys[..]).unwrap();
    img.write_data(&data).unwrap();
}

fn write_geofence(path: &Path) {
    let fence = json!({"type": "Feature", "properties": {"name": "test-field"},
    "geometry": {"type": "Polygon", "coordinates": [[
        [11.0, 48.0], [11.25, 48.0], [11.25, 48.25], [11.0, 48.25], [11.0, 48.0]
    ]]}});
    std::fs::write(path, fence.to_string()).unwrap();
}

struct Env {
    running: Running,
    base: String,
    mavlink_port: u16,
    http: reqwest::Client,
}

async fn start_app(db_name: &str, dir: &Path) -> Env {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let mavlink_port = free_udp_port();
    let dem = dir.join("dem.tif");
    let fence = dir.join("fence.geojson");
    write_dem(&dem);
    write_geofence(&fence);
    let vars: HashMap<&str, String> = HashMap::from([
        ("DATABASE_URL", url_for(db_name)),
        ("MAVLINK_ADDR", format!("udpin:127.0.0.1:{mavlink_port}")),
        ("HTTP_ADDR", "127.0.0.1:0".to_owned()),
        ("DEM_PATH", dem.display().to_string()),
        ("GEOFENCE_PATH", fence.display().to_string()),
        ("MIN_AGL_M", "30".to_owned()),
        ("LINK_TIMEOUT_S", "1".to_owned()),
    ]);
    let config = Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    eprintln!("[e2e] starting app");
    let running = server::start(config).await.unwrap();
    eprintln!("[e2e] app started on {}", running.http_addr);
    Env {
        base: format!("http://{}", running.http_addr),
        running,
        mavlink_port,
        http: reqwest::Client::new(),
    }
}

impl Env {
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    /// Polls `path` until `pred` holds or `secs` elapse.
    async fn wait_for(&self, path: &str, secs: u64, pred: impl Fn(u16, &Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let (status, body) = self.get(path).await;
            if pred(status, &body) {
                return body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {path}: last {status} {body}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn shutdown(self) {
        self.running.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(15), self.running.wait())
            .await
            .expect("graceful shutdown timed out");
    }
}

async fn vehicle(port: u16) -> Arc<AsyncConnection<MavMessage>> {
    Arc::new(
        mavlink::connect_async::<MavMessage>(&format!("udpout:127.0.0.1:{port}"))
            .await
            .unwrap(),
    )
}

async fn send_heartbeat(v: &AsyncConnection<MavMessage>) {
    let hb = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
        base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
        ..HEARTBEAT_DATA::default()
    });
    v.send(&VEHICLE, &hb).await.unwrap();
}

/// Sends heartbeats every 250 ms (like an autopilot) until `stop` fires.
fn spawn_heartbeats(
    v: Arc<AsyncConnection<MavMessage>>,
    stop: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            send_heartbeat(&v).await;
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
    })
}

/// Flies `n` points north from (48.03, 11.03) at 560 m AMSL (60 m AGL).
async fn send_track(v: &AsyncConnection<MavMessage>, n: u32) {
    for i in 0..n {
        let msg = MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
            time_boot_ms: i * 200,
            lat: 480_300_000 + i32::try_from(i).unwrap() * 5_000,
            lon: 110_300_000,
            alt: 560_000,
            relative_alt: 60_000,
            hdg: 0,
            ..GLOBAL_POSITION_INT_DATA::default()
        });
        v.send(&VEHICLE, &msg).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Answers the mission upload protocol like an autopilot would.
fn spawn_autopilot(v: Arc<AsyncConnection<MavMessage>>) -> tokio::task::JoinHandle<Vec<u16>> {
    tokio::spawn(async move {
        let request = |seq| {
            MavMessage::MISSION_REQUEST_INT(MISSION_REQUEST_INT_DATA {
                seq,
                target_system: 255,
                target_component: 190,
            })
        };
        let mut count = 0;
        let mut received = Vec::new();
        loop {
            match v.recv().await.unwrap().1 {
                MavMessage::MISSION_COUNT(c) => {
                    count = c.count;
                    v.send(&VEHICLE, &request(0)).await.unwrap();
                }
                MavMessage::MISSION_ITEM_INT(item) => {
                    received.push(item.seq);
                    if item.seq + 1 < count {
                        v.send(&VEHICLE, &request(item.seq + 1)).await.unwrap();
                    } else {
                        let ack = MavMessage::MISSION_ACK(MISSION_ACK_DATA {
                            target_system: 255,
                            target_component: 190,
                            mavtype: MavMissionResult::MAV_MISSION_ACCEPTED,
                        });
                        v.send(&VEHICLE, &ack).await.unwrap();
                        return received;
                    }
                }
                _ => {}
            }
        }
    })
}

/// Bounding box, conversions, terrain and geofence endpoints.
async fn check_spatial_endpoints(env: &Env) {
    // Bounding box: points 0..=10 have lat <= 48.035.
    let (s, fc) = env.get("/positions?bbox=11.0,48.0,11.1,48.03525").await;
    assert_eq!(s, 200);
    assert_eq!(fc["features"].as_array().unwrap().len(), 11);
    let (s, err) = env.get("/positions?bbox=11,48,10,49").await;
    assert_eq!((s, err["error"].as_str()), (400, Some("bad_request")));

    // Conversions and terrain.
    let (_, utm) = env.get("/convert/utm?lat=48.1374&lon=11.5755").await;
    assert_eq!(
        (utm["zone"].as_u64(), utm["north"].as_bool()),
        (Some(32), Some(true))
    );
    let (_, ground) = env.get("/terrain?lat=48.15&lon=11.15").await;
    assert!((ground["elevation_m"].as_f64().unwrap() - 900.0).abs() < 1e-3);

    // The geofence file is the fallback active fence.
    let (s, fence) = env.get("/geofences/active").await;
    assert_eq!(
        (s, fence["properties"]["name"].as_str()),
        (200, Some("test-field"))
    );
}

/// Mission validation (422/201) and upload to a fake autopilot.
async fn check_missions(env: &Env, v: &Arc<AsyncConnection<MavMessage>>) {
    // Invalid mission: leaves the fence and crosses the hill.
    let bad = json!({"system_id": 1, "waypoints": {"type": "LineString",
        "coordinates": [[11.03, 48.03, 560.0], [11.27, 48.27, 560.0]]}});
    let (s, err) = env.post("/missions", &bad).await;
    assert_eq!(s, 422, "{err}");
    let kinds: Vec<&str> = err["details"]["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"outside_geofence"), "{kinds:?}");
    assert!(kinds.contains(&"below_min_agl"), "{kinds:?}");

    // Valid mission is stored and uploaded to the (fake) autopilot.
    let good = json!({"system_id": 1, "waypoints": {"type": "LineString",
        "coordinates": [[11.03, 48.03, 560.0], [11.08, 48.03, 560.0], [11.08, 48.05, 570.0]]}});
    let (s, mission) = env.post("/missions", &good).await;
    assert_eq!(s, 201, "{mission}");
    let id = mission["id"].as_str().unwrap().to_owned();
    assert_eq!(mission["status"], "validated");

    let autopilot = spawn_autopilot(Arc::clone(v));
    let (s, _) = env
        .post(&format!("/missions/{id}/upload"), &json!({}))
        .await;
    assert_eq!(s, 202);
    let stored = env
        .wait_for(&format!("/missions/{id}"), 10, |_, b| {
            b["status"] != "uploading"
        })
        .await;
    assert_eq!(stored["status"], "uploaded", "{stored}");
    assert_eq!(autopilot.await.unwrap(), vec![0, 1, 2]);
}

// ------------------------------------------------------------------ tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_flow_ingest_api_live_missions_and_shutdown() {
    let db = unique_name("e2e");
    create_db(&db).await;
    let dir = temp_dir();
    let env = start_app(&db, &dir).await;

    // Health reports the loaded DEM and the database.
    let health = env
        .wait_for("/health", 10, |s, b| s == 200 && b["database"] == "up")
        .await;
    assert_eq!(health["terrain"], true);

    // Live stream filtered to vehicle 1.
    let ws_url = format!("ws://{}/live?system_id=1", env.running.http_addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();

    let v = vehicle(env.mavlink_port).await;
    // First heartbeat before any position, then periodic ones.
    send_heartbeat(&v).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stop_heartbeats = tokio_util::sync::CancellationToken::new();
    let heartbeats = spawn_heartbeats(v.clone(), stop_heartbeats.clone());
    send_track(&v, 21).await;

    // First live messages: link connected, then positions with AGL from the DEM.
    let mut saw_link = false;
    let first_position = loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no live message")
            .unwrap()
            .unwrap();
        let Message::Text(text) = msg else { continue };
        let m: Value = serde_json::from_str(&text).unwrap();
        match m["type"].as_str() {
            Some("link") => saw_link |= m["state"] == "connected",
            Some("position") => break m,
            _ => {}
        }
    };
    assert!(saw_link, "link connected message before positions");
    assert!((first_position["agl_m"].as_f64().unwrap() - 60.0).abs() < 1e-3);

    // Track is persisted through the write buffer.
    let track = env
        .wait_for("/vehicles/1/track", 10, |s, b| {
            s == 200 && b["properties"]["points"] == 21
        })
        .await;
    assert_eq!(track["geometry"]["type"], "LineString");
    assert_eq!(
        track["geometry"]["coordinates"][0],
        json!([11.03, 48.03, 560.0])
    );
    assert!((track["properties"]["agl_m"][0].as_f64().unwrap() - 60.0).abs() < 1e-3);

    // A straight line simplifies to its end points.
    let (_, simplified) = env.get("/vehicles/1/track?simplify_m=1").await;
    assert_eq!(simplified["properties"]["points"], 2);
    assert_eq!(simplified["properties"]["raw_points"], 21);

    // Vehicle row gets the heartbeat details and live status.
    let vehicles = env
        .wait_for("/vehicles", 15, |_, b| b[0]["vehicle_type"] == "QUADROTOR")
        .await;
    assert_eq!(vehicles[0]["autopilot"], "PX4");
    assert_eq!(vehicles[0]["status"]["armed"], true);

    check_spatial_endpoints(&env).await;
    check_missions(&env, &v).await;

    // No more heartbeats: the link is reported lost on the live stream.
    stop_heartbeats.cancel();
    heartbeats.await.unwrap();
    let lost = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let m: Value = serde_json::from_str(&t).unwrap();
                if m["type"] == "link" && m["state"] == "lost" {
                    return true;
                }
            }
        }
        false
    })
    .await;
    assert_eq!(lost, Ok(true));

    env.shutdown().await;
    drop_db(&db).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffers_while_database_is_missing_and_replays() {
    // The database does not exist yet: the service must still start and ingest.
    let db = unique_name("e2e_outage");
    let dir = temp_dir();
    let env = start_app(&db, &dir).await;
    let (_, health) = env.get("/health").await;
    assert_eq!(health["database"], "down");

    let v = vehicle(env.mavlink_port).await;
    send_heartbeat(&v).await;
    send_track(&v, 30).await;
    let health = env
        .wait_for("/health", 5, |_, b| b["buffer"]["queued"] == 30)
        .await;
    assert_eq!(health["buffer"]["written"], 0);

    // Database comes up: migrations run, the buffer replays everything.
    create_db(&db).await;
    let track = env
        .wait_for("/vehicles/1/track", 40, |s, b| {
            s == 200 && b["properties"]["points"] == 30
        })
        .await;
    let lats: Vec<f64> = track["geometry"]["coordinates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c[1].as_f64().unwrap())
        .collect();
    assert!(lats.windows(2).all(|w| w[0] < w[1]), "replayed in order");
    let (_, health) = env.get("/health").await;
    assert_eq!(health["buffer"]["queued"], 0);
    assert_eq!(health["buffer"]["dropped"], 0);

    env.shutdown().await;
    drop_db(&db).await;
    let _ = std::fs::remove_dir_all(dir);
}

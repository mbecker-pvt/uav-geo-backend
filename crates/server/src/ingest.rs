//! Ingest task: telemetry events → live broadcast, write buffer, vehicle state.
//!
//! The single consumer of decoded telemetry. For each event:
//!
//! - **Position** → compute AGL, update the in-memory vehicle status, push a
//!   `LiveMessage::Position` to WebSocket clients, and hand a `PositionRow`
//!   to the write buffer for PostGIS.
//! - **Heartbeat** → feed the [`LinkMonitor`], update `armed`, and upsert the
//!   vehicle row (in the background, rate-limited).
//!
//! A 500 ms timer additionally asks the monitor for vehicles whose heartbeat
//! timed out, since "no message arrived" can only be noticed by a clock.
//!
//! Nothing here awaits the database directly: a slow PostGIS must never
//! delay the live feed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use storage::PositionRow;
use telemetry::{Heartbeat, LinkChange, LinkMonitor, LinkState, PositionReport, TelemetryEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::state::{AppState, LiveMessage, LivePosition, VehicleStatus};

/// Minimum interval between vehicle upserts per vehicle (heartbeats are ~1 Hz).
const VEHICLE_UPSERT_INTERVAL: Duration = Duration::from_secs(10);

/// Runs until `cancel` fires or the telemetry channel closes.
pub async fn run_ingest(
    state: AppState,
    mut events: mpsc::Receiver<TelemetryEvent>,
    rows: mpsc::Sender<PositionRow>,
    link_timeout: Duration,
    cancel: CancellationToken,
) {
    let mut monitor = LinkMonitor::new(link_timeout);
    let last_upsert: UpsertLog = Arc::default();
    let mut check = tokio::time::interval(Duration::from_millis(500));
    // After a stall, run one check instead of a burst of catch-up ticks.
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // Wait for whichever comes first: shutdown, an event, or the timer.
        tokio::select! {
            () = cancel.cancelled() => break,
            event = events.recv() => match event {
                Some(TelemetryEvent::Heartbeat(hb)) => {
                    if let Some(change) = monitor.heartbeat(hb.system_id, std::time::Instant::now()) {
                        apply_link_change(&state, change).await;
                    }
                    set_armed(&state, &hb).await;
                    maybe_upsert_vehicle(&state, &hb, &last_upsert);
                }
                Some(TelemetryEvent::Position(p)) => {
                    // Live first (low latency), then storage. `send` only
                    // waits if the writer's channel is full, which means the
                    // writer itself is stuck, not merely the database.
                    handle_position(&state, &p).await;
                    if rows.send(to_row(&p)).await.is_err() {
                        tracing::warn!("write buffer closed, stopping ingest");
                        break;
                    }
                }
                None => break,
            },
            _ = check.tick() => {
                for change in monitor.check(std::time::Instant::now()) {
                    apply_link_change(&state, change).await;
                }
            }
        }
    }
    tracing::info!("ingest stopped");
}

fn to_row(p: &PositionReport) -> PositionRow {
    PositionRow {
        system_id: p.system_id,
        time: p.received_at,
        time_boot_ms: p.time_boot_ms,
        position: p.position,
        relative_alt_m: p.relative_alt_m,
        velocity_ned_mps: p.velocity_ned_mps,
        heading_deg: p.heading_deg,
    }
}

async fn handle_position(state: &AppState, p: &PositionReport) {
    let live = LivePosition {
        system_id: p.system_id,
        time: p.received_at,
        lat: p.position.lat,
        lon: p.position.lon,
        alt_m: p.position.alt_m,
        relative_alt_m: p.relative_alt_m,
        agl_m: state.agl_m(&p.position),
        heading_deg: p.heading_deg,
        velocity_ned_mps: p.velocity_ned_mps,
    };
    // Scoped block: release the write lock before broadcasting, so readers
    // (`GET /vehicles`) are blocked as briefly as possible.
    {
        let mut vehicles = state.vehicles.write().await;
        let v = vehicles.entry(p.system_id).or_insert(VehicleStatus {
            // Positions without heartbeats (yet): treat as connected.
            link: LinkState::Connected,
            armed: false,
            last_position: None,
        });
        v.last_position = Some(live.clone());
    }
    // No WebSocket clients is fine.
    let _ = state.live.send(LiveMessage::Position(live));
}

async fn apply_link_change(state: &AppState, change: LinkChange) {
    match change.state {
        LinkState::Lost => tracing::warn!(system_id = change.system_id, "vehicle link lost"),
        LinkState::Connected => {
            tracing::info!(system_id = change.system_id, "vehicle link connected");
        }
    }
    state
        .vehicles
        .write()
        .await
        .entry(change.system_id)
        .and_modify(|v| v.link = change.state)
        .or_insert(VehicleStatus {
            link: change.state,
            armed: false,
            last_position: None,
        });
    let _ = state.live.send(LiveMessage::Link {
        system_id: change.system_id,
        state: change.state,
    });
}

async fn set_armed(state: &AppState, hb: &Heartbeat) {
    if let Some(v) = state.vehicles.write().await.get_mut(&hb.system_id) {
        v.armed = hb.armed;
    }
}

/// Last successful vehicle upsert per system id: (time, type key).
///
/// A `std` (not `tokio`) mutex: it is only held for a map lookup/insert and
/// never across an `.await`, which is the case where a sync mutex is the
/// faster and simpler choice.
type UpsertLog = Arc<Mutex<BTreeMap<u8, (std::time::Instant, String)>>>;

/// Upserts the vehicle in the background so a slow or down database never
/// blocks ingestion. Rate-limited per vehicle after a **successful** upsert
/// (a failure is retried on the next heartbeat), immediate on type changes.
fn maybe_upsert_vehicle(state: &AppState, hb: &Heartbeat, log: &UpsertLog) {
    let now = std::time::Instant::now();
    // The "identity" of the vehicle row; if it changes (e.g. a different
    // airframe reusing system id 1), write immediately.
    let key = format!("{}/{}/{}", hb.component_id, hb.vehicle_type, hb.autopilot);
    if let Some((at, prev)) = log.lock().expect("upsert log poisoned").get(&hb.system_id)
        && *prev == key
        && now.duration_since(*at) < VEHICLE_UPSERT_INTERVAL
    {
        return;
    }
    let (db, hb, log) = (state.db.clone(), hb.clone(), log.clone());
    tokio::spawn(async move {
        match db
            .upsert_vehicle(
                hb.system_id,
                hb.component_id,
                &hb.vehicle_type,
                &hb.autopilot,
                hb.received_at,
            )
            .await
        {
            // Only remember success. Recording the attempt instead would
            // skip retries for 10 s, which is the bug the e2e test found: the
            // first heartbeat arrived before the migrations had run.
            Ok(()) => {
                log.lock()
                    .expect("upsert log poisoned")
                    .insert(hb.system_id, (now, key));
            }
            Err(e) => {
                tracing::debug!(error = %e, system_id = hb.system_id, "vehicle upsert failed");
            }
        }
    });
}

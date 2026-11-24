//! Storage tests against a real PostGIS (`--features db-tests`, needs `DATABASE_URL`).
//!
//! `#[sqlx::test]` creates a fresh database per test and applies the migrations.
#![cfg(feature = "db-tests")]

use chrono::{Duration, TimeZone, Utc};
use geo_core::{BBox, Geofence, Mission, Position};
use sqlx::PgPool;
use storage::{Db, MissionStatus, PositionRow};

fn row(system_id: u8, secs: i64, lat: f64, lon: f64) -> PositionRow {
    PositionRow {
        system_id,
        time: Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap() + Duration::seconds(secs),
        time_boot_ms: u32::try_from(secs * 1000).unwrap(),
        position: Position::new(lat, lon, 560.5).unwrap(),
        relative_alt_m: 40.0,
        velocity_ned_mps: [1.0, 0.5, -0.1],
        heading_deg: Some(90.0),
    }
}

#[sqlx::test(migrator = "storage::MIGRATOR")]
async fn positions_before_heartbeat_create_unknown_vehicle(pool: PgPool) {
    let db = Db::from_pool(pool);
    let n = db
        .insert_positions(&[row(7, 1, 48.1, 11.5), row(7, 2, 48.2, 11.6)])
        .await
        .unwrap();
    assert_eq!(n, 2);
    let vehicles = db.list_vehicles().await.unwrap();
    assert_eq!(vehicles.len(), 1);
    assert_eq!(vehicles[0].vehicle_type, "UNKNOWN");
    assert_eq!(vehicles[0].last_seen, row(7, 2, 0.0, 0.0).time);

    // A later heartbeat fills in the type but keeps last_seen monotonic.
    db.upsert_vehicle(7, 1, "QUADROTOR", "PX4", row(7, 0, 0.0, 0.0).time)
        .await
        .unwrap();
    let v = &db.list_vehicles().await.unwrap()[0];
    assert_eq!(
        (v.vehicle_type.as_str(), v.autopilot.as_str()),
        ("QUADROTOR", "PX4")
    );
    assert_eq!(v.last_seen, row(7, 2, 0.0, 0.0).time);
}

#[sqlx::test(migrator = "storage::MIGRATOR")]
async fn track_is_ordered_and_filtered(pool: PgPool) {
    let db = Db::from_pool(pool);
    // Inserted out of order (replay after a link/DB outage).
    let rows: Vec<_> = [3, 1, 2, 5, 4]
        .iter()
        .map(|&s| {
            row(
                1,
                s,
                48.0 + f64::from(u8::try_from(s).unwrap()) * 0.001,
                11.0,
            )
        })
        .chain([row(2, 1, 50.0, 8.0)])
        .collect();
    db.insert_positions(&rows).await.unwrap();

    let track = db.track(1, None, None, 100).await.unwrap();
    let secs: Vec<_> = track
        .iter()
        .map(|p| (p.time - row(1, 0, 0.0, 0.0).time).num_seconds())
        .collect();
    assert_eq!(secs, vec![1, 2, 3, 4, 5]);
    assert!((track[0].position.alt_m - 560.5).abs() < 1e-9);
    assert!((track[0].relative_alt_m - 40.0).abs() < 1e-6);

    let from = Some(row(1, 2, 0.0, 0.0).time);
    let to = Some(row(1, 4, 0.0, 0.0).time);
    assert_eq!(db.track(1, from, to, 100).await.unwrap().len(), 3);
    assert_eq!(db.track(1, None, None, 2).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "storage::MIGRATOR")]
async fn bbox_query_uses_lon_lat_order(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.insert_positions(&[
        row(1, 1, 48.1, 11.5),
        row(1, 2, 48.9, 11.5),
        row(2, 3, 48.1, 11.6),
    ])
    .await
    .unwrap();
    let bbox = BBox::new(11.0, 48.0, 12.0, 48.5).unwrap();
    let all = db
        .positions_in_bbox(&bbox, None, None, None, 100)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].system_id, 2, "newest first");
    let only1 = db
        .positions_in_bbox(&bbox, Some(1), None, None, 100)
        .await
        .unwrap();
    assert_eq!(only1.len(), 1);
}

#[sqlx::test(migrator = "storage::MIGRATOR")]
async fn large_batches_are_chunked(pool: PgPool) {
    let db = Db::from_pool(pool);
    let rows: Vec<_> = (0..2_500).map(|i| row(1, i, 48.0, 11.0)).collect();
    assert_eq!(db.insert_positions(&rows).await.unwrap(), 2_500);
}

#[sqlx::test(migrator = "storage::MIGRATOR")]
async fn geofence_and_mission_round_trip(pool: PgPool) {
    let db = Db::from_pool(pool);
    assert!(db.active_geofence().await.unwrap().is_none());
    let a = Geofence::new("a", &[(11.0, 48.0), (11.1, 48.0), (11.1, 48.1)]).unwrap();
    let b = Geofence::new(
        "b",
        &[(11.0, 48.0), (11.2, 48.0), (11.2, 48.2), (11.0, 48.2)],
    )
    .unwrap();
    db.create_geofence(&a).await.unwrap();
    let id_b = db.create_geofence(&b).await.unwrap();
    let (active_id, active) = db.active_geofence().await.unwrap().unwrap();
    assert_eq!(active_id, id_b);
    assert_eq!(active, b);

    let mission = Mission {
        system_id: 1,
        waypoints: vec![
            Position::new(48.05, 11.05, 600.0).unwrap(),
            Position::new(48.06, 11.07, 610.5).unwrap(),
            Position::new(48.05, 11.05, 600.0).unwrap(),
        ],
    };
    let id = db.insert_mission(&mission).await.unwrap();
    let stored = db.get_mission(id).await.unwrap().unwrap();
    assert_eq!(stored.waypoints, mission.waypoints);
    assert_eq!(stored.status, MissionStatus::Validated);

    assert!(db.begin_mission_upload(id).await.unwrap());
    assert!(
        !db.begin_mission_upload(id).await.unwrap(),
        "already uploading"
    );
    db.set_mission_status(id, MissionStatus::Failed, Some("timeout"))
        .await
        .unwrap();
    let stored = db.get_mission(id).await.unwrap().unwrap();
    assert_eq!(stored.status, MissionStatus::Failed);
    assert_eq!(stored.status_detail.as_deref(), Some("timeout"));
    assert!(
        db.get_mission(uuid::Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
}

//! Vehicles and positions.
//!
//! Write path: the server's write buffer calls [`Db::insert_positions`] with
//! batches; heartbeats call [`Db::upsert_vehicle`]. Read path: `GET
//! /vehicles`, `GET /vehicles/{id}/track` and `GET /positions?bbox=`.
//!
//! Coordinates go in with `ST_MakePoint(lon, lat, alt)` (note: x = lon first)
//! and come out with `ST_X`/`ST_Y`/`ST_Z`, so Rust never has to parse WKB.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use geo_core::{BBox, Position};
use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, QueryBuilder, Row};

use crate::{Db, INSERT_CHUNK_ROWS, Result, StorageError};

/// A known vehicle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VehicleRow {
    pub system_id: u8,
    pub component_id: u8,
    pub vehicle_type: String,
    pub autopilot: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

/// A position sample to insert.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionRow {
    pub system_id: u8,
    pub time: DateTime<Utc>,
    pub time_boot_ms: u32,
    pub position: Position,
    pub relative_alt_m: f64,
    pub velocity_ned_mps: [f64; 3],
    pub heading_deg: Option<f64>,
}

/// A stored position sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackPoint {
    pub system_id: u8,
    pub time: DateTime<Utc>,
    pub position: Position,
    pub relative_alt_m: f64,
}

/// Postgres has no unsigned byte type, so ids are stored as `smallint` (with
/// a CHECK 0..=255) and converted back here.
pub(crate) fn system_id(v: i16) -> Result<u8> {
    u8::try_from(v).map_err(|_| StorageError::Corrupt(format!("system_id {v}")))
}

fn track_point(row: &PgRow) -> Result<TrackPoint> {
    let position = Position::new(
        row.try_get("lat")?,
        row.try_get("lon")?,
        row.try_get("alt")?,
    )
    .map_err(|e| StorageError::Corrupt(e.to_string()))?;
    Ok(TrackPoint {
        system_id: system_id(row.try_get("system_id")?)?,
        time: row.try_get("time")?,
        position,
        relative_alt_m: f64::from(row.try_get::<f32, _>("relative_alt_m")?),
    })
}

/// Column list shared by track queries (a macro so SQL stays a literal).
///
/// sqlx 0.9 only accepts `&'static str` SQL (its `SqlSafeStr` guard against
/// injection via `format!`). `concat!` with a macro builds the full query at
/// compile time, so it is still a literal.
macro_rules! track_columns {
    () => {
        "SELECT system_id, time, ST_Y(geom) AS lat, ST_X(geom) AS lon,          ST_Z(geom) AS alt, relative_alt_m FROM positions "
    };
}

impl Db {
    /// Inserts or updates a vehicle seen in a heartbeat.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn upsert_vehicle(
        &self,
        system_id: u8,
        component_id: u8,
        vehicle_type: &str,
        autopilot: &str,
        seen_at: DateTime<Utc>,
    ) -> Result<()> {
        // Upsert: the first heartbeat inserts, later ones update. `GREATEST`
        // keeps `last_seen` monotonic if an older write lands late.
        sqlx::query(
            "INSERT INTO vehicles (system_id, component_id, vehicle_type, autopilot, first_seen, last_seen)
             VALUES ($1, $2, $3, $4, $5, $5)
             ON CONFLICT (system_id) DO UPDATE SET
                 component_id = EXCLUDED.component_id,
                 vehicle_type = EXCLUDED.vehicle_type,
                 autopilot    = EXCLUDED.autopilot,
                 last_seen    = GREATEST(vehicles.last_seen, EXCLUDED.last_seen)",
        )
        .bind(i16::from(system_id))
        .bind(i16::from(component_id))
        .bind(vehicle_type)
        .bind(autopilot)
        .bind(seen_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Inserts a batch of positions in one transaction.
    ///
    /// Vehicles not seen in a heartbeat yet are created with type `UNKNOWN`,
    /// so positions are never rejected because of ordering.
    ///
    /// # Errors
    /// Returns an error on database failure; nothing is inserted in that case.
    pub async fn insert_positions(&self, rows: &[PositionRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        // Newest time per vehicle in this batch, for the `vehicles` upsert.
        let mut last_seen: BTreeMap<u8, DateTime<Utc>> = BTreeMap::new();
        for r in rows {
            let e = last_seen.entry(r.system_id).or_insert(r.time);
            *e = (*e).max(r.time);
        }

        // One transaction for the whole batch: either all rows are stored or
        // none, so the write buffer can simply retry the same batch.
        let mut tx = self.pool().begin().await?;
        // `positions.system_id` is a foreign key. If a position arrives before
        // the vehicle's first heartbeat was written, create a placeholder row;
        // the next heartbeat upsert fills in the real type.
        for (&id, &seen) in &last_seen {
            sqlx::query(
                "INSERT INTO vehicles (system_id, component_id, vehicle_type, autopilot, first_seen, last_seen)
                 VALUES ($1, 0, 'UNKNOWN', 'UNKNOWN', $2, $2)
                 ON CONFLICT (system_id) DO UPDATE SET
                     last_seen = GREATEST(vehicles.last_seen, EXCLUDED.last_seen)",
            )
            .bind(i16::from(id))
            .bind(seen)
            .execute(&mut *tx)
            .await?;
        }

        let mut inserted = 0;
        // Multi-row `INSERT ... VALUES (...), (...), ...`: one round trip per
        // chunk instead of one per row (50 Hz × several vehicles adds up).
        for chunk in rows.chunks(INSERT_CHUNK_ROWS) {
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
                "INSERT INTO positions \
                 (system_id, time, time_boot_ms, geom, relative_alt_m, vn, ve, vd, heading_deg) ",
            );
            // Columns are `real`; f32 precision (≈ mm) is plenty for these values.
            #[allow(clippy::cast_possible_truncation)]
            qb.push_values(chunk, |mut b, r| {
                let [vn, ve, vd] = r.velocity_ned_mps;
                b.push_bind(i16::from(r.system_id))
                    .push_bind(r.time)
                    .push_bind(i64::from(r.time_boot_ms))
                    // Geometry built in SQL from bound numbers; the
                    // `_unseparated` variants avoid the comma that
                    // `push_values` would otherwise insert between them.
                    .push("ST_SetSRID(ST_MakePoint(")
                    .push_bind_unseparated(r.position.lon)
                    .push_unseparated(", ")
                    .push_bind_unseparated(r.position.lat)
                    .push_unseparated(", ")
                    .push_bind_unseparated(r.position.alt_m)
                    .push_unseparated("), 4326)")
                    .push_bind(r.relative_alt_m as f32)
                    .push_bind(vn as f32)
                    .push_bind(ve as f32)
                    .push_bind(vd as f32)
                    .push_bind(r.heading_deg.map(|h| h as f32));
            });
            inserted += qb.build().execute(&mut *tx).await?.rows_affected();
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// All known vehicles ordered by system id.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn list_vehicles(&self) -> Result<Vec<VehicleRow>> {
        let rows = sqlx::query(
            "SELECT system_id, component_id, vehicle_type, autopilot, first_seen, last_seen
             FROM vehicles ORDER BY system_id",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                Ok(VehicleRow {
                    system_id: system_id(r.try_get("system_id")?)?,
                    component_id: u8::try_from(r.try_get::<i16, _>("component_id")?)
                        .map_err(|e| StorageError::Corrupt(e.to_string()))?,
                    vehicle_type: r.try_get("vehicle_type")?,
                    autopilot: r.try_get("autopilot")?,
                    first_seen: r.try_get("first_seen")?,
                    last_seen: r.try_get("last_seen")?,
                })
            })
            .collect()
    }

    /// Positions of one vehicle in time order.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn track(
        &self,
        system_id: u8,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<TrackPoint>> {
        let rows = sqlx::query(concat!(
            track_columns!(),
            // `$n IS NULL OR ...` makes the time filters optional without
            // building different SQL strings. `id` breaks ties between rows
            // with the same timestamp so the order is stable.
            "WHERE system_id = $1
               AND ($2::timestamptz IS NULL OR time >= $2)
               AND ($3::timestamptz IS NULL OR time <= $3)
             ORDER BY time, id
             LIMIT $4"
        ))
        .bind(i16::from(system_id))
        .bind(from)
        .bind(to)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(track_point).collect()
    }

    /// Positions inside a bounding box (uses the GIST index), newest first.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn positions_in_bbox(
        &self,
        bbox: &BBox,
        system_id: Option<u8>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<TrackPoint>> {
        let rows = sqlx::query(concat!(
            track_columns!(),
            // `&&` is the bounding-box overlap operator, answered by the GIST
            // index. For points, "bbox overlaps envelope" is exactly "point
            // inside envelope", so no slower ST_Within is needed.
            "WHERE geom && ST_MakeEnvelope($1, $2, $3, $4, 4326)
               AND ($5::smallint IS NULL OR system_id = $5)
               AND ($6::timestamptz IS NULL OR time >= $6)
               AND ($7::timestamptz IS NULL OR time <= $7)
             ORDER BY time DESC, id DESC
             LIMIT $8"
        ))
        .bind(bbox.min_lon)
        .bind(bbox.min_lat)
        .bind(bbox.max_lon)
        .bind(bbox.max_lat)
        .bind(system_id.map(i16::from))
        .bind(from)
        .bind(to)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(track_point).collect()
    }
}

//! Geofences and missions.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use geo_core::{Geofence, Mission, Position};
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

use crate::telemetry::system_id;
use crate::{Db, Result, StorageError};

/// Lifecycle of a stored mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    /// Passed validation and stored.
    Validated,
    /// Upload to the vehicle in progress.
    Uploading,
    /// Accepted by the vehicle.
    Uploaded,
    /// Upload failed (see `status_detail`).
    Failed,
}

impl MissionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Validated => "validated",
            Self::Uploading => "uploading",
            Self::Uploaded => "uploaded",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "validated" => Self::Validated,
            "uploading" => Self::Uploading,
            "uploaded" => Self::Uploaded,
            "failed" => Self::Failed,
            other => return Err(StorageError::Corrupt(format!("mission status {other}"))),
        })
    }
}

/// A mission as stored in the database.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredMission {
    pub id: Uuid,
    pub system_id: u8,
    pub created_at: DateTime<Utc>,
    pub waypoints: Vec<Position>,
    pub status: MissionStatus,
    pub status_detail: Option<String>,
}

fn polygon_wkt(ring: &[(f64, f64)]) -> String {
    let mut s = String::from("POLYGON((");
    for (i, (lon, lat)) in ring.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        // `{}` on f64 prints the shortest representation that round-trips.
        let _ = write!(s, "{lon} {lat}");
    }
    s.push_str("))");
    s
}

fn multipoint_z_wkt(points: &[Position]) -> String {
    let mut s = String::from("MULTIPOINT Z (");
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "({} {} {})", p.lon, p.lat, p.alt_m);
    }
    s.push(')');
    s
}

impl Db {
    /// Stores a geofence and makes it the single active one.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn create_geofence(&self, fence: &Geofence) -> Result<i32> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("UPDATE geofences SET active = false WHERE active")
            .execute(&mut *tx)
            .await?;
        let id: i32 = sqlx::query_scalar(
            "INSERT INTO geofences (name, area, active)
             VALUES ($1, ST_GeomFromText($2, 4326), true) RETURNING id",
        )
        .bind(&fence.name)
        .bind(polygon_wkt(&fence.ring()))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The active geofence, if any.
    ///
    /// # Errors
    /// Returns an error on database failure or invalid stored geometry.
    pub async fn active_geofence(&self) -> Result<Option<(i32, Geofence)>> {
        let Some(row) = sqlx::query("SELECT id, name FROM geofences WHERE active")
            .fetch_optional(self.pool())
            .await?
        else {
            return Ok(None);
        };
        let id: i32 = row.try_get("id")?;
        let name: String = row.try_get("name")?;
        let ring: Vec<(f64, f64)> = sqlx::query(
            "SELECT ST_X(dp.geom) AS lon, ST_Y(dp.geom) AS lat
             FROM geofences, ST_DumpPoints(ST_ExteriorRing(area)) AS dp
             WHERE id = $1 ORDER BY dp.path",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(|r| Ok((r.try_get("lon")?, r.try_get("lat")?)))
        .collect::<Result<_>>()?;
        let fence = Geofence::new(name, &ring).map_err(|e| StorageError::Corrupt(e.to_string()))?;
        Ok(Some((id, fence)))
    }

    /// Stores a validated mission.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn insert_mission(&self, mission: &Mission) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO missions (id, system_id, waypoints, status)
             VALUES ($1, $2, ST_GeomFromText($3, 4326), 'validated')",
        )
        .bind(id)
        .bind(i16::from(mission.system_id))
        .bind(multipoint_z_wkt(&mission.waypoints))
        .execute(self.pool())
        .await?;
        Ok(id)
    }

    /// Loads a mission by id.
    ///
    /// # Errors
    /// Returns an error on database failure or invalid stored data.
    pub async fn get_mission(&self, id: Uuid) -> Result<Option<StoredMission>> {
        let Some(row) = sqlx::query(
            "SELECT system_id, created_at, status, status_detail FROM missions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?
        else {
            return Ok(None);
        };
        let waypoints = sqlx::query(
            "SELECT ST_Y(dp.geom) AS lat, ST_X(dp.geom) AS lon, ST_Z(dp.geom) AS alt
             FROM missions, ST_DumpPoints(waypoints) AS dp
             WHERE id = $1 ORDER BY dp.path",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(|r| {
            Position::new(r.try_get("lat")?, r.try_get("lon")?, r.try_get("alt")?)
                .map_err(|e| StorageError::Corrupt(e.to_string()))
        })
        .collect::<Result<_>>()?;
        Ok(Some(StoredMission {
            id,
            system_id: system_id(row.try_get("system_id")?)?,
            created_at: row.try_get("created_at")?,
            waypoints,
            status: MissionStatus::parse(row.try_get("status")?)?,
            status_detail: row.try_get("status_detail")?,
        }))
    }

    /// Atomically marks a mission as uploading unless an upload is already
    /// running. Returns `false` if the mission is missing or busy.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn begin_mission_upload(&self, id: Uuid) -> Result<bool> {
        let updated = sqlx::query(
            "UPDATE missions SET status = 'uploading', status_detail = NULL
             WHERE id = $1 AND status <> 'uploading'",
        )
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Sets the status of a mission.
    ///
    /// # Errors
    /// Returns an error on database failure.
    pub async fn set_mission_status(
        &self,
        id: Uuid,
        status: MissionStatus,
        detail: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE missions SET status = $2, status_detail = $3 WHERE id = $1")
            .bind(id)
            .bind(status.as_str())
            .bind(detail)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_wkt() {
        assert_eq!(
            polygon_wkt(&[(11.0, 48.0), (11.5, 48.0), (11.0, 48.25), (11.0, 48.0)]),
            "POLYGON((11 48, 11.5 48, 11 48.25, 11 48))"
        );
        let p = Position::new(48.1, 11.2, 560.5).unwrap();
        assert_eq!(
            multipoint_z_wkt(&[p, p]),
            "MULTIPOINT Z ((11.2 48.1 560.5), (11.2 48.1 560.5))"
        );
    }

    #[test]
    fn status_round_trips() {
        for s in [
            MissionStatus::Validated,
            MissionStatus::Uploading,
            MissionStatus::Uploaded,
            MissionStatus::Failed,
        ] {
            assert_eq!(MissionStatus::parse(s.as_str()).unwrap(), s);
        }
    }
}

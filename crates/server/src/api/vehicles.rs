//! Health, vehicles and tracks.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, TRACK_LIMIT, limit};
use crate::geo_json::track_feature;
use crate::state::AppState;

/// `GET /health`: always 200 while the process runs; reports dependencies.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    // Liveness, not readiness: the service is useful (buffering, live feed)
    // even without a database, so a down DB is reported, not a 503. A tight
    // timeout keeps health checks fast during an outage.
    let db_up = matches!(
        tokio::time::timeout(Duration::from_secs(1), state.db.ping()).await,
        Ok(Ok(()))
    );
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "database": if db_up { "up" } else { "down" },
        "buffer": state.buffer_stats.snapshot(),
        "terrain": state.terrain.is_some(),
        "mavlink": state.link.is_some(),
    }))
}

/// `GET /vehicles`: database rows merged with live link state.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let rows = state.db.list_vehicles().await?;
    // Clone the small map and drop the read lock right away, so ingest is
    // not blocked while we build JSON.
    let live = state.vehicles.read().await.clone();
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = json!(r);
            v["status"] = json!(live.get(&r.system_id));
            v
        })
        .collect();
    // Vehicles seen live but not yet persisted (e.g. database just recovered).
    for (id, status) in &live {
        if !rows.iter().any(|r| r.system_id == *id) {
            out.push(json!({ "system_id": id, "status": status }));
        }
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Debug, Deserialize)]
pub struct TrackQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    /// Douglas–Peucker tolerance in metres.
    simplify_m: Option<f64>,
    limit: Option<i64>,
}

/// `GET /vehicles/{id}/track`: GeoJSON `LineString` feature.
pub async fn track(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Query(q): Query<TrackQuery>,
) -> ApiResult<Json<Value>> {
    if let Some(s) = q.simplify_m
        && !(s.is_finite() && s >= 0.0)
    {
        return Err(ApiError::bad_request(
            "simplify_m must be a non-negative number",
        ));
    }
    let points = state
        .db
        .track(id, q.from, q.to, limit(q.limit, TRACK_LIMIT))
        .await?;
    let raw_count = points.len();
    // Simplify after loading: RDP needs the whole line. `simplify_track`
    // returns indices, so the kept points keep their timestamps.
    let points = match q.simplify_m {
        Some(tol) if tol > 0.0 => {
            let positions: Vec<_> = points.iter().map(|p| p.position).collect();
            let keep = geo_core::simplify_track(&positions, tol)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            keep.into_iter().map(|i| points[i].clone()).collect()
        }
        _ => points,
    };
    // AGL is computed on read, not stored: loading a better DEM later
    // improves all historic tracks too. Array parallel to the coordinates;
    // `null` entries where the DEM has no data, absent without a DEM.
    let agl: Option<Vec<Option<f64>>> = state
        .terrain
        .as_ref()
        .map(|_| points.iter().map(|p| state.agl_m(&p.position)).collect());
    let extra = json!({
        "raw_points": raw_count,
        "simplify_m": q.simplify_m,
        "agl_m": agl,
    });
    Ok(Json(json!(track_feature(id, &points, &extra))))
}

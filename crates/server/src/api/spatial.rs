//! Spatial queries, coordinate conversion and terrain lookup.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use geo_core::{BBox, Position, Utm};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, POSITIONS_LIMIT, limit};
use crate::geo_json::positions_collection;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PositionsQuery {
    /// `minLon,minLat,maxLon,maxLat`
    bbox: String,
    system_id: Option<u8>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: Option<i64>,
}

/// `GET /positions?bbox=...`: positions in a bounding box, newest first.
pub async fn positions(
    State(state): State<AppState>,
    Query(q): Query<PositionsQuery>,
) -> ApiResult<Json<Value>> {
    let bbox: BBox = q
        .bbox
        .parse()
        .map_err(|e: geo_core::BBoxError| ApiError::bad_request(e.to_string()))?;
    let points = state
        .db
        .positions_in_bbox(
            &bbox,
            q.system_id,
            q.from,
            q.to,
            limit(q.limit, POSITIONS_LIMIT),
        )
        .await?;
    Ok(Json(json!(positions_collection(&points))))
}

#[derive(Debug, Deserialize)]
pub struct LatLon {
    lat: f64,
    lon: f64,
}

fn position(q: &LatLon) -> ApiResult<Position> {
    Position::new(q.lat, q.lon, 0.0).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// `GET /convert/utm?lat&lon`
pub async fn to_utm(Query(q): Query<LatLon>) -> ApiResult<Json<Utm>> {
    geo_core::to_utm(&position(&q)?)
        .map(Json)
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct UtmQuery {
    zone: u8,
    /// `N` or `S`
    hemisphere: String,
    easting: f64,
    northing: f64,
}

/// `GET /convert/wgs84?zone&hemisphere&easting&northing`
pub async fn to_wgs84(Query(q): Query<UtmQuery>) -> ApiResult<Json<Value>> {
    let north = match q.hemisphere.to_ascii_uppercase().as_str() {
        "N" => true,
        "S" => false,
        _ => return Err(ApiError::bad_request("hemisphere must be N or S")),
    };
    let utm = Utm {
        zone: q.zone,
        north,
        easting: q.easting,
        northing: q.northing,
    };
    let p = geo_core::from_utm(&utm, 0.0).map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({ "lat": p.lat, "lon": p.lon })))
}

/// `GET /terrain?lat&lon`: DEM elevation.
pub async fn terrain(
    State(state): State<AppState>,
    Query(q): Query<LatLon>,
) -> ApiResult<Json<Value>> {
    let p = position(&q)?;
    let terrain = state
        .terrain
        .as_ref()
        .ok_or_else(|| ApiError::not_found("no DEM loaded (set DEM_PATH)"))?;
    let elevation = terrain
        .elevation_m(p.lat, p.lon)
        .ok_or_else(|| ApiError::not_found("no terrain data at this location"))?;
    Ok(Json(
        json!({ "lat": p.lat, "lon": p.lon, "elevation_m": elevation }),
    ))
}

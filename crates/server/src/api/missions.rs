//! Geofences and missions.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use geo_core::{Geofence, Mission, MissionRules};
use serde_json::{Value, json};
use storage::{MissionStatus, StoredMission};
use telemetry::{UploadConfig, upload_mission};
use uuid::Uuid;

use super::{ApiError, ApiResult, json_rejection};
use crate::geo_json::{geofence_feature, parse_geofence, parse_waypoints, waypoints_geometry};
use crate::state::AppState;

/// `POST /geofences`: GeoJSON `Polygon` or `Feature`; becomes the active fence.
pub async fn create_geofence(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(body) = body.map_err(|e| json_rejection(&e))?;
    let fence = parse_geofence(&body).map_err(|e| ApiError::bad_request(e.0))?;
    let id = state.db.create_geofence(&fence).await?;
    tracing::info!(id, name = %fence.name, "geofence activated");
    Ok((
        StatusCode::CREATED,
        Json(json!(geofence_feature(Some(id), &fence))),
    ))
}

/// The fence used for validation: active one in the DB, else `GEOFENCE_PATH`.
async fn current_geofence(state: &AppState) -> ApiResult<Option<(Option<i32>, Arc<Geofence>)>> {
    Ok(match state.db.active_geofence().await? {
        Some((id, fence)) => Some((Some(id), Arc::new(fence))),
        None => state.default_geofence.clone().map(|f| (None, f)),
    })
}

/// `GET /geofences/active`
pub async fn active_geofence(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let (id, fence) = current_geofence(&state)
        .await?
        .ok_or_else(|| ApiError::not_found("no active geofence"))?;
    Ok(Json(json!(geofence_feature(id, &fence))))
}

fn mission_json(m: &StoredMission) -> Value {
    json!({
        "id": m.id,
        "system_id": m.system_id,
        "created_at": m.created_at,
        "status": m.status,
        "status_detail": m.status_detail,
        "waypoint_count": m.waypoints.len(),
        "waypoints": waypoints_geometry(&m.waypoints),
    })
}

/// `POST /missions`: `{"system_id": 1, "waypoints": <GeoJSON>}`.
///
/// Validates against the active geofence and minimum AGL; responds `422`
/// with all violations, or `201` with the stored mission.
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(body) = body.map_err(|e| json_rejection(&e))?;
    let system_id = body
        .get("system_id")
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| ApiError::bad_request("system_id (0..=255) is required"))?;
    let waypoints = parse_waypoints(
        body.get("waypoints")
            .ok_or_else(|| ApiError::bad_request("waypoints (GeoJSON) is required"))?,
    )
    .map_err(|e| ApiError::bad_request(e.0))?;
    let mission = Mission {
        system_id,
        waypoints,
    };

    let fence = current_geofence(&state).await?;
    let rules = MissionRules {
        geofence: fence.as_ref().map(|(_, f)| f.as_ref()),
        terrain: state.terrain.as_deref(),
        min_agl_m: state.min_agl_m,
        sample_spacing_m: state.terrain_sample_m,
    };
    let checks = json!({
        "geofence": fence.as_ref().map(|(_, f)| f.name.clone()),
        "terrain": state.terrain.is_some(),
        "min_agl_m": state.min_agl_m,
    });
    if let Err(violations) = mission.validate(&rules) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "mission_invalid",
            format!("mission violates {} rule(s)", violations.len()),
        )
        .with_details(json!({ "violations": violations, "checks": checks })));
    }

    let id = state.db.insert_mission(&mission).await?;
    let stored = state
        .db
        .get_mission(id)
        .await?
        .ok_or_else(|| ApiError::not_found("mission vanished"))?;
    let mut out = mission_json(&stored);
    out["checks"] = checks;
    Ok((StatusCode::CREATED, Json(out)))
}

/// `GET /missions/{id}`
pub async fn get(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let m = state
        .db
        .get_mission(id)
        .await?
        .ok_or_else(|| ApiError::not_found("mission not found"))?;
    Ok(Json(mission_json(&m)))
}

/// `POST /missions/{id}/upload`: starts the MAVLink upload in the background
/// and responds `202`; poll `GET /missions/{id}` for the result.
pub async fn upload(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let link = state.link.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_link",
            "MAVLink is not connected",
        )
    })?;
    let mission = state
        .db
        .get_mission(id)
        .await?
        .ok_or_else(|| ApiError::not_found("mission not found"))?;
    if !state.db.begin_mission_upload(id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "upload_in_progress",
            "an upload of this mission is already running",
        ));
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        let result = upload_mission(
            &link,
            mission.system_id,
            &mission.waypoints,
            UploadConfig::default(),
        )
        .await;
        let (status, detail) = match &result {
            Ok(()) => (MissionStatus::Uploaded, None),
            Err(e) => (MissionStatus::Failed, Some(e.to_string())),
        };
        match &result {
            Ok(()) => tracing::info!(%id, "mission uploaded"),
            Err(e) => tracing::warn!(%id, error = %e, "mission upload failed"),
        }
        if let Err(e) = db.set_mission_status(id, status, detail.as_deref()).await {
            tracing::error!(%id, error = %e, "could not store mission upload result");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "id": id, "status": MissionStatus::Uploading })),
    ))
}

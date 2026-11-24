//! HTTP routes.

mod error;
mod missions;
mod spatial;
mod vehicles;
mod ws;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub use error::{ApiError, ApiResult};

use crate::state::AppState;

/// Default and maximum number of rows returned by list endpoints.
const TRACK_LIMIT: (i64, i64) = (100_000, 1_000_000);
const POSITIONS_LIMIT: (i64, i64) = (10_000, 100_000);

/// Builds the application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(vehicles::health))
        .route("/vehicles", get(vehicles::list))
        .route("/vehicles/{id}/track", get(vehicles::track))
        .route("/positions", get(spatial::positions))
        .route("/convert/utm", get(spatial::to_utm))
        .route("/convert/wgs84", get(spatial::to_wgs84))
        .route("/terrain", get(spatial::terrain))
        .route("/geofences", post(missions::create_geofence))
        .route("/geofences/active", get(missions::active_geofence))
        .route("/missions", post(missions::create))
        .route("/missions/{id}", get(missions::get))
        .route("/missions/{id}/upload", post(missions::upload))
        .route("/live", get(ws::live))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Clamps an optional client limit to `1..=max`, defaulting to `default`.
fn limit(requested: Option<i64>, (default, max): (i64, i64)) -> i64 {
    requested.unwrap_or(default).clamp(1, max)
}

/// Maps axum's JSON rejection to our error format.
fn json_rejection(e: &JsonRejection) -> ApiError {
    ApiError::bad_request(e.body_text())
}

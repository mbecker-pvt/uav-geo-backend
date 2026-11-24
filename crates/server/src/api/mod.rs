//! HTTP routes.
//!
//! Handlers are thin: parse and validate the request, call `storage` /
//! `geo-core`, shape the response. Business rules live in `geo-core` (e.g.
//! mission validation), not here, so they can be tested without HTTP.
//!
//! All errors go through [`ApiError`], so every failure has the same JSON
//! shape: `{"error": "<code>", "message": "...", "details": ...}`.
//!
//! | File | Routes |
//! |---|---|
//! | `vehicles.rs` | `/health`, `/vehicles`, `/vehicles/{id}/track` |
//! | `spatial.rs` | `/positions?bbox`, `/convert/*`, `/terrain` |
//! | `missions.rs` | `/geofences*`, `/missions*` |
//! | `ws.rs` | `/live` WebSocket |

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
///
/// Without a cap, `GET /vehicles/1/track` over a month of 50 Hz data would
/// try to serialise ~100 M points into one response.
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
        // Logs every request (method, path, status, latency) via `tracing`.
        .layer(TraceLayer::new_for_http())
        // Lets a browser map on another origin (e.g. a local dev server)
        // call the API. Permissive is fine for a demo backend without auth;
        // restrict the origins before exposing it publicly.
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

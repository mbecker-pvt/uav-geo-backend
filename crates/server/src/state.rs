//! Shared application state and live messages.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use geo_core::{Geofence, Position, TerrainSource};
use serde::Serialize;
use storage::Db;
use telemetry::{LinkState, MavLink};
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

use crate::buffer::BufferStats;

/// Capacity of the live broadcast; slow WebSocket clients skip messages.
pub const LIVE_CHANNEL_CAPACITY: usize = 1024;

/// A message pushed to WebSocket clients.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveMessage {
    Position(LivePosition),
    Link { system_id: u8, state: LinkState },
}

impl LiveMessage {
    /// Vehicle the message is about.
    #[must_use]
    pub fn system_id(&self) -> u8 {
        match self {
            Self::Position(p) => p.system_id,
            Self::Link { system_id, .. } => *system_id,
        }
    }
}

/// A live position update.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LivePosition {
    pub system_id: u8,
    pub time: DateTime<Utc>,
    pub lat: f64,
    pub lon: f64,
    /// Altitude above mean sea level.
    pub alt_m: f64,
    /// Altitude above home.
    pub relative_alt_m: f64,
    /// Height above ground from the DEM, if loaded and covering the position.
    pub agl_m: Option<f64>,
    pub heading_deg: Option<f64>,
    pub velocity_ned_mps: [f64; 3],
}

/// In-memory status of a vehicle (complements the database row).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VehicleStatus {
    pub link: LinkState,
    pub armed: bool,
    pub last_position: Option<LivePosition>,
}

/// State shared by all handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub live: broadcast::Sender<LiveMessage>,
    pub vehicles: Arc<RwLock<BTreeMap<u8, VehicleStatus>>>,
    pub terrain: Option<Arc<dyn TerrainSource>>,
    /// Fallback geofence from `GEOFENCE_PATH` if none is active in the DB.
    pub default_geofence: Option<Arc<Geofence>>,
    pub link: Option<MavLink>,
    pub buffer_stats: Arc<BufferStats>,
    pub min_agl_m: f64,
    pub terrain_sample_m: f64,
    /// Cancelled on shutdown (closes WebSockets).
    pub shutdown: CancellationToken,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("terrain", &self.terrain.is_some())
            .field("link", &self.link.is_some())
            .field("min_agl_m", &self.min_agl_m)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Height above ground of `p`, if terrain data covers it.
    #[must_use]
    pub fn agl_m(&self, p: &Position) -> Option<f64> {
        self.terrain.as_ref().and_then(|t| t.agl_m(p))
    }
}

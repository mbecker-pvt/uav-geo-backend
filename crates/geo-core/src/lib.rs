//! Domain types and pure geodesy shared by all crates of `uav-geo-backend`.
//!
//! This crate does no I/O: positions, bounding boxes, WGS84 ↔ UTM, track
//! simplification, geofences, missions and the [`TerrainSource`] trait.

pub mod bbox;
pub mod geofence;
pub mod mission;
pub mod position;
pub mod simplify;
pub mod terrain;
pub mod utm;

pub use bbox::{BBox, BBoxError};
pub use geofence::{Geofence, GeofenceError};
pub use mission::{MAX_WAYPOINTS, Mission, MissionRules, Violation};
pub use position::{Position, PositionError};
pub use simplify::simplify_track;
pub use terrain::{FlatTerrain, TerrainSource};
pub use utm::{Utm, UtmError, from_utm, to_utm, to_utm_in_zone, zone_for};

//! Domain types and pure geodesy shared by all crates of `uav-geo-backend`.
//!
//! This crate does no I/O: positions, bounding boxes, WGS84 ↔ UTM, track
//! simplification, geofences, missions and the [`TerrainSource`] trait.
//!
//! It sits at the bottom of the dependency graph: `telemetry`, `terrain`,
//! `storage` and `server` all depend on it, it depends on none of them.
//! Keeping it free of sockets, databases and files means every rule here
//! (e.g. "may this mission be flown?") can be unit-tested in microseconds
//! and is computed the same way by the API and the tests.
//!
//! | Module | Used by |
//! |---|---|
//! | [`position`] | everywhere: the one validated coordinate type |
//! | [`bbox`] | `GET /positions?bbox=` and the `/live?bbox=` filter |
//! | [`utm`] | `/convert/*`, track simplification, mission leg sampling |
//! | [`simplify`] | `GET /vehicles/{id}/track?simplify_m=` |
//! | [`geofence`], [`mission`] | `POST /missions` validation |
//! | [`terrain`] | implemented by the `terrain` crate's DEM, used for AGL |

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

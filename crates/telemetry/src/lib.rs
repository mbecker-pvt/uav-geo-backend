//! MAVLink telemetry ingestion.
//!
//! [`MavLink`] owns an async UDP connection (e.g. from PX4 SITL), decodes
//! `HEARTBEAT` and `GLOBAL_POSITION_INT` into [`TelemetryEvent`]s and forwards
//! them through a `tokio` channel. It also uploads missions
//! ([`upload_mission`]) and detects link loss ([`LinkMonitor`]).

mod decode;
mod link;
mod link_state;
mod mission;

pub use decode::{Heartbeat, PositionReport, TelemetryEvent, decode, from_global_position_int};
pub use link::{GCS_COMPONENT_ID, GCS_SYSTEM_ID, LinkError, MavLink, RawMessage};
pub use link_state::{LinkChange, LinkMonitor, LinkState};
/// Re-export of the MAVLink `common` dialect used by this crate.
pub use mavlink::dialects::common;
pub use mission::{UploadConfig, UploadError, mission_item, upload_mission};

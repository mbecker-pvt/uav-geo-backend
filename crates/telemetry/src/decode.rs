//! Pure decoding of MAVLink messages into [`TelemetryEvent`]s (no I/O).
//!
//! An autopilot sends dozens of message types. The backend only needs two:
//!
//! - `HEARTBEAT` (1 Hz): "I am alive", plus vehicle type, autopilot and the
//!   armed flag. Drives the vehicle list and link-loss detection.
//! - `GLOBAL_POSITION_INT` (up to 50 Hz): the fused GNSS/IMU position.
//!   Becomes a stored track point and a live WebSocket message.
//!
//! Everything else returns `None` here and is only seen by subscribers of the
//! raw broadcast (the mission upload).
//!
//! MAVLink sends integers to save bandwidth; the conversions are:
//! lat/lon degE7 → degrees (`/ 1e7`), altitudes mm → m (`/ 1000`),
//! velocities cm/s → m/s (`/ 100`), heading cdeg → degrees (`/ 100`).

use chrono::{DateTime, Utc};
use geo_core::{Position, PositionError};
use mavlink::MavHeader;
use mavlink::dialects::common::{
    GLOBAL_POSITION_INT_DATA, HEARTBEAT_DATA, MavAutopilot, MavMessage, MavModeFlag, MavType,
};

/// A decoded, validated telemetry event.
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryEvent {
    /// A vehicle autopilot heartbeat.
    Heartbeat(Heartbeat),
    /// A fused global position estimate.
    Position(PositionReport),
}

impl TelemetryEvent {
    /// MAVLink system id of the vehicle that produced the event.
    #[must_use]
    pub fn system_id(&self) -> u8 {
        match self {
            Self::Heartbeat(h) => h.system_id,
            Self::Position(p) => p.system_id,
        }
    }
}

/// Decoded `HEARTBEAT` of a vehicle autopilot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub system_id: u8,
    pub component_id: u8,
    /// Vehicle type without the `MAV_TYPE_` prefix, e.g. `QUADROTOR`.
    pub vehicle_type: String,
    /// Autopilot without the `MAV_AUTOPILOT_` prefix, e.g. `PX4`.
    pub autopilot: String,
    pub armed: bool,
    pub received_at: DateTime<Utc>,
}

/// Decoded `GLOBAL_POSITION_INT`.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionReport {
    pub system_id: u8,
    pub component_id: u8,
    /// Milliseconds since autopilot boot.
    pub time_boot_ms: u32,
    /// Wall-clock time the message was received by the backend.
    ///
    /// This, not `time_boot_ms`, is the row's `time` in the database: the
    /// boot clock restarts with every reboot of the autopilot, so it cannot
    /// order points across flights.
    pub received_at: DateTime<Utc>,
    /// WGS84 position, altitude above mean sea level.
    pub position: Position,
    /// Altitude above the home position in metres.
    pub relative_alt_m: f64,
    /// Ground speed (north, east, down) in m/s.
    pub velocity_ned_mps: [f64; 3],
    /// Heading in degrees 0..360, `None` if unknown.
    pub heading_deg: Option<f64>,
}

/// Converts the raw integer fields of a MAVLink `GLOBAL_POSITION_INT` message
/// (degE7 for lat/lon, millimetres for altitude) into a [`Position`].
///
/// # Errors
/// Returns [`PositionError`] if the decoded coordinates are out of range.
pub fn from_global_position_int(
    lat_e7: i32,
    lon_e7: i32,
    alt_mm: i32,
) -> Result<Position, PositionError> {
    Position::new(
        f64::from(lat_e7) / 1e7,
        f64::from(lon_e7) / 1e7,
        f64::from(alt_mm) / 1000.0,
    )
}

/// Decodes a MAVLink message into a [`TelemetryEvent`].
///
/// Returns `None` for messages that are not relevant, for heartbeats of
/// non-autopilot components (GCS, cameras, ...), and for positions without a
/// valid fix (out of range or exactly 0/0).
#[must_use]
pub fn decode(
    header: &MavHeader,
    msg: &MavMessage,
    received_at: DateTime<Utc>,
) -> Option<TelemetryEvent> {
    // Extended (MAVLink 2) system ids above 255 cannot be addressed by the
    // mission protocol and are not used by PX4/ArduPilot; ignore them.
    let system_id = u8::try_from(header.system_id).ok()?;
    match msg {
        MavMessage::HEARTBEAT(hb) => {
            decode_heartbeat(system_id, header.component_id, hb, received_at)
        }
        MavMessage::GLOBAL_POSITION_INT(gp) => {
            decode_position(system_id, header.component_id, gp, received_at)
        }
        _ => None,
    }
}

fn decode_heartbeat(
    system_id: u8,
    component_id: u8,
    hb: &HEARTBEAT_DATA,
    received_at: DateTime<Utc>,
) -> Option<TelemetryEvent> {
    // Ground stations, cameras, gimbals etc. also send heartbeats, with
    // autopilot = INVALID. Only the flight controller describes the vehicle;
    // without this filter a camera heartbeat would overwrite the vehicle type.
    if hb.autopilot == MavAutopilot::MAV_AUTOPILOT_INVALID || hb.mavtype == MavType::MAV_TYPE_GCS {
        return None;
    }
    Some(TelemetryEvent::Heartbeat(Heartbeat {
        system_id,
        component_id,
        vehicle_type: enum_name(&hb.mavtype, "MAV_TYPE_"),
        autopilot: enum_name(&hb.autopilot, "MAV_AUTOPILOT_"),
        armed: hb
            .base_mode
            .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED),
        received_at,
    }))
}

fn decode_position(
    system_id: u8,
    component_id: u8,
    gp: &GLOBAL_POSITION_INT_DATA,
    received_at: DateTime<Utc>,
) -> Option<TelemetryEvent> {
    // Before the first GNSS fix, autopilots report 0/0 ("Null Island" in the
    // Gulf of Guinea). Storing those would draw a line across half the globe.
    if gp.lat == 0 && gp.lon == 0 {
        tracing::trace!(system_id, "position without fix ignored");
        return None;
    }
    let position = match from_global_position_int(gp.lat, gp.lon, gp.alt) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(system_id, error = %e, "invalid position dropped");
            return None;
        }
    };
    Some(TelemetryEvent::Position(PositionReport {
        system_id,
        component_id,
        time_boot_ms: gp.time_boot_ms,
        received_at,
        position,
        relative_alt_m: f64::from(gp.relative_alt) / 1000.0,
        velocity_ned_mps: [
            f64::from(gp.vx) / 100.0,
            f64::from(gp.vy) / 100.0,
            f64::from(gp.vz) / 100.0,
        ],
        // UINT16_MAX is MAVLink's "unknown" marker for the heading.
        heading_deg: (gp.hdg != u16::MAX).then(|| f64::from(gp.hdg) / 100.0),
    }))
}

/// `MAV_TYPE_QUADROTOR` → `QUADROTOR`. The generated MAVLink enums have no
/// name accessor, but their `Debug` output is the variant name.
fn enum_name<T: std::fmt::Debug>(value: &T, prefix: &str) -> String {
    let s = format!("{value:?}");
    s.strip_prefix(prefix)
        .map_or_else(|| s.clone(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> MavHeader {
        MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 0,
        }
    }

    #[test]
    fn decodes_global_position_int_fields() {
        let p = from_global_position_int(481_086_000, 112_939_000, 560_250).unwrap();
        assert!((p.lat - 48.1086).abs() < 1e-9);
        assert!((p.lon - 11.2939).abs() < 1e-9);
        assert!((p.alt_m - 560.25).abs() < 1e-9);
    }

    #[test]
    fn decodes_position_message() {
        let msg = MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
            time_boot_ms: 1234,
            lat: 481_086_000,
            lon: 112_939_000,
            alt: 560_250,
            relative_alt: 40_000,
            vx: 150,
            vy: -50,
            vz: 10,
            hdg: 9000,
        });
        let Some(TelemetryEvent::Position(p)) = decode(&header(), &msg, Utc::now()) else {
            panic!("expected position");
        };
        assert_eq!(p.time_boot_ms, 1234);
        assert!((p.relative_alt_m - 40.0).abs() < 1e-9);
        assert_eq!(p.velocity_ned_mps, [1.5, -0.5, 0.1]);
        assert_eq!(p.heading_deg, Some(90.0));
    }

    #[test]
    fn ignores_position_without_fix_and_unknown_heading() {
        let mut data = GLOBAL_POSITION_INT_DATA::default();
        assert!(
            decode(
                &header(),
                &MavMessage::GLOBAL_POSITION_INT(data.clone()),
                Utc::now()
            )
            .is_none()
        );
        data.lat = 481_086_000;
        data.lon = 112_939_000;
        data.hdg = u16::MAX;
        let Some(TelemetryEvent::Position(p)) = decode(
            &header(),
            &MavMessage::GLOBAL_POSITION_INT(data),
            Utc::now(),
        ) else {
            panic!("expected position");
        };
        assert_eq!(p.heading_deg, None);
    }

    #[test]
    fn decodes_autopilot_heartbeat_and_skips_gcs() {
        let hb = HEARTBEAT_DATA {
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
            base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
            ..HEARTBEAT_DATA::default()
        };
        let Some(TelemetryEvent::Heartbeat(h)) =
            decode(&header(), &MavMessage::HEARTBEAT(hb), Utc::now())
        else {
            panic!("expected heartbeat");
        };
        assert_eq!(h.vehicle_type, "QUADROTOR");
        assert_eq!(h.autopilot, "PX4");
        assert!(h.armed);

        let gcs = HEARTBEAT_DATA {
            mavtype: MavType::MAV_TYPE_GCS,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            ..HEARTBEAT_DATA::default()
        };
        assert!(decode(&header(), &MavMessage::HEARTBEAT(gcs), Utc::now()).is_none());
    }
}

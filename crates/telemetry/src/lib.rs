//! MAVLink telemetry ingestion.
//!
//! Week 2: receive `HEARTBEAT` and `GLOBAL_POSITION_INT` over UDP with the
//! `mavlink` crate and forward positions through a `tokio` channel.

use geo_core::{Position, PositionError};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_global_position_int() {
        let p = from_global_position_int(481_086_000, 112_939_000, 560_250).unwrap();
        assert!((p.lat - 48.1086).abs() < 1e-9);
        assert!((p.lon - 11.2939).abs() < 1e-9);
        assert!((p.alt_m - 560.25).abs() < 1e-9);
    }
}

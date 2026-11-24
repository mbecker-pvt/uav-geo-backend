//! Validated WGS84 positions.
//!
//! [`Position`] is the only coordinate type that crosses crate boundaries.
//! It can only be built through [`Position::new`], so any value of this type
//! is known to be in range. Decoded MAVLink messages, rows read from PostGIS
//! and waypoints parsed from GeoJSON all go through it.

use serde::Serialize;

/// Mean Earth radius in metres (IUGG).
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A WGS84 position (EPSG:4326) with altitude above mean sea level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Position {
    /// Latitude in degrees, -90..=90.
    pub lat: f64,
    /// Longitude in degrees, -180..=180.
    pub lon: f64,
    /// Altitude above mean sea level in metres.
    pub alt_m: f64,
}

/// Error returned when a coordinate is out of range.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PositionError {
    #[error("latitude {0} out of range -90..=90")]
    Latitude(f64),
    #[error("longitude {0} out of range -180..=180")]
    Longitude(f64),
    #[error("altitude {0} is not finite")]
    Altitude(f64),
}

impl Position {
    /// Creates a validated position.
    ///
    /// # Errors
    /// Returns [`PositionError`] if latitude or longitude is out of range, or
    /// any value is not finite.
    pub fn new(lat: f64, lon: f64, alt_m: f64) -> Result<Self, PositionError> {
        // `contains` is false for NaN, so the range checks also reject NaN.
        if !(-90.0..=90.0).contains(&lat) {
            return Err(PositionError::Latitude(lat));
        }
        if !(-180.0..=180.0).contains(&lon) {
            return Err(PositionError::Longitude(lon));
        }
        if !alt_m.is_finite() {
            return Err(PositionError::Altitude(alt_m));
        }
        Ok(Self { lat, lon, alt_m })
    }

    /// Great-circle distance to `other` in metres (haversine, ignores altitude).
    ///
    /// Uses a spherical Earth, so the error is up to ~0.5 %. That is fine for
    /// "how far apart are two points" checks; anything that needs real metres
    /// (simplification, terrain sampling) projects to UTM instead.
    #[must_use]
    pub fn distance_m(&self, other: &Self) -> f64 {
        // Haversine: a = sin²(Δφ/2) + cos φ1 · cos φ2 · sin²(Δλ/2),
        // central angle = 2 · asin(√a).
        let (phi1, phi2) = (self.lat.to_radians(), other.lat.to_radians());
        let d_phi = phi2 - phi1;
        let d_lambda = (other.lon - self.lon).to_radians();
        let a =
            (d_phi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (d_lambda / 2.0).sin().powi(2);
        2.0 * EARTH_RADIUS_M * a.sqrt().asin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_coordinates() {
        assert_eq!(
            Position::new(91.0, 0.0, 0.0),
            Err(PositionError::Latitude(91.0))
        );
        assert_eq!(
            Position::new(0.0, -181.0, 0.0),
            Err(PositionError::Longitude(-181.0))
        );
        assert!(Position::new(f64::NAN, 0.0, 0.0).is_err());
        assert!(Position::new(0.0, 0.0, f64::INFINITY).is_err());
    }

    #[test]
    fn distance_munich_to_gilching() {
        let munich = Position::new(48.1374, 11.5755, 520.0).unwrap();
        let gilching = Position::new(48.1086, 11.2939, 560.0).unwrap();
        let d = munich.distance_m(&gilching);
        assert!((20_000.0..22_000.0).contains(&d), "got {d}");
    }

    #[test]
    fn distance_to_self_is_zero() {
        let p = Position::new(48.0, 11.0, 0.0).unwrap();
        assert!(p.distance_m(&p).abs() < 1e-9);
    }
}

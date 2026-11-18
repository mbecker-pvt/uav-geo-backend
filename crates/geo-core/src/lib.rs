//! Domain types shared by all crates of `uav-geo-backend`.

use std::fmt;

/// Mean Earth radius in metres (IUGG).
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A WGS84 position (EPSG:4326) with altitude above mean sea level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    /// Latitude in degrees, -90..=90.
    pub lat: f64,
    /// Longitude in degrees, -180..=180.
    pub lon: f64,
    /// Altitude above mean sea level in metres.
    pub alt_m: f64,
}

/// Error returned when a coordinate is out of range.
#[derive(Debug, Clone, PartialEq)]
pub enum PositionError {
    Latitude(f64),
    Longitude(f64),
}

impl fmt::Display for PositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latitude(v) => write!(f, "latitude {v} out of range -90..=90"),
            Self::Longitude(v) => write!(f, "longitude {v} out of range -180..=180"),
        }
    }
}

impl std::error::Error for PositionError {}

impl Position {
    /// Creates a validated position.
    ///
    /// # Errors
    /// Returns [`PositionError`] if latitude or longitude is out of range or not finite.
    pub fn new(lat: f64, lon: f64, alt_m: f64) -> Result<Self, PositionError> {
        if !(-90.0..=90.0).contains(&lat) {
            return Err(PositionError::Latitude(lat));
        }
        if !(-180.0..=180.0).contains(&lon) {
            return Err(PositionError::Longitude(lon));
        }
        Ok(Self { lat, lon, alt_m })
    }

    /// Great-circle distance to `other` in metres (haversine, ignores altitude).
    #[must_use]
    pub fn distance_m(&self, other: &Self) -> f64 {
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

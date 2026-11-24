//! WGS84 ↔ UTM conversion (transverse Mercator, Krüger series to n³).
//!
//! Accuracy is well below a millimetre within a zone (and its usual overlap),
//! which is far below GNSS noise. The Norway/Svalbard zone exceptions are not
//! applied: zones are always the regular 6° bands.

use serde::Serialize;

use crate::{Position, PositionError};

/// WGS84 semi-major axis in metres.
const A: f64 = 6_378_137.0;
/// WGS84 flattening.
const F: f64 = 1.0 / 298.257_223_563;
/// UTM central scale factor.
const K0: f64 = 0.9996;
/// False easting in metres.
const E0: f64 = 500_000.0;
/// False northing for the southern hemisphere in metres.
const N0_SOUTH: f64 = 10_000_000.0;

/// A UTM coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Utm {
    /// Zone number 1..=60.
    pub zone: u8,
    /// `true` for the northern hemisphere.
    pub north: bool,
    pub easting: f64,
    pub northing: f64,
}

/// Errors of UTM conversions.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum UtmError {
    #[error("UTM zone {0} out of range 1..=60")]
    Zone(u8),
    #[error("latitude {0} outside the UTM range -80..=84")]
    Latitude(f64),
    #[error(transparent)]
    Position(#[from] PositionError),
}

/// Series coefficients derived from the WGS84 third flattening `n`.
struct Coefficients {
    big_a: f64,
    alpha: [f64; 3],
    beta: [f64; 3],
    delta: [f64; 3],
    /// `2√n / (1 + n)`
    e_term: f64,
}

fn coefficients() -> Coefficients {
    let n = F / (2.0 - F);
    let (n2, n3) = (n * n, n * n * n);
    Coefficients {
        big_a: A / (1.0 + n) * (1.0 + n2 / 4.0 + n2 * n2 / 64.0),
        alpha: [
            n / 2.0 - 2.0 * n2 / 3.0 + 5.0 * n3 / 16.0,
            13.0 * n2 / 48.0 - 3.0 * n3 / 5.0,
            61.0 * n3 / 240.0,
        ],
        beta: [
            n / 2.0 - 2.0 * n2 / 3.0 + 37.0 * n3 / 96.0,
            n2 / 48.0 + n3 / 15.0,
            17.0 * n3 / 480.0,
        ],
        delta: [
            2.0 * n - 2.0 * n2 / 3.0 - 2.0 * n3,
            7.0 * n2 / 3.0 - 8.0 * n3 / 5.0,
            56.0 * n3 / 15.0,
        ],
        e_term: 2.0 * n.sqrt() / (1.0 + n),
    }
}

/// The regular UTM zone (1..=60) of a longitude.
#[must_use]
pub fn zone_for(lon: f64) -> u8 {
    // floor((lon + 180) / 6) + 1 is in 1..=61 for lon in -180..=180.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let zone = ((lon + 180.0) / 6.0).floor() as u8 + 1;
    zone.min(60)
}

fn central_meridian(zone: u8) -> f64 {
    f64::from(zone) * 6.0 - 183.0
}

/// Converts a position to UTM in its regular zone.
///
/// # Errors
/// Returns [`UtmError::Latitude`] outside -80..=84°.
pub fn to_utm(p: &Position) -> Result<Utm, UtmError> {
    to_utm_in_zone(p, zone_for(p.lon), p.lat >= 0.0)
}

/// Converts a position to UTM in a fixed zone and hemisphere.
///
/// Useful to project a whole track into one planar system even if it crosses
/// a zone boundary (the projection stays accurate a few degrees outside).
///
/// # Errors
/// Returns [`UtmError`] if the zone or latitude is out of range.
pub fn to_utm_in_zone(p: &Position, zone: u8, north: bool) -> Result<Utm, UtmError> {
    if !(1..=60).contains(&zone) {
        return Err(UtmError::Zone(zone));
    }
    if !(-80.0..=84.0).contains(&p.lat) {
        return Err(UtmError::Latitude(p.lat));
    }
    let c = coefficients();
    let phi = p.lat.to_radians();
    let dlambda = (p.lon - central_meridian(zone)).to_radians();

    let sin_phi = phi.sin();
    let t = (sin_phi.atanh() - c.e_term * (c.e_term * sin_phi).atanh()).sinh();
    let xi_p = t.atan2(dlambda.cos());
    let eta_p = (dlambda.sin() / (1.0 + t * t).sqrt()).atanh();

    let mut easting = eta_p;
    let mut northing = xi_p;
    for (j, alpha) in (1_u8..).zip(c.alpha) {
        let k = 2.0 * f64::from(j);
        easting += alpha * (k * xi_p).cos() * (k * eta_p).sinh();
        northing += alpha * (k * xi_p).sin() * (k * eta_p).cosh();
    }
    Ok(Utm {
        zone,
        north,
        easting: E0 + K0 * c.big_a * easting,
        northing: if north { 0.0 } else { N0_SOUTH } + K0 * c.big_a * northing,
    })
}

/// Converts a UTM coordinate back to WGS84. `alt_m` is passed through.
///
/// # Errors
/// Returns [`UtmError`] if the zone is invalid or the result is out of range.
pub fn from_utm(u: &Utm, alt_m: f64) -> Result<Position, UtmError> {
    if !(1..=60).contains(&u.zone) {
        return Err(UtmError::Zone(u.zone));
    }
    let c = coefficients();
    let n0 = if u.north { 0.0 } else { N0_SOUTH };
    let xi = (u.northing - n0) / (K0 * c.big_a);
    let eta = (u.easting - E0) / (K0 * c.big_a);

    let mut xi_p = xi;
    let mut eta_p = eta;
    for (j, beta) in (1_u8..).zip(c.beta) {
        let k = 2.0 * f64::from(j);
        xi_p -= beta * (k * xi).sin() * (k * eta).cosh();
        eta_p -= beta * (k * xi).cos() * (k * eta).sinh();
    }
    let chi = (xi_p.sin() / eta_p.cosh()).asin();
    let mut phi = chi;
    for (j, delta) in (1_u8..).zip(c.delta) {
        phi += delta * (2.0 * f64::from(j) * chi).sin();
    }
    let lambda = central_meridian(u.zone).to_radians() + eta_p.sinh().atan2(xi_p.cos());
    let mut lon = lambda.to_degrees();
    if lon > 180.0 {
        lon -= 360.0;
    } else if lon < -180.0 {
        lon += 360.0;
    }
    Ok(Position::new(phi.to_degrees(), lon, alt_m)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values computed with PROJ (pyproj, EPSG:326xx/327xx).
    #[allow(clippy::inconsistent_digit_grouping)]
    const REFERENCE: &[(f64, f64, u8, bool, f64, f64)] = &[
        (48.1374, 11.5755, 32, true, 691_603.0320, 5_334_780.0309),
        (0.0, 9.0, 32, true, 500_000.0, 0.0),
        (-33.8688, 151.2093, 56, false, 334_368.6336, 6_250_948.3454),
        (48.0, 12.0, 32, true, 723_775.9154, 5_320_655.7892),
        (48.0, 12.0, 33, true, 276_224.0846, 5_320_655.7892),
        (70.5, 25.3, 35, true, 436_673.1828, 7_822_519.0694),
        (-54.8, -68.3, 19, false, 545_000.0534, 3_927_239.3813),
    ];

    #[test]
    fn matches_proj_reference_within_a_millimetre() {
        for &(lat, lon, zone, north, e, n) in REFERENCE {
            let p = Position::new(lat, lon, 0.0).unwrap();
            let u = to_utm_in_zone(&p, zone, north).unwrap();
            assert!(
                (u.easting - e).abs() < 1e-3,
                "{lat},{lon}: E {} vs {e}",
                u.easting
            );
            assert!(
                (u.northing - n).abs() < 1e-3,
                "{lat},{lon}: N {} vs {n}",
                u.northing
            );
        }
    }

    #[test]
    fn round_trip_is_sub_millimetre() {
        for &(lat, lon, zone, north, _, _) in REFERENCE {
            let p = Position::new(lat, lon, 123.4).unwrap();
            let back = from_utm(&to_utm_in_zone(&p, zone, north).unwrap(), 123.4).unwrap();
            assert!(p.distance_m(&back) < 1e-3, "{lat},{lon}: {back:?}");
            assert!((back.alt_m - 123.4).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn picks_regular_zone_and_hemisphere() {
        assert_eq!(zone_for(-180.0), 1);
        assert_eq!(zone_for(11.5755), 32);
        assert_eq!(zone_for(12.0), 33);
        assert_eq!(zone_for(180.0), 60);
        let u = to_utm(&Position::new(-33.8688, 151.2093, 0.0).unwrap()).unwrap();
        assert_eq!((u.zone, u.north), (56, false));
    }

    #[test]
    fn rejects_polar_latitudes_and_bad_zones() {
        let pole = Position::new(85.0, 0.0, 0.0).unwrap();
        assert_eq!(to_utm(&pole), Err(UtmError::Latitude(85.0)));
        let p = Position::new(0.0, 0.0, 0.0).unwrap();
        assert_eq!(to_utm_in_zone(&p, 61, true), Err(UtmError::Zone(61)));
    }
}

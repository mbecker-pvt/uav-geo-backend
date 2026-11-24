//! Track simplification (Ramer–Douglas–Peucker in metres).

use geo::{LineString, SimplifyIdx};

use crate::Position;
use crate::utm::{UtmError, to_utm_in_zone, zone_for};

/// Simplifies a track and returns the indices of the points to keep.
///
/// The track is projected into the UTM zone of its first point, so
/// `tolerance_m` is a real horizontal distance in metres. Altitude is ignored
/// for the simplification; the caller keeps the original points (with
/// altitude and timestamps) by index. The first and last point are always kept.
///
/// # Errors
/// Returns [`UtmError`] if a point cannot be projected (polar latitudes).
pub fn simplify_track(points: &[Position], tolerance_m: f64) -> Result<Vec<usize>, UtmError> {
    let Some(first) = points.first() else {
        return Ok(Vec::new());
    };
    if points.len() < 3 || tolerance_m <= 0.0 || !tolerance_m.is_finite() {
        return Ok((0..points.len()).collect());
    }
    let (zone, north) = (zone_for(first.lon), first.lat >= 0.0);
    let coords = points
        .iter()
        .map(|p| to_utm_in_zone(p, zone, north).map(|u| (u.easting, u.northing)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LineString::from(coords).simplify_idx(tolerance_m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(lat: f64, lon: f64) -> Position {
        Position::new(lat, lon, 500.0).unwrap()
    }

    #[test]
    fn removes_collinear_points_and_keeps_corners() {
        // Straight line north, then a 90° turn east (≈ 111 m per 0.001° lat).
        let track = [
            p(48.000, 11.0),
            p(48.001, 11.0),
            p(48.002, 11.0),
            p(48.003, 11.0),
            p(48.003, 11.002),
            p(48.003, 11.004),
        ];
        assert_eq!(simplify_track(&track, 1.0).unwrap(), vec![0, 3, 5]);
    }

    #[test]
    fn tolerance_is_in_metres() {
        // Middle point deviates ≈ 7.4 m from the straight line.
        let track = [p(48.0, 11.0), p(48.001, 11.0001), p(48.002, 11.0)];
        assert_eq!(simplify_track(&track, 5.0).unwrap(), vec![0, 1, 2]);
        assert_eq!(simplify_track(&track, 10.0).unwrap(), vec![0, 2]);
    }

    #[test]
    fn short_or_disabled_returns_all() {
        assert_eq!(simplify_track(&[], 5.0).unwrap(), Vec::<usize>::new());
        let track = [p(48.0, 11.0), p(48.001, 11.0), p(48.002, 11.0)];
        assert_eq!(simplify_track(&track, 0.0).unwrap(), vec![0, 1, 2]);
    }
}

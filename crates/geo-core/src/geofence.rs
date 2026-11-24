//! Polygon geofences.

use geo::{Coord, Covers, Line, LineString, Point, Polygon};

use crate::Position;

/// An inclusion geofence: the vehicle must stay inside (or on the edge of)
/// the polygon.
///
/// Geometry is evaluated planar in lon/lat degrees. For fences of a few
/// kilometres the difference to geodesic edges is in the centimetre range.
#[derive(Debug, Clone, PartialEq)]
pub struct Geofence {
    pub name: String,
    polygon: Polygon<f64>,
}

/// Errors when building a [`Geofence`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GeofenceError {
    #[error("a geofence needs at least 3 distinct vertices")]
    TooFewVertices,
    #[error("geofence vertex {0} is out of range")]
    InvalidVertex(usize),
}

impl Geofence {
    /// Creates a geofence from an exterior ring of `(lon, lat)` pairs. The
    /// ring is closed automatically.
    ///
    /// # Errors
    /// Returns [`GeofenceError`] for invalid vertices or fewer than 3 vertices.
    pub fn new(name: impl Into<String>, ring: &[(f64, f64)]) -> Result<Self, GeofenceError> {
        for (i, &(lon, lat)) in ring.iter().enumerate() {
            if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                return Err(GeofenceError::InvalidVertex(i));
            }
        }
        let mut coords: Vec<Coord<f64>> = ring.iter().map(|&(x, y)| Coord { x, y }).collect();
        if coords.len() > 1 && coords.first() == coords.last() {
            coords.pop();
        }
        coords.dedup();
        if coords.len() < 3 {
            return Err(GeofenceError::TooFewVertices);
        }
        Ok(Self {
            name: name.into(),
            polygon: Polygon::new(LineString::from(coords), vec![]),
        })
    }

    /// The closed exterior ring as `(lon, lat)` pairs.
    #[must_use]
    pub fn ring(&self) -> Vec<(f64, f64)> {
        self.polygon
            .exterior()
            .coords()
            .map(|c| (c.x, c.y))
            .collect()
    }

    /// Whether `p` is inside or on the boundary.
    #[must_use]
    pub fn contains(&self, p: &Position) -> bool {
        self.polygon.covers(&Point::new(p.lon, p.lat))
    }

    /// Whether the straight segment `a → b` stays inside the fence.
    #[must_use]
    pub fn contains_segment(&self, a: &Position, b: &Position) -> bool {
        self.polygon.covers(&Line::new(
            Coord { x: a.lon, y: a.lat },
            Coord { x: b.lon, y: b.lat },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A U-shaped fence: the notch between x=1..2 for y>1 is outside.
    fn u_shape() -> Geofence {
        Geofence::new(
            "u",
            &[
                (0.0, 0.0),
                (3.0, 0.0),
                (3.0, 3.0),
                (2.0, 3.0),
                (2.0, 1.0),
                (1.0, 1.0),
                (1.0, 3.0),
                (0.0, 3.0),
                (0.0, 0.0),
            ],
        )
        .unwrap()
    }

    fn p(lon: f64, lat: f64) -> Position {
        Position::new(lat, lon, 0.0).unwrap()
    }

    #[test]
    fn point_containment_includes_boundary() {
        let f = u_shape();
        assert!(f.contains(&p(0.5, 2.0)));
        assert!(f.contains(&p(0.0, 1.5)));
        assert!(!f.contains(&p(1.5, 2.0)));
    }

    #[test]
    fn segment_crossing_the_notch_is_rejected() {
        let f = u_shape();
        assert!(f.contains_segment(&p(0.5, 0.5), &p(2.5, 0.5)));
        assert!(!f.contains_segment(&p(0.5, 2.5), &p(2.5, 2.5)));
    }

    #[test]
    fn validates_vertices() {
        assert_eq!(
            Geofence::new("x", &[(0.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
            Err(GeofenceError::TooFewVertices)
        );
        assert_eq!(
            Geofence::new("x", &[(0.0, 0.0), (1.0, 95.0), (1.0, 0.0)]),
            Err(GeofenceError::InvalidVertex(1))
        );
        assert_eq!(u_shape().ring().len(), 9);
    }
}

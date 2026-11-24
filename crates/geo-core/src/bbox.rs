//! Axis-aligned WGS84 bounding boxes.
//!
//! Parsed from the `bbox=minLon,minLat,maxLon,maxLat` query parameter. The
//! database query uses the values with `ST_MakeEnvelope` (index-backed); the
//! WebSocket filter uses [`BBox::contains`] in memory for each live position.

use std::str::FromStr;

use serde::Serialize;

use crate::Position;

/// A WGS84 bounding box. Boxes crossing the antimeridian are not supported.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BBox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

/// Errors when building or parsing a [`BBox`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BBoxError {
    #[error("bbox must be 'minLon,minLat,maxLon,maxLat'")]
    Format,
    #[error("bbox coordinates out of range")]
    Range,
    #[error("bbox min must be <= max (antimeridian crossing is not supported)")]
    Order,
}

impl BBox {
    /// Creates a validated bounding box.
    ///
    /// # Errors
    /// Returns [`BBoxError`] if values are out of range or min > max.
    pub fn new(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Result<Self, BBoxError> {
        let lon_ok = |v: f64| (-180.0..=180.0).contains(&v);
        let lat_ok = |v: f64| (-90.0..=90.0).contains(&v);
        if !(lon_ok(min_lon) && lon_ok(max_lon) && lat_ok(min_lat) && lat_ok(max_lat)) {
            return Err(BBoxError::Range);
        }
        if min_lon > max_lon || min_lat > max_lat {
            return Err(BBoxError::Order);
        }
        Ok(Self {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        })
    }

    /// Whether `p` lies inside or on the edge of the box.
    #[must_use]
    pub fn contains(&self, p: &Position) -> bool {
        (self.min_lon..=self.max_lon).contains(&p.lon)
            && (self.min_lat..=self.max_lat).contains(&p.lat)
    }
}

impl FromStr for BBox {
    type Err = BBoxError;

    /// Parses `minLon,minLat,maxLon,maxLat` (the `OGC` / `GeoJSON` order).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let values: Vec<f64> = s
            .split(',')
            .map(|v| v.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|_| BBoxError::Format)?;
        // Slice pattern: exactly four numbers, anything else is a format error.
        let [min_lon, min_lat, max_lon, max_lat] = values[..] else {
            return Err(BBoxError::Format);
        };
        Self::new(min_lon, min_lat, max_lon, max_lat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_contains() {
        let b: BBox = "11.0, 48.0, 12.0, 48.5".parse().unwrap();
        assert!(b.contains(&Position::new(48.1, 11.5, 0.0).unwrap()));
        assert!(b.contains(&Position::new(48.0, 11.0, 0.0).unwrap()));
        assert!(!b.contains(&Position::new(48.6, 11.5, 0.0).unwrap()));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!("1,2,3".parse::<BBox>(), Err(BBoxError::Format));
        assert_eq!("a,2,3,4".parse::<BBox>(), Err(BBoxError::Format));
        assert_eq!("0,0,200,1".parse::<BBox>(), Err(BBoxError::Range));
        assert_eq!("12,48,11,49".parse::<BBox>(), Err(BBoxError::Order));
        assert_eq!("NaN,0,1,1".parse::<BBox>(), Err(BBoxError::Range));
    }
}

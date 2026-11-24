//! Terrain elevation abstraction.

use crate::Position;

/// A source of terrain elevation (e.g. a DEM raster).
pub trait TerrainSource: Send + Sync {
    /// Terrain elevation above mean sea level in metres, or `None` if the
    /// location is outside the data or has no value.
    fn elevation_m(&self, lat: f64, lon: f64) -> Option<f64>;

    /// Height above ground level of `p`, or `None` without terrain data.
    fn agl_m(&self, p: &Position) -> Option<f64> {
        self.elevation_m(p.lat, p.lon)
            .map(|ground| p.alt_m - ground)
    }
}

/// Flat terrain at a constant elevation, useful for tests and demos.
#[derive(Debug, Clone, Copy)]
pub struct FlatTerrain(pub f64);

impl TerrainSource for FlatTerrain {
    fn elevation_m(&self, _lat: f64, _lon: f64) -> Option<f64> {
        Some(self.0)
    }
}

//! Terrain elevation abstraction.
//!
//! The trait lives here (not in the `terrain` crate) so mission validation
//! can ask for ground height without depending on GeoTIFF parsing. The real
//! implementation is `terrain::TerrainModel`; tests use [`FlatTerrain`] or
//! small test-only implementations. This is plain dependency inversion: the domain defines
//! what it needs, the infrastructure crate provides it.

use crate::Position;

/// A source of terrain elevation (e.g. a DEM raster).
pub trait TerrainSource: Send + Sync {
    /// Terrain elevation above mean sea level in metres, or `None` if the
    /// location is outside the data or has no value.
    fn elevation_m(&self, lat: f64, lon: f64) -> Option<f64>;

    /// Height above ground level of `p`, or `None` without terrain data.
    ///
    /// Both values are metres above mean sea level, so AGL is a plain
    /// difference. (PX4's `alt` and Copernicus DEMs both use MSL heights.)
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

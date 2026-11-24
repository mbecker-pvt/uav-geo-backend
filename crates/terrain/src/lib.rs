//! Terrain elevation from DEM rasters (GeoTIFF, pure Rust).
//!
//! Supported: single-band, north-up rasters in geographic WGS84 / ETRS89
//! coordinates (e.g. Copernicus GLO-30, SRTM), any integer or float sample
//! type, tiled or stripped, GDAL nodata. Elevations are bilinearly
//! interpolated between pixel centres.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};

use geo_core::TerrainSource;
use tiff::ColorType;
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

/// `GTModelTypeGeoKey`
const KEY_MODEL_TYPE: u16 = 1024;
/// `GTRasterTypeGeoKey`
const KEY_RASTER_TYPE: u16 = 1025;
/// `GeographicTypeGeoKey`
const KEY_GEOGRAPHIC_TYPE: u16 = 2048;
/// `ProjectedCSTypeGeoKey`
const KEY_PROJECTED_TYPE: u16 = 3072;
/// `ModelTypeGeographic`
const MODEL_TYPE_GEOGRAPHIC: u16 = 2;
/// `RasterPixelIsPoint`
const RASTER_PIXEL_IS_POINT: u16 = 2;
/// Accepted geographic CRS codes: WGS84 and ETRS89 (identical at DEM accuracy).
const ACCEPTED_CRS: [u16; 2] = [4326, 4258];

/// Errors when loading a DEM.
#[derive(Debug, thiserror::Error)]
pub enum DemError {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("TIFF error: {0}")]
    Tiff(#[from] tiff::TiffError),
    #[error("not a usable GeoTIFF DEM: {0}")]
    Unsupported(String),
    #[error("no GeoTIFF files found in {0}")]
    Empty(PathBuf),
}

/// One DEM raster held in memory.
#[derive(Debug, Clone)]
pub struct GeoTiffDem {
    width: usize,
    height: usize,
    /// Longitude of the centre of pixel (0, 0).
    lon0: f64,
    /// Latitude of the centre of pixel (0, 0).
    lat0: f64,
    /// Pixel size in degrees (positive).
    dx: f64,
    dy: f64,
    nodata: Option<f64>,
    data: Vec<f32>,
}

impl GeoTiffDem {
    /// Opens a GeoTIFF file.
    ///
    /// # Errors
    /// Returns [`DemError`] if the file cannot be read or is not a supported DEM.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DemError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| DemError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_reader(BufReader::new(file))
    }

    /// Reads a GeoTIFF from any seekable reader.
    ///
    /// # Errors
    /// Returns [`DemError`] if the data is not a supported DEM.
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self, DemError> {
        // DEM tiles are larger than the default decoding limits allow.
        let mut decoder = Decoder::new(reader)?.with_limits(Limits::unlimited());
        if !matches!(decoder.colortype()?, ColorType::Gray(_)) {
            return Err(DemError::Unsupported("DEM must be single-band".into()));
        }
        let (width, height) = decoder.dimensions()?;
        let (width, height) = (width as usize, height as usize);

        if decoder.find_tag(Tag::ModelTransformationTag)?.is_some() {
            return Err(DemError::Unsupported(
                "rotated rasters (ModelTransformation) are not supported".into(),
            ));
        }
        let scale = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag)?;
        let tie = decoder.get_tag_f64_vec(Tag::ModelTiepointTag)?;
        let (&[dx, dy, ..], &[ti, tj, _, tx, ty, ..]) = (&scale[..], &tie[..]) else {
            return Err(DemError::Unsupported(
                "invalid pixel scale / tiepoint".into(),
            ));
        };
        if !(dx > 0.0 && dy > 0.0) {
            return Err(DemError::Unsupported("pixel scale must be positive".into()));
        }

        let keys = decoder.get_tag_u16_vec(Tag::GeoKeyDirectoryTag)?;
        let key = |id: u16| geo_key(&keys, id);
        if key(KEY_PROJECTED_TYPE).is_some()
            || key(KEY_MODEL_TYPE).is_some_and(|t| t != MODEL_TYPE_GEOGRAPHIC)
        {
            return Err(DemError::Unsupported(
                "projected CRS; reproject the DEM to EPSG:4326".into(),
            ));
        }
        if let Some(crs) = key(KEY_GEOGRAPHIC_TYPE)
            && !ACCEPTED_CRS.contains(&crs)
        {
            return Err(DemError::Unsupported(format!("geographic CRS EPSG:{crs}")));
        }
        // PixelIsArea (default): the tiepoint is the outer corner of the pixel.
        let half = if key(KEY_RASTER_TYPE) == Some(RASTER_PIXEL_IS_POINT) {
            0.0
        } else {
            0.5
        };

        let nodata = decoder
            .get_tag_ascii_string(Tag::GdalNodata)
            .ok()
            .and_then(|s| s.trim_matches(char::from(0)).trim().parse::<f64>().ok());

        let data = to_f32(decoder.read_image()?)?;
        if data.len() != width * height {
            return Err(DemError::Unsupported("unexpected sample count".into()));
        }
        Ok(Self {
            width,
            height,
            lon0: tx + (half - ti) * dx,
            lat0: ty - (half - tj) * dy,
            dx,
            dy,
            nodata,
            data,
        })
    }

    /// Raster size `(width, height)` in pixels.
    #[must_use]
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn value(&self, col: usize, row: usize) -> Option<f64> {
        let v = f64::from(*self.data.get(row * self.width + col)?);
        let is_nodata = v.is_nan() || self.nodata.is_some_and(|nd| (v - nd).abs() < 1e-6);
        (!is_nodata).then_some(v)
    }

    /// Bilinearly interpolated elevation, `None` outside the raster or on nodata.
    #[must_use]
    pub fn elevation(&self, lat: f64, lon: f64) -> Option<f64> {
        // Fractional pixel coordinates relative to pixel centres.
        let fx = (lon - self.lon0) / self.dx;
        let fy = (self.lat0 - lat) / self.dy;
        #[allow(clippy::cast_precision_loss)]
        let (w, h) = (self.width as f64, self.height as f64);
        // Accept the outer half pixel at the edges (clamped).
        if !(-0.5..=w - 0.5).contains(&fx) || !(-0.5..=h - 0.5).contains(&fy) {
            return None;
        }
        let fx = fx.clamp(0.0, w - 1.0);
        let fy = fy.clamp(0.0, h - 1.0);
        // In range 0..width/height after the clamp above.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (c0, r0) = (fx.floor() as usize, fy.floor() as usize);
        let (c1, r1) = ((c0 + 1).min(self.width - 1), (r0 + 1).min(self.height - 1));
        #[allow(clippy::cast_precision_loss)]
        let (tx, ty) = (fx - c0 as f64, fy - r0 as f64);

        let corners = [
            (self.value(c0, r0), (1.0 - tx) * (1.0 - ty)),
            (self.value(c1, r0), tx * (1.0 - ty)),
            (self.value(c0, r1), (1.0 - tx) * ty),
            (self.value(c1, r1), tx * ty),
        ];
        // Renormalise over valid neighbours so nodata does not drag values to
        // the nodata constant; require the nearest pixel to be valid.
        let nearest = corners
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .and_then(|c| c.0);
        nearest?;
        let (sum, weight) = corners
            .iter()
            .filter_map(|&(v, w)| v.map(|v| (v * w, w)))
            .fold((0.0, 0.0), |(s, ws), (v, w)| (s + v, ws + w));
        (weight > 0.0).then(|| sum / weight)
    }
}

impl TerrainSource for GeoTiffDem {
    fn elevation_m(&self, lat: f64, lon: f64) -> Option<f64> {
        self.elevation(lat, lon)
    }
}

/// A set of DEM tiles; the first tile with data at a location wins.
#[derive(Debug, Clone, Default)]
pub struct TerrainModel {
    tiles: Vec<GeoTiffDem>,
}

impl TerrainModel {
    /// Loads a single GeoTIFF file or every `.tif`/`.tiff` file in a directory.
    ///
    /// # Errors
    /// Returns [`DemError`] if a file is invalid or a directory has no DEMs.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, DemError> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Ok(Self {
                tiles: vec![GeoTiffDem::open(path)?],
            });
        }
        let io_err = |source| DemError::Io {
            path: path.to_owned(),
            source,
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(io_err)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff")
                })
            })
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(DemError::Empty(path.to_owned()));
        }
        let tiles = files
            .iter()
            .map(GeoTiffDem::open)
            .collect::<Result<_, _>>()?;
        Ok(Self { tiles })
    }

    /// Creates a model from already loaded tiles.
    #[must_use]
    pub fn from_tiles(tiles: Vec<GeoTiffDem>) -> Self {
        Self { tiles }
    }

    /// Number of loaded tiles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// Whether no tile is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }
}

impl TerrainSource for TerrainModel {
    fn elevation_m(&self, lat: f64, lon: f64) -> Option<f64> {
        self.tiles.iter().find_map(|t| t.elevation(lat, lon))
    }
}

/// Looks up a short (inline) value in a `GeoKeyDirectory`.
fn geo_key(dir: &[u16], id: u16) -> Option<u16> {
    let count = usize::from(*dir.get(3)?);
    dir.get(4..4 + count * 4)?
        .chunks_exact(4)
        .find(|k| k[0] == id && k[1] == 0)
        .map(|k| k[3])
}

fn to_f32(result: DecodingResult) -> Result<Vec<f32>, DemError> {
    // Elevations fit f32 precision (≈ 0.1 mm at 9 km); precision loss is fine.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    Ok(match result {
        DecodingResult::F32(v) => v,
        DecodingResult::F64(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::F16(v) => v.into_iter().map(f32::from).collect(),
        DecodingResult::I8(v) => v.into_iter().map(f32::from).collect(),
        DecodingResult::U8(v) => v.into_iter().map(f32::from).collect(),
        DecodingResult::I16(v) => v.into_iter().map(f32::from).collect(),
        DecodingResult::U16(v) => v.into_iter().map(f32::from).collect(),
        DecodingResult::I32(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::U32(v) => v.into_iter().map(|x| x as f32).collect(),
        other @ (DecodingResult::I64(_) | DecodingResult::U64(_)) => {
            return Err(DemError::Unsupported(format!(
                "unsupported sample type {:?}",
                std::mem::discriminant(&other)
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tiff::encoder::{TiffEncoder, colortype::Gray32Float};

    use super::*;

    /// Writes a 4×3 Float32 GeoTIFF with pixel size 0.01° whose top-left
    /// corner is at (lon 11.0, lat 48.03). Values: 500 + 10·col + 100·row.
    fn fixture(raster_type: u16, crs: u16, nodata_at: Option<usize>) -> Vec<u8> {
        let (w, h) = (4_u32, 3_u32);
        #[allow(clippy::cast_precision_loss)]
        let mut data: Vec<f32> = (0..h)
            .flat_map(|r| (0..w).map(move |c| 500.0 + 10.0 * c as f32 + 100.0 * r as f32))
            .collect();
        if let Some(i) = nodata_at {
            data[i] = -9999.0;
        }
        let mut buf = Cursor::new(Vec::new());
        let mut enc = TiffEncoder::new(&mut buf).unwrap();
        let mut img = enc.new_image::<Gray32Float>(w, h).unwrap();
        let tags = img.encoder();
        tags.write_tag(Tag::ModelPixelScaleTag, &[0.01, 0.01, 0.0][..])
            .unwrap();
        tags.write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, 11.0, 48.03, 0.0][..],
        )
        .unwrap();
        let keys: [u16; 16] = [
            1,
            1,
            0,
            3, //
            KEY_MODEL_TYPE,
            0,
            1,
            MODEL_TYPE_GEOGRAPHIC, //
            KEY_RASTER_TYPE,
            0,
            1,
            raster_type, //
            KEY_GEOGRAPHIC_TYPE,
            0,
            1,
            crs,
        ];
        tags.write_tag(Tag::GeoKeyDirectoryTag, &keys[..]).unwrap();
        tags.write_tag(Tag::GdalNodata, "-9999").unwrap();
        img.write_data(&data).unwrap();
        buf.into_inner()
    }

    fn dem(raster_type: u16) -> GeoTiffDem {
        GeoTiffDem::from_reader(Cursor::new(fixture(raster_type, 4326, None))).unwrap()
    }

    #[test]
    fn samples_pixel_centres_exactly() {
        let d = dem(1);
        assert_eq!(d.dimensions(), (4, 3));
        // Centre of pixel (col 0, row 0) is half a pixel inside the corner.
        let v = d.elevation(48.025, 11.005).unwrap();
        assert!((v - 500.0).abs() < 1e-4, "{v}");
        let v = d.elevation(48.005, 11.035).unwrap(); // col 3, row 2
        assert!((v - 730.0).abs() < 1e-4, "{v}");
    }

    #[test]
    fn interpolates_bilinearly() {
        let d = dem(1);
        // Midway between cols 0/1 and rows 0/1: mean of 500, 510, 600, 610.
        let v = d.elevation(48.02, 11.01).unwrap();
        assert!((v - 555.0).abs() < 1e-4, "{v}");
    }

    #[test]
    fn pixel_is_point_shifts_grid_by_half_a_pixel() {
        let d = dem(RASTER_PIXEL_IS_POINT);
        let v = d.elevation(48.03, 11.0).unwrap();
        assert!((v - 500.0).abs() < 1e-4, "{v}");
    }

    #[test]
    fn outside_and_nodata_return_none() {
        let d = dem(1);
        assert_eq!(d.elevation(48.1, 11.0), None);
        assert_eq!(d.elevation(48.02, 10.9), None);
        // Edge half-pixel is clamped to the edge value.
        assert!(d.elevation(48.029, 11.001).is_some());

        let with_hole = GeoTiffDem::from_reader(Cursor::new(fixture(1, 4326, Some(0)))).unwrap();
        assert_eq!(with_hole.elevation(48.025, 11.005), None);
        // Neighbour of the hole: interpolated over valid pixels only.
        let v = with_hole.elevation(48.024, 11.014).unwrap();
        assert!((500.0..=620.0).contains(&v), "{v}");
    }

    #[test]
    fn rejects_unsupported_crs() {
        let err = GeoTiffDem::from_reader(Cursor::new(fixture(1, 4269, None))).unwrap_err();
        assert!(matches!(err, DemError::Unsupported(_)), "{err}");
    }

    #[test]
    fn model_uses_first_tile_with_data() {
        let m = TerrainModel::from_tiles(vec![dem(1)]);
        assert_eq!(m.len(), 1);
        let p = geo_core::Position::new(48.025, 11.005, 560.0).unwrap();
        assert!((m.agl_m(&p).unwrap() - 60.0).abs() < 1e-4);
        assert_eq!(m.elevation_m(0.0, 0.0), None);
    }
}

//! GeoJSON conversions for API input and output.
//!
//! GeoJSON (RFC 7946) is what map tools (geojson.io, Leaflet, QGIS) read and
//! write, so the API speaks it at the edges while the rest of the code uses
//! the `geo-core` types. Note the axis order: GeoJSON is `[lon, lat, alt]`,
//! [`Position::new`] takes `(lat, lon, alt)`; every swap happens in this file.
//!
//! Input parsing is deliberately lenient about the container (bare geometry,
//! `Feature`, `FeatureCollection`) and strict about the content (every
//! waypoint needs an altitude, fences must be simple polygons).

use geo_core::{Geofence, Position};
use geojson::{Feature, FeatureCollection, GeoJson, Geometry, GeometryValue, JsonObject};
use serde_json::{Value, json};
use storage::TrackPoint;

/// Invalid GeoJSON input (message is shown to the client).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GeoJsonError(pub String);

fn err(msg: impl Into<String>) -> GeoJsonError {
    GeoJsonError(msg.into())
}

fn object(v: Value) -> JsonObject {
    match v {
        Value::Object(o) => o,
        _ => JsonObject::new(),
    }
}

/// A track as a `LineString` feature (`[lon, lat, alt]`). Tracks with fewer
/// than two points have a `null` geometry.
#[must_use]
pub fn track_feature(system_id: u8, points: &[TrackPoint], extra: &Value) -> Feature {
    let geometry = (points.len() >= 2).then(|| {
        Geometry::new(GeometryValue::new_line_string(
            points
                .iter()
                .map(|p| [p.position.lon, p.position.lat, p.position.alt_m]),
        ))
    });
    let mut properties = object(json!({
        "system_id": system_id,
        "points": points.len(),
        "start": points.first().map(|p| p.time),
        "end": points.last().map(|p| p.time),
    }));
    if let Value::Object(e) = extra {
        properties.extend(e.clone());
    }
    Feature {
        geometry,
        properties: Some(properties),
        ..Feature::default()
    }
}

/// Positions as a `FeatureCollection` of `Point` features.
#[must_use]
pub fn positions_collection(points: &[TrackPoint]) -> FeatureCollection {
    let features = points
        .iter()
        .map(|p| Feature {
            geometry: Some(Geometry::new(GeometryValue::new_point([
                p.position.lon,
                p.position.lat,
                p.position.alt_m,
            ]))),
            properties: Some(object(json!({
                "system_id": p.system_id,
                "time": p.time,
                "relative_alt_m": p.relative_alt_m,
            }))),
            ..Feature::default()
        })
        .collect();
    FeatureCollection {
        bbox: None,
        features,
        foreign_members: None,
    }
}

/// Waypoints as a `LineString` feature (or `Point` for a single waypoint).
#[must_use]
pub fn waypoints_geometry(waypoints: &[Position]) -> Geometry {
    let coords = waypoints.iter().map(|p| [p.lon, p.lat, p.alt_m]);
    if waypoints.len() == 1 {
        Geometry::new(GeometryValue::new_point([
            waypoints[0].lon,
            waypoints[0].lat,
            waypoints[0].alt_m,
        ]))
    } else {
        Geometry::new(GeometryValue::new_line_string(coords))
    }
}

fn parse_geojson(value: &Value) -> Result<GeoJson, GeoJsonError> {
    serde_json::from_value(value.clone()).map_err(|e| err(format!("invalid GeoJSON: {e}")))
}

fn waypoint(coords: &[f64], index: usize) -> Result<Position, GeoJsonError> {
    // The altitude is mandatory: a 2D waypoint would leave the flight height
    // undefined, and the AGL check could not run.
    let &[lon, lat, alt, ..] = coords else {
        return Err(err(format!(
            "waypoint {index}: coordinates must be [lon, lat, alt_amsl]"
        )));
    };
    Position::new(lat, lon, alt).map_err(|e| err(format!("waypoint {index}: {e}")))
}

fn geometry_waypoints(g: &Geometry) -> Result<Vec<Position>, GeoJsonError> {
    let coords: Vec<&[f64]> = match &g.value {
        GeometryValue::Point { coordinates } => vec![coordinates.as_slice()],
        GeometryValue::LineString { coordinates } | GeometryValue::MultiPoint { coordinates } => {
            coordinates
                .iter()
                .map(geojson::Position::as_slice)
                .collect()
        }
        other => {
            return Err(err(format!(
                "unsupported geometry {} for waypoints (use LineString, MultiPoint or Point)",
                other.type_name()
            )));
        }
    };
    coords
        .iter()
        .enumerate()
        .map(|(i, c)| waypoint(c, i))
        .collect()
}

/// Parses mission waypoints from a `LineString`/`MultiPoint`/`Point`
/// geometry, a `Feature` with such a geometry, or a `FeatureCollection` of
/// `Point` features (in order). Every coordinate needs an altitude (AMSL).
///
/// # Errors
/// Returns [`GeoJsonError`] describing the first problem.
pub fn parse_waypoints(value: &Value) -> Result<Vec<Position>, GeoJsonError> {
    match parse_geojson(value)? {
        GeoJson::Geometry(g) => geometry_waypoints(&g),
        GeoJson::Feature(f) => geometry_waypoints(
            f.geometry
                .as_ref()
                .ok_or_else(|| err("feature has no geometry"))?,
        ),
        GeoJson::FeatureCollection(fc) => fc
            .features
            .iter()
            .enumerate()
            .map(|(i, f)| match f.geometry.as_ref().map(|g| &g.value) {
                Some(GeometryValue::Point { coordinates }) => waypoint(coordinates.as_slice(), i),
                _ => Err(err(format!("feature {i}: expected a Point geometry"))),
            })
            .collect(),
    }
}

/// Parses a geofence from a `Polygon` geometry or a `Feature` with a
/// `Polygon` (name from `properties.name`). Holes are not supported.
///
/// # Errors
/// Returns [`GeoJsonError`] if the input is not a valid simple polygon.
pub fn parse_geofence(value: &Value) -> Result<Geofence, GeoJsonError> {
    let (geometry, name) = match parse_geojson(value)? {
        GeoJson::Geometry(g) => (g, None),
        GeoJson::Feature(f) => {
            let name = f
                .properties
                .as_ref()
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            (
                f.geometry.ok_or_else(|| err("feature has no geometry"))?,
                name,
            )
        }
        GeoJson::FeatureCollection(_) => {
            return Err(err("expected a Polygon geometry or Feature"));
        }
    };
    let GeometryValue::Polygon { coordinates } = &geometry.value else {
        return Err(err(format!(
            "geofence must be a Polygon, got {}",
            geometry.value.type_name()
        )));
    };
    // GeoJSON polygon = [exterior ring, hole 1, hole 2, ...]. Holes (no-fly
    // islands inside the fence) are not modelled, so reject them rather
    // than silently ignore them.
    let [exterior] = coordinates.as_slice() else {
        return Err(err(
            "geofence polygon must have exactly one ring (no holes)",
        ));
    };
    let ring: Vec<(f64, f64)> = exterior
        .iter()
        .map(|p| match p.as_slice() {
            &[lon, lat, ..] => Ok((lon, lat)),
            _ => Err(err("polygon coordinates must be [lon, lat]")),
        })
        .collect::<Result<_, _>>()?;
    Geofence::new(name.unwrap_or_else(|| "geofence".to_owned()), &ring)
        .map_err(|e| err(e.to_string()))
}

/// The geofence as a GeoJSON `Feature`.
#[must_use]
pub fn geofence_feature(id: Option<i32>, fence: &Geofence) -> Feature {
    let ring: Vec<[f64; 2]> = fence.ring().iter().map(|&(lon, lat)| [lon, lat]).collect();
    Feature {
        geometry: Some(Geometry::new(GeometryValue::new_polygon([ring]))),
        properties: Some(object(json!({ "id": id, "name": fence.name }))),
        ..Feature::default()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn parses_waypoints_from_all_shapes() {
        let line = json!({"type": "LineString", "coordinates": [[11.0, 48.0, 600.0], [11.1, 48.1, 610.0]]});
        let wps = parse_waypoints(&line).unwrap();
        assert_eq!(wps.len(), 2);
        assert_eq!((wps[1].lat, wps[1].lon, wps[1].alt_m), (48.1, 11.1, 610.0));

        let feature = json!({"type": "Feature", "properties": null, "geometry": line});
        assert_eq!(parse_waypoints(&feature).unwrap(), wps);

        let fc = json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [11.0, 48.0, 600.0]}},
            {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [11.1, 48.1, 610.0]}}
        ]});
        assert_eq!(parse_waypoints(&fc).unwrap(), wps);
    }

    #[test]
    fn rejects_waypoints_without_altitude_or_out_of_range() {
        let no_alt = json!({"type": "LineString", "coordinates": [[11.0, 48.0], [11.1, 48.1]]});
        assert!(parse_waypoints(&no_alt).unwrap_err().0.contains("alt_amsl"));
        let bad = json!({"type": "Point", "coordinates": [11.0, 95.0, 600.0]});
        assert!(parse_waypoints(&bad).unwrap_err().0.contains("latitude"));
        let polygon = json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]});
        assert!(
            parse_waypoints(&polygon)
                .unwrap_err()
                .0
                .contains("unsupported")
        );
    }

    #[test]
    fn parses_geofence_feature_and_rejects_holes() {
        let ring = json!([
            [11.0, 48.0],
            [11.2, 48.0],
            [11.2, 48.2],
            [11.0, 48.2],
            [11.0, 48.0]
        ]);
        let f = json!({"type": "Feature", "properties": {"name": "field"},
                       "geometry": {"type": "Polygon", "coordinates": [ring]}});
        let fence = parse_geofence(&f).unwrap();
        assert_eq!(fence.name, "field");
        assert_eq!(fence.ring().len(), 5);

        let holes = json!({"type": "Polygon", "coordinates": [ring, ring]});
        assert!(parse_geofence(&holes).unwrap_err().0.contains("holes"));
    }

    #[test]
    fn track_feature_is_a_linestring() {
        let p = |s: i64, lat: f64| TrackPoint {
            system_id: 1,
            time: Utc::now() + chrono::Duration::seconds(s),
            position: Position::new(lat, 11.0, 560.0).unwrap(),
            relative_alt_m: 40.0,
        };
        let f = track_feature(1, &[p(0, 48.0), p(1, 48.1)], &json!({"simplified": false}));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["geometry"]["type"], "LineString");
        assert_eq!(v["geometry"]["coordinates"][1], json!([11.0, 48.1, 560.0]));
        assert_eq!(v["properties"]["points"], 2);
        assert_eq!(v["properties"]["simplified"], false);
        assert!(
            track_feature(1, &[p(0, 48.0)], &Value::Null)
                .geometry
                .is_none()
        );
    }
}

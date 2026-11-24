//! Missions and their validation against a geofence and terrain clearance.

use serde::Serialize;

use crate::utm::{from_utm, to_utm_in_zone, zone_for};
use crate::{Geofence, Position, TerrainSource};

/// Maximum number of waypoints accepted in one mission.
pub const MAX_WAYPOINTS: usize = 500;

/// A list of waypoints (altitude above mean sea level) for one vehicle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mission {
    pub system_id: u8,
    pub waypoints: Vec<Position>,
}

/// Validation parameters.
#[derive(Clone, Copy)]
pub struct MissionRules<'a> {
    /// Inclusion fence; `None` disables the fence check.
    pub geofence: Option<&'a Geofence>,
    /// Terrain model; `None` disables the AGL check.
    pub terrain: Option<&'a dyn TerrainSource>,
    /// Minimum height above ground in metres.
    pub min_agl_m: f64,
    /// Spacing of terrain samples along each segment in metres.
    pub sample_spacing_m: f64,
}

impl std::fmt::Debug for MissionRules<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MissionRules")
            .field("geofence", &self.geofence.map(|g| &g.name))
            .field("terrain", &self.terrain.is_some())
            .field("min_agl_m", &self.min_agl_m)
            .field("sample_spacing_m", &self.sample_spacing_m)
            .finish()
    }
}

/// A single rule violation.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Violation {
    /// The mission has no waypoints.
    Empty,
    /// More than [`MAX_WAYPOINTS`] waypoints.
    TooManyWaypoints { count: usize, max: usize },
    /// A waypoint lies outside the geofence.
    OutsideGeofence { waypoint: usize },
    /// The straight leg between two waypoints leaves the geofence.
    LeavesGeofence { segment: usize },
    /// Clearance below the minimum (first offending sample of a leg).
    BelowMinAgl {
        /// Index of the leg starting at this waypoint; `None` for a
        /// single-waypoint mission.
        segment: Option<usize>,
        at: Position,
        agl_m: f64,
        min_agl_m: f64,
    },
    /// No terrain data at a sample, so clearance cannot be verified.
    NoTerrainData {
        segment: Option<usize>,
        at: Position,
    },
}

impl Mission {
    /// Validates the mission. Collects all violations instead of stopping at
    /// the first one, so the client can show every problem at once.
    ///
    /// # Errors
    /// Returns the list of [`Violation`]s if any rule is broken.
    pub fn validate(&self, rules: &MissionRules<'_>) -> Result<(), Vec<Violation>> {
        let wps = &self.waypoints;
        if wps.is_empty() {
            return Err(vec![Violation::Empty]);
        }
        if wps.len() > MAX_WAYPOINTS {
            return Err(vec![Violation::TooManyWaypoints {
                count: wps.len(),
                max: MAX_WAYPOINTS,
            }]);
        }
        let mut violations = Vec::new();

        if let Some(fence) = rules.geofence {
            for (i, wp) in wps.iter().enumerate() {
                if !fence.contains(wp) {
                    violations.push(Violation::OutsideGeofence { waypoint: i });
                }
            }
            for (i, pair) in wps.windows(2).enumerate() {
                if fence.contains(&pair[0])
                    && fence.contains(&pair[1])
                    && !fence.contains_segment(&pair[0], &pair[1])
                {
                    violations.push(Violation::LeavesGeofence { segment: i });
                }
            }
        }

        if let Some(terrain) = rules.terrain {
            if wps.len() == 1 {
                check_clearance(terrain, rules.min_agl_m, None, &wps[0], &mut violations);
            }
            for (i, pair) in wps.windows(2).enumerate() {
                for sample in sample_segment(&pair[0], &pair[1], rules.sample_spacing_m) {
                    if check_clearance(terrain, rules.min_agl_m, Some(i), &sample, &mut violations)
                    {
                        // Report only the first offending sample per leg.
                        break;
                    }
                }
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// Pushes a violation if `p` is too low or unverifiable; returns whether it did.
fn check_clearance(
    terrain: &dyn TerrainSource,
    min_agl_m: f64,
    segment: Option<usize>,
    p: &Position,
    out: &mut Vec<Violation>,
) -> bool {
    match terrain.agl_m(p) {
        Some(agl_m) if agl_m >= min_agl_m => false,
        Some(agl_m) => {
            out.push(Violation::BelowMinAgl {
                segment,
                at: *p,
                agl_m,
                min_agl_m,
            });
            true
        }
        None => {
            out.push(Violation::NoTerrainData { segment, at: *p });
            true
        }
    }
}

/// Points from `a` to `b` (both included) at most `spacing_m` apart.
///
/// Interpolation is linear in the UTM zone of `a` (straight line on the
/// map), altitude is interpolated linearly. Falls back to the end points
/// if a point cannot be projected.
#[must_use]
pub fn sample_segment(a: &Position, b: &Position, spacing_m: f64) -> Vec<Position> {
    let (zone, north) = (zone_for(a.lon), a.lat >= 0.0);
    let (Ok(ua), Ok(ub)) = (
        to_utm_in_zone(a, zone, north),
        to_utm_in_zone(b, zone, north),
    ) else {
        return vec![*a, *b];
    };
    let length = (ub.easting - ua.easting).hypot(ub.northing - ua.northing);
    let spacing = if spacing_m.is_finite() && spacing_m > 0.0 {
        spacing_m
    } else {
        length.max(1.0)
    };
    // Number of steps is small (leg length / spacing); clamp defensively.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = ((length / spacing).ceil() as usize).clamp(1, 100_000);
    #[allow(clippy::cast_precision_loss)]
    let steps_f = steps as f64;
    (0..=steps)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f64 / steps_f;
            if i == 0 {
                return *a;
            }
            if i == steps {
                return *b;
            }
            let mut u = ua;
            u.easting += (ub.easting - ua.easting) * t;
            u.northing += (ub.northing - ua.northing) * t;
            from_utm(&u, a.alt_m + (b.alt_m - a.alt_m) * t).unwrap_or(*a)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::FlatTerrain;

    fn p(lat: f64, lon: f64, alt: f64) -> Position {
        Position::new(lat, lon, alt).unwrap()
    }

    fn fence() -> Geofence {
        Geofence::new(
            "box",
            &[(11.0, 48.0), (11.1, 48.0), (11.1, 48.1), (11.0, 48.1)],
        )
        .unwrap()
    }

    /// Terrain with a 100 m high ridge between lon 11.05 and 11.06.
    struct Ridge;
    impl TerrainSource for Ridge {
        fn elevation_m(&self, _lat: f64, lon: f64) -> Option<f64> {
            if lon > 11.09 {
                return None;
            }
            Some(if (11.05..=11.06).contains(&lon) {
                600.0
            } else {
                500.0
            })
        }
    }

    fn rules<'a>(fence: &'a Geofence, terrain: &'a dyn TerrainSource) -> MissionRules<'a> {
        MissionRules {
            geofence: Some(fence),
            terrain: Some(terrain),
            min_agl_m: 30.0,
            sample_spacing_m: 25.0,
        }
    }

    #[test]
    fn valid_mission_passes() {
        let f = fence();
        let m = Mission {
            system_id: 1,
            waypoints: vec![p(48.01, 11.01, 560.0), p(48.05, 11.04, 560.0)],
        };
        assert_eq!(m.validate(&rules(&f, &FlatTerrain(500.0))), Ok(()));
    }

    #[test]
    fn reports_geofence_violations() {
        let f = fence();
        let m = Mission {
            system_id: 1,
            waypoints: vec![p(48.01, 11.01, 560.0), p(48.2, 11.01, 560.0)],
        };
        let errs = m.validate(&rules(&f, &FlatTerrain(500.0))).unwrap_err();
        assert_eq!(errs, vec![Violation::OutsideGeofence { waypoint: 1 }]);
    }

    #[test]
    fn detects_ridge_between_waypoints() {
        // Both waypoints are 60 m AGL, but the leg crosses the 100 m ridge.
        let f = fence();
        let m = Mission {
            system_id: 1,
            waypoints: vec![p(48.05, 11.02, 560.0), p(48.05, 11.08, 560.0)],
        };
        let errs = m.validate(&rules(&f, &Ridge)).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        let Violation::BelowMinAgl {
            segment, at, agl_m, ..
        } = &errs[0]
        else {
            panic!("unexpected {errs:?}");
        };
        assert_eq!(*segment, Some(0));
        assert!((11.05..=11.06).contains(&at.lon));
        assert!((agl_m + 40.0).abs() < 1e-6);
    }

    #[test]
    fn missing_terrain_is_a_violation() {
        let f = fence();
        let m = Mission {
            system_id: 1,
            waypoints: vec![p(48.05, 11.095, 700.0)],
        };
        let errs = m.validate(&rules(&f, &Ridge)).unwrap_err();
        assert!(matches!(
            errs[0],
            Violation::NoTerrainData { segment: None, .. }
        ));
    }

    #[test]
    fn empty_mission_is_rejected() {
        let m = Mission {
            system_id: 1,
            waypoints: vec![],
        };
        let r = MissionRules {
            geofence: None,
            terrain: None,
            min_agl_m: 0.0,
            sample_spacing_m: 10.0,
        };
        assert_eq!(m.validate(&r), Err(vec![Violation::Empty]));
    }

    #[test]
    fn samples_are_evenly_spaced_and_include_endpoints() {
        let a = p(48.0, 11.0, 500.0);
        let b = p(48.0, 11.01, 600.0); // ≈ 744 m east
        let s = sample_segment(&a, &b, 100.0);
        assert_eq!(s.len(), 9);
        assert_eq!((s[0], s[8]), (a, b));
        for w in s.windows(2) {
            let d = w[0].distance_m(&w[1]);
            assert!((90.0..=100.0).contains(&d), "{d}");
        }
        assert!((s[4].alt_m - 550.0).abs() < 1e-9);
    }
}

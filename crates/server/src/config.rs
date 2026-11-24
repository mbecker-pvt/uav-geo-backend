//! Configuration from environment variables.
//!
//! Twelve-factor style: everything comes from the environment (or `.env` via
//! Docker Compose), so the same binary runs locally, in Docker and in CI.
//! Only `DATABASE_URL` is required; everything else has a default. Empty
//! values count as unset, so `DEM_PATH=` in Compose disables the DEM.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// `DATABASE_URL` (required).
    pub database_url: String,
    /// `MAVLINK_ADDR`, default `udpin:0.0.0.0:14550`.
    pub mavlink_addr: String,
    /// `HTTP_ADDR`, default `127.0.0.1:8080` (loopback only; the Docker image
    /// sets `0.0.0.0:8080`).
    pub http_addr: SocketAddr,
    /// `DEM_PATH`: GeoTIFF file or directory (optional).
    pub dem_path: Option<PathBuf>,
    /// `GEOFENCE_PATH`: GeoJSON polygon loaded as active fence on start (optional).
    pub geofence_path: Option<PathBuf>,
    /// `MIN_AGL_M`, default 30.
    pub min_agl_m: f64,
    /// `TERRAIN_SAMPLE_M`: spacing of terrain checks along mission legs, default 25.
    pub terrain_sample_m: f64,
    /// `BUFFER_CAPACITY`: positions kept while the database is down, default 100 000.
    pub buffer_capacity: usize,
    /// `LINK_TIMEOUT_S`: heartbeat timeout, default 3.
    pub link_timeout: Duration,
    /// `SHUTDOWN_FLUSH_S`: max time for the final buffer flush, default 5.
    pub shutdown_flush: Duration,
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {name}: {value:?}")]
    Invalid { name: &'static str, value: String },
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for missing or unparsable variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Reads the configuration from an arbitrary lookup (used by tests).
    ///
    /// Taking a lookup function instead of reading `std::env` directly lets
    /// tests pass a `HashMap` without mutating the process environment
    /// (which is shared by all test threads and `unsafe` in edition 2024).
    ///
    /// # Errors
    /// Returns [`ConfigError`] for missing or unparsable variables.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        fn parse<T: std::str::FromStr>(
            get: &impl Fn(&str) -> Option<String>,
            name: &'static str,
            default: T,
        ) -> Result<T, ConfigError> {
            match get(name).filter(|v| !v.trim().is_empty()) {
                None => Ok(default),
                Some(v) => v
                    .trim()
                    .parse()
                    .map_err(|_| ConfigError::Invalid { name, value: v }),
            }
        }
        let path = |name| {
            get(name)
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
        };
        Ok(Self {
            database_url: get("DATABASE_URL").ok_or(ConfigError::Missing("DATABASE_URL"))?,
            mavlink_addr: get("MAVLINK_ADDR").unwrap_or_else(|| "udpin:0.0.0.0:14550".to_owned()),
            http_addr: parse(&get, "HTTP_ADDR", SocketAddr::from(([127, 0, 0, 1], 8080)))?,
            dem_path: path("DEM_PATH"),
            geofence_path: path("GEOFENCE_PATH"),
            min_agl_m: parse(&get, "MIN_AGL_M", 30.0)?,
            terrain_sample_m: parse(&get, "TERRAIN_SAMPLE_M", 25.0)?,
            buffer_capacity: parse(&get, "BUFFER_CAPACITY", 100_000)?,
            link_timeout: Duration::from_secs(parse(&get, "LINK_TIMEOUT_S", 3)?),
            shutdown_flush: Duration::from_secs(parse(&get, "SHUTDOWN_FLUSH_S", 5)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn applies_defaults() {
        let c = Config::from_lookup(env(&[("DATABASE_URL", "postgres://x")])).unwrap();
        assert_eq!(c.http_addr, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(c.mavlink_addr, "udpin:0.0.0.0:14550");
        assert!(c.dem_path.is_none());
        assert!((c.min_agl_m - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reports_missing_and_invalid() {
        assert!(matches!(
            Config::from_lookup(env(&[])),
            Err(ConfigError::Missing("DATABASE_URL"))
        ));
        let err =
            Config::from_lookup(env(&[("DATABASE_URL", "x"), ("MIN_AGL_M", "high")])).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "MIN_AGL_M",
                ..
            }
        ));
    }
}

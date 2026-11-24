//! `uav-geo-backend` service: ingestion pipeline, REST and WebSocket API.
//!
//! This crate is the composition root: it wires the libraries (`telemetry`,
//! `storage`, `terrain`, `geo-core`) into a running service. It is a library
//! plus a thin binary (`main.rs`) so the end-to-end tests can start the whole
//! service in-process with [`start`] on random ports.
//!
//! | Module | Role |
//! |---|---|
//! | [`config`] | environment variables → [`Config`] |
//! | [`app`] | starts all tasks, owns shutdown order |
//! | [`ingest`] | telemetry events → live broadcast, write buffer, vehicle state |
//! | [`buffer`] | batches positions into PostGIS, survives database outages |
//! | [`state`] | [`state::AppState`] shared by handlers, WebSocket message types |
//! | [`api`] | axum routes (REST + `/live` WebSocket) |
//! | [`geo_json`] | conversions between domain types and GeoJSON |

pub mod api;
pub mod app;
pub mod buffer;
pub mod config;
pub mod geo_json;
pub mod ingest;
pub mod state;

pub use app::{Running, shutdown_signal, start};
pub use config::Config;

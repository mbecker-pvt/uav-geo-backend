//! `uav-geo-backend` service: ingestion pipeline, REST and WebSocket API.

pub mod api;
pub mod app;
pub mod buffer;
pub mod config;
pub mod geo_json;
pub mod ingest;
pub mod state;

pub use app::{Running, shutdown_signal, start};
pub use config::Config;

//! Wiring of all tasks and graceful shutdown.
//!
//! ```text
//! MavLink::run ──mpsc<TelemetryEvent>──▶ ingest ──broadcast<LiveMessage>──▶ /live
//!                                          │
//!                                          └──mpsc<PositionRow>──▶ write buffer ──▶ PostGIS
//! ```
//! Shutdown order (one `CancellationToken`): HTTP stops accepting and drains,
//! WebSockets close, the receive loop and ingest stop, the write buffer does a
//! final flush (bounded), then the pool closes.
//!
//! Every long-running part is its own `tokio` task connected by channels.
//! Channels are bounded, so a slow consumer slows its producer down instead
//! of growing memory without limit; the one exception is the live broadcast,
//! where slow WebSocket clients skip messages instead (see `api::ws`).
//!
//! Startup fails fast on configuration problems (bad DEM, port in use), but
//! deliberately **not** on a missing database: telemetry is the data that
//! cannot be re-sent later, so the service starts, buffers it and keeps
//! retrying the database in the background.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use geo_core::{Geofence, TerrainSource};
use storage::Db;
use telemetry::MavLink;
use terrain::TerrainModel;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::buffer::{BufferConfig, BufferStats, run_writer};
use crate::config::Config;
use crate::state::{AppState, LIVE_CHANNEL_CAPACITY};
use crate::{api, geo_json, ingest};

/// A started application.
#[derive(Debug)]
pub struct Running {
    /// Address the HTTP server is bound to (useful with port 0).
    pub http_addr: SocketAddr,
    /// Cancel to shut down gracefully.
    pub shutdown: CancellationToken,
    tasks: Vec<(&'static str, JoinHandle<()>)>,
    db: Db,
}

impl Running {
    /// Waits until all tasks have finished after `shutdown` was cancelled.
    pub async fn wait(self) {
        for (name, task) in self.tasks {
            if let Err(e) = task.await {
                tracing::error!(task = name, error = %e, "task panicked");
            }
        }
        self.db.close().await;
        tracing::info!("shutdown complete");
    }
}

fn load_terrain(path: &std::path::Path) -> anyhow::Result<Arc<dyn TerrainSource>> {
    let model =
        TerrainModel::load(path).with_context(|| format!("loading DEM from {}", path.display()))?;
    tracing::info!(tiles = model.len(), path = %path.display(), "DEM loaded");
    Ok(Arc::new(model))
}

fn load_geofence(path: &std::path::Path) -> anyhow::Result<Geofence> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading geofence {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).context("geofence is not JSON")?;
    let fence = geo_json::parse_geofence(&value).map_err(|e| anyhow::anyhow!(e.0))?;
    tracing::info!(name = %fence.name, "default geofence loaded");
    Ok(fence)
}

/// Applies migrations, retrying in the background until the database is up.
/// Telemetry is buffered meanwhile.
fn spawn_migrations(db: Db, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut delay = Duration::from_millis(500);
        loop {
            match db.migrate().await {
                Ok(()) => {
                    tracing::info!("database migrations applied");
                    return;
                }
                Err(e) => tracing::warn!(error = %e, "database not ready, retrying migrations"),
            }
            // Exponential backoff 0.5 s → 1 → 2 → ... → 10 s, interruptible
            // by shutdown.
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(10));
        }
    })
}

/// Starts all tasks and the HTTP server.
///
/// # Errors
/// Fails if the DEM, geofence file, MAVLink address or HTTP address cannot be
/// used. A database outage is **not** an error: it is retried.
pub async fn start(config: Config) -> anyhow::Result<Running> {
    // One token for the whole app; cancelling it starts the shutdown.
    let shutdown = CancellationToken::new();
    // Lazy: no connection is opened here, so a down database does not stop
    // startup (only an unparsable URL does).
    let db = Db::connect_lazy(&config.database_url, 10).context("invalid DATABASE_URL")?;

    let terrain = match &config.dem_path {
        Some(p) => {
            let p = p.clone();
            // Decoding a 50 MB GeoTIFF is CPU-bound, blocking file I/O; run it
            // on the blocking pool so it does not stall the async runtime.
            // `??`: first for the task join error, then for the load error.
            Some(tokio::task::spawn_blocking(move || load_terrain(&p)).await??)
        }
        None => None,
    };
    let default_geofence = config
        .geofence_path
        .as_deref()
        .map(load_geofence)
        .transpose()?
        .map(Arc::new);

    // Bind both sockets before spawning anything, so "port in use" is a
    // startup error, not a log line from a background task.
    let link = MavLink::connect(&config.mavlink_addr).await?;
    tracing::info!(addr = %config.mavlink_addr, "MAVLink listening");
    let listener = TcpListener::bind(config.http_addr)
        .await
        .with_context(|| format!("binding HTTP to {}", config.http_addr))?;
    let http_addr = listener.local_addr()?;

    let (live, _) = broadcast::channel(LIVE_CHANNEL_CAPACITY);
    let buffer_stats = Arc::new(BufferStats::default());
    let state = AppState {
        db: db.clone(),
        live,
        vehicles: Arc::new(RwLock::new(std::collections::BTreeMap::new())),
        terrain,
        default_geofence,
        link: Some(link.clone()),
        buffer_stats: buffer_stats.clone(),
        min_agl_m: config.min_agl_m,
        terrain_sample_m: config.terrain_sample_m,
        shutdown: shutdown.clone(),
    };

    // The write buffer gets its own token so it outlives the producers and
    // can drain everything they sent.
    let writer_cancel = CancellationToken::new();
    // Receive loop → ingest (decoded events), ingest → writer (rows to store).
    // Sizes are a few seconds of 50 Hz telemetry; the writer has its own,
    // much larger, in-memory buffer for database outages.
    let (event_tx, event_rx) = mpsc::channel(1024);
    let (row_tx, row_rx) = mpsc::channel(4096);

    let migrations = spawn_migrations(db.clone(), shutdown.clone());
    let receiver = tokio::spawn({
        let (link, cancel) = (link.clone(), shutdown.clone());
        async move { link.run(event_tx, cancel).await }
    });
    let ingest = tokio::spawn(ingest::run_ingest(
        state.clone(),
        event_rx,
        row_tx,
        config.link_timeout,
        shutdown.clone(),
    ));
    let writer = tokio::spawn(run_writer(
        db.clone(),
        BufferConfig::new(config.buffer_capacity, config.shutdown_flush),
        row_rx,
        buffer_stats,
        writer_cancel.clone(),
    ));
    let http = tokio::spawn({
        let cancel = shutdown.clone();
        let app = api::router(state);
        async move {
            tracing::info!(addr = %http_addr, "HTTP listening");
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
            {
                tracing::error!(error = %e, "HTTP server failed");
            }
        }
    });
    // Stop the writer only after the producers have finished. If the writer
    // stopped together with them, positions still in the channels at that
    // moment would be lost; this way it drains everything first.
    let writer_stop = tokio::spawn({
        let cancel = shutdown.clone();
        async move {
            cancel.cancelled().await;
            let _ = receiver.await;
            let _ = ingest.await;
            writer_cancel.cancel();
        }
    });

    Ok(Running {
        http_addr,
        shutdown,
        tasks: vec![
            ("http", http),
            ("migrations", migrations),
            ("writer-stop", writer_stop),
            ("writer", writer),
        ],
        db,
    })
}

/// Resolves on Ctrl-C, or SIGTERM on Unix (e.g. `docker stop`).
pub async fn shutdown_signal() {
    // If a signal handler cannot be installed, wait forever on that branch
    // instead of resolving, which would shut the service down immediately.
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

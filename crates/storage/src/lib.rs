//! PostGIS persistence for vehicles, positions, geofences and missions.
//!
//! Queries are built at runtime (no compile-time checked macros), so building
//! the workspace never needs a database. Schema lives in `migrations/` and is
//! embedded into the binary.

mod missions;
mod telemetry;

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

pub use missions::{MissionStatus, StoredMission};
pub use telemetry::{PositionRow, TrackPoint, VehicleRow};

/// Embedded migrations of this crate.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Postgres bind parameter limit is 65 535; rows per multi-row insert.
const INSERT_CHUNK_ROWS: usize = 1_000;

/// Storage errors.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("invalid data in database: {0}")]
    Corrupt(String),
}

/// Result alias for storage operations.
pub type Result<T, E = StorageError> = std::result::Result<T, E>;

/// Handle to the database (cheap to clone).
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connects lazily: the pool opens connections on first use, so the
    /// service can start (and buffer telemetry) while the database is down.
    ///
    /// # Errors
    /// Returns an error if the URL is invalid.
    pub fn connect_lazy(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(3))
            .connect_lazy(url)?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool (used by tests).
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying connection pool.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies pending migrations.
    ///
    /// # Errors
    /// Returns an error if the database is unreachable or a migration fails.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Checks connectivity.
    ///
    /// # Errors
    /// Returns an error if the database is unreachable.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Closes the pool, waiting for checked-out connections.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

//! Async MAVLink connection shared by the receive loop and outgoing protocols.
//!
//! Data flow:
//!
//! ```text
//!  UDP socket ──recv──▶ run() ──decode──▶ mpsc<TelemetryEvent> ──▶ server ingest
//!                          └──────raw────▶ broadcast<RawMessage> ──▶ upload_mission
//!  upload_mission ──send()──▶ UDP socket
//! ```
//!
//! Only one task may read from the socket, otherwise messages would be split
//! randomly between readers. So the mission upload does not read itself; it
//! subscribes to the raw broadcast that the single receive loop feeds.
//!
//! With `udpin:` the `mavlink` crate replies to the address of the last
//! received packet, so sending works on the same socket without knowing the
//! vehicle's address in advance.

use std::sync::Arc;

use chrono::Utc;
use mavlink::dialects::common::MavMessage;
use mavlink::{AsyncConnection, AsyncMavConnection, MavHeader};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::decode::{TelemetryEvent, decode};

/// System id the backend uses when it sends messages (a GCS-like id).
pub const GCS_SYSTEM_ID: u8 = 255;
/// Component id the backend uses when it sends messages (`MAV_COMP_ID_MISSIONPLANNER`).
pub const GCS_COMPONENT_ID: u8 = 190;

/// Capacity of the raw message broadcast used by request/response protocols.
///
/// At ~100 messages/s from PX4 this is a few seconds of backlog; a slower
/// subscriber gets `Lagged` and skips ahead, the receive loop never waits.
const RAW_CHANNEL_CAPACITY: usize = 256;

/// A raw received message with its header.
pub type RawMessage = (MavHeader, MavMessage);

/// Errors of the MAVLink link.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("cannot open MAVLink connection {addr}: {source}")]
    Connect {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to send MAVLink message: {0}")]
    Send(String),
}

/// Shared MAVLink connection.
///
/// Cloning is cheap; all clones use the same socket. Exactly one task should
/// call [`MavLink::run`]; other tasks receive through [`MavLink::subscribe`].
#[derive(Clone)]
pub struct MavLink {
    conn: Arc<AsyncConnection<MavMessage>>,
    raw_tx: broadcast::Sender<RawMessage>,
}

impl std::fmt::Debug for MavLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MavLink")
            .field("subscribers", &self.raw_tx.receiver_count())
            .finish_non_exhaustive()
    }
}

impl MavLink {
    /// Opens a connection, e.g. `udpin:0.0.0.0:14550`.
    ///
    /// # Errors
    /// Returns [`LinkError::Connect`] if the address is invalid or cannot be bound.
    pub async fn connect(addr: &str) -> Result<Self, LinkError> {
        let conn = mavlink::connect_async::<MavMessage>(addr)
            .await
            .map_err(|source| LinkError::Connect {
                addr: addr.to_owned(),
                source,
            })?;
        let (raw_tx, _) = broadcast::channel(RAW_CHANNEL_CAPACITY);
        Ok(Self {
            conn: Arc::new(conn),
            raw_tx,
        })
    }

    /// Subscribes to every raw message received by [`MavLink::run`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RawMessage> {
        self.raw_tx.subscribe()
    }

    /// Sends a message with the backend's GCS header.
    ///
    /// # Errors
    /// Returns [`LinkError::Send`] on I/O errors.
    pub async fn send(&self, msg: &MavMessage) -> Result<(), LinkError> {
        // We identify as a ground station. The sequence number is only used
        // by receivers to estimate packet loss, so a constant 0 is harmless.
        let header = MavHeader {
            system_id: u32::from(GCS_SYSTEM_ID),
            component_id: GCS_COMPONENT_ID,
            sequence: 0,
        };
        self.conn
            .send(&header, msg)
            .await
            .map(|_| ())
            .map_err(|e| LinkError::Send(e.to_string()))
    }

    /// Receive loop: decodes messages and forwards [`TelemetryEvent`]s to `tx`.
    ///
    /// Returns when `cancel` fires or the event receiver is dropped.
    pub async fn run(&self, tx: mpsc::Sender<TelemetryEvent>, cancel: CancellationToken) {
        tracing::info!("MAVLink receive loop started");
        loop {
            // `select!` makes the blocking-looking `recv` cancellable: on
            // shutdown we stop waiting for the next packet immediately.
            let received = tokio::select! {
                () = cancel.cancelled() => break,
                r = self.conn.recv() => r,
            };
            let (header, msg) = match received {
                Ok(m) => m,
                // Corrupt packets / CRC errors / unknown message ids are
                // normal on a radio link; skip them and keep listening.
                Err(e) => {
                    tracing::debug!(error = %e, "MAVLink read error");
                    continue;
                }
            };
            if let Some(event) = decode(&header, &msg, Utc::now()) {
                // `mpsc` is bounded: if ingest falls behind, this awaits
                // (backpressure) instead of growing memory. Still cancellable.
                let sent = tokio::select! {
                    () = cancel.cancelled() => break,
                    r = tx.send(event) => r,
                };
                if sent.is_err() {
                    tracing::warn!("telemetry receiver dropped, stopping receive loop");
                    break;
                }
            }
            // No subscribers is the normal case; ignore the error.
            let _ = self.raw_tx.send((header, msg));
        }
        tracing::info!("MAVLink receive loop stopped");
    }
}

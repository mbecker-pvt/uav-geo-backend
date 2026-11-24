//! `GET /live`: WebSocket stream of live positions and link changes.
//!
//! Every connection subscribes to the ingest task's `broadcast` channel and
//! forwards matching messages as JSON text frames. Filters (`system_id`,
//! `bbox`) are applied per connection on the server, so a map showing one
//! area does not receive the whole fleet.
//!
//! `broadcast` gives each subscriber its own read position in a shared ring
//! buffer. A client that reads too slowly falls behind, gets `Lagged(n)`,
//! and continues with the newest messages. Ingestion is never slowed down by
//! a slow browser.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use geo_core::{BBox, Position};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use super::{ApiError, ApiResult};
use crate::state::{AppState, LiveMessage};

#[derive(Debug, Deserialize)]
pub struct LiveQuery {
    /// Only this vehicle.
    system_id: Option<u8>,
    /// Only positions inside `minLon,minLat,maxLon,maxLat` (link messages pass).
    bbox: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Filter {
    system_id: Option<u8>,
    bbox: Option<BBox>,
}

impl Filter {
    fn accepts(&self, msg: &LiveMessage) -> bool {
        if self.system_id.is_some_and(|id| id != msg.system_id()) {
            return false;
        }
        match (msg, self.bbox) {
            (LiveMessage::Position(p), Some(b)) => {
                Position::new(p.lat, p.lon, p.alt_m).is_ok_and(|pos| b.contains(&pos))
            }
            _ => true,
        }
    }
}

pub async fn live(
    State(state): State<AppState>,
    Query(q): Query<LiveQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let bbox = q
        .bbox
        .as_deref()
        .map(str::parse::<BBox>)
        .transpose()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let filter = Filter {
        system_id: q.system_id,
        bbox,
    };
    // Validation happens before the upgrade, so a bad bbox is a normal HTTP
    // 400 instead of a WebSocket that closes immediately.
    Ok(ws.on_upgrade(move |socket| stream(socket, state, filter)))
}

async fn stream(socket: WebSocket, state: AppState, filter: Filter) {
    let mut rx = state.live.subscribe();
    // Split into send and receive halves so we can wait on both at once:
    // outgoing live messages and incoming close frames.
    let (mut sink, mut incoming) = socket.split();
    let shutdown = state.shutdown.clone();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
            msg = rx.recv() => match msg {
                Ok(msg) if filter.accepts(&msg) => {
                    let Ok(text) = serde_json::to_string(&msg) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                // Slow client: skip what it missed instead of disconnecting.
                Err(RecvError::Lagged(n)) => tracing::debug!(skipped = n, "websocket client lagged"),
                Err(RecvError::Closed) => break,
            },
            frame = incoming.next() => match frame {
                // Pings are answered by axum; other client frames are ignored.
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use telemetry::LinkState;

    use super::*;
    use crate::state::LivePosition;

    fn pos(system_id: u8, lat: f64, lon: f64) -> LiveMessage {
        LiveMessage::Position(LivePosition {
            system_id,
            time: Utc::now(),
            lat,
            lon,
            alt_m: 500.0,
            relative_alt_m: 0.0,
            agl_m: None,
            heading_deg: None,
            velocity_ned_mps: [0.0; 3],
        })
    }

    #[test]
    fn filters_by_vehicle_and_bbox() {
        let f = Filter {
            system_id: Some(1),
            bbox: Some("11,48,12,49".parse().unwrap()),
        };
        assert!(f.accepts(&pos(1, 48.5, 11.5)));
        assert!(!f.accepts(&pos(2, 48.5, 11.5)));
        assert!(!f.accepts(&pos(1, 50.0, 11.5)));
        let link = LiveMessage::Link {
            system_id: 1,
            state: LinkState::Lost,
        };
        assert!(f.accepts(&link), "link messages ignore the bbox");
    }
}

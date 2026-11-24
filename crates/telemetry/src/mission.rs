//! MAVLink mission upload protocol (GCS → vehicle).
//!
//! ```text
//! GCS                         Vehicle
//!  ── MISSION_COUNT(n) ──────▶
//!  ◀── MISSION_REQUEST_INT(0) ─
//!  ── MISSION_ITEM_INT(0) ────▶
//!        ... n times ...
//!  ◀── MISSION_ACK(ACCEPTED) ──
//! ```
//! Every step has a timeout; on timeout the last message is re-sent up to
//! `retries` times (the vehicle re-requests lost items itself).
//!
//! The vehicle drives the transfer: after `MISSION_COUNT` it asks for each
//! item by sequence number, and it may ask for the same item twice (if our
//! reply was lost) or out of order. So the loop below does not count items;
//! it answers whatever the vehicle requests until the vehicle sends
//! `MISSION_ACK`. This is why a lost UDP packet costs one timeout, not the
//! whole upload.
//!
//! Called from `POST /missions/{id}/upload` in a background task; the result
//! ends up in the mission's `status` (`uploaded` / `failed`).

use std::time::Duration;

use geo_core::Position;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use crate::common::{
    MISSION_COUNT_DATA, MISSION_ITEM_INT_DATA, MavCmd, MavFrame, MavMessage, MavMissionResult,
};
use crate::link::{LinkError, MavLink, RawMessage};

/// Component id of the primary autopilot (`MAV_COMP_ID_AUTOPILOT1`).
const AUTOPILOT_COMPONENT_ID: u8 = 1;

/// Upload timing parameters.
#[derive(Debug, Clone, Copy)]
pub struct UploadConfig {
    /// Time to wait for each vehicle response.
    pub step_timeout: Duration,
    /// How often the last message is re-sent before giving up.
    pub retries: u32,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            step_timeout: Duration::from_millis(1500),
            retries: 3,
        }
    }
}

/// Errors of a mission upload.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("mission must contain 1..=65535 waypoints")]
    InvalidCount,
    #[error("vehicle {0} did not respond")]
    Timeout(u8),
    #[error("vehicle rejected the mission: {0}")]
    Rejected(String),
    #[error("vehicle requested unknown item {0}")]
    BadRequest(u16),
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error("MAVLink receive loop is not running")]
    LinkClosed,
}

/// Builds the `MISSION_ITEM_INT` for waypoint `seq` (altitude AMSL).
#[must_use]
pub fn mission_item(target_system: u8, seq: u16, wp: &Position) -> MavMessage {
    // degE7 always fits in i32 for valid coordinates; z is f32 metres.
    #[allow(clippy::cast_possible_truncation)]
    MavMessage::MISSION_ITEM_INT(MISSION_ITEM_INT_DATA {
        target_system,
        target_component: AUTOPILOT_COMPONENT_ID,
        seq,
        // AMSL altitude; GLOBAL_INT is a deprecated synonym of GLOBAL.
        frame: MavFrame::MAV_FRAME_GLOBAL,
        // A plain "fly to this point" item. Takeoff/land/loiter items could
        // be added later; the validated route only needs waypoints.
        command: MavCmd::MAV_CMD_NAV_WAYPOINT,
        current: 0,
        // Continue to the next item automatically once this one is reached.
        autocontinue: 1,
        param1: 0.0, // hold time
        param2: 0.0, // acceptance radius (vehicle default)
        param3: 0.0,
        param4: f32::NAN, // yaw: keep vehicle behaviour
        x: (wp.lat * 1e7).round() as i32,
        y: (wp.lon * 1e7).round() as i32,
        z: wp.alt_m as f32,
    })
}

/// What the vehicle answered in one protocol step.
enum Reply {
    Request(u16),
    Ack(MavMissionResult),
}

/// Filters the raw stream down to mission-protocol replies from `target`.
/// Everything else on the link (positions, other vehicles) is ignored.
fn as_reply(target: u8, (header, msg): &RawMessage) -> Option<Reply> {
    if header.system_id != u32::from(target) {
        return None;
    }
    match msg {
        MavMessage::MISSION_REQUEST_INT(r) => Some(Reply::Request(r.seq)),
        // Legacy request from older firmware; answered with MISSION_ITEM_INT
        // as the spec requires.
        #[allow(deprecated)]
        MavMessage::MISSION_REQUEST(r) => Some(Reply::Request(r.seq)),
        MavMessage::MISSION_ACK(a) => Some(Reply::Ack(a.mavtype)),
        _ => None,
    }
}

/// Waits for the next relevant reply until `deadline`. `Ok(None)` means
/// timeout. The deadline is absolute, so the 50 Hz stream of unrelated
/// position messages cannot keep extending the wait.
async fn next_reply(
    rx: &mut broadcast::Receiver<RawMessage>,
    target: u8,
    deadline: Instant,
) -> Result<Option<Reply>, UploadError> {
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return Ok(None),
            Ok(Ok(raw)) => {
                if let Some(reply) = as_reply(target, &raw) {
                    return Ok(Some(reply));
                }
            }
            // We were too slow and the broadcast dropped old messages. The
            // dropped ones may include a reply; the timeout + re-send path
            // recovers from that, so just log and keep reading.
            Ok(Err(RecvError::Lagged(n))) => {
                tracing::warn!(skipped = n, "mission upload lagged behind telemetry");
            }
            Ok(Err(RecvError::Closed)) => return Err(UploadError::LinkClosed),
        }
    }
}

/// Uploads `waypoints` to vehicle `target_system`.
///
/// Requires [`MavLink::run`] to be running (it feeds the raw broadcast).
///
/// # Errors
/// Returns [`UploadError`] on timeout, rejection or link failure.
pub async fn upload_mission(
    link: &MavLink,
    target_system: u8,
    waypoints: &[Position],
    cfg: UploadConfig,
) -> Result<(), UploadError> {
    let count = u16::try_from(waypoints.len()).map_err(|_| UploadError::InvalidCount)?;
    if count == 0 {
        return Err(UploadError::InvalidCount);
    }
    // Subscribe before sending so no reply can be missed.
    let mut rx = link.subscribe();
    let count_msg = MavMessage::MISSION_COUNT(MISSION_COUNT_DATA {
        count,
        target_system,
        target_component: AUTOPILOT_COMPONENT_ID,
    });

    // `last_sent` is what we re-send on timeout: first the COUNT, later the
    // most recent ITEM.
    let mut last_sent = count_msg;
    link.send(&last_sent).await?;
    let mut retries_left = cfg.retries;
    loop {
        let deadline = Instant::now() + cfg.step_timeout;
        match next_reply(&mut rx, target_system, deadline).await? {
            None => {
                if retries_left == 0 {
                    return Err(UploadError::Timeout(target_system));
                }
                retries_left -= 1;
                tracing::debug!(target_system, "mission upload step timed out, re-sending");
                link.send(&last_sent).await?;
            }
            Some(Reply::Request(seq)) => {
                let wp = waypoints
                    .get(usize::from(seq))
                    .ok_or(UploadError::BadRequest(seq))?;
                last_sent = mission_item(target_system, seq, wp);
                link.send(&last_sent).await?;
                // Progress was made, so the retry budget starts over.
                retries_left = cfg.retries;
            }
            Some(Reply::Ack(MavMissionResult::MAV_MISSION_ACCEPTED)) => {
                tracing::info!(target_system, count, "mission accepted");
                return Ok(());
            }
            // Any other ACK result (NO_SPACE, INVALID_PARAM, DENIED, ...) ends
            // the transfer; the vehicle keeps its previous mission.
            Some(Reply::Ack(result)) => {
                return Err(UploadError::Rejected(format!("{result:?}")));
            }
        }
    }
}

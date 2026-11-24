//! Heartbeat-based link loss detection (pure, clock passed in).
//!
//! MAVLink has no "disconnect" message: UDP is connectionless, and a drone
//! that flies out of radio range simply goes quiet. The convention is that
//! every system sends a `HEARTBEAT` at 1 Hz, and a peer counts as lost after
//! a few missed heartbeats (`LINK_TIMEOUT_S`, default 3 s).
//!
//! The server's ingest task calls [`LinkMonitor::heartbeat`] for every
//! heartbeat and [`LinkMonitor::check`] on a 500 ms timer. Both return only
//! *transitions*, which become `{"type": "link", ...}` WebSocket messages, so
//! clients are told once ("lost") instead of on every tick.
//!
//! Taking `now` as a parameter instead of calling `Instant::now()` inside
//! keeps the logic deterministic: tests step the clock by hand.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Link state of one vehicle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Connected,
    Lost,
}

/// A state transition reported by [`LinkMonitor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LinkChange {
    pub system_id: u8,
    pub state: LinkState,
}

/// Tracks the last heartbeat per vehicle and reports transitions.
#[derive(Debug, Clone)]
pub struct LinkMonitor {
    timeout: Duration,
    vehicles: BTreeMap<u8, (Instant, LinkState)>,
}

impl LinkMonitor {
    /// A vehicle is `Lost` after `timeout` without heartbeat.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            vehicles: BTreeMap::new(),
        }
    }

    /// Records a heartbeat; returns a change if the vehicle is new or recovered.
    pub fn heartbeat(&mut self, system_id: u8, now: Instant) -> Option<LinkChange> {
        let previous = self
            .vehicles
            .insert(system_id, (now, LinkState::Connected))
            .map(|(_, s)| s);
        // `None` (first heartbeat ever) or `Some(Lost)` (link recovered) are
        // transitions; `Some(Connected)` is just the normal 1 Hz tick.
        (previous != Some(LinkState::Connected)).then_some(LinkChange {
            system_id,
            state: LinkState::Connected,
        })
    }

    /// Marks vehicles without a recent heartbeat as lost; returns the changes.
    pub fn check(&mut self, now: Instant) -> Vec<LinkChange> {
        let mut changes = Vec::new();
        for (&system_id, (last, state)) in &mut self.vehicles {
            // Only Connected → Lost is a transition; an already lost vehicle
            // stays lost silently until its next heartbeat.
            if *state == LinkState::Connected && now.duration_since(*last) > self.timeout {
                *state = LinkState::Lost;
                changes.push(LinkChange {
                    system_id,
                    state: LinkState::Lost,
                });
            }
        }
        changes
    }

    /// Current state of a vehicle, `None` if never seen.
    #[must_use]
    pub fn state(&self, system_id: u8) -> Option<LinkState> {
        self.vehicles.get(&system_id).map(|&(_, s)| s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_loss_and_recovery_once() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut m = LinkMonitor::new(s(3));
        assert_eq!(
            m.heartbeat(1, t0),
            Some(LinkChange {
                system_id: 1,
                state: LinkState::Connected
            })
        );
        assert_eq!(m.heartbeat(1, t0 + s(1)), None);
        assert_eq!(m.check(t0 + s(3)), vec![]);
        assert_eq!(
            m.check(t0 + s(5)),
            vec![LinkChange {
                system_id: 1,
                state: LinkState::Lost
            }]
        );
        assert_eq!(m.check(t0 + s(6)), vec![], "reported only once");
        assert_eq!(m.state(1), Some(LinkState::Lost));
        assert_eq!(
            m.heartbeat(1, t0 + s(7)),
            Some(LinkChange {
                system_id: 1,
                state: LinkState::Connected
            })
        );
        assert_eq!(m.state(2), None);
    }
}

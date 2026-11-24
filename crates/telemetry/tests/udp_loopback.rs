//! End-to-end test of the receive loop over a loopback UDP socket.

use std::time::Duration;

use mavlink::{AsyncMavConnection, MavHeader};
use telemetry::common::{
    GLOBAL_POSITION_INT_DATA, HEARTBEAT_DATA, MavAutopilot, MavMessage, MavType,
};
use telemetry::{MavLink, TelemetryEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn receives_heartbeat_and_position_over_udp() {
    let port = free_udp_port();
    let link = MavLink::connect(&format!("udpin:127.0.0.1:{port}"))
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let link = link.clone();
        let cancel = cancel.clone();
        async move { link.run(tx, cancel).await }
    });

    let vehicle = mavlink::connect_async::<MavMessage>(&format!("udpout:127.0.0.1:{port}"))
        .await
        .unwrap();
    let header = MavHeader {
        system_id: 1,
        component_id: 1,
        sequence: 0,
    };
    let hb = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
        ..HEARTBEAT_DATA::default()
    });
    let pos = MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
        lat: 481_086_000,
        lon: 112_939_000,
        alt: 560_250,
        ..GLOBAL_POSITION_INT_DATA::default()
    });
    vehicle.send(&header, &hb).await.unwrap();
    vehicle.send(&header, &pos).await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out")
        .unwrap();
    assert!(matches!(first, TelemetryEvent::Heartbeat(ref h) if h.system_id == 1));
    let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out")
        .unwrap();
    let TelemetryEvent::Position(p) = second else {
        panic!("expected position, got {second:?}");
    };
    assert!((p.position.lat - 48.1086).abs() < 1e-9);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("receive loop did not stop")
        .unwrap();
}

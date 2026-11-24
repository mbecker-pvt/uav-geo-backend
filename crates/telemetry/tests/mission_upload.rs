//! Mission upload against a fake vehicle over loopback UDP, including a lost item.

use std::time::Duration;

use geo_core::Position;
use mavlink::{AsyncMavConnection, MavHeader};
use telemetry::common::{
    HEARTBEAT_DATA, MISSION_ACK_DATA, MISSION_REQUEST_INT_DATA, MavAutopilot, MavMessage,
    MavMissionResult, MavType,
};
use telemetry::{MavLink, UploadConfig, UploadError, upload_mission};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const VEHICLE: MavHeader = MavHeader {
    system_id: 1,
    component_id: 1,
    sequence: 0,
};

async fn setup() -> (
    MavLink,
    mavlink::AsyncConnection<MavMessage>,
    CancellationToken,
) {
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let link = MavLink::connect(&format!("udpin:127.0.0.1:{port}"))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn({
        let (link, cancel) = (link.clone(), cancel.clone());
        async move { link.run(tx, cancel).await }
    });
    // Drain telemetry events so the receive loop never blocks.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let vehicle = mavlink::connect_async::<MavMessage>(&format!("udpout:127.0.0.1:{port}"))
        .await
        .unwrap();
    // The first packet teaches the udpin side where to reply.
    let hb = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
        ..HEARTBEAT_DATA::default()
    });
    vehicle.send(&VEHICLE, &hb).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    (link, vehicle, cancel)
}

fn waypoints() -> Vec<Position> {
    vec![
        Position::new(48.1, 11.5, 600.0).unwrap(),
        Position::new(48.11, 11.51, 610.0).unwrap(),
        Position::new(48.12, 11.52, 620.0).unwrap(),
    ]
}

fn request(seq: u16) -> MavMessage {
    MavMessage::MISSION_REQUEST_INT(MISSION_REQUEST_INT_DATA {
        seq,
        target_system: 255,
        target_component: 190,
    })
}

#[tokio::test]
async fn uploads_mission_and_recovers_lost_item() {
    let (link, vehicle, cancel) = setup().await;
    let cfg = UploadConfig {
        step_timeout: Duration::from_millis(300),
        retries: 3,
    };
    let upload = tokio::spawn({
        let link = link.clone();
        async move { upload_mission(&link, 1, &waypoints(), cfg).await }
    });

    let fake_vehicle = async {
        let mut received = Vec::new();
        let mut dropped_once = false;
        loop {
            let (_, msg) = vehicle.recv().await.unwrap();
            match msg {
                MavMessage::MISSION_COUNT(c) => {
                    assert_eq!(c.count, 3);
                    vehicle.send(&VEHICLE, &request(0)).await.unwrap();
                }
                MavMessage::MISSION_ITEM_INT(item) => {
                    if item.seq == 1 && !dropped_once {
                        dropped_once = true; // simulate a lost packet
                        continue;
                    }
                    if received.last() == Some(&item.seq) {
                        continue;
                    }
                    received.push(item.seq);
                    if item.seq == 2 {
                        let ack = MavMessage::MISSION_ACK(MISSION_ACK_DATA {
                            target_system: 255,
                            target_component: 190,
                            mavtype: MavMissionResult::MAV_MISSION_ACCEPTED,
                        });
                        vehicle.send(&VEHICLE, &ack).await.unwrap();
                        return (received, item);
                    }
                    vehicle
                        .send(&VEHICLE, &request(item.seq + 1))
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
    };
    let (received, last) = tokio::time::timeout(Duration::from_secs(10), fake_vehicle)
        .await
        .expect("fake vehicle timed out");
    assert_eq!(received, vec![0, 1, 2]);
    assert_eq!((last.x, last.y), (481_200_000, 115_200_000));
    assert!((last.z - 620.0).abs() < 1e-3);
    upload.await.unwrap().unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn reports_timeout_and_rejection() {
    let (link, vehicle, cancel) = setup().await;
    let cfg = UploadConfig {
        step_timeout: Duration::from_millis(100),
        retries: 1,
    };
    // Vehicle never answers.
    let err = upload_mission(&link, 1, &waypoints(), cfg)
        .await
        .unwrap_err();
    assert!(matches!(err, UploadError::Timeout(1)), "{err}");

    // Vehicle rejects immediately.
    let upload = tokio::spawn({
        let link = link.clone();
        async move { upload_mission(&link, 1, &waypoints(), cfg).await }
    });
    loop {
        if let (_, MavMessage::MISSION_COUNT(_)) = vehicle.recv().await.unwrap() {
            break;
        }
    }
    let nack = MavMessage::MISSION_ACK(MISSION_ACK_DATA {
        target_system: 255,
        target_component: 190,
        mavtype: MavMissionResult::MAV_MISSION_NO_SPACE,
    });
    vehicle.send(&VEHICLE, &nack).await.unwrap();
    let err = upload.await.unwrap().unwrap_err();
    assert!(
        matches!(err, UploadError::Rejected(ref r) if r.contains("NO_SPACE")),
        "{err}"
    );
    cancel.cancel();
}

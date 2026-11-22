//! End-to-end check of the Connection Engine on localhost: SDP exchange,
//! DTLS/ICE connectivity, H.264 over RTP and the control DataChannel.

use std::time::Duration;
use vv_codec::{EncoderSettings, H264Decoder, H264Encoder};
use vv_protocol::control::ControlMessage;
use vv_transport::{ConnectionState, Peer, PeerEvent, Role, TransportConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn viewer_receives_video_and_agent_receives_input() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // Host candidates only: no STUN needed on loopback.
    let config = TransportConfig { ice_servers: vec![], relay_only: false };

    let (mut viewer, mut viewer_events) = Peer::new(Role::Viewer, &config).await.unwrap();
    let (mut agent, mut agent_events) = Peer::new(Role::Agent, &config).await.unwrap();

    let offer = viewer.create_offer().await.unwrap();
    let answer = agent.answer_offer(&offer).await.unwrap();
    viewer.accept_answer(&answer).await.unwrap();

    // Wait for the agent to be connected and its control channel open.
    let wait_agent = async {
        let (mut connected, mut open) = (false, false);
        while !(connected && open) {
            match agent_events.recv().await.expect("agent events closed") {
                PeerEvent::State(ConnectionState::Connected) => connected = true,
                PeerEvent::ControlOpen => open = true,
                PeerEvent::State(s @ (ConnectionState::Failed | ConnectionState::Closed)) => {
                    panic!("agent connection {s}")
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), wait_agent).await.expect("agent never connected");

    // Agent streams a few real H.264 frames.
    let (w, h) = (640u32, 360u32);
    let mut enc = H264Encoder::new(w, h, EncoderSettings::default()).unwrap();
    let streamer = async {
        for i in 0..60u32 {
            let rgba: Vec<u8> = (0..w * h)
                .flat_map(|p| [((p + i * 4) % 256) as u8, 64, 160, 255])
                .collect();
            if let Some(frame) = enc.encode_rgba(&rgba, w, h).unwrap() {
                agent.send_video_frame(frame.data, Duration::from_millis(33)).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    };

    // Viewer decodes video and sends one input event.
    let receiver = async {
        let mut dec = H264Decoder::new().unwrap();
        let mut decoded = 0;
        let mut sent_input = false;
        while decoded < 10 {
            match viewer_events.recv().await.expect("viewer events closed") {
                PeerEvent::VideoFrame(au) => {
                    if let Some(f) = dec.decode(&au).unwrap() {
                        assert_eq!((f.width, f.height), (w, h));
                        decoded += 1;
                    }
                }
                PeerEvent::ControlOpen if !sent_input => {
                    viewer
                        .send_control(&ControlMessage::Key { code: "KeyA".into(), down: true })
                        .await
                        .unwrap();
                    sent_input = true;
                }
                _ => {}
            }
        }
        if !sent_input {
            viewer
                .send_control(&ControlMessage::Key { code: "KeyA".into(), down: true })
                .await
                .unwrap();
        }
        decoded
    };

    let (_, decoded) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(streamer, receiver)
    })
    .await
    .expect("video did not arrive in time");
    assert!(decoded >= 10);

    let got_key = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(PeerEvent::Control(ControlMessage::Key { code, down })) =
                agent_events.recv().await
            {
                return (code, down);
            }
        }
    })
    .await
    .expect("agent never got the input event");
    assert_eq!(got_key, ("KeyA".to_string(), true));

    viewer.close().await;
    agent.close().await;
}

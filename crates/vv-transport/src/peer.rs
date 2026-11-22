//! WebRTC peer connection wrapper.
//!
//! Roles:
//! * [`Role::Viewer`] — offerer. Receives video (recvonly transceiver) and
//!   creates the `control` DataChannel.
//! * [`Role::Agent`] — answerer. Sends the H.264 screen track and accepts the
//!   viewer's DataChannel.
//!
//! Phase 0 uses non-trickle ICE: each side waits for candidate gathering to
//! finish and ships one complete SDP through signaling. Trickle ICE (faster
//! setup) is a Phase 1 item.

use crate::video::{Assembled, FrameAssembler};
use crate::{TransportConfig, video};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use rtc::media::Sample;
use rtc::rtp_transceiver::PayloadType;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, watch};
use vv_protocol::CONTROL_CHANNEL_LABEL;
use vv_protocol::control::ControlMessage;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer, RTCIceTransportPolicy,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry,
    register_default_interceptors,
};
use webrtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit, RtpSender};
use webrtc::runtime::TokioRuntime;

/// H.264 Constrained Baseline, packetization-mode 1 — universally supported.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const H264_PAYLOAD_TYPE: u8 = 102;
const GATHER_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Agent,
}

#[derive(Debug)]
pub enum PeerEvent {
    State(RTCPeerConnectionState),
    ControlOpen,
    Control(ControlMessage),
    ControlClosed,
    /// Complete H.264 Annex-B access unit (viewer only).
    VideoFrame(Bytes),
    /// Video packets were lost; a keyframe should be requested (viewer only).
    VideoLoss,
}

type ControlSlot = Arc<Mutex<Option<Arc<dyn DataChannel>>>>;

#[derive(Clone)]
struct Handler {
    events: mpsc::UnboundedSender<PeerEvent>,
    gathered: watch::Sender<bool>,
    control: ControlSlot,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        tracing::debug!("local ICE candidate: {}", event.candidate);
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::info!("peer connection state: {state}");
        let _ = self.events.send(PeerEvent::State(state));
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let label = dc.label().await.unwrap_or_default();
        if label != CONTROL_CHANNEL_LABEL {
            tracing::warn!(label, "ignoring unexpected data channel");
            return;
        }
        *self.control.lock().await = Some(dc.clone());
        spawn_control_reader(dc, self.events.clone());
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if track.kind().await != RtpCodecKind::Video {
            return;
        }
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut assembler = FrameAssembler::new();
            while let Some(evt) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = evt {
                    let out = match assembler.push(&packet) {
                        Some(Assembled::Frame(f)) => PeerEvent::VideoFrame(f),
                        Some(Assembled::Loss) => PeerEvent::VideoLoss,
                        None => continue,
                    };
                    if events.send(out).is_err() {
                        break;
                    }
                }
            }
            tracing::info!("remote video track ended");
        });
    }
}

fn spawn_control_reader(dc: Arc<dyn DataChannel>, events: mpsc::UnboundedSender<PeerEvent>) {
    tokio::spawn(async move {
        while let Some(evt) = dc.poll().await {
            let out = match evt {
                DataChannelEvent::OnOpen => PeerEvent::ControlOpen,
                DataChannelEvent::OnClose => {
                    let _ = events.send(PeerEvent::ControlClosed);
                    break;
                }
                DataChannelEvent::OnMessage(msg) => {
                    let Ok(text) = std::str::from_utf8(&msg.data) else {
                        tracing::warn!("non-UTF8 control message dropped");
                        continue;
                    };
                    match vv_protocol::decode::<ControlMessage>(text) {
                        Ok(m) => PeerEvent::Control(m),
                        Err(e) => {
                            tracing::warn!("invalid control message: {e}");
                            continue;
                        }
                    }
                }
                _ => continue,
            };
            if events.send(out).is_err() {
                break;
            }
        }
    });
}

/// Outgoing H.264 track (agent only).
struct VideoOut {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    payload_type: Option<PayloadType>,
}

pub struct Peer {
    role: Role,
    pc: Arc<dyn PeerConnection>,
    gathered: watch::Receiver<bool>,
    control: ControlSlot,
    video: Option<Mutex<VideoOut>>,
}

fn h264_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: H264_FMTP.to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: H264_PAYLOAD_TYPE,
    }
}

fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    h.finish() as u32
}

impl Peer {
    /// Creates the peer connection. Events are delivered on the returned receiver.
    pub async fn new(
        role: Role,
        config: &TransportConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<PeerEvent>)> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_codec(h264_codec(), RtpCodecKind::Video)?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

        let ice_servers = config
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect();
        let rtc_config = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .with_ice_transport_policy(if config.relay_only {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            })
            .build();

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (gathered_tx, gathered_rx) = watch::channel(false);
        let control: ControlSlot = Arc::new(Mutex::new(None));
        let handler = Arc::new(Handler {
            events: events_tx.clone(),
            gathered: gathered_tx,
            control: control.clone(),
        });

        let pc = PeerConnectionBuilder::new()
            .with_configuration(rtc_config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_runtime(Arc::new(TokioRuntime))
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .context("creating peer connection")?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        let mut video = None;
        match role {
            Role::Viewer => {
                pc.add_transceiver_from_kind(
                    RtpCodecKind::Video,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        ..Default::default()
                    }),
                )
                .await?;
                let dc = pc.create_data_channel(CONTROL_CHANNEL_LABEL, None).await?;
                *control.lock().await = Some(dc.clone());
                spawn_control_reader(dc, events_tx);
            }
            Role::Agent => {
                let ssrc = random_u32();
                let codec = h264_codec();
                let track = Arc::new(TrackLocalStaticSample::new(
                    Instant::now(),
                    MediaStreamTrack::new(
                        "versaviewer-screen".to_owned(),
                        "versaviewer-screen-video".to_owned(),
                        "screen".to_owned(),
                        RtpCodecKind::Video,
                        vec![RTCRtpEncodingParameters {
                            rtp_coding_parameters: RTCRtpCodingParameters {
                                ssrc: Some(ssrc),
                                ..Default::default()
                            },
                            codec: codec.rtp_codec,
                            ..Default::default()
                        }],
                    ),
                )?);
                let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
                video = Some(Mutex::new(VideoOut { track, sender, ssrc, payload_type: None }));
            }
        }

        Ok((Self { role, pc, gathered: gathered_rx, control, video }, events_rx))
    }

    async fn wait_gathering(&mut self) {
        let wait = self.gathered.wait_for(|done| *done);
        if tokio::time::timeout(GATHER_TIMEOUT, wait).await.is_err() {
            tracing::warn!("ICE gathering timed out; sending candidates gathered so far");
        }
    }

    async fn local_sdp(&mut self) -> Result<String> {
        self.wait_gathering().await;
        let desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description"))?;
        log_candidates("local", &desc.sdp);
        Ok(serde_json::to_string(&desc)?)
    }

    /// Viewer: creates the SDP offer (JSON-encoded `RTCSessionDescription`).
    pub async fn create_offer(&mut self) -> Result<String> {
        anyhow::ensure!(self.role == Role::Viewer, "only the viewer creates offers");
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer).await?;
        self.local_sdp().await
    }

    /// Viewer: applies the agent's answer.
    pub async fn accept_answer(&mut self, sdp_json: &str) -> Result<()> {
        let answer: RTCSessionDescription =
            serde_json::from_str(sdp_json).context("parsing SDP answer")?;
        log_candidates("remote", &answer.sdp);
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Agent: applies the viewer's offer and returns the answer.
    pub async fn answer_offer(&mut self, sdp_json: &str) -> Result<String> {
        anyhow::ensure!(self.role == Role::Agent, "only the agent answers offers");
        let offer: RTCSessionDescription =
            serde_json::from_str(sdp_json).context("parsing SDP offer")?;
        log_candidates("remote", &offer.sdp);
        self.pc.set_remote_description(offer).await?;
        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer).await?;
        self.local_sdp().await
    }

    /// Sends a control message on the DataChannel (dropped if not open yet).
    pub async fn send_control(&self, msg: &ControlMessage) -> Result<()> {
        let dc = self.control.lock().await.clone();
        let Some(dc) = dc else {
            return Err(anyhow!("control channel not open"));
        };
        dc.send_text(&vv_protocol::encode(msg)?).await?;
        Ok(())
    }

    /// Agent: sends one encoded H.264 access unit (Annex-B).
    pub async fn send_video_frame(&self, annexb: Vec<u8>, duration: Duration) -> Result<()> {
        let video = self.video.as_ref().ok_or_else(|| anyhow!("peer has no video track"))?;
        let mut v = video.lock().await;
        let pt = match v.payload_type {
            Some(pt) => pt,
            None => {
                let params = v.sender.get_parameters().await?;
                let pt = params
                    .rtp_parameters
                    .codecs
                    .first()
                    .map(|c| c.payload_type)
                    .ok_or_else(|| anyhow!("video sender has no negotiated codec"))?;
                v.payload_type = Some(pt);
                pt
            }
        };
        v.track
            .sample_writer(v.ssrc, pt)
            .write_sample(&Sample {
                data: Bytes::from(annexb),
                duration,
                ..Sample::new(Instant::now())
            })
            .await?;
        Ok(())
    }

    pub async fn close(&self) {
        if let Some(dc) = self.control.lock().await.take() {
            let _ = dc.close().await;
        }
        if let Err(e) = self.pc.close().await {
            tracing::debug!("closing peer connection: {e}");
        }
    }
}

/// Logs which candidate types are on offer — handy to see whether a session
/// can go P2P (host/srflx) or will need TURN (relay).
fn log_candidates(side: &str, sdp: &str) {
    let mut kinds: Vec<&str> = sdp
        .lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .filter_map(|l| l.split_whitespace().skip_while(|t| *t != "typ").nth(1))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    tracing::info!("{side} ICE candidate types: {}", kinds.join(", "));
}

// Keep the helper reachable for engine-level tests.
#[doc(hidden)]
pub fn contains_idr(annexb: &[u8]) -> bool {
    video::contains_idr(annexb)
}


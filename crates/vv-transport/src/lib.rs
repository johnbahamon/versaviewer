//! Connection Engine (Phase 0).
//!
//! * [`signaling`] — WebSocket client for the signaling server.
//! * [`peer`] — WebRTC peer connection with the VersaViewer media layout:
//!   one H.264 video track (agent -> viewer) and one reliable, ordered
//!   `control` DataChannel (both directions).
//!
//! Media and input never traverse the signaling server: they flow P2P over
//! ICE (host/srflx candidates) or, when that fails, through a TURN relay.
//! Everything on the peer connection is encrypted by DTLS-SRTP / DTLS-SCTP.

pub mod peer;
pub mod signaling;
mod video;

pub use peer::{Peer, PeerEvent, Role};
pub use signaling::{SignalingClient, SignalingSender};
pub use webrtc::peer_connection::RTCPeerConnectionState as ConnectionState;

/// STUN/TURN server entry.
#[derive(Debug, Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub ice_servers: Vec<IceServer>,
    /// Only use TURN relay candidates (privacy mode / TURN fallback testing).
    pub relay_only: bool,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                ..Default::default()
            }],
            relay_only: false,
        }
    }
}

impl TransportConfig {
    /// Builds a config from CLI-style options.
    ///
    /// `stun`: e.g. `stun:host:3478`. `turn`: e.g. `turn:host:3478?transport=udp`.
    pub fn from_options(
        stun: Option<String>,
        turn: Option<String>,
        turn_user: Option<String>,
        turn_pass: Option<String>,
        relay_only: bool,
    ) -> Self {
        let mut ice_servers = Vec::new();
        if let Some(stun) = stun {
            ice_servers.push(IceServer { urls: vec![stun], ..Default::default() });
        } else {
            ice_servers = Self::default().ice_servers;
        }
        if let Some(turn) = turn {
            ice_servers.push(IceServer {
                urls: vec![turn],
                username: turn_user.unwrap_or_default(),
                credential: turn_pass.unwrap_or_default(),
            });
        }
        Self { ice_servers, relay_only }
    }
}

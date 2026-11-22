//! Signaling messages (agent/viewer <-> signaling server).
//!
//! Flow (Phase 0, non-trickle ICE):
//!
//! ```text
//! agent  -> server : Register { device_id }
//! viewer -> server : Connect { device_id, password }
//! server -> agent  : ConnectRequest { session_id, password }
//! agent            : validates password + asks local user for consent
//! agent  -> server : Accept { session_id } | Reject { session_id, reason }
//! server -> viewer : Accepted { session_id } | Rejected { .. }
//! viewer -> server : Sdp { session_id, sdp }          (offer, relayed to agent)
//! agent  -> server : Sdp { session_id, sdp }          (answer, relayed to viewer)
//! any    -> server : Hangup { session_id }
//! ```
//!
//! The one-time password is generated and checked **by the agent**; the
//! server relays it but never stores it. This is a Phase 0 simplification:
//! Phase 2 replaces it with authenticated users + temporary credentials
//! issued by the session service.

use serde::{Deserialize, Serialize};

/// Messages sent by agents and viewers to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Agent announces itself as reachable under `device_id`.
    Register { device_id: String, protocol: u32 },
    /// Viewer asks to open a session with `device_id`.
    Connect { device_id: String, password: String, protocol: u32 },
    /// Agent accepts a pending session.
    Accept { session_id: String },
    /// Agent rejects a pending session.
    Reject { session_id: String, reason: String },
    /// SDP offer/answer, relayed to the other side of the session.
    Sdp { session_id: String, sdp: String },
    /// Either side ends the session.
    Hangup { session_id: String },
}

/// Messages sent by the server to agents and viewers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Agent registration confirmed.
    Registered { device_id: String },
    /// A viewer wants to connect to this agent.
    ConnectRequest { session_id: String, password: String },
    /// Viewer: the agent accepted, proceed with the SDP offer.
    Accepted { session_id: String },
    /// Viewer: the agent (or server) rejected the session.
    Rejected { session_id: Option<String>, reason: String },
    /// SDP relayed from the other peer.
    Sdp { session_id: String, sdp: String },
    /// The other side ended the session (or disconnected).
    Hangup { session_id: String },
    /// Protocol / server-side error.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_uses_snake_case_tag() {
        let msg = ClientMessage::Connect {
            device_id: "123456789".into(),
            password: "abc123".into(),
            protocol: 1,
        };
        let json = crate::encode(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"connect","device_id":"123456789","password":"abc123","protocol":1}"#
        );
        assert_eq!(crate::decode::<ClientMessage>(&json).unwrap(), msg);
    }

    #[test]
    fn server_message_roundtrip() {
        let msg = ServerMessage::Rejected { session_id: None, reason: "offline".into() };
        let json = crate::encode(&msg).unwrap();
        assert_eq!(crate::decode::<ServerMessage>(&json).unwrap(), msg);
    }
}

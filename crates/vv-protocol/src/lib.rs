//! VersaViewer wire protocol.
//!
//! Two independent message families live here:
//!
//! * [`signal`] — JSON messages exchanged with the signaling server over WebSocket.
//!   The server only relays them; it never sees media or input.
//! * [`control`] — JSON messages exchanged peer-to-peer over the WebRTC
//!   `control` DataChannel (input events, session control).
//!
//! Every message carries no implicit state: a receiver must be able to
//! validate each one on its own. The protocol version is negotiated in
//! [`signal::ClientMessage::Register`] / [`signal::ClientMessage::Connect`].

pub mod control;
pub mod signal;

/// Protocol version spoken by this build. Bumped on any breaking change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Label of the reliable, ordered DataChannel used for control/input messages.
pub const CONTROL_CHANNEL_LABEL: &str = "control";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid JSON message: {0}")]
    Json(#[from] serde_json::Error),
}

/// Serializes any protocol message to a JSON string.
pub fn encode<T: serde::Serialize>(msg: &T) -> Result<String, ProtocolError> {
    Ok(serde_json::to_string(msg)?)
}

/// Parses any protocol message from a JSON string.
pub fn decode<'a, T: serde::Deserialize<'a>>(text: &'a str) -> Result<T, ProtocolError> {
    Ok(serde_json::from_str(text)?)
}

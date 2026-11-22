//! Peer-to-peer control messages carried on the `control` DataChannel.
//!
//! Coordinates are **normalized** to `[0.0, 1.0]` relative to the shared
//! screen, so the viewer never needs to know the remote resolution or DPI
//! scaling to send pointer input.
//!
//! Keys are identified by their **physical position** using W3C
//! `KeyboardEvent.code` names (`"KeyA"`, `"ShiftLeft"`, `"ArrowUp"`...).
//! This makes the remote keyboard layout authoritative (like sitting at the
//! remote machine), which is what remote-support tools expect.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Absolute pointer move, normalized coordinates.
    MouseMove { x: f32, y: f32 },
    MouseButton { button: MouseButton, down: bool },
    /// Wheel scroll in "lines"; positive `dy` scrolls down, positive `dx` right.
    MouseWheel { dx: f32, dy: f32 },
    /// Physical key press/release by W3C `KeyboardEvent.code`.
    Key { code: String, down: bool },
    /// Viewer asks the agent for a fresh keyframe (e.g. after decode errors).
    RequestKeyframe,
    /// Remote screen description, sent by the agent when the channel opens.
    ScreenInfo { width: u32, height: u32 },
    /// Either side ends the session gracefully.
    Bye,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_message_format() {
        let msg = ControlMessage::Key { code: "KeyA".into(), down: true };
        assert_eq!(crate::encode(&msg).unwrap(), r#"{"type":"key","code":"KeyA","down":true}"#);
    }

    #[test]
    fn mouse_roundtrip() {
        let msg = ControlMessage::MouseButton { button: MouseButton::Right, down: false };
        let json = crate::encode(&msg).unwrap();
        assert_eq!(crate::decode::<ControlMessage>(&json).unwrap(), msg);
    }
}

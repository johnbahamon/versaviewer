//! Input injection abstraction.
//!
//! The agent receives [`ControlMessage`]s from the viewer and forwards the
//! input ones to an [`InputInjector`]. Backends:
//!
//! * Windows: direct `SendInput` with scan codes (precise extended-key
//!   handling, absolute coordinates on the primary display).
//! * Other OSes: `enigo` (CGEvent on macOS, XTest/libei on Linux). Used in
//!   Phase 0 so the agent can be smoke-tested on a developer Mac.
//!
//! Security: injection only happens while a session the local user
//! explicitly accepted is active; the engine enforces that, not this crate.

pub mod keymap;

#[cfg(windows)]
mod windows;
#[cfg(not(windows))]
mod enigo_backend;

use anyhow::Result;
use vv_protocol::control::{ControlMessage, MouseButton};

/// Logical bounds of the display being controlled (OS coordinate space).
#[derive(Debug, Clone, Copy)]
pub struct DisplayBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Not `Send`: some backends (CGEvent sources, X11 connections) are thread-affine.
/// Create the injector on the thread that will use it.
pub trait InputInjector {
    /// Absolute pointer move, `x`/`y` normalized to `[0, 1]`.
    fn mouse_move(&mut self, x: f32, y: f32) -> Result<()>;
    fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<()>;
    /// Scroll in lines; positive `dy` = down, positive `dx` = right.
    fn mouse_wheel(&mut self, dx: f32, dy: f32) -> Result<()>;
    /// Physical key by W3C `code`.
    fn key(&mut self, code: &str, down: bool) -> Result<()>;
    /// Releases every key/button this injector pressed (called on session end
    /// so a dropped connection never leaves e.g. Ctrl held down remotely).
    fn release_all(&mut self) -> Result<()>;
}

/// Creates the native injector for this platform.
pub fn native_injector(bounds: DisplayBounds) -> Result<Box<dyn InputInjector>> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::SendInputInjector::new(bounds)))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(enigo_backend::EnigoInjector::new(bounds)?))
    }
}

/// Applies an input message. Non-input messages are ignored (returns `false`).
pub fn apply(injector: &mut dyn InputInjector, msg: &ControlMessage) -> Result<bool> {
    match msg {
        ControlMessage::MouseMove { x, y } => injector.mouse_move(clamp01(*x), clamp01(*y))?,
        ControlMessage::MouseButton { button, down } => injector.mouse_button(*button, *down)?,
        ControlMessage::MouseWheel { dx, dy } => injector.mouse_wheel(*dx, *dy)?,
        ControlMessage::Key { code, down } => injector.key(code, *down)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// Tracks pressed keys/buttons so they can be released on session end.
#[derive(Default)]
pub(crate) struct PressedState {
    pub keys: std::collections::BTreeSet<String>,
    pub buttons: Vec<MouseButton>,
}

impl PressedState {
    pub fn key(&mut self, code: &str, down: bool) {
        if down {
            self.keys.insert(code.to_owned());
        } else {
            self.keys.remove(code);
        }
    }

    pub fn button(&mut self, button: MouseButton, down: bool) {
        self.buttons.retain(|b| *b != button);
        if down {
            self.buttons.push(button);
        }
    }

    pub fn take(&mut self) -> (Vec<String>, Vec<MouseButton>) {
        (std::mem::take(&mut self.keys).into_iter().collect(), std::mem::take(&mut self.buttons))
    }
}

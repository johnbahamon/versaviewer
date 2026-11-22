//! Non-Windows backend on top of `enigo` (CGEvent on macOS, X11/libei on Linux).
//!
//! On macOS the process needs the Accessibility permission
//! (System Settings > Privacy & Security > Accessibility) to inject input.

use crate::{DisplayBounds, InputInjector, PressedState, keymap};
use anyhow::{Context, Result, anyhow};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use vv_protocol::control::MouseButton;

pub struct EnigoInjector {
    enigo: Enigo,
    bounds: DisplayBounds,
    pressed: PressedState,
    /// Sub-line scroll remainder (trackpads send fractional deltas).
    wheel_acc: (f32, f32),
}

impl EnigoInjector {
    pub fn new(bounds: DisplayBounds) -> Result<Self> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| anyhow!("{e}"))
            .context("initializing enigo (on macOS grant Accessibility permission)")?;
        Ok(Self { enigo, bounds, pressed: PressedState::default(), wheel_acc: (0.0, 0.0) })
    }
}

fn dir(down: bool) -> Direction {
    if down { Direction::Press } else { Direction::Release }
}

impl InputInjector for EnigoInjector {
    fn mouse_move(&mut self, x: f32, y: f32) -> Result<()> {
        let b = self.bounds;
        let px = b.x + (x * b.width.saturating_sub(1) as f32).round() as i32;
        let py = b.y + (y * b.height.saturating_sub(1) as f32).round() as i32;
        self.enigo.move_mouse(px, py, Coordinate::Abs).map_err(|e| anyhow!("{e}"))
    }

    fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        let b = match button {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
        };
        self.pressed.button(button, down);
        self.enigo.button(b, dir(down)).map_err(|e| anyhow!("{e}"))
    }

    fn mouse_wheel(&mut self, dx: f32, dy: f32) -> Result<()> {
        self.wheel_acc.0 += dx;
        self.wheel_acc.1 += dy;
        let (lx, ly) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
        self.wheel_acc.0 -= lx;
        self.wheel_acc.1 -= ly;
        if ly != 0.0 {
            self.enigo.scroll(ly as i32, Axis::Vertical).map_err(|e| anyhow!("{e}"))?;
        }
        if lx != 0.0 {
            self.enigo.scroll(lx as i32, Axis::Horizontal).map_err(|e| anyhow!("{e}"))?;
        }
        Ok(())
    }

    fn key(&mut self, code: &str, down: bool) -> Result<()> {
        let Some(native) = keymap::lookup(code) else {
            tracing::debug!(code, "unmapped key ignored");
            return Ok(());
        };
        #[cfg(target_os = "macos")]
        let key = match native.mac_vk {
            Some(vk) => Key::Other(u32::from(vk)),
            None => return Ok(()),
        };
        // Linux: enigo's raw path expects X11 keycodes = evdev + 8. Phase 0 does
        // not target Linux agents yet; ignore keys there.
        #[cfg(not(target_os = "macos"))]
        let key: Key = {
            let _ = native;
            return Ok(());
        };
        self.pressed.key(code, down);
        self.enigo.key(key, dir(down)).map_err(|e| anyhow!("{e}"))
    }

    fn release_all(&mut self) -> Result<()> {
        let (keys, buttons) = self.pressed.take();
        for code in keys {
            let _ = self.key(&code, false);
        }
        for b in buttons {
            let _ = self.mouse_button(b, false);
        }
        self.pressed = PressedState::default();
        Ok(())
    }
}

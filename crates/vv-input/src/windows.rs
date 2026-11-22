//! Windows backend: `SendInput` with hardware scan codes.

use crate::{DisplayBounds, InputInjector, PressedState, keymap};
use anyhow::{Result, bail};
use vv_protocol::control::MouseButton;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};

/// `WHEEL_DELTA`: one notch of a standard mouse wheel.
const WHEEL_DELTA: f32 = 120.0;

/// Marks injected events so the agent could tell them apart from local input.
const VV_EXTRA_INFO: usize = 0x5656_5657; // "VVVW"

pub struct SendInputInjector {
    bounds: DisplayBounds,
    pressed: PressedState,
}

impl SendInputInjector {
    pub fn new(bounds: DisplayBounds) -> Self {
        Self { bounds, pressed: PressedState::default() }
    }

    fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: VV_EXTRA_INFO,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> Result<()> {
        // SAFETY: `inputs` is a valid slice of initialized INPUT structs and
        // cbSize is the size of one element, as SendInput requires.
        let sent = unsafe {
            SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32)
        };
        if sent as usize != inputs.len() {
            // Typical cause: UIPI blocks injecting into an elevated window
            // (e.g. Task Manager, UAC prompt) from a non-elevated agent.
            bail!("SendInput injected {sent}/{} events (blocked by UIPI?)", inputs.len());
        }
        Ok(())
    }
}

impl InputInjector for SendInputInjector {
    fn mouse_move(&mut self, x: f32, y: f32) -> Result<()> {
        // Phase 0 captures the primary display only. MOUSEEVENTF_ABSOLUTE
        // without VIRTUALDESK maps 0..=65535 onto the primary monitor, which
        // makes this independent of DPI scaling and process DPI awareness.
        // Multi-monitor (Phase 2) will switch to VIRTUALDESK + `self.bounds`.
        let _ = self.bounds;
        let ax = (x * 65535.0).round() as i32;
        let ay = (y * 65535.0).round() as i32;
        Self::send(&[Self::mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)])
    }

    fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        let flags = match (button, down) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        self.pressed.button(button, down);
        Self::send(&[Self::mouse(0, 0, 0, flags)])
    }

    fn mouse_wheel(&mut self, dx: f32, dy: f32) -> Result<()> {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0.0 {
            // Windows: positive = wheel rotated forward (scroll up).
            let amount = (-dy * WHEEL_DELTA).round() as i32;
            inputs.push(Self::mouse(0, 0, amount as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0.0 {
            let amount = (dx * WHEEL_DELTA).round() as i32;
            inputs.push(Self::mouse(0, 0, amount as u32, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() { Ok(()) } else { Self::send(&inputs) }
    }

    fn key(&mut self, code: &str, down: bool) -> Result<()> {
        let Some(native) = keymap::lookup(code) else {
            tracing::debug!(code, "unmapped key ignored");
            return Ok(());
        };
        let mut flags = KEYEVENTF_SCANCODE;
        if native.win_extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        self.pressed.key(code, down);
        Self::send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: native.win_scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: VV_EXTRA_INFO,
                },
            },
        }])
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

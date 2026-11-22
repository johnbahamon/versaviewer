//! Physical key table: W3C `KeyboardEvent.code` -> native key identifiers.
//!
//! * Windows: PC/AT Set-1 scan code + "extended" (E0-prefixed) flag.
//! * macOS:   Carbon virtual key code (`kVK_*`, HIToolbox/Events.h).
//!
//! Both are physical-position codes, so the remote layout decides the
//! resulting character, exactly as if the user typed on the remote keyboard.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeKey {
    /// Set-1 scan code (without the E0 prefix).
    pub win_scan: u16,
    /// Whether the scan code is E0-prefixed (`KEYEVENTF_EXTENDEDKEY`).
    pub win_extended: bool,
    /// macOS virtual key code, `None` when the key has no Mac equivalent.
    pub mac_vk: Option<u16>,
}

const fn k(win_scan: u16, win_extended: bool, mac_vk: u16) -> NativeKey {
    NativeKey { win_scan, win_extended, mac_vk: Some(mac_vk) }
}

const fn w(win_scan: u16, win_extended: bool) -> NativeKey {
    NativeKey { win_scan, win_extended, mac_vk: None }
}

/// Looks up a W3C `code`. Unknown codes return `None` and are ignored.
pub fn lookup(code: &str) -> Option<NativeKey> {
    Some(match code {
        // Letters
        "KeyA" => k(0x1E, false, 0x00),
        "KeyB" => k(0x30, false, 0x0B),
        "KeyC" => k(0x2E, false, 0x08),
        "KeyD" => k(0x20, false, 0x02),
        "KeyE" => k(0x12, false, 0x0E),
        "KeyF" => k(0x21, false, 0x03),
        "KeyG" => k(0x22, false, 0x05),
        "KeyH" => k(0x23, false, 0x04),
        "KeyI" => k(0x17, false, 0x22),
        "KeyJ" => k(0x24, false, 0x26),
        "KeyK" => k(0x25, false, 0x28),
        "KeyL" => k(0x26, false, 0x25),
        "KeyM" => k(0x32, false, 0x2E),
        "KeyN" => k(0x31, false, 0x2D),
        "KeyO" => k(0x18, false, 0x1F),
        "KeyP" => k(0x19, false, 0x23),
        "KeyQ" => k(0x10, false, 0x0C),
        "KeyR" => k(0x13, false, 0x0F),
        "KeyS" => k(0x1F, false, 0x01),
        "KeyT" => k(0x14, false, 0x11),
        "KeyU" => k(0x16, false, 0x20),
        "KeyV" => k(0x2F, false, 0x09),
        "KeyW" => k(0x11, false, 0x0D),
        "KeyX" => k(0x2D, false, 0x07),
        "KeyY" => k(0x15, false, 0x10),
        "KeyZ" => k(0x2C, false, 0x06),
        // Digit row
        "Digit1" => k(0x02, false, 0x12),
        "Digit2" => k(0x03, false, 0x13),
        "Digit3" => k(0x04, false, 0x14),
        "Digit4" => k(0x05, false, 0x15),
        "Digit5" => k(0x06, false, 0x17),
        "Digit6" => k(0x07, false, 0x16),
        "Digit7" => k(0x08, false, 0x1A),
        "Digit8" => k(0x09, false, 0x1C),
        "Digit9" => k(0x0A, false, 0x19),
        "Digit0" => k(0x0B, false, 0x1D),
        // Punctuation
        "Minus" => k(0x0C, false, 0x1B),
        "Equal" => k(0x0D, false, 0x18),
        "BracketLeft" => k(0x1A, false, 0x21),
        "BracketRight" => k(0x1B, false, 0x1E),
        "Backslash" => k(0x2B, false, 0x2A),
        "Semicolon" => k(0x27, false, 0x29),
        "Quote" => k(0x28, false, 0x27),
        "Backquote" => k(0x29, false, 0x32),
        "Comma" => k(0x33, false, 0x2B),
        "Period" => k(0x34, false, 0x2F),
        "Slash" => k(0x35, false, 0x2C),
        "IntlBackslash" => k(0x56, false, 0x0A),
        // Editing / whitespace
        "Escape" => k(0x01, false, 0x35),
        "Backspace" => k(0x0E, false, 0x33),
        "Tab" => k(0x0F, false, 0x30),
        "Enter" => k(0x1C, false, 0x24),
        "Space" => k(0x39, false, 0x31),
        "CapsLock" => k(0x3A, false, 0x39),
        // Modifiers
        "ShiftLeft" => k(0x2A, false, 0x38),
        "ShiftRight" => k(0x36, false, 0x3C),
        "ControlLeft" => k(0x1D, false, 0x3B),
        "ControlRight" => k(0x1D, true, 0x3E),
        "AltLeft" => k(0x38, false, 0x3A),
        "AltRight" => k(0x38, true, 0x3D),
        "MetaLeft" => k(0x5B, true, 0x37),
        "MetaRight" => k(0x5C, true, 0x36),
        "ContextMenu" => w(0x5D, true),
        // Function keys
        "F1" => k(0x3B, false, 0x7A),
        "F2" => k(0x3C, false, 0x78),
        "F3" => k(0x3D, false, 0x63),
        "F4" => k(0x3E, false, 0x76),
        "F5" => k(0x3F, false, 0x60),
        "F6" => k(0x40, false, 0x61),
        "F7" => k(0x41, false, 0x62),
        "F8" => k(0x42, false, 0x64),
        "F9" => k(0x43, false, 0x65),
        "F10" => k(0x44, false, 0x6D),
        "F11" => k(0x57, false, 0x67),
        "F12" => k(0x58, false, 0x6F),
        // Navigation (E0-extended on Windows)
        "Insert" => k(0x52, true, 0x72),
        "Delete" => k(0x53, true, 0x75),
        "Home" => k(0x47, true, 0x73),
        "End" => k(0x4F, true, 0x77),
        "PageUp" => k(0x49, true, 0x74),
        "PageDown" => k(0x51, true, 0x79),
        "ArrowUp" => k(0x48, true, 0x7E),
        "ArrowDown" => k(0x50, true, 0x7D),
        "ArrowLeft" => k(0x4B, true, 0x7B),
        "ArrowRight" => k(0x4D, true, 0x7C),
        "PrintScreen" => w(0x37, true),
        "ScrollLock" => w(0x46, false),
        // Numpad
        "NumLock" => k(0x45, true, 0x47),
        "NumpadDivide" => k(0x35, true, 0x4B),
        "NumpadMultiply" => k(0x37, false, 0x43),
        "NumpadSubtract" => k(0x4A, false, 0x4E),
        "NumpadAdd" => k(0x4E, false, 0x45),
        "NumpadEnter" => k(0x1C, true, 0x4C),
        "NumpadDecimal" => k(0x53, false, 0x41),
        "Numpad0" => k(0x52, false, 0x52),
        "Numpad1" => k(0x4F, false, 0x53),
        "Numpad2" => k(0x50, false, 0x54),
        "Numpad3" => k(0x51, false, 0x55),
        "Numpad4" => k(0x4B, false, 0x56),
        "Numpad5" => k(0x4C, false, 0x57),
        "Numpad6" => k(0x4D, false, 0x58),
        "Numpad7" => k(0x47, false, 0x59),
        "Numpad8" => k(0x48, false, 0x5B),
        "Numpad9" => k(0x49, false, 0x5C),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_are_extended_on_windows() {
        let up = lookup("ArrowUp").unwrap();
        assert!(up.win_extended);
        assert_eq!(up.win_scan, 0x48);
        // Same scan code, but not extended, is Numpad8.
        assert!(!lookup("Numpad8").unwrap().win_extended);
    }

    #[test]
    fn unknown_code_is_ignored() {
        assert_eq!(lookup("NotAKey"), None);
    }
}

//! Screen capture abstraction.
//!
//! [`ScreenCapturer`] is the platform-neutral contract used by the engine.
//! Phase 0 ships a single backend built on `xcap` (GDI/DXGI on Windows,
//! CoreGraphics on macOS, X11/PipeWire on Linux). Phase 2 will add a native
//! Windows backend on DXGI Desktop Duplication with dirty-rect support and
//! zero-copy GPU encode; it only needs to implement this trait.

use anyhow::{Context, Result, anyhow};

/// One captured frame, tightly packed RGBA8 (`stride == width * 4`).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Not `Send`: OS capture handles are thread-affine on Windows, so a capturer
/// must be created on (and stay on) the thread that drives it.
pub trait ScreenCapturer {
    /// Size in pixels of the frames this capturer produces.
    fn dimensions(&self) -> (u32, u32);
    /// Captures the current screen contents (blocking).
    fn capture(&mut self) -> Result<Frame>;
}

/// Description of a display available for capture.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: usize,
    pub name: String,
    /// Origin and size in the OS's logical (point) coordinate space.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let monitors = xcap::Monitor::all().context("enumerating monitors")?;
    Ok(monitors
        .iter()
        .enumerate()
        .map(|(index, m)| DisplayInfo {
            index,
            name: m.name().unwrap_or_default(),
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            primary: m.is_primary().unwrap_or(false),
        })
        .collect())
}

/// Capturer backed by `xcap`.
pub struct XcapCapturer {
    monitor: xcap::Monitor,
    width: u32,
    height: u32,
}

impl XcapCapturer {
    /// Opens the display at `index`, or the primary one when `None`.
    pub fn open(index: Option<usize>) -> Result<Self> {
        let monitors = xcap::Monitor::all().context("enumerating monitors")?;
        let monitor = match index {
            Some(i) => monitors.into_iter().nth(i).ok_or_else(|| anyhow!("no display #{i}"))?,
            None => {
                let primary = monitors.iter().position(|m| m.is_primary().unwrap_or(false));
                monitors
                    .into_iter()
                    .nth(primary.unwrap_or(0))
                    .ok_or_else(|| anyhow!("no displays found"))?
            }
        };
        // Probe once: dimensions reported by the OS may be logical (scaled)
        // while captured images are physical pixels. Trust the image.
        let probe = monitor.capture_image().context("initial capture")?;
        let (width, height) = (probe.width(), probe.height());
        tracing::info!(width, height, name = %monitor.name().unwrap_or_default(), "capturing display");
        Ok(Self { monitor, width, height })
    }
}

impl ScreenCapturer for XcapCapturer {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn capture(&mut self) -> Result<Frame> {
        let img = self.monitor.capture_image().context("capturing screen")?;
        let (width, height) = (img.width(), img.height());
        if (width, height) != (self.width, self.height) {
            // Resolution changed mid-session; caller must reconfigure the encoder.
            tracing::warn!(width, height, "display resolution changed");
            self.width = width;
            self.height = height;
        }
        Ok(Frame { width, height, rgba: img.into_raw() })
    }
}

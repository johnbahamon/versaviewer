//! Screen capture abstraction.
//!
//! [`ScreenCapturer`] is the platform-neutral contract used by the engine:
//! "give me the next frame if the screen changed within `timeout`".
//!
//! Backends:
//! * Windows: DXGI Desktop Duplication ([`dxgi`]) — GPU-side, change-driven,
//!   BGRA without per-pixel conversion. Falls back to GDI when unavailable
//!   (RDP sessions, some VMs, hybrid-GPU laptops).
//! * Everywhere: `xcap` polling (GDI on Windows, CoreGraphics on macOS,
//!   X11/PipeWire on Linux).

#[cfg(windows)]
pub mod dxgi;

use anyhow::{Context, Result, anyhow};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
}

/// One captured frame (32 bits per pixel).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (>= width * 4).
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

/// Not `Send`: OS capture handles are thread-affine on Windows, so a capturer
/// must be created on (and stay on) the thread that drives it.
pub trait ScreenCapturer {
    /// Size in pixels of the frames this capturer currently produces.
    fn dimensions(&self) -> (u32, u32);
    /// Short backend name for logs (`"dxgi"`, `"gdi/xcap"`...).
    fn backend(&self) -> &'static str;
    /// Waits up to `timeout` for a new frame. `Ok(None)` means the screen did
    /// not change (change-driven backends) — the caller keeps its last frame.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;
}

/// Process-wide setup that must run before any capture or window API.
///
/// On Windows it makes the process per-monitor DPI aware, so capture and
/// input work in physical pixels instead of DPI-virtualized coordinates.
pub fn platform_init() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        // SAFETY: no preconditions; fails harmlessly if already set (e.g. by a manifest).
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!("SetProcessDpiAwarenessContext: {e}");
        }
    }
}

/// Description of a display available for capture.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: usize,
    pub name: String,
    /// Origin and size in the OS's desktop coordinate space.
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

fn pick_display(index: Option<usize>) -> Result<DisplayInfo> {
    let displays = list_displays()?;
    match index {
        Some(i) => displays.get(i).cloned().ok_or_else(|| anyhow!("no display #{i}")),
        None => displays
            .iter()
            .find(|d| d.primary)
            .or(displays.first())
            .cloned()
            .ok_or_else(|| anyhow!("no displays found")),
    }
}

/// Opens the best available capturer for the display at `index` (primary
/// when `None`).
pub fn open_capturer(index: Option<usize>) -> Result<Box<dyn ScreenCapturer>> {
    #[cfg(windows)]
    {
        let display = pick_display(index)?;
        match dxgi::DxgiCapturer::open(display.x, display.y) {
            Ok(c) => {
                tracing::info!(
                    width = c.dimensions().0,
                    height = c.dimensions().1,
                    "capturing with DXGI Desktop Duplication"
                );
                return Ok(Box::new(c));
            }
            Err(e) => tracing::warn!("DXGI capture unavailable, falling back to GDI: {e:#}"),
        }
    }
    Ok(Box::new(XcapCapturer::open(index)?))
}

/// Polling capturer backed by `xcap` (GDI on Windows).
pub struct XcapCapturer {
    monitor: xcap::Monitor,
    width: u32,
    height: u32,
    /// First image, captured while probing the real size.
    pending: Option<Frame>,
}

impl XcapCapturer {
    /// Opens the display at `index`, or the primary one when `None`.
    pub fn open(index: Option<usize>) -> Result<Self> {
        let info = pick_display(index)?;
        let monitor = xcap::Monitor::all()
            .context("enumerating monitors")?
            .into_iter()
            .nth(info.index)
            .ok_or_else(|| anyhow!("display #{} disappeared", info.index))?;
        // Dimensions reported by the OS may be logical (scaled) while captured
        // images are physical pixels. Trust the image.
        let probe = Self::grab(&monitor)?;
        tracing::info!(
            width = probe.width,
            height = probe.height,
            name = %info.name,
            "capturing with xcap (polling)"
        );
        Ok(Self { width: probe.width, height: probe.height, pending: Some(probe), monitor })
    }

    fn grab(monitor: &xcap::Monitor) -> Result<Frame> {
        let img = monitor.capture_image().context("capturing screen")?;
        let (width, height) = (img.width(), img.height());
        Ok(Frame {
            width,
            height,
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            data: img.into_raw(),
        })
    }
}

impl ScreenCapturer for XcapCapturer {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn backend(&self) -> &'static str {
        "xcap"
    }

    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<Frame>> {
        if let Some(f) = self.pending.take() {
            return Ok(Some(f));
        }
        let frame = Self::grab(&self.monitor)?;
        if (frame.width, frame.height) != (self.width, self.height) {
            tracing::warn!(frame.width, frame.height, "display resolution changed");
            self.width = frame.width;
            self.height = frame.height;
        }
        Ok(Some(frame))
    }
}

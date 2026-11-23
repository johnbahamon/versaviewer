//! Windows DXGI Desktop Duplication capturer.
//!
//! * Change-driven: `AcquireNextFrame` blocks until the desktop changes, so a
//!   static screen costs ~nothing and the caller simply keeps its last frame.
//! * One staging texture, reused; one `memcpy` of the mapped surface; BGRA is
//!   handed to the encoder as-is (no per-pixel swizzle).
//! * `DXGI_ERROR_ACCESS_LOST` (desktop switch, mode change, UAC prompt on the
//!   secure desktop, full-screen app) recreates the duplication with backoff.
//! * Everything is released on drop — no background threads.
//!
//! Known limits (Phase 0/1): the mouse cursor is not composited (the viewer
//! shows its own), rotated displays are sent unrotated, and the secure
//! desktop (UAC/lock screen) is not visible to a user-session process.

use crate::{Frame, PixelFormat, ScreenCapturer};
use anyhow::{Context, Result, anyhow};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource,
};
use windows::core::Interface;

/// A desktop frame held by DXGI. The texture is only valid until this is
/// dropped (which calls `ReleaseFrame`), so copy what you need first.
pub struct AcquiredFrame {
    duplication: IDXGIOutputDuplication,
    pub texture: ID3D11Texture2D,
}

impl Drop for AcquiredFrame {
    fn drop(&mut self) {
        // SAFETY: paired with the successful AcquireNextFrame that created us.
        // ACCESS_LOST here is detected again by the next acquire.
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }
}

pub struct DxgiCapturer {
    device: ID3D11Device,
    adapter_name: String,
    /// Device created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` (needed for the
    /// GPU scaler and hardware encoders).
    video_support: bool,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    width: u32,
    height: u32,
    /// True until the first frame has been delivered (it must be delivered
    /// even if DXGI reports "no new present").
    need_initial: bool,
    retry_at: Option<Instant>,
    /// Recycled frame buffers (see [`ScreenCapturer::recycle`]).
    spare: Vec<Vec<u8>>,
}

impl DxgiCapturer {
    /// Opens the output whose desktop origin is (`x`, `y`) — (0, 0) is the
    /// primary display.
    pub fn open(x: i32, y: i32) -> Result<Self> {
        // SAFETY: plain COM calls; all out-pointers are valid locals.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let mut adapter_index = 0;
            loop {
                let adapter = match factory.EnumAdapters1(adapter_index) {
                    Ok(a) => a,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e).context("EnumAdapters1"),
                };
                adapter_index += 1;
                let mut output_index = 0;
                loop {
                    let output = match adapter.EnumOutputs(output_index) {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    output_index += 1;
                    let desc = output.GetDesc().context("IDXGIOutput::GetDesc")?;
                    let r = desc.DesktopCoordinates;
                    if !desc.AttachedToDesktop.as_bool() || (r.left, r.top) != (x, y) {
                        continue;
                    }
                    let adapter_desc = adapter.GetDesc1().context("GetDesc1")?;
                    let name_len =
                        adapter_desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
                    let adapter_name =
                        String::from_utf16_lossy(&adapter_desc.Description[..name_len]);
                    let adapter: IDXGIAdapter = adapter.cast()?;
                    let create = |flags| {
                        let mut device = None;
                        let mut context = None;
                        D3D11CreateDevice(
                            &adapter,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            flags,
                            None,
                            D3D11_SDK_VERSION,
                            Some(&mut device),
                            None,
                            Some(&mut context),
                        )
                        .map(|()| (device, context))
                    };
                    let (created, video_support) = match create(
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    ) {
                        Ok(dc) => (dc, true),
                        Err(e) => {
                            tracing::info!("D3D11 device without video support ({e})");
                            (create(D3D11_CREATE_DEVICE_BGRA_SUPPORT).context("D3D11CreateDevice")?, false)
                        }
                    };
                    let device = created.0.ok_or_else(|| anyhow!("no D3D11 device"))?;
                    let context = created.1.ok_or_else(|| anyhow!("no D3D11 context"))?;
                    let output: IDXGIOutput1 = output.cast().context("IDXGIOutput1")?;
                    let mut capturer = Self {
                        device,
                        adapter_name,
                        video_support,
                        context,
                        output,
                        duplication: None,
                        staging: None,
                        width: (r.right - r.left) as u32,
                        height: (r.bottom - r.top) as u32,
                        need_initial: true,
                        retry_at: None,
                        spare: Vec::new(),
                    };
                    capturer.duplicate().context("DuplicateOutput")?;
                    return Ok(capturer);
                }
            }
        }
        Err(anyhow!("no DXGI output at desktop origin ({x}, {y})"))
    }

    fn duplicate(&mut self) -> Result<()> {
        // SAFETY: valid device/output owned by self.
        let dup = unsafe { self.output.DuplicateOutput(&self.device)? };
        // SAFETY: valid duplication interface.
        let desc = unsafe { dup.GetDesc() };
        self.width = desc.ModeDesc.Width;
        self.height = desc.ModeDesc.Height;
        if desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(anyhow!("unsupported desktop format {:?}", desc.ModeDesc.Format));
        }
        self.duplication = Some(dup);
        self.need_initial = true;
        Ok(())
    }

    fn staging_for(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture, valid out-pointer.
        unsafe { src.GetDesc(&mut desc) };
        if let Some((tex, w, h)) = &self.staging
            && (*w, *h) == (desc.Width, desc.Height) {
                return Ok(tex.clone());
            }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        let mut tex = None;
        // SAFETY: valid descriptor and out-pointer.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("CreateTexture2D(staging)")?;
        let tex = tex.ok_or_else(|| anyhow!("no staging texture"))?;
        self.staging = Some((tex.clone(), desc.Width, desc.Height));
        Ok(tex)
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    pub fn video_support(&self) -> bool {
        self.video_support
    }

    /// GPU (adapter) the captured display is attached to.
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Waits up to `timeout` for a desktop change and returns the frame while
    /// DXGI holds it. `Ok(None)`: no change (or duplication being recreated).
    pub fn acquire(&mut self, timeout: Duration) -> Result<Option<AcquiredFrame>> {
        if self.duplication.is_none() {
            // Recreate with backoff: while the secure desktop (UAC/lock) is up,
            // DuplicateOutput fails with E_ACCESSDENIED.
            if self.retry_at.is_some_and(|t| t > Instant::now()) {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            if let Err(e) = self.duplicate() {
                tracing::debug!("DuplicateOutput failed, will retry: {e:#}");
                self.retry_at = Some(Instant::now() + Duration::from_millis(500));
                std::thread::sleep(timeout);
                return Ok(None);
            }
        }
        let dup = self.duplication.clone().expect("duplication present");

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: valid out-pointers; a successful acquire is released by
        // AcquiredFrame::drop (or right below when unused).
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.lost("access lost");
                return Ok(None);
            }
            Err(e) => {
                self.lost(&e.to_string());
                return Ok(None);
            }
        }
        // LastPresentTime == 0: only the cursor moved, the image is unchanged.
        let changed = info.LastPresentTime != 0 || self.need_initial;
        let texture = resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok());
        match (changed, texture) {
            (true, Some(texture)) => {
                self.need_initial = false;
                Ok(Some(AcquiredFrame { duplication: dup, texture }))
            }
            _ => {
                // SAFETY: paired with the successful AcquireNextFrame above.
                let _ = unsafe { dup.ReleaseFrame() };
                Ok(None)
            }
        }
    }

    fn read_back(&mut self, texture: &ID3D11Texture2D) -> Result<Frame> {
        let staging = self.staging_for(texture)?;
        let (width, height) = self
            .staging
            .as_ref()
            .map(|s| (s.1, s.2))
            .ok_or_else(|| anyhow!("no staging texture"))?;
        // SAFETY: both textures belong to self.device; Map/Unmap are paired and
        // the mapped region is read only within RowPitch * height bytes.
        unsafe {
            self.context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(staging)")?;
            let stride = mapped.RowPitch as usize;
            let len = stride * height as usize;
            let mut data = self.spare.pop().unwrap_or_default();
            data.clear();
            data.extend_from_slice(std::slice::from_raw_parts(mapped.pData as *const u8, len));
            self.context.Unmap(&staging, 0);
            Ok(Frame { width, height, stride, format: PixelFormat::Bgra8, data })
        }
    }

    fn lost(&mut self, why: &str) {
        tracing::info!("DXGI duplication lost ({why}); recreating");
        self.duplication = None;
        self.retry_at = Some(Instant::now());
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn backend(&self) -> &'static str {
        "dxgi"
    }

    fn recycle(&mut self, buffer: Vec<u8>) {
        if self.spare.len() < 3 {
            self.spare.push(buffer);
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let Some(frame) = self.acquire(timeout)? else {
            return Ok(None);
        };
        let texture = frame.texture.clone();
        let result = self.read_back(&texture);
        drop(frame); // ReleaseFrame after the copy is queued and mapped
        result.map(Some)
    }
}

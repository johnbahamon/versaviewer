//! Windows GPU video pipeline:
//!
//! ```text
//! DXGI desktop (BGRA, GPU) -> D3D11 video processor (scale + NV12, GPU)
//!     -> Media Foundation hardware H.264 encoder        (zero CPU copies)
//!     -> or: NV12 read-back (~3 MB at 1080p) + OpenH264 (no HW encoder)
//! ```

mod mf;
mod scaler;

use crate::{EncoderChoice, GpuTimings, PipelineInfo};
use anyhow::{Context, Result, anyhow, ensure};
use mf::MfEncoder;
use scaler::GpuScaler;
use std::time::{Duration, Instant};
use vv_capture::dxgi::DxgiCapturer;
use vv_codec::{EncodedFrame, EncoderSettings, H264Encoder, RawNv12, fit_within};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

/// COM + Media Foundation lifetime for the pipeline thread.
struct Runtime {
    com: bool,
    mf: bool,
}

impl Runtime {
    fn start() -> Result<Self> {
        // SAFETY: called on the thread that will use COM; paired in Drop.
        unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            MFStartup(MF_VERSION, MFSTARTUP_LITE).context("MFStartup")?;
            Ok(Self { com, mf: true })
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: paired with `start` on the same thread.
        unsafe {
            if self.mf {
                let _ = MFShutdown();
            }
            if self.com {
                CoUninitialize();
            }
        }
    }
}

enum Backend {
    Hardware(MfEncoder),
    Software { encoder: Box<H264Encoder>, staging: ID3D11Texture2D },
}

/// Not `Send`: lives on the thread that created it (COM apartment, D3D11
/// immediate context).
pub struct GpuPipeline {
    // Field order = drop order: everything using MF/COM before `_runtime`.
    backend: Backend,
    scaler: GpuScaler,
    capturer: DxgiCapturer,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    settings: EncoderSettings,
    choice: EncoderChoice,
    current: usize,
    /// The current picture changed but the encoder produced nothing for it
    /// (busy, or skipped by rate control): encode it again on the next poll
    /// so a screen that then stays static is never left stale on the viewer.
    pending: bool,
    info: PipelineInfo,
    timings: GpuTimings,
    _runtime: Runtime,
}

fn texture_size(t: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture and out-pointer.
    unsafe { t.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

fn software_backend(device: &ID3D11Device, size: (u32, u32), settings: &EncoderSettings) -> Result<Backend> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: valid descriptor and out-pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.context("NV12 staging texture")?;
    Ok(Backend::Software {
        encoder: Box::new(H264Encoder::new(size.0, size.1, *settings)?),
        staging: staging.ok_or_else(|| anyhow!("no staging texture"))?,
    })
}

fn make_backend(
    device: &ID3D11Device,
    size: (u32, u32),
    settings: &EncoderSettings,
    choice: EncoderChoice,
) -> Result<(Backend, String, bool)> {
    let hardware = || MfEncoder::open(device, size.0, size.1, settings);
    match choice {
        EncoderChoice::Software => Ok((software_backend(device, size, settings)?, "openh264".into(), false)),
        EncoderChoice::Hardware => {
            let enc = hardware()?;
            let name = enc.name().to_owned();
            Ok((Backend::Hardware(enc), name, true))
        }
        EncoderChoice::Auto => match hardware() {
            Ok(enc) => {
                let name = enc.name().to_owned();
                Ok((Backend::Hardware(enc), name, true))
            }
            Err(e) => {
                tracing::info!("hardware encoder unavailable, using OpenH264: {e:#}");
                Ok((software_backend(device, size, settings)?, "openh264".into(), false))
            }
        },
    }
}

impl GpuPipeline {
    pub fn open(display: Option<usize>, settings: EncoderSettings, choice: EncoderChoice) -> Result<Self> {
        let runtime = Runtime::start()?;
        let target = vv_capture::pick_display(display)?;
        let mut capturer = DxgiCapturer::open(target.x, target.y)?;
        ensure!(capturer.video_support(), "the GPU driver has no D3D11 video support");
        let device = capturer.device().clone();
        let context = capturer.context().clone();

        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            if let Some(f) = capturer.acquire(Duration::from_millis(100))? {
                break f;
            }
            ensure!(Instant::now() < deadline, "no initial desktop frame");
        };
        let source = texture_size(&first.texture);
        let encoded = fit_within(source.0, source.1, settings.max_width, settings.max_height);
        let mut scaler = GpuScaler::new(&device, source, encoded).context("GPU scaler")?;
        let current = scaler.process(&context, &first.texture)?;
        drop(first);

        let (backend, encoder_name, hardware) = make_backend(&device, encoded, &settings, choice)?;
        let info = PipelineInfo {
            adapter: capturer.adapter_name().to_owned(),
            encoder: encoder_name,
            hardware_encoder: hardware,
            source,
            encoded,
        };
        Ok(Self {
            backend,
            scaler,
            capturer,
            device,
            context,
            settings,
            choice,
            current,
            pending: true,
            info,
            timings: GpuTimings::default(),
            _runtime: runtime,
        })
    }

    pub fn info(&self) -> &PipelineInfo {
        &self.info
    }

    pub fn last_timings(&self) -> GpuTimings {
        self.timings
    }

    pub fn request_keyframe(&mut self) {
        match &mut self.backend {
            Backend::Hardware(enc) => enc.request_keyframe(),
            Backend::Software { encoder, .. } => encoder.request_keyframe(),
        }
    }

    /// Display resolution changed: rebuild the scaler (and the encoder if the
    /// encoded size changes).
    fn rebuild(&mut self, source: (u32, u32)) -> Result<()> {
        let s = &self.settings;
        let encoded = fit_within(source.0, source.1, s.max_width, s.max_height);
        self.scaler = GpuScaler::new(&self.device, source, encoded)?;
        if encoded != self.info.encoded {
            let (backend, name, hw) = make_backend(&self.device, encoded, s, self.choice)?;
            self.backend = backend;
            self.info.encoder = name;
            self.info.hardware_encoder = hw;
        }
        tracing::info!(
            source = format!("{}x{}", source.0, source.1),
            encoded = format!("{}x{}", encoded.0, encoded.1),
            "display resolution changed; GPU pipeline rebuilt"
        );
        self.info.source = source;
        self.info.encoded = encoded;
        self.request_keyframe();
        Ok(())
    }

    /// Waits up to `timeout` for a screen change and encodes it. With
    /// `force`, the current picture is (re-)encoded even without a change
    /// (keyframe requests, refresh after a skipped frame, first frame).
    /// An empty result means nothing was produced this time.
    pub fn poll(&mut self, timeout: Duration, force: bool) -> Result<Vec<EncodedFrame>> {
        let t0 = Instant::now();
        let mut changed = false;
        if let Some(frame) = self.capturer.acquire(timeout)? {
            let size = texture_size(&frame.texture);
            if size != self.scaler.source_size() {
                self.rebuild(size)?;
            }
            self.current = self.scaler.process(&self.context, &frame.texture)?;
            changed = true;
        }
        self.timings.capture = t0.elapsed();
        if !changed && !force && !self.pending {
            return Ok(Vec::new());
        }

        let surface = self.scaler.surface(self.current).clone();
        let (w, h) = self.scaler.output_size();
        let t1 = Instant::now();
        let frames = match &mut self.backend {
            Backend::Hardware(enc) => {
                self.timings.readback = Duration::ZERO;
                enc.encode(&surface)?
            }
            Backend::Software { encoder, staging } => {
                // SAFETY: Map/Unmap paired; reads stay within RowPitch * h * 3/2,
                // the NV12 layout of a staging texture (UV plane after Y).
                let result = unsafe {
                    self.context.CopyResource(&*staging, &surface);
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    self.context.Map(&*staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("Map(NV12)")?;
                    self.timings.readback = t1.elapsed();
                    let pitch = mapped.RowPitch as usize;
                    let base = mapped.pData as *const u8;
                    let y = std::slice::from_raw_parts(base, pitch * h as usize);
                    let uv = std::slice::from_raw_parts(base.add(pitch * h as usize), pitch * h as usize / 2);
                    let r = encoder.encode_nv12(&RawNv12 { y, uv, width: w, height: h, y_stride: pitch, uv_stride: pitch });
                    self.context.Unmap(&*staging, 0);
                    r
                };
                result?.into_iter().collect()
            }
        };
        self.timings.encode = t1.elapsed() - self.timings.readback;
        self.pending = frames.is_empty();
        Ok(frames)
    }
}

/// Human-readable report of what the GPU pipeline can use on this machine.
/// Never fails: every problem becomes a line of the report.
pub fn diagnose(display: Option<usize>) -> Vec<String> {
    let mut out = Vec::new();
    let runtime = match Runtime::start() {
        Ok(r) => r,
        Err(e) => return vec![format!("Media Foundation: NOT available ({e:#})")],
    };
    out.push("Media Foundation: available".into());

    match vv_capture::pick_display(display) {
        Ok(d) => out.push(format!("Display: #{} {} {}x{} at ({},{})", d.index, d.name, d.width, d.height, d.x, d.y)),
        Err(e) => out.push(format!("Display: none ({e:#})")),
    }
    match vv_capture::pick_display(display).and_then(|d| DxgiCapturer::open(d.x, d.y)) {
        Ok(c) => out.push(format!(
            "DXGI Desktop Duplication: OK on \"{}\" (D3D11 video support: {})",
            c.adapter_name(),
            if c.video_support() { "yes" } else { "NO" }
        )),
        Err(e) => out.push(format!("DXGI Desktop Duplication: FAILED ({e:#})")),
    }

    match mf::enumerate() {
        Ok(list) if list.is_empty() => out.push("Hardware H.264 encoders: none".into()),
        Ok(list) => {
            out.push(format!("Hardware H.264 encoders: {}", list.len()));
            for a in &list {
                out.push(format!("  - {}", mf::friendly_name(a)));
            }
        }
        Err(e) => out.push(format!("Hardware H.264 encoders: enumeration failed ({e:#})")),
    }

    match GpuPipeline::open(display, EncoderSettings::default(), EncoderChoice::Auto) {
        Ok(p) => {
            let i = p.info();
            out.push(format!(
                "GPU pipeline: OK — {}x{} -> {}x{}, encoder: {}{}",
                i.source.0,
                i.source.1,
                i.encoded.0,
                i.encoded.1,
                i.encoder,
                if i.hardware_encoder { " (hardware)" } else { " (software)" }
            ));
        }
        Err(e) => out.push(format!("GPU pipeline: UNAVAILABLE ({e:#}) -> the agent will use the CPU pipeline")),
    }
    drop(runtime);
    out
}

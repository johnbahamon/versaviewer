//! GPU downscaling + BGRA -> NV12 colour conversion with the D3D11 video
//! processor (fixed-function video hardware present on every DX11 GPU).
//!
//! Output colour space: BT.601 limited range (16-235), which is what the
//! viewer's decoder assumes when converting back to RGB.

use anyhow::{Context, Result, anyhow, ensure};
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_QUALITY, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::{BOOL, Interface};

/// Number of NV12 output surfaces. A hardware encoder may still be reading
/// surface N while we write N+1, so we rotate instead of overwriting.
const RING: usize = 4;

/// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bitfield: Usage:1, RGB_Range:1,
/// YCbCr_Matrix:1, YCbCr_xvYCC:1, Nominal_Range:2.
const CS_RGB_FULL: u32 = 0; // RGB_Range = 0 -> 0-255
const CS_YCBCR_601_STUDIO: u32 = 1 << 4; // matrix BT.601, Nominal_Range = 16-235

pub struct GpuScaler {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    src: (u32, u32),
    dst: (u32, u32),
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: i32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: valid descriptor and out-pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }?;
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))
}

impl GpuScaler {
    /// `src`: desktop size (BGRA). `dst`: encoded size (NV12, even dimensions).
    pub fn new(device: &ID3D11Device, src: (u32, u32), dst: (u32, u32)) -> Result<Self> {
        ensure!(dst.0.is_multiple_of(2) && dst.1.is_multiple_of(2), "NV12 needs even dimensions");
        let video_device: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice")?;
        // SAFETY: device is valid; every COM out-value is checked.
        unsafe {
            let context = device.GetImmediateContext().context("GetImmediateContext")?;
            let video_context: ID3D11VideoContext = context.cast().context("ID3D11VideoContext")?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: src.0,
                InputHeight: src.1,
                OutputFrameRate: rate,
                OutputWidth: dst.0,
                OutputHeight: dst.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
            };
            let enumerator: ID3D11VideoProcessorEnumerator = video_device
                .CreateVideoProcessorEnumerator(&content)
                .context("CreateVideoProcessorEnumerator")?;
            let bgra = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?;
            let nv12 = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?;
            ensure!(
                bgra & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 != 0,
                "video processor cannot read BGRA"
            );
            ensure!(
                nv12 & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0,
                "video processor cannot write NV12"
            );
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .context("CreateVideoProcessor")?;

            // Our own copy of the desktop: the duplication surface may change
            // identity between frames and is released right after the copy.
            let input = texture(
                device,
                src.0,
                src.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0,
            )
            .context("input texture")?;
            let mut input_view = None;
            video_device
                .CreateVideoProcessorInputView(
                    &input,
                    &enumerator,
                    &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                        },
                    },
                    Some(&mut input_view),
                )
                .context("CreateVideoProcessorInputView")?;
            let input_view = input_view.ok_or_else(|| anyhow!("no input view"))?;

            let mut outputs = Vec::with_capacity(RING);
            for _ in 0..RING {
                // BIND_VIDEO_ENCODER lets hardware encoders read the surface
                // directly; fall back if the driver refuses it.
                let tex = texture(
                    device,
                    dst.0,
                    dst.1,
                    DXGI_FORMAT_NV12,
                    D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0,
                )
                .or_else(|_| texture(device, dst.0, dst.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0))
                .context("NV12 output texture")?;
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(
                        &tex,
                        &enumerator,
                        &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                            },
                        },
                        Some(&mut view),
                    )
                    .context("CreateVideoProcessorOutputView")?;
                outputs.push((tex, view.ok_or_else(|| anyhow!("no output view"))?));
            }

            // Fixed configuration: progressive input, no driver "enhancements",
            // explicit full-surface rects and colour spaces.
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let src_rect = RECT { left: 0, top: 0, right: src.0 as i32, bottom: src.1 as i32 };
            let dst_rect = RECT { left: 0, top: 0, right: dst.0 as i32, bottom: dst.1 as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src_rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst_rect));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst_rect));
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: CS_RGB_FULL },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: CS_YCBCR_601_STUDIO },
            );

            Ok(Self { video_context, processor, input, input_view, outputs, next: 0, src, dst })
        }
    }

    pub fn source_size(&self) -> (u32, u32) {
        self.src
    }

    pub fn output_size(&self) -> (u32, u32) {
        self.dst
    }

    /// Copies `desktop` and scales/converts it into the next NV12 surface.
    /// Returns that surface's slot index. Work is queued on the GPU; whoever
    /// consumes the surface (Map, encoder) synchronises implicitly.
    pub fn process(&mut self, context: &ID3D11DeviceContext, desktop: &ID3D11Texture2D) -> Result<usize> {
        let slot = self.next;
        self.next = (self.next + 1) % self.outputs.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let streams = [stream];
        // SAFETY: all resources belong to the same device; the stream array
        // outlives the call; the cloned view reference is released below.
        let result = unsafe {
            context.CopyResource(&self.input, desktop);
            self.video_context.VideoProcessorBlt(&self.processor, &self.outputs[slot].1, 0, &streams)
        };
        let [stream] = streams;
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        result.context("VideoProcessorBlt")?;
        Ok(slot)
    }

    pub fn surface(&self, slot: usize) -> &ID3D11Texture2D {
        &self.outputs[slot].0
    }
}

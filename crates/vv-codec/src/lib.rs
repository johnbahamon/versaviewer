//! Video encode/decode.
//!
//! Phase 0 uses Cisco's OpenH264 (software, built from source, BSD-licensed)
//! on both ends: it compiles everywhere without system dependencies and its
//! Constrained Baseline output is what WebRTC H.264 negotiation expects
//! (`profile-level-id=42e01f`, `packetization-mode=1`).
//!
//! Hardware encoders (Media Foundation / NVENC / QuickSync on Windows,
//! VideoToolbox on Apple) are a Phase 2 optimization behind the same API.
//!
//! The bitstream format at this boundary is H.264 **Annex-B** (start-code
//! prefixed NAL units), which is what the RTP packetizer consumes and what the
//! depacketizer produces.

pub mod convert;

use anyhow::{Context, Result};
pub use convert::{PixelOrder, RawImage, RawNv12, fit_within};
use convert::I420Buffer;
use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSource;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Force an IDR every N frames so a viewer can always resync.
    pub keyframe_interval: u32,
    /// Frames larger than this are downscaled (area average) before encoding.
    /// Software H.264 cost grows with pixel count; 1920x1200 keeps text legible
    /// while making 4K / high-DPI screens encodable in real time.
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 6_000_000,
            max_fps: 30.0,
            keyframe_interval: 300,
            max_width: 1920,
            max_height: 1200,
        }
    }
}

/// Encoded access unit (one frame) in Annex-B format.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Time spent in each stage of the last [`H264Encoder::encode`] call.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeTimings {
    pub convert: Duration,
    pub encode: Duration,
}

pub struct H264Encoder {
    inner: Encoder,
    yuv: I420Buffer,
    source: (u32, u32),
    settings: EncoderSettings,
    timings: EncodeTimings,
}

impl H264Encoder {
    /// Creates an encoder for frames of `src_width` x `src_height`. The encoded
    /// size is that, scaled down to fit `settings.max_*` and rounded to even.
    pub fn new(src_width: u32, src_height: u32, settings: EncoderSettings) -> Result<Self> {
        let (width, height) =
            fit_within(src_width, src_height, settings.max_width, settings.max_height);
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps))
            .intra_frame_period(IntraFramePeriod::from_num_frames(settings.keyframe_interval))
            .complexity(Complexity::Low)
            .num_threads(0) // auto: one per core
            .adaptive_quantization(false)
            .background_detection(false)
            .skip_frames(true);
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("creating OpenH264 encoder")?;
        Ok(Self {
            inner,
            yuv: I420Buffer::new(width, height),
            source: (src_width, src_height),
            settings,
            timings: EncodeTimings::default(),
        })
    }

    /// Encoded (output) size.
    pub fn dimensions(&self) -> (u32, u32) {
        self.yuv.dimensions_u32()
    }

    /// Input frame size this encoder was created for.
    pub fn source_dimensions(&self) -> (u32, u32) {
        self.source
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    pub fn last_timings(&self) -> EncodeTimings {
        self.timings
    }

    /// Makes the next encoded frame an IDR (keyframe).
    pub fn request_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }

    /// Converts, downscales and encodes one frame. Returns `None` when the
    /// encoder decided to skip it (rate control).
    pub fn encode(&mut self, src: &RawImage<'_>) -> Result<Option<EncodedFrame>> {
        anyhow::ensure!(
            (src.width, src.height) == self.source,
            "frame {}x{} does not match encoder input {}x{}",
            src.width,
            src.height,
            self.source.0,
            self.source.1
        );
        let t0 = Instant::now();
        self.yuv.fill_from(src);
        let t1 = Instant::now();
        let bitstream = self.inner.encode(&self.yuv).context("encoding frame")?;
        let frame_type = bitstream.frame_type();
        let data = bitstream.to_vec();
        self.timings = EncodeTimings { convert: t1 - t0, encode: t1.elapsed() };
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) || data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame { data, keyframe: matches!(frame_type, FrameType::IDR | FrameType::I) }))
    }

    /// Encodes an NV12 frame that is already at the encoded size (GPU path:
    /// scaling and colour conversion were done by the video processor).
    pub fn encode_nv12(&mut self, src: &RawNv12<'_>) -> Result<Option<EncodedFrame>> {
        anyhow::ensure!(
            (src.width, src.height) == self.dimensions(),
            "NV12 frame {}x{} does not match encoder output {}x{}",
            src.width,
            src.height,
            self.dimensions().0,
            self.dimensions().1
        );
        let t0 = Instant::now();
        self.yuv.fill_from_nv12(src);
        let t1 = Instant::now();
        let bitstream = self.inner.encode(&self.yuv).context("encoding frame")?;
        let frame_type = bitstream.frame_type();
        let data = bitstream.to_vec();
        self.timings = EncodeTimings { convert: t1 - t0, encode: t1.elapsed() };
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) || data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame { data, keyframe: matches!(frame_type, FrameType::IDR | FrameType::I) }))
    }

    /// Convenience for tightly packed RGBA buffers.
    pub fn encode_rgba(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<Option<EncodedFrame>> {
        self.encode(&RawImage { data: rgba, width, height, stride: width as usize * 4, order: PixelOrder::Rgba })
    }
}

/// Decoded frame as packed `0x00RRGGBB` pixels (the format `softbuffer` wants).
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub xrgb: Vec<u32>,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DecodedFrame({}x{})", self.width, self.height)
    }
}

pub struct H264Decoder {
    inner: Decoder,
    rgba: Vec<u8>,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let inner = Decoder::new().context("creating OpenH264 decoder")?;
        Ok(Self { inner, rgba: Vec::new() })
    }

    /// Decodes one Annex-B access unit. Returns `None` while the decoder is
    /// still waiting for a keyframe or buffering.
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedFrame>> {
        let Some(yuv) = self.inner.decode(annexb).context("decoding frame")? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        self.rgba.resize(w * h * 4, 0);
        yuv.write_rgba8(&mut self.rgba);
        let xrgb = self
            .rgba
            .as_chunks::<4>().0.iter()
            .map(|p| (u32::from(p[0]) << 16) | (u32::from(p[1]) << 8) | u32::from(p[2]))
            .collect();
        Ok(Some(DecodedFrame { width: w as u32, height: h as u32, xrgb }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (321u32, 241u32); // odd on purpose
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = (i % 255) as u8;
            px[1] = 128;
            px[2] = 200;
            px[3] = 255;
        }
        let mut enc = H264Encoder::new(w, h, EncoderSettings::default()).unwrap();
        assert_eq!(enc.dimensions(), (320, 240));
        let mut dec = H264Decoder::new().unwrap();

        let mut decoded = None;
        for _ in 0..3 {
            if let Some(frame) = enc.encode_rgba(&rgba, w, h).unwrap()
                && let Some(d) = dec.decode(&frame.data).unwrap() {
                    decoded = Some(d);
                }
        }
        let d = decoded.expect("decoder produced no frame");
        assert_eq!((d.width, d.height), (320, 240));
        assert_eq!(d.xrgb.len(), 320 * 240);
    }
}

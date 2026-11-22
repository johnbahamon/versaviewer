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

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Force an IDR every N frames so a viewer can always resync.
    pub keyframe_interval: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { bitrate_bps: 6_000_000, max_fps: 30.0, keyframe_interval: 300 }
    }
}

/// Encoded access unit (one frame) in Annex-B format.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub struct H264Encoder {
    inner: Encoder,
    yuv: YUVBuffer,
    width: u32,
    height: u32,
    settings: EncoderSettings,
}

/// H.264 4:2:0 requires even dimensions; we crop the last odd row/column.
pub fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, settings: EncoderSettings) -> Result<Self> {
        let (width, height) = even_dimensions(width, height);
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps))
            .intra_frame_period(IntraFramePeriod::from_num_frames(settings.keyframe_interval))
            .adaptive_quantization(false)
            .background_detection(false)
            .skip_frames(true);
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("creating OpenH264 encoder")?;
        Ok(Self {
            inner,
            yuv: YUVBuffer::new(width as usize, height as usize),
            width,
            height,
            settings,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// Makes the next encoded frame an IDR (keyframe).
    pub fn request_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }

    /// Encodes one RGBA frame. `src_width`/`src_height` may be one pixel larger
    /// than the encoder dimensions (odd sizes); extra rows/columns are dropped.
    /// Returns `None` when the encoder decided to skip the frame.
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
    ) -> Result<Option<EncodedFrame>> {
        anyhow::ensure!(
            even_dimensions(src_width, src_height) == (self.width, self.height),
            "frame {src_width}x{src_height} does not match encoder {}x{}",
            self.width,
            self.height
        );
        let cropped;
        let pixels: &[u8] = if src_width == self.width && src_height == self.height {
            rgba
        } else {
            cropped = crop_rgba(rgba, src_width, self.width, self.height);
            &cropped
        };
        self.yuv
            .read_rgba8(RgbaSliceU8::new(pixels, (self.width as usize, self.height as usize)));
        let bitstream = self.inner.encode(&self.yuv).context("encoding frame")?;
        let frame_type = bitstream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame { data, keyframe: matches!(frame_type, FrameType::IDR | FrameType::I) }))
    }
}

fn crop_rgba(src: &[u8], src_width: u32, width: u32, height: u32) -> Vec<u8> {
    let src_stride = src_width as usize * 4;
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        out.extend_from_slice(&src[y * src_stride..y * src_stride + row]);
    }
    out
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

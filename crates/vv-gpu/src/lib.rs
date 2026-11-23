//! GPU video pipeline (Phase 1): capture, downscale, colour-convert and
//! encode without the CPU touching every pixel.
//!
//! Windows only for now (DXGI + D3D11 video processor + Media Foundation
//! hardware encoders). On other platforms [`GpuPipeline::open`] always fails
//! and the engine uses its CPU pipeline; the API is identical so callers need
//! no `cfg`.

use std::time::Duration;

pub use vv_codec::EncodedFrame;

/// Which H.264 encoder the GPU pipeline should use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EncoderChoice {
    /// Hardware encoder if one works, otherwise OpenH264.
    #[default]
    Auto,
    /// Hardware encoder or fail (useful for testing).
    Hardware,
    /// OpenH264 on the GPU-scaled frames.
    Software,
}

impl std::str::FromStr for EncoderChoice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "hardware" | "hw" => Ok(Self::Hardware),
            "software" | "sw" => Ok(Self::Software),
            other => Err(format!("unknown encoder '{other}' (auto | hardware | software)")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PipelineInfo {
    /// GPU the captured display is attached to.
    pub adapter: String,
    /// Encoder in use (hardware MFT friendly name, or "openh264").
    pub encoder: String,
    pub hardware_encoder: bool,
    pub source: (u32, u32),
    pub encoded: (u32, u32),
}

/// Time spent in each stage of the last [`GpuPipeline::poll`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuTimings {
    /// Waiting for a desktop change + queueing copy/scale on the GPU.
    pub capture: Duration,
    /// NV12 read-back to the CPU (software encoder only).
    pub readback: Duration,
    pub encode: Duration,
}

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::GpuPipeline;

#[cfg(not(windows))]
mod stub {
    use super::*;
    use anyhow::{Result, bail};
    use vv_codec::EncoderSettings;

    /// Placeholder: the GPU pipeline only exists on Windows.
    pub struct GpuPipeline(std::convert::Infallible);

    impl GpuPipeline {
        pub fn open(_display: Option<usize>, _settings: EncoderSettings, _choice: EncoderChoice) -> Result<Self> {
            bail!("the GPU pipeline is only available on Windows")
        }

        pub fn info(&self) -> &PipelineInfo {
            match self.0 {}
        }

        pub fn last_timings(&self) -> GpuTimings {
            match self.0 {}
        }

        pub fn request_keyframe(&mut self) {
            match self.0 {}
        }

        pub fn poll(&mut self, _timeout: Duration, _force: bool) -> Result<Vec<EncodedFrame>> {
            match self.0 {}
        }
    }
}
#[cfg(not(windows))]
pub use stub::GpuPipeline;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_choice_parses() {
        assert_eq!("auto".parse::<EncoderChoice>(), Ok(EncoderChoice::Auto));
        assert_eq!("HW".parse::<EncoderChoice>(), Ok(EncoderChoice::Hardware));
        assert_eq!("software".parse::<EncoderChoice>(), Ok(EncoderChoice::Software));
        assert!("nvenc".parse::<EncoderChoice>().is_err());
    }
}

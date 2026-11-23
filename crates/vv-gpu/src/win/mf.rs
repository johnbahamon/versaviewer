//! Hardware H.264 encoding through Media Foundation.
//!
//! Media Foundation exposes each GPU vendor's encoder (Intel Quick Sync,
//! NVIDIA NVENC, AMD AMF/VCN) as a hardware MFT. Hardware MFTs are always
//! *asynchronous*: they announce `METransformNeedInput` / `METransformHaveOutput`
//! events and we feed / drain them accordingly. Input surfaces are NV12
//! textures on our own D3D11 device (shared through an `IMFDXGIDeviceManager`),
//! so frames never leave the GPU until they are compressed.
//!
//! Configuration for remote desktop: Baseline profile (what the viewer's
//! decoder and the WebRTC SDP expect), no B-frames, CBR, low-latency mode.

use anyhow::{Context, Result, anyhow, bail};
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use vv_codec::{EncodedFrame, EncoderSettings};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer,
    IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
    eAVEncH264VProfile_ConstrainedBase,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

/// How long to wait for the encoder to accept input / produce output before
/// giving up on a frame (the caller retries it as a "refresh").
const STEP_TIMEOUT: Duration = Duration::from_millis(150);

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: writing the active members of a zero-initialised VARIANT.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_true() -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: as above.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_TRUE;
    }
    var
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

fn friendly_name(activate: &IMFActivate) -> String {
    // SAFETY: buffer sized from GetStringLength (+1 for the terminator).
    unsafe {
        let Ok(len) = activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) else {
            return "unknown hardware encoder".into();
        };
        let mut buf = vec![0u16; len as usize + 1];
        if activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None).is_err() {
            return "unknown hardware encoder".into();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// Hardware H.264 encoder MFTs registered on this machine, best first.
fn enumerate() -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates `array` with CoTaskMemAlloc; we move every
    // element out (leaving None) and then free the array itself.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut array,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let list = (0..count as usize).filter_map(|i| (*array.add(i)).take()).collect();
        if !array.is_null() {
            CoTaskMemFree(Some(array as *const _));
        }
        Ok(list)
    }
}

pub struct MfEncoder {
    name: String,
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    /// Input requests announced by the MFT and not yet satisfied.
    need_input: u32,
    provides_samples: bool,
    output_buffer_size: u32,
    sequence_header: Option<Vec<u8>>,
    force_keyframe: bool,
    started: Instant,
    frame_duration: i64,
    pending: Vec<EncodedFrame>,
}

impl MfEncoder {
    /// Tries every hardware H.264 encoder until one accepts our device.
    pub fn open(device: &ID3D11Device, width: u32, height: u32, settings: &EncoderSettings) -> Result<Self> {
        // MF calls into the device from its own worker threads.
        let mt: ID3D11Multithread = device.cast().context("ID3D11Multithread")?;
        // SAFETY: valid interface.
        let _previous = unsafe { mt.SetMultithreadProtected(true) };

        let candidates = enumerate()?;
        if candidates.is_empty() {
            bail!("no hardware H.264 encoder found");
        }
        let mut errors = Vec::new();
        for activate in candidates {
            let name = friendly_name(&activate);
            match Self::try_open(activate.clone(), &name, device, width, height, settings) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::info!(encoder = %name, "hardware encoder rejected: {e:#}");
                    // SAFETY: valid activation object.
                    let _ = unsafe { activate.ShutdownObject() };
                    errors.push(format!("{name}: {e:#}"));
                }
            }
        }
        bail!("no usable hardware encoder ({})", errors.join("; "))
    }

    fn try_open(
        activate: IMFActivate,
        name: &str,
        device: &ID3D11Device,
        width: u32,
        height: u32,
        settings: &EncoderSettings,
    ) -> Result<Self> {
        // SAFETY: COM calls on freshly created, valid objects; every result is checked.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;
            let attributes = transform.GetAttributes().context("GetAttributes")?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                bail!("not an asynchronous MFT");
            }
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("ASYNC_UNLOCK")?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut reset_token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager).context("MFCreateDXGIDeviceManager")?;
            let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
            manager.ResetDevice(device, reset_token).context("ResetDevice")?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("SET_D3D_MANAGER (encoder may belong to another GPU)")?;

            // Rate control must be configured before the media types on some encoders.
            let codec_api = transform.cast::<ICodecAPI>().ok();
            if let Some(api) = &codec_api {
                let set = |key: &GUID, v: VARIANT, what: &str| {
                    if let Err(e) = api.SetValue(key, &v) {
                        tracing::debug!(encoder = name, "{what} not supported: {e}");
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, variant_true(), "low latency");
                set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(settings.bitrate_bps), "bitrate");
                set(&CODECAPI_AVEncMPVGOPSize, variant_u32(settings.keyframe_interval), "GOP size");
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "B-frames off");
            }

            let fps = settings.max_fps.round().max(1.0) as u32;
            let mut profile_error = None;
            let mut output_set = false;
            for profile in [eAVEncH264VProfile_Base.0, eAVEncH264VProfile_ConstrainedBase.0] {
                let out = MFCreateMediaType()?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                out.SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_bps)?;
                out.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
                out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
                out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
                out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
                match transform.SetOutputType(0, &out, 0) {
                    Ok(()) => {
                        output_set = true;
                        break;
                    }
                    Err(e) => profile_error = Some(e),
                }
            }
            if !output_set {
                bail!("baseline profile not supported: {:?}", profile_error);
            }

            let input: IMFMediaType = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0).context("SetInputType(NV12)")?;

            let info = transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            let events: IMFMediaEventGenerator = transform.cast().context("IMFMediaEventGenerator")?;

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let mut enc = Self {
                name: name.to_owned(),
                activate,
                transform,
                events,
                codec_api,
                _manager: manager,
                need_input: 0,
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                output_buffer_size: info.cbSize.max(1 << 20),
                sequence_header: None,
                force_keyframe: true,
                started: Instant::now(),
                frame_duration: 10_000_000 / i64::from(fps),
                pending: Vec::new(),
            };
            enc.refresh_sequence_header();
            Ok(enc)
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn refresh_sequence_header(&mut self) {
        // SAFETY: buffer sized from GetBlobSize.
        self.sequence_header = unsafe {
            self.transform.GetOutputCurrentType(0).ok().and_then(|t| {
                let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
                let mut buf = vec![0u8; size as usize];
                t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).ok()?;
                (!buf.is_empty()).then_some(buf)
            })
        };
    }

    /// Handles every queued MFT event without blocking.
    fn pump(&mut self) -> Result<()> {
        loop {
            // SAFETY: valid event generator.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).context("IMFMediaEventGenerator::GetEvent"),
            };
            // SAFETY: valid event.
            let (kind, status) = unsafe { (event.GetType()?, event.GetStatus()?) };
            status.ok().context("encoder reported an error event")?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.drain_output()?;
            }
        }
    }

    fn drain_output(&mut self) -> Result<()> {
        // SAFETY: the output buffer struct owns at most one sample and one
        // event collection; both are moved out of their ManuallyDrop wrappers
        // after the call so they are released exactly once.
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.output_buffer_size)?)?;
                Some(s)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder changed its output format (e.g. updated SPS):
                    // accept its proposal and continue.
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    self.refresh_sequence_header();
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            if let Some(sample) = sample {
                let frame = self.read_sample(&sample)?;
                if !frame.data.is_empty() {
                    self.pending.push(frame);
                }
            }
        }
        Ok(())
    }

    fn read_sample(&mut self, sample: &IMFSample) -> Result<EncodedFrame> {
        // SAFETY: Lock/Unlock are paired; we copy exactly `len` bytes.
        let mut data = unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            data
        };
        // SAFETY: valid sample attribute store.
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0
            || annexb_nal_types(&data).any(|t| t == 5);
        // Some encoders only put SPS/PPS in the media type, not in-band. The
        // viewer needs them before every IDR to (re)start decoding.
        if keyframe && !annexb_nal_types(&data).any(|t| t == 7)
            && let Some(header) = &self.sequence_header {
                let mut with_header = header.clone();
                with_header.extend_from_slice(&data);
                data = with_header;
            }
        Ok(EncodedFrame { data, keyframe })
    }

    /// Encodes one NV12 surface. Returns the access units the encoder emitted
    /// (normally exactly one; empty if it is still busy — the caller should
    /// submit the frame again later).
    pub fn encode(&mut self, surface: &ID3D11Texture2D) -> Result<Vec<EncodedFrame>> {
        // 1. Wait until the encoder asks for input.
        let deadline = Instant::now() + STEP_TIMEOUT;
        self.pump()?;
        while self.need_input == 0 {
            if Instant::now() > deadline {
                return Ok(std::mem::take(&mut self.pending));
            }
            std::thread::sleep(Duration::from_millis(1));
            self.pump()?;
        }

        // 2. Wrap the texture in a sample and submit it.
        // SAFETY: COM calls on valid objects.
        unsafe {
            if std::mem::take(&mut self.force_keyframe)
                && let Some(api) = &self.codec_api
            {
                let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, surface, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let now = (self.started.elapsed().as_nanos() / 100) as i64;
            sample.SetSampleTime(now)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")?;
        }
        self.need_input -= 1;

        // 3. Wait for the compressed frame.
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            self.pump()?;
            if !self.pending.is_empty() || Instant::now() > deadline {
                return Ok(std::mem::take(&mut self.pending));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: best-effort shutdown of valid objects.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// NAL unit types present in an Annex-B buffer.
fn annexb_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let t = data[i + 3] & 0x1F;
                i += 3;
                return Some(t);
            }
            i += 1;
        }
        None
    })
}

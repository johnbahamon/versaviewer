//! Agent (controlled side) session engine.
//!
//! Security model (Phase 0):
//! * The agent generates its own device ID and a one-time password (OTP)
//!   from the OS CSPRNG. The signaling server relays but never stores it.
//! * Password checks are constant-time and rate limited with exponential
//!   lockout (brute-force protection).
//! * [`ConsentPolicy::Prompt`] (default) asks the local user to accept every
//!   session; the OTP rotates after each session.
//! * [`ConsentPolicy::Unattended`] skips the prompt but requires a
//!   fixed password configured by the machine's owner.
//! * Only one session at a time. While it is active the app is told via
//!   [`AgentEvent::SessionStarted`] so it can show a visible indicator.
//! * Media and input are end-to-end encrypted by WebRTC (DTLS-SRTP/SCTP).

use crate::credentials::{self, AttemptLimiter};
use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use vv_codec::{EncodedFrame, EncoderSettings, H264Encoder};
use vv_protocol::PROTOCOL_VERSION;
use vv_protocol::control::ControlMessage;
use vv_protocol::signal::{ClientMessage, ServerMessage};
use vv_transport::{
    ConnectionState, Peer, PeerEvent, Role, SignalingClient, SignalingSender, TransportConfig,
};

#[derive(Debug, Clone)]
pub enum ConsentPolicy {
    /// Ask the local user for every incoming session (assisted support).
    Prompt,
    /// Accept sessions that present this fixed password (unattended access).
    Unattended { password: String },
}

/// Information shown to the local user when asking for consent.
#[derive(Debug, Clone)]
pub struct ConsentRequest {
    pub session_id: String,
}

/// Asks the local user whether to accept a session. Blocking; called on a
/// dedicated thread.
pub trait ConsentPrompt: Send + Sync + 'static {
    fn ask(&self, request: &ConsentRequest) -> bool;
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Registered with the server; show these credentials to the user.
    Ready { device_id: String, password: String },
    /// A remote session is now active (show a visible indicator!).
    SessionStarted { session_id: String },
    SessionEnded { session_id: String, reason: String },
    /// The one-time password changed (after each session in Prompt mode).
    PasswordChanged { password: String },
    Rejected { reason: String },
}

pub struct AgentConfig {
    pub signaling_url: String,
    pub device_id: Option<String>,
    pub transport: TransportConfig,
    pub encoder: EncoderSettings,
    pub display: Option<usize>,
    pub consent: ConsentPolicy,
    pub video: VideoOptions,
}

type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

enum Phase {
    Idle,
    /// Waiting for the local user's decision.
    AwaitingConsent { session_id: String },
    /// Accepted; waiting for the viewer's SDP offer.
    AwaitingOffer { session_id: String },
    Active { session_id: String, stop: watch::Sender<bool> },
}

impl Phase {
    fn session_id(&self) -> Option<&str> {
        match self {
            Phase::Idle => None,
            Phase::AwaitingConsent { session_id }
            | Phase::AwaitingOffer { session_id }
            | Phase::Active { session_id, .. } => Some(session_id),
        }
    }
}

/// Runs the agent until the signaling connection closes.
pub async fn run_agent(
    config: AgentConfig,
    prompt: Arc<dyn ConsentPrompt>,
    on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
) -> Result<()> {
    let on_event: EventSink = Arc::new(on_event);
    if let ConsentPolicy::Unattended { password } = &config.consent {
        anyhow::ensure!(
            password.chars().count() >= 8,
            "unattended password must have at least 8 characters"
        );
    }
    let device_id = match &config.device_id {
        Some(id) => id.clone(),
        None => credentials::generate_device_id()?,
    };
    let mut password = match &config.consent {
        ConsentPolicy::Prompt => credentials::generate_password()?,
        ConsentPolicy::Unattended { password } => password.clone(),
    };

    let mut signaling = SignalingClient::connect(&config.signaling_url).await?;
    signaling.send(ClientMessage::Register {
        device_id: device_id.clone(),
        protocol: PROTOCOL_VERSION,
    })?;

    let (consent_tx, mut consent_rx) = mpsc::unbounded_channel::<(String, bool)>();
    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel::<(String, String)>();
    let mut limiter = AttemptLimiter::default();
    let mut phase = Phase::Idle;
    let config = Arc::new(config);

    loop {
        tokio::select! {
            msg = signaling.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    ServerMessage::Registered { device_id } => {
                        on_event(AgentEvent::Ready { device_id, password: password.clone() });
                    }
                    ServerMessage::ConnectRequest { session_id, password: offered } => {
                        let now = Instant::now();
                        let reject = |reason: &str| ClientMessage::Reject {
                            session_id: session_id.clone(),
                            reason: reason.to_owned(),
                        };
                        if !matches!(phase, Phase::Idle) {
                            signaling.send(reject("busy"))?;
                        } else if let Some(wait) = limiter.locked(now) {
                            signaling.send(reject(&format!(
                                "too many failed attempts, retry in {}s", wait.as_secs() + 1
                            )))?;
                        } else if !credentials::constant_time_eq(&offered, &password) {
                            limiter.record_failure(now);
                            tracing::warn!(%session_id, "connection attempt with wrong password");
                            on_event(AgentEvent::Rejected { reason: "wrong password".into() });
                            signaling.send(reject("invalid credentials"))?;
                        } else {
                            limiter.record_success();
                            match &config.consent {
                                ConsentPolicy::Unattended { .. } => {
                                    signaling.send(ClientMessage::Accept { session_id: session_id.clone() })?;
                                    phase = Phase::AwaitingOffer { session_id };
                                }
                                ConsentPolicy::Prompt => {
                                    let prompt = prompt.clone();
                                    let tx = consent_tx.clone();
                                    let request = ConsentRequest { session_id: session_id.clone() };
                                    tokio::task::spawn_blocking(move || {
                                        let accepted = prompt.ask(&request);
                                        let _ = tx.send((request.session_id, accepted));
                                    });
                                    phase = Phase::AwaitingConsent { session_id };
                                }
                            }
                        }
                    }
                    ServerMessage::Sdp { session_id, sdp } => {
                        let Phase::AwaitingOffer { session_id: expected } = &phase else {
                            tracing::warn!("unexpected SDP ignored");
                            continue;
                        };
                        if *expected != session_id {
                            tracing::warn!("SDP for unknown session ignored");
                            continue;
                        }
                        let (stop_tx, stop_rx) = watch::channel(false);
                        let sender = signaling.sender();
                        let ended = ended_tx.clone();
                        let cfg = config.clone();
                        let id = session_id.clone();
                        tokio::spawn(async move {
                            let reason = match run_session(&cfg, &id, &sdp, &sender, stop_rx).await {
                                Ok(reason) => reason,
                                Err(e) => format!("error: {e:#}"),
                            };
                            let _ = sender.send(ClientMessage::Hangup { session_id: id.clone() });
                            let _ = ended.send((id, reason));
                        });
                        on_event(AgentEvent::SessionStarted { session_id: session_id.clone() });
                        phase = Phase::Active { session_id, stop: stop_tx };
                    }
                    ServerMessage::Hangup { session_id } => {
                        if phase.session_id() == Some(session_id.as_str()) {
                            match &phase {
                                Phase::Active { stop, .. } => { let _ = stop.send(true); }
                                _ => phase = Phase::Idle,
                            }
                        }
                    }
                    ServerMessage::Error { message } => tracing::warn!("server error: {message}"),
                    other => tracing::debug!("ignored server message: {other:?}"),
                }
            }
            Some((session_id, accepted)) = consent_rx.recv() => {
                if !matches!(&phase, Phase::AwaitingConsent { session_id: s } if *s == session_id) {
                    continue; // viewer gave up meanwhile
                }
                if accepted {
                    signaling.send(ClientMessage::Accept { session_id: session_id.clone() })?;
                    phase = Phase::AwaitingOffer { session_id };
                } else {
                    signaling.send(ClientMessage::Reject {
                        session_id,
                        reason: "rejected by the remote user".into(),
                    })?;
                    phase = Phase::Idle;
                }
            }
            Some((session_id, reason)) = ended_rx.recv() => {
                on_event(AgentEvent::SessionEnded { session_id, reason });
                phase = Phase::Idle;
                if matches!(config.consent, ConsentPolicy::Prompt) {
                    password = credentials::generate_password()?;
                    on_event(AgentEvent::PasswordChanged { password: password.clone() });
                }
            }
        }
    }

    if let Phase::Active { stop, .. } = &phase {
        let _ = stop.send(true);
    }
    Err(anyhow!("signaling connection lost"))
}

/// Runs one accepted session to completion. Returns the end reason.
async fn run_session(
    config: &AgentConfig,
    session_id: &str,
    offer: &str,
    signaling: &SignalingSender,
    mut stop: watch::Receiver<bool>,
) -> Result<String> {
    let (mut peer, mut events) = Peer::new(Role::Agent, &config.transport).await?;
    let answer = peer.answer_offer(offer).await?;
    signaling.send(ClientMessage::Sdp { session_id: session_id.to_owned(), sdp: answer })?;

    let stop_capture = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(true));
    // Video is only encoded/sent once DTLS-SRTP is up; before that every RTP
    // packet would be dropped by the stack (and logged).
    let connected = Arc::new(AtomicBool::new(false));
    let (frames_tx, mut frames_rx) = mpsc::channel::<(EncodedFrame, Duration)>(2);
    let (dims_tx, dims_rx) = tokio::sync::oneshot::channel();
    let capture = spawn_video(
        config.display,
        config.encoder,
        config.video,
        frames_tx,
        keyframe.clone(),
        connected.clone(),
        stop_capture.clone(),
        dims_tx,
    );
    let (width, height) = dims_rx.await.context("capture thread died")??;

    let (input_tx, input_thread) = spawn_input(config.display)?;

    let reason = loop {
        tokio::select! {
            _ = stop.changed() => break "ended by viewer".to_string(),
            frame = frames_rx.recv() => {
                let Some((frame, duration)) = frame else {
                    break "screen capture stopped".to_string();
                };
                if !connected.load(Ordering::Relaxed) {
                    continue; // encoded before the handshake finished; IDR follows on connect
                }
                if let Err(e) = peer.send_video_frame(frame.data, duration).await {
                    tracing::debug!("video send failed: {e}");
                }
            }
            evt = events.recv() => {
                let Some(evt) = evt else { break "connection closed".to_string() };
                match evt {
                    PeerEvent::State(ConnectionState::Connected) => {
                        tracing::info!("session {session_id}: peer connected");
                        keyframe.store(true, Ordering::Relaxed);
                        connected.store(true, Ordering::Relaxed);
                    }
                    PeerEvent::State(s @ (ConnectionState::Failed | ConnectionState::Closed)) => {
                        break format!("connection {s}");
                    }
                    PeerEvent::ControlOpen => {
                        let _ = peer.send_control(&ControlMessage::ScreenInfo { width, height }).await;
                    }
                    PeerEvent::ControlClosed => break "control channel closed".to_string(),
                    PeerEvent::Control(ControlMessage::RequestKeyframe) => {
                        keyframe.store(true, Ordering::Relaxed);
                    }
                    PeerEvent::Control(ControlMessage::Bye) => break "ended by viewer".to_string(),
                    PeerEvent::Control(msg) => {
                        if input_tx.send(msg).is_err() {
                            break "input thread stopped".to_string();
                        }
                    }
                    _ => {}
                }
            }
        }
    };

    stop_capture.store(true, Ordering::Relaxed);
    drop(frames_rx);
    drop(input_tx); // input thread releases held keys and exits
    peer.close().await;
    let _ = tokio::task::spawn_blocking(move || {
        let _ = capture.join();
        let _ = input_thread.join();
    })
    .await;
    Ok(reason)
}

/// Latest-frame mailbox between the capture and encode threads. Holding at
/// most one frame means the encoder always works on the freshest screen
/// state; a frame it had no time for is simply replaced (raw frames can be
/// dropped freely — only *encoded* frames form a reference chain).
struct Mailbox {
    slot: Mutex<Option<vv_capture::Frame>>,
    ready: Condvar,
}

impl Mailbox {
    fn put(&self, frame: vv_capture::Frame) -> Option<vv_capture::Frame> {
        let old = self.slot.lock().expect("mailbox poisoned").replace(frame);
        self.ready.notify_one();
        old
    }

    fn take(&self, timeout: Duration) -> Option<vv_capture::Frame> {
        let guard = self.slot.lock().expect("mailbox poisoned");
        let (mut guard, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |s| s.is_none())
            .expect("mailbox poisoned");
        guard.take()
    }
}

/// Capture-side counters read by the encoder thread's stats.
#[derive(Default)]
struct CaptureCounters {
    busy_us: AtomicU64,
    frames: AtomicU64,
}

/// Where the session's encoded video comes from.
#[derive(Debug, Clone, Copy)]
pub struct VideoOptions {
    /// Try the GPU pipeline first (Windows: GPU scaling + hardware encoder).
    pub gpu: bool,
    pub encoder: vv_gpu::EncoderChoice,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self { gpu: true, encoder: vv_gpu::EncoderChoice::Auto }
    }
}

/// Starts the video thread: the GPU pipeline when available, otherwise (or
/// if it fails mid-session) the CPU pipeline.
#[allow(clippy::too_many_arguments)]
fn spawn_video(
    display: Option<usize>,
    settings: EncoderSettings,
    options: VideoOptions,
    frames: mpsc::Sender<(EncodedFrame, Duration)>,
    keyframe: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    dims: tokio::sync::oneshot::Sender<Result<(u32, u32)>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("vv-video".into())
        .spawn(move || {
            let mut dims = Some(dims);
            if options.gpu {
                match vv_gpu::GpuPipeline::open(display, settings, options.encoder) {
                    Ok(pipeline) => {
                        let info = pipeline.info().clone();
                        tracing::info!(
                            backend = "gpu",
                            adapter = %info.adapter,
                            encoder = %info.encoder,
                            source = format!("{}x{}", info.source.0, info.source.1),
                            encoded = format!("{}x{}", info.encoded.0, info.encoded.1),
                            "video pipeline ready"
                        );
                        if let Some(d) = dims.take() {
                            let _ = d.send(Ok(info.encoded));
                        }
                        match run_gpu_pipeline(pipeline, settings, &frames, &keyframe, &connected, &stop) {
                            Ok(()) => return,
                            Err(e) => tracing::warn!("GPU video pipeline failed, switching to CPU: {e:#}"),
                        }
                        keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(e) => tracing::info!("GPU video pipeline unavailable, using CPU: {e:#}"),
                }
            }
            run_cpu_pipeline(display, settings, frames, keyframe, connected, stop, dims);
        })
        .expect("spawning video thread")
}

/// GPU pipeline loop. Returns `Ok` when the session ends, `Err` if the
/// pipeline broke (the caller falls back to the CPU pipeline).
fn run_gpu_pipeline(
    mut pipeline: vv_gpu::GpuPipeline,
    settings: EncoderSettings,
    frames: &mpsc::Sender<(EncodedFrame, Duration)>,
    keyframe: &AtomicBool,
    connected: &AtomicBool,
    stop: &AtomicBool,
) -> Result<()> {
    let interval = Duration::from_secs_f32(1.0 / settings.max_fps.max(1.0));
    let mut last_encode = Instant::now() - interval;
    let mut failures = 0u32;
    let mut stats = GpuStats::new();
    while !stop.load(Ordering::Relaxed) {
        if let Some(rest) = interval.checked_sub(last_encode.elapsed()) {
            std::thread::sleep(rest);
        }
        if !connected.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let want_keyframe = keyframe.swap(false, Ordering::Relaxed);
        if want_keyframe {
            pipeline.request_keyframe();
        }
        let started = Instant::now();
        let produced = match pipeline.poll(Duration::from_millis(100), want_keyframe) {
            Ok(p) => {
                failures = 0;
                p
            }
            Err(e) => {
                failures += 1;
                tracing::warn!("GPU pipeline error ({failures}/3): {e:#}");
                if failures >= 3 {
                    return Err(e);
                }
                keyframe.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if produced.is_empty() {
            // No change, or the encoder had nothing ready yet (the pipeline
            // retries pending pictures by itself on the next poll).
            continue;
        }
        let duration = started - last_encode;
        last_encode = started;
        stats.add(&pipeline.last_timings(), &produced);
        for frame in produced {
            if frames.blocking_send((frame, duration)).is_err() {
                return Ok(());
            }
        }
        stats.maybe_log(&pipeline);
    }
    Ok(())
}

struct GpuStats {
    since: Instant,
    frames: u32,
    bytes: usize,
    capture: Duration,
    readback: Duration,
    encode: Duration,
}

impl GpuStats {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            bytes: 0,
            capture: Duration::ZERO,
            readback: Duration::ZERO,
            encode: Duration::ZERO,
        }
    }

    fn add(&mut self, t: &vv_gpu::GpuTimings, frames: &[EncodedFrame]) {
        self.frames += frames.len() as u32;
        self.bytes += frames.iter().map(|f| f.data.len()).sum::<usize>();
        self.capture += t.capture;
        self.readback += t.readback;
        self.encode += t.encode;
    }

    fn maybe_log(&mut self, pipeline: &vv_gpu::GpuPipeline) {
        let secs = self.since.elapsed().as_secs_f32();
        if secs < 5.0 {
            return;
        }
        let per = |d: Duration| d.as_secs_f32() * 1000.0 / self.frames.max(1) as f32;
        let info = pipeline.info();
        tracing::info!(
            fps = format!("{:.1}", self.frames as f32 / secs),
            kbps = format!("{:.0}", self.bytes as f32 * 8.0 / 1000.0 / secs),
            capture_ms = format!("{:.1}", per(self.capture)),
            readback_ms = format!("{:.1}", per(self.readback)),
            encode_ms = format!("{:.1}", per(self.encode)),
            res = format!("{}x{}->{}x{}", info.source.0, info.source.1, info.encoded.0, info.encoded.1),
            encoder = %info.encoder,
            "stream stats (gpu)"
        );
        *self = Self::new();
    }
}

/// CPU pipeline: a capture thread and an encode thread (this one) running in
/// parallel, so frame N+1 is captured while frame N is being encoded.
///
/// Change-driven: with DXGI nothing is encoded while the screen is static.
/// The encoder keeps the last frame to answer keyframe requests and to
/// re-send the final state if rate control skipped it.
fn run_cpu_pipeline(
    display: Option<usize>,
    settings: EncoderSettings,
    frames: mpsc::Sender<(EncodedFrame, Duration)>,
    keyframe: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    dims: Option<tokio::sync::oneshot::Sender<Result<(u32, u32)>>>,
) {
    let interval = Duration::from_secs_f32(1.0 / settings.max_fps.max(1.0));
    let mailbox = Arc::new(Mailbox { slot: Mutex::new(None), ready: Condvar::new() });
    let counters = Arc::new(CaptureCounters::default());
    let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<&'static str>>();

    let capture_thread = {
        let (mailbox, counters, stop) = (mailbox.clone(), counters.clone(), stop.clone());
        std::thread::Builder::new()
            .name("vv-capture".into())
            .spawn(move || {
                capture_loop(display, interval, &mailbox, &counters, &recycle_rx, &stop, init_tx)
            })
            .expect("spawning capture thread")
    };

    let init = (|| -> Result<_> {
        let backend = init_rx.recv().map_err(|_| anyhow!("capture thread died"))??;
        let first = mailbox
            .take(Duration::from_secs(3))
            .ok_or_else(|| anyhow!("no initial frame from capturer"))?;
        let encoder = H264Encoder::new(first.width, first.height, settings)?;
        Ok((backend, encoder, first))
    })();
    let (backend, mut encoder, mut last) = match init {
        Ok(v) => {
            if let Some(dims) = dims {
                let _ = dims.send(Ok(v.1.dimensions()));
            }
            v
        }
        Err(e) => {
            match dims {
                Some(dims) => {
                    let _ = dims.send(Err(e));
                }
                None => tracing::error!("CPU video pipeline failed: {e:#}"),
            }
            stop.store(true, Ordering::Relaxed);
            let _ = capture_thread.join();
            return;
        }
    };
    tracing::info!(
        backend,
        source = format!("{}x{}", last.width, last.height),
        encoded = format!("{}x{}", encoder.dimensions().0, encoder.dimensions().1),
        "video pipeline ready"
    );

    let mut last_encode = Instant::now() - interval;
    let mut fresh = true; // `last` has not been sent yet
    let mut needs_refresh = false; // last encode was skipped by rate control
    let mut stats = Stats::new();
    while !stop.load(Ordering::Relaxed) {
        if let Some(rest) = interval.checked_sub(last_encode.elapsed()) {
            std::thread::sleep(rest);
        }
        let want_keyframe = keyframe.swap(false, Ordering::Relaxed);
        match mailbox.take(Duration::from_millis(100)) {
            Some(f) => {
                let old = std::mem::replace(&mut last, f);
                let _ = recycle_tx.send(old.data);
                fresh = true;
            }
            None if !(want_keyframe || needs_refresh || fresh) => continue,
            None => {}
        }
        if !connected.load(Ordering::Relaxed) {
            // Nothing can be sent yet: keep the newest frame and the
            // keyframe request for when the connection is up.
            if want_keyframe {
                keyframe.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        if encoder.source_dimensions() != (last.width, last.height) {
            match H264Encoder::new(last.width, last.height, settings) {
                Ok(e) => {
                    tracing::info!(
                        source = format!("{}x{}", last.width, last.height),
                        "display resolution changed; encoder recreated"
                    );
                    encoder = e;
                }
                Err(e) => {
                    tracing::error!("recreating encoder: {e:#}");
                    break;
                }
            }
        }
        if want_keyframe {
            encoder.request_keyframe();
        }
        let image = vv_codec::RawImage {
            data: &last.data,
            width: last.width,
            height: last.height,
            stride: last.stride,
            order: match last.format {
                vv_capture::PixelFormat::Rgba8 => vv_codec::PixelOrder::Rgba,
                vv_capture::PixelFormat::Bgra8 => vv_codec::PixelOrder::Bgra,
            },
        };
        let started = Instant::now();
        let encoded = encoder.encode(&image);
        let duration = started - last_encode;
        last_encode = started;
        fresh = false;
        let t = encoder.last_timings();
        stats.convert += t.convert;
        stats.encode += t.encode;
        match encoded {
            Ok(Some(encoded)) => {
                needs_refresh = false;
                stats.frames += 1;
                stats.bytes += encoded.data.len();
                // Blocking send = backpressure. Dropping encoded frames
                // would break the H.264 reference chain.
                if frames.blocking_send((encoded, duration)).is_err() {
                    break;
                }
            }
            Ok(None) => needs_refresh = true,
            Err(e) => tracing::warn!("encode failed: {e:#}"),
        }
        stats.maybe_log(backend, &encoder, &counters);
    }
    stop.store(true, Ordering::Relaxed);
    let _ = capture_thread.join();
    tracing::info!("video pipeline stopped");
}

fn capture_loop(
    display: Option<usize>,
    interval: Duration,
    mailbox: &Mailbox,
    counters: &CaptureCounters,
    recycle: &std::sync::mpsc::Receiver<Vec<u8>>,
    stop: &AtomicBool,
    init: std::sync::mpsc::Sender<Result<&'static str>>,
) {
    let mut capturer = match vv_capture::open_capturer(display) {
        Ok(c) => {
            let _ = init.send(Ok(c.backend()));
            c
        }
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let mut last_capture = Instant::now() - interval;
    while !stop.load(Ordering::Relaxed) {
        // Same pacing as the encoder: polling backends would otherwise spin,
        // and DXGI simply accumulates changes while we wait.
        if let Some(rest) = interval.checked_sub(last_capture.elapsed()) {
            std::thread::sleep(rest);
        }
        while let Ok(buffer) = recycle.try_recv() {
            capturer.recycle(buffer);
        }
        let started = Instant::now();
        match capturer.next_frame(Duration::from_millis(100)) {
            Ok(Some(frame)) => {
                last_capture = started;
                counters.busy_us.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                counters.frames.fetch_add(1, Ordering::Relaxed);
                if let Some(dropped) = mailbox.put(frame) {
                    capturer.recycle(dropped.data);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("capture failed: {e:#}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Per-stage timing, logged every few seconds to find bottlenecks.
struct Stats {
    since: Instant,
    frames: u32,
    bytes: usize,
    convert: Duration,
    encode: Duration,
}

impl Stats {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            bytes: 0,
            convert: Duration::ZERO,
            encode: Duration::ZERO,
        }
    }

    fn maybe_log(&mut self, backend: &str, encoder: &H264Encoder, capture: &CaptureCounters) {
        let secs = self.since.elapsed().as_secs_f32();
        if secs < 5.0 {
            return;
        }
        let per = |d: Duration| {
            if self.frames == 0 { 0.0 } else { d.as_secs_f32() * 1000.0 / self.frames as f32 }
        };
        let (sw, sh) = encoder.source_dimensions();
        let (ew, eh) = encoder.dimensions();
        let cap_frames = capture.frames.swap(0, Ordering::Relaxed);
        let cap_us = capture.busy_us.swap(0, Ordering::Relaxed);
        tracing::info!(
            fps = format!("{:.1}", self.frames as f32 / secs),
            kbps = format!("{:.0}", self.bytes as f32 * 8.0 / 1000.0 / secs),
            captured_fps = format!("{:.1}", cap_frames as f32 / secs),
            capture_ms = format!("{:.1}", cap_us as f32 / 1000.0 / cap_frames.max(1) as f32),
            convert_ms = format!("{:.1}", per(self.convert)),
            encode_ms = format!("{:.1}", per(self.encode)),
            res = format!("{sw}x{sh}->{ew}x{eh}"),
            backend,
            "stream stats"
        );
        *self = Self::new();
    }
}

/// Input injection on a dedicated thread (injectors are thread-affine).
fn spawn_input(
    display: Option<usize>,
) -> Result<(std::sync::mpsc::Sender<ControlMessage>, std::thread::JoinHandle<()>)> {
    let displays = vv_capture::list_displays()?;
    let info = match display {
        Some(i) => displays.get(i).cloned(),
        None => displays.iter().find(|d| d.primary).or(displays.first()).cloned(),
    }
    .ok_or_else(|| anyhow!("no display to control"))?;
    let bounds = vv_input::DisplayBounds {
        x: info.x,
        y: info.y,
        width: info.width,
        height: info.height,
    };
    let (tx, rx) = std::sync::mpsc::channel::<ControlMessage>();
    let handle = std::thread::Builder::new()
        .name("vv-input".into())
        .spawn(move || {
            let mut injector = match vv_input::native_injector(bounds) {
                Ok(i) => i,
                Err(e) => {
                    // Degrade to view-only instead of ending the session.
                    tracing::error!("input injection unavailable, session is view-only: {e:#}");
                    for _ in rx {}
                    return;
                }
            };
            for msg in rx {
                if let Err(e) = vv_input::apply(injector.as_mut(), &msg) {
                    tracing::warn!("input injection failed: {e:#}");
                }
            }
            let _ = injector.release_all();
        })?;
    Ok((tx, handle))
}

/// Process-wide platform setup (DPI awareness on Windows). Call first thing in `main`.
pub fn platform_init() {
    vv_capture::platform_init();
}

/// Diagnostic report of the video capabilities of this machine (no session needed).
pub fn probe(display: Option<usize>) -> Vec<String> {
    let mut out = Vec::new();
    match vv_capture::list_displays() {
        Ok(ds) => {
            for d in ds {
                out.push(format!(
                    "Display #{}: {} {}x{} at ({},{}){}",
                    d.index, d.name, d.width, d.height, d.x, d.y,
                    if d.primary { " [primary]" } else { "" }
                ));
            }
        }
        Err(e) => out.push(format!("Displays: enumeration failed ({e:#})")),
    }
    out.extend(vv_gpu::diagnose(display));
    out
}

/// Displays available for sharing.
pub fn list_displays() -> Result<Vec<vv_capture::DisplayInfo>> {
    vv_capture::list_displays()
}

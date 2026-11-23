//! VersaViewer viewer (Phase 0): connect to an agent and control it.
//!
//! Deliberately minimal UI (winit + softbuffer, CPU blit). The Flutter app
//! replaces it in a later phase; all session logic lives in `vv-engine`.

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeSet;
use std::io::Write;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vv_engine::viewer::{ViewerConfig, ViewerEvent, ViewerHandle, start_viewer};
use vv_engine::{ControlMessage, DecodedFrame, MouseButton, TransportConfig};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// Default log filter. ICE probes every local interface, so virtual adapters
/// (VirtualBox, WSL/Hyper-V, VMware...) produce "network unreachable" send
/// errors and DTLS/ICE emit setup chatter: all expected and harmless, so they
/// are hidden. Set `RUST_LOG=debug` to see everything.
const DEFAULT_LOG_FILTER: &str =
    "info,rtc=warn,rtc_ice=error,rtc_dtls=error,webrtc=warn,webrtc::peer_connection::driver=off";

#[derive(Parser)]
#[command(name = "vv-viewer", version, about = "VersaViewer viewer: view and control a remote device")]
struct Cli {
    /// Signaling server URL, e.g. ws://192.168.1.20:8080
    #[arg(short, long, env = "VV_SERVER")]
    server: String,
    /// Remote device ID (spaces allowed).
    #[arg(long)]
    id: String,
    /// Password shown on the remote device (asked interactively if omitted).
    #[arg(long, env = "VV_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    #[arg(long, env = "VV_STUN")]
    stun: Option<String>,
    #[arg(long, env = "VV_TURN")]
    turn: Option<String>,
    #[arg(long, env = "VV_TURN_USER")]
    turn_user: Option<String>,
    #[arg(long, env = "VV_TURN_PASS", hide_env_values = true)]
    turn_pass: Option<String>,
    /// Force media through TURN (tests the relay fallback).
    #[arg(long)]
    relay_only: bool,
    /// Send the Mac Command key as Command/Windows key instead of Ctrl.
    #[arg(long)]
    raw_cmd: bool,
}

#[derive(Debug)]
enum UiEvent {
    Engine(ViewerEvent),
}

/// Where the remote frame is drawn inside the window (letterboxing).
#[derive(Clone, Copy, Default)]
struct Viewport {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

struct App {
    handle: ViewerHandle,
    swap_cmd_ctrl: bool,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    frame: Option<DecodedFrame>,
    viewport: Viewport,
    /// Precomputed nearest-neighbour column map for the current scale.
    x_map: Vec<u32>,
    status: String,
    connected: bool,
    pressed_keys: BTreeSet<String>,
    last_mouse: Option<(f32, f32)>,
    closing: Option<Instant>,
    fps: (Instant, u32, f32),
}

impl App {
    fn set_title(&self) {
        if let Some(w) = &self.window {
            let fps = if self.connected { format!(" — {:.0} fps", self.fps.2) } else { String::new() };
            w.set_title(&format!("VersaViewer — {}{fps}", self.status));
        }
    }

    fn compute_viewport(&mut self, win_w: u32, win_h: u32) {
        let Some(f) = &self.frame else { return };
        let scale = (win_w as f32 / f.width as f32).min(win_h as f32 / f.height as f32);
        let w = ((f.width as f32 * scale) as u32).clamp(1, win_w.max(1));
        let h = ((f.height as f32 * scale) as u32).clamp(1, win_h.max(1));
        self.viewport = Viewport { x: (win_w - w) / 2, y: (win_h - h) / 2, w, h };
        self.x_map = (0..w).map(|x| (x as u64 * f.width as u64 / w as u64) as u32).collect();
    }

    fn redraw(&mut self) -> Result<()> {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        surface.resize(w, h).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut buffer = surface.buffer_mut().map_err(|e| anyhow::anyhow!("{e}"))?;
        let (win_w, _win_h) = (size.width as usize, size.height as usize);
        buffer.fill(0x0020_2124);

        if let Some(f) = &self.frame {
            let vp = self.viewport;
            if self.x_map.len() == vp.w as usize {
                for row in 0..vp.h as usize {
                    let src_y = row * f.height as usize / vp.h as usize;
                    let src = &f.xrgb[src_y * f.width as usize..(src_y + 1) * f.width as usize];
                    let dst_start = (vp.y as usize + row) * win_w + vp.x as usize;
                    let dst = &mut buffer[dst_start..dst_start + vp.w as usize];
                    for (d, &sx) in dst.iter_mut().zip(&self.x_map) {
                        *d = src[sx as usize];
                    }
                }
            }
        }
        buffer.present().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Window position -> normalized remote coordinates (clamped to the frame).
    fn normalize(&self, pos: PhysicalPosition<f64>) -> Option<(f32, f32)> {
        let vp = self.viewport;
        if vp.w == 0 || vp.h == 0 {
            return None;
        }
        let x = ((pos.x as f32 - vp.x as f32) / vp.w as f32).clamp(0.0, 1.0);
        let y = ((pos.y as f32 - vp.y as f32) / vp.h as f32).clamp(0.0, 1.0);
        Some((x, y))
    }

    fn key_code_name(&self, code: KeyCode) -> Option<String> {
        let name = match code {
            KeyCode::SuperLeft if self.swap_cmd_ctrl => "ControlLeft".to_owned(),
            KeyCode::SuperRight if self.swap_cmd_ctrl => "ControlRight".to_owned(),
            KeyCode::SuperLeft => "MetaLeft".to_owned(),
            KeyCode::SuperRight => "MetaRight".to_owned(),
            // winit's KeyCode names follow W3C `KeyboardEvent.code` for the rest.
            other => format!("{other:?}"),
        };
        Some(name)
    }

    fn release_all_keys(&mut self) {
        for code in std::mem::take(&mut self.pressed_keys) {
            self.handle.send(ControlMessage::Key { code, down: false });
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("VersaViewer")
            .with_inner_size(LogicalSize::new(1280.0, 760.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("creating window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("surface"));
        self.window = Some(window);
        self.set_title();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        let UiEvent::Engine(event) = event;
        match event {
            ViewerEvent::Status(s) => {
                self.status = s;
                self.set_title();
            }
            ViewerEvent::Connected => {
                self.connected = true;
                self.status = "Conectado".into();
                self.set_title();
            }
            ViewerEvent::ScreenInfo { width, height } => {
                tracing::info!("remote screen {width}x{height}");
            }
            ViewerEvent::Frame(frame) => {
                let dims_changed = self
                    .frame
                    .as_ref()
                    .is_none_or(|f| (f.width, f.height) != (frame.width, frame.height));
                self.frame = Some(frame);
                if dims_changed
                    && let Some(w) = &self.window {
                        let s = w.inner_size();
                        self.compute_viewport(s.width, s.height);
                    }
                self.fps.1 += 1;
                let elapsed = self.fps.0.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    self.fps.2 = self.fps.1 as f32 / elapsed.as_secs_f32();
                    self.fps = (Instant::now(), 0, self.fps.2);
                    self.set_title();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            ViewerEvent::Ended(reason) => {
                eprintln!("Sesión finalizada: {reason}");
                if self.closing.is_some() {
                    event_loop.exit();
                    return;
                }
                self.connected = false;
                self.status = format!("Desconectado: {reason}");
                self.set_title();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.release_all_keys();
                self.handle.disconnect();
                if !self.connected {
                    event_loop.exit();
                } else {
                    // Wait (briefly) for the engine to send Bye/Hangup.
                    self.closing = Some(Instant::now());
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        Instant::now() + Duration::from_millis(1500),
                    ));
                }
            }
            WindowEvent::Resized(size) => {
                self.compute_viewport(size.width, size.height);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw() {
                    tracing::warn!("redraw failed: {e:#}");
                }
            }
            WindowEvent::Focused(false) => self.release_all_keys(),
            WindowEvent::CursorMoved { position, .. } if self.connected => {
                if let Some((x, y)) = self.normalize(position)
                    && self.last_mouse != Some((x, y)) {
                        self.last_mouse = Some((x, y));
                        self.handle.send(ControlMessage::MouseMove { x, y });
                    }
            }
            WindowEvent::MouseInput { state, button, .. } if self.connected => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    _ => return,
                };
                self.handle.send(ControlMessage::MouseButton {
                    button,
                    down: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } if self.connected => {
                // winit: positive y = away from the user (scroll up).
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, -y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 40.0, -(p.y as f32) / 40.0),
                };
                if dx != 0.0 || dy != 0.0 {
                    self.handle.send(ControlMessage::MouseWheel { dx, dy });
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if self.connected => {
                if event.repeat {
                    return; // the remote OS generates its own auto-repeat
                }
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let Some(name) = self.key_code_name(code) else { return };
                let down = event.state == ElementState::Pressed;
                if down {
                    self.pressed_keys.insert(name.clone());
                } else {
                    self.pressed_keys.remove(&name);
                }
                self.handle.send(ControlMessage::Key { code: name, down });
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.closing.is_some_and(|t| t.elapsed() >= Duration::from_millis(1500)) {
            event_loop.exit();
        }
    }
}

fn read_password() -> Result<String> {
    print!("Contraseña del equipo remoto: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .init();
    let cli = Cli::parse();
    let device_id: String = cli.id.chars().filter(|c| !c.is_whitespace()).collect();
    let password = match cli.password {
        Some(p) => p,
        None => read_password()?,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("vv-net")
        .build()
        .context("starting async runtime")?;

    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    let proxy: EventLoopProxy<UiEvent> = event_loop.create_proxy();

    let handle = start_viewer(
        runtime.handle(),
        ViewerConfig {
            signaling_url: cli.server,
            device_id,
            password,
            transport: TransportConfig::from_options(
                cli.stun,
                cli.turn,
                cli.turn_user,
                cli.turn_pass,
                cli.relay_only,
            ),
        },
        move |event| {
            let _ = proxy.send_event(UiEvent::Engine(event));
        },
    );

    let mut app = App {
        handle,
        swap_cmd_ctrl: cfg!(target_os = "macos") && !cli.raw_cmd,
        window: None,
        surface: None,
        frame: None,
        viewport: Viewport::default(),
        x_map: Vec::new(),
        status: "Iniciando...".into(),
        connected: false,
        pressed_keys: BTreeSet::new(),
        last_mouse: None,
        closing: None,
        fps: (Instant::now(), 0, 0.0),
    };
    event_loop.run_app(&mut app)?;
    runtime.shutdown_timeout(Duration::from_millis(500));
    Ok(())
}

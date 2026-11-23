//! VersaViewer agent (Phase 0 CLI): makes this machine reachable for remote
//! assistance. Run it on the Windows PC you want to control.

use anyhow::Result;
use clap::Parser;
#[cfg(not(windows))]
use std::io::{BufRead, Write};
use std::sync::Arc;
use vv_engine::agent::{AgentConfig, AgentEvent, ConsentPolicy, ConsentPrompt, ConsentRequest};
use vv_engine::credentials::format_device_id;
use vv_engine::{EncoderSettings, TransportConfig};

/// Default log filter. ICE probes every local interface, so virtual adapters
/// (VirtualBox, WSL/Hyper-V, VMware...) produce "network unreachable" send
/// errors and DTLS/ICE emit setup chatter: all expected and harmless, so they
/// are hidden. Set `RUST_LOG=debug` to see everything.
const DEFAULT_LOG_FILTER: &str =
    "info,rtc=warn,rtc_ice=error,rtc_dtls=error,webrtc=warn,webrtc::peer_connection::driver=off";

#[derive(Parser)]
#[command(name = "vv-agent", version, about = "VersaViewer agent: share this screen for remote assistance")]
struct Cli {
    /// Signaling server URL, e.g. ws://192.168.1.20:8080
    #[arg(short, long, env = "VV_SERVER")]
    server: String,
    /// Fixed 9-digit device ID (random if omitted).
    #[arg(long, env = "VV_DEVICE_ID")]
    id: Option<String>,
    /// Unattended access: accept sessions presenting this password without
    /// asking. At least 8 characters. Prefer the env var over the flag.
    #[arg(long, env = "VV_UNATTENDED_PASSWORD", hide_env_values = true)]
    unattended_password: Option<String>,
    /// Display index to share (primary if omitted). Use --list-displays.
    #[arg(long)]
    display: Option<usize>,
    #[arg(long)]
    list_displays: bool,
    #[arg(long, default_value_t = 30.0)]
    fps: f32,
    /// Video bitrate in kbit/s.
    #[arg(long, default_value_t = 6000)]
    bitrate: u32,
    /// Larger screens are downscaled to fit this size before encoding.
    #[arg(long, default_value_t = 1920)]
    max_width: u32,
    #[arg(long, default_value_t = 1200)]
    max_height: u32,
    #[arg(long, env = "VV_STUN")]
    stun: Option<String>,
    /// TURN server, e.g. turn:192.168.1.20:3478
    #[arg(long, env = "VV_TURN")]
    turn: Option<String>,
    #[arg(long, env = "VV_TURN_USER")]
    turn_user: Option<String>,
    #[arg(long, env = "VV_TURN_PASS", hide_env_values = true)]
    turn_pass: Option<String>,
    /// Force media through TURN (tests the relay fallback).
    #[arg(long)]
    relay_only: bool,
}

struct LocalConsent;

impl ConsentPrompt for LocalConsent {
    fn ask(&self, request: &ConsentRequest) -> bool {
        ask_consent(request)
    }
}

#[cfg(windows)]
fn ask_consent(_request: &ConsentRequest) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO,
        MessageBoxW,
    };
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let text = wide(
        "Un técnico solicita acceso remoto a este equipo.\n\n\
         Podrá VER su pantalla y CONTROLAR el mouse y el teclado.\n\n\
         ¿Desea permitir la conexión?",
    );
    let title = wide("VersaViewer — Solicitud de acceso remoto");
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND | MB_SYSTEMMODAL,
        )
    };
    result == IDYES
}

#[cfg(not(windows))]
fn ask_consent(_request: &ConsentRequest) -> bool {
    print!(
        "\n>>> Un técnico solicita acceso remoto (ver pantalla y controlar mouse/teclado).\n\
         >>> ¿Permitir? [s/N]: "
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim().to_lowercase().as_str(), "s" | "si" | "sí" | "y" | "yes")
}

/// Visible session indicator. Phase 0: console title + banner.
/// Phase 1: tray icon + always-on-top border overlay.
fn set_indicator(active: bool) {
    let title = if active {
        "VersaViewer — SESIÓN REMOTA ACTIVA"
    } else {
        "VersaViewer Agent"
    };
    #[cfg(windows)]
    {
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: NUL-terminated UTF-16 buffer valid for the duration of the call.
        unsafe { windows_sys::Win32::System::Console::SetConsoleTitleW(wide.as_ptr()) };
    }
    #[cfg(not(windows))]
    {
        print!("\x1b]0;{title}\x07");
        let _ = std::io::stdout().flush();
    }
}

fn print_event(event: AgentEvent, unattended: bool) {
    match event {
        AgentEvent::Ready { device_id, password } => {
            println!();
            println!("  ┌───────────────────────────────────────┐");
            println!("  │  VersaViewer listo para recibir ayuda │");
            println!("  │                                       │");
            println!("  │  ID:          {:<23} │", format_device_id(&device_id));
            if unattended {
                println!("  │  Contraseña:  {:<23} │", "(desatendida, fija)");
            } else {
                println!("  │  Contraseña:  {:<23} │", password);
            }
            println!("  └───────────────────────────────────────┘");
            println!("  Comparta el ID y la contraseña solo con quien le dará soporte.\n");
            set_indicator(false);
        }
        AgentEvent::SessionStarted { session_id } => {
            set_indicator(true);
            println!("\n  ██ SESIÓN REMOTA ACTIVA ██  ({session_id})");
            println!("  Un técnico está viendo y controlando este equipo.");
            println!("  Para terminarla, cierre esta ventana o pulse Ctrl+C.\n");
        }
        AgentEvent::SessionEnded { reason, .. } => {
            set_indicator(false);
            println!("\n  Sesión remota finalizada: {reason}\n");
        }
        AgentEvent::PasswordChanged { password } => {
            println!("  Nueva contraseña de un solo uso: {password}\n");
        }
        AgentEvent::Rejected { reason } => {
            println!("  Intento de conexión rechazado: {reason}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    vv_engine::agent::platform_init();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .init();
    let cli = Cli::parse();

    if cli.list_displays {
        for d in vv_engine::agent::list_displays()? {
            println!(
                "#{} {} {}x{} at ({},{}){}",
                d.index,
                d.name,
                d.width,
                d.height,
                d.x,
                d.y,
                if d.primary { " [primary]" } else { "" }
            );
        }
        return Ok(());
    }

    let unattended = cli.unattended_password.is_some();
    let config = AgentConfig {
        signaling_url: cli.server,
        device_id: cli.id,
        transport: TransportConfig::from_options(
            cli.stun,
            cli.turn,
            cli.turn_user,
            cli.turn_pass,
            cli.relay_only,
        ),
        encoder: EncoderSettings {
            bitrate_bps: cli.bitrate * 1000,
            max_fps: cli.fps,
            max_width: cli.max_width,
            max_height: cli.max_height,
            ..Default::default()
        },
        display: cli.display,
        consent: match cli.unattended_password {
            Some(password) => ConsentPolicy::Unattended { password },
            None => ConsentPolicy::Prompt,
        },
    };
    if unattended {
        println!("  ⚠  Modo DESATENDIDO: las sesiones con la contraseña fija se aceptan sin preguntar.");
    }

    vv_engine::agent::run_agent(config, Arc::new(LocalConsent), move |e| print_event(e, unattended))
        .await
}

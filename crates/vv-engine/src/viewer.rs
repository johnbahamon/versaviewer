//! Viewer (controlling side) session engine.
//!
//! The UI owns the window and the event loop; this module owns everything
//! else. The UI receives [`ViewerEvent`]s through a callback (called from
//! engine threads — the UI must hop to its own thread, e.g. via an event-loop
//! proxy) and sends input through [`ViewerHandle`].

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use vv_codec::{DecodedFrame, H264Decoder};
use vv_protocol::PROTOCOL_VERSION;
use vv_protocol::control::ControlMessage;
use vv_protocol::signal::{ClientMessage, ServerMessage};
use vv_transport::{ConnectionState, Peer, PeerEvent, Role, SignalingClient, TransportConfig};

/// How long the viewer waits for the remote user to accept.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(90);
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(500);

pub struct ViewerConfig {
    pub signaling_url: String,
    pub device_id: String,
    pub password: String,
    pub transport: TransportConfig,
}

#[derive(Debug)]
pub enum ViewerEvent {
    Status(String),
    Connected,
    ScreenInfo { width: u32, height: u32 },
    Frame(DecodedFrame),
    Ended(String),
}

/// Handle for the UI to send input and end the session.
#[derive(Clone)]
pub struct ViewerHandle {
    input: mpsc::UnboundedSender<ControlMessage>,
}

impl ViewerHandle {
    /// Queues an input/control message for the remote agent.
    pub fn send(&self, msg: ControlMessage) {
        let _ = self.input.send(msg);
    }

    pub fn disconnect(&self) {
        let _ = self.input.send(ControlMessage::Bye);
    }
}

type Sink = Arc<dyn Fn(ViewerEvent) + Send + Sync>;

/// Starts a viewer session on `runtime`. Events are delivered to `on_event`.
pub fn start_viewer(
    runtime: &tokio::runtime::Handle,
    config: ViewerConfig,
    on_event: impl Fn(ViewerEvent) + Send + Sync + 'static,
) -> ViewerHandle {
    let sink: Sink = Arc::new(on_event);
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    runtime.spawn(async move {
        let reason = match run(config, sink.clone(), input_rx).await {
            Ok(reason) => reason,
            Err(e) => format!("{e:#}"),
        };
        sink(ViewerEvent::Ended(reason));
    });
    ViewerHandle { input: input_tx }
}

async fn run(
    config: ViewerConfig,
    sink: Sink,
    mut input: mpsc::UnboundedReceiver<ControlMessage>,
) -> Result<String> {
    sink(ViewerEvent::Status(format!("Conectando a {}...", config.signaling_url)));
    let mut signaling = SignalingClient::connect(&config.signaling_url).await?;
    signaling.send(ClientMessage::Connect {
        device_id: config.device_id.clone(),
        password: config.password.clone(),
        protocol: PROTOCOL_VERSION,
    })?;
    sink(ViewerEvent::Status("Esperando autorización del equipo remoto...".into()));

    let session_id = tokio::time::timeout(CONSENT_TIMEOUT, async {
        loop {
            match signaling.recv().await {
                Some(ServerMessage::Accepted { session_id }) => return Ok(session_id),
                Some(ServerMessage::Rejected { reason, .. }) => bail!("conexión rechazada: {reason}"),
                Some(ServerMessage::Error { message }) => bail!("error del servidor: {message}"),
                Some(_) => continue,
                None => bail!("se perdió la conexión con el servidor de señalización"),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("el equipo remoto no respondió a tiempo"))??;

    sink(ViewerEvent::Status("Negociando conexión WebRTC...".into()));
    let (mut peer, mut events) = Peer::new(Role::Viewer, &config.transport).await?;
    let offer = peer.create_offer().await?;
    signaling.send(ClientMessage::Sdp { session_id: session_id.clone(), sdp: offer })?;

    let answer = loop {
        match signaling.recv().await {
            Some(ServerMessage::Sdp { sdp, .. }) => break sdp,
            Some(ServerMessage::Hangup { .. }) => bail!("el equipo remoto cerró la sesión"),
            Some(_) => continue,
            None => bail!("se perdió la conexión con el servidor de señalización"),
        }
    };
    peer.accept_answer(&answer).await?;

    let decoder = spawn_decoder(sink.clone());
    let mut last_keyframe_request = Instant::now() - KEYFRAME_REQUEST_MIN_INTERVAL;

    let reason = loop {
        tokio::select! {
            evt = events.recv() => {
                let Some(evt) = evt else { break "conexión cerrada".to_string() };
                match evt {
                    PeerEvent::State(ConnectionState::Connected) => {
                        sink(ViewerEvent::Connected);
                    }
                    PeerEvent::State(ConnectionState::Disconnected) => {
                        sink(ViewerEvent::Status("Conexión inestable, reintentando...".into()));
                    }
                    PeerEvent::State(s @ (ConnectionState::Failed | ConnectionState::Closed)) => {
                        break format!("conexión {s}");
                    }
                    PeerEvent::State(_) => {}
                    PeerEvent::ControlOpen => {
                        let _ = peer.send_control(&ControlMessage::RequestKeyframe).await;
                    }
                    PeerEvent::ControlClosed => break "el canal de control se cerró".to_string(),
                    PeerEvent::Control(ControlMessage::ScreenInfo { width, height }) => {
                        sink(ViewerEvent::ScreenInfo { width, height });
                    }
                    PeerEvent::Control(ControlMessage::Bye) => break "sesión finalizada por el equipo remoto".to_string(),
                    PeerEvent::Control(_) => {}
                    PeerEvent::VideoFrame(au) => {
                        if decoder.send(au).is_err() {
                            break "el decodificador se detuvo".to_string();
                        }
                    }
                    PeerEvent::VideoLoss => {
                        if last_keyframe_request.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL {
                            last_keyframe_request = Instant::now();
                            let _ = peer.send_control(&ControlMessage::RequestKeyframe).await;
                        }
                    }
                }
            }
            msg = input.recv() => {
                match msg {
                    None | Some(ControlMessage::Bye) => {
                        let _ = peer.send_control(&ControlMessage::Bye).await;
                        break "sesión finalizada".to_string();
                    }
                    Some(msg) => {
                        if let Err(e) = peer.send_control(&msg).await {
                            tracing::debug!("control send failed: {e}");
                        }
                    }
                }
            }
            msg = signaling.recv() => {
                if let Some(ServerMessage::Hangup { .. }) = msg {
                    break "el equipo remoto cerró la sesión".to_string();
                }
            }
        }
    };

    let _ = signaling.send(ClientMessage::Hangup { session_id });
    peer.close().await;
    // Give the WebSocket writer a moment to flush the hangup.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(reason)
}

/// Decoding happens on its own thread: it's CPU-bound and must not stall the
/// network event loop.
fn spawn_decoder(sink: Sink) -> std::sync::mpsc::Sender<Bytes> {
    let (tx, rx) = std::sync::mpsc::channel::<Bytes>();
    std::thread::Builder::new()
        .name("vv-decoder".into())
        .spawn(move || {
            let mut decoder = match H264Decoder::new() {
                Ok(d) => d,
                Err(e) => {
                    sink(ViewerEvent::Ended(format!("no se pudo crear el decodificador: {e:#}")));
                    return;
                }
            };
            for au in rx {
                match decoder.decode(&au) {
                    Ok(Some(frame)) => sink(ViewerEvent::Frame(frame)),
                    Ok(None) => {}
                    Err(e) => tracing::debug!("decode error: {e:#}"),
                }
            }
        })
        .expect("spawning decoder thread");
    tx
}

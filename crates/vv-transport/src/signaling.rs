//! Signaling client: JSON messages over a WebSocket.

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use vv_protocol::signal::{ClientMessage, ServerMessage};

/// Cloneable handle for sending signaling messages from any task.
#[derive(Clone)]
pub struct SignalingSender(mpsc::UnboundedSender<ClientMessage>);

impl SignalingSender {
    pub fn send(&self, msg: ClientMessage) -> Result<()> {
        self.0.send(msg).map_err(|_| anyhow!("signaling connection closed"))
    }
}

pub struct SignalingClient {
    tx: mpsc::UnboundedSender<ClientMessage>,
    rx: mpsc::UnboundedReceiver<ServerMessage>,
}

impl SignalingClient {
    /// Connects to `url` (e.g. `ws://192.168.1.10:8080`).
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connecting to signaling server {url}"))?;
        let (mut sink, mut stream) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerMessage>();

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let text = match vv_protocol::encode(&msg) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("encoding signaling message: {e}");
                        continue;
                    }
                };
                if let Err(e) = sink.send(Message::text(text)).await {
                    tracing::warn!("signaling send failed: {e}");
                    break;
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Text(text)) => match vv_protocol::decode::<ServerMessage>(&text) {
                        Ok(msg) => {
                            if in_tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!("invalid signaling message: {e}"),
                    },
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("signaling connection error: {e}");
                        break;
                    }
                }
            }
            tracing::info!("signaling connection closed");
        });

        Ok(Self { tx: out_tx, rx: in_rx })
    }

    pub fn send(&self, msg: ClientMessage) -> Result<()> {
        self.tx.send(msg).map_err(|_| anyhow!("signaling connection closed"))
    }

    pub fn sender(&self) -> SignalingSender {
        SignalingSender(self.tx.clone())
    }

    /// Next message from the server; `None` once the connection is gone.
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        self.rx.recv().await
    }
}

//! Headless viewer for smoke tests / CI: connects, counts decoded frames for
//! a few seconds, sends a harmless mouse move, then disconnects.
//!
//! cargo run -p vv-engine --example headless_viewer -- ws://127.0.0.1:8080 <ID> <PASSWORD> [secs]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vv_engine::viewer::{ViewerConfig, ViewerEvent, start_viewer};
use vv_engine::{ControlMessage, TransportConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(url), Some(id), Some(password)) = (args.next(), args.next(), args.next()) else {
        anyhow::bail!("usage: headless_viewer <ws-url> <device-id> <password> [secs]");
    };
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    #[derive(Default)]
    struct Stats {
        frames: u32,
        size: (u32, u32),
        connected_at: Option<Instant>,
        ended: Option<String>,
    }
    let stats = Arc::new(Mutex::new(Stats::default()));
    let s = stats.clone();
    let started = Instant::now();
    let handle = start_viewer(
        &tokio::runtime::Handle::current(),
        ViewerConfig {
            signaling_url: url,
            device_id: id,
            password,
            transport: TransportConfig { ice_servers: vec![], relay_only: false },
        },
        move |e| {
            let mut s = s.lock().unwrap();
            match e {
                ViewerEvent::Status(m) => println!("status: {m}"),
                ViewerEvent::Connected => {
                    s.connected_at = Some(Instant::now());
                    println!("connected after {:?}", started.elapsed());
                }
                ViewerEvent::Frame(f) => {
                    if s.frames == 0 {
                        println!("first frame {}x{} after {:?}", f.width, f.height, started.elapsed());
                    }
                    s.frames += 1;
                    s.size = (f.width, f.height);
                }
                ViewerEvent::ScreenInfo { width, height } => println!("screen info {width}x{height}"),
                ViewerEvent::Ended(r) => s.ended = Some(r),
            }
        },
    );

    let deadline = Instant::now() + Duration::from_secs(secs + 30);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = stats.lock().unwrap();
        if let Some(r) = &s.ended {
            anyhow::bail!("session ended early: {r}");
        }
        if s.connected_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(secs)) {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for connection");
        }
    }
    handle.send(ControlMessage::MouseMove { x: 0.5, y: 0.5 });
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.disconnect();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let s = stats.lock().unwrap();
    let fps = s.frames as f32 / secs as f32;
    println!("RESULT frames={} size={}x{} avg_fps={fps:.1}", s.frames, s.size.0, s.size.1);
    anyhow::ensure!(s.frames > 0, "no frames decoded");
    Ok(())
}

//! Encode throughput benchmark for typical screen sizes.
//!
//! cargo run --release -p vv-codec --example encode_bench
//!
//! Uses a synthetic "desktop" (flat background + text-like stripes) with a
//! moving region each frame, roughly what remote support traffic looks like.

use std::time::{Duration, Instant};
use vv_codec::{EncoderSettings, H264Encoder, PixelOrder, RawImage};

fn synthetic(w: u32, h: u32, frame: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let text = (y / 18) % 2 == 0 && (x / 7 + y / 3 + frame) % 5 < 2;
            let window = x > w / 4 && x < w * 3 / 4 && y > h / 5 && y < h * 4 / 5;
            let v: [u8; 3] = if text { [30, 30, 30] } else if window { [250, 250, 250] } else { [20, 90, 160] };
            buf[i..i + 4].copy_from_slice(&[v[2], v[1], v[0], 255]); // BGRA
        }
    }
    buf
}

fn main() {
    run(EncoderSettings::default());
    println!("-- without downscaling (Phase 0 behaviour):");
    run(EncoderSettings { max_width: 8192, max_height: 8192, ..Default::default() });
}

fn run(settings: EncoderSettings) {
    println!("max {}x{} @ {} kbps", settings.max_width, settings.max_height, settings.bitrate_bps / 1000);
    for (w, h) in [(1920, 1080), (2560, 1600), (3840, 2160)] {
        let frames: Vec<Vec<u8>> = (0..10).map(|f| synthetic(w, h, f)).collect();
        let mut enc = H264Encoder::new(w, h, settings).unwrap();
        let (mut conv, mut code, mut n) = (Duration::ZERO, Duration::ZERO, 0u32);
        let start = Instant::now();
        for i in 0..60 {
            let data = &frames[i % frames.len()];
            let img = RawImage { data, width: w, height: h, stride: w as usize * 4, order: PixelOrder::Bgra };
            enc.encode(&img).unwrap();
            let t = enc.last_timings();
            conv += t.convert;
            code += t.encode;
            n += 1;
        }
        let total = start.elapsed() / n;
        let (ow, oh) = enc.dimensions();
        println!(
            "{w}x{h} -> {ow}x{oh}: convert {:.1} ms, encode {:.1} ms, total {:.1} ms/frame (~{:.0} fps max)",
            (conv / n).as_secs_f64() * 1e3,
            (code / n).as_secs_f64() * 1e3,
            total.as_secs_f64() * 1e3,
            1.0 / total.as_secs_f64()
        );
    }
}

//! What the capture stages cost per 10 ms frame in the usual idle state:
//! connected, unmuted, a quiet room, silence playing. Needs no devices.
//!
//! ```bash
//! cargo run --release -p blurt-audio --example pipeline_cost
//! # Frames 10 ms apart, as in real use (the CPU clocks down and caches go
//! # cold between frames, so each costs more). Takes a few minutes.
//! SPACED=1 cargo run --release -p blurt-audio --example pipeline_cost
//! # Room noise level, as linear amplitude (default 0.0002, about -74 dBFS).
//! LEVEL=0.001 cargo run --release -p blurt-audio --example pipeline_cost
//! ```
//!
//! The input is white noise, which the voice detector sometimes mistakes for
//! speech once the gain control has raised it; "packets" shows whether that
//! happened, since encoding adds to the cost.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use blurt_audio::FRAME_SAMPLES;
use blurt_audio::capture::{Capture, Processing, TransmitMode};
use blurt_audio::codec::EncoderConfig;

const WARMUP: usize = 300;
const FRAMES: usize = 3000;

fn main() {
    let spaced = std::env::var_os("SPACED").is_some();
    let level: f32 = std::env::var("LEVEL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0002);

    let none = Processing {
        echo_cancellation: false,
        noise_suppression: false,
        auto_gain: false,
    };
    let all = Processing::default();
    let configs = [
        ("voice detector only", none),
        (
            "+ echo cancellation",
            Processing {
                echo_cancellation: true,
                ..none
            },
        ),
        (
            "+ noise suppression",
            Processing {
                noise_suppression: true,
                ..none
            },
        ),
        (
            "+ auto gain",
            Processing {
                auto_gain: true,
                ..none
            },
        ),
        ("everything (default)", all),
    ];

    let mut seed = 1u64;
    let mut noise = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0) * level
    };
    let mic: Vec<[f32; FRAME_SAMPLES]> = (0..WARMUP + FRAMES)
        .map(|_| std::array::from_fn(|_| noise()))
        .collect();
    let silence = [0.0; FRAME_SAMPLES];

    for (name, processing) in configs {
        let mut capture = Capture::new(
            processing,
            TransmitMode::default(),
            EncoderConfig::default(),
        )
        .expect("capture");
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let mut sink = move |_: &[u8], _: u64, _: bool| {
            counter.fetch_add(1, Ordering::Relaxed);
        };
        let mut elapsed = Duration::ZERO;
        for (i, frame) in mic.iter().enumerate() {
            if spaced {
                std::thread::sleep(Duration::from_millis(10));
            }
            let start = Instant::now();
            capture.render(&silence);
            capture.capture(frame, false, false, &mut sink);
            if i >= WARMUP {
                elapsed += start.elapsed();
            }
        }
        let per_frame_us = elapsed.as_secs_f64() * 1e6 / FRAMES as f64;
        println!(
            "{name:22} {per_frame_us:6.1} µs/frame ({:.2}% of a core), packets {}",
            per_frame_us / 100.0,
            sent.load(Ordering::Relaxed),
        );
    }
}

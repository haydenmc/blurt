//! Replays a recording (from `EngineConfig::record_to`) through the capture
//! pipeline offline, with chosen settings, and scores the result. This
//! makes processing problems reproducible on real voice without devices.
//!
//! ```bash
//! cargo run --release -p blurt-audio --example reprocess -- DIR [OPTIONS]
//!   --no-aec --no-ns --no-agc   turn a stage off
//!   --ns KIND                   off, low, moderate, high, veryhigh, rnnoise
//!   --out DIR                   also write processed.wav and sent.wav there
//!   --vad-threshold P           voice activity threshold (default 0.5)
//!   --inject-noises             add slams, thumps, claps, typing and drops to
//!                               the silences, and report how much was sent
//!   --repeat N                  play the recording N times, scoring the last
//!                               (to see where adaptation settles)
//!   --far-end-shift SECS        shift the playback, to simulate someone else
//!                               talking over us instead of our own voice
//! ```
//!
//! Scores, over the raw mic's utterances:
//! - ducked: speech frames where processing pushed the voice 15 dB or more
//!   below its usual level (relative to the raw mic);
//! - gate: how far into an utterance sending started (median, worst);
//! - missed: utterances never sent;
//! - lag: how far processing delays the signal.

use std::path::{Path, PathBuf};

use blurt_audio::FRAME_SAMPLES;
use blurt_audio::capture::{Capture, NoiseSuppression, Processing, TransmitMode};
use blurt_audio::codec::EncoderConfig;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: reprocess DIR [OPTIONS]"));
    let mut processing = Processing::default();
    let mut out = None;
    let mut shift_secs: f64 = 0.0;
    let mut repeat = 1;
    let mut inject = false;
    let mut mode = TransmitMode::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-aec" => processing.echo_cancellation = false,
            "--no-ns" => processing.noise_suppression = NoiseSuppression::Off,
            "--ns" => {
                processing.noise_suppression = match args.next().expect("--ns KIND").as_str() {
                    "off" => NoiseSuppression::Off,
                    "low" => NoiseSuppression::Low,
                    "moderate" => NoiseSuppression::Moderate,
                    "high" => NoiseSuppression::High,
                    "veryhigh" => NoiseSuppression::VeryHigh,
                    "rnnoise" => NoiseSuppression::Rnnoise,
                    other => panic!("unknown noise suppression {other}"),
                }
            }
            "--no-agc" => processing.auto_gain = false,
            "--out" => out = Some(PathBuf::from(args.next().expect("--out DIR"))),
            "--vad-threshold" => {
                mode = TransmitMode::VoiceActivity {
                    threshold: args.next().expect("--vad-threshold P").parse().unwrap(),
                }
            }
            "--inject-noises" => inject = true,
            "--repeat" => repeat = args.next().expect("--repeat N").parse().unwrap(),
            "--far-end-shift" => {
                shift_secs = args.next().expect("--far-end-shift SECS").parse().unwrap()
            }
            other => panic!("unknown option {other}"),
        }
    }

    let clean_raw = read_wav(&dir.join("mic-raw.wav"));
    let mut raw = clean_raw.clone();
    let events = if inject {
        inject_noises(&mut raw)
    } else {
        Vec::new()
    };
    let mut playback = read_wav(&dir.join("playback.wav"));
    // Shifted, the far end is no longer our own voice: like someone else
    // talking over us.
    let shift = (shift_secs * 48_000.0) as usize % playback.len().max(1);
    playback.rotate_left(shift);
    let frames = raw.len().min(playback.len()) / FRAME_SAMPLES;
    let one = frames * FRAME_SAMPLES;
    let raw_all = raw[..one].repeat(repeat);
    let playback = playback[..one].repeat(repeat);

    let mut capture = Capture::new(processing, mode, EncoderConfig::default()).unwrap();
    // As the engine sets it: about the device latencies, both ways.
    capture.set_echo_delay_ms(25);
    let mut processed = Vec::with_capacity(frames * FRAME_SAMPLES);
    let mut sent = Vec::with_capacity(frames * FRAME_SAMPLES);
    let mut sink = |_: &[u8], _: u64, _: bool| {};
    let mut backlogs = Vec::new();
    for i in 0..frames * repeat {
        let span = i * FRAME_SAMPLES..(i + 1) * FRAME_SAMPLES;
        let play: &[f32; FRAME_SAMPLES] = playback[span.clone()].try_into().unwrap();
        let mic: &[f32; FRAME_SAMPLES] = raw_all[span].try_into().unwrap();
        capture.render(play);
        let report = capture.capture(mic, false, false, &mut sink);
        processed.extend_from_slice(capture.processed());
        if report.transmitting {
            sent.extend_from_slice(capture.processed());
        } else {
            sent.extend_from_slice(&[0.0; FRAME_SAMPLES]);
        }
        // Earlier frames sent now (reaching back to where speech began):
        // mark them sent where they belong, and count what it costs.
        if report.backlog > 0 {
            let end = sent.len() - FRAME_SAMPLES;
            let start = end - report.backlog * FRAME_SAMPLES;
            sent[start..end].copy_from_slice(&processed[start..end]);
            if i >= frames * (repeat - 1) {
                backlogs.push(report.backlog);
            }
        }
    }
    // Score only the last pass, once everything has adapted.
    processed.drain(..one * (repeat - 1));
    sent.drain(..one * (repeat - 1));
    if let Some(out) = &out {
        std::fs::create_dir_all(out).unwrap();
        write_wav(&out.join("processed.wav"), &processed);
        write_wav(&out.join("sent.wav"), &sent);
    }

    let raw_db = frame_levels(&clean_raw[..frames * FRAME_SAMPLES]);
    let proc_db = frame_levels(&processed);
    let sent_db = frame_levels(&sent);
    let lag = processing_lag(&raw_db, &proc_db);

    let floor = percentile(&raw_db, 0.1);
    let speech = floor + 20.0;
    // Processing's usual gain on speech, to judge ducking against.
    let mut gains: Vec<f32> = (0..frames)
        .filter(|&i| raw_db[i] > speech && i + lag < frames)
        .map(|i| proc_db[i + lag] - raw_db[i])
        .collect();
    let usual_gain = percentile(&gains, 0.5);
    let ducked = gains.iter().filter(|&&g| g < usual_gain - 15.0).count();
    let speech_frames = gains.len().max(1);
    gains.clear();

    let mut gate_delays = Vec::new();
    let mut missed = 0;
    let mut utterances = 0;
    // Frames sent with no speech nearby (beyond the gate's hold): false
    // triggers.
    let mut near_speech = vec![false; frames];
    for (start, end) in utterances_in(&raw_db, floor + 15.0) {
        utterances += 1;
        for f in near_speech
            .iter_mut()
            .take((end + 40).min(frames))
            .skip(start.saturating_sub(5))
        {
            *f = true;
        }
        match (start..(end + lag).min(frames)).find(|&f| sent_db[f] > -90.0) {
            Some(f) => gate_delays.push((f - start) * 10),
            None => missed += 1,
        }
    }
    for &(_, at) in &events {
        for f in near_speech.iter_mut().skip(at).take(EVENT_WINDOW) {
            *f = true;
        }
    }
    let false_ms = (0..frames)
        .filter(|&f| sent_db[f] > -90.0 && !near_speech[f])
        .count()
        * 10;
    let silent: Vec<f32> = (0..frames.saturating_sub(lag))
        .filter(|&f| raw_db[f] < floor + 6.0)
        .map(|f| proc_db[f + lag] - raw_db[f])
        .collect();
    let noise_db = percentile(&silent, 0.5);
    gate_delays.sort_unstable();
    let median_gate = gate_delays.get(gate_delays.len() / 2).copied().unwrap_or(0);
    let worst_gate = gate_delays.last().copied().unwrap_or(0);
    println!(
        "{:28} ducked {:5.1}%  gate {:3} ms median, {:3} ms worst  missed {}/{}  false {:4} ms  noise {:5.1} dB  lag {} ms",
        label(processing),
        100.0 * ducked as f64 / speech_frames as f64,
        median_gate,
        worst_gate,
        missed,
        utterances,
        false_ms,
        noise_db,
        lag * 10,
    );
    backlogs.sort_unstable();
    // The receiver would have waited for its initial jitter delay (3
    // frames) anyway; frames beyond that add latency to the utterance.
    let added = |b: usize| b.saturating_sub(3) * 10;
    println!(
        "{:28} backlog: {} starts, added latency {} ms median, {} ms worst",
        "",
        backlogs.len(),
        backlogs.get(backlogs.len() / 2).map_or(0, |&b| added(b)),
        backlogs.last().map_or(0, |&b| added(b)),
    );
    if !events.is_empty() {
        let mut by_kind: Vec<(Noise, usize, usize, usize, usize)> = Vec::new();
        for &(kind, at) in &events {
            let sent_frames = (at..(at + EVENT_WINDOW).min(frames))
                .filter(|&f| sent_db[f] > -90.0)
                .count();
            match by_kind.iter_mut().find(|k| k.0 == kind) {
                Some(k) => {
                    k.1 += 1;
                    k.2 += usize::from(sent_frames > 0);
                    k.3 += sent_frames * 10;
                    k.4 = k.4.max(sent_frames * 10);
                }
                None => by_kind.push((
                    kind,
                    1,
                    usize::from(sent_frames > 0),
                    sent_frames * 10,
                    sent_frames * 10,
                )),
            }
        }
        let line: Vec<String> = by_kind
            .iter()
            .map(|(kind, n, leaked, total, worst)| {
                format!(
                    "{kind:?} {leaked}/{n} sent (avg {} ms, worst {worst} ms)",
                    total / n
                )
            })
            .collect();
        println!("{:28} noises: {}", "", line.join("; "));
    }
}

/// Frames after a noise's start in which anything sent counts against it.
const EVENT_WINDOW: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Noise {
    Slam,
    Thump,
    Clap,
    Typing,
    Drop,
}

/// Adds a noise to each long silent stretch of `mic`, cycling through the
/// kinds. Returns each one's kind and starting frame.
fn inject_noises(mic: &mut [f32]) -> Vec<(Noise, usize)> {
    const KINDS: [Noise; 5] = [
        Noise::Slam,
        Noise::Thump,
        Noise::Clap,
        Noise::Typing,
        Noise::Drop,
    ];
    let levels = frame_levels(mic);
    let floor = percentile(&levels, 0.1);
    let mut events = Vec::new();
    let mut seed = 99u64;
    let mut noise = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0
    };
    let mut quiet_run = 0;
    let mut f = 0;
    while f < levels.len() {
        quiet_run = if levels[f] < floor + 10.0 {
            quiet_run + 1
        } else {
            0
        };
        // 200 ms into a silence that lasts at least 700 ms.
        let lasts = (f..(f + 50).min(levels.len())).all(|g| levels[g] < floor + 12.0);
        if quiet_run == 20 && lasts && f + 60 < levels.len() {
            let kind = KINDS[events.len() % KINDS.len()];
            let start = f * FRAME_SAMPLES;
            for (n, sample) in mic[start..].iter_mut().take(48_000 / 2).enumerate() {
                let t = n as f32 / 48_000.0;
                let tau = std::f32::consts::TAU;
                *sample += match kind {
                    Noise::Slam => 0.5 * (-t / 0.08).exp() * (noise() + (tau * 70.0 * t).sin()),
                    Noise::Thump => {
                        0.4 * (-t / 0.03).exp() * (tau * 110.0 * t).sin()
                            + 0.2 * (-t / 0.005).exp() * noise()
                    }
                    Noise::Clap => 0.6 * (-t / 0.012).exp() * noise(),
                    Noise::Typing => {
                        // Eight keys, each a press and a softer release.
                        let key = (t / 0.11) as u32;
                        let since = t - key as f32 * 0.11;
                        let press = 0.08 * (-since / 0.003).exp();
                        let release = if since > 0.06 {
                            0.04 * (-(since - 0.06) / 0.003).exp()
                        } else {
                            0.0
                        };
                        if key < 8 {
                            (press + release) * noise()
                        } else {
                            0.0
                        }
                    }
                    Noise::Drop => {
                        let mut bounce = |at: f32, amp: f32| {
                            if t < at {
                                0.0
                            } else {
                                let s = t - at;
                                amp * (-s / 0.02).exp() * (noise() + (tau * 300.0 * s).sin())
                            }
                        };
                        bounce(0.0, 0.4) + bounce(0.18, 0.25) + bounce(0.3, 0.15)
                    }
                };
            }
            events.push((kind, f));
            f += 90;
            quiet_run = 0;
            continue;
        }
        f += 1;
    }
    events
}

fn label(p: Processing) -> String {
    let on = |b: bool, name: &str| {
        if b {
            name.to_string()
        } else {
            format!("no-{name}")
        }
    };
    format!(
        "{} {} {}",
        on(p.echo_cancellation, "aec"),
        format!("ns-{:?}", p.noise_suppression).to_lowercase(),
        on(p.auto_gain, "agc")
    )
}

/// Runs of frames above `threshold`, ending after 100 ms below it; at
/// least 50 ms long.
fn utterances_in(levels: &[f32], threshold: f32) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < levels.len() {
        if levels[i] < threshold {
            i += 1;
            continue;
        }
        let start = i;
        let mut quiet = 0;
        while i < levels.len() && quiet < 10 {
            quiet = if levels[i] < threshold { quiet + 1 } else { 0 };
            i += 1;
        }
        let end = i - quiet;
        if end - start >= 5 {
            found.push((start, end));
        }
    }
    found
}

/// Frames (0-5) by which `processed` trails `raw`, by correlating levels.
fn processing_lag(raw: &[f32], processed: &[f32]) -> usize {
    (0..=5)
        .max_by(|&a, &b| {
            let score = |lag: usize| -> f32 {
                raw.iter()
                    .zip(&processed[lag..])
                    .map(|(r, p)| (r.max(-80.0) + 80.0) * (p.max(-80.0) + 80.0))
                    .sum()
            };
            score(a).total_cmp(&score(b))
        })
        .unwrap_or(0)
}

fn frame_levels(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks(FRAME_SAMPLES)
        .map(|f| {
            let ms = f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32;
            (10.0 * ms.log10()).max(-100.0)
        })
        .collect()
}

fn percentile(v: &[f32], p: f64) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut v = v.to_vec();
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f64 * p) as usize]
}

fn read_wav(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(bytes.len() >= 44 && &bytes[..4] == b"RIFF", "not a WAV");
    bytes[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn write_wav(path: &Path, samples: &[f32]) {
    let data = (samples.len() * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 4);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(data + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

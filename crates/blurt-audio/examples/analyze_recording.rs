//! Analyzes a recording made with `EngineConfig::record_to` (blurt-cli's
//! `--record DIR`): the delays between the tracks, and what processing and
//! the gate did to each utterance.
//!
//! ```bash
//! cargo run --release -p blurt-audio --example analyze_recording -- DIR
//! ```
//!
//! Also writes `DIR/levels.csv`: every track's level per 10 ms frame.

use std::path::Path;

const RATE: usize = 48_000;
/// Envelope resolution: 1 ms.
const HOP: usize = RATE / 1000;
const FRAME: usize = RATE / 100;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: analyze_recording DIR");
    let dir = Path::new(&dir);
    let load = |name: &str| read_wav(&dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let raw = load("mic-raw.wav");
    let processed = load("mic-processed.wav");
    let sent = load("sent.wav");
    let playback = load("playback.wav");
    let len = raw
        .len()
        .min(processed.len())
        .min(sent.len())
        .min(playback.len());
    println!(
        "{:.1} s recorded ({} ms of it sent, {} ms with something playing)",
        len as f64 / RATE as f64,
        active_ms(&sent[..len]),
        active_ms(&playback[..len]),
    );

    let env = |s: &[f32]| envelope(&s[..len]);
    let (raw_e, sent_e, play_e) = (env(&raw), env(&sent), env(&playback));

    println!("\nDelays (by cross-correlating loudness):");
    match best_lag(&sent_e, &play_e, 0, 1500) {
        Some((lag, r)) => println!(
            "  sent -> played back:          {lag:4} ms  (r = {r:.2})  encoder, network, server, jitter buffer"
        ),
        None => println!("  sent -> played back:          nothing to correlate"),
    }
    match best_lag(&play_e, &raw_e, 0, 1000) {
        Some((lag, r)) => println!(
            "  played -> heard by the mic:   {lag:4} ms  (r = {r:.2})  output latency, speakers to mic, input latency"
        ),
        None => println!("  played -> heard by the mic:   nothing to correlate"),
    }
    println!(
        "  (mouth to ear is the first plus the device latencies from --stats;\n   \
         the second is the echo path the canceller has to cover)"
    );

    // Echo: playback-only stretches, where anything the mic hears is echo.
    let mut echo_raw = Vec::new();
    let mut echo_out = Vec::new();
    for i in 0..len / FRAME {
        let f = i * FRAME..(i + 1) * FRAME;
        let play = db(rms(&playback[f.clone()]));
        if play > -45.0 {
            echo_raw.push(db(rms(&raw[f.clone()])));
            echo_out.push(db(rms(&processed[f.clone()])));
        }
    }
    if !echo_raw.is_empty() {
        println!(
            "\nWhile something played ({} frames): mic {:.1} dBFS raw -> {:.1} dBFS processed (median)",
            echo_raw.len(),
            median(&mut echo_raw),
            median(&mut echo_out),
        );
    }

    // Utterances: runs of raw mic well above its noise floor.
    let frames = len / FRAME;
    let raw_db: Vec<f32> = (0..frames)
        .map(|i| db(rms(&raw[i * FRAME..(i + 1) * FRAME])))
        .collect();
    let proc_db: Vec<f32> = (0..frames)
        .map(|i| db(rms(&processed[i * FRAME..(i + 1) * FRAME])))
        .collect();
    let sent_db: Vec<f32> = (0..frames)
        .map(|i| db(rms(&sent[i * FRAME..(i + 1) * FRAME])))
        .collect();
    let play_db: Vec<f32> = (0..frames)
        .map(|i| db(rms(&playback[i * FRAME..(i + 1) * FRAME])))
        .collect();
    let floor = percentile(&raw_db, 0.1);
    let threshold = floor + 15.0;
    println!("\nMic noise floor {floor:.1} dBFS; utterances are runs above {threshold:.1} dBFS:");
    println!(
        "  {:>7} {:>6} {:>8} {:>9} {:>9} {:>11} {:>10} {:>8}",
        "start s", "len ms", "raw dB", "proc dB", "gain dB", "first 50ms", "sent late", "echo?"
    );
    let mut i = 0;
    while i < frames {
        if raw_db[i] < threshold {
            i += 1;
            continue;
        }
        let start = i;
        // Ends after 100 ms below the threshold.
        let mut quiet = 0;
        while i < frames && quiet < 10 {
            quiet = if raw_db[i] < threshold { quiet + 1 } else { 0 };
            i += 1;
        }
        let end = i - quiet;
        if end - start < 5 {
            continue;
        }
        let span = start..end;
        let raw_level = mean_db(&raw_db[span.clone()]);
        let proc_level = mean_db(&proc_db[span.clone()]);
        let head = start..(start + 5).min(end);
        let first = mean_db(&proc_db[head]) - proc_level;
        let sent_from = (start..end).find(|&f| sent_db[f] > -90.0);
        let late = match sent_from {
            Some(f) => format!("{} ms", (f - start) * 10),
            None => "not sent".into(),
        };
        let echo = mean_db(&play_db[span.clone()]) > -45.0;
        println!(
            "  {:7.2} {:6} {:8.1} {:9.1} {:9.1} {:11.1} {:>10} {:>8}",
            start as f64 / 100.0,
            (end - start) * 10,
            raw_level,
            proc_level,
            proc_level - raw_level,
            first,
            late,
            if echo { "playing" } else { "" },
        );
    }
    println!(
        "  (\"first 50ms\": processed level at the start relative to the whole\n   \
         utterance; well below 0 means it faded in. \"sent late\": how long into\n   \
         it the gate opened.)"
    );

    let csv: String =
        std::iter::once("time_s,raw_db,processed_db,sent_db,playback_db\n".to_string())
            .chain((0..frames).map(|i| {
                format!(
                    "{:.2},{:.1},{:.1},{:.1},{:.1}\n",
                    i as f64 / 100.0,
                    raw_db[i],
                    proc_db[i],
                    sent_db[i],
                    play_db[i]
                )
            }))
            .collect();
    std::fs::write(dir.join("levels.csv"), csv).expect("writing levels.csv");
    println!("\nPer-frame levels: {}", dir.join("levels.csv").display());
}

fn read_wav(path: &Path) -> std::io::Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[36..40] != b"data" {
        return Err(std::io::Error::other("not a recorder WAV"));
    }
    Ok(bytes[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

fn db(rms: f32) -> f32 {
    (20.0 * rms.log10()).max(-100.0)
}

fn mean_db(levels: &[f32]) -> f32 {
    // Average power, not the average of dB values.
    let power = levels.iter().map(|d| 10f32.powf(d / 10.0)).sum::<f32>() / levels.len() as f32;
    (10.0 * power.log10()).max(-100.0)
}

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

fn percentile(v: &[f32], p: f64) -> f32 {
    let mut v = v.to_vec();
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f64 * p) as usize]
}

fn active_ms(s: &[f32]) -> usize {
    s.chunks(FRAME).filter(|f| db(rms(f)) > -90.0).count() * 10
}

/// Loudness in dB per millisecond, floored so silence doesn't dominate.
fn envelope(s: &[f32]) -> Vec<f32> {
    s.chunks(HOP).map(|c| db(rms(c)).max(-80.0)).collect()
}

/// The lag (ms, `b` behind `a`) in `min..max` where `b` best matches `a`,
/// with its correlation coefficient. Only counts where `a` is active.
fn best_lag(a: &[f32], b: &[f32], min: usize, max: usize) -> Option<(usize, f32)> {
    let active = a.iter().filter(|&&x| x > -60.0).count();
    if active < 200 {
        return None;
    }
    let mut best: Option<(usize, f32)> = None;
    for lag in min..max.min(a.len()) {
        let n = a.len() - lag;
        let (x, y) = (&a[..n], &b[lag..lag + n]);
        let mx = x.iter().sum::<f32>() / n as f32;
        let my = y.iter().sum::<f32>() / n as f32;
        let (mut sxy, mut sxx, mut syy) = (0.0f32, 0.0f32, 0.0f32);
        for (p, q) in x.iter().zip(y) {
            let (dp, dq) = (p - mx, q - my);
            sxy += dp * dq;
            sxx += dp * dp;
            syy += dq * dq;
        }
        let r = sxy / (sxx * syy).sqrt().max(1e-9);
        if best.is_none_or(|(_, b)| r > b) {
            best = Some((lag, r));
        }
    }
    best
}

//! Debug recording: what the engine heard, did and played, as WAV files on
//! one timeline, for working out where audio goes wrong (like WebRTC's
//! aecdump). Turned on by [`EngineConfig::record_to`](crate::EngineConfig).
//!
//! The DSP thread only pushes samples into lock-free rings; a writer thread
//! does the file I/O. Each track is 48 kHz mono 32-bit float and starts at
//! the same instant (earlier silence is padded), with samples placed at the
//! time the DSP thread handled them. So the mic tracks run the device's
//! input latency behind the room, and the playback track its output latency
//! ahead of the speakers; both are in the engine's stats.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

/// The recorded tracks, one WAV file each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Track {
    /// The mic, before any processing.
    MicRaw,
    /// After echo cancellation, gain and noise suppression, before the gate.
    MicProcessed,
    /// What was encoded and sent (silence where the gate was closed).
    Sent,
    /// What was given to the speakers (and to the echo canceller).
    Playback,
}

impl Track {
    const ALL: [Track; 4] = [
        Track::MicRaw,
        Track::MicProcessed,
        Track::Sent,
        Track::Playback,
    ];

    fn file_name(self) -> &'static str {
        match self {
            Track::MicRaw => "mic-raw.wav",
            Track::MicProcessed => "mic-processed.wav",
            Track::Sent => "sent.wav",
            Track::Playback => "playback.wav",
        }
    }
}

/// Room for two seconds per track, far more than the writer ever falls
/// behind.
const RING_SAMPLES: usize = 2 * SAMPLE_RATE as usize;

pub(crate) struct Recorder {
    rings: Vec<rtrb::Producer<f32>>,
    started: Instant,
    /// Samples pushed so far, per track.
    written: [u64; 4],
    dropped: bool,
    stop: Arc<AtomicBool>,
    writer: Option<thread::JoinHandle<()>>,
}

impl Recorder {
    pub(crate) fn start(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut rings = Vec::new();
        let mut outputs = Vec::new();
        for track in Track::ALL {
            let (tx, rx) = rtrb::RingBuffer::new(RING_SAMPLES);
            rings.push(tx);
            outputs.push((rx, WavWriter::create(&dir.join(track.file_name()))?));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = stop.clone();
        let writer = thread::Builder::new()
            .name("blurt-recorder".into())
            .spawn(move || write_until_stopped(outputs, &writer_stop))?;
        log::info!("Recording audio to {}", dir.display());
        Ok(Self {
            rings,
            started: Instant::now(),
            written: [0; 4],
            dropped: false,
            stop,
            writer: Some(writer),
        })
    }

    /// Adds one frame to `track`, at the current time. Never blocks.
    pub(crate) fn push(&mut self, track: Track, frame: &[f32; FRAME_SAMPLES]) {
        let i = track as usize;
        // Pad to now on a track's first frame, so all tracks share a start.
        if self.written[i] == 0 {
            let lead = (self.started.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE)) as u64;
            let lead = lead.saturating_sub(FRAME_SAMPLES as u64);
            for _ in 0..lead {
                if self.rings[i].push(0.0).is_err() {
                    break;
                }
            }
            self.written[i] = lead;
        }
        let ring = &mut self.rings[i];
        let fits = ring.slots() >= FRAME_SAMPLES;
        if let (true, Ok(mut chunk)) = (fits, ring.write_chunk(FRAME_SAMPLES)) {
            let (a, b) = chunk.as_mut_slices();
            let split = a.len();
            a.copy_from_slice(&frame[..split]);
            b.copy_from_slice(&frame[split..]);
            chunk.commit_all();
        } else if !self.dropped {
            self.dropped = true;
            log::warn!("Recording fell behind; the tracks are no longer aligned");
        }
        self.written[i] += FRAME_SAMPLES as u64;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_until_stopped(mut outputs: Vec<(rtrb::Consumer<f32>, WavWriter)>, stop: &AtomicBool) {
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        for (ring, wav) in &mut outputs {
            let n = ring.slots();
            if let Ok(chunk) = ring.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                if let Err(e) = wav.write(a).and_then(|()| wav.write(b)) {
                    log::warn!("Recording failed: {e}");
                    return;
                }
                chunk.commit_all();
            }
        }
        if stopping {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    for (_, wav) in outputs {
        if let Err(e) = wav.finish() {
            log::warn!("Could not finish a recording: {e}");
        }
    }
}

/// A minimal 32-bit float mono WAV writer.
struct WavWriter {
    file: BufWriter<File>,
    path: PathBuf,
    samples: u32,
}

impl WavWriter {
    fn create(path: &Path) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&header(0))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            samples: 0,
        })
    }

    fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        for sample in samples {
            self.file.write_all(&sample.to_le_bytes())?;
        }
        self.samples = self.samples.saturating_add(samples.len() as u32);
        Ok(())
    }

    /// Fills in the sizes, which aren't known until the end.
    fn finish(mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(self.samples))?;
        self.file.flush()?;
        log::info!(
            "Wrote {} ({:.1} s)",
            self.path.display(),
            f64::from(self.samples) / f64::from(SAMPLE_RATE)
        );
        Ok(())
    }
}

fn header(samples: u32) -> [u8; 44] {
    const FLOAT: u16 = 3;
    let data_bytes = samples.saturating_mul(4);
    let mut h = [0u8; 44];
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        h[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(b"RIFF");
    put(&data_bytes.saturating_add(36).to_le_bytes());
    put(b"WAVEfmt ");
    put(&16u32.to_le_bytes());
    put(&FLOAT.to_le_bytes());
    put(&1u16.to_le_bytes()); // mono
    put(&SAMPLE_RATE.to_le_bytes());
    put(&(SAMPLE_RATE * 4).to_le_bytes()); // bytes per second
    put(&4u16.to_le_bytes()); // bytes per sample frame
    put(&32u16.to_le_bytes()); // bits per sample
    put(b"data");
    put(&data_bytes.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_share_a_timeline_and_files_are_valid_wav() {
        let dir = std::env::temp_dir().join(format!("blurt-recorder-test-{}", std::process::id()));
        {
            let mut recorder = Recorder::start(&dir).unwrap();
            let ones = [1.0; FRAME_SAMPLES];
            recorder.push(Track::MicRaw, &ones);
            thread::sleep(Duration::from_millis(30));
            // Starts ~30 ms later, so it's padded with ~20 ms of silence
            // (the time less the frame itself).
            recorder.push(Track::Playback, &ones);
        }
        let read = |track: Track| std::fs::read(dir.join(track.file_name())).unwrap();
        let mic = read(Track::MicRaw);
        let playback = read(Track::Playback);
        assert_eq!(&mic[..4], b"RIFF");
        assert_eq!(&mic[8..16], b"WAVEfmt ");
        let data_len = |wav: &[u8]| u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len(&mic), FRAME_SAMPLES * 4);
        assert_eq!(mic.len(), 44 + FRAME_SAMPLES * 4);
        let padded = data_len(&playback) / 4 - FRAME_SAMPLES;
        assert!(
            (19 * 48..40 * 48).contains(&padded),
            "padded {padded} samples"
        );
        // Untouched tracks are still valid, empty files.
        assert_eq!(data_len(&read(Track::Sent)), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

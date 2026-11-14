//! The microphone side: clean up each 10 ms frame, decide whether to send
//! it, and encode it.
//!
//! Order matters: echo cancellation needs the raw (linear) mic signal, so it
//! runs first; RNNoise runs on its output; the gain controller is part of the
//! same sonora pass as echo cancellation and runs after it.

use sonora::config::{AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::codec::{Encoder, EncoderConfig};
use crate::{FRAME_SAMPLES, SAMPLE_RATE, VoiceSink};

/// How long to keep sending after speech stops, so pauses between words and
/// quiet word endings aren't cut off.
const VOICE_HOLD_FRAMES: usize = 25;
/// The same after push-to-talk is released, for a word still trailing off.
const PTT_HOLD_FRAMES: usize = 5;
/// Frames kept from before speech was detected, and sent at its start, so
/// the first syllable isn't clipped. Costs no latency: they're already late.
const PREROLL_FRAMES: usize = 2;

/// When to transmit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransmitMode {
    /// When the voice detector hears speech with at least this probability.
    VoiceActivity {
        threshold: f32,
    },
    PushToTalk,
    Continuous,
}

impl Default for TransmitMode {
    fn default() -> Self {
        Self::VoiceActivity { threshold: 0.5 }
    }
}

/// Which cleanup stages run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Processing {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub auto_gain: bool,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain: true,
        }
    }
}

fn sonora_config(processing: Processing) -> Config {
    Config {
        // Removes rumble and DC offset; AEC enables it anyway.
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: processing.echo_cancellation.then(EchoCanceller::default),
        // RNNoise does noise suppression instead; it sounds better on speech.
        noise_suppression: None,
        // The default controller only limits; the adaptive part is what
        // raises a quiet mic.
        gain_controller2: processing.auto_gain.then(|| GainController2 {
            adaptive_digital: Some(AdaptiveDigital::default()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// What happened to one mic frame, for meters and indicators.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameReport {
    /// The cleaned-up frame's level, in dBFS.
    pub level_db: f32,
    /// The voice detector's speech probability, 0 to 1.
    pub speech: f32,
    pub transmitting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transmit {
    Skip,
    Start,
    Continue,
    End,
}

/// Decides which frames to send: wanted ones, plus a hold after them.
#[derive(Default)]
struct Gate {
    /// Frames since the last wanted one, while transmitting.
    since_wanted: Option<usize>,
}

impl Gate {
    fn update(&mut self, wanted: bool, hold: usize) -> Transmit {
        match (self.since_wanted, wanted) {
            (None, false) => Transmit::Skip,
            (None, true) => {
                self.since_wanted = Some(0);
                Transmit::Start
            }
            (Some(_), true) => {
                self.since_wanted = Some(0);
                Transmit::Continue
            }
            (Some(n), false) if n + 1 >= hold => {
                self.since_wanted = None;
                Transmit::End
            }
            (Some(n), false) => {
                self.since_wanted = Some(n + 1);
                Transmit::Continue
            }
        }
    }

    fn is_open(&self) -> bool {
        self.since_wanted.is_some()
    }
}

pub struct Capture {
    apm: AudioProcessing,
    processing: Processing,
    denoiser: Box<nnnoiseless::DenoiseState<'static>>,
    vad: sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper,
    gate: Gate,
    pub mode: TransmitMode,
    encoder: Encoder,
    frames_per_packet: usize,
    /// Processed frames from before the gate opened.
    preroll: [[f32; FRAME_SAMPLES]; PREROLL_FRAMES],
    preroll_len: usize,
    /// Frames waiting to fill a packet.
    pending: [f32; 2 * FRAME_SAMPLES],
    pending_frames: usize,
    pending_first: u64,
    /// Counts every 10 ms frame captured, sent or not: Mumble's frame number.
    frame_counter: u64,
    packet: [u8; 1500],
    scratch: [f32; FRAME_SAMPLES],
}

impl Capture {
    pub fn new(
        processing: Processing,
        mode: TransmitMode,
        encoder: EncoderConfig,
    ) -> Result<Self, opus::Error> {
        let stream = StreamConfig::new(SAMPLE_RATE, 1);
        let apm = AudioProcessing::builder()
            .config(sonora_config(processing))
            .capture_config(stream)
            .render_config(stream)
            .build();
        Ok(Self {
            apm,
            processing,
            denoiser: nnnoiseless::DenoiseState::new(),
            vad: sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper::new(
                sonora_simd::detect_backend(),
                SAMPLE_RATE as i32,
            ),
            gate: Gate::default(),
            mode,
            encoder: Encoder::new(encoder)?,
            frames_per_packet: encoder.frames_per_packet.clamp(1, 2),
            preroll: [[0.0; FRAME_SAMPLES]; PREROLL_FRAMES],
            preroll_len: 0,
            pending: [0.0; 2 * FRAME_SAMPLES],
            pending_frames: 0,
            pending_first: 0,
            frame_counter: 0,
            packet: [0; 1500],
            scratch: [0.0; FRAME_SAMPLES],
        })
    }

    pub fn set_processing(&mut self, processing: Processing) {
        if processing != self.processing {
            self.processing = processing;
            self.apm.apply_config(sonora_config(processing));
        }
    }

    pub fn set_bitrate(&mut self, bitrate: i32) {
        if let Err(e) = self.encoder.set_bitrate(bitrate) {
            log::warn!("Could not set the Opus bitrate to {bitrate}: {e}");
        }
    }

    /// The delay between a frame being given to [`render`](Self::render)
    /// and its echo reaching [`capture`](Self::capture). The echo canceller
    /// also estimates it, but starts better from a good guess.
    pub fn set_echo_delay_ms(&mut self, ms: i32) {
        // Clamped to sonora's range; nothing to do about that error.
        let _ = self.apm.set_stream_delay_ms(ms);
    }

    /// Gives the echo canceller what is about to be played.
    pub fn render(&mut self, frame: &[f32; FRAME_SAMPLES]) {
        if !self.processing.echo_cancellation {
            return;
        }
        if let Err(e) = self
            .apm
            .process_render_f32(&[frame], &mut [&mut self.scratch])
        {
            log::debug!("Echo canceller render failed: {e}");
        }
    }

    /// Processes one 10 ms mic frame and sends it if the gate is open.
    /// `talk` is push-to-talk's state; `muted` closes the gate.
    pub fn capture(
        &mut self,
        mic: &[f32; FRAME_SAMPLES],
        talk: bool,
        muted: bool,
        sink: &mut impl VoiceSink,
    ) -> FrameReport {
        if muted {
            return self.capture_muted(mic, sink);
        }
        let mut frame = [0.0; FRAME_SAMPLES];
        if let Err(e) = self.apm.process_capture_f32(&[mic], &mut [&mut frame]) {
            log::debug!("Echo canceller capture failed: {e}");
            frame = *mic;
        }
        if self.processing.noise_suppression {
            // RNNoise works on 16-bit-scaled samples.
            let scaled: [f32; FRAME_SAMPLES] = std::array::from_fn(|i| frame[i] * 32768.0);
            self.denoiser.process_frame(&mut frame, &scaled);
            for sample in &mut frame {
                *sample /= 32768.0;
            }
        }

        // Like WebRTC, the detector expects 16-bit-scaled samples too.
        let scaled: [f32; FRAME_SAMPLES] = std::array::from_fn(|i| frame[i] * 32768.0);
        let speech = self.vad.analyze(&scaled);

        let (wanted, hold) = match self.mode {
            TransmitMode::VoiceActivity { threshold } => (speech >= threshold, VOICE_HOLD_FRAMES),
            TransmitMode::PushToTalk => (talk, PTT_HOLD_FRAMES),
            TransmitMode::Continuous => (true, 1),
        };
        let decision = self.gate.update(wanted, hold);

        let frame_number = self.frame_counter;
        self.frame_counter += 1;
        match decision {
            Transmit::Skip => self.keep_for_preroll(&frame),
            Transmit::Start => {
                if let Err(e) = self.encoder.reset() {
                    log::warn!("Opus reset failed: {e}");
                }
                let preroll = self.preroll_len;
                for i in 0..preroll {
                    let earlier = self.preroll[i];
                    let number = frame_number - (preroll - i) as u64;
                    self.queue(&earlier, number, false, sink);
                }
                self.preroll_len = 0;
                self.queue(&frame, frame_number, false, sink);
            }
            Transmit::Continue => self.queue(&frame, frame_number, false, sink),
            Transmit::End => self.queue(&frame, frame_number, true, sink),
        }

        FrameReport {
            level_db: level_db(&frame),
            speech,
            transmitting: decision != Transmit::Skip,
        }
    }

    /// Muted, nothing is sent, so none of the processing is needed (which
    /// is most of the engine's CPU use). A spurt in progress still ends
    /// properly, with a terminator, rather than just going silent.
    fn capture_muted(
        &mut self,
        mic: &[f32; FRAME_SAMPLES],
        sink: &mut impl VoiceSink,
    ) -> FrameReport {
        let frame_number = self.frame_counter;
        self.frame_counter += 1;
        self.preroll_len = 0;
        let ending = self.gate.is_open();
        if ending {
            self.gate = Gate::default();
            // Silence, not the unprocessed mic: muting should cut off at once.
            self.queue(&[0.0; FRAME_SAMPLES], frame_number, true, sink);
        }
        FrameReport {
            level_db: level_db(mic),
            speech: 0.0,
            transmitting: ending,
        }
    }

    fn keep_for_preroll(&mut self, frame: &[f32; FRAME_SAMPLES]) {
        if self.preroll_len == PREROLL_FRAMES {
            self.preroll.rotate_left(1);
            self.preroll_len -= 1;
        }
        self.preroll[self.preroll_len] = *frame;
        self.preroll_len += 1;
    }

    /// Adds a frame to the packet being built, sending it once full (or at
    /// the end of a spurt).
    fn queue(
        &mut self,
        frame: &[f32; FRAME_SAMPLES],
        frame_number: u64,
        last: bool,
        sink: &mut impl VoiceSink,
    ) {
        if self.pending_frames == 0 {
            self.pending_first = frame_number;
        }
        let at = self.pending_frames * FRAME_SAMPLES;
        self.pending[at..at + FRAME_SAMPLES].copy_from_slice(frame);
        self.pending_frames += 1;
        if self.pending_frames < self.frames_per_packet && !last {
            return;
        }
        let samples = self.pending_frames * FRAME_SAMPLES;
        self.pending_frames = 0;
        match self
            .encoder
            .encode(&self.pending[..samples], &mut self.packet)
        {
            Ok(len) => sink.send(&self.packet[..len], self.pending_first, last),
            Err(e) => log::warn!("Opus encode failed: {e}"),
        }
    }
}

/// RMS level in dBFS, floored at -100.
pub fn level_db(frame: &[f32]) -> f32 {
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    (10.0 * mean_square.log10()).max(-100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Sent(Vec<(usize, u64, bool)>);

    impl VoiceSink for &'static std::sync::Mutex<Sent> {
        fn send(&mut self, opus: &[u8], frame_number: u64, is_terminator: bool) {
            self.lock()
                .unwrap()
                .0
                .push((opus.len(), frame_number, is_terminator));
        }
    }

    fn sink() -> &'static std::sync::Mutex<Sent> {
        Box::leak(Box::default())
    }

    /// Deterministic noise in [-1, 1].
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 23) as f32) * 2.0 - 1.0
        }
        fn frame(&mut self, gain: f32) -> [f32; FRAME_SAMPLES] {
            std::array::from_fn(|_| self.next() * gain)
        }
    }

    fn capture(mode: TransmitMode, frames_per_packet: usize) -> Capture {
        Capture::new(
            Processing {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain: false,
            },
            mode,
            EncoderConfig {
                frames_per_packet,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn push_to_talk_sends_while_held_plus_a_short_hold() {
        let mut capture = capture(TransmitMode::PushToTalk, 1);
        let sent = sink();
        let mut out = sent;
        let quiet = [0.0; FRAME_SAMPLES];
        for i in 0..20 {
            // Held for frames 5..10.
            capture.capture(&quiet, (5..10).contains(&i), false, &mut out);
        }
        let sent = &sent.lock().unwrap().0;
        let numbers: Vec<u64> = sent.iter().map(|s| s.1).collect();
        // Two preroll frames, the five held, then the hold after release.
        assert_eq!(numbers, (3..15).collect::<Vec<_>>());
        assert!(sent.last().unwrap().2, "ends with a terminator");
        assert_eq!(sent.iter().filter(|s| s.2).count(), 1);
    }

    #[test]
    fn twenty_ms_packets_carry_the_first_frames_number() {
        let mut capture = capture(TransmitMode::Continuous, 2);
        let sent = sink();
        let mut out = sent;
        let quiet = [0.0; FRAME_SAMPLES];
        for _ in 0..6 {
            capture.capture(&quiet, false, false, &mut out);
        }
        let numbers: Vec<u64> = sent.lock().unwrap().0.iter().map(|s| s.1).collect();
        assert_eq!(numbers, [0, 2, 4]);
    }

    #[test]
    fn muting_ends_the_spurt_with_a_terminator() {
        let mut capture = capture(TransmitMode::Continuous, 1);
        let sent = sink();
        let mut out = sent;
        let quiet = [0.0; FRAME_SAMPLES];
        for i in 0..10 {
            capture.capture(&quiet, false, i >= 5, &mut out);
        }
        let sent = &sent.lock().unwrap().0;
        assert_eq!(sent.len(), 6);
        assert!(sent[5].2);
    }

    #[test]
    fn voice_activity_ignores_silence() {
        let mut capture = capture(TransmitMode::default(), 1);
        let sent = sink();
        let mut out = sent;
        for _ in 0..200 {
            capture.capture(&[0.0; FRAME_SAMPLES], false, false, &mut out);
        }
        assert!(sent.lock().unwrap().0.is_empty());
    }

    #[test]
    fn noise_suppression_quiets_steady_noise() {
        let mut capture = Capture::new(
            Processing {
                echo_cancellation: false,
                noise_suppression: true,
                auto_gain: false,
            },
            TransmitMode::Continuous,
            EncoderConfig::default(),
        )
        .unwrap();
        let mut noise = Noise(1);
        let mut out = |_: &[u8], _: u64, _: bool| {};
        let (mut noise_energy, mut out_energy) = (0.0f32, 0.0f32);
        for i in 0..300 {
            // About -36 dBFS: a fan or a busy room.
            let frame = noise.frame(0.01);
            let report = capture.capture(&frame, false, false, &mut out);
            // Judge the last second, once RNNoise has settled.
            if i >= 200 {
                noise_energy += 10f32.powf(level_db(&frame) / 10.0);
                out_energy += 10f32.powf(report.level_db / 10.0);
            }
        }
        let reduction_db = 10.0 * (noise_energy / out_energy).log10();
        assert!(reduction_db > 25.0, "noise reduced by {reduction_db:.1} dB");
    }

    #[test]
    fn echo_cancellation_removes_what_was_played() {
        let mut capture = Capture::new(
            Processing {
                echo_cancellation: true,
                noise_suppression: false,
                auto_gain: false,
            },
            TransmitMode::Continuous,
            EncoderConfig::default(),
        )
        .unwrap();
        // The mic hears what was played 3 frames (30 ms) ago, quieter.
        const DELAY: usize = 3;
        capture.set_echo_delay_ms((DELAY * 10) as i32);
        let mut noise = Noise(7);
        let mut played: Vec<[f32; FRAME_SAMPLES]> = Vec::new();
        let mut out = |_: &[u8], _: u64, _: bool| {};
        let (mut echo_energy, mut out_energy) = (0.0f32, 0.0f32);
        for i in 0..500usize {
            let far = noise.frame(0.3);
            capture.render(&far);
            played.push(far);
            let mic: [f32; FRAME_SAMPLES] = match i.checked_sub(DELAY) {
                Some(j) => played[j].map(|s| s * 0.5),
                None => [0.0; FRAME_SAMPLES],
            };
            let report = capture.capture(&mic, false, false, &mut out);
            // Judge the last second, once the canceller has converged.
            if i >= 400 {
                echo_energy += 10f32.powf(level_db(&mic) / 10.0);
                out_energy += 10f32.powf(report.level_db / 10.0);
            }
        }
        let reduction_db = 10.0 * (echo_energy / out_energy).log10();
        assert!(reduction_db > 20.0, "echo reduced by {reduction_db:.1} dB");
    }
}

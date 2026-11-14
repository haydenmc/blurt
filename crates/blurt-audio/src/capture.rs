//! The microphone side: clean up each 10 ms frame, decide whether to send
//! it, and encode it.
//!
//! Order matters: echo cancellation needs the raw (linear) mic signal, so it
//! runs first; noise suppression next; gain control last, so it raises the
//! voice rather than the noise. With WebRTC noise suppression all three are
//! one sonora pass, in that order. RNNoise isn't part of sonora, so with it
//! the gain controller runs in a second pass after RNNoise.

use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppressionLevel,
};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::codec::{Encoder, EncoderConfig};
use crate::{FRAME_SAMPLES, SAMPLE_RATE, VoiceSink};

/// How long to keep sending after speech stops, so pauses between words and
/// quiet word endings aren't cut off.
const VOICE_HOLD_FRAMES: usize = 25;
/// The same after push-to-talk is released, for a word still trailing off.
const PTT_HOLD_FRAMES: usize = 5;
/// Frames from before speech was detected that are sent at its start, so
/// the first syllable isn't clipped. Up to the receiver's initial jitter
/// delay, this costs no latency: it would have waited for them anyway.
const PREROLL_FRAMES: usize = 2;
/// How far back the gate can reach, when speech is confirmed, to where it
/// began (see [`OnsetDetector`]).
const LOOKBACK_FRAMES: usize = 15;

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

/// Which noise suppressor runs, and how hard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NoiseSuppression {
    Off,
    /// WebRTC's suppressor, about 6 dB.
    Low,
    /// About 12 dB.
    Moderate,
    /// About 18 dB. Keeps the starts of words intact.
    #[default]
    High,
    /// About 21 dB.
    VeryHigh,
    /// RNNoise: removes more kinds of noise (keyboards, fans with tones),
    /// but also softens the first few tens of milliseconds of words.
    Rnnoise,
}

/// Which cleanup stages run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Processing {
    pub echo_cancellation: bool,
    pub noise_suppression: NoiseSuppression,
    pub auto_gain: bool,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: NoiseSuppression::default(),
            auto_gain: true,
        }
    }
}

impl Processing {
    /// Everything off: the mic as it is.
    pub const NONE: Self = Self {
        echo_cancellation: false,
        noise_suppression: NoiseSuppression::Off,
        auto_gain: false,
    };

    fn rnnoise(self) -> bool {
        self.noise_suppression == NoiseSuppression::Rnnoise
    }
}

fn gain_controller(processing: Processing) -> Option<GainController2> {
    // The default controller only limits; the adaptive part is what raises
    // a quiet mic.
    processing.auto_gain.then(|| GainController2 {
        adaptive_digital: Some(AdaptiveDigital::default()),
        ..Default::default()
    })
}

/// The main sonora pass: high-pass, echo cancellation, WebRTC noise
/// suppression, and gain control unless RNNoise has to run first.
fn main_config(processing: Processing) -> Config {
    let level = match processing.noise_suppression {
        NoiseSuppression::Off | NoiseSuppression::Rnnoise => None,
        NoiseSuppression::Low => Some(NoiseSuppressionLevel::Low),
        NoiseSuppression::Moderate => Some(NoiseSuppressionLevel::Moderate),
        NoiseSuppression::High => Some(NoiseSuppressionLevel::High),
        NoiseSuppression::VeryHigh => Some(NoiseSuppressionLevel::VeryHigh),
    };
    Config {
        // Removes rumble and DC offset; AEC enables it anyway.
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: processing.echo_cancellation.then(EchoCanceller::default),
        noise_suppression: level.map(|level| sonora::config::NoiseSuppression {
            level,
            ..Default::default()
        }),
        gain_controller2: if processing.rnnoise() {
            None
        } else {
            gain_controller(processing)
        },
        ..Default::default()
    }
}

/// The second pass, after RNNoise: gain control only.
fn gain_pass(processing: Processing) -> Option<AudioProcessing> {
    if !processing.rnnoise() || !processing.auto_gain {
        return None;
    }
    let stream = StreamConfig::new(SAMPLE_RATE, 1);
    Some(
        AudioProcessing::builder()
            .config(Config {
                gain_controller2: gain_controller(processing),
                ..Default::default()
            })
            .capture_config(stream)
            .render_config(stream)
            .build(),
    )
}

/// The echo canceller's statistics, for diagnostics. Each is `None` until
/// it has enough to go on.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EchoStats {
    /// How much quieter the echo is than what was played (speakers to
    /// mic), in dB.
    pub return_loss_db: Option<f64>,
    /// How much the canceller removed of the echo that reached the mic,
    /// in dB. Higher is better; under ~10 dB, echo is getting through.
    pub return_loss_enhancement_db: Option<f64>,
    /// How often its filter diverged in the last second (0 to 1).
    pub divergent_fraction: Option<f64>,
    /// Its estimate of the delay from playing to hearing the echo.
    pub delay_ms: Option<i32>,
    /// How likely it is that echo remains after cancellation (0 to 1).
    pub residual_echo_likelihood: Option<f64>,
}

/// What happened to one mic frame, for meters and indicators.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameReport {
    /// The cleaned-up frame's level, in dBFS.
    pub level_db: f32,
    /// The voice detector's speech probability, 0 to 1.
    pub speech: f32,
    pub transmitting: bool,
    /// Earlier frames sent along with this one because speech was confirmed
    /// here (preroll, or back to where it began). The receiver plays them
    /// first, so this many frames' latency is added for the utterance,
    /// less its initial jitter delay.
    pub backlog: usize,
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

/// Spots where speech may have begun: a sudden rise in the processed level
/// (steady noise is already gone) against a tracked noise floor.
///
/// It never sends anything itself. The voice detector needs a few frames to
/// be sure of speech; once it is, the gate reaches back to the onset, so the
/// start of the word isn't clipped. A bang or slam that the detector never
/// confirms is never sent.
struct OnsetDetector {
    floor_db: f32,
    /// Frames since the earliest onset still within reach of the gate.
    age: Option<usize>,
}

impl OnsetDetector {
    /// How far above the floor counts as an onset.
    const MARGIN_DB: f32 = 20.0;
    /// And never below this, so near-silence doesn't count.
    const MIN_DB: f32 = -55.0;
    /// The floor follows quieter levels at once and louder ones slowly
    /// (1.5 dB a second), so speech doesn't raise it.
    const RISE_DB_PER_FRAME: f32 = 0.015;

    fn new() -> Self {
        Self {
            floor_db: -70.0,
            age: None,
        }
    }

    fn update(&mut self, level_db: f32) {
        let onset = level_db >= (self.floor_db + Self::MARGIN_DB).max(Self::MIN_DB);
        self.floor_db = if level_db < self.floor_db {
            level_db
        } else {
            self.floor_db + Self::RISE_DB_PER_FRAME
        };
        self.age = match self.age {
            Some(age) if age + 1 < LOOKBACK_FRAMES => Some(age + 1),
            _ if onset => Some(0),
            _ => None,
        };
    }

    /// How many earlier frames to send with the frame that confirmed speech:
    /// back to just before the onset, or the usual preroll.
    fn lookback(&self) -> usize {
        self.age
            .map_or(PREROLL_FRAMES, |age| (age + 1).max(PREROLL_FRAMES))
    }

    fn reset(&mut self) {
        self.age = None;
    }
}

pub struct Capture {
    apm: AudioProcessing,
    /// Gain control after RNNoise, when RNNoise is in use.
    gain: Option<AudioProcessing>,
    processing: Processing,
    denoiser: Box<nnnoiseless::DenoiseState<'static>>,
    vad: sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper,
    gate: Gate,
    onset: OnsetDetector,
    pub mode: TransmitMode,
    encoder: Encoder,
    frames_per_packet: usize,
    /// Processed frames from before the gate opened, oldest first.
    history: [[f32; FRAME_SAMPLES]; LOOKBACK_FRAMES],
    history_len: usize,
    /// Frames waiting to fill a packet.
    pending: [f32; 2 * FRAME_SAMPLES],
    pending_frames: usize,
    pending_first: u64,
    /// Counts every 10 ms frame captured, sent or not: Mumble's frame number.
    frame_counter: u64,
    packet: [u8; 1500],
    scratch: [f32; FRAME_SAMPLES],
    /// The last frame after all processing, before the gate.
    processed: [f32; FRAME_SAMPLES],
}

impl Capture {
    pub fn new(
        processing: Processing,
        mode: TransmitMode,
        encoder: EncoderConfig,
    ) -> Result<Self, opus::Error> {
        let stream = StreamConfig::new(SAMPLE_RATE, 1);
        let apm = AudioProcessing::builder()
            .config(main_config(processing))
            .capture_config(stream)
            .render_config(stream)
            .build();
        Ok(Self {
            apm,
            gain: gain_pass(processing),
            processing,
            denoiser: nnnoiseless::DenoiseState::new(),
            vad: sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper::new(
                sonora_simd::detect_backend(),
                SAMPLE_RATE as i32,
            ),
            gate: Gate::default(),
            onset: OnsetDetector::new(),
            mode,
            encoder: Encoder::new(encoder)?,
            frames_per_packet: encoder.frames_per_packet.clamp(1, 2),
            history: [[0.0; FRAME_SAMPLES]; LOOKBACK_FRAMES],
            history_len: 0,
            pending: [0.0; 2 * FRAME_SAMPLES],
            pending_frames: 0,
            pending_first: 0,
            frame_counter: 0,
            packet: [0; 1500],
            scratch: [0.0; FRAME_SAMPLES],
            processed: [0.0; FRAME_SAMPLES],
        })
    }

    pub fn set_processing(&mut self, processing: Processing) {
        if processing != self.processing {
            self.apm.apply_config(main_config(processing));
            // Only rebuilt when whether it's needed changes, so its gain
            // doesn't start over.
            if processing.rnnoise() != self.processing.rnnoise()
                || processing.auto_gain != self.processing.auto_gain
            {
                self.gain = gain_pass(processing);
            }
            self.processing = processing;
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

    /// The last mic frame after all processing, before the gate decided
    /// whether to send it. Silence while muted.
    pub fn processed(&self) -> &[f32; FRAME_SAMPLES] {
        &self.processed
    }

    /// The echo canceller's view of how it's doing; `None` while echo
    /// cancellation is off.
    pub fn echo_stats(&self) -> Option<EchoStats> {
        if !self.processing.echo_cancellation {
            return None;
        }
        let stats = self.apm.statistics();
        Some(EchoStats {
            return_loss_db: stats.echo_return_loss,
            return_loss_enhancement_db: stats.echo_return_loss_enhancement,
            divergent_fraction: stats.divergent_filter_fraction,
            delay_ms: stats.delay_ms,
            residual_echo_likelihood: stats.residual_echo_likelihood,
        })
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
        if self.processing.rnnoise() {
            // RNNoise works on 16-bit-scaled samples.
            let scaled: [f32; FRAME_SAMPLES] = std::array::from_fn(|i| frame[i] * 32768.0);
            self.denoiser.process_frame(&mut frame, &scaled);
            for sample in &mut frame {
                *sample /= 32768.0;
            }
            if let Some(gain) = &mut self.gain {
                let input = frame;
                if let Err(e) = gain.process_capture_f32(&[&input], &mut [&mut frame]) {
                    log::debug!("Gain control failed: {e}");
                    frame = input;
                }
            }
        }

        // Like WebRTC, the detector expects 16-bit-scaled samples too.
        let scaled: [f32; FRAME_SAMPLES] = std::array::from_fn(|i| frame[i] * 32768.0);
        let speech = self.vad.analyze(&scaled);
        let level = level_db(&frame);
        self.onset.update(level);

        let (wanted, hold, lookback) = match self.mode {
            TransmitMode::VoiceActivity { threshold } => (
                speech >= threshold,
                VOICE_HOLD_FRAMES,
                self.onset.lookback(),
            ),
            TransmitMode::PushToTalk => (talk, PTT_HOLD_FRAMES, PREROLL_FRAMES),
            TransmitMode::Continuous => (true, 1, 0),
        };
        let decision = self.gate.update(wanted, hold);

        let frame_number = self.frame_counter;
        self.frame_counter += 1;
        let mut backlog = 0;
        match decision {
            Transmit::Skip => self.remember(&frame),
            Transmit::Start => {
                if let Err(e) = self.encoder.reset() {
                    log::warn!("Opus reset failed: {e}");
                }
                backlog = lookback.min(self.history_len);
                let first = self.history_len - backlog;
                for i in first..self.history_len {
                    let earlier = self.history[i];
                    let number = frame_number - (self.history_len - i) as u64;
                    self.queue(&earlier, number, false, sink);
                }
                self.history_len = 0;
                self.onset.reset();
                self.queue(&frame, frame_number, false, sink);
            }
            Transmit::Continue => self.queue(&frame, frame_number, false, sink),
            Transmit::End => self.queue(&frame, frame_number, true, sink),
        }

        self.processed = frame;
        FrameReport {
            level_db: level,
            speech,
            transmitting: decision != Transmit::Skip,
            backlog,
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
        self.history_len = 0;
        self.onset.reset();
        self.processed = [0.0; FRAME_SAMPLES];
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
            backlog: 0,
        }
    }

    /// Keeps a frame the gate didn't send, in case speech turns out to have
    /// started there.
    fn remember(&mut self, frame: &[f32; FRAME_SAMPLES]) {
        if self.history_len == LOOKBACK_FRAMES {
            self.history.rotate_left(1);
            self.history_len -= 1;
        }
        self.history[self.history_len] = *frame;
        self.history_len += 1;
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
            Processing::NONE,
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

    /// How much `suppression` (with gain control on, as by default) reduces
    /// steady noise at about -36 dBFS, a fan or a busy room.
    fn noise_reduction_db(suppression: NoiseSuppression) -> f32 {
        let mut capture = Capture::new(
            Processing {
                noise_suppression: suppression,
                auto_gain: true,
                ..Processing::NONE
            },
            TransmitMode::Continuous,
            EncoderConfig::default(),
        )
        .unwrap();
        let mut noise = Noise(1);
        let mut out = |_: &[u8], _: u64, _: bool| {};
        let (mut noise_energy, mut out_energy) = (0.0f32, 0.0f32);
        for i in 0..500 {
            let frame = noise.frame(0.01);
            let report = capture.capture(&frame, false, false, &mut out);
            // Judge the last second, once everything has settled.
            if i >= 400 {
                noise_energy += 10f32.powf(level_db(&frame) / 10.0);
                out_energy += 10f32.powf(report.level_db / 10.0);
            }
        }
        10.0 * (noise_energy / out_energy).log10()
    }

    #[test]
    fn noise_suppression_quiets_steady_noise() {
        let none = noise_reduction_db(NoiseSuppression::Off);
        let high = noise_reduction_db(NoiseSuppression::High);
        let rnnoise = noise_reduction_db(NoiseSuppression::Rnnoise);
        // Measured against no suppression, so the gain control's own
        // effect on noise cancels out. On loud steady noise WebRTC's
        // suppressor does far less than its nominal 18 dB (about 8 dB here),
        // which is why RNNoise (about 40 dB) is offered for noisy rooms.
        assert!(high - none > 6.0, "High: {:.1} dB", high - none);
        assert!(rnnoise - none > 30.0, "RNNoise: {:.1} dB", rnnoise - none);
    }

    /// A slam: a broadband crack decaying over ~80 ms, with a low thump.
    fn bang(at: usize, i: usize, noise: &mut Noise) -> [f32; FRAME_SAMPLES] {
        std::array::from_fn(|n| {
            let t = (i.wrapping_sub(at) * FRAME_SAMPLES + n) as f32 / SAMPLE_RATE as f32;
            if i < at || t > 0.4 {
                return 0.0;
            }
            let decay = (-t / 0.08).exp();
            0.5 * decay * (noise.next() + (2.0 * std::f32::consts::PI * 70.0 * t).sin())
        })
    }

    #[test]
    #[ignore = "known gap: the voice detector takes slams for speech; see the reprocess example's --inject-noises"]
    fn a_bang_alone_is_never_sent() {
        let mut capture = Capture::new(
            Processing::default(),
            TransmitMode::default(),
            EncoderConfig::default(),
        )
        .unwrap();
        let sent = sink();
        let mut out = sent;
        let mut noise = Noise(3);
        for i in 0..300 {
            let mic = bang(150, i, &mut noise);
            capture.capture(&mic, false, false, &mut out);
        }
        let sent = &sent.lock().unwrap().0;
        assert!(sent.is_empty(), "sent {} frames of a bang", sent.len());
    }

    #[test]
    fn the_gate_reaches_back_to_where_speech_began() {
        let mut onset = OnsetDetector::new();
        for _ in 0..50 {
            onset.update(-80.0);
        }
        // Nothing has started: the usual preroll.
        assert_eq!(onset.lookback(), PREROLL_FRAMES);
        // A word starts, and the detector confirms it 6 frames later.
        for _ in 0..6 {
            onset.update(-30.0);
        }
        // Back to the frame before the onset: 5 more speech frames, plus 1.
        assert_eq!(onset.lookback(), 6);
        // Past the lookback window, it's forgotten.
        for _ in 0..LOOKBACK_FRAMES {
            onset.update(-80.0);
        }
        assert_eq!(onset.lookback(), PREROLL_FRAMES);
    }

    #[test]
    fn echo_cancellation_removes_what_was_played() {
        let mut capture = Capture::new(
            Processing {
                echo_cancellation: true,
                ..Processing::NONE
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

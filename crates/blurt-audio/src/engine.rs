//! Runs the pipeline against real devices.
//!
//! Device callbacks only move samples through lock-free ring buffers (no
//! locks, no allocation). One DSP thread does everything else, woken by the
//! callbacks, by incoming voice and by controls: it mixes enough playback to
//! keep the output ring at its target level (which follows the device's
//! clock), then processes whatever mic audio has arrived. Controls reach it
//! through atomics and a command queue.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::capture::{Capture, Processing, TransmitMode};
use crate::codec::EncoderConfig;
use crate::jitter::JitterConfig;
use crate::level::{self, Envelope};
use crate::mixer::{Mixer, SpeakerSettings};
use crate::recorder::{Recorder, Track};
use crate::resample::Resampler;
use crate::{Closed, FRAME_SAMPLES, SAMPLE_RATE, VoiceSink, VoiceSource};

/// The DSP thread sleeps until there is work: the mic callback delivered
/// audio, the speaker callback made room, or a voice packet or control
/// arrived (see [`Waker`]). This timeout is only a backstop, for a missed
/// wakeup or a stalled device, and short enough that even then playback
/// (always at least 10 ms queued) doesn't run dry.
const FALLBACK_WAKE: Duration = Duration::from_millis(5);

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no {0} device")]
    NoDevice(&'static str),
    #[error("unknown audio device {0:?}")]
    UnknownDevice(String),
    #[error("audio host {0:?} isn't available (see `hosts()`)")]
    UnknownHost(String),
    #[error("unsupported sample format {0:?}")]
    UnsupportedFormat(cpal::SampleFormat),
    #[error("audio device error: {0}")]
    Device(#[from] cpal::Error),
    #[error("Opus error: {0}")]
    Opus(#[from] opus::Error),
    #[error("resampler error: {0}")]
    Resampler(#[from] rubato::ResamplerConstructionError),
    #[error("could not record audio: {0}")]
    Recording(#[from] std::io::Error),
    #[error("the audio thread stopped unexpectedly")]
    ThreadDied,
}

/// An audio device, as listed for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable across runs; pass it to [`EngineConfig`].
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// The audio hosts (system audio APIs) available here, e.g. "PipeWire",
/// "PulseAudio", "ALSA". Any of them can go in [`EngineConfig::host`].
pub fn hosts() -> Vec<&'static str> {
    cpal::available_hosts().iter().map(|id| id.name()).collect()
}

/// `None` is the platform's preferred host.
fn host(name: Option<&str>) -> Result<cpal::Host, AudioError> {
    let Some(name) = name else {
        return Ok(cpal::default_host());
    };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| AudioError::UnknownHost(name.to_string()))?;
    Ok(cpal::host_from_id(id)?)
}

/// Device ids are only meaningful to the host they were listed from.
pub fn input_devices(host_name: Option<&str>) -> Result<Vec<DeviceInfo>, AudioError> {
    let host = host(host_name)?;
    let default = host.default_input_device().and_then(|d| d.id().ok());
    list(host.input_devices()?, default)
}

pub fn output_devices(host_name: Option<&str>) -> Result<Vec<DeviceInfo>, AudioError> {
    let host = host(host_name)?;
    let default = host.default_output_device().and_then(|d| d.id().ok());
    list(host.output_devices()?, default)
}

fn list(
    devices: impl Iterator<Item = cpal::Device>,
    default: Option<cpal::DeviceId>,
) -> Result<Vec<DeviceInfo>, AudioError> {
    Ok(devices
        .filter_map(|device| {
            let id = device.id().ok()?;
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| id.to_string());
            Some(DeviceInfo {
                is_default: Some(&id) == default.as_ref(),
                id: id.to_string(),
                name,
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The audio host, by name (see [`hosts`]). `None` is the platform's
    /// preferred one.
    pub host: Option<String>,
    /// `None` follows the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub processing: Processing,
    pub mode: TransmitMode,
    pub encoder: EncoderConfig,
    pub jitter: JitterConfig,
    /// The device buffer to ask for, in frames. Smaller is lower latency but
    /// risks dropouts on a busy machine.
    pub device_buffer_frames: u32,
    /// How much mixed audio to keep queued for the output device.
    pub playback_buffer: Duration,
    /// For diagnosing audio problems: record the raw mic, the processed mic,
    /// what was sent and what was played into WAV files in this directory,
    /// all on one timeline.
    pub record_to: Option<std::path::PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            host: None,
            input_device: None,
            output_device: None,
            processing: Processing::default(),
            mode: TransmitMode::default(),
            encoder: EncoderConfig::default(),
            jitter: JitterConfig::default(),
            // 5 ms.
            device_buffer_frames: 240,
            playback_buffer: Duration::from_millis(10),
            record_to: None,
        }
    }
}

/// A snapshot of the engine's state, for meters and diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EngineStats {
    /// The cleaned-up mic level, in dBFS.
    pub mic_level_db: f32,
    /// The cleaned-up mic level, smoothed for meters (see [`crate::level`]).
    pub mic_level: f32,
    /// The voice detector's speech probability, 0 to 1.
    pub speech: f32,
    pub transmitting: bool,
    /// Speakers in the last mixed frame.
    pub speakers: usize,
    /// Mic samples lost because the DSP thread fell behind.
    pub input_overruns: u64,
    /// Output callbacks that ran short because the DSP thread fell behind.
    pub output_underruns: u64,
    /// Capture to processing: the device's buffering.
    pub input_latency: Duration,
    /// Mixing to sound: queued playback plus device buffering.
    pub output_latency: Duration,
    /// The longest DSP pass in the last second.
    pub max_tick: Duration,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    push_to_talk: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    mic_level_db: AtomicU32,
    mic_level: AtomicU32,
    speech: AtomicU32,
    transmitting: AtomicBool,
    speakers: AtomicU32,
    input_overruns: AtomicU64,
    output_underruns: AtomicU64,
    input_device_us: AtomicU64,
    output_latency_us: AtomicU64,
    max_tick_us: AtomicU64,
    /// Smoothed level per speaker, refreshed whenever audio is mixed. The
    /// DSP thread only ever `try_lock`s it, so a reader can't stall audio.
    speaker_levels: Mutex<Vec<(u32, f32)>>,
}

enum Command {
    Mode(TransmitMode),
    Processing(Processing),
    Speaker(u32, SpeakerSettings),
    RemoveSpeaker(u32),
    Volume(f32),
    Bitrate(i32),
}

/// Wakes the engine's DSP thread. Cheap, never blocks, and safe to call
/// from any thread, including async tasks.
#[derive(Debug, Clone)]
pub struct Waker(Option<thread::Thread>);

impl Waker {
    pub fn wake(&self) {
        if let Some(thread) = &self.0 {
            thread.unpark();
        }
    }
}

/// The running audio engine. Dropping it stops audio.
pub struct Engine {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Engine {
    /// Opens the devices and starts processing. Voice is read from `source`
    /// and sent to `sink`; the engine stops on its own once `source` closes.
    pub fn start(
        config: EngineConfig,
        source: impl VoiceSource,
        sink: impl VoiceSink,
    ) -> Result<Self, AudioError> {
        let shared = Arc::new(Shared::default());
        // Quiet until the first frame is measured.
        shared
            .mic_level_db
            .store((-100f32).to_bits(), Ordering::Relaxed);
        let (commands, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("blurt-audio".into())
            .spawn(move || {
                // cpal streams can't move between threads, so they're opened
                // (and dropped) here.
                let devices = match Devices::open(&config, &thread_shared) {
                    Ok(devices) => devices,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let dsp = match Dsp::new(&config, &devices) {
                    Ok(dsp) => dsp,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                dsp.run(devices, source, sink, &thread_shared, &command_rx);
            })
            .map_err(|_| AudioError::ThreadDied)?;
        ready_rx.recv().map_err(|_| AudioError::ThreadDied)??;
        Ok(Self {
            shared,
            commands,
            thread: Some(thread),
        })
    }

    /// Whether the engine stopped on its own (the voice source closed, or a
    /// device failed).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    pub fn set_push_to_talk(&self, pressed: bool) {
        self.shared.push_to_talk.store(pressed, Ordering::Relaxed);
        self.wake();
    }

    /// Muted, nothing is sent. Takes effect from the next 10 ms frame.
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
        self.wake();
    }

    /// Deafened, nothing is played (and incoming voice isn't decoded).
    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
        self.wake();
    }

    pub fn set_mode(&self, mode: TransmitMode) {
        self.send(Command::Mode(mode));
    }

    pub fn set_processing(&self, processing: Processing) {
        self.send(Command::Processing(processing));
    }

    /// Sets a speaker's volume or local mute.
    pub fn set_speaker(&self, session: u32, settings: SpeakerSettings) {
        self.send(Command::Speaker(session, settings));
    }

    /// Master output volume: 1.0 plays everyone as they are.
    pub fn set_output_volume(&self, volume: f32) {
        self.send(Command::Volume(volume));
    }

    /// Each speaker's smoothed level, by session, for meters and visuals.
    /// Speakers not heard from in a while are left out.
    pub fn speaker_levels(&self) -> Vec<(u32, f32)> {
        self.shared
            .speaker_levels
            .lock()
            .map(|levels| levels.clone())
            .unwrap_or_default()
    }

    /// Forgets a speaker who left the server.
    pub fn remove_speaker(&self, session: u32) {
        self.send(Command::RemoveSpeaker(session));
    }

    /// E.g. to stay under the server's bandwidth limit.
    pub fn set_bitrate(&self, bitrate: i32) {
        self.send(Command::Bitrate(bitrate));
    }

    /// For the network side to call after handing the [`VoiceSource`] a
    /// packet, so it's handled at once instead of at the next device
    /// callback.
    pub fn waker(&self) -> Waker {
        Waker(self.thread.as_ref().map(|t| t.thread().clone()))
    }

    fn wake(&self) {
        self.waker().wake();
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
        self.wake();
    }

    pub fn stats(&self) -> EngineStats {
        let s = &self.shared;
        let us = |a: &AtomicU64| Duration::from_micros(a.load(Ordering::Relaxed));
        EngineStats {
            mic_level_db: f32::from_bits(s.mic_level_db.load(Ordering::Relaxed)),
            mic_level: f32::from_bits(s.mic_level.load(Ordering::Relaxed)),
            speech: f32::from_bits(s.speech.load(Ordering::Relaxed)),
            transmitting: s.transmitting.load(Ordering::Relaxed),
            speakers: s.speakers.load(Ordering::Relaxed) as usize,
            input_overruns: s.input_overruns.load(Ordering::Relaxed),
            output_underruns: s.output_underruns.load(Ordering::Relaxed),
            input_latency: us(&s.input_device_us),
            output_latency: us(&s.output_latency_us),
            max_tick: us(&s.max_tick_us),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The open devices and the rings between them and the DSP thread.
struct Devices {
    _input: cpal::Stream,
    _output: cpal::Stream,
    mic: rtrb::Consumer<f32>,
    speaker: rtrb::Producer<f32>,
    input_rate: u32,
    output_rate: u32,
    /// How much the output device takes per callback, when known.
    output_buffer_frames: Option<u32>,
}

impl Devices {
    fn open(config: &EngineConfig, shared: &Arc<Shared>) -> Result<Self, AudioError> {
        let host = host(config.host.as_deref())?;
        log::info!("Audio host: {}", host.id().name());
        let input = find_device(&host, config.input_device.as_deref(), true)?;
        let output = find_device(&host, config.output_device.as_deref(), false)?;

        let (input_config, input_format) =
            stream_config(&input, true, config.device_buffer_frames)?;
        let (output_config, output_format) =
            stream_config(&output, false, config.device_buffer_frames)?;
        log::info!(
            "Mic: {} ({input_config:?}, {input_format:?})",
            device_name(&input)
        );
        log::info!(
            "Speakers: {} ({output_config:?}, {output_format:?})",
            device_name(&output)
        );

        // Half a second of room either way: far more than ever queues up.
        let (mic_tx, mic) = rtrb::RingBuffer::new(input_config.sample_rate as usize / 2);
        let (speaker, speaker_rx) = rtrb::RingBuffer::new(output_config.sample_rate as usize / 2);
        // Opened on the DSP thread, so the callbacks wake it directly.
        let dsp = thread::current();

        let input_stream = match input_format {
            cpal::SampleFormat::F32 => {
                build_input::<f32>(&input, &input_config, mic_tx, shared, dsp.clone())
            }
            cpal::SampleFormat::I16 => {
                build_input::<i16>(&input, &input_config, mic_tx, shared, dsp.clone())
            }
            other => return Err(AudioError::UnsupportedFormat(other)),
        }?;
        let output_stream = match output_format {
            cpal::SampleFormat::F32 => {
                build_output::<f32>(&output, &output_config, speaker_rx, shared, dsp.clone())
            }
            cpal::SampleFormat::I16 => {
                build_output::<i16>(&output, &output_config, speaker_rx, shared, dsp.clone())
            }
            other => return Err(AudioError::UnsupportedFormat(other)),
        }?;
        input_stream.play()?;
        output_stream.play()?;

        Ok(Self {
            _input: input_stream,
            _output: output_stream,
            mic,
            speaker,
            input_rate: input_config.sample_rate,
            output_rate: output_config.sample_rate,
            output_buffer_frames: match output_config.buffer_size {
                cpal::BufferSize::Fixed(frames) => Some(frames),
                cpal::BufferSize::Default => None,
            },
        })
    }
}

fn find_device(
    host: &cpal::Host,
    id: Option<&str>,
    is_input: bool,
) -> Result<cpal::Device, AudioError> {
    let kind = if is_input { "input" } else { "output" };
    match id {
        Some(id) => id
            .parse::<cpal::DeviceId>()
            .ok()
            .and_then(|id| host.device_by_id(&id))
            .ok_or_else(|| AudioError::UnknownDevice(id.to_string())),
        None if is_input => host
            .default_input_device()
            .ok_or(AudioError::NoDevice(kind)),
        None => host
            .default_output_device()
            .ok_or(AudioError::NoDevice(kind)),
    }
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown device".into())
}

/// Prefers 48 kHz mono f32 with a small buffer, which needs no conversion
/// at all. Otherwise takes the device's default and converts.
fn stream_config(
    device: &cpal::Device,
    is_input: bool,
    buffer_frames: u32,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), AudioError> {
    let supported: Vec<_> = if is_input {
        device.supported_input_configs()?.collect()
    } else {
        device.supported_output_configs()?.collect()
    };
    let ideal = supported
        .iter()
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .filter(|c| c.channels() == 1)
        .find_map(|c| c.try_with_sample_rate(SAMPLE_RATE));
    let chosen = match ideal {
        Some(config) => config,
        None if is_input => device.default_input_config()?,
        None => device.default_output_config()?,
    };
    let buffer_size = match chosen.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(buffer_frames.clamp(*min, *max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    };
    let format = chosen.sample_format();
    let mut config = chosen.config();
    config.buffer_size = buffer_size;
    Ok((config, format))
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut mic: rtrb::Producer<f32>,
    shared: &Arc<Shared>,
    dsp: thread::Thread,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let shared = shared.clone();
    let errors = shared.clone();
    Ok(device.build_input_stream(
        *config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let t = info.timestamp();
            if let Some(delay) = t.callback.checked_duration_since(t.capture) {
                shared
                    .input_device_us
                    .store(delay.as_micros() as u64, Ordering::Relaxed);
            }
            let mut lost = 0;
            for frame in data.chunks(channels) {
                let mono =
                    frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32;
                if mic.push(mono).is_err() {
                    lost += 1;
                }
            }
            if lost > 0 {
                shared.input_overruns.fetch_add(lost, Ordering::Relaxed);
            }
            dsp.unpark();
        },
        move |e| {
            // Closing the streams on shutdown reports them disconnected.
            if errors.stop.swap(true, Ordering::Relaxed) {
                log::debug!("Mic stream error while stopping: {e}");
            } else {
                log::error!("Mic stream error: {e}");
            }
        },
        None,
    )?)
}

fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut speaker: rtrb::Consumer<f32>,
    shared: &Arc<Shared>,
    dsp: thread::Thread,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let shared = shared.clone();
    let errors = shared.clone();
    Ok(device.build_output_stream(
        *config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let t = info.timestamp();
            let queued = speaker.slots() as u64 * 1_000_000 / u64::from(rate);
            let device = t
                .playback
                .checked_duration_since(t.callback)
                .map_or(0, |d| d.as_micros() as u64);
            shared
                .output_latency_us
                .store(queued + device, Ordering::Relaxed);
            let mut short = false;
            for frame in data.chunks_mut(channels) {
                let sample = speaker.pop().unwrap_or_else(|_| {
                    short = true;
                    0.0
                });
                frame.fill(T::from_sample(sample));
            }
            if short {
                shared.output_underruns.fetch_add(1, Ordering::Relaxed);
            }
            dsp.unpark();
        },
        move |e| {
            // Closing the streams on shutdown reports them disconnected.
            if errors.stop.swap(true, Ordering::Relaxed) {
                log::debug!("Speaker stream error while stopping: {e}");
            } else {
                log::error!("Speaker stream error: {e}");
            }
        },
        None,
    )?)
}

/// Output ring level to keep, in device samples: the configured buffer, but
/// always enough for a whole device request plus 5 ms, or the device
/// would drain the ring between refills and underrun.
fn playback_target(config: &EngineConfig, devices: &Devices) -> usize {
    let rate = devices.output_rate as usize;
    let configured = (rate as f64 * config.playback_buffer.as_secs_f64()) as usize;
    let request = devices.output_buffer_frames.unwrap_or(0) as usize;
    configured.max(request + rate / 200)
}

/// The DSP thread's state.
struct Dsp {
    capture: Capture,
    mixer: Mixer,
    mic_resampler: Resampler,
    speaker_resampler: Resampler,
    /// Output ring level to keep, in device samples.
    playback_target: usize,
    speaker_capacity: usize,
    recorder: Option<Recorder>,
}

impl Dsp {
    fn new(config: &EngineConfig, devices: &Devices) -> Result<Self, AudioError> {
        Ok(Self {
            capture: Capture::new(config.processing, config.mode, config.encoder)?,
            mixer: Mixer::new(config.jitter),
            mic_resampler: Resampler::new(devices.input_rate, SAMPLE_RATE)?,
            speaker_resampler: Resampler::new(SAMPLE_RATE, devices.output_rate)?,
            playback_target: playback_target(config, devices),
            speaker_capacity: devices.speaker.buffer().capacity(),
            recorder: config
                .record_to
                .as_deref()
                .map(Recorder::start)
                .transpose()?,
        })
    }

    fn run(
        mut self,
        mut devices: Devices,
        mut source: impl VoiceSource,
        mut sink: impl VoiceSink,
        shared: &Shared,
        commands: &mpsc::Receiver<Command>,
    ) {
        let mut mic_48k: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize / 10);
        let mut mic_raw: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize / 10);
        let mut speaker_out: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize / 10);
        let mut was_deafened = false;
        let mut last_housekeeping = Instant::now();
        let mut max_tick = Duration::ZERO;
        let mut mic_level = Envelope::default();

        while !shared.stop.load(Ordering::Relaxed) {
            let tick_start = Instant::now();

            while let Ok(command) = commands.try_recv() {
                match command {
                    Command::Mode(mode) => self.capture.mode = mode,
                    Command::Processing(p) => self.capture.set_processing(p),
                    Command::Speaker(session, settings) => {
                        self.mixer.set_speaker(session, settings)
                    }
                    Command::RemoveSpeaker(session) => self.mixer.remove(session),
                    Command::Volume(volume) => self.mixer.set_volume(volume),
                    Command::Bitrate(bitrate) => self.capture.set_bitrate(bitrate),
                }
            }

            // Incoming voice.
            let deafened = shared.deafened.load(Ordering::Relaxed);
            if deafened && !was_deafened {
                self.mixer.clear();
            }
            was_deafened = deafened;
            let mut closed = false;
            loop {
                match source.try_recv() {
                    Ok(Some(voice)) if !deafened => self.mixer.push(voice, tick_start),
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(Closed) => {
                        closed = true;
                        break;
                    }
                }
            }

            // Playback: keep the output ring at its target level. The device
            // drains it at its own pace, so this follows the device's clock.
            // Only passes that mixed update the count; mic-only passes keep it.
            let mut speakers = None;
            while self.speaker_capacity - devices.speaker.slots() < self.playback_target {
                let mut frame = [0.0; FRAME_SAMPLES];
                if !deafened {
                    let mixed = self.mixer.mix(&mut frame);
                    speakers = Some(speakers.unwrap_or(0).max(mixed));
                } else {
                    speakers = Some(0);
                }
                if let Some(recorder) = &mut self.recorder {
                    recorder.push(Track::Playback, &frame);
                }
                // Everything played is the echo canceller's reference.
                // Deafened, nothing plays, so there is no echo to cancel.
                if !deafened {
                    self.capture.render(&frame);
                }
                speaker_out.clear();
                self.speaker_resampler.process(&frame, &mut speaker_out);
                if push_all(&mut devices.speaker, &speaker_out) < speaker_out.len() {
                    break;
                }
            }

            // Capture: process every complete 10 ms of mic audio.
            let available = devices.mic.slots();
            if let Ok(chunk) = devices.mic.read_chunk(available) {
                let (a, b) = chunk.as_slices();
                mic_raw.clear();
                mic_raw.extend_from_slice(a);
                mic_raw.extend_from_slice(b);
                chunk.commit_all();
            }
            self.mic_resampler.process(&mic_raw, &mut mic_48k);
            let push_to_talk = shared.push_to_talk.load(Ordering::Relaxed);
            let muted = shared.muted.load(Ordering::Relaxed);
            let mut consumed = 0;
            while mic_48k.len() - consumed >= FRAME_SAMPLES {
                let frame: &[f32; FRAME_SAMPLES] = mic_48k[consumed..consumed + FRAME_SAMPLES]
                    .try_into()
                    .expect("exactly one frame");
                let report = self.capture.capture(frame, push_to_talk, muted, &mut sink);
                if let Some(recorder) = &mut self.recorder {
                    let processed = self.capture.processed();
                    recorder.push(Track::MicRaw, frame);
                    recorder.push(Track::MicProcessed, processed);
                    let silence = [0.0; FRAME_SAMPLES];
                    let sent = if report.transmitting {
                        processed
                    } else {
                        &silence
                    };
                    recorder.push(Track::Sent, sent);
                }
                consumed += FRAME_SAMPLES;
                // Processed audio is silence while muted, so the meter rests.
                let level = mic_level.step(level::rms(self.capture.processed()));
                shared.mic_level.store(level.to_bits(), Ordering::Relaxed);
                shared
                    .mic_level_db
                    .store(report.level_db.to_bits(), Ordering::Relaxed);
                shared
                    .speech
                    .store(report.speech.to_bits(), Ordering::Relaxed);
                shared
                    .transmitting
                    .store(report.transmitting, Ordering::Relaxed);
            }
            mic_48k.drain(..consumed);
            if let Some(speakers) = speakers {
                shared.speakers.store(speakers as u32, Ordering::Relaxed);
                if let Ok(mut levels) = shared.speaker_levels.try_lock() {
                    levels.clear();
                    levels.extend(self.mixer.levels());
                }
            }

            // Once a second: tidy up and refresh the echo canceller's delay.
            max_tick = max_tick.max(tick_start.elapsed());
            if last_housekeeping.elapsed() >= Duration::from_secs(1) {
                last_housekeeping = Instant::now();
                self.mixer.prune(tick_start);
                if log::log_enabled!(log::Level::Debug) {
                    if let Some(echo) = self.capture.echo_stats() {
                        log::debug!("Echo canceller: {echo:?}");
                    }
                    for (session, stats, delay) in self.mixer.speaker_stats() {
                        log::debug!(
                            "Speaker {session}: played {}, recovered {}, concealed {}, \
                             late {}, overflowed {}; delay {delay} frames",
                            stats.played,
                            stats.recovered,
                            stats.concealed,
                            stats.late,
                            stats.overflowed,
                        );
                    }
                }
                let input = Duration::from_micros(shared.input_device_us.load(Ordering::Relaxed));
                let output =
                    Duration::from_micros(shared.output_latency_us.load(Ordering::Relaxed));
                let echo_delay = input + output;
                self.capture
                    .set_echo_delay_ms(echo_delay.as_millis() as i32);
                shared
                    .max_tick_us
                    .store(max_tick.as_micros() as u64, Ordering::Relaxed);
                max_tick = Duration::ZERO;
            }

            if closed {
                log::info!("Voice source closed; stopping audio");
                break;
            }
            // Work that arrived while this pass ran (its wakeup is pending)
            // makes this return at once.
            thread::park_timeout(FALLBACK_WAKE);
        }
    }
}

/// Pushes as much of `samples` as fits; returns how many did.
fn push_all(ring: &mut rtrb::Producer<f32>, samples: &[f32]) -> usize {
    let n = samples.len().min(ring.slots());
    if let Ok(mut chunk) = ring.write_chunk(n) {
        let (a, b) = chunk.as_mut_slices();
        let split = a.len();
        a.copy_from_slice(&samples[..split]);
        b.copy_from_slice(&samples[split..n]);
        chunk.commit_all();
    }
    n
}

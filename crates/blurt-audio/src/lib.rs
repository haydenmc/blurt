//! Real-time audio engine for blurt.
//!
//! Capture → echo cancellation and gain (sonora) → noise suppression
//! (RNNoise) → voice activity or push-to-talk gate → Opus; and per-speaker
//! jitter buffer → Opus → mix → playback. Device callbacks only move samples
//! through lock-free ring buffers; everything else runs on one DSP thread.
//!
//! The engine knows nothing about the network: voice comes in as
//! [`IncomingVoice`] and goes out through a [`VoiceSink`].

pub mod capture;
pub mod codec;
pub mod engine;
pub mod jitter;
pub mod level;
pub mod mixer;
mod recorder;
pub mod resample;

pub use engine::{
    AudioError, DeviceInfo, Engine, EngineConfig, EngineStats, Waker, hosts, input_devices,
    output_devices,
};

/// Everything inside the engine runs at 48 kHz mono, Opus's native rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// 10 ms: the unit of processing, and of Mumble's frame numbers.
pub const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 100;
/// 120 ms, the longest Opus packet.
pub const MAX_PACKET_SAMPLES: usize = SAMPLE_RATE as usize * 120 / 1000;

/// A voice packet from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingVoice {
    pub session: u32,
    /// Counts 10 ms frames; orders packets within a talk spurt.
    pub frame_number: u64,
    pub opus: Vec<u8>,
    /// The last packet of a talk spurt.
    pub is_terminator: bool,
}

/// Where encoded voice goes: the network.
pub trait VoiceSink: Send + 'static {
    fn send(&mut self, opus: &[u8], frame_number: u64, is_terminator: bool);
}

impl<F: FnMut(&[u8], u64, bool) + Send + 'static> VoiceSink for F {
    fn send(&mut self, opus: &[u8], frame_number: u64, is_terminator: bool) {
        self(opus, frame_number, is_terminator)
    }
}

/// Where received voice comes from: polled from the DSP thread, so it must
/// not block.
pub trait VoiceSource: Send + 'static {
    /// The next packet, if one is waiting. `Err` once the source is closed.
    fn try_recv(&mut self) -> Result<Option<IncomingVoice>, Closed>;
}

/// The voice source closed (e.g. disconnected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

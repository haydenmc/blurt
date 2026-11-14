//! Mixes everyone talking into one 48 kHz mono stream.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::codec::Decoder;
use crate::jitter::{FrameDecoder, JitterBuffer, JitterConfig, JitterStats};
use crate::level::{self, Envelope};
use crate::{FRAME_SAMPLES, IncomingVoice};

/// Speakers silent for this long are dropped, freeing their decoder.
const FORGET_AFTER: Duration = Duration::from_secs(30);

struct Speaker<D> {
    jitter: JitterBuffer<D>,
    last_heard: Instant,
    /// How loud they are, before their volume setting.
    level: Envelope,
}

/// Per-speaker volume and local mute, set by the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakerSettings {
    pub gain: f32,
    pub muted: bool,
}

impl Default for SpeakerSettings {
    fn default() -> Self {
        Self {
            gain: 1.0,
            muted: false,
        }
    }
}

pub struct Mixer<D = Decoder> {
    speakers: HashMap<u32, Speaker<D>>,
    settings: HashMap<u32, SpeakerSettings>,
    new_decoder: fn() -> Option<D>,
    jitter: JitterConfig,
    frame: [f32; FRAME_SAMPLES],
    /// Master output volume, applied to the whole mix.
    volume: f32,
}

impl Mixer<Decoder> {
    pub fn new(jitter: JitterConfig) -> Self {
        Self::with_decoders(jitter, || Decoder::new().ok())
    }
}

impl<D: FrameDecoder> Mixer<D> {
    pub fn with_decoders(jitter: JitterConfig, new_decoder: fn() -> Option<D>) -> Self {
        Self {
            speakers: HashMap::new(),
            settings: HashMap::new(),
            new_decoder,
            jitter,
            frame: [0.0; FRAME_SAMPLES],
            volume: 1.0,
        }
    }

    pub fn push(&mut self, voice: IncomingVoice, now: Instant) {
        if self.settings.get(&voice.session).is_some_and(|s| s.muted) {
            return;
        }
        let speaker = match self.speakers.entry(voice.session) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Some(decoder) = (self.new_decoder)() else {
                    log::warn!("Could not create a decoder for session {}", voice.session);
                    return;
                };
                entry.insert(Speaker {
                    jitter: JitterBuffer::new(decoder, self.jitter),
                    last_heard: now,
                    level: Envelope::default(),
                })
            }
        };
        speaker.last_heard = now;
        speaker
            .jitter
            .push(voice.frame_number, voice.opus, voice.is_terminator);
    }

    /// Mixes the next 10 ms into `out`. Returns how many speakers were in it.
    pub fn mix(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> usize {
        out.fill(0.0);
        let mut active = 0;
        for (session, speaker) in &mut self.speakers {
            if !speaker.jitter.pull(&mut self.frame) {
                speaker.level.step(0.0);
                continue;
            }
            active += 1;
            speaker.level.step(level::rms(&self.frame));
            let gain = self.volume * self.settings.get(session).map_or(1.0, |s| s.gain);
            for (o, s) in out.iter_mut().zip(&self.frame) {
                *o += s * gain;
            }
        }
        if active > 0 {
            for sample in out.iter_mut() {
                *sample = soft_clip(*sample);
            }
        }
        active
    }

    pub fn set_speaker(&mut self, session: u32, settings: SpeakerSettings) {
        if settings.muted {
            self.speakers.remove(&session);
        }
        self.settings.insert(session, settings);
    }

    /// Scales everything played; 1.0 leaves it as it is.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.max(0.0);
    }

    /// Each speaker's smoothed level (see [`crate::level`]), before their
    /// volume setting, updated with every mixed frame.
    pub fn levels(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.speakers
            .iter()
            .map(|(&session, s)| (session, s.level.value()))
    }

    /// Forgets a speaker who left.
    pub fn remove(&mut self, session: u32) {
        self.speakers.remove(&session);
        self.settings.remove(&session);
    }

    /// Stops everything playing (e.g. on deafen).
    pub fn clear(&mut self) {
        self.speakers.clear();
    }

    /// Drops speakers not heard from in a while.
    pub fn prune(&mut self, now: Instant) {
        self.speakers
            .retain(|_, s| !s.jitter.is_idle() || now - s.last_heard < FORGET_AFTER);
    }

    pub fn speaker_stats(&self) -> impl Iterator<Item = (u32, JitterStats, usize)> + '_ {
        self.speakers
            .iter()
            .map(|(&session, s)| (session, s.jitter.stats, s.jitter.target_delay()))
    }
}

/// Passes normal levels through untouched and smoothly bends anything louder
/// (e.g. overlapping speakers) toward, but never past, full scale.
pub fn soft_clip(sample: f32) -> f32 {
    const THRESHOLD: f32 = 0.8;
    let magnitude = sample.abs();
    if magnitude <= THRESHOLD {
        return sample;
    }
    // tanh's slope is 1 at 0, so the curve continues the straight line at
    // THRESHOLD without a kink, and approaches 1.0 asymptotically.
    let headroom = 1.0 - THRESHOLD;
    let bent = THRESHOLD + headroom * ((magnitude - THRESHOLD) / headroom).tanh();
    bent.copysign(sample)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitter::tests::FakeDecoder;

    fn mixer() -> Mixer<FakeDecoder> {
        Mixer::with_decoders(
            JitterConfig {
                initial_delay: 1,
                min_delay: 1,
                ..Default::default()
            },
            || Some(FakeDecoder),
        )
    }

    fn voice(session: u32, frame_number: u64, value: u8) -> IncomingVoice {
        IncomingVoice {
            session,
            frame_number,
            opus: vec![1, value],
            is_terminator: false,
        }
    }

    #[test]
    fn speakers_are_summed_with_their_gain() {
        let mut mixer = mixer();
        let now = Instant::now();
        mixer.push(voice(1, 0, 1), now);
        mixer.push(voice(2, 50, 2), now);
        mixer.set_speaker(
            2,
            SpeakerSettings {
                gain: 0.25,
                muted: false,
            },
        );
        let mut out = [0.0; FRAME_SAMPLES];
        // FakeDecoder outputs the value itself, so the sum is soft-clipped.
        assert_eq!(mixer.mix(&mut out), 2);
        assert_eq!(out[0], soft_clip(1.0 + 0.5));
    }

    #[test]
    fn volume_scales_the_mix() {
        let mut mixer = mixer();
        mixer.push(voice(1, 0, 1), Instant::now());
        mixer.set_volume(0.5);
        let mut out = [0.0; FRAME_SAMPLES];
        mixer.mix(&mut out);
        // FakeDecoder plays the packet's value: 1.0, halved.
        assert_eq!(out[0], 0.5);
    }

    #[test]
    fn levels_rise_with_speech_and_fall_after_it() {
        let mut mixer = mixer();
        let now = Instant::now();
        for frame in 0..5 {
            mixer.push(
                IncomingVoice {
                    is_terminator: frame == 4,
                    ..voice(1, frame, 1)
                },
                now,
            );
        }
        let level = |mixer: &Mixer<FakeDecoder>| mixer.levels().find(|&(s, _)| s == 1).unwrap().1;
        let mut out = [0.0; FRAME_SAMPLES];
        let mut peak: f32 = 0.0;
        for _ in 0..6 {
            mixer.mix(&mut out);
            peak = peak.max(level(&mixer));
        }
        assert!(peak > 0.5, "{peak}");
        // After the talk spurt it fades rather than dropping to 0 at once...
        for _ in 0..5 {
            mixer.mix(&mut out);
        }
        let fading = level(&mixer);
        assert!(fading > 0.0 && fading < peak, "{fading}");
        // ...and settles to silence.
        for _ in 0..500 {
            mixer.mix(&mut out);
        }
        assert_eq!(level(&mixer), 0.0);
    }

    #[test]
    fn muted_speakers_are_not_played() {
        let mut mixer = mixer();
        mixer.set_speaker(
            1,
            SpeakerSettings {
                gain: 1.0,
                muted: true,
            },
        );
        mixer.push(voice(1, 0, 1), Instant::now());
        let mut out = [0.0; FRAME_SAMPLES];
        assert_eq!(mixer.mix(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn idle_speakers_are_forgotten() {
        let mut mixer = mixer();
        let start = Instant::now();
        mixer.push(
            IncomingVoice {
                is_terminator: true,
                ..voice(1, 0, 1)
            },
            start,
        );
        let mut out = [0.0; FRAME_SAMPLES];
        for _ in 0..3 {
            mixer.mix(&mut out);
        }
        mixer.prune(start + FORGET_AFTER / 2);
        assert_eq!(mixer.speaker_stats().count(), 1);
        mixer.prune(start + FORGET_AFTER * 2);
        assert_eq!(mixer.speaker_stats().count(), 0);
    }

    #[test]
    fn soft_clip_is_transparent_below_threshold_and_bounded_above() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.8), -0.8);
        assert!(soft_clip(5.0) <= 1.0 && soft_clip(5.0) > 0.99);
        assert!(soft_clip(0.9) < 0.9 && soft_clip(0.9) > 0.8);
        assert!(soft_clip(-5.0) >= -1.0);
    }
}

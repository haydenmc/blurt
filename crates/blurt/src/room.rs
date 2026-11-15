//! What the voice screen shows about a connection: the server's state, who
//! is talking, the transcript, and view choices. No handles to the network
//! or audio, so it can be built (and drawn) without them.

use std::collections::HashMap;
use std::time::Instant;

use iced::Animation;
use mumble_protocol::state::MumbleState;

use crate::config::Server;
use crate::ui::Pivot;
use crate::{talk, theme, transcript};

pub struct Room {
    pub server: Server,
    /// The client's state as of the last change, for drawing.
    pub state: MumbleState,
    pub muted: bool,
    pub deafened: bool,
    /// Whether the engine is sending your voice (by push-to-talk or voice
    /// activity), as of the last tick.
    pub transmitting: bool,
    /// Your smoothed mic level, as of the last tick or frame.
    pub mic_level: f32,
    /// Everyone's smoothed playback level by session, for the room glow;
    /// only kept up to date while it can be drawn.
    pub levels: Vec<(u32, f32)>,
    /// What's playing and what you're sending, by frequency band, for the
    /// room glow; kept up to date like `levels`.
    pub spectrum: blurt_audio::Spectrum,
    /// The room glow as shown, following `spectrum`.
    pub glow: GlowLevels,
    pub draft: String,
    pub talk: talk::TalkLog,
    pub transcript: transcript::Transcript,
    /// Per person in your channel: easing between idle and talking colours.
    /// Follows actual speaking, without the talking line's hold, so the
    /// colour is immediate feedback.
    pub talking: HashMap<u32, Animation<bool>>,
    /// The channel `talk` and `talking` are about.
    channel: Option<u32>,
    pub pivot: Pivot,
    pub pivot_changed: Option<Instant>,
    /// Compact: the volume slider is showing.
    pub volume_open: bool,
}

impl Room {
    pub fn new(server: Server) -> Self {
        Self {
            server,
            state: MumbleState::default(),
            muted: false,
            deafened: false,
            transmitting: false,
            mic_level: 0.0,
            levels: Vec::new(),
            spectrum: blurt_audio::Spectrum::default(),
            glow: GlowLevels::default(),
            draft: String::new(),
            talk: talk::TalkLog::default(),
            transcript: transcript::Transcript::default(),
            talking: HashMap::new(),
            channel: None,
            pivot: Pivot::People,
            pivot_changed: None,
            volume_open: false,
        }
    }

    /// Takes the client's state after it changed.
    pub fn update(&mut self, state: MumbleState, now: Instant) {
        self.state = state;
        self.transcript.sync(&self.state, chrono::Utc::now());
        self.observe_talking(now);
    }

    /// How loud each person in your channel is, 0 to 1, you included while
    /// you're heard: what the room glow shows.
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    pub fn loudness(&self) -> Vec<f32> {
        let Some(me) = self.state.current_user() else {
            return Vec::new();
        };
        let channel = me.channel_id;
        let mut loudness: Vec<f32> = self
            .levels
            .iter()
            .filter(|(session, _)| {
                self.state
                    .users
                    .get(session)
                    .is_some_and(|u| u.channel_id == channel && !u.is_me)
            })
            .map(|&(_, level)| crate::ui::meter::loudness(level))
            .collect();
        if self.live() {
            loudness.push(crate::ui::meter::loudness(self.mic_level));
        }
        loudness.retain(|&l| l > 0.0);
        loudness
    }

    /// How many in your channel are talking audibly, you included.
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    pub fn talkers(&self) -> usize {
        self.loudness().iter().filter(|&&l| l > 0.08).count()
    }

    /// Whether there's any sound for the room glow to show.
    pub fn sounding(&self) -> bool {
        let any = |bands: &[f32]| bands.iter().any(|&b| b > 0.0);
        any(&self.spectrum.playback) || (self.live() && any(&self.spectrum.mic))
    }

    /// Moves the room glow towards the sound now playing (and sent, while
    /// you're heard).
    pub fn step_glow(&mut self, now: Instant) {
        let live = self.live();
        let bands = std::array::from_fn(|i| {
            let mic = if live { self.spectrum.mic[i] } else { 0.0 };
            let level = self.spectrum.playback[i].max(mic);
            // Voices carry most of their energy low; tilt up 1.5 dB a band
            // so the high bands, in the middle, move too.
            let tilt = 2f32.powf(i as f32 * 0.25);
            crate::ui::meter::loudness(level * tilt)
        });
        let talkers = self.talkers();
        self.glow.step(bands, talkers, now);
    }

    /// Whether you're actually being heard.
    pub fn live(&self) -> bool {
        self.transmitting && !self.muted && !self.deafened
    }

    /// Updates who's talking in your channel, and starts colour changes for
    /// anyone who started or stopped.
    pub fn observe_talking(&mut self, now: Instant) {
        let Some(me) = self.state.current_user() else {
            return;
        };
        let channel = me.channel_id;
        if self.channel != Some(channel) {
            // A new channel: the talking line starts over.
            self.talk.clear();
            self.talking.clear();
            self.channel = Some(channel);
        }
        let live = self.live();
        let here: Vec<(u32, bool)> = self
            .state
            .users
            .values()
            .filter(|u| u.channel_id == channel)
            .map(|u| (u.session, if u.is_me { live } else { u.is_speaking }))
            .collect();
        for &(session, speaking) in &here {
            self.talk.observe(session, speaking, now);
        }
        let present = |s: u32| here.iter().any(|&(h, _)| h == s);
        self.talk.retain(present);
        self.talking.retain(|&s, _| present(s));
        for &(session, _) in &here {
            let talking = self.talk.speaking(session);
            let animation = self.talking.entry(session).or_insert_with(|| {
                Animation::new(false)
                    .duration(theme::motion::STATE)
                    .easing(theme::EASING)
            });
            if animation.value() != talking {
                animation.go_mut(talking, now);
            }
        }
    }
}

/// The room glow's levels as shown. They follow the sound up quickly but
/// drift back down slowly, so the glow settles after someone stops rather
/// than dropping out, and so does its height when a talker stops.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GlowLevels {
    /// Per frequency band, lowest first: 0 (flat) to 1 (full height).
    pub bands: [f32; blurt_audio::level::BANDS],
    /// How many are talking, eased.
    pub talkers: f32,
    at: Option<Instant>,
}

impl GlowLevels {
    /// Time to cover about 63 % of a rise, in seconds.
    const RISE: f32 = 0.05;
    /// Time to cover about 63 % of a fall: from full height, the glow is
    /// mostly gone after about a second.
    const FALL: f32 = 0.4;
    /// Below this, a level is shown as nothing.
    const FLOOR: f32 = 0.004;

    /// Whether there's anything to draw.
    pub fn visible(&self) -> bool {
        self.bands.iter().any(|&b| b > 0.0)
    }

    /// Moves towards `bands` and `talkers` by the time since the last step.
    pub fn step(&mut self, bands: [f32; blurt_audio::level::BANDS], talkers: usize, now: Instant) {
        // After a pause between steps, move only as if a frame had passed
        // or two, rather than jumping.
        let dt = self
            .at
            .map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f32())
            .min(0.1);
        self.at = Some(now);
        let follow = |shown: f32, target: f32| {
            let time = if target > shown {
                Self::RISE
            } else {
                Self::FALL
            };
            let v = shown + (target - shown) * (1.0 - (-dt / time).exp());
            if v < Self::FLOOR && target < Self::FLOOR {
                0.0
            } else {
                v
            }
        };
        for (shown, target) in self.bands.iter_mut().zip(bands) {
            *shown = follow(*shown, target);
        }
        self.talkers = follow(self.talkers, talkers as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blurt_audio::level::BANDS;
    use std::time::Duration;

    fn run(
        glow: &mut GlowLevels,
        bands: [f32; BANDS],
        talkers: usize,
        from: Instant,
        ms: u64,
    ) -> Instant {
        let mut now = from;
        for _ in 0..ms / 16 {
            now += Duration::from_millis(16);
            glow.step(bands, talkers, now);
        }
        now
    }

    #[test]
    fn glow_rises_fast_and_drifts_down() {
        let mut glow = GlowLevels::default();
        let start = Instant::now();
        glow.step([0.0; BANDS], 0, start);
        let now = run(&mut glow, [0.8; BANDS], 2, start, 200);
        assert!(glow.bands[0] > 0.75, "{:?}", glow.bands);
        assert!(glow.talkers > 1.9);
        // Stopped: still showing a little after a third of a second...
        let now = run(&mut glow, [0.0; BANDS], 0, now, 330);
        assert!((0.2..0.5).contains(&glow.bands[0]), "{:?}", glow.bands);
        assert!(glow.visible());
        // ...and gone in a few seconds.
        run(&mut glow, [0.0; BANDS], 0, now, 4000);
        assert!(!glow.visible());
        assert_eq!(glow.talkers, 0.0);
    }

    #[test]
    fn a_pause_between_steps_doesnt_jump() {
        let mut glow = GlowLevels::default();
        let start = Instant::now();
        glow.step([0.0; BANDS], 0, start);
        glow.step([1.0; BANDS], 1, start + Duration::from_secs(10));
        assert!(glow.bands[0] < 1.0);
    }
}

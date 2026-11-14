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
    pub draft: String,
    pub talk: talk::TalkLog,
    pub transcript: transcript::Transcript,
    /// Per person in your channel: easing between idle and talking colours.
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
            let talking = self.talk.is_talking(session, now);
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

//! Notification sounds: someone joining or leaving your channel, a chat
//! message, and your own mute and deafen. Played through the voice engine,
//! so they follow the output volume and the echo canceller hears them.
//!
//! The sounds are from Pixabay (see the README), transcoded to mono Opus.

use std::sync::OnceLock;

use blurt_audio::{Cue, Engine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Join,
    Leave,
    Message,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
}

/// Below the voices: the sounds peak near full scale.
const GAIN: f32 = 0.5;

const JOIN: &[u8] = include_bytes!("../assets/sounds/join.opus");
const LEAVE: &[u8] = include_bytes!("../assets/sounds/leave.opus");
const MESSAGE: &[u8] = include_bytes!("../assets/sounds/message.opus");
const OFF: &[u8] = include_bytes!("../assets/sounds/off.opus");
const ON: &[u8] = include_bytes!("../assets/sounds/on.opus");

impl Sound {
    /// Decoded on first use, then kept.
    fn cue(self) -> Option<&'static Cue> {
        static CUES: [OnceLock<Option<Cue>>; 5] = [const { OnceLock::new() }; 5];
        let (slot, file) = match self {
            Sound::Join => (0, JOIN),
            Sound::Leave => (1, LEAVE),
            Sound::Message => (2, MESSAGE),
            Sound::Mute | Sound::Deafen => (3, OFF),
            Sound::Unmute | Sound::Undeafen => (4, ON),
        };
        CUES[slot]
            .get_or_init(|| {
                Cue::from_ogg_opus(file)
                    .inspect_err(|e| log::warn!("Could not decode the {self:?} sound: {e}"))
                    .ok()
            })
            .as_ref()
    }

    pub fn play(self, engine: &Engine) {
        if let Some(cue) = self.cue() {
            engine.play_cue(cue, GAIN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_decodes() {
        for sound in [
            Sound::Join,
            Sound::Leave,
            Sound::Message,
            Sound::Mute,
            Sound::Unmute,
            Sound::Deafen,
            Sound::Undeafen,
        ] {
            let cue = sound.cue().expect("decodes");
            let secs = cue.duration().as_secs_f32();
            assert!((0.2..4.0).contains(&secs), "{sound:?}: {secs} s");
        }
    }
}

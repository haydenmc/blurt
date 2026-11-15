//! Smoothed signal levels, for meters and visuals.
//!
//! Levels are linear RMS (0 is silence, 1 is a full-scale sine's peak), and
//! they are smoothed per 10 ms frame with a fast attack and a slow release.
//! That way a meter jumps when someone starts speaking and doesn't flicker
//! between syllables.

use crate::FRAME_SAMPLES;

/// Time to cover about 63 % of a rise.
pub const ATTACK_MS: f32 = 40.0;
/// Time to cover about 63 % of a fall.
pub const RELEASE_MS: f32 = 250.0;

/// RMS of one frame.
pub fn rms(frame: &[f32; FRAME_SAMPLES]) -> f32 {
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    (sum / FRAME_SAMPLES as f32).sqrt()
}

/// A one-pole follower with separate attack and release, stepped once per
/// 10 ms frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Envelope {
    value: f32,
}

impl Envelope {
    /// Below this, a level is reported as silence.
    const FLOOR: f32 = 1e-4;

    pub fn value(&self) -> f32 {
        self.value
    }

    /// Moves toward `target` by one frame and returns the new level.
    pub fn step(&mut self, target: f32) -> f32 {
        let frame_ms = 1000.0 * FRAME_SAMPLES as f32 / crate::SAMPLE_RATE as f32;
        let time = if target > self.value {
            ATTACK_MS
        } else {
            RELEASE_MS
        };
        let k = 1.0 - (-frame_ms / time).exp();
        self.value += (target - self.value) * k;
        if self.value < Self::FLOOR {
            self.value = 0.0;
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_a_constant_is_the_constant() {
        assert!((rms(&[0.5; FRAME_SAMPLES]) - 0.5).abs() < 1e-6);
        assert_eq!(rms(&[0.0; FRAME_SAMPLES]), 0.0);
    }

    #[test]
    fn rises_fast_and_falls_slowly() {
        let mut env = Envelope::default();
        // 40 ms of signal: about 63 % of the way up.
        for _ in 0..4 {
            env.step(1.0);
        }
        let risen = env.value();
        assert!((0.55..0.7).contains(&risen), "{risen}");
        for _ in 30..40 {
            env.step(1.0);
        }
        // 40 ms of silence takes off much less than 40 ms of signal added.
        let top = env.value();
        for _ in 0..4 {
            env.step(0.0);
        }
        assert!(top - env.value() < 0.2, "{} → {}", top, env.value());
    }

    #[test]
    fn settles_to_exact_silence() {
        let mut env = Envelope::default();
        env.step(1.0);
        for _ in 0..1000 {
            env.step(0.0);
        }
        assert_eq!(env.value(), 0.0);
    }
}

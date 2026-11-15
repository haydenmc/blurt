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

/// How many bands [`Bands`] splits a signal into.
pub const BANDS: usize = 8;

/// The bands' centre frequencies: log-spaced over the voice's range.
pub const BAND_CENTRES: [f32; BANDS] = [125.0, 220.0, 390.0, 690.0, 1200.0, 2100.0, 3700.0, 6500.0];

/// A signal's level in [`BANDS`] frequency bands, lowest first, each
/// smoothed like [`Envelope`]. For spectrum displays, not measurement: a
/// bank of band-pass filters, cheaper than an FFT at this resolution.
#[derive(Debug, Clone)]
pub struct Bands {
    filters: [Biquad; BANDS],
    levels: [Envelope; BANDS],
}

impl Default for Bands {
    fn default() -> Self {
        Self::new()
    }
}

impl Bands {
    pub fn new() -> Self {
        Self {
            filters: BAND_CENTRES.map(|f| Biquad::band_pass(f, 1.6)),
            levels: [Envelope::default(); BANDS],
        }
    }

    /// Analyses one frame and returns the updated levels (linear RMS).
    pub fn step(&mut self, frame: &[f32; FRAME_SAMPLES]) -> [f32; BANDS] {
        for (filter, level) in self.filters.iter_mut().zip(&mut self.levels) {
            let mut sum = 0.0;
            for &x in frame {
                let y = filter.process(x);
                sum += y * y;
            }
            level.step((sum / FRAME_SAMPLES as f32).sqrt());
        }
        self.levels()
    }

    /// Lets the levels fall as if a silent frame went by, without filtering.
    pub fn decay(&mut self) -> [f32; BANDS] {
        for level in &mut self.levels {
            level.step(0.0);
        }
        self.levels()
    }

    pub fn levels(&self) -> [f32; BANDS] {
        std::array::from_fn(|i| self.levels[i].value())
    }
}

/// A second-order filter (RBJ cookbook), direct form I.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    /// Band-pass with 0 dB gain at `centre` Hz.
    fn band_pass(centre: f32, q: f32) -> Self {
        let w = std::f32::consts::TAU * centre / crate::SAMPLE_RATE as f32;
        let alpha = w.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b: [alpha / a0, 0.0, -alpha / a0],
            a: [-2.0 * w.cos() / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        // Flush denormals so silence stays cheap.
        let y = if y.abs() < 1e-20 { 0.0 } else { y };
        self.y = [y, self.y[0]];
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, frames: usize) -> Vec<[f32; FRAME_SAMPLES]> {
        (0..frames)
            .map(|f| {
                std::array::from_fn(|i| {
                    let n = (f * FRAME_SAMPLES + i) as f32;
                    0.5 * (std::f32::consts::TAU * freq * n / crate::SAMPLE_RATE as f32).sin()
                })
            })
            .collect()
    }

    #[test]
    fn a_tone_lights_its_own_band() {
        for (band, &centre) in BAND_CENTRES.iter().enumerate() {
            let mut bands = Bands::new();
            let mut levels = [0.0; BANDS];
            for frame in sine(centre, 30) {
                levels = bands.step(&frame);
            }
            let loudest = (0..BANDS)
                .max_by(|&a, &b| levels[a].total_cmp(&levels[b]))
                .unwrap();
            assert_eq!(loudest, band, "{centre} Hz: {levels:?}");
            // About the tone's RMS (0.35) in its own band.
            assert!(
                (0.25..0.45).contains(&levels[band]),
                "{centre} Hz: {levels:?}"
            );
        }
    }

    #[test]
    fn bands_fall_back_to_silence() {
        let mut bands = Bands::new();
        for frame in sine(1200.0, 10) {
            bands.step(&frame);
        }
        let silence = [0.0; FRAME_SAMPLES];
        for _ in 0..1000 {
            bands.step(&silence);
        }
        assert_eq!(bands.levels(), [0.0; BANDS]);
        assert_eq!(bands.decay(), [0.0; BANDS]);
    }

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

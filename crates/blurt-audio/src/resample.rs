//! Converts mono audio between a device's rate and the engine's 48 kHz.
//!
//! Only needed when a device can't run at 48 kHz: the PulseAudio backend
//! converts for us, so this is mostly for ALSA fallbacks.

use audioadapter_buffers::direct::InterleavedSlice;
use rubato::Resampler as _;

pub enum Resampler {
    /// Same rate both sides.
    Passthrough,
    Resample {
        resampler: Box<rubato::Fft<f32>>,
        /// Input not yet resampled: rubato consumes fixed-size chunks.
        pending: Vec<f32>,
        scratch: Vec<f32>,
    },
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Result<Self, rubato::ResamplerConstructionError> {
        if from == to {
            return Ok(Self::Passthrough);
        }
        // 10 ms of input per chunk.
        let chunk = from as usize / 100;
        let resampler = Box::new(rubato::Fft::new(
            from as usize,
            to as usize,
            chunk,
            1,
            1,
            rubato::FixedSync::Input,
        )?);
        let scratch = vec![0.0; resampler.output_frames_max()];
        Ok(Self::Resample {
            resampler,
            pending: Vec::with_capacity(chunk * 2),
            scratch,
        })
    }

    /// Appends the resampled `input` to `output`. Input that doesn't fill a
    /// whole chunk yet is kept for the next call.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        let Self::Resample {
            resampler,
            pending,
            scratch,
        } = self
        else {
            output.extend_from_slice(input);
            return;
        };
        pending.extend_from_slice(input);
        let mut consumed = 0;
        while pending.len() - consumed >= resampler.input_frames_next() {
            let frames_in = resampler.input_frames_next();
            let frames_out = scratch.len();
            let (Ok(chunk_in), Ok(mut chunk_out)) = (
                InterleavedSlice::new(&pending[consumed..consumed + frames_in], 1, frames_in),
                InterleavedSlice::new_mut(&mut scratch[..], 1, frames_out),
            ) else {
                break;
            };
            match resampler.process_into_buffer(&chunk_in, &mut chunk_out, None) {
                Ok((used, written)) => {
                    output.extend_from_slice(&scratch[..written]);
                    consumed += used;
                }
                Err(e) => {
                    log::warn!("Resampling failed: {e}");
                    consumed += frames_in;
                }
            }
        }
        pending.drain(..consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_copies() {
        let mut r = Resampler::new(48_000, 48_000).unwrap();
        let mut out = Vec::new();
        r.process(&[1.0, 2.0], &mut out);
        assert_eq!(out, [1.0, 2.0]);
    }

    #[test]
    fn converts_the_amount_of_audio_by_the_rate_ratio() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let mut out = Vec::new();
        // One second, in odd-sized pieces as a device would deliver it.
        for _ in 0..100 {
            r.process(&[0.0; 441], &mut out);
        }
        assert_eq!(out.len(), 48_000);
    }
}

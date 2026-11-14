//! Opus, configured for low-latency voice.

use crate::jitter::FrameDecoder;
use crate::{FRAME_SAMPLES, SAMPLE_RATE};

/// Opus encoder settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderConfig {
    pub bitrate: i32,
    /// Audio per packet, in 10 ms frames: 1 (lowest latency) or 2.
    pub frames_per_packet: usize,
    /// Expected packet loss, which sizes the redundancy (FEC) Opus adds.
    pub expected_loss_percent: i32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            // Mumble's default quality.
            bitrate: 40_000,
            frames_per_packet: 1,
            expected_loss_percent: 5,
        }
    }
}

pub struct Encoder {
    inner: opus::Encoder,
}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, opus::Error> {
        let mut inner =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        inner.set_bitrate(opus::Bitrate::Bits(config.bitrate))?;
        inner.set_signal(opus::Signal::Voice)?;
        // Redundancy lets a listener rebuild a lost packet from the next one.
        inner.set_inband_fec(true)?;
        inner.set_packet_loss_perc(config.expected_loss_percent)?;
        Ok(Self { inner })
    }

    pub fn set_bitrate(&mut self, bitrate: i32) -> Result<(), opus::Error> {
        self.inner.set_bitrate(opus::Bitrate::Bits(bitrate))
    }

    /// Encodes 10 or 20 ms of audio. Returns the packet length.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, opus::Error> {
        self.inner.encode_float(pcm, out)
    }

    /// Forgets the previous talk spurt, so a new one starts clean.
    pub fn reset(&mut self) -> Result<(), opus::Error> {
        self.inner.reset_state()
    }
}

pub struct Decoder {
    inner: opus::Decoder,
}

impl Decoder {
    pub fn new() -> Result<Self, opus::Error> {
        Ok(Self {
            inner: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?,
        })
    }
}

impl FrameDecoder for Decoder {
    fn frames_in(&self, packet: &[u8]) -> Option<usize> {
        let samples = opus::packet::get_nb_samples(packet, SAMPLE_RATE).ok()?;
        // Mumble packets hold whole 10 ms frames; round anything shorter up.
        Some(samples.div_ceil(FRAME_SAMPLES))
    }

    fn decode(&mut self, packet: &[u8], out: &mut [f32], fec: bool) -> Option<usize> {
        match self.inner.decode_float(packet, out, fec) {
            Ok(n) => Some(n),
            Err(e) => {
                log::debug!("Opus decode failed: {e}");
                None
            }
        }
    }

    fn conceal(&mut self, out: &mut [f32]) {
        // An empty packet asks libopus for packet loss concealment.
        if self.inner.decode_float(&[], out, false).is_err() {
            out.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frequency: f32, samples: usize, offset: usize) -> Vec<f32> {
        (offset..offset + samples)
            .map(|i| {
                0.5 * (2.0 * std::f32::consts::PI * frequency * i as f32 / SAMPLE_RATE as f32).sin()
            })
            .collect()
    }

    /// The strength of `frequency` in `signal` (a single-bin DFT), 0 to ~0.5.
    pub fn tone_strength(signal: &[f32], frequency: f32) -> f32 {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, s) in signal.iter().enumerate() {
            let phase = 2.0 * std::f32::consts::PI * frequency * i as f32 / SAMPLE_RATE as f32;
            re += s * phase.cos();
            im += s * phase.sin();
        }
        2.0 * (re * re + im * im).sqrt() / signal.len() as f32
    }

    #[test]
    fn a_tone_survives_encoding_at_both_packet_sizes() {
        for frames in [1, 2] {
            let mut encoder = Encoder::new(EncoderConfig {
                frames_per_packet: frames,
                ..Default::default()
            })
            .unwrap();
            let mut decoder = Decoder::new().unwrap();
            let len = frames * FRAME_SAMPLES;
            let mut decoded = Vec::new();
            let mut packet = [0u8; 1500];
            for n in 0..50 {
                let input = tone(1000.0, len, n * len);
                let size = encoder.encode(&input, &mut packet).unwrap();
                assert_eq!(decoder.frames_in(&packet[..size]), Some(frames));
                let mut out = vec![0.0; len];
                assert_eq!(decoder.decode(&packet[..size], &mut out, false), Some(len));
                decoded.extend(out);
            }
            // Skip the codec's start-up; judge the rest.
            let settled = &decoded[decoded.len() / 2..];
            assert!(tone_strength(settled, 1000.0) > 0.35, "{frames} frames");
            assert!(tone_strength(settled, 1700.0) < 0.05, "{frames} frames");
        }
    }

    #[test]
    fn fec_rebuilds_the_previous_frame() {
        let mut encoder = Encoder::new(EncoderConfig {
            expected_loss_percent: 20,
            ..Default::default()
        })
        .unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut packets = Vec::new();
        for n in 0..40 {
            let mut packet = [0u8; 1500];
            let size = encoder
                .encode(&tone(440.0, FRAME_SAMPLES, n * FRAME_SAMPLES), &mut packet)
                .unwrap();
            packets.push(packet[..size].to_vec());
        }
        let mut out = [0.0; FRAME_SAMPLES];
        for packet in &packets[..30] {
            decoder.decode(packet, &mut out, false).unwrap();
        }
        // Packet 30 is lost: rebuild it from packet 31.
        assert_eq!(
            decoder.decode(&packets[31], &mut out, true),
            Some(FRAME_SAMPLES)
        );
        assert!(tone_strength(&out, 440.0) > 0.2);
    }
}

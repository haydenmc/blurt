//! Cues: short sounds the app plays itself (a notification, a click), mixed
//! into playback beside voices.
//!
//! A [`Cue`] is decoded once, from an Ogg Opus file, to 48 kHz mono; then
//! [`Engine::play_cue`](crate::Engine::play_cue) plays it as often as asked.
//! Cues are heard while deafened (they're the app talking to you, not the
//! room), and they're part of the echo canceller's reference like anything
//! else played.

use std::sync::Arc;

use crate::{FRAME_SAMPLES, MAX_PACKET_SAMPLES, SAMPLE_RATE};

/// A decoded sound, ready to play: 48 kHz mono samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue(Arc<[f32]>);

#[derive(Debug, thiserror::Error)]
pub enum CueError {
    #[error("not an Ogg Opus file: {0}")]
    Format(&'static str),
    #[error("could not decode: {0}")]
    Opus(#[from] opus::Error),
}

impl Cue {
    /// Decodes a whole Ogg Opus file (one logical stream; mono or stereo,
    /// which is mixed down).
    pub fn from_ogg_opus(file: &[u8]) -> Result<Self, CueError> {
        let packets = ogg_packets(file)?;
        let (head, rest) = packets
            .packets
            .split_first()
            .ok_or(CueError::Format("empty"))?;
        if head.len() < 19 || &head[..8] != b"OpusHead" {
            return Err(CueError::Format("no OpusHead"));
        }
        let pre_skip = u16::from_le_bytes([head[10], head[11]]) as usize;
        // libopus mixes a stereo stream down when asked for mono.
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?;
        let mut samples = Vec::new();
        let mut buffer = [0.0; MAX_PACKET_SAMPLES];
        // After OpusHead comes OpusTags, then audio.
        for packet in rest.iter().skip(1) {
            let n = decoder.decode_float(packet, &mut buffer, false)?;
            samples.extend_from_slice(&buffer[..n]);
        }
        // The encoder's delay comes first; the last page's granule position
        // says where the audio really ends.
        let end = packets
            .last_granule
            .map_or(samples.len(), |g| (g as usize).min(samples.len()));
        let start = pre_skip.min(end);
        Ok(Self(samples[start..end].into()))
    }

    /// Wraps samples already at 48 kHz mono.
    pub fn from_samples(samples: impl Into<Arc<[f32]>>) -> Self {
        Self(samples.into())
    }

    pub fn samples(&self) -> &[f32] {
        &self.0
    }

    pub fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.0.len() as f64 / SAMPLE_RATE as f64)
    }
}

struct Packets {
    packets: Vec<Vec<u8>>,
    last_granule: Option<u64>,
}

/// Splits an Ogg file into its packets (RFC 3533). Only what a single
/// stream needs; checksums aren't checked, as the files are our own.
fn ogg_packets(mut file: &[u8]) -> Result<Packets, CueError> {
    let mut packets = Vec::new();
    let mut packet = Vec::new();
    let mut last_granule = None;
    while !file.is_empty() {
        if file.len() < 27 || &file[..4] != b"OggS" {
            return Err(CueError::Format("bad page"));
        }
        let granule = u64::from_le_bytes(file[6..14].try_into().expect("8 bytes"));
        let segments = file[26] as usize;
        let table = file
            .get(27..27 + segments)
            .ok_or(CueError::Format("short page"))?;
        let mut body = &file[27 + segments..];
        let body_len: usize = table.iter().map(|&l| l as usize).sum();
        if body.len() < body_len {
            return Err(CueError::Format("short page"));
        }
        for &lacing in table {
            let (segment, rest) = body.split_at(lacing as usize);
            packet.extend_from_slice(segment);
            body = rest;
            // A segment shorter than 255 ends its packet.
            if lacing < 255 {
                packets.push(std::mem::take(&mut packet));
            }
        }
        // -1 means no packet ends on this page.
        if granule != u64::MAX {
            last_granule = Some(granule);
        }
        file = &file[27 + segments + body_len..];
    }
    Ok(Packets {
        packets,
        last_granule,
    })
}

/// Cues playing now, mixed a frame at a time.
#[derive(Debug, Default)]
pub(crate) struct Cues {
    playing: Vec<Playing>,
}

#[derive(Debug)]
struct Playing {
    cue: Cue,
    at: usize,
    gain: f32,
}

impl Cues {
    pub fn play(&mut self, cue: Cue, gain: f32) {
        self.playing.push(Playing { cue, at: 0, gain });
    }

    pub fn is_empty(&self) -> bool {
        self.playing.is_empty()
    }

    /// Adds the next frame of every playing cue, times `volume`, into
    /// `frame`. Returns whether there were any.
    pub fn mix(&mut self, frame: &mut [f32; FRAME_SAMPLES], volume: f32) -> bool {
        if self.playing.is_empty() {
            return false;
        }
        for p in &mut self.playing {
            let samples = &p.cue.samples()[p.at..];
            let n = samples.len().min(FRAME_SAMPLES);
            let gain = p.gain * volume;
            for (out, s) in frame.iter_mut().zip(&samples[..n]) {
                *out += s * gain;
            }
            p.at += n;
        }
        self.playing.retain(|p| p.at < p.cue.samples().len());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One Ogg page holding `packets`, as RFC 3533 lays it out.
    fn page(packets: &[&[u8]], granule: u64) -> Vec<u8> {
        let mut table = Vec::new();
        let mut body = Vec::new();
        for p in packets {
            let mut left = p.len();
            while left >= 255 {
                table.push(255);
                left -= 255;
            }
            table.push(left as u8);
            body.extend_from_slice(p);
        }
        let mut page = b"OggS".to_vec();
        page.push(0); // version
        page.push(0); // flags
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&[0; 12]); // serial, sequence, checksum
        page.push(table.len() as u8);
        page.extend_from_slice(&table);
        page.extend_from_slice(&body);
        page
    }

    fn head(pre_skip: u16) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]); // gain, mapping family
        head
    }

    #[test]
    fn splits_packets_across_segments() {
        let long = vec![7u8; 600];
        let file = [page(&[b"a", &long], 0), page(&[b"bc"], 42)].concat();
        let packets = ogg_packets(&file).unwrap();
        assert_eq!(packets.packets.len(), 3);
        assert_eq!(packets.packets[1], long);
        assert_eq!(packets.packets[2], b"bc");
        assert_eq!(packets.last_granule, Some(42));
    }

    #[test]
    fn decodes_and_trims_an_opus_stream() {
        // Half a second of a tone, encoded the way an Opus file would be.
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Audio)
                .unwrap();
        let pre_skip = encoder.get_lookahead().unwrap() as u16;
        let frame: usize = 960; // 20 ms
        let total: usize = 24_000;
        let mut audio = Vec::new();
        let mut out = [0u8; 1000];
        for f in 0..total.div_ceil(frame) + 1 {
            let pcm: Vec<f32> = (0..frame)
                .map(|i| {
                    let n = (f * frame + i) as f32;
                    0.3 * (std::f32::consts::TAU * 440.0 * n / SAMPLE_RATE as f32).sin()
                })
                .collect();
            let len = encoder.encode_float(&pcm, &mut out).unwrap();
            audio.push(out[..len].to_vec());
        }
        let audio: Vec<&[u8]> = audio.iter().map(Vec::as_slice).collect();
        let file = [
            page(&[&head(pre_skip)], 0),
            page(&[b"OpusTags\0\0\0\0\0\0\0\0"], 0),
            page(&audio, (total + pre_skip as usize) as u64),
        ]
        .concat();
        let cue = Cue::from_ogg_opus(&file).unwrap();
        assert_eq!(cue.samples().len(), total);
        let peak = cue.samples().iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.2..0.4).contains(&peak), "{peak}");
    }

    #[test]
    fn rejects_what_isnt_ogg_opus() {
        assert!(Cue::from_ogg_opus(b"RIFF....").is_err());
        assert!(Cue::from_ogg_opus(&page(&[b"OpusTags"], 0)).is_err());
    }

    #[test]
    fn cues_mix_until_they_end() {
        let mut cues = Cues::default();
        cues.play(Cue::from_samples(vec![1.0; FRAME_SAMPLES + 10]), 0.5);
        let mut frame = [0.1; FRAME_SAMPLES];
        assert!(cues.mix(&mut frame, 0.5));
        assert_eq!(frame[0], 0.1 + 0.25);
        let mut frame = [0.0; FRAME_SAMPLES];
        assert!(cues.mix(&mut frame, 1.0));
        assert_eq!(frame[9], 0.5);
        assert_eq!(frame[10], 0.0);
        assert!(cues.is_empty());
        assert!(!cues.mix(&mut frame, 1.0));
    }
}

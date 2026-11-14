//! Voice packet formats, shared by UDP and the TCP tunnel.
//!
//! Mumble 1.5 introduced a protobuf format (`MumbleUDP.proto`): a type byte
//! (0 = audio, 1 = ping) followed by the message. Older servers, and 1.5
//! servers talking to older clients, use the legacy format: a header byte
//! `(codec << 5) | target` and Mumble's own variable-length integers. See
//! <https://github.com/mumble-voip/mumble/blob/master/docs/dev/network-protocol/voice_data.md>.
//!
//! Received packets are recognized by their first byte, so decoding needs no
//! state. Sending follows the negotiated [`VoiceFormat`].

use prost::Message;

use crate::{ServerToClientUdpMessage, UdpMessageId, raw, raw_udp};

/// Legacy codec numbers (the top three bits of the header byte).
const LEGACY_PING: u8 = 1;
const LEGACY_OPUS: u8 = 4;

/// The legacy Opus length field: 13 bits of size, then the terminator bit.
const LEGACY_SIZE_MASK: u64 = 0x1FFF;
const LEGACY_TERMINATOR: u64 = 0x2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceFormat {
    /// Mumble 1.5+.
    Protobuf,
    /// Before 1.5.
    Legacy,
}

impl VoiceFormat {
    /// The first version that speaks the protobuf format.
    pub const PROTOBUF_SINCE: u64 = 1 << 48 | 5 << 32;

    /// The format to use with a server, given its `Version`. Servers
    /// that only report the old version field are treated as legacy.
    pub fn for_server(version: &raw::Version) -> Self {
        match version.version_v2 {
            Some(v2) if v2 >= Self::PROTOBUF_SINCE => Self::Protobuf,
            _ => Self::Legacy,
        }
    }

    /// Encodes outgoing voice into `out` (cleared first).
    pub fn encode_audio(self, audio: &raw_udp::Audio, out: &mut Vec<u8>) {
        out.clear();
        match self {
            Self::Protobuf => {
                out.push(raw_udp::Audio::TYPE_ID);
                audio.encode(out).expect("Vec<u8> grows as needed");
            }
            Self::Legacy => {
                let target = match audio.header {
                    Some(raw_udp::audio::Header::Target(t)) => (t & 0x1F) as u8,
                    _ => 0,
                };
                out.push(LEGACY_OPUS << 5 | target);
                write_varint(out, audio.frame_number);
                let mut size = audio.opus_data.len() as u64 & LEGACY_SIZE_MASK;
                if audio.is_terminator {
                    size |= LEGACY_TERMINATOR;
                }
                write_varint(out, size);
                out.extend_from_slice(&audio.opus_data);
                if let [x, y, z, ..] = audio.positional_data[..] {
                    for v in [x, y, z] {
                        out.extend_from_slice(&v.to_be_bytes());
                    }
                }
            }
        }
    }

    /// Encodes a connectivity ping into `out` (cleared first).
    pub fn encode_ping(self, timestamp: u64, out: &mut Vec<u8>) {
        out.clear();
        match self {
            Self::Protobuf => {
                out.push(raw_udp::Ping::TYPE_ID);
                raw_udp::Ping {
                    timestamp,
                    ..Default::default()
                }
                .encode(out)
                .expect("Vec<u8> grows as needed");
            }
            Self::Legacy => {
                out.push(LEGACY_PING << 5);
                write_varint(out, timestamp);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VoiceDecodeError {
    #[error("empty voice packet")]
    Empty,
    #[error("truncated voice packet")]
    Truncated,
    #[error("unsupported codec {0} (only Opus is supported)")]
    UnsupportedCodec(u8),
    #[error("malformed protobuf voice packet: {0}")]
    Protobuf(#[from] prost::DecodeError),
}

/// Decodes a voice packet the server sent, in either format.
pub fn decode(packet: &[u8]) -> Result<ServerToClientUdpMessage, VoiceDecodeError> {
    let (&first, rest) = packet.split_first().ok_or(VoiceDecodeError::Empty)?;
    match first {
        // Protobuf type bytes. Legacy CELT-alpha packets could also start
        // with these, but servers only send CELT to clients that offer it.
        raw_udp::Audio::TYPE_ID => Ok(ServerToClientUdpMessage::Audio(raw_udp::Audio::decode(
            rest,
        )?)),
        raw_udp::Ping::TYPE_ID => Ok(ServerToClientUdpMessage::Ping(raw_udp::Ping::decode(rest)?)),
        _ => decode_legacy(first, rest),
    }
}

fn decode_legacy(header: u8, body: &[u8]) -> Result<ServerToClientUdpMessage, VoiceDecodeError> {
    let mut reader = Reader(body);
    match header >> 5 {
        LEGACY_PING => Ok(ServerToClientUdpMessage::Ping(raw_udp::Ping {
            timestamp: reader.varint()?,
            ..Default::default()
        })),
        LEGACY_OPUS => {
            let sender_session = reader.varint()? as u32;
            let frame_number = reader.varint()?;
            let size = reader.varint()?;
            let opus_data = reader.bytes((size & LEGACY_SIZE_MASK) as usize)?.to_vec();
            let positional_data = match reader.0.len() >= 12 {
                true => (0..3).map(|_| reader.f32_be()).collect::<Result<_, _>>()?,
                false => Vec::new(),
            };
            Ok(ServerToClientUdpMessage::Audio(raw_udp::Audio {
                header: Some(raw_udp::audio::Header::Context(u32::from(header & 0x1F))),
                sender_session,
                frame_number,
                opus_data,
                positional_data,
                is_terminator: size & LEGACY_TERMINATOR != 0,
                ..Default::default()
            }))
        }
        codec => Err(VoiceDecodeError::UnsupportedCodec(codec)),
    }
}

/// Writes Mumble's variable-length integer (PacketDataStream). Only the
/// non-negative forms are needed: sessions, sequence numbers, timestamps.
fn write_varint(out: &mut Vec<u8>, value: u64) {
    match value {
        0..0x80 => out.push(value as u8),
        0x80..0x4000 => out.extend_from_slice(&[(value >> 8) as u8 | 0x80, value as u8]),
        0x4000..0x20_0000 => {
            out.extend_from_slice(&[(value >> 16) as u8 | 0xC0, (value >> 8) as u8, value as u8])
        }
        0x20_0000..0x1000_0000 => out.extend_from_slice(&[
            (value >> 24) as u8 | 0xE0,
            (value >> 16) as u8,
            (value >> 8) as u8,
            value as u8,
        ]),
        0x1000_0000..0x1_0000_0000 => {
            out.push(0xF0);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(0xF4);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], VoiceDecodeError> {
        if self.0.len() < n {
            return Err(VoiceDecodeError::Truncated);
        }
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u64, VoiceDecodeError> {
        Ok(u64::from(self.bytes(1)?[0]))
    }

    fn be(&mut self, n: usize) -> Result<u64, VoiceDecodeError> {
        Ok(self
            .bytes(n)?
            .iter()
            .fold(0, |acc, &b| acc << 8 | u64::from(b)))
    }

    fn f32_be(&mut self) -> Result<f32, VoiceDecodeError> {
        Ok(f32::from_bits(self.be(4)? as u32))
    }

    /// Reads Mumble's variable-length integer. Negative forms decode to their
    /// two's-complement bit pattern, as Mumble's own `quint64` reads do.
    fn varint(&mut self) -> Result<u64, VoiceDecodeError> {
        let v = self.byte()?;
        Ok(if v & 0x80 == 0 {
            v & 0x7F
        } else if v & 0xC0 == 0x80 {
            (v & 0x3F) << 8 | self.byte()?
        } else if v & 0xF0 == 0xF0 {
            match v & 0xFC {
                0xF0 => self.be(4)?,
                0xF4 => self.be(8)?,
                0xF8 => !self.varint()?,
                _ => !(v & 0x03),
            }
        } else if v & 0xF0 == 0xE0 {
            (v & 0x0F) << 24 | self.be(3)?
        } else {
            (v & 0x1F) << 16 | self.be(2)?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_bytes(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint(&mut out, value);
        out
    }

    #[test]
    fn varints_use_mumbles_encoding() {
        assert_eq!(varint_bytes(0x05), [0x05]);
        assert_eq!(varint_bytes(0x2003), [0xA0, 0x03]);
        assert_eq!(varint_bytes(0x12_3456), [0xD2, 0x34, 0x56]);
        assert_eq!(varint_bytes(0x0123_4567), [0xE1, 0x23, 0x45, 0x67]);
        assert_eq!(varint_bytes(0xDEAD_BEEF), [0xF0, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            varint_bytes(u64::MAX - 1),
            [0xF4, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
        );
    }

    #[test]
    fn varints_round_trip_at_every_boundary() {
        for value in [
            0,
            0x7F,
            0x80,
            0x3FFF,
            0x4000,
            0x1F_FFFF,
            0x20_0000,
            0x0FFF_FFFF,
            0x1000_0000,
            0xFFFF_FFFF,
            0x1_0000_0000,
            u64::MAX,
        ] {
            let bytes = varint_bytes(value);
            let mut reader = Reader(&bytes);
            assert_eq!(reader.varint().unwrap(), value, "{value:#x}");
            assert!(reader.0.is_empty(), "{value:#x} left bytes over");
        }
    }

    #[test]
    fn negative_varints_decode_like_mumble() {
        // 0xFC | n is the short form of !n (-1 to -4); 0xF8 prefixes !next.
        assert_eq!(Reader(&[0xFC]).varint().unwrap(), !0);
        assert_eq!(Reader(&[0xFE]).varint().unwrap(), !2);
        assert_eq!(Reader(&[0xF8, 0x05]).varint().unwrap(), !5);
    }

    #[test]
    fn legacy_audio_matches_the_documented_layout() {
        let audio = raw_udp::Audio {
            header: Some(raw_udp::audio::Header::Target(0)),
            frame_number: 5,
            opus_data: vec![1, 2, 3],
            is_terminator: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        VoiceFormat::Legacy.encode_audio(&audio, &mut out);
        // Opus to target 0, sequence 5, length 3 with the terminator bit.
        assert_eq!(out, [0x80, 0x05, 0xA0, 0x03, 1, 2, 3]);
    }

    #[test]
    fn legacy_audio_from_the_server_decodes() {
        // Opus, context 2 (whisper), session 7, sequence 300, 2 bytes, then
        // positional data.
        let mut packet = vec![0x82, 0x07, 0x81, 0x2C, 0x02, 0xAA, 0xBB];
        for v in [1.0f32, 2.0, 3.0] {
            packet.extend_from_slice(&v.to_be_bytes());
        }
        let ServerToClientUdpMessage::Audio(audio) = decode(&packet).unwrap() else {
            panic!("expected audio");
        };
        assert_eq!(
            audio,
            raw_udp::Audio {
                header: Some(raw_udp::audio::Header::Context(2)),
                sender_session: 7,
                frame_number: 300,
                opus_data: vec![0xAA, 0xBB],
                positional_data: vec![1.0, 2.0, 3.0],
                is_terminator: false,
                ..Default::default()
            }
        );
    }

    #[test]
    fn legacy_ping_round_trips() {
        let mut out = Vec::new();
        VoiceFormat::Legacy.encode_ping(123_456_789, &mut out);
        assert_eq!(out[0], 0x20);
        let ServerToClientUdpMessage::Ping(ping) = decode(&out).unwrap() else {
            panic!("expected ping");
        };
        assert_eq!(ping.timestamp, 123_456_789);
    }

    #[test]
    fn protobuf_packets_round_trip() {
        let audio = raw_udp::Audio {
            sender_session: 3,
            frame_number: 42,
            opus_data: vec![9, 9],
            is_terminator: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        VoiceFormat::Protobuf.encode_audio(&audio, &mut out);
        assert!(matches!(decode(&out).unwrap(), ServerToClientUdpMessage::Audio(a) if a == audio));

        VoiceFormat::Protobuf.encode_ping(77, &mut out);
        assert!(
            matches!(decode(&out).unwrap(), ServerToClientUdpMessage::Ping(p) if p.timestamp == 77)
        );
    }

    #[test]
    fn bad_packets_are_errors_not_panics() {
        assert_eq!(decode(&[]).unwrap_err(), VoiceDecodeError::Empty);
        assert_eq!(
            decode(&[0x80, 0x07]).unwrap_err(),
            VoiceDecodeError::Truncated
        );
        // Claims 3 bytes of Opus, has 1.
        assert_eq!(
            decode(&[0x80, 0x07, 0x01, 0x03, 0xAA]).unwrap_err(),
            VoiceDecodeError::Truncated
        );
        // Speex.
        assert_eq!(
            decode(&[0x40, 0x07]).unwrap_err(),
            VoiceDecodeError::UnsupportedCodec(2)
        );
    }

    #[test]
    fn server_version_picks_the_format() {
        let version = |v2| raw::Version {
            version_v2: v2,
            ..Default::default()
        };
        assert_eq!(
            VoiceFormat::for_server(&version(Some(1 << 48 | 5 << 32 | 735 << 16))),
            VoiceFormat::Protobuf
        );
        assert_eq!(
            VoiceFormat::for_server(&version(Some(1 << 48 | 4 << 32 | 287 << 16))),
            VoiceFormat::Legacy
        );
        assert_eq!(VoiceFormat::for_server(&version(None)), VoiceFormat::Legacy);
    }
}

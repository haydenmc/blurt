//! A per-speaker jitter buffer, keyed by Mumble's `frame_number`.
//!
//! Packets arrive with network jitter, possibly out of order or not at all.
//! The buffer holds a talk spurt back by a small target delay before playing
//! it, then plays one 10 ms frame per [`JitterBuffer::pull`], in frame order:
//!
//! - A missing frame is rebuilt from the next packet's forward error
//!   correction (FEC) if that packet is here, or else concealed (PLC).
//! - A packet that arrives after its frame was played is dropped.
//! - The delay adapts between talk spurts: up after a spurt that ran dry, and
//!   slowly back down after spurts that didn't. There is no time-stretching
//!   within a spurt, so audio is never sped up or slowed down (which shifts
//!   pitch).
//!
//! This is the same approach as Mumble's own client.

use std::collections::BTreeMap;

use crate::{FRAME_SAMPLES, MAX_PACKET_SAMPLES};

/// Decodes one speaker's packets. Implemented by Opus; faked in tests.
pub trait FrameDecoder {
    /// How many 10 ms frames a packet holds, or `None` if it is malformed.
    fn frames_in(&self, packet: &[u8]) -> Option<usize>;
    /// Decodes a packet into `out` and returns the samples written. With
    /// `fec`, decodes instead the packet's redundant copy of the frame
    /// before it, filling all of `out`.
    fn decode(&mut self, packet: &[u8], out: &mut [f32], fec: bool) -> Option<usize>;
    /// Fills `out` with concealment for a lost frame.
    fn conceal(&mut self, out: &mut [f32]);
}

/// Jitter buffer delays, in 10 ms frames.
#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    pub initial_delay: usize,
    pub min_delay: usize,
    pub max_delay: usize,
    /// Frames to conceal when a spurt runs dry without a terminator, before
    /// treating the speaker as gone quiet.
    pub max_conceal: usize,
    /// Buffered audio beyond this is dropped to bound latency.
    pub max_buffered: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            initial_delay: 3,
            min_delay: 2,
            max_delay: 20,
            max_conceal: 5,
            max_buffered: 40,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    /// Frames played from packets.
    pub played: u64,
    /// Lost frames rebuilt from FEC.
    pub recovered: u64,
    /// Lost frames concealed.
    pub concealed: u64,
    /// Packets that arrived after their time.
    pub late: u64,
    /// Packets dropped because too much was buffered.
    pub overflowed: u64,
}

struct Packet {
    data: Vec<u8>,
    frames: usize,
    terminator: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not talking.
    Idle,
    /// A spurt started; waiting until `target` frames are buffered.
    Buffering {
        waited: usize,
    },
    Playing,
}

pub struct JitterBuffer<D> {
    decoder: D,
    config: JitterConfig,
    packets: BTreeMap<u64, Packet>,
    state: State,
    /// The frame number to play next.
    cursor: u64,
    /// Decoded audio not played yet.
    pcm: Box<[f32; MAX_PACKET_SAMPLES]>,
    pcm_len: usize,
    pcm_pos: usize,
    /// Current target delay, in frames.
    target: usize,
    /// Consecutive frames concealed.
    concealing: usize,
    /// The spurt's last packet (a terminator) has been played.
    ended: bool,
    /// The current spurt ran out of audio before its end.
    ran_dry: bool,
    smooth_spurts: usize,
    pub stats: JitterStats,
}

impl<D: FrameDecoder> JitterBuffer<D> {
    pub fn new(decoder: D, config: JitterConfig) -> Self {
        Self {
            decoder,
            config,
            packets: BTreeMap::new(),
            state: State::Idle,
            cursor: 0,
            pcm: Box::new([0.0; MAX_PACKET_SAMPLES]),
            pcm_len: 0,
            pcm_pos: 0,
            target: config.initial_delay,
            concealing: 0,
            ended: false,
            ran_dry: false,
            smooth_spurts: 0,
            stats: JitterStats::default(),
        }
    }

    /// The current target delay, in frames.
    pub fn target_delay(&self) -> usize {
        self.target
    }

    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// Buffered audio, in frames.
    pub fn buffered_frames(&self) -> usize {
        self.packets.values().map(|p| p.frames).sum::<usize>()
            + (self.pcm_len - self.pcm_pos) / FRAME_SAMPLES
    }

    /// Adds a packet. An empty `data` with `terminator` just ends the spurt.
    pub fn push(&mut self, frame_number: u64, data: Vec<u8>, terminator: bool) {
        let frames = if data.is_empty() {
            0
        } else {
            match self.decoder.frames_in(&data) {
                Some(frames) if frames > 0 => frames,
                _ => {
                    log::debug!("Dropping a malformed voice packet");
                    return;
                }
            }
        };

        match self.state {
            State::Idle => self.start_spurt(frame_number),
            // Way behind or ahead of where we are: the sender restarted its
            // counter, or went quiet without a terminator. A new spurt.
            _ if frame_number + 100 < self.cursor || frame_number > self.cursor + 500 => {
                self.reset();
                self.start_spurt(frame_number);
            }
            State::Buffering { .. } => self.cursor = self.cursor.min(frame_number),
            State::Playing if frame_number < self.cursor => {
                self.stats.late += 1;
                return;
            }
            State::Playing => {}
        }

        self.packets.insert(
            frame_number,
            Packet {
                data,
                frames,
                terminator,
            },
        );

        // Bound the latency: drop the oldest audio.
        while self.buffered_frames() > self.config.max_buffered {
            let Some((frame, packet)) = self.packets.pop_first() else {
                break;
            };
            self.stats.overflowed += 1;
            self.cursor = frame + packet.frames as u64;
        }
    }

    /// Plays the next 10 ms into `out`. Returns `false` (and leaves `out`
    /// untouched) when there is nothing to play.
    pub fn pull(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> bool {
        if let State::Buffering { waited } = self.state {
            let ready = self.buffered_frames() >= self.target
                || self.packets.values().any(|p| p.terminator)
                // Don't wait forever on a slow start.
                || waited >= self.target * 2;
            if !ready {
                self.state = State::Buffering { waited: waited + 1 };
                return false;
            }
            self.state = State::Playing;
            if let Some(&first) = self.packets.keys().next() {
                self.cursor = first;
            }
        }
        if self.state != State::Playing {
            return false;
        }

        let mut filled = 0;
        while filled < FRAME_SAMPLES {
            if self.pcm_pos == self.pcm_len && !self.fetch() {
                if filled == 0 {
                    return false;
                }
                out[filled..].fill(0.0);
                break;
            }
            let n = (self.pcm_len - self.pcm_pos).min(FRAME_SAMPLES - filled);
            out[filled..filled + n].copy_from_slice(&self.pcm[self.pcm_pos..self.pcm_pos + n]);
            self.pcm_pos += n;
            filled += n;
        }
        true
    }

    /// Decodes or conceals the next frame(s) into `pcm`. Returns `false` when
    /// the spurt is over.
    fn fetch(&mut self) -> bool {
        self.pcm_pos = 0;
        self.pcm_len = 0;

        if let Some(packet) = self.packets.remove(&self.cursor) {
            self.cursor += packet.frames.max(1) as u64;
            self.concealing = 0;
            if packet.terminator {
                self.ended = true;
            }
            if packet.data.is_empty() {
                return self.fetch_after_end();
            }
            let samples = packet.frames * FRAME_SAMPLES;
            match self
                .decoder
                .decode(&packet.data, &mut self.pcm[..samples], false)
            {
                Some(n) => {
                    self.pcm_len = n;
                    self.stats.played += packet.frames as u64;
                }
                None => {
                    self.decoder.conceal(&mut self.pcm[..samples]);
                    self.pcm_len = samples;
                    self.stats.concealed += packet.frames as u64;
                }
            }
            return true;
        }

        if self.packets.is_empty() {
            return self.fetch_after_end();
        }

        // A gap: the packet for this frame is lost (or very late).
        let next = *self.packets.keys().next().unwrap();
        let frame = &mut self.pcm[..FRAME_SAMPLES];
        if next == self.cursor + 1
            && self
                .decoder
                .decode(&self.packets[&next].data, frame, true)
                .is_some()
        {
            self.stats.recovered += 1;
        } else {
            self.decoder.conceal(frame);
            self.stats.concealed += 1;
        }
        self.pcm_len = FRAME_SAMPLES;
        self.cursor += 1;
        true
    }

    /// Nothing buffered: the spurt ended, or the buffer ran dry.
    fn fetch_after_end(&mut self) -> bool {
        if self.ended || self.concealing >= self.config.max_conceal {
            self.state = State::Idle;
            return false;
        }
        // Ran dry mid-spurt: conceal briefly in case packets are just late.
        self.ran_dry = true;
        self.concealing += 1;
        self.decoder.conceal(&mut self.pcm[..FRAME_SAMPLES]);
        self.pcm_len = FRAME_SAMPLES;
        self.cursor += 1;
        self.stats.concealed += 1;
        true
    }

    fn start_spurt(&mut self, frame_number: u64) {
        // Adapt the delay to how the last spurt went.
        if self.ran_dry {
            self.target = (self.target + 1).min(self.config.max_delay);
            self.smooth_spurts = 0;
        } else {
            self.smooth_spurts += 1;
            if self.smooth_spurts >= 3 {
                self.target = self.target.saturating_sub(1).max(self.config.min_delay);
                self.smooth_spurts = 0;
            }
        }
        self.ran_dry = false;
        self.ended = false;
        self.concealing = 0;
        self.cursor = frame_number;
        self.state = State::Buffering { waited: 0 };
    }

    fn reset(&mut self) {
        self.packets.clear();
        self.pcm_len = 0;
        self.pcm_pos = 0;
        self.state = State::Idle;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake codec: a packet is `[frames, value]`, decoding to `frames`
    /// frames of `value`. FEC recovers `-value`; concealment plays 9.0.
    pub struct FakeDecoder;

    pub const CONCEALED: f32 = 9.0;

    impl FrameDecoder for FakeDecoder {
        fn frames_in(&self, packet: &[u8]) -> Option<usize> {
            packet.first().map(|&f| usize::from(f))
        }
        fn decode(&mut self, packet: &[u8], out: &mut [f32], fec: bool) -> Option<usize> {
            let value = f32::from(packet[1]);
            if fec {
                out.fill(-value);
                return Some(out.len());
            }
            let n = usize::from(packet[0]) * FRAME_SAMPLES;
            out[..n].fill(value);
            Some(n)
        }
        fn conceal(&mut self, out: &mut [f32]) {
            out.fill(CONCEALED);
        }
    }

    fn buffer() -> JitterBuffer<FakeDecoder> {
        JitterBuffer::new(FakeDecoder, JitterConfig::default())
    }

    /// One 10 ms packet whose audio is `value`.
    fn packet(value: u8) -> Vec<u8> {
        vec![1, value]
    }

    /// Pulls `n` frames, returning the first sample of each (`None` when silent).
    fn pull(jb: &mut JitterBuffer<FakeDecoder>, n: usize) -> Vec<Option<f32>> {
        (0..n)
            .map(|_| {
                let mut out = [0.0; FRAME_SAMPLES];
                jb.pull(&mut out).then_some(out[0])
            })
            .collect()
    }

    #[test]
    fn waits_for_the_target_delay_then_plays_in_order() {
        let mut jb = buffer();
        jb.push(100, packet(1), false);
        assert_eq!(pull(&mut jb, 1), [None], "buffering");
        jb.push(101, packet(2), false);
        jb.push(102, packet(3), true);
        assert_eq!(pull(&mut jb, 4), [Some(1.0), Some(2.0), Some(3.0), None]);
        assert!(jb.is_idle());
    }

    #[test]
    fn reorders_packets() {
        let mut jb = buffer();
        jb.push(11, packet(2), false);
        jb.push(10, packet(1), false);
        jb.push(12, packet(3), true);
        assert_eq!(pull(&mut jb, 3), [Some(1.0), Some(2.0), Some(3.0)]);
    }

    #[test]
    fn a_lost_packet_is_recovered_from_the_next_ones_fec() {
        let mut jb = buffer();
        jb.push(0, packet(1), false);
        // Frame 1 is lost.
        jb.push(2, packet(3), false);
        jb.push(3, packet(4), true);
        assert_eq!(
            pull(&mut jb, 4),
            [Some(1.0), Some(-3.0), Some(3.0), Some(4.0)]
        );
        assert_eq!(jb.stats.recovered, 1);
    }

    #[test]
    fn a_longer_gap_is_concealed() {
        let mut jb = buffer();
        jb.push(0, packet(1), false);
        jb.push(3, packet(4), false);
        jb.push(4, packet(5), true);
        assert_eq!(
            pull(&mut jb, 5),
            [Some(1.0), Some(CONCEALED), Some(-4.0), Some(4.0), Some(5.0)]
        );
        assert_eq!(jb.stats.concealed, 1);
        assert_eq!(jb.stats.recovered, 1);
    }

    #[test]
    fn late_packets_are_dropped() {
        let mut jb = buffer();
        for f in 0..3 {
            jb.push(f, packet(f as u8 + 1), false);
        }
        assert_eq!(pull(&mut jb, 3), [Some(1.0), Some(2.0), Some(3.0)]);
        jb.push(1, packet(7), false);
        assert_eq!(jb.stats.late, 1);
    }

    #[test]
    fn multi_frame_packets_play_out_over_several_pulls() {
        let mut jb = buffer();
        // 20 ms packets, as the official client sends by default.
        jb.push(0, vec![2, 1], false);
        jb.push(2, vec![2, 2], true);
        assert_eq!(
            pull(&mut jb, 5),
            [Some(1.0), Some(1.0), Some(2.0), Some(2.0), None]
        );
    }

    #[test]
    fn running_dry_conceals_briefly_then_goes_quiet() {
        let mut jb = buffer();
        for f in 0..3 {
            jb.push(f, packet(1), false);
        }
        let played = pull(&mut jb, 3 + 5 + 1);
        assert_eq!(&played[..3], [Some(1.0); 3]);
        assert_eq!(&played[3..8], [Some(CONCEALED); 5]);
        assert_eq!(played[8], None);
        assert!(jb.is_idle());
    }

    #[test]
    fn the_delay_grows_after_running_dry_and_shrinks_after_smooth_spurts() {
        let mut jb = buffer();
        let start = jb.target_delay();

        // A spurt that runs dry...
        for f in 0..3 {
            jb.push(f, packet(1), false);
        }
        pull(&mut jb, 20);
        // ...makes the next spurt wait longer.
        jb.push(1000, packet(1), true);
        assert_eq!(jb.target_delay(), start + 1);
        pull(&mut jb, 10);

        // Smooth spurts bring it back down.
        let mut frame = 2000;
        for _ in 0..3 {
            jb.push(frame, packet(1), true);
            pull(&mut jb, 10);
            frame += 1000;
        }
        jb.push(frame, packet(1), true);
        assert_eq!(jb.target_delay(), start);
    }

    #[test]
    fn excess_buffering_is_dropped_to_bound_latency() {
        let mut jb = buffer();
        let max = JitterConfig::default().max_buffered as u64;
        for f in 0..max + 10 {
            jb.push(f, packet(1), false);
        }
        assert_eq!(jb.buffered_frames() as u64, max);
        assert_eq!(jb.stats.overflowed, 10);
    }

    #[test]
    fn a_restarted_counter_starts_a_new_spurt() {
        let mut jb = buffer();
        for f in 5000..5003 {
            jb.push(f, packet(1), false);
        }
        pull(&mut jb, 2);
        // The sender reconnected and counts from zero again.
        jb.push(0, packet(2), false);
        jb.push(1, packet(2), false);
        jb.push(2, packet(2), true);
        assert_eq!(pull(&mut jb, 3), [Some(2.0); 3]);
    }

    #[test]
    fn an_empty_terminator_just_ends_the_spurt() {
        let mut jb = buffer();
        jb.push(0, packet(1), false);
        jb.push(1, packet(1), false);
        jb.push(2, Vec::new(), true);
        assert_eq!(pull(&mut jb, 3), [Some(1.0), Some(1.0), None]);
        assert!(jb.is_idle());
    }
}

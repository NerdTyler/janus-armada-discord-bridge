//! The Janus PCM socket protocol + mixer core.
//!
//! Wire format (see `shared/pcm-protocol.md` for the full
//! document): a Unix stream socket carrying length-prefixed
//! frames — `u32 LE length` (header + payload), a fixed
//! 24-byte header, then the payload. PCM payloads are exactly
//! one 20 ms frame of 48 kHz mono s16le (960 samples, 1,920
//! bytes).
//!
//! NOTE on the header size: the design spec (bridge spec
//! §5.3) calls the header "a fixed 16-byte header" but then
//! lists fields that cannot fit in 16 bytes (magic + version
//! + type + flags + speaker u64 + seq u32 + timestamp u32 +
//! samples u16 + energy u8 = 23 bytes minimum). This crate
//! implements a 24-byte header (the listed fields + 1 reserved
//! byte) and the wire doc records the deviation.
//!
//! Backpressure: none, by design. Every queue in this crate is
//! a bounded drop-oldest ring; audio that is 200 ms late is
//! worse than no audio.

use std::collections::{HashMap, VecDeque};

/// Samples per 20 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
/// Bytes per PCM frame payload (s16le mono).
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// Fixed header length on the wire.
pub const HEADER_LEN: usize = 24;
/// Hard cap on one framed message (header + payload). CONTROL
/// frames carry JSON and are the only frames allowed past the
/// PCM size; anything larger is a protocol violation.
pub const MAX_FRAME_LEN: usize = 64 * 1024;
/// Jitter buffer target depth (frames) per source: 60 ms.
pub const JITTER_TARGET: usize = 3;
/// Jitter buffer max depth (frames) per source: 200 ms.
pub const JITTER_MAX: usize = 10;
/// Underrun ride-through (2026-11-23, diagnosis round 2):
/// when a flowing source's buffer runs dry for a tick, the
/// buffer emits a concealment frame (the last real frame,
/// decayed) instead of going silent and demanding a full
/// re-prebuffer. At most this many consecutive ticks
/// (5 × 20 ms = 100 ms) are ridden through; a source dry
/// longer than that has genuinely stopped and re-prebuffers
/// as before. Rationale, measured in
/// tests/robotic_harness.rs: the Discord receive path has
/// NO packet-loss concealment anywhere — @discordjs/voice
/// pushes received packets in arrival order with no
/// sequence check, and prism decodes what it gets, so one
/// lost packet compresses the decoded timeline by 20 ms.
/// Against a fixed 20 ms mixer tick the per-source buffer
/// then drains at the loss rate and, as built, ONE 20 ms
/// hole became a ≥60 ms mute (underrun + 3-frame
/// re-prebuffer): 26 hole events / 1000 ms of silence per
/// 3 min of speech at 1% packet loss. Ride-through
/// eliminated every measured hole in every scenario
/// (0–2% loss, jitter spikes, simulated CPU stalls,
/// two speakers) at zero added latency.
pub const UNDERRUN_RIDE: usize = 5;
/// Per-tick amplitude decay of concealment frames
/// (0.7, 0.49, 0.34, …): a fading repeat, like standard
/// Opus PLC, not a frozen loop.
pub const CONCEAL_DECAY: f32 = 0.7;
/// Ring capacity for socket write queues (frames): 200 ms.
pub const RING_CAPACITY: usize = 10;

pub const MAGIC: u8 = b'B';
pub const VERSION: u8 = 1;

/// Frame types on the socket (spec §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// Discord → mixer, per-user PCM.
    PcmDown = 1,
    /// Armada → mixer, per-participant PCM.
    PcmUp = 2,
    /// Mixer → a leg: that leg's incoming mix.
    MixOut = 3,
    /// Speaking start/stop for `speaker_id` (flags bit0 = start).
    Speak = 4,
    /// Control plane: UTF-8 JSON payload.
    Control = 5,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::PcmDown,
            2 => Self::PcmUp,
            3 => Self::MixOut,
            4 => Self::Speak,
            5 => Self::Control,
            _ => return None,
        })
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameType,
    pub flags: u8,
    /// Discord user id, FNV-1a hash of the SFU identity, or 0
    /// for mixed/control frames.
    pub speaker_id: u64,
    /// Per-sender sequence (gaps = loss).
    pub seq: u32,
    /// Sender's frame clock, ms.
    pub timestamp_ms: u32,
    /// Sample count for PCM frames (960); 0 otherwise.
    pub samples: u16,
    /// RMS energy bucket 0–255 (logging / speaking display).
    pub energy: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn pcm(kind: FrameType, speaker_id: u64, seq: u32, timestamp_ms: u32, pcm: &[i16]) -> Self {
        let mut payload = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            payload.extend_from_slice(&s.to_le_bytes());
        }
        Self {
            kind,
            flags: 0,
            speaker_id,
            seq,
            timestamp_ms,
            samples: pcm.len() as u16,
            energy: energy_bucket(pcm),
            payload,
        }
    }

    pub fn control(json: &str) -> Self {
        Self {
            kind: FrameType::Control,
            flags: 0,
            speaker_id: 0,
            seq: 0,
            timestamp_ms: 0,
            samples: 0,
            energy: 0,
            payload: json.as_bytes().to_vec(),
        }
    }

    pub fn speak(speaker_id: u64, start: bool) -> Self {
        Self {
            kind: FrameType::Speak,
            flags: if start { 1 } else { 0 },
            speaker_id,
            seq: 0,
            timestamp_ms: 0,
            samples: 0,
            energy: 0,
            payload: Vec::new(),
        }
    }

    /// The PCM samples of a PCM frame (payload decoded as
    /// s16le). Empty for non-PCM frames.
    pub fn pcm_samples(&self) -> Vec<i16> {
        self.payload
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// Serialize: `u32 LE length || 24-byte header || payload`.
    pub fn encode(&self) -> Vec<u8> {
        let total = HEADER_LEN + self.payload.len();
        let mut out = Vec::with_capacity(4 + total);
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.push(MAGIC);
        out.push(VERSION);
        out.push(self.kind as u8);
        out.push(self.flags);
        out.extend_from_slice(&self.speaker_id.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        out.extend_from_slice(&self.samples.to_le_bytes());
        out.push(self.energy);
        out.push(0); // reserved
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Incremental frame decoder for a stream socket. Feed bytes
/// as they arrive; pull whole frames out. On a protocol
/// violation (bad magic/version, absurd length) the decoder
/// drops ONE byte and resynchronizes on the next plausible
/// frame start — a stream socket has no other framing to
/// recover to, and a garbage byte must not kill the bridge.
#[derive(Default)]
pub struct StreamDecoder {
    buf: Vec<u8>,
    /// Frames dropped to resynchronization (diagnostics).
    pub resyncs: u64,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, if one is buffered.
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if self.buf.len() < 4 {
                return None;
            }
            let total = u32::from_le_bytes(self.buf[0..4].try_into().unwrap()) as usize;
            if !(HEADER_LEN..=MAX_FRAME_LEN).contains(&total) {
                self.drop_one();
                continue;
            }
            // Validate the header as soon as it is buffered —
            // BEFORE waiting for the whole frame. Otherwise a
            // garbage prefix can fake an in-range length and
            // the decoder waits forever for bytes that are
            // actually the next frame's start.
            if self.buf.len() >= 4 + HEADER_LEN {
                let h = &self.buf[4..4 + HEADER_LEN];
                if h[0] != MAGIC || h[1] != VERSION || FrameType::from_u8(h[2]).is_none() {
                    self.drop_one();
                    continue;
                }
            }
            if self.buf.len() < 4 + total {
                return None; // incomplete; wait for more bytes
            }
            let h = &self.buf[4..4 + HEADER_LEN];
            let frame = Frame {
                kind: FrameType::from_u8(h[2]).unwrap(),
                flags: h[3],
                speaker_id: u64::from_le_bytes(h[4..12].try_into().unwrap()),
                seq: u32::from_le_bytes(h[12..16].try_into().unwrap()),
                timestamp_ms: u32::from_le_bytes(h[16..20].try_into().unwrap()),
                samples: u16::from_le_bytes(h[20..22].try_into().unwrap()),
                energy: h[22],
                payload: self.buf[4 + HEADER_LEN..4 + total].to_vec(),
            };
            self.buf.drain(..4 + total);
            return Some(frame);
        }
    }

    fn drop_one(&mut self) {
        self.buf.drain(..1);
        self.resyncs += 1;
    }
}

/// FNV-1a 64-bit — the speaker_id for Armada participants
/// (hash of the SFU identity string).
pub fn fnv1a_64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// RMS energy of a frame as a 0–255 bucket.
pub fn energy_bucket(pcm: &[i16]) -> u8 {
    if pcm.is_empty() {
        return 0;
    }
    let sum: f64 = pcm.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum / pcm.len() as f64).sqrt();
    ((rms / i16::MAX as f64) * 255.0).min(255.0) as u8
}

/// Peak absolute level of a frame, 0..1 (speaking detection).
pub fn frame_level(pcm: &[i16]) -> f32 {
    pcm.iter().map(|s| (*s as i32).unsigned_abs()).max().unwrap_or(0) as f32 / i16::MAX as f32
}

/// A bounded drop-oldest ring: the only queue shape this
/// protocol allows. Overflow drops the OLDEST item and counts
/// the drop — audio too late is worse than no audio.
pub struct DropRing<T> {
    q: VecDeque<T>,
    capacity: usize,
    pub dropped: u64,
}

impl<T> DropRing<T> {
    pub fn new(capacity: usize) -> Self {
        Self { q: VecDeque::new(), capacity: capacity.max(1), dropped: 0 }
    }

    pub fn push(&mut self, item: T) {
        if self.q.len() >= self.capacity {
            self.q.pop_front();
            self.dropped += 1;
        }
        self.q.push_back(item);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.q.pop_front()
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    pub fn clear(&mut self) {
        self.q.clear();
    }
}

/// One source's jitter buffer: frames arrive off the network
/// clock and leave on the mixer tick. Output starts once
/// JITTER_TARGET frames are buffered (60 ms of cushion).
/// A brief underrun (≤ UNDERRUN_RIDE ticks) is ridden
/// through with a decayed repeat of the last frame —
/// packet-loss concealment for a receive path that has
/// none (see UNDERRUN_RIDE) — instead of muting the source
/// until it re-prebuffers. Only a source dry longer than
/// the ride window re-enters prebuffering.
pub struct JitterBuffer {
    ring: DropRing<Vec<i16>>,
    started: bool,
    /// Last real frame popped (concealment source material).
    last: Option<Vec<i16>>,
    /// Concealment ticks left before giving up.
    ride_left: usize,
    /// Ticks this buffer has concealed (telemetry).
    pub concealed: u64,
    /// Ticks this buffer underran past the ride window and
    /// fell silent mid-flow (telemetry).
    pub underruns: u64,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            ring: DropRing::new(JITTER_MAX),
            started: false,
            last: None,
            ride_left: 0,
            concealed: 0,
            underruns: 0,
        }
    }

    pub fn push(&mut self, frame: Vec<i16>) {
        self.ring.push(frame);
        if self.ring.len() >= JITTER_TARGET {
            self.started = true;
        }
    }

    /// One frame for this tick, or None when this source
    /// contributes silence (prebuffering, or dry past the
    /// ride-through window).
    pub fn pop_for_tick(&mut self) -> Option<Vec<i16>> {
        if !self.started {
            return None;
        }
        match self.ring.pop() {
            Some(f) => {
                self.last = Some(f.clone());
                self.ride_left = UNDERRUN_RIDE;
                Some(f)
            }
            None => {
                if self.ride_left > 0 {
                    if let Some(last) = &self.last {
                        self.ride_left -= 1;
                        self.concealed += 1;
                        let ride_idx = (UNDERRUN_RIDE - self.ride_left) as i32;
                        let decay = CONCEAL_DECAY.powi(ride_idx);
                        return Some(
                            last.iter().map(|s| (*s as f32 * decay) as i16).collect(),
                        );
                    }
                }
                self.started = false;
                self.underruns += 1;
                None
            }
        }
    }

    pub fn buffered(&self) -> usize {
        self.ring.len()
    }

    pub fn dropped(&self) -> u64 {
        self.ring.dropped
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Which platform a mixer input belongs to. Mix-minus is
/// structural (spec §4.1): a direction's mix contains ONLY
/// the other platform's inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Discord,
    Armada,
}

/// The bridge mixer: owns both directions' inputs and the
/// 20 ms tick. On each tick, each direction sums its active
/// inputs (i32 accumulate) at UNITY — no gain staging, no
/// limiter — and the only level safety is the last-resort
/// hard clamp at full scale on the way out. (The 2026-11-22
/// per-direction master gains + tanh soft limiter were
/// removed 2026-11-23 on the owner's order: unity passthrough.)
/// A direction with no input producing audio this
/// tick yields None — the legs publish nothing rather than
/// streaming digital silence.
pub struct Mixer {
    inputs: HashMap<(Side, u64), JitterBuffer>,
    /// Speaking edges emitted by the last tick:
    /// (side, speaker_id, started).
    speaking_edges: Vec<(Side, u64, bool)>,
    active: HashMap<(Side, u64), bool>,
    pub dropped_frames: u64,
    /// Concealment frames emitted by underrun ride-through
    /// across all inputs (telemetry; see UNDERRUN_RIDE).
    pub concealed_frames: u64,
    /// Ticks an input stayed silent past the ride window
    /// mid-flow across all inputs (telemetry).
    pub underrun_ticks: u64,
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            speaking_edges: Vec::new(),
            active: HashMap::new(),
            dropped_frames: 0,
            concealed_frames: 0,
            underrun_ticks: 0,
        }
    }

    /// Feed one frame from one source.
    pub fn push(&mut self, side: Side, speaker_id: u64, frame: Vec<i16>) {
        let buf = self.inputs.entry((side, speaker_id)).or_default();
        let before = buf.dropped();
        buf.push(frame);
        self.dropped_frames += buf.dropped() - before;
    }

    /// Forget a source entirely (participant left).
    pub fn remove(&mut self, side: Side, speaker_id: u64) {
        self.inputs.remove(&(side, speaker_id));
        self.active.remove(&(side, speaker_id));
    }

    pub fn clear(&mut self) {
        self.inputs.clear();
        self.active.clear();
    }

    /// Advance one 20 ms tick. Returns
    /// `(to_discord, to_armada)` — each the mix of the OTHER
    /// side's inputs, or None when that direction had no
    /// audio this tick.
    pub fn tick(&mut self) -> (Option<Vec<i16>>, Option<Vec<i16>>) {
        let to_discord = self.mix_direction(Side::Armada);
        let to_armada = self.mix_direction(Side::Discord);
        (to_discord, to_armada)
    }

    /// Sum one direction: all inputs from `source_side` at
    /// unity, finished with the last-resort hard clamp at
    /// full scale (the pre-loudness-fix behavior).
    fn mix_direction(&mut self, source_side: Side) -> Option<Vec<i16>> {
        let mut acc: Option<[i32; FRAME_SAMPLES]> = None;
        let mut any_audio = false;
        let keys: Vec<(Side, u64)> =
            self.inputs.keys().filter(|(s, _)| *s == source_side).copied().collect();
        for key in keys {
            let mut frame = None;
            if let Some(b) = self.inputs.get_mut(&key) {
                let (c0, u0) = (b.concealed, b.underruns);
                frame = b.pop_for_tick();
                self.concealed_frames += b.concealed - c0;
                self.underrun_ticks += b.underruns - u0;
            }
            let speaking = frame
                .as_ref()
                .map(|f| frame_level(f) >= SPEAKING_LEVEL)
                .unwrap_or(false);
            let was = self.active.insert(key, speaking).unwrap_or(false);
            if speaking != was {
                self.speaking_edges.push((key.0, key.1, speaking));
            }
            if let Some(frame) = frame {
                any_audio = true;
                let acc = acc.get_or_insert([0i32; FRAME_SAMPLES]);
                for (i, s) in frame.iter().take(FRAME_SAMPLES).enumerate() {
                    acc[i] += *s as i32;
                }
            }
        }
        if !any_audio {
            return None;
        }
        let acc = acc.unwrap_or([0i32; FRAME_SAMPLES]);
        Some(acc.iter().map(|v| (*v).clamp(-32768, 32767) as i16).collect())
    }

    /// Speaking start/stop edges since the last call.
    pub fn take_speaking_edges(&mut self) -> Vec<(Side, u64, bool)> {
        std::mem::take(&mut self.speaking_edges)
    }

    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

/// Peak level a frame must clear to count as speaking
/// (mirrors the gate the reference bot's segmenter uses).
pub const SPEAKING_LEVEL: f32 = 0.02;

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_frame(amp: i16) -> Vec<i16> {
        (0..FRAME_SAMPLES).map(|i| ((i as f64 * 0.3).sin() * amp as f64) as i16).collect()
    }

    #[test]
    fn frame_roundtrip_through_decoder() {
        let pcm = sine_frame(10_000);
        let f = Frame::pcm(FrameType::PcmDown, 123_456_789, 7, 42_000, &pcm);
        let bytes = f.encode();
        assert_eq!(bytes.len(), 4 + HEADER_LEN + FRAME_BYTES);
        let mut dec = StreamDecoder::new();
        dec.feed(&bytes);
        let got = dec.next_frame().expect("one frame");
        assert_eq!(got, f);
        assert_eq!(got.pcm_samples(), pcm);
        assert!(dec.next_frame().is_none());
    }

    #[test]
    fn golden_wire_bytes() {
        // The Node implementation must produce byte-identical
        // frames; this vector is mirrored in
        // discord-leg/test/protocol.test.mjs.
        let f = Frame {
            kind: FrameType::Control,
            flags: 0,
            speaker_id: 0,
            seq: 0,
            timestamp_ms: 0,
            samples: 0,
            energy: 0,
            payload: b"{}".to_vec(),
        };
        let bytes = f.encode();
        let expected: Vec<u8> = vec![
            26, 0, 0, 0, // length = 24 header + 2 payload
            b'B', 1, 5, 0, // magic, version, type=Control, flags
            0, 0, 0, 0, 0, 0, 0, 0, // speaker_id
            0, 0, 0, 0, // seq
            0, 0, 0, 0, // timestamp_ms
            0, 0, // samples
            0, 0, // energy, reserved
            b'{', b'}',
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn decoder_resyncs_after_garbage() {
        let f = Frame::control("{\"cmd\":\"status\"}");
        let mut bytes = vec![0xFF, 0x00, 0x13];
        bytes.extend_from_slice(&f.encode());
        let mut dec = StreamDecoder::new();
        dec.feed(&bytes);
        let got = dec.next_frame().expect("frame after garbage");
        assert_eq!(got, f);
        assert!(dec.resyncs >= 1);
    }

    #[test]
    fn decoder_handles_split_feeds() {
        let f = Frame::pcm(FrameType::PcmUp, 99, 1, 20, &sine_frame(5000));
        let bytes = f.encode();
        let mut dec = StreamDecoder::new();
        dec.feed(&bytes[..7]);
        assert!(dec.next_frame().is_none());
        dec.feed(&bytes[7..]);
        assert_eq!(dec.next_frame(), Some(f));
    }

    #[test]
    fn ring_drops_oldest_and_counts() {
        let mut r = DropRing::new(3);
        for i in 0..5 {
            r.push(i);
        }
        assert_eq!(r.dropped, 2);
        assert_eq!(r.pop(), Some(2));
        assert_eq!(r.pop(), Some(3));
        assert_eq!(r.pop(), Some(4));
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn jitter_buffer_prebuffers_then_flows() {
        let mut j = JitterBuffer::new();
        let f = sine_frame(8000);
        assert!(j.pop_for_tick().is_none());
        j.push(f.clone());
        j.push(f.clone());
        assert!(j.pop_for_tick().is_none(), "still prebuffering at 2");
        j.push(f.clone());
        assert!(j.pop_for_tick().is_some(), "flows at target depth");
        // Drain fully: the buffer first rides through the
        // underrun with concealment (UNDERRUN_RIDE ticks),
        // THEN falls silent and re-prebuffers.
        let _ = j.pop_for_tick();
        let _ = j.pop_for_tick();
        for _ in 0..UNDERRUN_RIDE {
            assert!(j.pop_for_tick().is_some(), "ride-through conceals brief underruns");
        }
        assert!(j.pop_for_tick().is_none(), "silent once the ride window is spent");
        j.push(f.clone());
        assert!(j.pop_for_tick().is_none(), "re-prebuffers after running dry");
    }

    #[test]
    fn jitter_buffer_concealment_decays_and_real_frames_take_over() {
        let mut j = JitterBuffer::new();
        let f = vec![10_000i16; FRAME_SAMPLES];
        for _ in 0..JITTER_TARGET {
            j.push(f.clone());
        }
        for _ in 0..JITTER_TARGET {
            let _ = j.pop_for_tick();
        }
        // First dry tick: concealment = last frame at 0.7.
        let c1 = j.pop_for_tick().expect("first concealment tick");
        assert!(c1.iter().all(|s| *s == 7_000), "concealment decays: {c1:?}…");
        // Second dry tick: 0.49.
        let c2 = j.pop_for_tick().expect("second concealment tick");
        assert!(c2.iter().all(|s| *s == 4_900));
        assert_eq!(j.concealed, 2);
        // A real frame arriving mid-ride is played as itself
        // on the next tick, not papered over.
        j.push(f.clone());
        let r = j.pop_for_tick().expect("real frame after concealment");
        assert!(r.iter().all(|s| *s == 10_000));
        assert_eq!(j.concealed, 2, "no concealment counted for real frames");
    }

    #[test]
    fn mixer_no_silence_gap_when_one_frame_goes_missing() {
        // REGRESSION (2026-11-23, "tiny bit robotic
        // occasionally"): the Discord receive path delivers
        // a timeline compressed by every lost packet (no
        // concealment upstream). As built, one missing 20 ms
        // frame drained the jitter buffer and the source
        // went silent for the underrun tick PLUS the 3-frame
        // re-prebuffer — a ≥60 ms hole per lost packet,
        // measured at 26 holes/1000 ms per 3 min of speech
        // at 1% loss (tests/robotic_harness.rs). With
        // ride-through, the mixed output must have NO silent
        // tick while the source is mid-spurt.
        let mut m = Mixer::new();
        let f = vec![8_000i16; FRAME_SAMPLES];
        // Prebuffer + start flowing (ring now holds 2).
        for _ in 0..JITTER_TARGET {
            m.push(Side::Discord, 7, f.clone());
        }
        let (_, ta) = m.tick();
        assert!(ta.is_some(), "flowing");
        // Steady state, one frame in per tick, except ticks
        // 2, 5, 8 whose frames were "lost in the network".
        // Each loss permanently drains the cushion by one
        // (the upstream timeline compresses — nothing ever
        // re-fills it), so the third loss empties the ring
        // mid-spurt: as built, that tick and the following
        // re-prebuffer ticks were silent.
        for tick in 0..10 {
            if tick % 3 != 2 {
                m.push(Side::Discord, 7, f.clone());
            }
            let (_, ta) = m.tick();
            assert!(ta.is_some(), "tick {tick}: mid-spurt output must not gap");
        }
        assert_eq!(m.concealed_frames, 1, "exactly the drained tick was concealed");
        assert_eq!(m.underrun_ticks, 0, "no underrun got past the ride window");
    }

    #[test]
    fn mixer_sums_clamps_and_separates_directions() {
        let mut m = Mixer::new();
        let loud = vec![30_000i16; FRAME_SAMPLES];
        // Two Discord speakers, loud enough to clip when summed.
        for _ in 0..JITTER_TARGET {
            m.push(Side::Discord, 1, loud.clone());
            m.push(Side::Discord, 2, loud.clone());
            m.push(Side::Armada, 9, vec![1000i16; FRAME_SAMPLES]);
        }
        let (to_discord, to_armada) = m.tick();
        // Discord hears ONLY the Armada input (mix-minus),
        // passed through at unity.
        let td = to_discord.expect("armada audio flows to discord");
        assert!(td.iter().all(|s| *s == 1000));
        // Armada hears the Discord sum: 30000 + 30000 = 60000,
        // held by the last-resort hard clamp at full scale —
        // the ONLY level stage left in the mixer (the master
        // gains and tanh limiter were removed 2026-11-23).
        let ta = to_armada.expect("discord audio flows to armada");
        assert!(ta.iter().all(|s| *s == i16::MAX));
    }

    #[test]
    fn mixer_hard_clamp_bounds_both_signs() {
        let mut m = Mixer::new();
        for _ in 0..JITTER_TARGET {
            m.push(Side::Discord, 1, vec![-30_000i16; FRAME_SAMPLES]);
            m.push(Side::Discord, 2, vec![-30_000i16; FRAME_SAMPLES]);
        }
        let (_, ta) = m.tick();
        assert!(ta.unwrap().iter().all(|s| *s == i16::MIN));
    }

    #[test]
    fn mixer_is_unity_passthrough() {
        // No gain staging anywhere: a single input at any
        // level below full scale passes bit-exact, both
        // directions, both signs.
        for level in [1i16, 1000, 10_000, 30_000, -1, -10_000, -30_000, i16::MAX, i16::MIN] {
            let mut m = Mixer::new();
            for _ in 0..JITTER_TARGET {
                m.push(Side::Armada, 9, vec![level; FRAME_SAMPLES]);
                m.push(Side::Discord, 1, vec![level; FRAME_SAMPLES]);
            }
            let (td, ta) = m.tick();
            assert!(td.unwrap().iter().all(|s| *s == level), "to_discord at {level}");
            assert!(ta.unwrap().iter().all(|s| *s == level), "to_armada at {level}");
        }
    }

    #[test]
    fn mixer_silence_when_no_input() {
        let mut m = Mixer::new();
        let (a, b) = m.tick();
        assert!(a.is_none() && b.is_none());
        m.push(Side::Discord, 1, vec![0i16; FRAME_SAMPLES]);
        let (a, b) = m.tick();
        assert!(a.is_none(), "nothing for discord from its own side");
        assert!(b.is_none(), "one frame is still prebuffering");
    }

    #[test]
    fn fnv1a_matches_reference_values() {
        // FNV-1a 64 offset basis for the empty string.
        assert_eq!(fnv1a_64(""), 0xcbf2_9ce4_8422_2325);
        // Known vector: fnv1a_64("a") = 0xaf63dc4c8601ec8c.
        assert_eq!(fnv1a_64("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn energy_bucket_scales() {
        assert_eq!(energy_bucket(&[]), 0);
        assert_eq!(energy_bucket(&vec![0i16; FRAME_SAMPLES]), 0);
        assert_eq!(energy_bucket(&vec![i16::MAX; FRAME_SAMPLES]), 255);
    }
}

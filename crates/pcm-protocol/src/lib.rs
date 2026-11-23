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
/// JITTER_TARGET frames are buffered (60 ms of cushion); a
/// source that underruns contributes silence for that tick
/// and re-enters prebuffering only if it runs fully dry —
/// small gaps are ridden out, not re-cushioned.
pub struct JitterBuffer {
    ring: DropRing<Vec<i16>>,
    started: bool,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self { ring: DropRing::new(JITTER_MAX), started: false }
    }

    pub fn push(&mut self, frame: Vec<i16>) {
        self.ring.push(frame);
        if self.ring.len() >= JITTER_TARGET {
            self.started = true;
        }
    }

    /// One frame for this tick, or None when this source
    /// contributes silence (prebuffering or underrun).
    pub fn pop_for_tick(&mut self) -> Option<Vec<i16>> {
        if !self.started {
            return None;
        }
        match self.ring.pop() {
            Some(f) => Some(f),
            None => {
                self.started = false;
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
/// inputs (i32 accumulate, optional per-input gain), applies
/// the direction's master gain, and finishes through the
/// soft limiter ([`soft_limit`]) — the final level stage.
/// A direction with no input producing audio this
/// tick yields None — the legs publish nothing rather than
/// streaming digital silence.
pub struct Mixer {
    inputs: HashMap<(Side, u64), JitterBuffer>,
    gains: HashMap<(Side, u64), f32>,
    /// Per-direction master gains (linear), named by where
    /// the audio is HEADED. Default 1.0 (unity); the Armada
    /// leg sets them from BRIDGE_GAIN_TO_*_DB at startup
    /// (the bridge ran unity end to end,
    /// so overlapping near-full-scale sources flat-topped).
    gain_to_discord: f32,
    gain_to_armada: f32,
    /// Speaking edges emitted by the last tick:
    /// (side, speaker_id, started).
    speaking_edges: Vec<(Side, u64, bool)>,
    active: HashMap<(Side, u64), bool>,
    pub dropped_frames: u64,
}

/// The soft limiter's ceiling: ≈ -1 dBFS (10^(-1/20)).
/// No mixed sample ever exceeds it, however hot the sum.
pub const LIMIT_CEILING: f32 = 0.891_250_9;

/// Where the limiter stops being perfectly linear:
/// -6 dBFS. Below the knee a sample passes untouched.
pub const LIMIT_KNEE: f32 = 0.5;

/// Soft-knee saturation (tanh), the mixer's final stage.
/// Linear below [`LIMIT_KNEE`]; above it, a tanh segment
/// with matching value and slope at the knee, asymptotic
/// to ±[`LIMIT_CEILING`] — overlaps compress gracefully
/// instead of flat-topping at i16::MAX the way the old
/// hard clamp did. Input/output are normalized (1.0 =
/// full scale); the input may exceed 1.0 (a hot sum).
pub fn soft_limit(s: f32) -> f32 {
    let a = s.abs();
    if a <= LIMIT_KNEE {
        return s;
    }
    let y = LIMIT_KNEE
        + (LIMIT_CEILING - LIMIT_KNEE) * ((a - LIMIT_KNEE) / (LIMIT_CEILING - LIMIT_KNEE)).tanh();
    y.copysign(s)
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            gains: HashMap::new(),
            gain_to_discord: 1.0,
            gain_to_armada: 1.0,
            speaking_edges: Vec::new(),
            active: HashMap::new(),
            dropped_frames: 0,
        }
    }

    /// Per-input gain trim (config; default 1.0).
    pub fn set_gain(&mut self, side: Side, speaker_id: u64, gain: f32) {
        self.gains.insert((side, speaker_id), gain);
    }

    /// Per-direction master gain (config; default 1.0),
    /// named by the direction the audio is headed TO. Applied
    /// to the summed frame, before the soft limiter.
    pub fn set_master_gain(&mut self, destination: Side, gain: f32) {
        match destination {
            Side::Discord => self.gain_to_discord = gain,
            Side::Armada => self.gain_to_armada = gain,
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
        let to_discord = self.mix_direction(Side::Armada, self.gain_to_discord);
        let to_armada = self.mix_direction(Side::Discord, self.gain_to_armada);
        (to_discord, to_armada)
    }

    /// Sum one direction: all inputs from `source_side`,
    /// finished with the direction's master gain and the
    /// soft limiter (the final level stage).
    fn mix_direction(&mut self, source_side: Side, master_gain: f32) -> Option<Vec<i16>> {
        let mut acc: Option<[i32; FRAME_SAMPLES]> = None;
        let mut any_audio = false;
        let keys: Vec<(Side, u64)> =
            self.inputs.keys().filter(|(s, _)| *s == source_side).copied().collect();
        for key in keys {
            let frame = self.inputs.get_mut(&key).and_then(|b| b.pop_for_tick());
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
                let gain = self.gains.get(&key).copied().unwrap_or(1.0);
                let acc = acc.get_or_insert([0i32; FRAME_SAMPLES]);
                for (i, s) in frame.iter().take(FRAME_SAMPLES).enumerate() {
                    acc[i] += (*s as f32 * gain) as i32;
                }
            }
        }
        if !any_audio {
            return None;
        }
        let acc = acc.unwrap_or([0i32; FRAME_SAMPLES]);
        Some(
            acc.iter()
                .map(|v| (soft_limit((*v as f32 / 32768.0) * master_gain) * 32768.0) as i16)
                .collect(),
        )
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
        // Drain fully: next tick underruns and re-prebuffers.
        let _ = j.pop_for_tick();
        let _ = j.pop_for_tick();
        assert!(j.pop_for_tick().is_none());
        j.push(f.clone());
        assert!(j.pop_for_tick().is_none(), "re-prebuffers after running dry");
    }

    #[test]
    fn mixer_sums_limits_and_separates_directions() {
        let mut m = Mixer::new();
        let loud = vec![30_000i16; FRAME_SAMPLES];
        // Two Discord speakers, loud enough to clip when summed.
        for _ in 0..JITTER_TARGET {
            m.push(Side::Discord, 1, loud.clone());
            m.push(Side::Discord, 2, loud.clone());
            m.push(Side::Armada, 9, vec![1000i16; FRAME_SAMPLES]);
        }
        let (to_discord, to_armada) = m.tick();
        // Discord hears ONLY the Armada input (mix-minus), and
        // a small signal passes the limiter untouched (it is
        // below the knee): direction separation AND the old
        // unity path both preserved.
        let td = to_discord.expect("armada audio flows to discord");
        assert!(td.iter().all(|s| *s == 1000));
        // Armada hears the Discord sum — but through the soft
        // limiter now: it must NOT pin at i16::MAX (the old
        // hard clamp flat-topped here) and must respect the
        // -1 dBFS ceiling while still landing near it (two
        // near-full-scale sources sum well past the knee).
        let ta = to_armada.expect("discord audio flows to armada");
        let ceiling = (LIMIT_CEILING * 32768.0) as i16;
        assert!(ta.iter().all(|s| *s <= ceiling && *s != i16::MAX));
        assert!(ta.iter().all(|s| *s > 28_000), "hot sum should ride near the ceiling: {}", ta[0]);
    }

    #[test]
    fn soft_limiter_ceiling_and_knee() {
        // Below the knee: perfectly linear, both signs.
        assert_eq!(soft_limit(0.25), 0.25);
        assert_eq!(soft_limit(-0.5), -0.5);
        assert_eq!(soft_limit(0.0), 0.0);
        // At full scale and far beyond: never over the
        // ceiling, approaching it as the sum grows,
        // symmetric, never at 1.0.
        assert!(soft_limit(1.0) > 0.80 && soft_limit(1.0) < LIMIT_CEILING);
        for s in [2.0f32, 10.0, 100.0] {
            let y = soft_limit(s);
            assert!(y <= LIMIT_CEILING && y > 0.88, "s={s} y={y}");
            assert_eq!(soft_limit(-s), -y);
        }
    }

    #[test]
    fn mixer_master_gain_scales_a_single_input() {
        // -6 dB ≈ 0.5012 linear (the BRIDGE_GAIN_TO_DISCORD_DB
        // default, applied by the armada leg at startup).
        let mut m = Mixer::new();
        m.set_master_gain(Side::Discord, 0.501_187_2);
        m.set_master_gain(Side::Armada, 0.707_945_8); // -3 dB
        for _ in 0..JITTER_TARGET {
            m.push(Side::Armada, 9, vec![10_000i16; FRAME_SAMPLES]);
            m.push(Side::Discord, 1, vec![10_000i16; FRAME_SAMPLES]);
        }
        let (to_discord, to_armada) = m.tick();
        // 10000 * 0.5012 ≈ 5012; the product stays below the
        // limiter knee, so it passes at exactly the gain.
        let td = to_discord.unwrap();
        assert!(td.iter().all(|s| (5012 - *s as i32).abs() <= 2), "to_discord {}", td[0]);
        // The other direction carries its OWN gain (-3 dB):
        // gains are per-direction, not global.
        let ta = to_armada.unwrap();
        assert!(ta.iter().all(|s| (7079 - *s as i32).abs() <= 2), "to_armada {}", ta[0]);
        // Default (unset) master gain is unity: a moderate
        // single input passes unchanged.
        let mut m2 = Mixer::new();
        for _ in 0..JITTER_TARGET {
            m2.push(Side::Armada, 9, vec![10_000i16; FRAME_SAMPLES]);
        }
        let (td2, _) = m2.tick();
        assert!(td2.unwrap().iter().all(|s| *s == 10_000));
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

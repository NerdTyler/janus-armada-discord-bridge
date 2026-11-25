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

use std::collections::{HashMap, HashSet, VecDeque};

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

/// RMS level of a frame, 0..1 (ducking speech detection —
/// RMS tracks perceived loudness better than peak, which a
/// single click or pop can trip).
pub fn frame_rms(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    ((sum / pcm.len() as f64).sqrt() / i16::MAX as f64) as f32
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

/// Ducking configuration (the owner's order, 2026-11-24):
/// sources marked as MUSIC get their gain ridden by an
/// envelope that pulls them down while any HUMAN source on
/// the same side is speaking, so people always sit on top
/// of the music in the mix. Human sources are NEVER
/// envelope-reduced — the only gain a human source can get
/// is its manual per-stream gain (see [`Mixer::set_manual_gain`]),
/// which is a deliberate, chat-commanded setting, not
/// something the envelope does to them.
#[derive(Clone, Debug)]
pub struct DuckConfig {
    /// Master switch (JANUS_DUCK=off in the daemon).
    pub enabled: bool,
    /// Gain music sources are ducked TO (0.30 = 30%).
    pub gain: f32,
    /// Ramp down to `gain` over this many ms.
    pub attack_ms: u32,
    /// Stay ducked this many ms after the last speech-active
    /// human frame before releasing.
    pub hold_ms: u32,
    /// Ramp back to unity over this many ms.
    pub release_ms: u32,
    /// A human frame at or above this RMS (0..1) counts as
    /// speech and (re)engages the duck.
    pub rms_on: f32,
    /// Hysteresis: speech is considered over only once
    /// every human frame falls to this RMS or below.
    /// Between the two thresholds the previous state holds,
    /// so a voice hovering at the edge cannot flap the duck.
    pub rms_off: f32,
    /// Sources that start out marked as music (the daemon
    /// seeds stable-id sources here — Discord user ids —
    /// and marks Armada sources at runtime once presence
    /// resolves their identity to a music npub, because
    /// Armada SFU identities are random per join).
    pub music_sources: HashSet<(Side, u64)>,
}

impl Default for DuckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gain: 0.30,
            attack_ms: 100,
            hold_ms: 1500,
            release_ms: 1000,
            rms_on: 0.02,
            rms_off: 0.01,
            music_sources: HashSet::new(),
        }
    }
}

/// One side's duck envelope state.
#[derive(Clone, Copy)]
struct DuckState {
    /// Current envelope gain for the side's music sources.
    gain: f32,
    /// Hysteresis latch: a human on this side is speaking.
    speaking: bool,
    /// Ticks of hold left after speech stops.
    hold_left: u32,
}

impl Default for DuckState {
    fn default() -> Self {
        Self { gain: 1.0, speaking: false, hold_left: 0 }
    }
}

/// Step `cur` toward `target` by at most `step`.
fn move_toward(cur: f32, target: f32, step: f32) -> f32 {
    if cur < target {
        (cur + step).min(target)
    } else {
        (cur - step).max(target)
    }
}

/// The bridge mixer: owns both directions' inputs and the
/// 20 ms tick. On each tick, each direction sums its active
/// inputs (i32 accumulate) and the only level safety is the
/// last-resort hard clamp at full scale on the way out.
/// Sources sum at UNITY unless a gain says otherwise; the
/// gains that exist are exactly two (both the owner's orders,
/// 2026-11-24 — the 2026-11-22 master gains + tanh limiter
/// stayed removed):
///   - a per-source MANUAL gain (0–150%, default 100%),
///     set from Armada chat by the owner alone (`!vol …`), and
///   - the DUCK envelope, which rides MUSIC sources only,
///     down under same-side human speech.
/// Effective gain = manual × envelope (music) or manual
/// alone (humans). With no manual gains set and no music
/// sources marked, the mixer is the same bit-exact unity
/// passthrough it was before.
/// A direction with no input producing audio this
/// tick yields None — the legs publish nothing rather than
/// streaming digital silence.
pub struct Mixer {
    inputs: HashMap<(Side, u64), JitterBuffer>,
    /// Speaking edges emitted by the last tick:
    /// (side, speaker_id, started).
    speaking_edges: Vec<(Side, u64, bool)>,
    active: HashMap<(Side, u64), bool>,
    duck_cfg: DuckConfig,
    /// Per-side duck envelope state.
    duck: HashMap<Side, DuckState>,
    /// Sources currently marked as music (duckable).
    music: HashSet<(Side, u64)>,
    /// Per-source manual gains (absent = 1.0). Runtime
    /// state only: cleared with the session, never stored.
    manual: HashMap<(Side, u64), f32>,
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
        Self::with_duck_config(DuckConfig::default())
    }

    pub fn with_duck_config(duck_cfg: DuckConfig) -> Self {
        let music = duck_cfg.music_sources.clone();
        Self {
            inputs: HashMap::new(),
            speaking_edges: Vec::new(),
            active: HashMap::new(),
            duck_cfg,
            duck: HashMap::new(),
            music,
            manual: HashMap::new(),
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

    /// Forget a source entirely (participant left) — its
    /// manual gain and music mark go with it.
    pub fn remove(&mut self, side: Side, speaker_id: u64) {
        self.inputs.remove(&(side, speaker_id));
        self.active.remove(&(side, speaker_id));
        self.manual.remove(&(side, speaker_id));
        self.music.remove(&(side, speaker_id));
    }

    pub fn clear(&mut self) {
        self.inputs.clear();
        self.active.clear();
        self.manual.clear();
        self.music = self.duck_cfg.music_sources.clone();
        self.duck.clear();
    }

    // -- gains ------------------------------------------------

    /// Mark (or unmark) a source as music: music sources
    /// are ridden by the duck envelope; every other source
    /// is human and never is.
    pub fn set_music_source(&mut self, side: Side, speaker_id: u64, music: bool) {
        if music {
            self.music.insert((side, speaker_id));
        } else {
            self.music.remove(&(side, speaker_id));
        }
    }

    pub fn is_music_source(&self, side: Side, speaker_id: u64) -> bool {
        self.music.contains(&(side, speaker_id))
    }

    /// A source's manual gain (1.0 when never set).
    pub fn manual_gain(&self, side: Side, speaker_id: u64) -> f32 {
        self.manual.get(&(side, speaker_id)).copied().unwrap_or(1.0)
    }

    /// Set a source's manual gain, clamped to 0.0–1.5
    /// (silence through a 50% boost). Applies from the next
    /// tick; composes multiplicatively with the duck
    /// envelope for music sources.
    pub fn set_manual_gain(&mut self, side: Side, speaker_id: u64, gain: f32) {
        let gain = gain.clamp(0.0, 1.5);
        if (gain - 1.0).abs() < f32::EPSILON {
            self.manual.remove(&(side, speaker_id));
        } else {
            self.manual.insert((side, speaker_id), gain);
        }
    }

    /// One source's manual gain back to 1.0.
    pub fn reset_manual_gain(&mut self, side: Side, speaker_id: u64) {
        self.manual.remove(&(side, speaker_id));
    }

    /// Every source's manual gain back to 1.0.
    pub fn reset_all_manual_gains(&mut self) {
        self.manual.clear();
    }

    /// The live input sources, sorted for stable display.
    pub fn sources(&self) -> Vec<(Side, u64)> {
        let mut out: Vec<(Side, u64)> = self.inputs.keys().copied().collect();
        out.sort_by_key(|(side, id)| (matches!(side, Side::Armada) as u8, *id));
        out
    }

    /// The current duck envelope gain for a side's music
    /// sources (1.0 when idle or ducking is disabled).
    /// Telemetry/tests.
    pub fn duck_gain(&self, side: Side) -> f32 {
        if !self.duck_cfg.enabled {
            return 1.0;
        }
        self.duck.get(&side).map(|s| s.gain).unwrap_or(1.0)
    }

    /// Advance one side's duck envelope by one tick, based
    /// on THIS tick's popped frames, and return the gain to
    /// apply to the side's music sources in this tick's mix.
    /// Speech state is a hysteresis latch over the side's
    /// HUMAN sources: any human frame at/above `rms_on`
    /// engages (and refreshes the hold); speech reads as
    /// over only when every human frame is at/below
    /// `rms_off`. Music sources never trigger their own
    /// duck, and a side with no music sources still tracks
    /// state harmlessly (the gain is simply never used).
    fn step_duck(&mut self, side: Side, frames: &[((Side, u64), Option<Vec<i16>>)]) -> f32 {
        let cfg = &self.duck_cfg;
        if !cfg.enabled {
            return 1.0;
        }
        let mut any_on = false;
        let mut all_off = true;
        for (key, frame) in frames {
            if self.music.contains(key) {
                continue;
            }
            let rms = frame.as_ref().map(|f| frame_rms(f)).unwrap_or(0.0);
            if rms >= cfg.rms_on {
                any_on = true;
            }
            if rms > cfg.rms_off {
                all_off = false;
            }
        }
        let st = self.duck.entry(side).or_default();
        if any_on {
            st.speaking = true;
        } else if all_off {
            st.speaking = false;
        }
        let attack_step = (1.0 - cfg.gain) / (cfg.attack_ms / 20).max(1) as f32;
        let release_step = (1.0 - cfg.gain) / (cfg.release_ms / 20).max(1) as f32;
        if st.speaking {
            st.hold_left = cfg.hold_ms / 20;
            st.gain = move_toward(st.gain, cfg.gain, attack_step);
        } else if st.hold_left > 0 {
            st.hold_left -= 1;
        } else {
            st.gain = move_toward(st.gain, 1.0, release_step);
        }
        st.gain
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

    /// Sum one direction: all inputs from `source_side`,
    /// each at its effective gain (manual × duck envelope
    /// for music sources, manual alone for humans — sources
    /// with no gains set pass at unity, bit-exact), finished
    /// with the last-resort hard clamp at full scale.
    /// Frames are popped BEFORE the duck steps so the
    /// envelope decision for a tick is made on that tick's
    /// human frames, then applied to that tick's music.
    fn mix_direction(&mut self, source_side: Side) -> Option<Vec<i16>> {
        let keys: Vec<(Side, u64)> =
            self.inputs.keys().filter(|(s, _)| *s == source_side).copied().collect();
        let mut frames: Vec<((Side, u64), Option<Vec<i16>>)> = Vec::with_capacity(keys.len());
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
            frames.push((key, frame));
        }
        let duck_gain = self.step_duck(source_side, &frames);
        let mut acc: Option<[i32; FRAME_SAMPLES]> = None;
        let mut any_audio = false;
        for (key, frame) in &frames {
            let Some(frame) = frame else { continue };
            any_audio = true;
            let manual = self.manual.get(key).copied().unwrap_or(1.0);
            let eff = if self.music.contains(key) { manual * duck_gain } else { manual };
            let acc = acc.get_or_insert([0i32; FRAME_SAMPLES]);
            if eff == 1.0 {
                for (i, s) in frame.iter().take(FRAME_SAMPLES).enumerate() {
                    acc[i] += *s as i32;
                }
            } else {
                for (i, s) in frame.iter().take(FRAME_SAMPLES).enumerate() {
                    acc[i] += (*s as f32 * eff).round() as i32;
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

    // -- ducking + manual gains (2026-11-24) ------------------

    const MUSICBOT: (Side, u64) = (Side::Armada, 0x1CE5);
    const VOICEBOT: (Side, u64) = (Side::Armada, 0xC4415);
    const HUMAN: (Side, u64) = (Side::Armada, 0x44);

    fn duck_mixer() -> Mixer {
        let mut cfg = DuckConfig::default();
        cfg.music_sources.insert(MUSICBOT);
        Mixer::with_duck_config(cfg)
    }

    /// Prefill one source's jitter buffer so it flows.
    fn prime(m: &mut Mixer, src: (Side, u64), amp: i16) {
        for _ in 0..JITTER_TARGET {
            m.push(src.0, src.1, vec![amp; FRAME_SAMPLES]);
        }
    }

    /// One tick with one fresh constant frame per listed
    /// source (steady-state flow).
    fn feed_tick(m: &mut Mixer, feeds: &[((Side, u64), i16)]) -> (Option<Vec<i16>>, Option<Vec<i16>>) {
        for (src, amp) in feeds {
            m.push(src.0, src.1, vec![*amp; FRAME_SAMPLES]);
        }
        m.tick()
    }

    #[test]
    fn duck_attack_reaches_target_in_about_100ms() {
        let mut m = duck_mixer();
        prime(&mut m, MUSICBOT, 10_000);
        prime(&mut m, HUMAN, 0);
        // Nobody speaking: no duck.
        for _ in 0..3 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 0)]);
        }
        assert_eq!(m.duck_gain(Side::Armada), 1.0);
        // Speech starts. The jitter buffer's steady-state
        // depth (JITTER_TARGET) means a content switch pops
        // 3 ticks later — the envelope itself then attacks
        // over 100 ms = 5 ticks of 0.14.
        for _ in 0..3 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
            assert_eq!(m.duck_gain(Side::Armada), 1.0, "jitter lag: old frames still popping");
        }
        feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
        let g1 = m.duck_gain(Side::Armada);
        assert!((g1 - 0.86).abs() < 0.001, "first attack tick: {g1}");
        for _ in 0..4 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
        }
        let g5 = m.duck_gain(Side::Armada);
        assert!((g5 - 0.30).abs() < 0.001, "after 5 attack ticks (100 ms): {g5}");
        // Steady state while speech continues.
        for _ in 0..10 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
        }
        assert!((m.duck_gain(Side::Armada) - 0.30).abs() < 0.001);
    }

    #[test]
    fn duck_holds_then_releases_on_schedule() {
        let mut m = duck_mixer();
        prime(&mut m, MUSICBOT, 10_000);
        prime(&mut m, HUMAN, 10_000);
        for _ in 0..10 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
        }
        assert!((m.duck_gain(Side::Armada) - 0.30).abs() < 0.001);
        // Speech stops (silent frames): the duck must HOLD
        // at target for the full 1500 ms hold (75 ticks of
        // popped silence, plus the 3-tick jitter lag while
        // the last loud frames drain)…
        for _ in 0..78 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 0)]);
        }
        assert!(
            (m.duck_gain(Side::Armada) - 0.30).abs() < 0.001,
            "still ducked at the end of the hold: {}",
            m.duck_gain(Side::Armada)
        );
        // …then release over 1000 ms (50 ticks): halfway
        // (25 ticks) ≈ 0.65, fully back at 50.
        for _ in 0..25 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 0)]);
        }
        let mid = m.duck_gain(Side::Armada);
        assert!((mid - 0.65).abs() < 0.03, "mid-release: {mid}");
        for _ in 0..25 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 0)]);
        }
        let done = m.duck_gain(Side::Armada);
        assert!((done - 1.0).abs() < 0.001, "fully released after 1000 ms: {done}");
    }

    #[test]
    fn duck_hysteresis_band_neither_engages_nor_releases() {
        // RMS of a constant 500-amp frame ≈ 0.0153: between
        // rms_off (0.01) and rms_on (0.02).
        let mut m = duck_mixer();
        prime(&mut m, MUSICBOT, 10_000);
        prime(&mut m, HUMAN, 500);
        // From idle, band-level noise never engages the duck.
        for _ in 0..20 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 500)]);
        }
        assert_eq!(m.duck_gain(Side::Armada), 1.0);
        // Real speech engages it…
        for _ in 0..10 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
        }
        assert!((m.duck_gain(Side::Armada) - 0.30).abs() < 0.001);
        // …and falling back INTO the band (not below it)
        // holds the speech latch: no hold countdown, no
        // release, for far longer than the hold window.
        for _ in 0..100 {
            feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 500)]);
        }
        assert!((m.duck_gain(Side::Armada) - 0.30).abs() < 0.001);
    }

    #[test]
    fn music_alone_never_ducks_itself() {
        let mut m = duck_mixer();
        prime(&mut m, MUSICBOT, 10_000);
        for _ in 0..30 {
            let (td, _) = feed_tick(&mut m, &[(MUSICBOT, 10_000)]);
            assert!(td.unwrap().iter().all(|s| *s == 10_000), "music passes bit-exact with no humans");
        }
        assert_eq!(m.duck_gain(Side::Armada), 1.0);
    }

    #[test]
    fn non_music_source_is_never_envelope_reduced() {
        // VOICEBOT is not on the music list. His voice must
        // pass at full level even while he himself is the
        // speech that ducks the music beside him — the mix
        // settles at VoiceBot 10000 + MusicBot 10000×0.30 = 13000,
        // never a point less for VoiceBot.
        let mut m = duck_mixer();
        prime(&mut m, MUSICBOT, 10_000);
        prime(&mut m, VOICEBOT, 10_000);
        for tick in 0..20 {
            let (td, _) = feed_tick(&mut m, &[(MUSICBOT, 10_000), (VOICEBOT, 10_000)]);
            let td = td.unwrap();
            if tick >= 6 {
                assert!(
                    td.iter().all(|s| *s == 13_000),
                    "tick {tick}: human full + music ducked: {:?}…",
                    &td[..4]
                );
            }
        }
        // And alone, with the duck fully engaged by his own
        // earlier speech, a non-music source is bit-exact.
        let mut m = duck_mixer();
        prime(&mut m, VOICEBOT, 10_000);
        for _ in 0..20 {
            let (td, _) = feed_tick(&mut m, &[(VOICEBOT, 10_000)]);
            assert!(td.unwrap().iter().all(|s| *s == 10_000));
        }
    }

    #[test]
    fn duck_kill_switch_disables_the_envelope() {
        let mut cfg = DuckConfig { enabled: false, ..DuckConfig::default() };
        cfg.music_sources.insert(MUSICBOT);
        let mut m = Mixer::with_duck_config(cfg);
        prime(&mut m, MUSICBOT, 10_000);
        prime(&mut m, HUMAN, 10_000);
        for _ in 0..20 {
            let (td, _) = feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
            assert!(td.unwrap().iter().all(|s| *s == 20_000), "no ducking while disabled");
        }
        assert_eq!(m.duck_gain(Side::Armada), 1.0);
    }

    #[test]
    fn manual_gain_applies_boosts_and_clamps() {
        let mut m = Mixer::new();
        prime(&mut m, (Side::Discord, 1), 10_000);
        m.set_manual_gain(Side::Discord, 1, 0.5);
        let (_, ta) = feed_tick(&mut m, &[((Side::Discord, 1), 10_000)]);
        assert!(ta.unwrap().iter().all(|s| *s == 5_000));
        m.set_manual_gain(Side::Discord, 1, 1.5);
        let (_, ta) = feed_tick(&mut m, &[((Side::Discord, 1), 10_000)]);
        assert!(ta.unwrap().iter().all(|s| *s == 15_000));
        // Out-of-range requests clamp to the 0–150% window.
        m.set_manual_gain(Side::Discord, 1, 2.0);
        assert_eq!(m.manual_gain(Side::Discord, 1), 1.5);
        m.set_manual_gain(Side::Discord, 1, -1.0);
        assert_eq!(m.manual_gain(Side::Discord, 1), 0.0);
        let (_, ta) = feed_tick(&mut m, &[((Side::Discord, 1), 10_000)]);
        assert!(ta.unwrap().iter().all(|s| *s == 0));
    }

    #[test]
    fn manual_gain_resets() {
        let mut m = Mixer::new();
        prime(&mut m, (Side::Discord, 1), 10_000);
        prime(&mut m, (Side::Discord, 2), 10_000);
        m.set_manual_gain(Side::Discord, 1, 0.5);
        m.set_manual_gain(Side::Discord, 2, 0.25);
        m.reset_manual_gain(Side::Discord, 1);
        assert_eq!(m.manual_gain(Side::Discord, 1), 1.0);
        assert_eq!(m.manual_gain(Side::Discord, 2), 0.25);
        let (_, ta) = feed_tick(&mut m, &[((Side::Discord, 1), 10_000), ((Side::Discord, 2), 10_000)]);
        assert!(ta.unwrap().iter().all(|s| *s == 12_500), "10000 + 2500 after one reset");
        m.reset_all_manual_gains();
        assert_eq!(m.manual_gain(Side::Discord, 2), 1.0);
        let (_, ta) = feed_tick(&mut m, &[((Side::Discord, 1), 10_000), ((Side::Discord, 2), 10_000)]);
        assert!(ta.unwrap().iter().all(|s| *s == 20_000));
    }

    #[test]
    fn manual_and_duck_compose_multiplicatively() {
        // Music at manual 50% under an engaged duck (30%)
        // contributes 15%: 10000 × 0.5 × 0.30 = 1500, on
        // top of the untouched human 10000 → 11500.
        let mut m = duck_mixer();
        m.set_manual_gain(MUSICBOT.0, MUSICBOT.1, 0.5);
        prime(&mut m, MUSICBOT, 10_000);
        prime(&mut m, HUMAN, 10_000);
        for tick in 0..20 {
            let (td, _) = feed_tick(&mut m, &[(MUSICBOT, 10_000), (HUMAN, 10_000)]);
            if tick >= 6 {
                assert!(td.unwrap().iter().all(|s| *s == 11_500), "tick {tick}");
            }
        }
    }
}

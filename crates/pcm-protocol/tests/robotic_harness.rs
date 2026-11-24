//! Diagnosis harness (2026-11-23, round 2): intermittent
//! robotic blips, Discord -> Armada direction.
//!
//! Models the as-built receive chain end to end at 1 ms
//! resolution:
//!   Discord sender (20 ms frames, DTX gaps between spurts)
//!   -> network (base delay + jitter + loss; lost packets
//!      NEVER arrive — @discordjs/voice 0.19.2 onUdpMessage
//!      pushes packets in arrival order with no sequence
//!      check, no reorder, no concealment, verified in the
//!      installed dist source; prism then decodes what it
//!      gets, so a lost packet COMPRESSES the decoded
//!      timeline by 20 ms)
//!   -> Node leg (decode + reframe + socket write are
//!      arrival-clock: frames are emitted the moment they
//!      are decoded; optional event-loop stall windows delay
//!      batches, in order)
//!   -> production Mixer (real code, 20 ms tick; optional
//!      tick-schedule stalls modelling MissedTickBehavior::
//!      Delay under CPU load)
//!
//! Metrics per run: mid-spurt hole events (ticks where the
//! spurt is established and should be audible but the source
//! contributes nothing), total hole ms, longest hole, and
//! drop-oldest count. Initial-prebuffer ticks at spurt start
//! are excluded (grace window) — the defect under test is
//! MID-spurt discontinuity, not startup latency.
//!
//! Variants are modelled by ModelMixer, a knobbed replica of
//! the production JitterBuffer/Mixer semantics. The harness
//! asserts the model with as-built knobs reproduces the
//! production Mixer tick-for-tick on the same trace, so
//! variant numbers are measured against a validated model.

use pcm_protocol::{Mixer, Side, FRAME_SAMPLES};

// ---------- deterministic PRNG (xorshift64*) ----------
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.f64() * (hi - lo)
    }
    fn normal(&mut self) -> f64 {
        // Box-Muller
        let u1 = self.f64().max(1e-12);
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
    fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
}

// ---------- scenario ----------
#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    loss: f64,        // per-packet loss probability
    jitter_ms: f64,   // sigma of per-packet arrival jitter
    spike_p: f64,     // probability a packet takes a big extra delay
    node_stall_p: f64, // per-second probability an event-loop stall window opens
    tick_stall_p: f64, // per-tick probability the mixer clock slips (Delay behavior)
    two_speakers: bool,
}

const DUR_MS: u64 = 180_000; // 3 minutes of simulated time
const BASE_DELAY_MS: f64 = 25.0;

/// One speaker's sender trace: (emit_ms, frame) in sender time.
fn make_sender(rng: &mut Rng, freq: f64, amp: f64) -> Vec<(u64, Vec<i16>)> {
    let mut out = Vec::new();
    let mut t = 500.0f64; // first spurt starts at 0.5 s
    while t < (DUR_MS as f64 - 8_000.0) {
        let spurt_ms = rng.range(1_500.0, 6_000.0);
        let frames = (spurt_ms / 20.0) as usize;
        let start_sample = (t * 48.0) as usize;
        for k in 0..frames {
            let mut frame = Vec::with_capacity(FRAME_SAMPLES);
            for i in 0..FRAME_SAMPLES {
                let n = start_sample + k * FRAME_SAMPLES + i;
                let s = (2.0 * std::f64::consts::PI * freq * n as f64 / 48_000.0).sin();
                let s2 = (2.0 * std::f64::consts::PI * freq * 2.0 * n as f64 / 48_000.0).sin();
                frame.push((amp * (s + 0.4 * s2)) as i16);
            }
            out.push(((t + (k * 20) as f64) as u64, frame));
        }
        t += spurt_ms + rng.range(400.0, 1_500.0);
    }
    out
}

/// Ground truth: spurt windows in MIXER time (sender time +
/// base delay), for hole accounting. Returns (start_ms,
/// end_ms) per spurt.
fn spurt_windows(sender: &[(u64, Vec<i16>)]) -> Vec<(u64, u64)> {
    let mut wins = Vec::new();
    for (t, _) in sender {
        if let Some(last) = wins.last_mut() {
            let (_, e) = last;
            if *t <= *e + 20 {
                *e = *t + 20;
                continue;
            }
        }
        wins.push((*t + BASE_DELAY_MS as u64, *t + BASE_DELAY_MS as u64 + 20));
    }
    wins
}

/// Apply network + Node model: returns deliveries sorted by
/// (delivery_ms, seq): (delivery_ms, seq, sender_ts_ms, frame).
fn deliver(
    rng: &mut Rng,
    sender: &[(u64, Vec<i16>)],
    sc: &Scenario,
) -> Vec<(u64, u32, u32, Vec<i16>)> {
    // Node stall windows: [start_ms, len_ms] in delivery time.
    let mut stalls: Vec<(u64, u64)> = Vec::new();
    let mut st = 0u64;
    while st < DUR_MS {
        if rng.chance(sc.node_stall_p / 50.0) {
            // p is per-second; step is 20 ms -> p/50 per step
            stalls.push((st, rng.range(30.0, 80.0) as u64));
        }
        st += 20;
    }
    let mut out: Vec<(u64, u32, u32, Vec<i16>)> = Vec::new();
    for (seq, (emit, frame)) in sender.iter().enumerate() {
        if rng.chance(sc.loss) {
            continue; // lost forever: timeline compresses
        }
        let mut d = *emit as f64 + BASE_DELAY_MS + rng.normal() * sc.jitter_ms;
        if rng.chance(sc.spike_p) {
            d += rng.range(30.0, 80.0);
        }
        let mut d = d.max(*emit as f64 + 5.0) as u64;
        // Event-loop stall: anything arriving inside a stall
        // window is processed at the window's end, in order.
        for (s, len) in &stalls {
            if d >= *s && d < *s + *len {
                d = *s + *len;
            }
        }
        out.push((d, seq as u32, *emit as u32, frame.clone()));
    }
    out.sort_by_key(|(d, seq, _, _)| (*d, *seq));
    out
}

// ---------- model mixer (knobbed replica) ----------
#[derive(Clone, Copy, PartialEq)]
struct Knobs {
    target: usize,
    /// Max consecutive underrun ticks ridden through with a
    /// concealment frame (repeat of last frame, decayed)
    /// before falling back to re-prebuffer. 0 = as-built.
    ride: usize,
    /// Timeline repair at push: use (seq, sender_ts) to
    /// detect holes and insert concealment frames so the
    /// buffer timeline matches the sender's clock.
    repair: bool,
}

struct ModelBuf {
    q: std::collections::VecDeque<Vec<i16>>,
    started: bool,
    last: Option<Vec<i16>>,
    ride_left: usize,
    dropped: u64,
    last_seq: Option<u32>,
    last_ts: Option<u32>,
}

impl ModelBuf {
    fn new() -> Self {
        Self {
            q: std::collections::VecDeque::new(),
            started: false,
            last: None,
            ride_left: 0,
            dropped: 0,
            last_seq: None,
            last_ts: None,
        }
    }
    fn push(&mut self, seq: u32, ts: u32, frame: Vec<i16>, k: &Knobs) {
        if k.repair {
            if let (Some(ls), Some(lt)) = (self.last_seq, self.last_ts) {
                let seq_gap = seq.wrapping_sub(ls) as i64;
                let expect_ts = lt as i64 + 20 * seq_gap;
                let hole_ms = ts as i64 - expect_ts;
                if hole_ms >= 30 {
                    let n = ((hole_ms + 10) / 20).min(5) as usize;
                    if let Some(last) = self.last.clone() {
                        for i in 0..n {
                            let f: Vec<i16> = last
                                .iter()
                                .map(|s| (*s as f32 * 0.75f32.powi(i as i32 + 1)) as i16)
                                .collect();
                            self.push_raw(f, k);
                        }
                    }
                }
            }
            self.last_seq = Some(seq);
            self.last_ts = Some(ts);
        }
        self.push_raw(frame, k);
    }
    fn push_raw(&mut self, frame: Vec<i16>, k: &Knobs) {
        if self.q.len() >= 10 {
            self.q.pop_front();
            self.dropped += 1;
        }
        self.q.push_back(frame);
        if self.q.len() >= k.target {
            self.started = true;
        }
    }
    fn pop(&mut self, k: &Knobs) -> Option<Vec<i16>> {
        if !self.started {
            return None;
        }
        match self.q.pop_front() {
            Some(f) => {
                self.last = Some(f.clone());
                self.ride_left = k.ride;
                Some(f)
            }
            None => {
                if self.ride_left > 0 {
                    self.ride_left -= 1;
                    let last = self.last.as_ref()?;
                    let decay = 0.7f32.powi((k.ride - self.ride_left) as i32);
                    Some(last.iter().map(|s| (*s as f32 * decay) as i16).collect())
                } else {
                    self.started = false;
                    None
                }
            }
        }
    }
}

// ---------- measurement ----------
#[derive(Default, Debug)]
struct Metrics {
    hole_events: u64,
    hole_ms: u64,
    longest_hole_ms: u64,
    dropped: u64,
    ticks_audible: u64,
}

fn goertzel(samples: &[i16], freq: f64) -> f64 {
    let n = samples.len() as f64;
    let w = 2.0 * std::f64::consts::PI * freq / 48_000.0;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = x as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / n
}

fn in_window(wins: &[(u64, u64)], t: u64, grace_start: u64, tail: u64) -> bool {
    wins.iter().any(|(s, e)| t >= *s + grace_start && t <= *e + tail)
}

/// Run one scenario against the PRODUCTION Mixer.
fn run_production(sc: &Scenario, seed: u64) -> (Metrics, Vec<Option<Vec<i16>>>) {
    let mut rng = Rng(seed);
    let s_a = make_sender(&mut rng, 300.0, 8000.0);
    let d_a = deliver(&mut rng, &s_a, sc);
    let wins_a = spurt_windows(&s_a);
    let (s_b, d_b, wins_b) = if sc.two_speakers {
        let s = make_sender(&mut rng, 900.0, 8000.0);
        let d = deliver(&mut rng, &s, sc);
        let w = spurt_windows(&s);
        (Some(s), d, w)
    } else {
        (None, Vec::new(), Vec::new())
    };
    let _ = s_b;

    let mut mixer = Mixer::new();
    let mut m = Metrics::default();
    let mut outputs: Vec<Option<Vec<i16>>> = Vec::new();
    let mut hole_run = 0u64;
    let mut ia = 0usize;
    let mut ib = 0usize;
    // Tick schedule: 20 ms, with Delay-style slips.
    let mut tick_at = 20u64;
    let mut trng = Rng(seed ^ 0x77aa);
    while tick_at < DUR_MS {
        while ia < d_a.len() && d_a[ia].0 <= tick_at {
            mixer.push(Side::Discord, 1, d_a[ia].3.clone());
            ia += 1;
        }
        while ib < d_b.len() && d_b[ib].0 <= tick_at {
            mixer.push(Side::Discord, 2, d_b[ib].3.clone());
            ib += 1;
        }
        let (_, to_armada) = mixer.tick();
        // Measure speaker A audibility (band energy at 300 Hz).
        let active = in_window(&wins_a, tick_at, 100, 40);
        let audible = to_armada
            .as_ref()
            .map(|f| goertzel(f, 300.0) > 400.0)
            .unwrap_or(false);
        if audible {
            m.ticks_audible += 1;
        }
        if active && !audible {
            hole_run += 1;
            m.hole_ms += 20;
        } else {
            if hole_run > 0 {
                m.hole_events += 1;
                m.longest_hole_ms = m.longest_hole_ms.max(hole_run * 20);
            }
            hole_run = 0;
        }
        outputs.push(to_armada);
        tick_at += 20;
        if sc.tick_stall_p > 0.0 && trng.chance(sc.tick_stall_p) {
            tick_at += trng.range(15.0, 40.0) as u64;
        }
    }
    if hole_run > 0 {
        m.hole_events += 1;
        m.longest_hole_ms = m.longest_hole_ms.max(hole_run * 20);
    }
    m.dropped = mixer.dropped_frames;
    let _ = wins_b;
    (m, outputs)
}

/// Run one scenario against the MODEL mixer with knobs.
fn run_model(sc: &Scenario, seed: u64, k: &Knobs) -> (Metrics, Vec<Option<Vec<i16>>>) {
    let mut rng = Rng(seed);
    let s_a = make_sender(&mut rng, 300.0, 8000.0);
    let d_a = deliver(&mut rng, &s_a, sc);
    let wins_a = spurt_windows(&s_a);
    let d_b = if sc.two_speakers {
        let s = make_sender(&mut rng, 900.0, 8000.0);
        deliver(&mut rng, &s, sc)
    } else {
        Vec::new()
    };

    let mut a = ModelBuf::new();
    let mut b = ModelBuf::new();
    let mut m = Metrics::default();
    let mut outputs: Vec<Option<Vec<i16>>> = Vec::new();
    let mut hole_run = 0u64;
    let mut ia = 0usize;
    let mut ib = 0usize;
    let mut tick_at = 20u64;
    let mut trng = Rng(seed ^ 0x77aa);
    while tick_at < DUR_MS {
        while ia < d_a.len() && d_a[ia].0 <= tick_at {
            a.push(d_a[ia].1, d_a[ia].2, d_a[ia].3.clone(), k);
            ia += 1;
        }
        while ib < d_b.len() && d_b[ib].0 <= tick_at {
            b.push(d_b[ib].1, d_b[ib].2, d_b[ib].3.clone(), k);
            ib += 1;
        }
        // Mix like production mix_direction(Side::Discord):
        // sum whatever each buffer yields this tick.
        let fa = a.pop(k);
        let fb = b.pop(k);
        let out = match (fa, fb) {
            (None, None) => None,
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (Some(x), Some(y)) => Some(
                x.iter()
                    .zip(y.iter())
                    .map(|(p, q)| (*p as i32 + *q as i32).clamp(-32768, 32767) as i16)
                    .collect(),
            ),
        };
        let active = in_window(&wins_a, tick_at, 100, 40);
        let audible = out
            .as_ref()
            .map(|f| goertzel(f, 300.0) > 400.0)
            .unwrap_or(false);
        if audible {
            m.ticks_audible += 1;
        }
        if active && !audible {
            hole_run += 1;
            m.hole_ms += 20;
        } else {
            if hole_run > 0 {
                m.hole_events += 1;
                m.longest_hole_ms = m.longest_hole_ms.max(hole_run * 20);
            }
            hole_run = 0;
        }
        outputs.push(out);
        tick_at += 20;
        if sc.tick_stall_p > 0.0 && trng.chance(sc.tick_stall_p) {
            tick_at += trng.range(15.0, 40.0) as u64;
        }
    }
    if hole_run > 0 {
        m.hole_events += 1;
        m.longest_hole_ms = m.longest_hole_ms.max(hole_run * 20);
    }
    m.dropped = a.dropped + b.dropped;
    (m, outputs)
}

fn fmt(name: &str, m: &Metrics) {
    println!(
        "{name:<46} holes={:<4} hole_ms={:<6} longest={:<5} dropped={:<5} audible_ticks={}",
        m.hole_events, m.hole_ms, m.longest_hole_ms, m.dropped, m.ticks_audible
    );
}

/// Ignored by default: a full run takes ~90 s. Run with:
///   cargo test -p pcm-protocol --test robotic_harness -- --ignored --nocapture
/// Post-fix status (2026-11-23): production now ships the
/// V2 ride-through, so the model-validation knobs below are
/// ride=5; the pre-fix behavior is kept as the LEGACY
/// model-only variant for the record.
#[test]
#[ignore]
fn robotic_blips_harness() {
    let scenarios = [
        Scenario { name: "S1 clean (loss 0, jitter only)", loss: 0.0, jitter_ms: 6.0, spike_p: 0.01, node_stall_p: 0.0, tick_stall_p: 0.0, two_speakers: false },
        Scenario { name: "S2 loss 1%", loss: 0.01, jitter_ms: 6.0, spike_p: 0.01, node_stall_p: 0.0, tick_stall_p: 0.0, two_speakers: false },
        Scenario { name: "S3 loss 0.5%", loss: 0.005, jitter_ms: 6.0, spike_p: 0.01, node_stall_p: 0.0, tick_stall_p: 0.0, two_speakers: false },
        Scenario { name: "S4 loss 2%", loss: 0.02, jitter_ms: 6.0, spike_p: 0.01, node_stall_p: 0.0, tick_stall_p: 0.0, two_speakers: false },
        Scenario { name: "S5 loss 1% + loaded box (stalls)", loss: 0.01, jitter_ms: 6.0, spike_p: 0.01, node_stall_p: 0.5, tick_stall_p: 0.01, two_speakers: false },
        Scenario { name: "S6 loss 1%, two speakers", loss: 0.01, jitter_ms: 6.0, spike_p: 0.01, node_stall_p: 0.0, tick_stall_p: 0.0, two_speakers: true },
    ];
    let as_built = Knobs { target: 3, ride: 5, repair: false };
    let variants: Vec<(&str, Knobs)> = vec![
        ("LEGACY ride=0 (pre-fix production)", Knobs { target: 3, ride: 0, repair: false }),
        ("V1 target=5", Knobs { target: 5, ride: 0, repair: false }),
        ("V3 repair ts/seq", Knobs { target: 3, ride: 0, repair: true }),
        ("V4 ride=5 + target=5", Knobs { target: 5, ride: 5, repair: false }),
        ("V5 repair + ride=5", Knobs { target: 3, ride: 5, repair: true }),
    ];
    for sc in &scenarios {
        println!("\n=== {} ===", sc.name);
        let (mp, out_p) = run_production(sc, 0xC0FFEE);
        fmt("PRODUCTION (ride-through)", &mp);
        let (mm, out_m) = run_model(sc, 0xC0FFEE, &as_built);
        fmt("MODEL production knobs", &mm);
        // Model validation: same trace, same schedule -> the
        // model with as-built knobs must match production
        // audibility tick-for-tick (single-speaker scenarios;
        // two-speaker mixes sum in a different order but hole
        // accounting for speaker A is equivalent — compare
        // metrics only there).
        assert_eq!(mp.hole_events, mm.hole_events, "{}: model/prod hole events diverge", sc.name);
        assert_eq!(mp.hole_ms, mm.hole_ms, "{}: model/prod hole ms diverge", sc.name);
        if !sc.two_speakers {
            assert_eq!(out_p.len(), out_m.len());
            for (i, (x, y)) in out_p.iter().zip(out_m.iter()).enumerate() {
                assert_eq!(x, y, "{}: model/prod output diverge at tick {i}", sc.name);
            }
        }
        for (name, k) in &variants {
            let (mv, _) = run_model(sc, 0xC0FFEE, k);
            fmt(name, &mv);
        }
    }
}

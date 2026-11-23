// Delivery measurement core for the Phase-0 DAVE spike.
//
// The question this answers (bridge spec §11 / issue #11419):
// when @discordjs/voice + davey receive a user's audio, what
// fraction of the Opus packets that SHOULD arrive actually
// do — idle, and during MLS key transitions (members joining
// and leaving)?
//
// Method: every Opus packet carries an RTP timestamp at
// 48 kHz; one voice packet covers 960 samples (20 ms). From
// the span of timestamps observed for a user, the expected
// packet count is (lastTs - firstTs)/960 + 1. Delivery is
// received/expected. Timestamps are unwrapped per user
// (32-bit wrap at 2^32). Duplicate timestamps count once.
//
// The meter is deliberately transport-blind: the spike feeds
// it (userId, rtpTimestamp, arrivalMs) triples from the real
// receiver, and the selftest feeds it synthetic triples —
// the math being verified is identical.

export const SAMPLES_PER_PACKET = 960;
export const GATE_THRESHOLD = 0.98;

class UserTrack {
  constructor() {
    this.seen = new Set(); // unwrapped timestamps
    this.firstTs = null;
    this.lastTs = null;
    this.lastRaw = null;
    this.unwrapOffset = 0;
    this.received = 0;
    this.duplicates = 0;
    this.decryptFailures = 0;
    // Per-minute buckets by ARRIVAL time: { received, spanSamples }
    this.minutes = new Map();
  }

  unwrap(rawTs) {
    if (this.lastRaw === null) {
      this.lastRaw = rawTs;
      return rawTs;
    }
    let delta = rawTs - this.lastRaw;
    if (delta < -0x80000000) this.unwrapOffset += 0x100000000;
    else if (delta > 0x80000000) this.unwrapOffset -= 0x100000000;
    this.lastRaw = rawTs;
    return rawTs + this.unwrapOffset;
  }
}

export class DeliveryMeter {
  constructor() {
    this.users = new Map();
  }

  #track(userId) {
    let t = this.users.get(userId);
    if (!t) {
      t = new UserTrack();
      this.users.set(userId, t);
    }
    return t;
  }

  /** One received Opus packet. */
  observe(userId, rawRtpTimestamp, arrivalMs = Date.now()) {
    const t = this.#track(userId);
    const ts = t.unwrap(rawRtpTimestamp >>> 0);
    const minute = Math.floor(arrivalMs / 60000);
    let bucket = t.minutes.get(minute);
    if (!bucket) {
      bucket = { received: 0, firstTs: ts, lastTs: ts };
      t.minutes.set(minute, bucket);
    }
    if (t.seen.has(ts)) {
      t.duplicates++;
      return;
    }
    t.seen.add(ts);
    t.received++;
    bucket.received++;
    if (t.firstTs === null || ts < t.firstTs) t.firstTs = ts;
    if (t.lastTs === null || ts > t.lastTs) t.lastTs = ts;
    if (ts < bucket.firstTs) bucket.firstTs = ts;
    if (ts > bucket.lastTs) bucket.lastTs = ts;
  }

  /** A packet the receiver reports as undecryptable. */
  observeDecryptFailure(userId) {
    this.#track(userId).decryptFailures++;
  }

  static #expected(firstTs, lastTs) {
    if (firstTs === null || lastTs === null || lastTs < firstTs) return 0;
    return Math.round((lastTs - firstTs) / SAMPLES_PER_PACKET) + 1;
  }

  /** Per-user + overall delivery report. */
  report() {
    const users = {};
    let allReceived = 0;
    let allExpected = 0;
    let worstMinute = 1;
    for (const [userId, t] of this.users) {
      const expected = DeliveryMeter.#expected(t.firstTs, t.lastTs);
      const minutes = [...t.minutes.entries()]
        .sort((a, b) => a[0] - b[0])
        .map(([minute, b]) => {
          const exp = DeliveryMeter.#expected(b.firstTs, b.lastTs);
          const delivery = exp > 0 ? b.received / exp : 1;
          if (exp >= 50 && delivery < worstMinute) worstMinute = delivery;
          return { minute, received: b.received, expected: exp, delivery };
        });
      users[userId] = {
        received: t.received,
        expected,
        delivery: expected > 0 ? t.received / expected : 1,
        duplicates: t.duplicates,
        decryptFailures: t.decryptFailures,
        minutes,
      };
      allReceived += t.received;
      allExpected += expected;
    }
    return {
      users,
      overall: {
        received: allReceived,
        expected: allExpected,
        delivery: allExpected > 0 ? allReceived / allExpected : 1,
        worstMinute,
      },
    };
  }

  /**
   * The Phase-0 gate: overall delivery AND the worst
   * substantive minute (>= 50 expected packets) must both
   * clear the threshold. Averages hide transition loss;
   * #11419 is exactly a worst-minute failure.
   */
  verdict(threshold = GATE_THRESHOLD) {
    const r = this.report();
    return {
      pass: r.overall.delivery >= threshold && r.overall.worstMinute >= threshold,
      threshold,
      overall: r.overall,
    };
  }
}

// The Janus PCM socket protocol — Node mirror of the Rust
// `pcm-protocol` crate. Wire format: shared/pcm-protocol.md.
//
// u32 LE length prefix (header + payload), 24-byte header,
// then payload. PCM payloads are one 20 ms frame of 48 kHz
// mono s16le (960 samples, 1,920 bytes). No backpressure:
// every queue is a bounded drop-oldest ring.

export const FRAME_SAMPLES = 960;
export const FRAME_BYTES = FRAME_SAMPLES * 2;
export const HEADER_LEN = 24;
export const MAX_FRAME_LEN = 64 * 1024;
export const RING_CAPACITY = 10;

export const FrameType = {
  PcmDown: 1, // Discord -> mixer, per-user
  PcmUp: 2, // Armada -> mixer, per-participant
  MixOut: 3, // mixer -> a leg (that leg's incoming mix)
  Speak: 4, // speaking start/stop (flags bit0 = start)
  Control: 5, // UTF-8 JSON control plane
};

const MAGIC = 0x42; // 'B'
const VERSION = 1;

/** Encode one frame. speakerId is a BigInt (Discord snowflakes don't fit in f64). */
export function encodeFrame({ kind, flags = 0, speakerId = 0n, seq = 0, timestampMs = 0, samples = 0, energy = 0, payload = Buffer.alloc(0) }) {
  const total = HEADER_LEN + payload.length;
  const out = Buffer.alloc(4 + total);
  out.writeUInt32LE(total, 0);
  out[4] = MAGIC;
  out[5] = VERSION;
  out[6] = kind;
  out[7] = flags;
  out.writeBigUInt64LE(BigInt(speakerId), 8);
  out.writeUInt32LE(seq >>> 0, 16);
  out.writeUInt32LE(timestampMs >>> 0, 20);
  out.writeUInt16LE(samples, 24);
  out[26] = energy;
  out[27] = 0; // reserved
  payload.copy(out, 28);
  return out;
}

/** Build a PCM frame from an Int16Array of mono samples. */
export function pcmFrame(kind, speakerId, seq, timestampMs, samples) {
  const payload = Buffer.alloc(samples.length * 2);
  for (let i = 0; i < samples.length; i++) payload.writeInt16LE(samples[i], i * 2);
  return encodeFrame({
    kind,
    speakerId,
    seq,
    timestampMs,
    samples: samples.length,
    energy: energyBucket(samples),
    payload,
  });
}

export function controlFrame(obj) {
  return encodeFrame({ kind: FrameType.Control, payload: Buffer.from(JSON.stringify(obj), 'utf8') });
}

export function speakFrame(speakerId, start) {
  return encodeFrame({ kind: FrameType.Speak, flags: start ? 1 : 0, speakerId });
}

/** Decode a frame's PCM payload to an Int16Array. */
export function framePcm(frame) {
  const n = Math.floor(frame.payload.length / 2);
  const out = new Int16Array(n);
  for (let i = 0; i < n; i++) out[i] = frame.payload.readInt16LE(i * 2);
  return out;
}

/**
 * Incremental decoder with the same resync contract as the
 * Rust side: on a protocol violation, drop ONE byte and scan
 * for the next plausible frame start.
 */
export class StreamDecoder {
  constructor() {
    this.buf = Buffer.alloc(0);
    this.resyncs = 0;
  }

  feed(bytes) {
    this.buf = this.buf.length === 0 ? Buffer.from(bytes) : Buffer.concat([this.buf, bytes]);
  }

  /** Next whole frame, or null. Frame: {kind, flags, speakerId(BigInt), seq, timestampMs, samples, energy, payload}. */
  nextFrame() {
    for (;;) {
      if (this.buf.length < 4) return null;
      const total = this.buf.readUInt32LE(0);
      if (total < HEADER_LEN || total > MAX_FRAME_LEN) {
        this.#dropOne();
        continue;
      }
      // Validate the header as soon as it is buffered, BEFORE
      // waiting for the whole frame — a garbage prefix can
      // fake an in-range length and stall the decoder forever
      // (mirrors the Rust crate).
      if (this.buf.length >= 4 + HEADER_LEN) {
        const h = this.buf.subarray(4, 4 + HEADER_LEN);
        if (h[0] !== MAGIC || h[1] !== VERSION || h[2] < 1 || h[2] > 5) {
          this.#dropOne();
          continue;
        }
      }
      if (this.buf.length < 4 + total) return null;
      const h = this.buf.subarray(4, 4 + HEADER_LEN);
      const frame = {
        kind: h[2],
        flags: h[3],
        speakerId: h.readBigUInt64LE(4),
        seq: h.readUInt32LE(12),
        timestampMs: h.readUInt32LE(16),
        samples: h.readUInt16LE(20),
        energy: h[22],
        payload: Buffer.from(this.buf.subarray(4 + HEADER_LEN, 4 + total)),
      };
      this.buf = this.buf.subarray(4 + total);
      return frame;
    }
  }

  #dropOne() {
    this.buf = this.buf.subarray(1);
    this.resyncs++;
  }
}

/** FNV-1a 64-bit of a string, as BigInt (Armada speaker ids). */
export function fnv1a64(str) {
  let hash = 0xcbf29ce484222325n;
  for (const b of Buffer.from(str, 'utf8')) {
    hash ^= BigInt(b);
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash;
}

/** RMS energy bucket 0-255 (mirrors the Rust crate). */
export function energyBucket(samples) {
  if (samples.length === 0) return 0;
  let sum = 0;
  for (const s of samples) sum += s * s;
  const rms = Math.sqrt(sum / samples.length);
  return Math.min(255, Math.floor((rms / 32767) * 255));
}

/** Bounded drop-oldest ring — the only queue shape allowed. */
export class DropRing {
  constructor(capacity = RING_CAPACITY) {
    this.capacity = Math.max(1, capacity);
    this.items = [];
    this.dropped = 0;
  }

  push(item) {
    if (this.items.length >= this.capacity) {
      this.items.shift();
      this.dropped++;
    }
    this.items.push(item);
  }

  shift() {
    return this.items.shift();
  }

  get length() {
    return this.items.length;
  }

  clear() {
    this.items = [];
  }
}

/** Downmix 48 kHz stereo s16le to mono (average, clamped). */
export function downmixStereoToMono(stereo) {
  const n = Math.floor(stereo.length / 2);
  const out = new Int16Array(n);
  for (let i = 0; i < n; i++) {
    out[i] = Math.trunc((stereo[2 * i] + stereo[2 * i + 1]) / 2);
  }
  return out;
}

/** Duplicate mono to stereo (for the Discord send path). */
export function monoToStereo(mono) {
  const out = new Int16Array(mono.length * 2);
  for (let i = 0; i < mono.length; i++) {
    out[2 * i] = mono[i];
    out[2 * i + 1] = mono[i];
  }
  return out;
}

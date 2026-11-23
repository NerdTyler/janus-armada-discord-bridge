import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  encodeFrame,
  pcmFrame,
  controlFrame,
  framePcm,
  StreamDecoder,
  FrameType,
  fnv1a64,
  energyBucket,
  DropRing,
  downmixStereoToMono,
} from '../src/protocol.mjs';

test('golden wire bytes match the Rust crate', () => {
  // Mirrors `golden_wire_bytes` in crates/pcm-protocol — the
  // two implementations must stay byte-identical.
  const bytes = controlFrame({});
  const expected = Buffer.from([
    26, 0, 0, 0,
    0x42, 1, 5, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0,
    0, 0, 0, 0,
    0, 0,
    0, 0,
    0x7b, 0x7d,
  ]);
  assert.deepEqual(bytes, expected);
});

test('pcm frame roundtrip', () => {
  const samples = new Int16Array(960);
  for (let i = 0; i < 960; i++) samples[i] = Math.round(Math.sin(i * 0.3) * 10000);
  const bytes = pcmFrame(FrameType.PcmDown, 123456789n, 7, 42000, samples);
  assert.equal(bytes.length, 4 + 24 + 1920);
  const dec = new StreamDecoder();
  dec.feed(bytes);
  const frame = dec.nextFrame();
  assert.equal(frame.kind, FrameType.PcmDown);
  assert.equal(frame.speakerId, 123456789n);
  assert.equal(frame.seq, 7);
  assert.equal(frame.timestampMs, 42000);
  assert.equal(frame.samples, 960);
  assert.deepEqual(framePcm(frame), samples);
  assert.equal(dec.nextFrame(), null);
});

test('decoder resyncs after garbage', () => {
  const dec = new StreamDecoder();
  dec.feed(Buffer.concat([Buffer.from([0xff, 0x00, 0x13]), controlFrame({ cmd: 'status' })]));
  const frame = dec.nextFrame();
  assert.equal(frame.kind, FrameType.Control);
  assert.deepEqual(JSON.parse(frame.payload.toString('utf8')), { cmd: 'status' });
  assert.ok(dec.resyncs >= 1);
});

test('decoder handles split feeds', () => {
  const bytes = pcmFrame(FrameType.PcmUp, 99n, 1, 20, new Int16Array(960).fill(5000));
  const dec = new StreamDecoder();
  dec.feed(bytes.subarray(0, 7));
  assert.equal(dec.nextFrame(), null);
  dec.feed(bytes.subarray(7));
  assert.equal(dec.nextFrame()?.speakerId, 99n);
});

test('snowflake speaker ids survive (BigInt)', () => {
  const id = 1234567890123456789n; // > Number.MAX_SAFE_INTEGER
  const bytes = encodeFrame({ kind: FrameType.Speak, flags: 1, speakerId: id });
  const dec = new StreamDecoder();
  dec.feed(bytes);
  assert.equal(dec.nextFrame().speakerId, id);
});

test('fnv1a64 matches the Rust reference values', () => {
  assert.equal(fnv1a64(''), 0xcbf29ce484222325n);
  assert.equal(fnv1a64('a'), 0xaf63dc4c8601ec8cn);
});

test('energy bucket scales like the Rust side', () => {
  assert.equal(energyBucket(new Int16Array(0)), 0);
  assert.equal(energyBucket(new Int16Array(960)), 0);
  assert.equal(energyBucket(new Int16Array(960).fill(32767)), 255);
});

test('drop ring drops oldest and counts', () => {
  const r = new DropRing(3);
  for (let i = 0; i < 5; i++) r.push(i);
  assert.equal(r.dropped, 2);
  assert.equal(r.shift(), 2);
  assert.equal(r.shift(), 3);
  assert.equal(r.shift(), 4);
  assert.equal(r.shift(), undefined);
});

test('stereo downmix averages', () => {
  const stereo = new Int16Array([100, 300, -100, -300, 32767, 32767]);
  assert.deepEqual(downmixStereoToMono(stereo), new Int16Array([200, -200, 32767]));
});

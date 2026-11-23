import { test } from 'node:test';
import assert from 'node:assert/strict';
import { DeliveryMeter, SAMPLES_PER_PACKET } from '../src/delivery-meter.mjs';

/** Feed `seconds` of continuous speech starting at ts0; drop packets where drop(i) is true. */
function feedSpeech(meter, user, seconds, { ts0 = 0, startMs = 0, drop = () => false } = {}) {
  const packets = Math.round(seconds * 50);
  for (let i = 0; i < packets; i++) {
    if (drop(i)) continue;
    meter.observe(user, (ts0 + i * SAMPLES_PER_PACKET) >>> 0, startMs + i * 20);
  }
}

test('perfect stream measures 100% and passes the gate', () => {
  const m = new DeliveryMeter();
  feedSpeech(m, 'alice', 60);
  const r = m.report();
  assert.equal(r.users.alice.received, 3000);
  assert.equal(r.users.alice.expected, 3000);
  assert.equal(r.users.alice.delivery, 1);
  assert.equal(m.verdict().pass, true);
});

test('the #11419 shape: 34% loss inside a transition window fails the gate', () => {
  const m = new DeliveryMeter();
  // 55 s of speech; between seconds 20-30 (an MLS rekey window)
  // roughly every third packet vanishes.
  feedSpeech(m, 'bob', 55, {
    drop: (i) => {
      const sec = i / 50;
      return sec >= 20 && sec < 30 && i % 3 === 0;
    },
  });
  const r = m.report();
  assert.ok(r.users.bob.delivery < 0.98, `overall ${r.users.bob.delivery}`);
  assert.equal(m.verdict().pass, false);
});

test('duplicates do not inflate delivery', () => {
  const m = new DeliveryMeter();
  feedSpeech(m, 'carol', 10);
  feedSpeech(m, 'carol', 10); // exact replay
  const r = m.report();
  assert.equal(r.users.carol.received, 500);
  assert.equal(r.users.carol.duplicates, 500);
  assert.equal(r.users.carol.delivery, 1);
});

test('timestamp wraparound is unwrapped', () => {
  const m = new DeliveryMeter();
  // Start just before the 2^32 wrap.
  feedSpeech(m, 'dave', 10, { ts0: 0xffff0000 });
  const r = m.report();
  assert.equal(r.users.dave.expected, 500);
  assert.equal(r.users.dave.delivery, 1);
});

test('a gap counts as loss, not as a shorter stream', () => {
  const m = new DeliveryMeter();
  // 10 s of speech with the middle 2 s missing entirely.
  feedSpeech(m, 'erin', 10, { drop: (i) => i >= 200 && i < 300 });
  const r = m.report();
  assert.equal(r.users.erin.expected, 500);
  assert.equal(r.users.erin.received, 400);
  assert.equal(r.users.erin.delivery, 0.8);
});

test('decrypt failures are tracked per user', () => {
  const m = new DeliveryMeter();
  m.observeDecryptFailure('frank');
  m.observeDecryptFailure('frank');
  assert.equal(m.report().users.frank.decryptFailures, 2);
});

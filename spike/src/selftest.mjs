// Selftest: exercise the delivery meter against synthetic
// packet streams — including the #11419 failure shape — and
// verify the analyzer reports the right rates and the gate
// gives the right answers.
//
// This verifies the MEASUREMENT, not davey. The Phase-0 gate
// itself can only be retired by the live spike (spike.mjs)
// with a real bot token and real join/leave churn.
//
// Run: npm run selftest

import { DeliveryMeter } from './delivery-meter.mjs';

function feed(meter, user, seconds, { ts0 = 0, startMs = 0, drop = () => false } = {}) {
  const packets = Math.round(seconds * 50);
  for (let i = 0; i < packets; i++) {
    if (drop(i)) continue;
    meter.observe(user, (ts0 + i * 960) >>> 0, startMs + i * 20);
  }
}

let failures = 0;
function check(name, cond, detail = '') {
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`);
  if (!cond) failures++;
}

// Scenario 1: clean channel, two speakers, 60 s each.
{
  const m = new DeliveryMeter();
  feed(m, 'alice', 60);
  feed(m, 'bob', 60, { ts0: 48000 });
  const v = m.verdict();
  check('clean streams pass the gate', v.pass, `overall=${v.overall.delivery.toFixed(4)}`);
}

// Scenario 2: the #11419 report — ~34% loss during a 10 s MLS
// transition window inside a 55 s stream.
{
  const m = new DeliveryMeter();
  feed(m, 'alice', 55, {
    drop: (i) => {
      const sec = i / 50;
      return sec >= 20 && sec < 30 && i % 3 === 0;
    },
  });
  const v = m.verdict();
  const r = m.report();
  check(
    '#11419-shaped loss fails the gate',
    !v.pass,
    `overall=${v.overall.delivery.toFixed(4)} worstMinute=${v.overall.worstMinute.toFixed(4)} alice=${r.users.alice.delivery.toFixed(4)}`,
  );
}

// Scenario 3: churn with only 1% loss — should pass.
{
  const m = new DeliveryMeter();
  feed(m, 'alice', 120, { drop: (i) => i % 100 === 0 });
  const v = m.verdict();
  check('1% loss passes the gate', v.pass, `overall=${v.overall.delivery.toFixed(4)}`);
}

console.log(failures === 0 ? '\nSELFTEST OK — the analyzer is trustworthy; the live gate still needs a token.' : `\nSELFTEST FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);

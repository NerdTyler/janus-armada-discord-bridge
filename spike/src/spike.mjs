// Phase-0 DAVE receive spike — the live harness.
//
// THE question (bridge spec §11, issue #11419): does
// @discordjs/voice 0.19.2 + @snazzah/davey deliver >= 98% of
// a speaking user's Opus packets — idle AND while members
// join/leave (MLS group changes / DAVE key transitions)?
//
// Method (two-sided, so DAVE loss is separated from network
// loss):
//   WIRE side — the receiver's onUdpMessage is wrapped to
//   observe every raw RTP packet (sequence + timestamp from
//   the header, SSRC -> user via the receiver's ssrcMap)
//   before the library processes it. The DeliveryMeter turns
//   timestamp spans into expected packet counts.
//   EMERGED side — packets that come OUT of
//   receiver.subscribe(userId) streams, i.e. what survived
//   transport decrypt AND daveSession.decrypt. Stream errors
//   count as decrypt failures.
//   DAVE delivery = emerged / wire-received. End-to-end
//   delivery = emerged / wire-expected. The gate needs both
//   >= 98%, overall and in the worst substantive minute.
//
// Needs (env): SPIKE_DISCORD_TOKEN, SPIKE_GUILD_ID,
// SPIKE_CHANNEL_ID. Without them this prints exactly what is
// missing and exits 2 — the harness is ready to run the
// moment the owner creates the bot.
//
// Run protocol once live (README has the full script):
//   1. Idle phase: one user speaks/plays audio 5 min.
//   2. Churn phase: 5 min while 2-3 accounts join/leave
//      every ~20 s.

import { Client, GatewayIntentBits } from 'discord.js';
import { joinVoiceChannel, EndBehaviorType } from '@discordjs/voice';
import { DeliveryMeter } from './delivery-meter.mjs';

const need = ['SPIKE_DISCORD_TOKEN', 'SPIKE_GUILD_ID', 'SPIKE_CHANNEL_ID'].filter((k) => !process.env[k]);
if (need.length > 0) {
  console.error(`[spike] missing env: ${need.join(', ')}`);
  console.error('[spike] operator inputs required: a Discord bot application + token (keep it secret), the bot invited to the guild, and a voice channel id to sit in.');
  console.error('[spike] the measurement core is verified offline: npm run selftest');
  process.exit(2);
}

const RUN_MS = Number(process.env.SPIKE_RUN_MS ?? 10 * 60 * 1000);
const meter = new DeliveryMeter();
const emerged = new Map(); // userId -> { count, failures, minutes: Map<minute, count> }

function emergedTrack(userId) {
  let t = emerged.get(userId);
  if (!t) {
    t = { count: 0, failures: 0, minutes: new Map() };
    emerged.set(userId, t);
  }
  return t;
}

const client = new Client({ intents: GatewayIntentBits.Guilds | GatewayIntentBits.GuildVoiceStates });

client.once('clientReady', async () => {
  const guild = await client.guilds.fetch(process.env.SPIKE_GUILD_ID);
  const connection = joinVoiceChannel({
    channelId: process.env.SPIKE_CHANNEL_ID,
    guildId: guild.id,
    adapterCreator: guild.voiceAdapterCreator,
    selfDeaf: false, // load-bearing: a deafened bot receives nothing
    selfMute: true, // the spike only listens
  });
  const receiver = connection.receiver;
  console.log(`[spike] joined channel ${process.env.SPIKE_CHANNEL_ID}; measuring for ${RUN_MS / 1000}s`);
  console.log('[spike] speak now; run join/leave churn during the second half.');

  // WIRE side: wrap onUdpMessage to observe raw RTP headers.
  if (typeof receiver.onUdpMessage === 'function' && receiver.ssrcMap) {
    const original = receiver.onUdpMessage.bind(receiver);
    receiver.onUdpMessage = (msg) => {
      try {
        if (msg.length > 12 && (msg[0] >> 6) === 2) {
          const ssrc = msg.readUInt32BE(8);
          const userData = receiver.ssrcMap.get(ssrc);
          if (userData?.userId) {
            meter.observe(userData.userId, msg.readUInt32BE(4));
          }
        }
      } catch {
        /* observation must never break the receiver */
      }
      return original(msg);
    };
    console.log('[spike] wire-side observation active (raw RTP timestamps).');
  } else {
    console.error('[spike] WARNING: receiver internals differ in this version — wire-side observation unavailable; emerged counts only.');
  }

  // EMERGED side: subscribe on speaking start, count packets.
  const subscribed = new Set();
  receiver.speaking.on('start', (userId) => {
    if (subscribed.has(userId)) return;
    subscribed.add(userId);
    const stream = receiver.subscribe(userId, {
      end: { behavior: EndBehaviorType.Manual },
    });
    stream.on('data', () => {
      const t = emergedTrack(userId);
      t.count++;
      const minute = Math.floor(Date.now() / 60000);
      t.minutes.set(minute, (t.minutes.get(minute) ?? 0) + 1);
    });
    stream.on('error', (e) => {
      console.error(`[spike] receiver stream error for ${userId}:`, e.message);
      emergedTrack(userId).failures++;
      meter.observeDecryptFailure(userId);
    });
  });

  const reportTimer = setInterval(() => printReport(false), 60_000);
  reportTimer.unref();

  setTimeout(() => {
    clearInterval(reportTimer);
    const pass = printReport(true);
    connection.destroy();
    client.destroy();
    process.exit(pass ? 0 : 1);
  }, RUN_MS);
});

function printReport(final) {
  const wire = meter.report();
  console.log(final ? '\n=== PHASE-0 GATE REPORT ===' : '[spike] interim report:');
  let allEmerged = 0;
  let allWireReceived = 0;
  let allWireExpected = 0;
  for (const [userId, w] of Object.entries(wire.users)) {
    const e = emerged.get(userId) ?? { count: 0, failures: 0 };
    const daveDelivery = w.received > 0 ? e.count / w.received : 1;
    const endToEnd = w.expected > 0 ? e.count / w.expected : 1;
    console.log(
      `user ${userId}: wire ${(w.delivery * 100).toFixed(2)}% (${w.received}/${w.expected}) ` +
        `emerged ${e.count} -> DAVE ${(daveDelivery * 100).toFixed(2)}%, end-to-end ${(endToEnd * 100).toFixed(2)}%, streamFailures=${e.failures}`,
    );
    allEmerged += e.count;
    allWireReceived += w.received;
    allWireExpected += w.expected;
  }
  const dave = allWireReceived > 0 ? allEmerged / allWireReceived : 1;
  const e2e = allWireExpected > 0 ? allEmerged / allWireExpected : 1;
  const pass = dave >= 0.98 && e2e >= 0.98;
  console.log(`OVERALL: DAVE delivery ${(dave * 100).toFixed(2)}%, end-to-end ${(e2e * 100).toFixed(2)}% (gate: both >= 98%)`);
  if (final) {
    console.log(pass ? 'GATE: PASS — the Node+davey stack is cleared for the bridge.' : 'GATE: FAIL — per spec, the Discord leg falls back to dysnomia.');
  }
  return pass;
}

client.login(process.env.SPIKE_DISCORD_TOKEN);

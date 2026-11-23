// Integration smoke test: the real janus-armada daemon (no
// Armada identity, selftest tone ON) + a Node client speaking
// the real PCM protocol over the real Unix socket.
//
// Asserts:
//   1. hello -> status push (idle, discord_connected)
//   2. MIX_OUT flows: the synthetic Armada-side tone reaches
//      the Discord leg as 960-sample frames
//   3. Mix-minus: PCM_DOWN sent by the client never comes
//      back inside MIX_OUT
//   4. A summon with no Armada identity fails cleanly with
//      session_failed (control plane works end to end)
//   5. status_request -> status push
//
// Run: node tests/integration-smoke.mjs [path-to-janus-armada]
// (build first: cargo build -p janus-armada)

import { spawn } from 'node:child_process';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  StreamDecoder,
  FrameType,
  pcmFrame,
  controlFrame,
  framePcm,
} from '../discord-leg/src/protocol.mjs';

const repo = path.dirname(fileURLToPath(import.meta.url)) + '/..';
const bin = process.argv[2] ?? path.join(repo, 'target/debug/janus-armada');
const sockDir = '/tmp/janus-smoke';
const sockPath = path.join(sockDir, 'pcm.sock');
fs.rmSync(sockDir, { recursive: true, force: true });
fs.mkdirSync(sockDir, { recursive: true });

let failures = 0;
const check = (name, cond, detail = '') => {
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`);
  if (!cond) failures++;
};

const daemon = spawn(bin, [], {
  env: {
    ...process.env,
    BRIDGE_SOCKET_PATH: sockPath,
    BRIDGE_SELFTEST_TONE: '1',
    RUST_LOG: 'info',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});
daemon.stderr.on('data', (d) => process.stderr.write(`[daemon] ${d}`));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  // Wait for the socket to appear.
  for (let i = 0; i < 50 && !fs.existsSync(sockPath); i++) await sleep(100);
  if (!fs.existsSync(sockPath)) throw new Error('daemon socket never appeared');

  const socket = net.createConnection(sockPath);
  await new Promise((res, rej) => {
    socket.once('connect', res);
    socket.once('error', rej);
  });
  const decoder = new StreamDecoder();
  const controls = [];
  const mixFrames = [];
  socket.on('data', (chunk) => {
    decoder.feed(chunk);
    let frame;
    while ((frame = decoder.nextFrame()) !== null) {
      if (frame.kind === FrameType.Control) controls.push(JSON.parse(frame.payload.toString('utf8')));
      else if (frame.kind === FrameType.MixOut) mixFrames.push(framePcm(frame));
    }
  });

  // 1. hello -> status
  socket.write(controlFrame({ cmd: 'hello', role: 'discord' }));
  await sleep(300);
  const status = controls.find((c) => c.cmd === 'status');
  check('hello answered with a status push', !!status, status ? `state=${status.state}` : 'none received');
  check('status reports the discord leg connected', status?.discord_connected === true);

  // 2. MIX_OUT tone flow
  mixFrames.length = 0;
  await sleep(1000);
  const toneFrames = mixFrames.filter((f) => f.length === 960);
  check('MIX_OUT frames flow (tone)', toneFrames.length >= 30, `${toneFrames.length} frames in 1s`);
  const peak = toneFrames.reduce((m, f) => Math.max(m, ...f.map((s) => Math.abs(s))), 0);
  check('tone is audible-level, not silence', peak > 3000, `peak=${peak}`);

  // 3. Mix-minus: blast a loud DC frame as a Discord user;
  //    it must NOT return in MIX_OUT.
  const dc = new Int16Array(960).fill(20000);
  mixFrames.length = 0;
  for (let i = 0; i < 25; i++) {
    socket.write(pcmFrame(FrameType.PcmDown, 42n, i + 1, i * 20, dc));
    await sleep(20);
  }
  await sleep(300);
  const leaked = mixFrames.some((f) => f.every((s) => s === 20000));
  check('mix-minus holds: own PCM_DOWN never returns', !leaked, `${mixFrames.length} mix frames observed`);
  const stillTone = mixFrames.some((f) => Math.max(...f.map((s) => Math.abs(s))) > 3000);
  check('tone still flowing during PCM_DOWN', stillTone);

  // 4. Summon without an Armada identity -> session_failed
  controls.length = 0;
  socket.write(
    controlFrame({ cmd: 'session_start', origin: 'discord', discord_channel_id: '1', discord_channel_name: 'General', armada_named: null }),
  );
  await sleep(500);
  const failed = controls.find((c) => c.cmd === 'session_failed');
  check('summon without identity fails cleanly', !!failed, failed?.error ?? 'no session_failed');

  // 5. status_request
  controls.length = 0;
  socket.write(controlFrame({ cmd: 'status_request' }));
  await sleep(300);
  check('status_request answered', controls.some((c) => c.cmd === 'status'));

  socket.destroy();
} catch (e) {
  check(`smoke run completed (${e.message})`, false);
} finally {
  daemon.kill('SIGTERM');
  await sleep(200);
  daemon.kill('SIGKILL');
}

console.log(failures === 0 ? '\nINTEGRATION SMOKE OK' : `\nINTEGRATION SMOKE FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);

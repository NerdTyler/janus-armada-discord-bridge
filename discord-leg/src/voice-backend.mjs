// The Discord voice backend: the spec §3.5 thin interface
// over @discordjs/voice. Everything DAVE-shaped is isolated
// in this file — if the Phase-0 gate fails and the leg moves
// to dysnomia, this is the only file that changes.
//
// Interface:
//   join(channelId) / leave()
//   onUserPcm(userId, frame Int16Array 960) — decoded, downmixed
//   onSpeaking(userId, bool)
//   playPcm(frame Int16Array 960) — the mix from the bridge
//
// Receive path (spec §3.3): subscribe per user on speaking
// start with a generous end timeout; decoded PCM is reframed
// to exactly 960-sample mono frames before it leaves.

import {
  joinVoiceChannel,
  createAudioPlayer,
  createAudioResource,
  VoiceConnectionStatus,
  StreamType,
  EndBehaviorType,
} from '@discordjs/voice';
import prism from 'prism-media';
import { Readable } from 'node:stream';
import { FRAME_SAMPLES, downmixStereoToMono } from './protocol.mjs';

// One 20 ms frame of PCM silence (48 kHz stereo s16le:
// 960 samples × 2 ch × 2 B). The playback resource is
// StreamType.Raw, so its warm-up burst MUST be PCM bytes.
// This used to push the Opus silence packet (0xf8 0xff 0xfe)
// three times — 9 stray bytes that shifted every 3,840-byte
// block the prism Opus encoder sliced from the stream, so
// every sample was assembled from mismatched bytes and the
// bridged voice came out robotic/unintelligible on the
// Discord side. Convicted empirically 2026-11-22: a known
// signal through the deployed encoder correlated 0.066
// with the burst vs 1.000 without it.
const PCM_SILENCE_FRAME = Buffer.alloc(FRAME_SAMPLES * 2 * 2);

export class VoiceBackend {
  #connection = null;
  #player = null;
  #guild = null;
  #playStream = null;
  #idleTimer = null;
  #lastFrameAt = 0;
  #subscribed = new Set();
  #reframeCarry = new Map(); // userId -> Int16Array leftover

  /** @param {import('discord.js').Guild} guild */
  constructor(guild, { onUserPcm, onSpeaking, onConnectionLost }) {
    this.#guild = guild;
    this.onUserPcm = onUserPcm;
    this.onSpeaking = onSpeaking;
    this.onConnectionLost = onConnectionLost;
  }

  get channelId() {
    return this.#connection?.joinConfig?.channelId ?? null;
  }

  get inChannel() {
    return this.#connection !== null;
  }

  async join(channelId) {
    if (this.#connection) await this.leave();
    const connection = joinVoiceChannel({
      channelId,
      guildId: this.#guild.id,
      adapterCreator: this.#guild.voiceAdapterCreator,
      // LOAD-BEARING (spec §3.2): the bot must NOT deafen
      // itself. selfDeaf deafens the receive path too — a
      // self-deafened bot receives nobody's audio, silently.
      selfDeaf: false,
      selfMute: false,
    });
    this.#connection = connection;

    this.#player = createAudioPlayer();
    connection.subscribe(this.#player);
    // (The 3-frame silence boot burst rides the start of
    // every playback resource — see playPcm.)

    connection.receiver.speaking.on('start', (userId) => this.#subscribe(userId));
    connection.receiver.speaking.on('end', (userId) => this.onSpeaking?.(userId, false));
    connection.on(VoiceConnectionStatus.Disconnected, async () => {
      // Reconnect is the library's first move; if it gives up
      // (state -> Destroyed) the leg reports the loss upward.
      try {
        const { entersState } = await import('@discordjs/voice');
        await Promise.race([
          entersState(connection, VoiceConnectionStatus.Signalling, 5_000),
          entersState(connection, VoiceConnectionStatus.Connecting, 5_000),
        ]);
      } catch {
        this.onConnectionLost?.();
      }
    });
    connection.on(VoiceConnectionStatus.Destroyed, () => {
      if (this.#connection === connection) this.#connection = null;
    });
    return connection;
  }

  async leave() {
    this.#subscribed.clear();
    this.#reframeCarry.clear();
    if (this.#idleTimer) clearTimeout(this.#idleTimer);
    this.#playStream = null;
    try {
      this.#connection?.destroy();
    } catch {
      /* already gone */
    }
    this.#connection = null;
    this.#player = null;
  }

  #subscribe(userId) {
    this.onSpeaking?.(userId, true);
    if (this.#subscribed.has(userId) || !this.#connection) return;
    this.#subscribed.add(userId);
    const receiver = this.#connection.receiver;
    // The DAVE receive path decrypts BEFORE any event fires
    // (spec §3.3): subscribe to the DECODED stream, opus ->
    // 48 kHz stereo s16le via prism, then downmix + reframe.
    const opusStream = receiver.subscribe(userId, {
      end: { behavior: EndBehaviorType.Manual },
    });
    const decoder = new prism.opus.Decoder({ rate: 48000, channels: 2, frameSize: FRAME_SAMPLES });
    opusStream.pipe(decoder);
    decoder.on('data', (chunk) => {
      const stereo = new Int16Array(chunk.buffer, chunk.byteOffset, chunk.length / 2);
      const mono = downmixStereoToMono(stereo);
      this.#emitFramed(userId, mono);
    });
    const cleanup = () => {
      this.#subscribed.delete(userId);
      this.#reframeCarry.delete(userId);
    };
    opusStream.on('end', cleanup);
    opusStream.on('error', cleanup);
    decoder.on('error', cleanup);
  }

  /** Reframe an arbitrary-length mono stream into exact 960-sample frames. */
  #emitFramed(userId, mono) {
    const prev = this.#reframeCarry.get(userId) ?? new Int16Array(0);
    const joined = new Int16Array(prev.length + mono.length);
    joined.set(prev);
    joined.set(mono, prev.length);
    let off = 0;
    while (joined.length - off >= FRAME_SAMPLES) {
      this.onUserPcm?.(userId, joined.slice(off, off + FRAME_SAMPLES));
      off += FRAME_SAMPLES;
    }
    this.#reframeCarry.set(userId, joined.slice(off));
  }

  /**
   * Queue one 20 ms mono frame for playback. Playback runs on
   * an on-demand AudioResource: created when audio arrives
   * after an idle gap (prefixed with the 3-frame silence
   * burst), ended after >200 ms without frames — an idle
   * resource would stream silence indefinitely and hold the
   * DAVE send session hostage.
   */
  playPcm(frame) {
    if (!this.#player) return;
    this.#lastFrameAt = Date.now();
    const stereo = new Int16Array(frame.length * 2);
    for (let i = 0; i < frame.length; i++) {
      stereo[2 * i] = frame[i];
      stereo[2 * i + 1] = frame[i];
    }
    const buf = Buffer.from(stereo.buffer, stereo.byteOffset, stereo.byteLength);
    if (this.#playStream) {
      this.#playStream.push(buf);
      this.#armIdleEnd();
      return;
    }
    const stream = new Readable({ read() {} });
    stream.push(PCM_SILENCE_FRAME);
    stream.push(PCM_SILENCE_FRAME);
    stream.push(PCM_SILENCE_FRAME);
    stream.push(buf);
    this.#playStream = stream;
    const resource = createAudioResource(stream, { inputType: StreamType.Raw });
    this.#player.play(resource);
    this.#armIdleEnd();
  }

  /** End the current resource once frames stop for >200 ms. */
  #armIdleEnd() {
    if (this.#idleTimer) clearTimeout(this.#idleTimer);
    this.#idleTimer = setTimeout(() => {
      if (this.#playStream && Date.now() - this.#lastFrameAt > 200) {
        this.#playStream.push(null);
        this.#playStream = null;
      }
    }, 250);
    this.#idleTimer.unref?.();
  }
}

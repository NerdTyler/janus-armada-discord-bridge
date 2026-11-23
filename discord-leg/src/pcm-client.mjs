// The Discord leg's end of the PCM socket: connects to the
// Rust mixer process, frames outbound PCM/CONTROL, dispatches
// inbound MIX_OUT / SPEAK / CONTROL. Reconnects with backoff
// — the socket is internal plumbing; its loss must never take
// the leg down, and the Rust side tolerates the leg vanishing
// (spec §5.4).

import net from 'node:net';
import {
  StreamDecoder,
  FrameType,
  pcmFrame,
  controlFrame,
  speakFrame,
  framePcm,
} from './protocol.mjs';

export class PcmClient {
  #socket = null;
  #decoder = new StreamDecoder();
  #seq = new Map(); // userId string -> next seq
  #clockStart = null;
  #timer = null;
  #stopped = false;

  /**
   * @param {string} socketPath
   * @param {{onMixOut(frame Int16Array): void, onControl(obj): void, onSpeak(speakerId BigInt, start bool): void}} handlers
   */
  constructor(socketPath, handlers) {
    this.socketPath = socketPath;
    this.handlers = handlers;
  }

  get connected() {
    return this.#socket !== null && !this.#socket.destroyed;
  }

  start() {
    this.#stopped = false;
    this.#connect();
  }

  stop() {
    this.#stopped = true;
    if (this.#timer) clearTimeout(this.#timer);
    this.#socket?.destroy();
    this.#socket = null;
  }

  #connect() {
    if (this.#stopped) return;
    const sock = net.createConnection(this.socketPath);
    this.#socket = sock;
    sock.on('connect', () => {
      this.#clockStart = Date.now();
      this.sendControl({ cmd: 'hello', role: 'discord' });
    });
    sock.on('data', (chunk) => {
      this.#decoder.feed(chunk);
      let frame;
      while ((frame = this.#decoder.nextFrame()) !== null) {
        if (frame.kind === FrameType.MixOut) {
          this.handlers.onMixOut?.(framePcm(frame));
        } else if (frame.kind === FrameType.Control) {
          try {
            this.handlers.onControl?.(JSON.parse(frame.payload.toString('utf8')));
          } catch {
            /* malformed control JSON: drop */
          }
        } else if (frame.kind === FrameType.Speak) {
          this.handlers.onSpeak?.(frame.speakerId, (frame.flags & 1) === 1);
        }
      }
    });
    const retry = () => {
      if (this.#socket === sock) this.#socket = null;
      if (!this.#stopped) {
        this.#timer = setTimeout(() => this.#connect(), 1000);
        this.#timer.unref?.();
      }
    };
    sock.on('error', retry);
    sock.on('close', retry);
  }

  #write(buf) {
    if (this.connected) this.#socket.write(buf);
  }

  /** One user's decoded 20 ms frame, Discord -> mixer. */
  sendUserPcm(userId, samples) {
    const key = String(userId);
    const seq = (this.#seq.get(key) ?? 0) + 1;
    this.#seq.set(key, seq);
    const ts = this.#clockStart ? Date.now() - this.#clockStart : 0;
    this.#write(pcmFrame(FrameType.PcmDown, BigInt(userId), seq, ts, samples));
  }

  sendSpeak(userId, start) {
    this.#write(speakFrame(BigInt(userId), start));
  }

  sendControl(obj) {
    this.#write(controlFrame(obj));
  }
}

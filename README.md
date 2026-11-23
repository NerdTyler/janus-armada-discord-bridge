# Janus — a Discord ↔ Armada voice bridge

Janus (the two-faced Roman god of doorways, one face in each world) is a
two-way voice bridge between a **Discord** voice channel and an
**Armada** voice room. Armada is a Nostr-based chat platform whose voice
channels run on a LiveKit SFU with end-to-end-encrypted frames; Discord
is… Discord. Janus appears as one named participant on each side —
"Janus" in both — and carries everyone's voice across, so people in a
Discord call and people in an Armada room talk to each other like
they're in the same call.

Pure audio transport: **no LLM, no transcription, no recording.** Janus
joins voice only when summoned and leaves when the rooms empty.

Wire protocol: [`shared/pcm-protocol.md`](shared/pcm-protocol.md).

## Features

- **Conversational summon/leave.** Talk to Janus like a person:
  "janus join", "janus join VC 2", "have janus bridge to General",
  "janus leave". The matcher is deliberately tight — a message must name
  Janus, carry a clear join/leave intent, and not be a question — so
  ordinary chat never triggers it. Slash/bang commands stay as
  backstops: `/bridge summon` / `/bridge leave` on Discord,
  `!summon` / `!leave` on Armada.
- **Presence auto-join (Discord side, opt-in).** When a human joins a
  Discord voice channel while Janus is idle, Janus starts a session on
  his own: he joins that channel and lands on Armada by the usual
  resolution. (`DISCORD_AUTOJOIN=on`; no extra portal intent needed.)
- **Stay-while-occupied rule.** Janus never leaves just because the
  person who summoned him left. A session ends on its own only when
  **no humans remain in either room** — bots don't count (configurable
  via `ARMADA_BOT_NPUBS`), and a side whose occupancy can't be
  determined counts as occupied, so missing data never drops a live
  bridge.
- **Music-bot friendly.** Because bots don't count as occupants, a
  companion bot (e.g. a music bot) can auto-summon Janus when its music
  starts — post `janus join <channel>` from the bot — and the bridge
  stays up for the humans listening, not for the bot.
- **Per-direction gain controls + soft limiter.** The bridge has no
  AGC: a raw mic feed arrives hotter than Discord's processed natives,
  and summed sources flat-top. `BRIDGE_GAIN_TO_DISCORD_DB` /
  `BRIDGE_GAIN_TO_ARMADA_DB` set master gains per direction, and the
  mixer finishes through a tanh soft limiter (≈ −1 dBFS ceiling)
  instead of a hard clamp.
- **Optional session announcements.** With `JANUS_ANNOUNCE=on`, Janus
  posts a short note in both platforms' text channels when a session
  starts/stops (worth doing: while the bridge is up, audio leaves both
  platforms' end-to-end encryption, and people should know). Default
  off — sessions run silently, with the full record in the journal.
- **Open or whitelist summoning.** `SUMMON_ACCESS=open` lets anyone in
  the guild/community summon; `whitelist` restricts it to the owner
  plus friend lists. Status commands and Armada invite auto-accept are
  owner-only in both modes.

## How a summon lands

The far side is resolved, never pinned: **(1)** a channel named in the
summon, else **(2)** the first voice channel currently occupied, else
**(3)** the configured default (`DISCORD_HOME_VOICE_CHANNEL_ID` /
`ARMADA_VOICE_CHANNEL_ID`), else **(4)** any voice channel there is.
The bridge can join *any* voice channel in the guild/community — the
defaults are only fallbacks.

## Architecture

```
Discord voice ⇄ [Node leg: @discordjs/voice + davey] ──PCM socket── [Rust: mixer + Armada leg] ⇄ Armada voice (LiveKit SFU)
```

- `armada-leg/` — the `janus-armada` Rust daemon: PCM socket server,
  the mixer, the session state machine, Armada chat triggers, landing
  resolution, presence watching.
- `discord-leg/` — Node 22, discord.js 14, **@discordjs/voice 0.19.2**
  (pinned) with **@snazzah/davey** for Discord's DAVE voice E2EE. A
  thin adapter behind `src/voice-backend.mjs`; everything DAVE-shaped
  lives there.
- `crates/pcm-protocol/` — the socket framing + mixer core in pure
  Rust, mirrored byte-for-byte in Node (`discord-leg/src/protocol.mjs`),
  pinned by a shared golden-bytes test on both sides.
- `crates/concord-voice/` — the Armada voice transport: CORD-07 broker
  grant flow, kind-23313 presence heartbeat, E2EE frame crypto, and a
  WebRTC room including an RFC 4571 ICE-TCP bridge (without it, UDP
  checks never validate against this SFU). Adapted from the live-proven
  LiveKit module of the author's earlier Armada bot.
- `spike/` — a Phase-0 DAVE delivery meter: measures per-user Opus
  packet delivery on the Discord stack, idle and under join/leave
  churn. Useful if you ever doubt the Discord leg's receive path.
- `deploy/` — systemd `--user` units for both legs.

Audio contract: 48 kHz mono s16le, 20 ms frames everywhere, one mixer
tick, mix-minus by construction (you never hear your own platform
back), drop-oldest 200 ms queues, silence is never streamed.

## Requirements

- A Linux host that stays on (the bridge is a resident daemon; it only
  joins voice when summoned).
- **Rust toolchain** (stable; builds the workspace — `cargo`).
- **Node 22+** and npm (Discord leg).
- Native-build basics for Node modules if prebuilds don't cover your
  platform: a C/C++ toolchain + Python 3 (`@discordjs/opus`,
  `@snazzah/davey`), and **ffmpeg** in PATH (prism-media).
- A **Discord bot application** (developer portal): a bot user invited
  to your guild with the Guilds + GuildVoiceStates intents and Connect /
  Speak / Use Voice Activity / View Channel / Send Messages permissions
  (permissions integer `36703232` covers it). For conversational
  triggers on Discord you must also enable the **MESSAGE_CONTENT
  privileged intent** (Bot → Privileged Gateway Intents) and set
  `DISCORD_CONVERSATIONAL=on`; without it the Discord leg is
  slash-command only. The Armada side is conversational with no portal
  equivalent.
- An **Armada community** you can invite a member to, and a **fresh
  Nostr keypair** for the bridge itself (don't reuse your personal
  key). You invite the bridge's npub with a kind-3313 invite from your
  Armada client; the daemon auto-accepts invites from the owner's npub
  only.

## Setup

1. **Create the Discord bot** and invite it to your guild (see
   Requirements). Copy its token.
2. **Mint the bridge's Nostr keypair** and invite its npub to your
   Armada community (kind-3313 invite).
3. **Configure:**
   ```sh
   mkdir -p ~/.config/janus
   cp bridge.env.example ~/.config/janus/janus.env
   chmod 600 ~/.config/janus/janus.env
   # fill in your values — see the config table below
   ```
4. **Build the Rust leg:**
   ```sh
   cargo build --release
   cp target/release/janus-armada ~/.local/bin/
   ```
5. **Install the Discord leg's deps:**
   ```sh
   cd discord-leg && npm ci
   ```
6. **Install and start the units:**
   ```sh
   cp deploy/janus-armada.service deploy/janus-discord.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now janus-armada janus-discord
   # optional, so the bridge survives logout/reboot without a session:
   loginctl enable-linger "$USER"
   ```
7. **Summon it:** join a voice channel on either side and say
   "janus join" in a text channel (Armada), use `/bridge summon`
   (Discord), or enable the conversational/auto-join options.

Logs: `journalctl --user -u janus-armada -u janus-discord -f`
(Rust leg verbosity via `RUST_LOG`, e.g. `RUST_LOG=debug`).

## Configuration

All configuration is one env file (`~/.config/janus/janus.env`), shared
by both legs. `bridge.env.example` is the annotated template.

| Variable | Meaning |
|---|---|
| `BRIDGE_SOCKET_PATH` | Unix socket between the legs; the Rust leg owns it, the Discord leg connects. |
| `BRIDGE_DATA_DIR` | Armada identity/SDK state directory. |
| `BRIDGE_NAME_DISCORD` / `BRIDGE_NAME_ARMADA` | The bridge's display name on each platform. |
| `BRIDGE_BROKER` | Armada voice broker (SFU front door). Optional; defaults to `https://av.armada.buzz`. |
| `SUMMON_ACCESS` | `whitelist` (owner + friend lists) or `open` (anyone in the guild/community). Unset/invalid = whitelist. |
| `SPEAKING_RELAY` | Speaking-indicator relay across platforms. Off in v1. |
| `JANUS_ANNOUNCE` | `on` = post session start/stop notes in both text channels. Anything else = silent. |
| `DISCORD_TOKEN` | Discord bot token. Secret — env file only. |
| `DISCORD_GUILD_ID` | The guild (server) the bridge serves. |
| `DISCORD_HOME_VOICE_CHANNEL_ID` | Default Discord landing channel (fallback only, never a pin). |
| `DISCORD_TEXT_CHANNEL_ID` | Text channel for announcements (if enabled). |
| `DISCORD_OWNER_USER_ID` | Owner's Discord user id: always whitelisted; sole audience for `/bridge status`. |
| `DISCORD_CONVERSATIONAL` | `on` = conversational triggers on Discord (needs the MESSAGE_CONTENT intent too). |
| `DISCORD_AUTOJOIN` | `on` = a human joining a Discord voice channel starts a session automatically. |
| `DISCORD_SUMMON_USER_IDS` | Whitelist mode: extra Discord user ids allowed to summon/leave. Comma-separated. |
| `BRIDGE_GAIN_TO_DISCORD_DB` | Master gain toward Discord, dB (default −6, clamps to [−60, +12]). |
| `BRIDGE_GAIN_TO_ARMADA_DB` | Master gain toward Armada, dB (default −3, clamps to [−60, +12]). |
| `ARMADA_BRIDGE_NSEC` | The bridge's own Nostr secret key. Secret — env file only. |
| `ARMADA_VOICE_CHANNEL_ID` | Default Armada landing channel, hex id (fallback only). |
| `ARMADA_TEXT_CHANNEL_ID` | Armada text channel for announcements; empty = the community's "general-chat". |
| `ARMADA_OWNER_NPUB` | Owner's Armada npub: always whitelisted; sole audience for `!bridge status`; the only inviter auto-accepted. |
| `ARMADA_SUMMON_NPUBS` | Whitelist mode: extra Armada npubs allowed to summon/leave. Comma-separated. |
| `ARMADA_BOT_NPUBS` | Other bots that never count as occupants for the stay rule (e.g. a music bot). Comma-separated npubs. |

The `spike/` tool has its own `SPIKE_DISCORD_TOKEN`, `SPIKE_GUILD_ID`,
`SPIKE_CHANNEL_ID`, and `SPIKE_RUN_MS` variables — see `spike/README.md`.

## Tests

```sh
cargo test                 # Rust: protocol, mixer, resolution, auth, triggers
cd discord-leg && npm test # Node: protocol mirror, resolution, auth, triggers
```

## Status

v0.1.0 — this exact code runs live in the reference deployment,
bridging daily-driver voice between the author's Discord server and
Armada community. The Discord receive path (DAVE) and the Armada
transport are both exercised in production; the `spike/` meter remains
if you want to re-measure delivery on your own setup.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Tyler.

# Janus PCM socket protocol

The two bridge legs talk over one Unix stream socket owned by the Rust
process (`janus-armada`); the Node Discord leg connects to it. This
document is the wire contract. Two implementations exist and must stay
byte-identical: `crates/pcm-protocol` (Rust) and
`discord-leg/src/protocol.mjs` (Node), pinned by a shared golden-bytes
test on both sides.

## Framing

Each frame on the stream:

```
u32 LE  length            -- header + payload bytes that follow
u8[24]  header
u8[]    payload           -- length - 24 bytes
```

Header layout (24 bytes):

| Offset | Field        | Type    | Notes                                  |
|-------:|--------------|---------|----------------------------------------|
| 0      | magic        | u8      | `'B'` (0x42)                           |
| 1      | version      | u8      | `1`                                    |
| 2      | type         | u8      | see below                              |
| 3      | flags        | u8      | SPEAK: bit0 = start                    |
| 4      | speaker_id   | u64 LE  | see below                              |
| 12     | seq          | u32 LE  | per-sender sequence; gaps = loss       |
| 16     | timestamp_ms | u32 LE  | sender's frame clock                   |
| 20     | samples      | u16 LE  | sample count for PCM frames (960)      |
| 22     | energy       | u8      | RMS bucket 0–255                       |
| 23     | reserved     | u8      | 0                                      |

> **Deviation from the design spec, recorded:** bridge spec §5.3 calls
> the header "a fixed 16-byte header" but its own field list (magic,
> version, type, flags, speaker u64, seq u32, timestamp u32, samples
> u16, energy u8) totals 23 bytes minimum. This protocol implements
> the field list at 24 bytes (with one reserved byte). The field list
> is what both implementations and the mixer actually need; the
> "16-byte" figure could not be satisfied simultaneously.

Frame types:

| Type | Name    | Direction         | Payload                                   |
|-----:|---------|-------------------|-------------------------------------------|
| 1    | PCM_DOWN| Discord → mixer   | one 20 ms frame, per-user                 |
| 2    | PCM_UP  | Armada → mixer    | one 20 ms frame, per-participant          |
|      |         |                   | (internal to the Rust process in v1 —    |
|      |         |                   | the Armada leg feeds the mixer in-process;|
|      |         |                   | the type is reserved for a future split) |
| 3    | MIX_OUT | mixer → a leg     | one 20 ms mixed frame (speaker_id = 0)    |
| 4    | SPEAK   | either            | empty; flags bit0 = speaking start        |
| 5    | CONTROL | either            | UTF-8 JSON (see below)                    |

## PCM format

48 kHz mono s16le, exactly one 20 ms frame per PCM payload:
960 samples = 1,920 bytes. There is one clock in the system — the
mixer's 20 ms tick — and neither leg streams silence: a direction
with no active input produces no frames.

`speaker_id`: for Discord users, the Discord user id (snowflake,
u64). For Armada participants, FNV-1a-64 of the SFU identity
string. For MIX_OUT, 0.

## Backpressure

None, by design. Every queue is a bounded drop-oldest ring
(10 frames = 200 ms). A frame that would be more than 200 ms late
is dropped and counted; the counters surface in `status`.

## Decoder resync

The stream has no other framing to recover to, so on a protocol
violation (length outside 24..=65536, bad magic/version/type) a
decoder drops ONE byte and rescans. The header is validated as
soon as 28 bytes are buffered — before waiting for a full frame —
so a garbage prefix that fakes an in-range length cannot stall
the stream.

## CONTROL messages (JSON)

Discord leg → Rust:

- `{"cmd":"hello","role":"discord"}` — on connect.
- `{"cmd":"session_start","origin":"discord","discord_channel_id":…,"discord_channel_name":…,"armada_named":…|null}` — an authorized member summoned from Discord (anyone, in open mode); the leg has already joined the summoner's channel. Rust resolves + joins the Armada side, then answers `session_started` or `session_failed`.
- `{"cmd":"landing","id":N,"channel_id":…,"channel_name":…,"via":…}` or `{"cmd":"landing","id":N,"error":…}` — answer to `resolve_landing`.
- `{"cmd":"session_started","origin":"armada"}` — confirms an Armada-origin `session_start`.
- `{"cmd":"session_failed","error":…}` — the Discord side could not join.
- `{"cmd":"session_ended","reason":…}` — Discord side ended the session (leave command, summon-side call emptied, connection lost).
- `{"cmd":"status_request"}` — answer with a status push.

Rust → Discord leg:

- `{"cmd":"status","state":"idle|joining|live","origin":…,"armada_channel_name":…,"discord_channel_name":…,"discord_connected":bool}` — pushed on every change and on request.
- `{"cmd":"resolve_landing","id":N,"named":…|null}` — Armada-origin summon: resolve where the Discord leg lands (named → the one occupied channel → configured default).
- `{"cmd":"session_start","origin":"armada","armada_channel_name":…,"discord_channel_id":…}` — join this channel; answer `session_started` / `session_failed`.
- `{"cmd":"session_started","origin":"discord","armada_channel_name":…}` — both legs are in voice; announce on Discord.
- `{"cmd":"session_failed","error":…}` — the Armada side could not join; leave and stand down.
- `{"cmd":"session_ended","reason":…}` — Armada side ended the session (leave command, summon-side call emptied, leg failure, Discord-leg disconnect is implied).

## Session shape

The daemon is resident; voice participation is summoned only
. Summon authorization follows
`SUMMON_ACCESS`: the owner's deployment is open (anyone may
summon); whitelist mode restricts summons to the owner plus a
whitelist of members he controls (per-platform lists in
`janus.env`). The summon side joins the channel the summoner
is in; the far side resolves by the landing rules. Leave is an
explicit command from either side, or the summon-side call
emptying. Consent announcements fire at join time in both
platforms' text channels.

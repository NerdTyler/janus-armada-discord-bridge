# Phase-0 spike: does @discordjs/voice + davey actually deliver?

The bridge's decisive risk (spec §11, upstream issue #11419):
Discord's DAVE end-to-end encryption for voice shipped in
2025–2026, and the open-source receive path
(@discordjs/voice 0.19.2 + @snazzah/davey) has a reported
failure mode where a large share of a speaker's packets never
emerge from decryption — especially around MLS group changes,
i.e. members joining and leaving mid-call. If that reproduces
here, the Discord leg falls back to dysnomia per spec. This
spike exists to answer the question with numbers BEFORE the
bridge depends on the stack.

## The instrument

`src/delivery-meter.mjs` — pure measurement core. Expected
packet counts come from RTP timestamp spans (48 kHz, 960
samples per 20 ms packet); received counts are deduplicated
by timestamp. Verified offline:

```bash
npm test          # unit tests incl. the #11419 loss shape
npm run selftest  # synthetic streams -> gate verdicts
```

`src/spike.mjs` — the live harness. Two-sided measurement so
DAVE loss is separated from network loss:

- **Wire side**: wraps the receiver's `onUdpMessage` to
  observe raw RTP headers (timestamp + SSRC → user) before
  the library processes them.
- **Emerged side**: counts packets leaving
  `receiver.subscribe(userId)` — what survived transport
  decrypt AND `daveSession.decrypt`. Stream errors count as
  decrypt failures.

Gate: **DAVE delivery (emerged / wire-received) AND
end-to-end delivery (emerged / wire-expected) both ≥ 98%.**

## Running it (needs the owner's bot)

```bash
SPIKE_DISCORD_TOKEN=... SPIKE_GUILD_ID=... SPIKE_CHANNEL_ID=... npm run spike
```

Without the env vars the harness prints exactly what is
missing and exits 2.

Protocol (~10 min, `SPIKE_RUN_MS` default):

1. **Idle phase (5 min):** the owner joins the channel and talks
   (or plays audio into his mic) continuously.
2. **Churn phase (5 min):** while the owner keeps talking, 2–3
   other accounts join and leave every ~20 s — this is what
   exercises MLS rekeys.

The harness prints per-user wire/emerged figures each minute
and a final PASS/FAIL verdict. PASS clears the Node+davey
stack for the bridge; FAIL triggers the dysnomia fallback.

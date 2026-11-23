# Deploying Janus

The full walkthrough lives in the top-level [README](../README.md)
(requirements, Discord bot creation, configuration table). This file is
the short version for the host install.

## Prerequisites (owner steps, in order)

1. **Discord bot application** (developer portal): create the
   application (name: Janus), add a bot, copy the token into your
   `janus.env` — never share it. Invite the bot to your guild with
   intents **129** (Guilds + GuildVoiceStates) and permissions integer
   **36703232** (Connect, Speak, Use VAD, View Channel, Send Messages —
   server-wide, so the bridge can join ANY voice channel). OPTIONAL,
   for Discord-side conversational triggers ("janus join" in a text
   channel): also enable the **Message Content Intent** (Bot →
   Privileged Gateway Intents) and set `DISCORD_CONVERSATIONAL=on` in
   `janus.env`. The Armada side is conversational with no portal step;
   without this toggle the Discord leg stays slash-command only.
2. **Armada invite** (in your Armada client): generate a kind-3313
   community invite for the bridge identity. The bridge's nsec goes
   into `janus.env`; the daemon auto-accepts invites from the owner's
   npub only.
3. Fill `~/.config/janus/janus.env` (from `bridge.env.example`,
   `chmod 600`): token, guild id, your Discord user id, a default
   Discord voice channel (fallback only), text channel ids for
   announcements, Armada defaults. Summon access: start with
   `SUMMON_ACCESS=whitelist` (owner + friend lists); `open` lets
   ANYONE in the guild/community summon and leave. Invite auto-accept
   and status commands stay owner-only in both modes.

## Install (on the host)

```sh
# Rust leg: build and stage the binary.
cargo build --release
cp target/release/janus-armada ~/.local/bin/

# Discord leg: install dependencies.
(cd discord-leg && npm ci)

# Units: adjust paths inside if your node or checkout lives elsewhere.
cp deploy/janus-armada.service deploy/janus-discord.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now janus-armada janus-discord
loginctl enable-linger "$USER"   # survive logout / run at boot

journalctl --user -u janus-armada -u janus-discord -f
```

Node 22+ is required for the Discord leg; `ExecStart` in
`janus-discord.service` must be the absolute path to it.

## Optional: the DAVE delivery spike

Before trusting the Discord leg on a new setup you can run the Phase-0
delivery meter in `spike/` (see `spike/README.md`): one run, ~10
minutes, measures per-user Opus packet delivery idle and under
join/leave churn.

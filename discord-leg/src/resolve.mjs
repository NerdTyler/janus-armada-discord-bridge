// Far-side landing resolution : when a
// summon originates on the OTHER platform, where does the
// Discord leg land?
//   (1) the channel named in the summon command, if given;
//   (2) else the first voice channel that is currently
//       occupied (one occupied is the normal case; if
//       several are occupied we still land — the owner's rule:
//       "if im not in a channel, he just joins any of
//       them" — a landing must never fail over ambiguity);
//   (3) else the configured default channel;
//   (4) else the first voice channel there is ("any").
// The ONLY failures left: a named channel that does not
// exist, and a server with no voice channels at all. In
// particular the summoner's own Discord presence is NEVER
// an input to this path — an Armada-origin summon lands
// here whether or not the summoner is in a Discord
// channel. The bot can join ANY voice channel in the
// guild; nothing here pins it to a home channel. This
// mirrors the Rust `resolve_landing` in
// armada-leg/src/resolve.rs — the two implementations
// are kept in lockstep and share test cases.
//
// Pure functions over plain data so they unit-test without a
// Discord connection:
//   channels: [{ id, name, occupants: [userId, ...] }]
//   (occupants excludes bots — a channel holding only the
//   bridge itself or music bots is not "occupied")

export function resolveLanding({ named = null, channels = [], defaultChannelId = null, excludeUserIds = new Set() }) {
  if (named) {
    const byId = channels.find((c) => c.id === named);
    if (byId) return { channelId: byId.id, via: 'named' };
    const byName = channels.find((c) => c.name.toLowerCase() === String(named).toLowerCase());
    if (byName) return { channelId: byName.id, via: 'named' };
    return { error: `no voice channel matches "${named}"` };
  }
  // (2) First occupied channel wins — one occupied is the
  // normal case, and several occupied is NOT an error
  // (the owner: "he just joins any of them").
  const occupied = channels.find((c) => c.occupants.some((u) => !excludeUserIds.has(u)));
  if (occupied) return { channelId: occupied.id, via: 'occupied' };
  // (3) The configured default, when it still exists.
  if (defaultChannelId) {
    const def = channels.find((c) => c.id === defaultChannelId);
    if (def) return { channelId: def.id, via: 'default' };
  }
  // (4) Any channel at all — the summoner not being in a
  // Discord channel (and a stale/missing default) must
  // never block the landing.
  if (channels.length > 0) return { channelId: channels[0].id, via: 'any' };
  return { error: 'there are no voice channels to land in' };
}

/** Snapshot a discord.js guild's voice channels into the plain shape above. */
export function snapshotChannels(guild, botUserId) {
  const out = [];
  for (const channel of guild.channels.cache.values()) {
    if (channel.type !== 2) continue; // ChannelType.GuildVoice
    const occupants = [];
    for (const [userId] of channel.members) {
      if (userId !== botUserId) occupants.push(userId);
    }
    out.push({ id: channel.id, name: channel.name, occupants });
  }
  return out;
}

/** The voice channel a user is currently in, or null. */
export function userVoiceChannelId(guild, userId) {
  const member = guild.members.cache.get(userId);
  return member?.voice?.channelId ?? null;
}

/**
 * How many HUMANS a member list holds (the owner's rule,
 * 2026-11-22: bots never count as "someone" for the
 * bridge's staying/leaving decision — the bridge itself
 * included). Plain shape so it unit-tests: [{ bot }].
 */
export function countHumans(members) {
  return members.filter((m) => !m.bot).length;
}

/**
 * Parse DISCORD_AUTOJOIN: only the exact value "on"
 * (case-insensitive, whitespace tolerated) enables
 * presence auto-join — the DISCORD_CONVERSATIONAL pattern.
 */
export function parseAutoJoinEnabled(raw) {
  if (typeof raw !== 'string') return false;
  return raw.trim().toLowerCase() === 'on';
}

/**
 * Parse JANUS_ANNOUNCE: only the exact value "on"
 * (case-insensitive, whitespace tolerated) enables the
 * chat-visible session start/stop announcements. Anything
 * else — unset, empty, "off", invalid — is silent: the owner's
 * order (2026-11-22) is that Janus stops announcing every
 * session in chat; the journal keeps the full record. The
 * Rust leg parses the same variable the same way, so one
 * env line silences both sides.
 */
export function parseAnnounceEnabled(raw) {
  if (typeof raw !== 'string') return false;
  return raw.trim().toLowerCase() === 'on';
}

/**
 * Presence auto-join decision ("when
 * janus sees someone join a discord call it also joins an
 * armada call"). Pure over plain data. Fires only when
 * ALL hold:
 * - the feature is enabled (DISCORD_AUTOJOIN=on),
 * - no bridge session is active (first join wins; later
 *   joins are just people entering the bridged room),
 * - the mover is human (bots — Janus himself included —
 *   never trigger it),
 * - the mover LANDED in a channel (newChannelId set —
 *   a departure is not a join), and
 * - the channel actually changed: a mute/deafen/server
 *   state update in the same channel is not a join,
 *   while a channel switch while Janus is idle counts
 *   as a join to the destination.
 */
export function shouldAutoJoin({ enabled, sessionActive, isBot, oldChannelId, newChannelId }) {
  if (!enabled || sessionActive || isBot) return false;
  if (!newChannelId) return false;
  return oldChannelId !== newChannelId;
}

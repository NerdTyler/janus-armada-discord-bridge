// Janus — Discord leg daemon.
//
// Resident process (janus-discord.service). Holds
// NO voice connection until summoned (the owner's model,
// 2026-11-22): a summon joins the voice channel the SUMMONER
// is in on the summoning side, resolves the far side's landing
// (named -> the one occupied channel -> configured default),
// and bridges until an explicit leave or until NO HUMANS
// remain in either channel of the session (the owner's rule,
// 2026-11-22 — the Rust side owns that decision and polls
// this leg's human count over the control channel).
//
// Summon/leave authorization follows SUMMON_ACCESS (the owner's
// orders): "open" — ANYONE may summon/leave
// (the reference deployment ships open) — or
// "whitelist": the owner (DISCORD_OWNER_USER_ID, always included)
// plus friends in DISCORD_SUMMON_USER_IDS. See auth.mjs.
// /bridge status keeps its historical owner-only check in
// BOTH modes.
//
// This process owns: the Discord gateway + voice connection,
// summon-gated slash commands (plus conversational triggers
// when DISCORD_CONVERSATIONAL=on — see converse.mjs), the
// Discord-side consent announcement. The Rust process owns:
// the mixer, the Armada leg, the Armada-side announcement.
// They coordinate over the PCM socket's CONTROL frames.

import { Client, GatewayIntentBits, REST, Routes, SlashCommandBuilder } from 'discord.js';
import { VoiceBackend } from './voice-backend.mjs';
import { PcmClient } from './pcm-client.mjs';
import { resolveLanding, snapshotChannels, userVoiceChannelId, countHumans, parseAutoJoinEnabled, parseAnnounceEnabled, shouldAutoJoin } from './resolve.mjs';
import { SummonAccess, parseSummonAccess, buildSummonWhitelist, isSummonAuthorized } from './auth.mjs';
import { parseConversational, parseConversationalEnabled } from './converse.mjs';

const env = (k, d = null) => process.env[k] ?? d;

const config = {
  token: env('DISCORD_TOKEN'),
  guildId: env('DISCORD_GUILD_ID'),
  // Fallback default for far-side landing step (3) — NOT a pin.
  defaultVoiceChannelId: env('DISCORD_HOME_VOICE_CHANNEL_ID'),
  textChannelId: env('DISCORD_TEXT_CHANNEL_ID'),
  ownerUserId: env('DISCORD_OWNER_USER_ID'),
  socketPath: env('BRIDGE_SOCKET_PATH', '/run/user/1000/janus/pcm.sock'),
  bridgeName: env('BRIDGE_NAME_DISCORD', 'Janus'),
  // Conversational triggers ("janus join" in a text
  // channel): opt-in, because reading message text needs
  // the MESSAGE_CONTENT privileged intent in the portal.
  conversational: parseConversationalEnabled(env('DISCORD_CONVERSATIONAL')),
  // Presence auto-join : a human
  // joining a voice channel while Janus is idle pulls
  // him into a session automatically. Same opt-in shape
  // as conversational; needs no portal intent (voice
  // states are already in the base intent set).
  autojoin: parseAutoJoinEnabled(env('DISCORD_AUTOJOIN')),
  // Chat-visible session start/stop announcements
  // (JANUS_ANNOUNCE): the owner's order (2026-11-22) — Janus
  // stops announcing every session. Only an explicit "on"
  // enables them; the deployment sets JANUS_ANNOUNCE=off.
  // The journal keeps the full session record either way.
  announce: parseAnnounceEnabled(env('JANUS_ANNOUNCE')),
};

// Summon access mode: SUMMON_ACCESS=open lets ANYONE
// summon/leave (the owner's deployment). Anything else — unset,
// empty, invalid — falls back to whitelist mode, where the
// whitelist below decides.
const summonAccess = parseSummonAccess(env('SUMMON_ACCESS'));

// Summon whitelist (whitelist mode only): the owner is ALWAYS in
// the union, so old configs (no DISCORD_SUMMON_USER_IDS) keep
// working as owner-only and the owner can never lock himself out.
const summonWhitelist = buildSummonWhitelist(config.ownerUserId, env('DISCORD_SUMMON_USER_IDS'));

const missing = ['token', 'guildId', 'ownerUserId'].filter((k) => !config[k]);
if (missing.length > 0) {
  console.error(`[janus-discord] missing required config: ${missing.join(', ')} (see bridge.env.example)`);
  console.error('[janus-discord] the Discord bot token is an operator input (developer portal — keep it secret). Exiting.');
  process.exit(78); // EX_CONFIG — do not crash-loop under systemd
}

const client = new Client({
  // Guilds (1) + GuildVoiceStates (128) = 129. Only when
  // conversational triggers are enabled
  // (DISCORD_CONVERSATIONAL=on) do we add GuildMessages +
  // MESSAGE_CONTENT — without that portal-side privileged
  // intent, message events would arrive with empty text.
  intents:
    GatewayIntentBits.Guilds |
    GatewayIntentBits.GuildVoiceStates |
    (config.conversational ? GatewayIntentBits.GuildMessages | GatewayIntentBits.MessageContent : 0),
});

let backend = null;
let guild = null;
let session = null; // { origin: 'discord'|'armada', discordChannelId, armadaChannelName }
let sessionStarting = false; // a summon (auto-join) is between "decided" and "session set"
let rustState = { state: 'unknown' };

const pcm = new PcmClient(config.socketPath, {
  onMixOut: (frame) => backend?.playPcm(frame),
  onControl: (msg) => handleControl(msg).catch((e) => console.error('[janus-discord] control error:', e)),
  onSpeak: () => {
    /* SPEAKING_RELAY is off in v1 (spec §7); edges are ignored. */
  },
});

async function announce(text) {
  if (!config.announce) {
    console.log('[janus-discord] session announcement suppressed (JANUS_ANNOUNCE is not "on")');
    return;
  }
  if (!config.textChannelId) return;
  try {
    const ch = await client.channels.fetch(config.textChannelId);
    if (ch?.isTextBased()) await ch.send(text);
  } catch (e) {
    console.error('[janus-discord] announcement failed:', e.message);
  }
}

async function announceStart() {
  if (!session) return;
  const chName = guild.channels.cache.get(session.discordChannelId)?.name ?? 'voice';
  await announce(
    `🔊 **${config.bridgeName} is live** — this server's **${chName}** voice channel is now bridged with Armada voice` +
      (session.armadaChannelName ? ` (**${session.armadaChannelName}**)` : '') +
      `. While the bridge is up, audio leaves both platforms' end-to-end encryption.`,
  );
}

async function startSession({ origin, discordChannelId, armadaChannelName }) {
  await backend.join(discordChannelId);
  session = { origin, discordChannelId, armadaChannelName: armadaChannelName ?? null };
  await announceStart();
}

async function endSession(reason, { notifyRust = true } = {}) {
  if (!session) return;
  session = null;
  await backend.leave();
  if (notifyRust) pcm.sendControl({ cmd: 'session_ended', reason });
  await announce(`🔇 **${config.bridgeName} left** — the voice bridge is closed (${reason}). This channel is private again.`);
}

async function handleControl(msg) {
  switch (msg.cmd) {
    case 'status':
      rustState = msg;
      break;
    case 'resolve_landing': {
      // Summon originated on the Armada side: resolve where
      // the Discord leg lands.
      const channels = snapshotChannels(guild, client.user.id);
      const r = resolveLanding({
        named: msg.named ?? null,
        channels,
        defaultChannelId: config.defaultVoiceChannelId,
        excludeUserIds: new Set([client.user.id]),
      });
      if (r.error) {
        pcm.sendControl({ cmd: 'landing', id: msg.id, error: r.error });
      } else {
        const name = guild.channels.cache.get(r.channelId)?.name ?? r.channelId;
        pcm.sendControl({ cmd: 'landing', id: msg.id, channel_id: r.channelId, channel_name: name, via: r.via });
      }
      break;
    }
    case 'occupancy': {
      // The Rust session-presence watcher asks: how many
      // HUMANS are in the Discord channel we're bridging?
      // Bots (this bridge included) never count (the owner's
      // rule). No session -> 0. Rust owns the
      // stay/leave decision; this leg only reports.
      let humans = 0;
      if (session) {
        const ch = guild?.channels.cache.get(session.discordChannelId);
        if (ch) humans = countHumans([...ch.members.values()].map((m) => ({ bot: m.user.bot })));
      }
      pcm.sendControl({ cmd: 'occupancy', id: msg.id, humans });
      break;
    }
    case 'session_start': {
      // Armada-origin summon: Rust resolved our landing and
      // is joining its side; join ours and confirm.
      try {
        await startSession({ origin: 'armada', discordChannelId: msg.discord_channel_id, armadaChannelName: msg.armada_channel_name });
        pcm.sendControl({ cmd: 'session_started', origin: 'armada' });
      } catch (e) {
        pcm.sendControl({ cmd: 'session_failed', error: String(e?.message ?? e) });
      }
      break;
    }
    case 'session_started':
      // Discord-origin summon confirmed by the Rust side:
      // both legs are in voice now — announce.
      if (session) {
        session.armadaChannelName = msg.armada_channel_name ?? session.armadaChannelName;
        await announceStart();
      }
      break;
    case 'session_ended':
      await endSession(msg.reason ?? 'ended from the Armada side', { notifyRust: false });
      break;
    case 'session_failed':
      console.error('[janus-discord] session failed on the Rust side:', msg.error);
      await endSession('the Armada side could not join', { notifyRust: false });
      break;
  }
}

/**
 * Discord-origin summon core, shared by /bridge summon and
 * the conversational trigger: join the SUMMONER's voice
 * channel (whoever summoned — in open mode, anyone), then
 * hand off to the Rust side. Returns { channelId } on
 * success or { error } with a user-facing reason.
 */
async function beginDiscordSummon(userId, named) {
  if (session) return { error: 'Janus is already in a session. `/bridge leave` first.' };
  const channelId = userVoiceChannelId(guild, userId);
  if (!channelId) return { error: 'You are not in a voice channel — join one, then summon.' };
  try {
    await backend.join(channelId);
  } catch (e) {
    return { error: `Could not join: ${e.message}` };
  }
  session = { origin: 'discord', discordChannelId: channelId, armadaChannelName: null };
  pcm.sendControl({
    cmd: 'session_start',
    origin: 'discord',
    discord_channel_id: channelId,
    discord_channel_name: guild.channels.cache.get(channelId)?.name ?? null,
    armada_named: named ?? null,
  });
  return { channelId };
}

// ---- slash commands -------------------------------------------------

const bridgeCommand = new SlashCommandBuilder()
  .setName('bridge')
  .setDescription(
    summonAccess === SummonAccess.Open
      ? 'Janus voice bridge (anyone can summon)'
      : 'Janus voice bridge (whitelisted members)',
  )
  .addSubcommand((s) =>
    s
      .setName('summon')
      .setDescription('Bring Janus into your current voice channel and bridge to Armada')
      .addStringOption((o) => o.setName('armada_channel').setDescription('Armada voice channel name (optional)').setRequired(false)),
  )
  .addSubcommand((s) => s.setName('leave').setDescription('End the bridge session'))
  .addSubcommand((s) => s.setName('status').setDescription('Bridge state'));

async function registerCommands() {
  const rest = new REST({ version: '10' }).setToken(config.token);
  await rest.put(Routes.applicationGuildCommands(client.user.id, config.guildId), {
    body: [bridgeCommand.toJSON()],
  });
}

client.on('interactionCreate', async (interaction) => {
  if (!interaction.isChatInputCommand() || interaction.commandName !== 'bridge') return;
  const sub = interaction.options.getSubcommand();
  if (sub === 'status') {
    // Status keeps its historical owner-only check — the
    // whitelist widening covers summon/leave only.
    if (interaction.user.id !== config.ownerUserId) {
      await interaction.reply({ content: 'The bridge answers to the owner only.', ephemeral: true });
      return;
    }
  } else if (!isSummonAuthorized(summonAccess, summonWhitelist, interaction.user.id)) {
    // Denial wording is whitelist-mode only: in open mode
    // isSummonAuthorized never rejects an identified user,
    // so this branch is unreachable there.
    await interaction.reply({
      content: 'Bridge summon and leave are for whitelisted members only — ask the owner to add your Discord user ID to the whitelist.',
      ephemeral: true,
    });
    return;
  }
  if (sub === 'summon') {
    await interaction.deferReply({ ephemeral: true });
    const r = await beginDiscordSummon(interaction.user.id, interaction.options.getString('armada_channel'));
    await interaction.editReply(r.error ?? 'Janus is joining here and landing on the Armada side…');
  } else if (sub === 'leave') {
    if (!session) {
      await interaction.reply({ content: 'No active session.', ephemeral: true });
      return;
    }
    await endSession('leave command');
    await interaction.reply({ content: 'Bridge closed.', ephemeral: true });
  } else if (sub === 'status') {
    const lines = [
      `Discord leg: ${backend?.inChannel ? `in <#${backend.channelId}>` : 'not in voice'}`,
      `Session: ${session ? `live (summoned from ${session.origin})` : 'none'}`,
      `Armada leg (via mixer): ${rustState.state ?? 'unknown'}${rustState.armada_channel_name ? ` in ${rustState.armada_channel_name}` : ''}`,
      `PCM socket: ${pcm.connected ? 'connected' : 'DISCONNECTED'}`,
    ];
    await interaction.reply({ content: lines.join('\n'), ephemeral: true });
  }
});

// ---- conversational triggers (gated) ---------------------------------
//
// "janus join" / "janus leave" in a guild text channel —
// the primary surface when enabled (the owner's order,
// 2026-11-22); /bridge stays as the backstop. The matcher
// (converse.mjs) is tight: named + intent + not-a-question,
// or nothing happens. Authorization is the same
// SUMMON_ACCESS check as the slash path, summoner = the
// message author. There is NO conversational status —
// status stays slash-only and owner-only.
//
// Gated on DISCORD_CONVERSATIONAL=on because reading
// message text requires the MESSAGE_CONTENT privileged
// intent (portal toggle, the owner's). When off: no listener
// is registered and behavior is exactly as before.
if (config.conversational) {
  client.on('messageCreate', async (message) => {
    try {
      if (!guild || !backend) return;
      if (message.author.bot) return;
      if (message.guildId !== config.guildId) return; // guild only, no DMs
      const cmd = parseConversational(message.content);
      if (!cmd) return;
      if (!isSummonAuthorized(summonAccess, summonWhitelist, message.author.id)) {
        // Whitelist-mode only (open mode never rejects an
        // identified user). Same wording as the slash denial.
        await message.reply(
          'Bridge summon and leave are for whitelisted members only — ask the owner to add your Discord user ID to the whitelist.',
        );
        return;
      }
      if (cmd.action === 'join') {
        const r = await beginDiscordSummon(message.author.id, cmd.named);
        await message.reply(r.error ?? 'Janus is joining here and landing on the Armada side…');
      } else {
        if (!session) {
          await message.reply('No active session.');
          return;
        }
        await endSession('leave command');
        await message.reply('Bridge closed.');
      }
    } catch (e) {
      console.error('[janus-discord] conversational trigger error:', e);
    }
  });
}

// NOTE : this leg deliberately
// has NO emptiness auto-end of its own anymore. Janus
// leaves on his own only when no humans remain in EITHER
// channel of the session; the Rust side's presence watcher
// owns that decision for both summon origins and polls
// this leg's human count via the 'occupancy' control
// message above. (The old Discord-side-only voiceStateUpdate
// auto-end dropped the bridge while Armada still had
// people in the room — exactly what the rule forbids.)

// Presence auto-join : "when janus
// sees someone join a discord call it also joins an
// armada call." A human landing in a voice channel while
// Janus is idle starts a Discord-origin session in THAT
// channel — the same beginDiscordSummon path a slash /
// conversational summon takes, so the Armada landing
// resolution (occupied -> configured fallback -> any)
// and both sides' announcements are identical. Departure
// is the stay rule's business (above), never this
// handler's. Gated on DISCORD_AUTOJOIN=on, following the
// conversational opt-in pattern; the decision itself is
// the pure shouldAutoJoin in resolve.mjs.
if (config.autojoin) {
  client.on('voiceStateUpdate', async (oldState, newState) => {
    try {
      if (!guild || !backend) return;
      if (newState.guild.id !== config.guildId) return;
      const fire = shouldAutoJoin({
        enabled: config.autojoin,
        sessionActive: session !== null || sessionStarting,
        isBot: newState.member?.user?.bot ?? true,
        oldChannelId: oldState.channelId,
        newChannelId: newState.channelId,
      });
      if (!fire) return;
      // Claim the start synchronously: a second join in
      // the same moment must see a session already coming
      // up, not race a second summon past the Rust side
      // (which would fail it and tear down the good one).
      sessionStarting = true;
      try {
        const r = await beginDiscordSummon(newState.member.id, null);
        if (r.error) {
          console.error('[janus-discord] auto-join failed:', r.error);
        } else {
          console.log(
            `[janus-discord] auto-join: ${newState.member.user.tag} joined voice; bridging from channel ${r.channelId}`,
          );
        }
      } finally {
        sessionStarting = false;
      }
    } catch (e) {
      console.error('[janus-discord] auto-join error:', e);
    }
  });
}

client.once('clientReady', async () => {
  guild = client.guilds.cache.get(config.guildId) ?? (await client.guilds.fetch(config.guildId));
  backend = new VoiceBackend(guild, {
    onUserPcm: (userId, frame) => {
      // Never bridge the bridge: our own playback must not loop back.
      if (userId === client.user.id) return;
      pcm.sendUserPcm(userId, frame);
    },
    onSpeaking: (userId, start) => pcm.sendSpeak(userId, start),
    onConnectionLost: () => {
      console.error('[janus-discord] voice connection lost');
      endSession('discord voice connection lost').catch(() => {});
    },
  });
  await registerCommands();
  pcm.start();
  console.log(`[janus-discord] ready as ${client.user.tag}; socket ${config.socketPath}; default channel ${config.defaultVoiceChannelId ?? '(none)'}`);
  if (config.conversational) {
    console.log('[janus-discord] conversational triggers ON (DISCORD_CONVERSATIONAL=on) — "janus join" / "janus leave" in guild text channels; /bridge remains the backstop.');
  } else {
    console.log('[janus-discord] conversational triggers OFF — DISCORD_CONVERSATIONAL is not "on" (enabling them also requires the MESSAGE_CONTENT privileged intent in the developer portal). Slash commands only.');
  }
  if (config.autojoin) {
    console.log('[janus-discord] presence auto-join ON (DISCORD_AUTOJOIN=on) — a human joining voice while Janus is idle pulls him into a session.');
  } else {
    console.log('[janus-discord] presence auto-join OFF — DISCORD_AUTOJOIN is not "on". Janus joins voice only when summoned.');
  }
  if (config.announce) {
    console.log('[janus-discord] session announcements ON (JANUS_ANNOUNCE=on) — start/stop posts go to the Discord text channel.');
  } else {
    console.log('[janus-discord] session announcements OFF — JANUS_ANNOUNCE is not "on" ; sessions start/stop silently, journal only.');
  }
});

client.login(config.token);

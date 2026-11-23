import { test } from 'node:test';
import assert from 'node:assert/strict';
import { resolveLanding } from '../src/resolve.mjs';

const chans = (...defs) => defs.map(([id, name, occupants]) => ({ id, name, occupants }));

test('named channel wins (by id or case-insensitive name)', () => {
  const channels = chans(['1', 'General', ['u1']], ['2', 'Raid', []]);
  assert.deepEqual(resolveLanding({ named: '2', channels }), { channelId: '2', via: 'named' });
  assert.deepEqual(resolveLanding({ named: 'raid', channels }), { channelId: '2', via: 'named' });
  assert.match(resolveLanding({ named: 'nope', channels }).error, /no voice channel/);
});

test('exactly one occupied channel is the landing', () => {
  const channels = chans(['1', 'General', []], ['2', 'Raid', ['u1', 'u2']], ['3', 'AFK', []]);
  assert.deepEqual(resolveLanding({ channels }), { channelId: '2', via: 'occupied' });
});

test('several occupied channels: picks the first, never an error', () => {
  // the owner's rule: several occupied is not ambiguous
  // enough to fail — he just joins one of them.
  const channels = chans(['1', 'General', ['u1']], ['2', 'Raid', ['u2']]);
  assert.deepEqual(resolveLanding({ channels }), { channelId: '1', via: 'occupied' });
});

test('falls back to the configured default', () => {
  const channels = chans(['1', 'General', []], ['2', 'Raid', []]);
  assert.deepEqual(resolveLanding({ channels, defaultChannelId: '1' }), { channelId: '1', via: 'default' });
});

test('summoner absent from Discord voice still lands (no error)', () => {
  // The 2026-11-22 requirement: an Armada-origin summon
  // resolves even when nobody — the summoner included —
  // is in a Discord channel. No default configured, or a
  // stale default id: land in ANY channel. Only a guild
  // with no voice channels at all errors.
  const channels = chans(['1', 'General', []], ['2', 'Raid', []]);
  assert.deepEqual(resolveLanding({ channels }), { channelId: '1', via: 'any' });
  assert.deepEqual(resolveLanding({ channels, defaultChannelId: 'gone' }), { channelId: '1', via: 'any' });
  assert.match(resolveLanding({ channels: [] }).error, /no voice channels/);
});

test('excluded users (the bot itself) do not count as occupancy', () => {
  const channels = chans(['1', 'General', ['bot']], ['2', 'Raid', []]);
  const r = resolveLanding({ channels, excludeUserIds: new Set(['bot']), defaultChannelId: '2' });
  assert.deepEqual(r, { channelId: '2', via: 'default' });
});

test('countHumans counts people only — bots never count as someone', async () => {
  const { countHumans } = await import('../src/resolve.mjs');
  // the owner's rule cases: a channel holding only the bridge
  // (a bot) is an empty room; one human is a room.
  assert.equal(countHumans([{ bot: true }]), 0);
  assert.equal(countHumans([{ bot: true }, { bot: false }]), 1);
  assert.equal(countHumans([{ bot: false }, { bot: false }, { bot: true }]), 2);
  assert.equal(countHumans([]), 0);
});

test('shouldAutoJoin fires only on a human join while idle', async () => {
  const { shouldAutoJoin, parseAutoJoinEnabled } = await import('../src/resolve.mjs');
  const base = { enabled: true, sessionActive: false, isBot: false, oldChannelId: null, newChannelId: 'vc1' };
  // The firing case: a human joins a channel, Janus idle.
  assert.equal(shouldAutoJoin(base), true);
  // A channel switch while idle is a join to the destination.
  assert.equal(shouldAutoJoin({ ...base, oldChannelId: 'vc2' }), true);
  // Bot joins never fire (Janus himself included).
  assert.equal(shouldAutoJoin({ ...base, isBot: true }), false);
  // A session already active: first join won; this is just
  // someone entering the bridged room.
  assert.equal(shouldAutoJoin({ ...base, sessionActive: true }), false);
  // Mute/deafen/server updates don't change the channel.
  assert.equal(shouldAutoJoin({ ...base, oldChannelId: 'vc1', newChannelId: 'vc1' }), false);
  // A departure is not a join.
  assert.equal(shouldAutoJoin({ ...base, oldChannelId: 'vc1', newChannelId: null }), false);
  // The kill switch: disabled never fires.
  assert.equal(shouldAutoJoin({ ...base, enabled: false }), false);
  // The env parser follows the DISCORD_CONVERSATIONAL pattern.
  assert.equal(parseAutoJoinEnabled('on'), true);
  assert.equal(parseAutoJoinEnabled(' On '), true);
  assert.equal(parseAutoJoinEnabled('off'), false);
  assert.equal(parseAutoJoinEnabled(''), false);
  assert.equal(parseAutoJoinEnabled(undefined), false);
});

test('parseAnnounceEnabled: only "on" announces (JANUS_ANNOUNCE)', async () => {
  const { parseAnnounceEnabled } = await import('../src/resolve.mjs');
  // the owner's order (2026-11-22): routine session announcements
  // stop. Only an explicit "on" brings them back; the
  // deployment sets JANUS_ANNOUNCE=off.
  assert.equal(parseAnnounceEnabled('on'), true);
  assert.equal(parseAnnounceEnabled(' On '), true);
  assert.equal(parseAnnounceEnabled('off'), false);
  assert.equal(parseAnnounceEnabled(''), false);
  assert.equal(parseAnnounceEnabled(undefined), false);
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  SummonAccess,
  parseSummonAccess,
  parseIdList,
  buildSummonWhitelist,
  isSummonAuthorized,
} from '../src/auth.mjs';

const OWNER = '111111111111111111';
const FRIEND = '222222222222222222';
const FRIEND2 = '333333333333333333';
const STRANGER = '999999999999999999';
const OPEN = SummonAccess.Open;
const WHITELIST = SummonAccess.Whitelist;

test('the owner passes in whitelist mode (owner always in the union)', () => {
  const wl = buildSummonWhitelist(OWNER, `${FRIEND}`);
  assert.equal(isSummonAuthorized(WHITELIST, wl, OWNER), true);
});

test('a listed friend passes in whitelist mode', () => {
  const wl = buildSummonWhitelist(OWNER, `${FRIEND},${FRIEND2}`);
  assert.equal(isSummonAuthorized(WHITELIST, wl, FRIEND), true);
  assert.equal(isSummonAuthorized(WHITELIST, wl, FRIEND2), true);
});

test('an unlisted ID fails in whitelist mode', () => {
  const wl = buildSummonWhitelist(OWNER, `${FRIEND}`);
  assert.equal(isSummonAuthorized(WHITELIST, wl, STRANGER), false);
  assert.equal(isSummonAuthorized(WHITELIST, wl, ''), false);
  assert.equal(isSummonAuthorized(WHITELIST, wl, null), false);
  assert.equal(isSummonAuthorized(WHITELIST, null, OWNER), false);
});

test('empty friend list = the owner only in whitelist mode (old configs keep working)', () => {
  for (const raw of ['', '   ', null, undefined]) {
    const wl = buildSummonWhitelist(OWNER, raw);
    assert.equal(isSummonAuthorized(WHITELIST, wl, OWNER), true);
    assert.equal(isSummonAuthorized(WHITELIST, wl, FRIEND), false);
  }
});

test('whitespace and duplicates in the comma list do not break parsing', () => {
  assert.deepEqual(parseIdList(`  ${FRIEND} , , ${FRIEND2} ,${FRIEND},  `), [FRIEND, FRIEND2]);
  const wl = buildSummonWhitelist(OWNER, ` ${FRIEND} ,, ${FRIEND} , ${OWNER} `);
  assert.equal(wl.size, 2); // owner + friend, deduped
  assert.equal(isSummonAuthorized(WHITELIST, wl, FRIEND), true);
  assert.equal(isSummonAuthorized(WHITELIST, wl, OWNER), true);
});

test('parseIdList tolerates non-strings', () => {
  assert.deepEqual(parseIdList(null), []);
  assert.deepEqual(parseIdList(undefined), []);
  assert.deepEqual(parseIdList(42), []);
});

test('open mode authorizes an unlisted ID', () => {
  const wl = buildSummonWhitelist(OWNER, `${FRIEND}`);
  assert.equal(isSummonAuthorized(OPEN, wl, STRANGER), true);
  // Even with no whitelist at all, open mode lets anyone in.
  assert.equal(isSummonAuthorized(OPEN, null, STRANGER), true);
  assert.equal(isSummonAuthorized(OPEN, new Set(), STRANGER), true);
});

test('open mode authorizes the owner', () => {
  const wl = buildSummonWhitelist(OWNER, '');
  assert.equal(isSummonAuthorized(OPEN, wl, OWNER), true);
});

test('open mode still rejects an unidentified user', () => {
  const wl = buildSummonWhitelist(OWNER, `${FRIEND}`);
  assert.equal(isSummonAuthorized(OPEN, wl, ''), false);
  assert.equal(isSummonAuthorized(OPEN, wl, null), false);
  assert.equal(isSummonAuthorized(OPEN, wl, undefined), false);
});

test('parseSummonAccess: only "open" opens; everything else is whitelist', () => {
  assert.equal(parseSummonAccess('open'), OPEN);
  assert.equal(parseSummonAccess(' Open '), OPEN);
  assert.equal(parseSummonAccess('OPEN'), OPEN);
  assert.equal(parseSummonAccess('whitelist'), WHITELIST);
  assert.equal(parseSummonAccess(''), WHITELIST);
  assert.equal(parseSummonAccess('   '), WHITELIST);
  assert.equal(parseSummonAccess(null), WHITELIST);
  assert.equal(parseSummonAccess(undefined), WHITELIST);
  assert.equal(parseSummonAccess(42), WHITELIST);
});

test('invalid mode value falls back to whitelist behavior', () => {
  const wl = buildSummonWhitelist(OWNER, `${FRIEND}`);
  for (const bad of ['openn', 'everyone', 'public', 'yes', '1']) {
    const mode = parseSummonAccess(bad);
    assert.equal(mode, WHITELIST);
    assert.equal(isSummonAuthorized(mode, wl, OWNER), true);
    assert.equal(isSummonAuthorized(mode, wl, FRIEND), true);
    assert.equal(isSummonAuthorized(mode, wl, STRANGER), false);
  }
  // An unrecognized mode passed straight to the check also
  // behaves as whitelist.
  assert.equal(isSummonAuthorized('bogus-mode', wl, STRANGER), false);
  assert.equal(isSummonAuthorized('bogus-mode', wl, OWNER), true);
});

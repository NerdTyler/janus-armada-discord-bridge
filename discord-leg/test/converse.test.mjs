import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseConversational, parseConversationalEnabled } from '../src/converse.mjs';

// Mirrors armada-leg/src/converse.rs tests — kept in lockstep.

const JOIN = { action: 'join', named: null };
const LEAVE = { action: 'leave' };
const joinNamed = (named) => ({ action: 'join', named });

test('join positives from the owner\'s examples', () => {
  assert.deepEqual(parseConversational('janus join'), JOIN);
  assert.deepEqual(parseConversational('join us janus'), JOIN);
  assert.deepEqual(parseConversational('janus bridge'), JOIN);
  assert.deepEqual(parseConversational('bridge us janus'), JOIN);
  assert.deepEqual(parseConversational('janus connect'), JOIN);
  assert.deepEqual(parseConversational('janus start the bridge'), JOIN);
  assert.deepEqual(parseConversational('have janus join'), JOIN);
  assert.deepEqual(parseConversational('get janus in here'), JOIN);
  assert.deepEqual(parseConversational('janus hop in'), JOIN);
});

test('join positives: natural variations', () => {
  assert.deepEqual(parseConversational('Janus, join!'), JOIN);
  assert.deepEqual(parseConversational('hey janus please join'), JOIN);
  assert.deepEqual(parseConversational('please janus bridge'), JOIN);
  assert.deepEqual(parseConversational('JANUS CONNECT'), JOIN);
  assert.deepEqual(parseConversational('janus join us'), JOIN);
});

test('join extracts the named channel like the slash option', () => {
  assert.deepEqual(parseConversational('janus join VC 2'), joinNamed('VC 2'));
  assert.deepEqual(parseConversational('janus bridge to General'), joinNamed('General'));
  assert.deepEqual(parseConversational('janus join the VC 2'), joinNamed('VC 2'));
  assert.deepEqual(parseConversational('have janus join Raid Night'), joinNamed('Raid Night'));
  // Filler-only remainders are not channel names.
  assert.deepEqual(parseConversational('janus join please'), JOIN);
  assert.deepEqual(parseConversational('get janus in here'), JOIN);
});

test('leave positives from the owner\'s examples', () => {
  assert.deepEqual(parseConversational('janus leave'), LEAVE);
  assert.deepEqual(parseConversational('janus stop'), LEAVE);
  assert.deepEqual(parseConversational('janus disconnect'), LEAVE);
  assert.deepEqual(parseConversational('janus hang up'), LEAVE);
  assert.deepEqual(parseConversational('stop the bridge janus'), LEAVE);
  assert.deepEqual(parseConversational('janus go away'), LEAVE);
});

test('leave wins over join words in the same message', () => {
  assert.deepEqual(parseConversational('janus stop the bridge'), LEAVE);
});

test('negatives from the owner\'s list never fire', () => {
  for (const text of [
    'janus is cool',
    'what did janus say',
    'i love janus',
    'janus',
    'everyone join vc',
    'the bridge was great yesterday',
  ]) {
    assert.equal(parseConversational(text), null, `must not fire: ${JSON.stringify(text)}`);
  }
});

test('questions about Janus never fire', () => {
  for (const text of [
    'did janus join the call earlier?',
    'janus join?',
    'can janus join us',
    'is janus in vc',
    'what channel did janus bridge to',
  ]) {
    assert.equal(parseConversational(text), null, `must not fire: ${JSON.stringify(text)}`);
  }
});

test('near misses never fire', () => {
  for (const text of [
    'janus joined vc earlier', // past tense is a report, not a request
    'janus is joining soon', // likewise
    'janusx join', // not the whole word "janus"
    'someone tell janus to join', // relayed ask — not in the tight set
    'join the vc everyone', // verb, no janus
    'bridge night was fun', // "bridge" as a noun, no janus
    'janus start', // incomplete pattern
    'janus is the bridge', // "bridge" as a noun, "is" breaks the pattern
    '',
    '   ',
  ]) {
    assert.equal(parseConversational(text), null, `must not fire: ${JSON.stringify(text)}`);
  }
});

test('non-string input never fires', () => {
  assert.equal(parseConversational(null), null);
  assert.equal(parseConversational(undefined), null);
  assert.equal(parseConversational(42), null);
});

test('parseConversationalEnabled: only "on" enables', () => {
  assert.equal(parseConversationalEnabled('on'), true);
  assert.equal(parseConversationalEnabled(' On '), true);
  assert.equal(parseConversationalEnabled('ON'), true);
  assert.equal(parseConversationalEnabled('off'), false);
  assert.equal(parseConversationalEnabled(''), false);
  assert.equal(parseConversationalEnabled('yes'), false);
  assert.equal(parseConversationalEnabled('1'), false);
  assert.equal(parseConversationalEnabled(null), false);
  assert.equal(parseConversationalEnabled(undefined), false);
});

// Conversational summon/leave triggers (the owner's order,
// 2026-11-22: "the command for janus to join and leave
// should be conversational").
//
// the owner's standing rule (inherited from the reference bot's conventions):
// conversational-first for every feature — /bridge slash
// commands are backstops only — and ordinary conversation
// must NEVER misfire a detector. This matcher is therefore
// deliberately TIGHT. A message fires only when ALL of
// these hold:
//
//   1. It names Janus: the token "janus" appears as a whole
//      word (case-insensitive). "janus is cool" names him
//      but has no intent (rule 2); "everyone join vc" has
//      an intent verb but never names him — neither fires.
//   2. A join or leave intent pattern matches (the exact
//      sets below — nothing else is an intent).
//   3. It is not a question: a message containing '?' or
//      starting with an interrogative word (what/did/is/
//      can/…) never fires. People talk ABOUT the bridge;
//      Janus answers requests, not questions about himself.
//
// There is deliberately NO conversational status trigger:
// /bridge status stays owner-only and command-only.
//
// Authorization is NOT this module's business: callers run
// the same SUMMON_ACCESS check (auth.mjs) as the slash
// path, so open/whitelist behavior is identical.
//
// DISCORD GATE: reading message text at all requires the
// MESSAGE_CONTENT privileged intent, so the listener that
// uses this matcher only activates when
// DISCORD_CONVERSATIONAL=on (see index.mjs and
// bridge.env.example). This module itself is pure.
//
// Pure functions over plain data so they unit-test without
// a Discord connection. Mirrors armada-leg/src/converse.rs
// — kept in lockstep (same patterns, same tests).

/**
 * Filler words a pattern's '*' slot may absorb between the
 * meaningful words: "janus please join", "join us janus",
 * "stop the bridge". Deliberately small, and prepositions
 * that can carry meaning (in/to/on/up) are NOT fillers —
 * they appear as literals in patterns or in name cleanup.
 * ("to" is excluded on purpose: it keeps relayed asks like
 * "tell janus to join" from firing.)
 */
const FILLERS = new Set(['us', 'the', 'please', 'now', 'over', 'here']);

/** Interrogative openers: a message starting with one of
 * these is a question, not a request — never fires. */
const QUESTION_STARTERS = new Set([
  'what', 'why', 'when', 'where', 'who', 'how', 'did', 'does', 'do', 'is', 'was', 'were',
  'are', 'can', 'could', 'would', 'should', 'will', 'has', 'had',
]);

/**
 * JOIN patterns — the complete join verb set: join, bridge
 * (as a verb), connect, "start the bridge", "hop in", and
 * the "get janus in (here)" idiom. '*' = zero or more
 * FILLERS words. Anything else is conversation, not a
 * summon.
 */
const JOIN_PATTERNS = [
  ['janus', '*', 'join'],
  ['join', '*', 'janus'],
  ['janus', '*', 'bridge'],
  ['bridge', '*', 'janus'],
  ['janus', '*', 'connect'],
  ['janus', 'start', '*', 'bridge'],
  ['janus', 'hop', 'in'],
  ['get', 'janus', 'in'],
];

/**
 * LEAVE patterns — the complete leave verb set, kept tight
 * on purpose: leave, stop, disconnect, "hang up", "go
 * away", plus "stop the bridge" (Janus is named elsewhere
 * in the message; rule 1 still applies). Checked BEFORE
 * the join patterns, so an explicit stop always wins over
 * any join word in the same message.
 */
const LEAVE_PATTERNS = [
  ['janus', '*', 'leave'],
  ['janus', '*', 'stop'],
  ['stop', '*', 'bridge'],
  ['janus', '*', 'disconnect'],
  ['janus', '*', 'hang', 'up'],
  ['janus', 'go', 'away'],
];

/** Split into word tokens (runs of alphanumeric chars),
 * keeping the original casing for channel-name extraction. */
function tokenize(text) {
  return text.match(/[\p{L}\p{N}]+/gu) ?? [];
}

/** Match `pat` against `lower` starting at token `start`;
 * returns the token index just past the match, or null. */
function matchAt(lower, pat, start) {
  const rec = (ti, pi) => {
    if (pi === pat.length) return ti;
    const el = pat[pi];
    if (el === '*') {
      // Zero or more filler words: try the rest of the
      // pattern at each absorption count.
      let end = ti;
      for (;;) {
        const r = rec(end, pi + 1);
        if (r !== null) return r;
        if (end < lower.length && FILLERS.has(lower[end])) end += 1;
        else return null;
      }
    }
    if (ti < lower.length && lower[ti] === el) return rec(ti + 1, pi + 1);
    return null;
  };
  return rec(start, 0);
}

/** First match of `pat` anywhere in the message; returns
 * the token index just past the match (name extraction
 * starts there), or null. */
function findPattern(lower, pat) {
  for (let start = 0; start < lower.length; start += 1) {
    const end = matchAt(lower, pat, start);
    if (end !== null) return end;
  }
  return null;
}

/**
 * The far-side channel name spoken after a join pattern:
 * the tokens past the match, minus leading connectives
 * ("to General", "the VC 2", "channel VC 2") and trailing
 * filler ("join us", "join please"). Original casing is
 * preserved — landing resolution matches names
 * case-insensitively, same as the slash option.
 */
function extractNamed(orig, lower, from) {
  const LEADING = new Set(['to', 'into', 'in', 'on', 'the', 'channel']);
  let lo = from;
  let hi = lower.length;
  while (lo < hi && (LEADING.has(lower[lo]) || FILLERS.has(lower[lo]))) lo += 1;
  while (hi > lo && FILLERS.has(lower[hi - 1])) hi -= 1;
  if (lo >= hi) return null;
  return orig.slice(lo, hi).join(' ');
}

/**
 * Parse a chat message into a conversational command:
 *   { action: 'join', named: string|null }
 *   { action: 'leave' }
 * or null when it is ordinary conversation (the
 * overwhelming majority — see the module docs for the
 * firing rules). Non-string input -> null.
 */
export function parseConversational(text) {
  if (typeof text !== 'string') return null;
  // Rule 3a: questions never fire.
  if (text.includes('?')) return null;
  const orig = tokenize(text);
  if (orig.length === 0) return null;
  const lower = orig.map((t) => t.toLowerCase());
  // Rule 3b: interrogative openers never fire.
  if (QUESTION_STARTERS.has(lower[0])) return null;
  // Rule 1: Janus must be named, as a whole word.
  if (!lower.includes('janus')) return null;
  // Rule 2: an intent pattern must match. Leave first —
  // an explicit stop always wins.
  for (const pat of LEAVE_PATTERNS) {
    if (findPattern(lower, pat) !== null) return { action: 'leave' };
  }
  for (const pat of JOIN_PATTERNS) {
    const end = findPattern(lower, pat);
    if (end !== null) return { action: 'join', named: extractNamed(orig, lower, end) };
  }
  return null;
}

/**
 * Parse DISCORD_CONVERSATIONAL: only the exact value "on"
 * (case-insensitive, surrounding whitespace tolerated)
 * enables the Discord-side conversational listener.
 * Anything else — unset, empty, "off", invalid — is off:
 * reading message text needs the MESSAGE_CONTENT
 * privileged intent in the owner's developer portal, so this
 * stays opt-in even though the owner's Armada side is always
 * conversational.
 */
export function parseConversationalEnabled(raw) {
  if (typeof raw !== 'string') return false;
  return raw.trim().toLowerCase() === 'on';
}

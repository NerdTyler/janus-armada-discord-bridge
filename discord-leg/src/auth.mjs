// Summon authorization : /bridge summon
// and /bridge leave are gated by the SUMMON_ACCESS mode:
//
//   - SUMMON_ACCESS=open      -> ANYONE may summon/leave.
//     This is the reference deployment's decision (2026-11-22, superseding
//     owner-only, then whitelisted-friends): anyone in the
//     Discord guild / Armada community can summon. The owner's
//     deployment ships this mode (see bridge.env.example).
//   - SUMMON_ACCESS=whitelist -> the whitelist below decides.
//     Unset, empty, or any invalid value falls back to this
//     mode: it is the safe default for the code.
//
// In whitelist mode the list is the union of:
//   - DISCORD_OWNER_USER_ID  (always included — the owner can
//     never lock himself out, and old configs keep working)
//   - DISCORD_SUMMON_USER_IDS (comma-separated friend IDs,
//     added as the owner collects them)
//
// NOT covered by this module, in EITHER mode — both keep
// their historical owner-only checks where they live:
//   - /bridge status (index.mjs), and
//   - Armada community-invite auto-accept (Armada leg).
//
// Pure functions over plain data so they unit-test without a
// Discord connection. Mirrors armada-leg/src/auth.rs.

/** The two SUMMON_ACCESS modes. */
export const SummonAccess = Object.freeze({
  Open: 'open',
  Whitelist: 'whitelist',
});

/**
 * Parse SUMMON_ACCESS: only the exact value "open"
 * (case-insensitive, surrounding whitespace tolerated)
 * selects open mode. Unset, empty, "whitelist", and every
 * invalid value fall back to whitelist — the safe default.
 * Accepts null/undefined/non-string -> 'whitelist'.
 */
export function parseSummonAccess(raw) {
  if (typeof raw !== 'string') return SummonAccess.Whitelist;
  return raw.trim().toLowerCase() === SummonAccess.Open
    ? SummonAccess.Open
    : SummonAccess.Whitelist;
}

/**
 * Parse a comma-separated ID list: trims whitespace, drops
 * empty entries, dedupes (first occurrence wins).
 * Accepts null/undefined/non-string -> [].
 */
export function parseIdList(raw) {
  if (typeof raw !== 'string') return [];
  const out = [];
  const seen = new Set();
  for (const part of raw.split(',')) {
    const id = part.trim();
    if (!id || seen.has(id)) continue;
    seen.add(id);
    out.push(id);
  }
  return out;
}

/**
 * Build the summon whitelist: owner's ID (when set) plus every
 * ID in the comma-separated extra list. Returns a Set.
 * Only consulted in whitelist mode.
 */
export function buildSummonWhitelist(ownerId, rawList) {
  const set = new Set(parseIdList(rawList));
  if (typeof ownerId === 'string' && ownerId.trim()) set.add(ownerId.trim());
  return set;
}

/**
 * True when userId may summon/leave under the given access
 * mode. Open mode authorizes ANY identified user (an empty
 * or missing userId is still rejected — there is nobody to
 * summon for). Whitelist mode requires membership in the
 * whitelist Set. An unrecognized mode value behaves as
 * whitelist.
 */
export function isSummonAuthorized(access, whitelist, userId) {
  if (typeof userId !== 'string' || !userId) return false;
  if (access === SummonAccess.Open) return true;
  if (!whitelist) return false;
  return whitelist.has(userId);
}

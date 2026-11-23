//! Summon authorization : `!summon` and
//! `!leave` are gated by the SUMMON_ACCESS mode:
//!
//!   - SUMMON_ACCESS=open      -> ANYONE may summon/leave.
//!     This is the reference deployment's decision (2026-11-22, superseding
//!     owner-only, then whitelisted-friends): anyone in the
//!     Armada community / Discord guild can summon. The owner's
//!     deployment ships this mode (see bridge.env.example).
//!   - SUMMON_ACCESS=whitelist -> the whitelist below
//!     decides. Unset, empty, or any invalid value falls
//!     back to this mode: the safe default for the code.
//!
//! In whitelist mode the list is the union of:
//!   - ARMADA_OWNER_NPUB (always included — the owner can never
//!     lock himself out, and old configs keep working)
//!   - ARMADA_SUMMON_NPUBS (comma-separated friend npubs,
//!     added as the owner collects them)
//!
//! This module covers SUMMON authorization only. Two checks
//! deliberately stay owner-only in BOTH modes and live
//! elsewhere:
//!   - community-invite auto-accept (run_bot's builder
//!     whitelist is fed the owner's npub alone), and
//!   - `!bridge status` (its historical check in
//!     handle_chat_message).
//!
//! Pure functions so they unit-test without a bot connection.
//! Mirrors discord-leg/src/auth.mjs — kept in lockstep.

use std::collections::HashSet;

/// SUMMON_ACCESS mode for summon/leave authorization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SummonAccess {
    /// Anyone may summon/leave.
    Open,
    /// Only the summon whitelist may summon/leave.
    Whitelist,
}

/// Parse SUMMON_ACCESS: only the exact value "open"
/// (case-insensitive, surrounding whitespace tolerated)
/// selects open mode. Unset, empty, "whitelist", and every
/// invalid value fall back to whitelist — the safe default.
pub fn parse_summon_access(raw: Option<&str>) -> SummonAccess {
    match raw {
        Some(raw) if raw.trim().eq_ignore_ascii_case("open") => SummonAccess::Open,
        _ => SummonAccess::Whitelist,
    }
}

/// Parse a comma-separated list: trims whitespace, drops
/// empty entries, dedupes (first occurrence wins).
pub fn parse_id_list(raw: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(raw) = raw {
        for part in raw.split(',') {
            let id = part.trim();
            if !id.is_empty() && !out.iter().any(|x| x == id) {
                out.push(id.to_string());
            }
        }
    }
    out
}

/// Build the summon whitelist: the owner's id (when set) plus
/// every id in the comma-separated extra list. Only consulted
/// in whitelist mode.
pub fn build_whitelist(owner: Option<&str>, raw_list: Option<&str>) -> HashSet<String> {
    let mut set: HashSet<String> = parse_id_list(raw_list).into_iter().collect();
    if let Some(owner) = owner {
        let owner = owner.trim();
        if !owner.is_empty() {
            set.insert(owner.to_string());
        }
    }
    set
}

/// True when `id` may summon/leave under the given access
/// mode. Open mode authorizes ANY identified user (a missing
/// or empty id is still rejected — there is nobody to summon
/// for). Whitelist mode requires membership in the whitelist.
pub fn is_authorized(access: SummonAccess, whitelist: &HashSet<String>, id: Option<&str>) -> bool {
    match id {
        Some(id) if !id.is_empty() => match access {
            SummonAccess::Open => true,
            SummonAccess::Whitelist => whitelist.contains(id),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "owner-pubkey";
    const FRIEND: &str = "friend-pubkey";
    const FRIEND2: &str = "friend2-pubkey";
    const STRANGER: &str = "stranger-pubkey";
    const WL: SummonAccess = SummonAccess::Whitelist;
    const OPEN: SummonAccess = SummonAccess::Open;

    #[test]
    fn owner_passes_always_in_union() {
        let wl = build_whitelist(Some(OWNER), Some(FRIEND));
        assert!(is_authorized(WL, &wl, Some(OWNER)));
    }

    #[test]
    fn listed_friends_pass() {
        let wl = build_whitelist(Some(OWNER), Some(&format!("{FRIEND},{FRIEND2}")));
        assert!(is_authorized(WL, &wl, Some(FRIEND)));
        assert!(is_authorized(WL, &wl, Some(FRIEND2)));
    }

    #[test]
    fn unlisted_id_fails_in_whitelist_mode() {
        let wl = build_whitelist(Some(OWNER), Some(FRIEND));
        assert!(!is_authorized(WL, &wl, Some(STRANGER)));
        assert!(!is_authorized(WL, &wl, Some("")));
        assert!(!is_authorized(WL, &wl, None));
    }

    #[test]
    fn empty_friend_list_is_owner_only() {
        for raw in [None, Some(""), Some("   ")] {
            let wl = build_whitelist(Some(OWNER), raw);
            assert!(is_authorized(WL, &wl, Some(OWNER)));
            assert!(!is_authorized(WL, &wl, Some(FRIEND)));
        }
    }

    #[test]
    fn whitespace_and_duplicates_do_not_break_parsing() {
        assert_eq!(
            parse_id_list(Some(&format!("  {FRIEND} , , {FRIEND2} ,{FRIEND},  "))),
            vec![FRIEND.to_string(), FRIEND2.to_string()]
        );
        let wl = build_whitelist(Some(OWNER), Some(&format!(" {FRIEND} ,, {FRIEND} , {OWNER} ")));
        assert_eq!(wl.len(), 2); // owner + friend, deduped
        assert!(is_authorized(WL, &wl, Some(FRIEND)));
        assert!(is_authorized(WL, &wl, Some(OWNER)));
    }

    #[test]
    fn no_owner_no_list_authorizes_nobody_in_whitelist_mode() {
        let wl = build_whitelist(None, None);
        assert!(wl.is_empty());
        assert!(!is_authorized(WL, &wl, Some(OWNER)));
    }

    #[test]
    fn open_mode_authorizes_an_unlisted_id() {
        let wl = build_whitelist(Some(OWNER), Some(FRIEND));
        assert!(is_authorized(OPEN, &wl, Some(STRANGER)));
        // Even with an empty whitelist, open mode lets anyone in.
        let empty = build_whitelist(None, None);
        assert!(is_authorized(OPEN, &empty, Some(STRANGER)));
    }

    #[test]
    fn open_mode_authorizes_owner() {
        let wl = build_whitelist(Some(OWNER), None);
        assert!(is_authorized(OPEN, &wl, Some(OWNER)));
    }

    #[test]
    fn open_mode_still_rejects_an_unidentified_user() {
        let wl = build_whitelist(Some(OWNER), Some(FRIEND));
        assert!(!is_authorized(OPEN, &wl, Some("")));
        assert!(!is_authorized(OPEN, &wl, None));
    }

    #[test]
    fn parse_summon_access_only_open_opens() {
        assert_eq!(parse_summon_access(Some("open")), OPEN);
        assert_eq!(parse_summon_access(Some(" Open ")), OPEN);
        assert_eq!(parse_summon_access(Some("OPEN")), OPEN);
        assert_eq!(parse_summon_access(Some("whitelist")), WL);
        assert_eq!(parse_summon_access(Some("")), WL);
        assert_eq!(parse_summon_access(Some("   ")), WL);
        assert_eq!(parse_summon_access(None), WL);
    }

    #[test]
    fn invalid_mode_value_falls_back_to_whitelist_behavior() {
        let wl = build_whitelist(Some(OWNER), Some(FRIEND));
        for bad in ["openn", "everyone", "public", "yes", "1"] {
            let mode = parse_summon_access(Some(bad));
            assert_eq!(mode, WL);
            assert!(is_authorized(mode, &wl, Some(OWNER)));
            assert!(is_authorized(mode, &wl, Some(FRIEND)));
            assert!(!is_authorized(mode, &wl, Some(STRANGER)));
        }
    }
}

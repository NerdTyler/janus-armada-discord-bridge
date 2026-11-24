//! Far-side landing resolution : when a
//! summon originates on the OTHER platform, where does this
//! leg land?
//!   (1) the channel named in the summon command, if given;
//!   (2) else the first voice channel that is currently
//!       occupied (one occupied is the normal case; if
//!       several are occupied we still land — the owner's rule:
//!       "if im not in a channel, he just joins any of
//!       them" — a landing must never fail over ambiguity);
//!   (3) else the configured default channel;
//!   (4) else the first voice channel there is ("any").
//! The ONLY failures left: a named channel that does not
//! exist, and a server with no voice channels at all. In
//! particular the summoner's own presence on the far side
//! is NEVER an input — an Armada-origin summon lands on
//! Discord whether or not the summoner is in a Discord
//! channel. Mirrors discord-leg/src/resolve.mjs — the two
//! implementations are kept in lockstep and share test
//! cases.
//!
//! Occupancy on the Armada side comes from vector-core's
//! folded kind-23313 voice presence, read on demand via
//! the SDK's `Channel::voice_members()` (the source the reference
//! bot uses); the wiring layer snapshots it into the plain
//! shape below. A member counts as an occupant when core
//! sees a fresh presence claim for them and they are not
//! the bridge itself. (The earlier hand-rolled relay fetch
//! of ephemeral presence wraps was blind: relays do not
//! store kind-21059, so a cold EOSE fetch folds nothing.)

/// Is this channel a voice channel? The native `view` says
/// so for Vector-born channels — but Discord-imported
/// communities (the owner's server) carry voice channels whose
/// view is NOT "voice": their voice-ness lives in the
/// "-voice" name suffix the import stamps (clients strip
/// it for display: stored "VC-voice" shows as "VC"). So:
/// native view OR the import suffix counts. This is
/// the reference bot's rule (its voice.rs `is_voice_channel`), adopted
/// after the 2026-11-22 live diagnosis: filtering on
/// view == "voice" alone made the snapshot EMPTY for
/// the reference server and every Armada summon failed with
/// "not in a voice channel". Pure — unit-tested.
pub fn is_voice_channel(name: &str, view: &str) -> bool {
    view == "voice" || name.trim().to_lowercase().ends_with("-voice")
}

/// One voice channel's occupancy snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelOccupancy {
    /// Channel id (hex, as the SDK names channels).
    pub id: String,
    pub name: String,
    /// Member npubs present in the channel, bridge excluded.
    pub occupants: Vec<String>,
}

/// One-line rendering of a snapshot for the summon log:
/// "VC-voice: 1 occupant(s), VC 2-voice: 0 occupant(s)".
/// A summon attempt must always log the snapshot it saw —
/// the 2026-11-22 blindness hid behind a silent empty Vec.
pub fn snapshot_summary(channels: &[ChannelOccupancy]) -> String {
    if channels.is_empty() {
        return "(no voice channels)".to_string();
    }
    channels
        .iter()
        .map(|c| format!("{}: {} occupant(s)", c.name, c.occupants.len()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    Channel { id: String, via: &'static str },
    Error(String),
}

/// Resolve the landing. `named` matches a channel id or a
/// case-insensitive channel name.
pub fn resolve_landing(
    named: Option<&str>,
    channels: &[ChannelOccupancy],
    default_channel_id: Option<&str>,
) -> Landing {
    if let Some(named) = named {
        if let Some(c) = channels.iter().find(|c| c.id == named) {
            return Landing::Channel { id: c.id.clone(), via: "named" };
        }
        if let Some(c) = channels.iter().find(|c| c.name.eq_ignore_ascii_case(named)) {
            return Landing::Channel { id: c.id.clone(), via: "named" };
        }
        return Landing::Error(format!("no voice channel matches \"{named}\""));
    }
    // (2) First occupied channel wins — one occupied is the
    // normal case, and several occupied is NOT an error
    // (the owner: "he just joins any of them").
    if let Some(c) = channels.iter().find(|c| !c.occupants.is_empty()) {
        return Landing::Channel { id: c.id.clone(), via: "occupied" };
    }
    // (3) The configured default, when it still exists.
    if let Some(def) = default_channel_id {
        if channels.iter().any(|c| c.id == def) {
            return Landing::Channel { id: def.to_string(), via: "default" };
        }
    }
    // (4) Any channel at all — the summoner not being in a
    // far-side channel (and a stale/missing default) must
    // never block the landing.
    if let Some(c) = channels.first() {
        return Landing::Channel { id: c.id.clone(), via: "any" };
    }
    Landing::Error("there are no voice channels to land in".into())
}

/// The occupants that count as "someone": everyone present
/// minus known bots (the caller has already excluded the
/// bridge itself). The owner's rule (2026-11-22): Janus's
/// staying/leaving is anchored to HUMAN occupancy — a bot
/// alone in a channel is an empty room.
pub fn humans_only(
    occupants: &[String],
    bot_npubs: &std::collections::HashSet<String>,
) -> Vec<String> {
    occupants
        .iter()
        .filter(|n| !bot_npubs.contains(n.as_str()))
        .cloned()
        .collect()
}

/// Whether a live session should end for emptiness
/// : ONLY when no humans remain
/// in EITHER channel of the session — both sides known
/// human-empty. One side emptying while the other still
/// has people never ends it, and a side whose occupancy
/// could not be determined (`None` — the far leg didn't
/// answer) never counts toward ending: missing data must
/// not drop a live bridge.
pub fn should_end_for_emptiness(armada_humans: usize, discord_humans: Option<usize>) -> bool {
    armada_humans == 0 && discord_humans == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chans(defs: &[(&str, &str, &[&str])]) -> Vec<ChannelOccupancy> {
        defs.iter()
            .map(|(id, name, occ)| ChannelOccupancy {
                id: id.to_string(),
                name: name.to_string(),
                occupants: occ.iter().map(|s| s.to_string()).collect(),
            })
            .collect()
    }

    #[test]
    fn named_wins_by_id_or_name() {
        let channels = chans(&[("aa", "VC", &["u1"]), ("bb", "VC 2", &[])]);
        assert_eq!(
            resolve_landing(Some("bb"), &channels, None),
            Landing::Channel { id: "bb".into(), via: "named" }
        );
        assert_eq!(
            resolve_landing(Some("vc 2"), &channels, None),
            Landing::Channel { id: "bb".into(), via: "named" }
        );
        assert!(matches!(
            resolve_landing(Some("nope"), &channels, None),
            Landing::Error(_)
        ));
    }

    #[test]
    fn exactly_one_occupied_wins() {
        let channels = chans(&[("aa", "VC", &[]), ("bb", "VC 2", &["u1", "u2"])]);
        assert_eq!(
            resolve_landing(None, &channels, None),
            Landing::Channel { id: "bb".into(), via: "occupied" }
        );
    }

    #[test]
    fn several_occupied_picks_the_first() {
        // the owner's rule: several occupied is not ambiguous
        // enough to fail — he just joins one of them.
        let channels = chans(&[("aa", "VC", &["u1"]), ("bb", "VC 2", &["u2"])]);
        assert_eq!(
            resolve_landing(None, &channels, None),
            Landing::Channel { id: "aa".into(), via: "occupied" }
        );
    }

    #[test]
    fn default_is_the_fallback() {
        let channels = chans(&[("aa", "VC", &[]), ("bb", "VC 2", &[])]);
        assert_eq!(
            resolve_landing(None, &channels, Some("aa")),
            Landing::Channel { id: "aa".into(), via: "default" }
        );
    }

    #[test]
    fn summoner_absent_still_lands() {
        // The 2026-11-22 requirement: nobody in voice on the
        // far side (the summoner included) must never block
        // the landing. With a default, land there; without
        // one — or with a stale default id — land in ANY
        // channel rather than erroring.
        let channels = chans(&[("aa", "VC", &[]), ("bb", "VC 2", &[])]);
        assert_eq!(
            resolve_landing(None, &channels, None),
            Landing::Channel { id: "aa".into(), via: "any" }
        );
        assert_eq!(
            resolve_landing(None, &channels, Some("zz")),
            Landing::Channel { id: "aa".into(), via: "any" }
        );
        // Only a server with no voice channels at all fails.
        assert!(matches!(resolve_landing(None, &[], None), Landing::Error(_)));
    }

    #[test]
    fn voice_channel_detection() {
        // Vector-born channels: native view.
        assert!(is_voice_channel("Lounge", "voice"));
        // Discord-imported channels (the owner's server): view
        // is not "voice"; the "-voice" suffix carries it.
        assert!(is_voice_channel("VC-voice", "chat"));
        assert!(is_voice_channel("VC 2-voice", "chat"));
        assert!(is_voice_channel("Raid-Voice", "chat"));
        // Plain text channels never count.
        assert!(!is_voice_channel("general-chat", "chat"));
        assert!(!is_voice_channel("voice-notes", "chat"));
    }

    #[test]
    fn humans_only_excludes_bots_not_people() {
        let bots: std::collections::HashSet<String> =
            ["music-bot".to_string()].into_iter().collect();
        let occ = vec!["alice".to_string(), "music-bot".to_string()];
        // Case 1 setup (the owner's rule): The music bot leaves, the owner
        // remains — the owner still counts, the room is occupied.
        assert_eq!(humans_only(&occ, &bots), vec!["alice".to_string()]);
        // The music bot alone is an empty room.
        assert!(humans_only(&["music-bot".to_string()], &bots).is_empty());
        // No bots configured: everyone counts.
        let none: std::collections::HashSet<String> = Default::default();
        assert_eq!(humans_only(&occ, &none).len(), 2);
    }

    #[test]
    fn emptiness_ends_only_when_both_sides_are_human_empty() {
        // Case 1: The music bot summoned Janus and left; the owner is
        // still in the Armada channel -> STAYS, whatever
        // Discord reports.
        assert!(!should_end_for_emptiness(1, Some(0)));
        assert!(!should_end_for_emptiness(1, Some(3)));
        assert!(!should_end_for_emptiness(1, None));
        // Case 2: the Armada side has emptied of humans but
        // friends remain in the Discord channel -> STAYS.
        assert!(!should_end_for_emptiness(0, Some(2)));
        // Case 3: no humans on either side -> Janus leaves
        // on his own.
        assert!(should_end_for_emptiness(0, Some(0)));
        // Unknown Discord occupancy never ends a session:
        // missing data is not an empty room.
        assert!(!should_end_for_emptiness(0, None));
    }

    #[test]
    fn summary_renders_occupancy() {
        let channels = chans(&[("aa", "VC-voice", &["u1"]), ("bb", "VC 2-voice", &[])]);
        assert_eq!(
            snapshot_summary(&channels),
            "VC-voice: 1 occupant(s), VC 2-voice: 0 occupant(s)"
        );
        assert_eq!(snapshot_summary(&[]), "(no voice channels)");
    }
}

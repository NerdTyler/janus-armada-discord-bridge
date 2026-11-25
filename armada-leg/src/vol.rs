//! Per-stream volume commands (the owner's order, 2026-11-24):
//! manual per-source gains in Janus's mixer, driven from
//! Armada room chat — `!vol` commands plus conversational
//! forms — by OWNER ALONE. Mixer gains change what every
//! listener on the other side of the bridge hears, so the
//! gate is his npub and everyone else's volume-shaped
//! messages are silently ignored: no reply, no state
//! change, not even a denial (chat-visible Janus output
//! stays command-response-only; JANUS_ANNOUNCE remains a
//! separate, off, thing).
//!
//! Forms:
//!   !vols                        list streams + gains
//!   !vol <name> <0-150>          set a stream's manual gain
//!   !vol <name> reset            one stream back to 100%
//!   !vol reset                   all streams back to 100%
//!   …and conversational equivalents when a volume intent
//!   and a known stream name are present ("janus set MusicBot
//!   to 50", "turn down MusicBot", "make MusicBot a bit quieter",
//!   "what are the volumes", "reset volumes", …). The
//!   matcher is deliberately conservative in the converse.rs
//!   spirit: a volume signal (volume/vol word, turn up/down,
//!   louder/quieter, set/make + number) must be present,
//!   bare relative/absolute forms additionally require the
//!   target stream to be named in the message, and ordinary
//!   chat that merely mentions a name never fires. Only
//!   the owner's messages are ever acted on, which is what makes
//!   the looser addressed forms safe.
//!
//! Composition: a stream's effective gain in the mix is
//! manual × duck envelope for music sources, manual alone
//! for humans (see pcm-protocol's Mixer). Manual gains are
//! runtime state for the current session only — the mixer
//! clears them at session end, and nothing is persisted.
//!
//! Pure functions over a candidate snapshot + the Mixer, so
//! everything here unit-tests without a bot connection.

use pcm_protocol::{Mixer, Side};

/// One mixer source as the chat layer sees it.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub side: Side,
    pub id: u64,
    /// Display name for replies ("MusicBot", "Discord 123456").
    pub label: String,
    /// Lowercase haystack for `!vol` name matching: the
    /// label plus the raw identity (full npub / user id),
    /// so a stream can be matched by any unique fragment.
    pub haystack: String,
    /// The token a conversational message must contain to
    /// count as naming this stream (lowercase label, or the
    /// Discord id's last 6 digits).
    pub token: String,
}

/// Build the candidate snapshot from the mixer's live
/// sources and the daemon's name registry, sorted by label
/// so list output is stable.
pub fn build_candidates(mixer: &Mixer, names: &std::collections::HashMap<(Side, u64), Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = mixer
        .sources()
        .into_iter()
        .map(|key| {
            names.get(&key).cloned().unwrap_or_else(|| Candidate {
                side: key.0,
                id: key.1,
                label: match key.0 {
                    Side::Armada => format!("armada-{:x}", key.1),
                    Side::Discord => format!("discord-{}", key.1),
                },
                haystack: format!("{}", key.1),
                token: format!("{}", key.1),
            })
        })
        .collect();
    out.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    out
}

/// What the chat layer should do with a message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VolOutcome {
    /// Not a volume command — the caller carries on with
    /// its normal (summon/leave) handling.
    NotVolume,
    /// A volume command from someone who is not the owner:
    /// swallow it whole. No reply, no state change.
    Silent,
    /// Send this one line back to the room.
    Reply(String),
}

/// A resolved volume action (resolution happens against the
/// candidate snapshot; execution then only touches gains).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Action {
    List,
    Set { label: String, side: Side, id: u64, pct: u32 },
    Relative { label: String, side: Side, id: u64, delta: i32 },
    ResetOne { label: String, side: Side, id: u64 },
    ResetAll,
    Ambiguous { labels: Vec<String> },
    UnknownName { query: String },
    NoTarget,
    BadPct,
    Usage,
}

/// Handle one chat message. `is_owner` is the caller's
/// npub check — the ONLY authorization this feature has.
pub fn handle_vol_message(
    mixer: &mut Mixer,
    candidates: &[Candidate],
    is_owner: bool,
    text: &str,
) -> VolOutcome {
    let Some(action) = parse(text, candidates) else {
        return VolOutcome::NotVolume;
    };
    if !is_owner {
        return VolOutcome::Silent;
    }
    VolOutcome::Reply(execute(mixer, candidates, action))
}

fn pct_of(mixer: &Mixer, side: Side, id: u64) -> u32 {
    (mixer.manual_gain(side, id) * 100.0).round() as u32
}

fn labels_of(candidates: &[Candidate]) -> Vec<String> {
    candidates.iter().map(|c| c.label.clone()).collect()
}

/// the owner's standing guidance, carried in the bot's own
/// text on the list (and usage) replies — deliberately NOT
/// appended to change replies, whose one-line
/// before → after format stays clean.
const GUIDANCE: &str = "Please only adjust if someone's level is really out of whack.";

fn list_line(mixer: &Mixer, candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return "No streams are being mixed right now.".to_string();
    }
    let parts: Vec<String> = candidates
        .iter()
        .map(|c| format!("{} {}%", c.label, pct_of(mixer, c.side, c.id)))
        .collect();
    format!("Streams: {}\n{GUIDANCE}", parts.join(" · "))
}

fn which_line(candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return "No streams are being mixed right now.".to_string();
    }
    format!("Which stream? {}", labels_of(candidates).join(" · "))
}

fn execute(mixer: &mut Mixer, candidates: &[Candidate], action: Action) -> String {
    match action {
        Action::List => list_line(mixer, candidates),
        Action::Set { label, side, id, pct } => {
            let old = pct_of(mixer, side, id);
            mixer.set_manual_gain(side, id, pct as f32 / 100.0);
            format!("{label} stream: {old}% → {pct}%")
        }
        Action::Relative { label, side, id, delta } => {
            let old = pct_of(mixer, side, id) as i32;
            let new = (old + delta).clamp(0, 150) as u32;
            mixer.set_manual_gain(side, id, new as f32 / 100.0);
            format!("{label} stream: {old}% → {new}%")
        }
        Action::ResetOne { label, side, id } => {
            let old = pct_of(mixer, side, id);
            mixer.reset_manual_gain(side, id);
            format!("{label} stream: {old}% → 100%")
        }
        Action::ResetAll => {
            mixer.reset_all_manual_gains();
            "All streams reset to 100%.".to_string()
        }
        Action::Ambiguous { labels } => format!("Which stream? {}", labels.join(" · ")),
        Action::UnknownName { query } => {
            if candidates.is_empty() {
                "No streams are being mixed right now.".to_string()
            } else {
                format!("No stream matches \"{query}\". Streams: {}", labels_of(candidates).join(" · "))
            }
        }
        Action::NoTarget => which_line(candidates),
        Action::BadPct => "Volume must be between 0 and 150.".to_string(),
        Action::Usage => {
            format!("Usage: !vol <name> <0–150>, !vol <name> reset, !vol reset, !vols.\n{GUIDANCE}")
        }
    }
}

// ---------------------------------------------------------------------------
// Name matching
// ---------------------------------------------------------------------------

enum Match {
    One(usize),
    Ambiguous { labels: Vec<String> },
    Unknown,
}

/// Match a `!vol` name argument against the candidates:
/// exact label first, then substring over the haystack
/// (label + raw identity). Case-insensitive throughout.
fn match_name(query: &str, candidates: &[Candidate]) -> Match {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Match::Unknown;
    }
    let exact: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.label.to_lowercase() == q)
        .map(|(i, _)| i)
        .collect();
    if exact.len() == 1 {
        return Match::One(exact[0]);
    }
    if exact.len() > 1 {
        return Match::Ambiguous { labels: exact.iter().map(|&i| candidates[i].label.clone()).collect() };
    }
    let sub: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.haystack.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect();
    match sub.len() {
        1 => Match::One(sub[0]),
        0 => Match::Unknown,
        _ => Match::Ambiguous { labels: sub.iter().map(|&i| candidates[i].label.clone()).collect() },
    }
}

/// Find the conversational target: candidates whose token
/// appears as a whole word in the message.
fn match_tokens(tokens: &[String], candidates: &[Candidate]) -> Match {
    let hits: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| tokens.iter().any(|t| t == &c.token))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        1 => Match::One(hits[0]),
        0 => Match::Unknown,
        _ => Match::Ambiguous { labels: hits.iter().map(|&i| candidates[i].label.clone()).collect() },
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse(text: &str, candidates: &[Candidate]) -> Option<Action> {
    let trimmed = text.trim();
    if trimmed.starts_with('!') {
        return parse_bang(trimmed, candidates);
    }
    parse_conversational(trimmed, candidates)
}

fn parse_bang(text: &str, candidates: &[Candidate]) -> Option<Action> {
    let lower = text.to_lowercase();
    if lower == "!vols" || lower == "!vol" {
        return Some(Action::List);
    }
    let rest = lower.strip_prefix("!vol ")?.trim().to_string();
    if rest.is_empty() {
        return Some(Action::List);
    }
    if rest == "reset" {
        return Some(Action::ResetAll);
    }
    // The name is everything before the last token (stream
    // labels can be two words: "Discord 123456").
    let (name, last) = match rest.rsplit_once(' ') {
        Some((n, l)) => (n.trim().to_string(), l.trim().to_string()),
        None => (String::new(), rest.clone()),
    };
    if last == "reset" {
        if name.is_empty() {
            return Some(Action::ResetAll);
        }
        return Some(match match_name(&name, candidates) {
            Match::One(i) => {
                let c = &candidates[i];
                Action::ResetOne { label: c.label.clone(), side: c.side, id: c.id }
            }
            Match::Ambiguous { labels } => Action::Ambiguous { labels },
            Match::Unknown => Action::UnknownName { query: name },
        });
    }
    let pct_str = last.strip_suffix('%').unwrap_or(&last);
    if let Ok(pct) = pct_str.parse::<u32>() {
        if pct > 150 {
            return Some(Action::BadPct);
        }
        if name.is_empty() {
            return Some(Action::Usage);
        }
        return Some(match match_name(&name, candidates) {
            Match::One(i) => {
                let c = &candidates[i];
                Action::Set { label: c.label.clone(), side: c.side, id: c.id, pct }
            }
            Match::Ambiguous { labels } => Action::Ambiguous { labels },
            Match::Unknown => Action::UnknownName { query: name },
        });
    }
    Some(Action::Usage)
}

/// Word tokens, lowercased (same tokenization discipline
/// as converse.rs: runs of alphanumeric chars).
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch.to_lowercase().next().unwrap_or(ch));
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_conversational(text: &str, candidates: &[Candidate]) -> Option<Action> {
    let tokens = tokenize(text);
    if tokens.is_empty() {
        return None;
    }
    let has = |w: &str| tokens.iter().any(|t| t == w);
    let addressed = has("janus");
    let volume_word = has("volume") || has("volumes") || has("vol");

    // List: "list volumes", "what are the volumes",
    // "volumes?" — the plural keeps "what is the volume of
    // the tv" from firing.
    if (has("list") && volume_word)
        || (tokens[0] == "what" && has("volumes"))
        || (tokens.len() == 1 && tokens[0] == "volumes")
    {
        return Some(Action::List);
    }

    // Reset: "reset volumes", "volumes back to normal",
    // "reset <name>".
    if has("reset") && volume_word {
        return Some(Action::ResetAll);
    }
    if has("volumes") && has("back") && has("normal") {
        return Some(Action::ResetAll);
    }
    if has("reset") {
        return Some(match match_tokens(&tokens, candidates) {
            Match::One(i) => {
                let c = &candidates[i];
                Action::ResetOne { label: c.label.clone(), side: c.side, id: c.id }
            }
            Match::Ambiguous { labels } => Action::Ambiguous { labels },
            // "i reset my router" is ordinary chat: a
            // targetless reset only counts as a command
            // when Janus is addressed.
            Match::Unknown => {
                if addressed {
                    Action::NoTarget
                } else {
                    return None;
                }
            }
        });
    }

    // Relative: "turn down MusicBot", "turn up <name>", "make
    // <name> louder/quieter". ±20 points, ±10 for
    // "a little / a bit".
    let down = has("quieter") || (has("turn") && has("down"));
    let up = has("louder") || (has("turn") && has("up"));
    if down || up {
        let step = if has("little") || has("bit") { 10 } else { 20 };
        let delta = if down { -step } else { step };
        return Some(match match_tokens(&tokens, candidates) {
            Match::One(i) => {
                let c = &candidates[i];
                Action::Relative { label: c.label.clone(), side: c.side, id: c.id, delta }
            }
            Match::Ambiguous { labels } => Action::Ambiguous { labels },
            // Bare and targetless ("turn down the music")
            // is ordinary chat; addressed but targetless
            // gets the which-stream reply.
            Match::Unknown => {
                if addressed {
                    Action::NoTarget
                } else {
                    return None;
                }
            }
        });
    }

    // Absolute: a 0–150 number plus a volume signal
    // (volume word or set/make) plus a named target.
    // "MusicBot volume 50", "make MusicBot 50%", "janus set MusicBot
    // to 50". Being addressed ALONE is not a signal —
    // "janus join VC 2" carries a number and an address
    // and is a summon, not a volume command.
    let number: Option<u32> = tokens.iter().find_map(|t| {
        if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
            t.parse::<u32>().ok().filter(|n| *n <= 150)
        } else {
            None
        }
    });
    if let Some(pct) = number {
        if volume_word || has("set") || has("make") {
            return Some(match match_tokens(&tokens, candidates) {
                Match::One(i) => {
                    let c = &candidates[i];
                    Action::Set { label: c.label.clone(), side: c.side, id: c.id, pct }
                }
                Match::Ambiguous { labels } => Action::Ambiguous { labels },
                Match::Unknown => {
                    if addressed {
                        Action::NoTarget
                    } else {
                        return None;
                    }
                }
            });
        }
        return None;
    }

    // Addressed + volume word but no actionable intent:
    // show the current state ("janus, the volumes?").
    if addressed && volume_word {
        return Some(Action::List);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcm_protocol::Side;

    const MUSICBOT_ID: u64 = 0x1CE5;
    const VOICEBOT_ID: u64 = 0xC4415;
    const DISCO_ID: u64 = 123_456_789_012_345_678;

    fn candidates() -> Vec<Candidate> {
        // In build_candidates' label-sorted order, as the
        // production path hands them over.
        vec![
            Candidate {
                side: Side::Armada,
                id: VOICEBOT_ID,
                label: "VoiceBot".into(),
                haystack: "voicebot npub1voicebot".into(),
                token: "voicebot".into(),
            },
            Candidate {
                side: Side::Discord,
                id: DISCO_ID,
                label: "Discord 345678".into(),
                haystack: "discord 345678 123456789012345678".into(),
                token: "345678".into(),
            },
            Candidate {
                side: Side::Armada,
                id: MUSICBOT_ID,
                label: "MusicBot".into(),
                haystack: "musicbot npub1musicbot".into(),
                token: "musicbot".into(),
            },
        ]
    }

    fn mixer_with(cands: &[Candidate]) -> Mixer {
        let mut m = Mixer::new();
        for c in cands {
            for _ in 0..3 {
                m.push(c.side, c.id, vec![1000i16; pcm_protocol::FRAME_SAMPLES]);
            }
        }
        m
    }

    fn run(text: &str, owner: bool) -> (VolOutcome, Mixer) {
        let cands = candidates();
        let mut m = mixer_with(&cands);
        let out = handle_vol_message(&mut m, &cands, owner, text);
        (out, m)
    }

    // -- bang forms ------------------------------------------

    #[test]
    fn bang_set_and_reply() {
        let (out, m) = run("!vol MusicBot 60", true);
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 100% → 60%".into()));
        assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 0.6);
    }

    #[test]
    fn bang_set_by_partial_and_identity_fragment() {
        let (out, m) = run("!vol mus 40", true);
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 100% → 40%".into()));
        assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 0.4);
        // A fragment of the npub matches too.
        let (out, _) = run("!vol 1musicb 40", true);
        assert!(matches!(out, VolOutcome::Reply(_)));
        // Two-word Discord label, and bare id fragment.
        let (out, m) = run("!vol Discord 345678 150", true);
        assert_eq!(out, VolOutcome::Reply("Discord 345678 stream: 100% → 150%".into()));
        assert_eq!(m.manual_gain(Side::Discord, DISCO_ID), 1.5);
    }

    #[test]
    fn bang_list_reset_and_reset_one() {
        let cands = candidates();
        let mut m = mixer_with(&cands);
        m.set_manual_gain(Side::Armada, MUSICBOT_ID, 0.5);
        m.set_manual_gain(Side::Armada, VOICEBOT_ID, 0.8);
        let out = handle_vol_message(&mut m, &cands, true, "!vols");
        assert_eq!(
            out,
            VolOutcome::Reply(
                "Streams: VoiceBot 80% · Discord 345678 100% · MusicBot 50%\nPlease only adjust if someone's level is really out of whack."
                    .into()
            )
        );
        let out = handle_vol_message(&mut m, &cands, true, "!vol MusicBot reset");
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 50% → 100%".into()));
        assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 1.0);
        assert_eq!(m.manual_gain(Side::Armada, VOICEBOT_ID), 0.8);
        let out = handle_vol_message(&mut m, &cands, true, "!vol reset");
        assert_eq!(out, VolOutcome::Reply("All streams reset to 100%.".into()));
        assert_eq!(m.manual_gain(Side::Armada, VOICEBOT_ID), 1.0);
    }

    #[test]
    fn bang_unknown_ambiguous_and_bad_pct() {
        let (out, _) = run("!vol Nobody 50", true);
        assert!(matches!(out, VolOutcome::Reply(ref s) if s.starts_with("No stream matches \"nobody\"")), "{out:?}");
        let (out, _) = run("!vol MusicBot 151", true);
        assert_eq!(out, VolOutcome::Reply("Volume must be between 0 and 150.".into()));
        // "c" is a fragment of both npubs → ambiguous.
        let (out, _) = run("!vol pub1 50", true);
        assert!(matches!(out, VolOutcome::Reply(ref s) if s.starts_with("Which stream?")), "{out:?}");
    }

    // -- owner gate ------------------------------------------

    #[test]
    fn non_owner_is_silently_ignored() {
        for text in ["!vol MusicBot 10", "!vols", "!vol reset", "janus set MusicBot to 10", "turn down MusicBot"] {
            let (out, m) = run(text, false);
            assert_eq!(out, VolOutcome::Silent, "non-owner: {text:?}");
            assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 1.0, "no state change: {text:?}");
        }
        // And a non-owner's ordinary chat is untouched.
        let (out, _) = run("nice song this", false);
        assert_eq!(out, VolOutcome::NotVolume);
    }

    // -- conversational forms --------------------------------

    #[test]
    fn conversational_absolute_forms() {
        for (text, pct) in [
            ("janus set MusicBot to 50", 0.5f32),
            ("@Janus set MusicBot to 50", 0.5),
            ("make MusicBot 50%", 0.5),
            ("MusicBot volume 50", 0.5),
            ("janus, make MusicBot 25", 0.25),
        ] {
            let (out, m) = run(text, true);
            assert!(matches!(out, VolOutcome::Reply(_)), "{text:?} -> {out:?}");
            assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), pct, "{text:?}");
        }
    }

    #[test]
    fn conversational_relative_steps() {
        let cands = candidates();
        let mut m = mixer_with(&cands);
        m.set_manual_gain(Side::Armada, MUSICBOT_ID, 0.8);
        let out = handle_vol_message(&mut m, &cands, true, "turn down MusicBot");
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 80% → 60%".into()));
        let out = handle_vol_message(&mut m, &cands, true, "make MusicBot quieter");
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 60% → 40%".into()));
        let out = handle_vol_message(&mut m, &cands, true, "turn MusicBot up a little");
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 40% → 50%".into()));
        let out = handle_vol_message(&mut m, &cands, true, "janus make VoiceBot louder");
        assert_eq!(out, VolOutcome::Reply("VoiceBot stream: 100% → 120%".into()));
        // Steps clamp at the 0–150 window.
        m.set_manual_gain(Side::Armada, MUSICBOT_ID, 1.45);
        let out = handle_vol_message(&mut m, &cands, true, "turn up MusicBot");
        assert_eq!(out, VolOutcome::Reply("MusicBot stream: 145% → 150%".into()));
    }

    #[test]
    fn conversational_list_and_reset() {
        let cands = candidates();
        let mut m = mixer_with(&cands);
        m.set_manual_gain(Side::Armada, MUSICBOT_ID, 0.5);
        for text in ["what are the volumes", "list volumes", "volumes?", "janus what are the volumes"] {
            let out = handle_vol_message(&mut m, &cands, true, text);
            assert!(
                matches!(out, VolOutcome::Reply(ref s) if s.starts_with("Streams:")),
                "{text:?} -> {out:?}"
            );
        }
        let out = handle_vol_message(&mut m, &cands, true, "reset volumes");
        assert_eq!(out, VolOutcome::Reply("All streams reset to 100%.".into()));
        assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 1.0);
        m.set_manual_gain(Side::Armada, MUSICBOT_ID, 0.5);
        let out = handle_vol_message(&mut m, &cands, true, "volumes back to normal");
        assert_eq!(out, VolOutcome::Reply("All streams reset to 100%.".into()));
    }

    #[test]
    fn conversational_no_target_asks_instead_of_guessing() {
        let (out, m) = run("janus turn it down", true);
        assert!(matches!(out, VolOutcome::Reply(ref s) if s.starts_with("Which stream?")), "{out:?}");
        assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 1.0);
    }

    #[test]
    fn ordinary_chat_never_fires() {
        for text in [
            "MusicBot is great",
            "i love this song MusicBot is playing",
            "janus is cool",
            "what did MusicBot play earlier",
            "MusicBot got 50 points in the game",
            "VoiceBot is loud today",
            "turn down the music",
            "the volume on my tv is broken",
            "what is the volume of the tv",
            "someone set the oven to 50",
            "janus join",
            "janus join VC 2",
            "janus bridge to General",
            "i reset my router",
            "make sure MusicBot comes to the raid",
        ] {
            let (out, m) = run(text, true);
            assert_eq!(out, VolOutcome::NotVolume, "must not fire: {text:?}");
            assert_eq!(m.manual_gain(Side::Armada, MUSICBOT_ID), 1.0);
        }
    }
}

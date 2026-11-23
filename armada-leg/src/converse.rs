//! Conversational summon/leave triggers (the owner's order,
//! 2026-11-22: "the command for janus to join and leave
//! should be conversational").
//!
//! The owner's standing rule (inherited from the reference bot's conventions):
//! conversational-first for every feature — `!summon` /
//! `!leave` are backstops only — and ordinary conversation
//! must NEVER misfire a detector. This matcher is therefore
//! deliberately TIGHT. A message fires only when ALL of
//! these hold:
//!
//!   1. It names Janus: the token "janus" appears as a whole
//!      word (case-insensitive). "janus is cool" names him
//!      but has no intent (rule 2); "everyone join vc" has
//!      an intent verb but never names him — neither fires.
//!   2. A join or leave intent pattern matches (the exact
//!      sets below — nothing else is an intent).
//!   3. It is not a question: a message containing '?' or
//!      starting with an interrogative word (what/did/is/
//!      can/…) never fires. People talk ABOUT the bridge;
//!      Janus answers requests, not questions about himself.
//!
//! There is deliberately NO conversational status trigger:
//! `!bridge status` stays owner-only and command-only.
//!
//! Authorization is NOT this module's business: callers run
//! the same SUMMON_ACCESS check (auth.rs) as the command
//! path, so open/whitelist behavior is identical.
//!
//! Pure functions so they unit-test without a bot
//! connection. Mirrors discord-leg/src/converse.mjs — kept
//! in lockstep (same patterns, same tests).

/// A conversational command extracted from a chat message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConverseCommand {
    /// Summon the bridge. `named` is the far-side (Discord)
    /// channel name spoken in the message, if any — extracted
    /// exactly like `!summon [name]`'s argument.
    Join { named: Option<String> },
    /// End the bridge session.
    Leave,
}

/// Filler words a pattern's `F` slot may absorb between the
/// meaningful words: "janus please join", "join us janus",
/// "stop the bridge". Deliberately small, and prepositions
/// that can carry meaning (in/to/on/up) are NOT fillers —
/// they appear as literals in patterns or in name cleanup.
/// ("to" is excluded on purpose: it keeps relayed asks like
/// "tell janus to join" from firing.)
const FILLERS: &[&str] = &["us", "the", "please", "now", "over", "here"];

/// Interrogative openers: a message starting with one of
/// these is a question, not a request — never fires.
const QUESTION_STARTERS: &[&str] = &[
    "what", "why", "when", "where", "who", "how", "did", "does", "do", "is", "was", "were",
    "are", "can", "could", "would", "should", "will", "has", "had",
];

/// One pattern element: a literal word, or `F` = zero or
/// more FILLERS words.
#[derive(Clone, Copy)]
enum P {
    W(&'static str),
    F,
}
use P::*;

/// JOIN patterns — the complete join verb set:
/// join, bridge (as a verb), connect, "start the bridge",
/// "hop in", and the "get janus in (here)" idiom. Anything
/// else is conversation, not a summon.
const JOIN_PATTERNS: &[&[P]] = &[
    &[W("janus"), F, W("join")],
    &[W("join"), F, W("janus")],
    &[W("janus"), F, W("bridge")],
    &[W("bridge"), F, W("janus")],
    &[W("janus"), F, W("connect")],
    &[W("janus"), W("start"), F, W("bridge")],
    &[W("janus"), W("hop"), W("in")],
    &[W("get"), W("janus"), W("in")],
];

/// LEAVE patterns — the complete leave verb set, kept
/// tight on purpose: leave, stop, disconnect, "hang up",
/// "go away", plus "stop the bridge" (Janus is named
/// elsewhere in the message; rule 1 still applies).
/// Checked BEFORE the join patterns, so an explicit stop
/// always wins over any join word in the same message.
const LEAVE_PATTERNS: &[&[P]] = &[
    &[W("janus"), F, W("leave")],
    &[W("janus"), F, W("stop")],
    &[W("stop"), F, W("bridge")],
    &[W("janus"), F, W("disconnect")],
    &[W("janus"), F, W("hang"), W("up")],
    &[W("janus"), W("go"), W("away")],
];

/// Split into word tokens (runs of alphanumeric chars),
/// keeping the original casing for channel-name extraction.
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Match `pat` against `lower` starting at token `start`;
/// returns the token index just past the match.
fn match_at(lower: &[String], pat: &[P], start: usize) -> Option<usize> {
    fn rec(lower: &[String], pat: &[P], ti: usize, pi: usize) -> Option<usize> {
        if pi == pat.len() {
            return Some(ti);
        }
        match pat[pi] {
            W(word) => {
                if ti < lower.len() && lower[ti] == word {
                    rec(lower, pat, ti + 1, pi + 1)
                } else {
                    None
                }
            }
            F => {
                // Zero or more filler words: try the rest of
                // the pattern at each absorption count.
                let mut end = ti;
                loop {
                    if let Some(r) = rec(lower, pat, end, pi + 1) {
                        return Some(r);
                    }
                    if end < lower.len() && FILLERS.contains(&lower[end].as_str()) {
                        end += 1;
                    } else {
                        return None;
                    }
                }
            }
        }
    }
    rec(lower, pat, start, 0)
}

/// First match of `pat` anywhere in the message; returns
/// the token index just past the match (name extraction
/// starts there).
fn find_pattern(lower: &[String], pat: &[P]) -> Option<usize> {
    (0..lower.len()).find_map(|start| match_at(lower, pat, start))
}

/// The far-side channel name spoken after a join pattern:
/// the tokens past the match, minus leading connectives
/// ("to General", "the VC 2", "channel VC 2") and trailing
/// filler ("join us", "join please"). Original casing is
/// preserved — landing resolution matches names
/// case-insensitively, same as `!summon`'s argument.
fn extract_named(orig: &[String], lower: &[String], from: usize) -> Option<String> {
    const LEADING: &[&str] = &["to", "into", "in", "on", "the", "channel"];
    let mut lo = from;
    let mut hi = lower.len();
    while lo < hi && (LEADING.contains(&lower[lo].as_str()) || FILLERS.contains(&lower[lo].as_str())) {
        lo += 1;
    }
    while hi > lo && FILLERS.contains(&lower[hi - 1].as_str()) {
        hi -= 1;
    }
    if lo >= hi {
        None
    } else {
        Some(orig[lo..hi].join(" "))
    }
}

/// Parse a chat message into a conversational command, or
/// None when it is ordinary conversation (the overwhelming
/// majority — see the module docs for the firing rules).
pub fn parse_conversational(text: &str) -> Option<ConverseCommand> {
    // Rule 3a: questions never fire.
    if text.contains('?') {
        return None;
    }
    let orig = tokenize(text);
    if orig.is_empty() {
        return None;
    }
    let lower: Vec<String> = orig.iter().map(|t| t.to_lowercase()).collect();
    // Rule 3b: interrogative openers never fire.
    if QUESTION_STARTERS.contains(&lower[0].as_str()) {
        return None;
    }
    // Rule 1: Janus must be named, as a whole word.
    if !lower.iter().any(|t| t == "janus") {
        return None;
    }
    // Rule 2: an intent pattern must match. Leave first —
    // an explicit stop always wins.
    for pat in LEAVE_PATTERNS {
        if find_pattern(&lower, pat).is_some() {
            return Some(ConverseCommand::Leave);
        }
    }
    for pat in JOIN_PATTERNS {
        if let Some(end) = find_pattern(&lower, pat) {
            return Some(ConverseCommand::Join { named: extract_named(&orig, &lower, end) });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(text: &str) -> Option<ConverseCommand> {
        parse_conversational(text)
    }

    #[test]
    fn join_positives_from_owner_examples() {
        assert_eq!(join("janus join"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("join us janus"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("janus bridge"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("bridge us janus"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("janus connect"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("janus start the bridge"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("have janus join"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("get janus in here"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("janus hop in"), Some(ConverseCommand::Join { named: None }));
    }

    #[test]
    fn join_positives_natural_variations() {
        assert_eq!(join("Janus, join!"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("hey janus please join"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("please janus bridge"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("JANUS CONNECT"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("janus join us"), Some(ConverseCommand::Join { named: None }));
    }

    #[test]
    fn join_extracts_the_named_channel_like_bang_summon() {
        assert_eq!(
            join("janus join VC 2"),
            Some(ConverseCommand::Join { named: Some("VC 2".into()) })
        );
        assert_eq!(
            join("janus bridge to General"),
            Some(ConverseCommand::Join { named: Some("General".into()) })
        );
        assert_eq!(
            join("janus join the VC 2"),
            Some(ConverseCommand::Join { named: Some("VC 2".into()) })
        );
        assert_eq!(
            join("have janus join Raid Night"),
            Some(ConverseCommand::Join { named: Some("Raid Night".into()) })
        );
        // Filler-only remainders are not channel names.
        assert_eq!(join("janus join please"), Some(ConverseCommand::Join { named: None }));
        assert_eq!(join("get janus in here"), Some(ConverseCommand::Join { named: None }));
    }

    #[test]
    fn leave_positives_from_owner_examples() {
        assert_eq!(join("janus leave"), Some(ConverseCommand::Leave));
        assert_eq!(join("janus stop"), Some(ConverseCommand::Leave));
        assert_eq!(join("janus disconnect"), Some(ConverseCommand::Leave));
        assert_eq!(join("janus hang up"), Some(ConverseCommand::Leave));
        assert_eq!(join("stop the bridge janus"), Some(ConverseCommand::Leave));
        assert_eq!(join("janus go away"), Some(ConverseCommand::Leave));
    }

    #[test]
    fn leave_wins_over_join_words_in_the_same_message() {
        assert_eq!(join("janus stop the bridge"), Some(ConverseCommand::Leave));
    }

    #[test]
    fn negatives_from_owner_list_never_fire() {
        for text in [
            "janus is cool",
            "what did janus say",
            "i love janus",
            "janus",
            "everyone join vc",
            "the bridge was great yesterday",
        ] {
            assert_eq!(parse_conversational(text), None, "must not fire: {text:?}");
        }
    }

    #[test]
    fn questions_about_janus_never_fire() {
        for text in [
            "did janus join the call earlier?",
            "janus join?",
            "can janus join us",
            "is janus in vc",
            "what channel did janus bridge to",
        ] {
            assert_eq!(parse_conversational(text), None, "must not fire: {text:?}");
        }
    }

    #[test]
    fn near_misses_never_fire() {
        for text in [
            "janus joined vc earlier",     // past tense is a report, not a request
            "janus is joining soon",        // likewise
            "janusx join",                  // not the whole word "janus"
            "someone tell janus to join",   // relayed ask — not in the tight set
            "join the vc everyone",         // verb, no janus
            "bridge night was fun",         // "bridge" as a noun, no janus
            "janus start",                  // incomplete pattern
            "janus is the bridge",          // "bridge" as a noun, "is" breaks the pattern
            "",
            "   ",
        ] {
            assert_eq!(parse_conversational(text), None, "must not fire: {text:?}");
        }
    }
}

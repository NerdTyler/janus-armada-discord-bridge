//! Identity mapping learned from CORD-07 §4 presence.
//!
//! Presence binds a member (npub, the rumor's author) to the
//! SFU identity their client was assigned. The presence
//! observer (see presence.rs) folds every `joined` rumor's
//! `identity` tag into [`IdentityMap`], so a LiveKit track's
//! identity resolves to the member who owns it. The §4
//! conflict rule applies: an identity claimed by two authors'
//! fresh presence is claimed by NEITHER (a copied identity
//! proves nothing), and claims go stale with the presence
//! itself (90 s freshness + slack).
//!
//! (In the reference bot's tree this module also carried an utterance
//! dedupe for his dual-transport transcription pipeline; the
//! bridge has no second transport and no transcripts, so only
//! the identity map is lifted.)

use std::collections::HashMap;

/// How long a presence claim stays believable after its last
/// heartbeat: the spec's 90 s staleness, plus slack for the
/// observer's fetch cadence.
pub const CLAIM_TTL_MS: u64 = 150_000;

/// member npub ↔ SFU identity, learned from presence.
pub struct IdentityMap {
    /// identity → (npub, last_seen_ms). A contested identity is
    /// simply absent (removed on the second claim).
    by_identity: HashMap<String, (String, u64)>,
    /// npub → identity (reverse view for tests/diagnostics).
    by_npub: HashMap<String, String>,
    /// identity → set of claimant npubs whose claims are fresh;
    /// ≥2 claimants = contested = unresolvable.
    claimants: HashMap<String, HashMap<String, u64>>,
}

impl IdentityMap {
    pub fn new() -> Self {
        Self {
            by_identity: HashMap::new(),
            by_npub: HashMap::new(),
            claimants: HashMap::new(),
        }
    }

    /// Fold one presence observation: `npub` claims `identity`
    /// at `at_ms` (their heartbeat time / our receive time).
    pub fn observe(&mut self, npub: &str, identity: &str, at_ms: u64) {
        let claims = self.claimants.entry(identity.to_string()).or_default();
        // Fetches can surface heartbeats out of order; a claim
        // never moves backwards.
        let entry = claims.entry(npub.to_string()).or_insert(at_ms);
        *entry = (*entry).max(at_ms);
        self.refresh(identity, at_ms);
    }

    /// The author stopped claiming (a `left`, or the claim aged
    /// out elsewhere): drop their claim.
    pub fn release(&mut self, npub: &str, identity: &str) {
        if let Some(claims) = self.claimants.get_mut(identity) {
            claims.remove(npub);
        }
        self.refresh(identity, 0);
    }

    fn refresh(&mut self, identity: &str, now_ms: u64) {
        let fresh: Vec<(String, u64)> = self
            .claimants
            .get(identity)
            .map(|claims| {
                claims
                    .iter()
                    .filter(|(_, seen)| now_ms.saturating_sub(**seen) <= CLAIM_TTL_MS || **seen >= now_ms)
                    .map(|(n, s)| (n.clone(), *s))
                    .collect()
            })
            .unwrap_or_default();
        // Retire any reverse mapping this identity used to hold.
        if let Some((old_npub, _)) = self.by_identity.remove(identity) {
            if self.by_npub.get(&old_npub).map(|i| i == identity).unwrap_or(false) {
                self.by_npub.remove(&old_npub);
            }
        }
        if fresh.len() == 1 {
            let (npub, seen) = fresh.into_iter().next().unwrap();
            self.by_identity.insert(identity.to_string(), (npub.clone(), seen));
            self.by_npub.insert(npub, identity.to_string());
        }
        // 0 fresh claims → gone; ≥2 → contested, unresolvable.
    }

    /// Resolve an SFU identity to its member npub, when exactly
    /// one member's fresh presence claims it.
    pub fn npub_of(&self, identity: &str, now_ms: u64) -> Option<String> {
        let (npub, seen) = self.by_identity.get(identity)?;
        if now_ms.saturating_sub(*seen) > CLAIM_TTL_MS {
            return None;
        }
        Some(npub.clone())
    }

    /// The SFU identity a member is currently mapped to, if any.
    pub fn identity_of(&self, npub: &str, now_ms: u64) -> Option<String> {
        let identity = self.by_npub.get(npub)?;
        self.npub_of(identity, now_ms)?;
        Some(identity.clone())
    }

    /// Every member npub with at least one fresh presence
    /// claim — the channel's occupancy roster. (The bridge's
    /// landing resolution and summon-side emptiness checks run
    /// on this; the reference bot never needed the roster itself.)
    pub fn fresh_members(&self, now_ms: u64) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for claims in self.claimants.values() {
            for (npub, seen) in claims {
                if now_ms.saturating_sub(*seen) <= CLAIM_TTL_MS && !out.contains(npub) {
                    out.push(npub.clone());
                }
            }
        }
        out
    }
}

impl Default for IdentityMap {
    fn default() -> Self {
        Self::new()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_members_reflects_claims_and_ttl() {
        let mut map = IdentityMap::new();
        map.observe("npub-alice", "id-a", 1_000);
        map.observe("npub-bob", "id-b", 1_000);
        let mut members = map.fresh_members(2_000);
        members.sort();
        assert_eq!(members, vec!["npub-alice".to_string(), "npub-bob".to_string()]);
        // Past the TTL both claims age out.
        assert!(map.fresh_members(1_000 + CLAIM_TTL_MS + 1).is_empty());
        // A release removes the member immediately (Bob's
        // claim, made at 1_000, is still fresh at 5_000).
        map.observe("npub-alice", "id-a", 5_000);
        map.release("npub-alice", "id-a");
        assert_eq!(map.fresh_members(5_000), vec!["npub-bob".to_string()]);
    }
}

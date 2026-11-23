//! CORD-07 key resolution for one voice channel, from the reference bot's
//! EXISTING community membership.
//!
//! Step-zero verdict (researched 2026-11-22, verified against the
//! vendored sources): the keys ARE reachable from bot code.
//! `vector_sdk` re-exports the whole `vector_core` crate;
//! `community::v2::realtime::load_held_v2()` hands back the held
//! `CommunityV2`s from this process's own store; and on those,
//! `CommunityV2::channel_secret` plus the `community::v2::derive`
//! functions (`voice_group_key`, `voice_media_key`,
//! `voice_sender_key`) are all `pub`. No separate Concord membership
//! module is needed — the LiveKit leg rides the membership the
//! bridge already has. The derivations themselves are vector-core's own
//! (frozen by CORD-02 App. A / CORD-07 §§1+3); the unit tests below
//! pin them against Armada's published reference vectors so a
//! future SDK bump that drifts a derivation fails loudly here
//! instead of silently producing undecryptable audio.

use anyhow::{anyhow, Result};
use nostr_sdk::prelude::Keys;
use vector_sdk::vector_core::community::v2::community::{ChannelView, CommunityV2};
use vector_sdk::vector_core::community::v2::derive::{self, GroupKey};
use vector_sdk::vector_core::community::v2::realtime::load_held_v2;
use vector_sdk::vector_core::community::{ChannelId, Epoch};

/// Everything the LiveKit leg needs for one voice channel, derived
/// from the channel's current secret + epoch. Re-resolve after a
/// rekey: a new epoch means a new room name and new media keys.
pub struct ChannelKeys {
    /// The held community that owns the channel (kept for its relay
    /// list and for sealing presence on its chat plane).
    pub community: CommunityV2,
    pub channel_id: ChannelId,
    pub epoch: u64,
    /// The channel secret the derivations fed on. Never logged.
    pub secret: [u8; 32],
    /// CORD-07 §1 voice signer keypair: `pk` IS the SFU room name,
    /// `sk` signs broker grant events (never the reference bot's own key).
    pub voice_keys: Keys,
    /// The SFU room name (voice pubkey, 64-char lowercase hex).
    pub room: String,
    /// CORD-07 §1 media root; per-sender keys derive from it.
    pub media_key: [u8; 32],
    /// The channel's chat-plane group key (presence wraps seal to
    /// this; presence fetches filter on its address).
    pub chat_group: GroupKey,
}

impl ChannelKeys {
    /// Derive the full CORD-07 key set from a channel secret +
    /// epoch, given the held community the channel belongs to.
    /// [`resolve_channel_keys`] is this plus the held-community
    /// lookup; a caller that obtained the secret through its own
    /// membership flow (the bridge) can construct keys directly.
    pub fn derive(community: CommunityV2, channel_id: ChannelId, secret: [u8; 32], epoch: Epoch) -> Self {
        let voice = derive::voice_group_key(&secret, &channel_id, epoch);
        Self {
            room: voice.pk_hex(),
            voice_keys: voice.keys().clone(),
            media_key: derive::voice_media_key(&secret, &channel_id, epoch),
            chat_group: derive::channel_group_key(&secret, &channel_id, epoch),
            community,
            channel_id,
            epoch: epoch.0,
            secret,
        }
    }
}

/// Resolve the CORD-07 keys for a voice channel (hex id, as the
/// SDK names channels) from the communities this process holds.
pub fn resolve_channel_keys(voice_channel_id_hex: &str) -> Result<ChannelKeys> {
    let raw = hex::decode(voice_channel_id_hex.trim())
        .map_err(|_| anyhow!("voice channel id is not hex: {voice_channel_id_hex}"))?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow!("voice channel id is not 32 bytes: {voice_channel_id_hex}"))?;
    let channel_id = ChannelId(bytes);
    for community in load_held_v2() {
        let Some(ch) = community.channel(&channel_id) else { continue };
        let (secret, epoch) = community.channel_secret(ch);
        return Ok(ChannelKeys::derive(community, channel_id, secret, epoch));
    }
    Err(anyhow!(
        "no held community contains voice channel {voice_channel_id_hex}"
    ))
}

/// The channel's native view, read from held state: `Some(true)`
/// when vector-core sees a Voice channel, `Some(false)` when it
/// sees any other view, `None` when no held community contains
/// the channel (or the id isn't a 32-byte channel id at all).
///
/// This mirrors the hard gate in vector-core 0.11.0's native call
/// stack (calls/voice.rs, `join_with`): `ch.view() !=
/// ChannelView::Voice` → the join fails with "That isn't a voice
/// channel". Discord-imported voice channels (the owner's server)
/// carry a non-Voice native view, so the native stack can NEVER
/// join them — but the call still exists as a LiveKit SFU room,
/// and everything the LiveKit leg needs derives from the channel
/// secret + id, which resolve view-agnostically
/// ([`resolve_channel_keys`] never consults the view — it finds
/// the channel by id in the held communities and derives). So
/// the caller can predict the refusal from this predicate and
/// host the call LiveKit-only instead of eating the error.
pub fn channel_native_voice(voice_channel_id_hex: &str) -> Option<bool> {
    let raw = hex::decode(voice_channel_id_hex.trim()).ok()?;
    let bytes: [u8; 32] = raw.try_into().ok()?;
    let channel_id = ChannelId(bytes);
    for community in load_held_v2() {
        if let Some(ch) = community.channel(&channel_id) {
            return Some(ch.view() == ChannelView::Voice);
        }
    }
    None
}

/// The per-sender key material for one SFU identity under a media
/// key (CORD-07 §3, vector-core's frozen derivation).
pub fn sender_key(media_key: &[u8; 32], identity: &str) -> [u8; 32] {
    derive::voice_sender_key(media_key, identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_sdk::vector_core::community::Epoch;

    // Armada's frozen reference vectors (CORD-02 App. A / CORD-07),
    // as published with the reference derivations: secret = [1;32],
    // id = [2;32]. These pin vector-core's derive functions — the
    // exact ones resolve_channel_keys calls — byte for byte.
    const A: [u8; 32] = [1u8; 32];
    const B: [u8; 32] = [2u8; 32];

    #[test]
    fn voice_group_key_matches_armada_vectors() {
        let vg = derive::voice_group_key(&A, &ChannelId(B), Epoch(0));
        assert_eq!(
            vg.keys().secret_key().to_secret_hex(),
            "89695aad5d1e8f39e9520e9ff002c479e9bae18e1d0e4e061c01ba119d07d451"
        );
        assert_eq!(
            vg.pk_hex(),
            "1e1f2f696f2885a85b02137a1c227a360aa3f40f2d01d366c1dc42da603d8a57"
        );
    }

    #[test]
    fn voice_media_key_matches_armada_vectors() {
        assert_eq!(
            hex::encode(derive::voice_media_key(&A, &ChannelId(B), Epoch(0))),
            "e27f294b1f0e9a9b4afe581b29b16b34e434dcdff70258e6c32ce12ce01ad192"
        );
        assert_eq!(
            hex::encode(derive::voice_media_key(&A, &ChannelId(B), Epoch(1))),
            "2249341edec7caeb296da5325030c1f939bc57f9c395bca1d940155a8b76dd24"
        );
    }

    #[test]
    fn voice_sender_key_matches_armada_vectors() {
        let vm0 = derive::voice_media_key(&A, &ChannelId(B), Epoch(0));
        assert_eq!(
            hex::encode(sender_key(&vm0, "00112233445566778899aabbccddeeff")),
            "44f16b303638b900ae245aad72944d9d5af2cf179c94b96ef43c32c6d4b24205"
        );
        assert_eq!(
            hex::encode(sender_key(&vm0, "alice-identity")),
            "61090929929de2d0c7f9053a5fc786a7fdbd253946c0ad81c4c007d45a062c15"
        );
    }
}

//! CORD-07 §4 presence for the LiveKit leg — the load-bearing
//! part. Armada clients install a random key for any SFU
//! identity no fresh kind-23313 presence claims, and drop the
//! audio as unverified: no presence = connected, publishing, and
//! silently inaudible. So while the leg is up, the bridge heartbeats
//! a `joined` rumor every 30 s on the channel's chat plane
//! carrying its broker-assigned `identity` + `broker` origin,
//! signed by its own member key (the signature is what binds
//! it to that identity), and a best-effort `left` on the way
//! out.
//!
//! The send replicates the core service's ephemeral publish path
//! with the public building blocks (`build_rumor_ms` +
//! `seal_chat_rumor_signed` + a `LiveTransport`), because the
//! core's own presence builder has no slot for the identity /
//! broker tags. One honest limitation: for writer-restricted
//! channels the core additionally rewrites sends under a writer
//! epoch; this replication publishes at the channel's read
//! address (which is where presence belongs), so on such a
//! channel strict peers might not fold it — ordinary voice
//! channels are unaffected.
//!
//! The same module OBSERVES everyone else's presence: a periodic
//! fetch of the plane's ephemeral wraps, opened with the chat
//! group key, folds `identity` claims into the session's
//! [`IdentityMap`] (the member↔identity mapping dedupe runs on)
//! and remembers which brokers the room is using (the §5
//! rendezvous input for the next join).

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use nostr_sdk::prelude::{Keys, PublicKey, Tag, Timestamp, ToBech32, UnsignedEvent};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vector_sdk::vector_core::community::transport::{LiveTransport, Query, Transport};
use vector_sdk::vector_core::community::v2::chat;
use vector_sdk::vector_core::community::v2::kind;
use vector_sdk::vector_core::community::v2::stream;
use vector_sdk::vector_core::community::{ChannelId, Epoch};
use vector_sdk::vector_core::signer::active_signer;

use super::dedupe::IdentityMap;
use super::keys::ChannelKeys;
use crate::util::now_ms;

/// The heartbeat cadence (CORD-07 §4: every 30 s; stale at 90 s).
pub const HEARTBEAT: Duration = Duration::from_secs(30);
/// How often the observer polls the plane for others' presence.
pub const OBSERVE_EVERY: Duration = Duration::from_secs(25);

/// Build the LiveKit presence rumor. Pure — unit-tested.
/// `identity`/`broker` are `Some` for a `joined`, `None` for a
/// `left` (spec: a left omits them).
pub fn build_livekit_presence_rumor(
    channel_id: &ChannelId,
    epoch: u64,
    author: PublicKey,
    identity: Option<&str>,
    broker: Option<&str>,
    muted: bool,
    deafened: bool,
    at_ms: u64,
) -> UnsignedEvent {
    let mut tags = stream::channel_binding_tags(channel_id, Epoch(epoch));
    if let (Some(identity), Some(broker)) = (identity, broker) {
        tags.push(Tag::custom("transport", ["livekit".to_string()]));
        tags.push(Tag::custom("identity", [identity.to_string()]));
        tags.push(Tag::custom("broker", [broker.to_string()]));
        if muted {
            tags.push(Tag::custom("muted", Vec::<String>::new()));
        }
        if deafened {
            tags.push(Tag::custom("deafened", Vec::<String>::new()));
        }
    }
    let content = if identity.is_some() { "joined" } else { "left" };
    stream::build_rumor_ms(kind::VOICE_PRESENCE, author, content, tags, at_ms)
}

/// Seal + publish one presence rumor (ephemeral wrap), mirroring
/// the core's ephemeral publish path.
async fn publish_presence(
    transport: &LiveTransport,
    keys: &ChannelKeys,
    rumor: &UnsignedEvent,
    at_ms: u64,
) -> Result<()> {
    let rumor = rumor.clone();
    let relays = keys.community.relays.clone();
    let group = keys.chat_group.clone();
    let wrap = vector_sdk::vector_core::db::scoped(async move {
        let signer = active_signer().map_err(|e| anyhow!(e))?;
        let author = rumor.pubkey;
        let owner = Keys::generate();
        chat::seal_chat_rumor_signed(
            &signer,
            author,
            &rumor,
            &group,
            Timestamp::from_secs(at_ms / 1000),
            true,
            &owner.public_key(),
        )
        .await
        .map_err(|e| anyhow!(e.to_string()))
    })
    .await?;
    transport.publish(&wrap, &relays).await.map_err(|e| anyhow!(e))?;
    Ok(())
}

/// Send one presence heartbeat (or the final `left`).
pub async fn send_presence(
    transport: &LiveTransport,
    keys: &ChannelKeys,
    author: PublicKey,
    identity: Option<&str>,
    broker: Option<&str>,
    muted: bool,
    deafened: bool,
) -> Result<()> {
    let at_ms = now_ms();
    let rumor = build_livekit_presence_rumor(
        &keys.channel_id,
        keys.epoch,
        author,
        identity,
        broker,
        muted,
        deafened,
        at_ms,
    );
    publish_presence(transport, keys, &rumor, at_ms).await
}

/// What the observer has learned about a channel's room: the
/// identity map (shared with the room layer) plus the broker
/// origins live presence currently names.
pub struct ObservedRoom {
    pub identities: Arc<Mutex<IdentityMap>>,
    pub brokers: Mutex<Vec<String>>,
}

impl ObservedRoom {
    pub fn new() -> Self {
        Self {
            identities: Arc::new(Mutex::new(IdentityMap::new())),
            brokers: Mutex::new(Vec::new()),
        }
    }
}

/// One observer pass: fetch recent ephemeral wraps on the chat
/// plane and fold any 23313 rumors into the identity map.
/// Returns how many presence rumors were folded.
pub async fn observe_once(
    transport: &LiveTransport,
    keys: &ChannelKeys,
    observed: &ObservedRoom,
) -> Result<usize> {
    let since = (now_ms() / 1000).saturating_sub(240);
    let query = Query {
        kinds: vec![stream::KIND_WRAP_EPHEMERAL],
        authors: vec![keys.chat_group.pk_hex()],
        since: Some(since),
        ..Default::default()
    };
    let wraps = transport
        .fetch(&query, &keys.community.relays)
        .await
        .map_err(|e| anyhow!(e))?;
    let mut folded = 0usize;
    let mut brokers: Vec<String> = Vec::new();
    {
        let mut map = observed.identities.lock().unwrap();
        for wrap in &wraps {
            let Ok(opened) = stream::open_wrap(wrap, &keys.chat_group) else { continue };
            if opened.rumor.kind.as_u16() != kind::VOICE_PRESENCE {
                continue;
            }
            // A verified pubkey always bech32s; the Result is
            // vestigial — fold the impossible error into a skip.
            let npub = opened.author.to_bech32().unwrap_or_default();
            if npub.is_empty() {
                continue;
            }
            let tag = |name: &str| -> Option<String> {
                opened
                    .rumor
                    .tags
                    .iter()
                    .find(|t| t.as_slice().first().map(|k| k.as_str()) == Some(name))
                    .and_then(|t| t.as_slice().get(1).cloned())
            };
            match opened.rumor.content.as_str() {
                "joined" => {
                    if let Some(identity) = tag("identity") {
                        // Freshness is timed from ARRIVAL (spec
                        // §4), not the sender's clock.
                        map.observe(&npub, &identity, now_ms());
                        folded += 1;
                    }
                    if let Some(broker) = tag("broker") {
                        if !brokers.contains(&broker) {
                            brokers.push(broker);
                        }
                    }
                }
                "left" => {
                    if let Some(identity) = tag("identity") {
                        map.release(&npub, &identity);
                    }
                }
                _ => {}
            }
        }
    }
    if !brokers.is_empty() {
        *observed.brokers.lock().unwrap() = brokers;
    }
    Ok(folded)
}

/// The observer loop for one session: fold presence until told
/// to stop. Failures are logged and retried next tick — presence
/// observation degrades dedupe, it must never kill the leg.
pub async fn run_observer(
    keys: Arc<ChannelKeys>,
    observed: Arc<ObservedRoom>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let transport = LiveTransport::new();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match observe_once(&transport, &keys, &observed).await {
            Ok(n) if n > 0 => debug!("[janus] presence observer folded {n} claim(s)"),
            Ok(_) => {}
            Err(e) => warn!("[janus] presence observation failed: {e}"),
        }
        // Sleep in short slices so a stop lands promptly.
        for _ in 0..OBSERVE_EVERY.as_secs() {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// The heartbeat loop for one session: announce `joined` now and
/// every 30 s; on stop, one best-effort `left`.
pub async fn run_heartbeat(
    keys: Arc<ChannelKeys>,
    author: PublicKey,
    identity: String,
    broker: String,
    muted: Arc<std::sync::atomic::AtomicBool>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let transport = LiveTransport::new();
    let send = |joined: bool| {
        let transport = &transport;
        let keys = &keys;
        let identity_ref = if joined { Some(identity.as_str()) } else { None };
        let broker_ref = if joined { Some(broker.as_str()) } else { None };
        let muted_now = muted.load(std::sync::atomic::Ordering::Relaxed);
        async move {
            send_presence(transport, keys, author, identity_ref, broker_ref, muted_now, false).await
        }
    };
    let muted_at_join = muted.load(std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = send(true).await {
        warn!("[janus] first presence heartbeat failed: {e}");
    } else {
        // The flag value rides in the log so a "the bot shows as
        // muted" report can be checked against what was
        // actually announced (the rumor builder's tag mapping
        // is unit-tested in this module).
        info!("[janus] presence announced (identity {identity}, muted={muted_at_join})");
    }
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        for _ in 0..HEARTBEAT.as_secs() {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if let Err(e) = send(true).await {
            warn!("[janus] presence heartbeat failed: {e}");
        }
    }
    // Best-effort left, short fuse.
    let _ = tokio::time::timeout(Duration::from_secs(5), send(false)).await;
    info!("[janus] presence withdrawn");
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::Keys as NostrKeys;

    #[test]
    fn joined_rumor_carries_identity_and_broker() {
        let author = NostrKeys::generate().public_key();
        let rumor = build_livekit_presence_rumor(
            &ChannelId([2u8; 32]),
            0,
            author,
            Some("sfu-identity-1"),
            Some("https://av.armada.buzz"),
            true,
            false,
            1_700_000_000_123,
        );
        assert_eq!(rumor.kind.as_u16(), kind::VOICE_PRESENCE);
        assert_eq!(rumor.content, "joined");
        let tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        assert!(tags.iter().any(|t| t == &vec!["identity".to_string(), "sfu-identity-1".to_string()]), "{tags:?}");
        assert!(tags.iter().any(|t| t == &vec!["broker".to_string(), "https://av.armada.buzz".to_string()]), "{tags:?}");
        assert!(tags.iter().any(|t| t.first().map(|s| s.as_str()) == Some("muted")), "muted flag present: {tags:?}");
        assert!(!tags.iter().any(|t| t.first().map(|s| s.as_str()) == Some("deafened")));
        // The channel binding tags ride along (channel + epoch),
        // like every chat-plane rumor.
        assert!(tags.iter().any(|t| t.first().map(|s| s.as_str()) == Some("channel")), "{tags:?}");
    }

    #[test]
    fn left_rumor_omits_identity_and_broker() {
        let author = NostrKeys::generate().public_key();
        let rumor = build_livekit_presence_rumor(
            &ChannelId([2u8; 32]),
            0,
            author,
            None,
            None,
            false,
            false,
            1_700_000_000_000,
        );
        assert_eq!(rumor.content, "left");
        let names: Vec<&str> = rumor.tags.iter().filter_map(|t| t.as_slice().first().map(|s| s.as_str())).collect();
        assert!(!names.contains(&"identity"));
        assert!(!names.contains(&"broker"));
    }
}

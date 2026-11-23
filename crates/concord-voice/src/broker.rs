//! CORD-07 §2: the blind broker grant.
//!
//! The client signs a NIP-98-style kind-27235 event with the
//! channel's VOICE key (never the reference bot's own key — `event.pubkey`
//! equals the room name), base64s it into
//! `Authorization: Concord <…>`, and GETs
//! `<broker>/.well-known/concord/av/<room>`. The broker answers
//! `{ token, url, identity }`: a short-lived LiveKit JWT, the SFU
//! wss URL, and a random SFU identity it assigns (≥128 bits — it
//! feeds the per-sender key derivation, and the bot must announce
//! it in presence or Armada clients render him unverified).
//!
//! The nonce tag is mandatory in practice: every member signs
//! with the same voice key, so without 32 fresh random bytes two
//! grants in one second would share an event id and the broker's
//! anti-replay set would refuse the second.
//!
//! Broker choice (CORD-07 §5 rendezvous): join the broker live
//! presence names; ties break to the smallest
//! `sha256(voice_room[32] || utf8(origin))`; an empty room uses
//! the configured default (`https://av.armada.buzz`).

use anyhow::{anyhow, Result};
use base64::Engine;
use nostr_sdk::prelude::Keys;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use vector_sdk::vector_core::net::build_http_client;

pub const DEFAULT_BROKER: &str = "https://av.armada.buzz";
pub const GRANT_KIND: u32 = 27235;

/// The broker's answer to a grant request.
#[derive(Debug, Clone, Deserialize)]
pub struct Grant {
    /// LiveKit JWT, scoped to joining this one room.
    pub token: String,
    /// The SFU WebSocket URL (wss).
    pub url: String,
    /// The broker-assigned SFU identity for this join.
    pub identity: String,
}

/// Build the signed grant event for one token request. Pure (the
/// caller supplies the nonce + timestamp) so the construction is
/// unit-testable: kind, pubkey == room, the exact `u` / `method` /
/// `nonce` tags, and a BIP-340 signature by the voice key over
/// the NIP-01 event id.
pub fn build_grant_event(
    voice_keys: &Keys,
    broker_origin: &str,
    room: &str,
    nonce: &[u8; 32],
    created_at: u64,
) -> serde_json::Value {
    let origin = broker_origin.trim_end_matches('/');
    let url = format!("{origin}/.well-known/concord/av/{room}");
    let pubkey = voice_keys.public_key().to_hex();
    debug_assert_eq!(pubkey, room, "the grant must be signed by the room key");
    let tags = serde_json::json!([
        ["u", url],
        ["method", "GET"],
        ["nonce", hex::encode(nonce)],
    ]);
    // NIP-01 id: sha256 of the compact serialization
    // [0, pubkey, created_at, kind, tags, content].
    let serialized = serde_json::to_string(&serde_json::json!([
        0, pubkey, created_at, GRANT_KIND, tags, ""
    ]))
    .expect("serializing plain JSON values cannot fail");
    let id: [u8; 32] = Sha256::digest(serialized.as_bytes()).into();
    let sig = voice_keys.sign_schnorr(id);
    serde_json::json!({
        "id": hex::encode(id),
        "pubkey": pubkey,
        "created_at": created_at,
        "kind": GRANT_KIND,
        "tags": tags,
        "content": "",
        "sig": sig.to_hex(),
    })
}

/// Canonical broker origin form for the §5 tie-break: lowercase
/// scheme + host, default port omitted, no path, no trailing
/// slash (RFC 6454 serialization).
pub fn canonical_origin(origin: &str) -> String {
    let o = origin.trim().trim_end_matches('/');
    let (scheme, rest) = match o.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r),
        None => ("https".to_string(), o),
    };
    let hostport = rest.split('/').next().unwrap_or(rest).to_lowercase();
    let hostport = match (scheme.as_str(), hostport.rsplit_once(':')) {
        ("https", Some((h, "443"))) => h.to_string(),
        ("http", Some((h, "80"))) => h.to_string(),
        _ => hostport,
    };
    format!("{scheme}://{hostport}")
}

/// CORD-07 §5 broker choice. `observed` = origins named by live
/// presence for this channel. Empty → the default. Otherwise the
/// smallest `sha256(room[32] || utf8(origin))` wins, so every
/// client converges on the same broker for the same room.
pub fn pick_broker(room: &[u8; 32], observed: &[String], default: &str) -> String {
    let mut best: Option<(String, [u8; 32])> = None;
    for origin in observed {
        let origin = canonical_origin(origin);
        let mut hasher = Sha256::new();
        hasher.update(room);
        hasher.update(origin.as_bytes());
        let score: [u8; 32] = hasher.finalize().into();
        match &best {
            Some((_, s)) if *s <= score => {}
            _ => best = Some((origin, score)),
        }
    }
    best.map(|(o, _)| o).unwrap_or_else(|| canonical_origin(default))
}

/// The configured default broker: `BRIDGE_BROKER`, else
/// the Armada default.
pub fn default_broker() -> String {
    std::env::var("BRIDGE_BROKER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_BROKER.to_string())
}

/// Probe a broker's capability endpoint (expects 204). Best
/// effort — the grant request itself is the real test.
pub async fn probe_broker(origin: &str) -> bool {
    let url = format!("{}/.well-known/concord/av", origin.trim_end_matches('/'));
    let Ok(http) = build_http_client(Duration::from_secs(10)) else { return false };
    match http.get(&url).send().await {
        Ok(resp) => resp.status().as_u16() == 204,
        Err(_) => false,
    }
}

/// Request a token from a broker. Uses the bot's hardened HTTP
/// client (the same transport the search fetches use).
pub async fn fetch_grant(voice_keys: &Keys, broker_origin: &str, room: &str) -> Result<Grant> {
    let mut nonce = [0u8; 32];
    {
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut nonce);
    }
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let event = build_grant_event(voice_keys, broker_origin, room, &nonce, created_at);
    let encoded = base64::engine::general_purpose::STANDARD.encode(event.to_string());
    let url = format!(
        "{}/.well-known/concord/av/{}",
        broker_origin.trim_end_matches('/'),
        room
    );
    let http = build_http_client(Duration::from_secs(15)).map_err(|e| anyhow!(e))?;
    let resp = http
        .get(&url)
        .header("Authorization", format!("Concord {encoded}"))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.bytes().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "broker grant HTTP {status}: {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    serde_json::from_slice::<Grant>(&body).map_err(|e| anyhow!("broker grant response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::{Keys, SecretKey};

    fn voice_keys() -> (Keys, String) {
        // The frozen-vector voice keypair (secret [1;32], id
        // [2;32], epoch 0): its pubkey is the room.
        let sk = SecretKey::from_slice(&hex::decode(
            "89695aad5d1e8f39e9520e9ff002c479e9bae18e1d0e4e061c01ba119d07d451",
        ).unwrap()).unwrap();
        let keys = Keys::new(sk);
        let room = keys.public_key().to_hex();
        assert_eq!(room, "1e1f2f696f2885a85b02137a1c227a360aa3f40f2d01d366c1dc42da603d8a57");
        (keys, room)
    }

    #[test]
    fn grant_event_shape_and_signature() {
        let (keys, room) = voice_keys();
        let nonce = [9u8; 32];
        let ev = build_grant_event(&keys, "https://av.armada.buzz", &room, &nonce, 1_700_000_000);
        assert_eq!(ev["kind"], GRANT_KIND);
        assert_eq!(ev["pubkey"], room);
        assert_eq!(ev["content"], "");
        assert_eq!(ev["created_at"], 1_700_000_000);
        let tags = ev["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0][0], "u");
        assert_eq!(
            tags[0][1],
            format!("https://av.armada.buzz/.well-known/concord/av/{room}")
        );
        assert_eq!(tags[1], serde_json::json!(["method", "GET"]));
        assert_eq!(tags[2][0], "nonce");
        assert_eq!(tags[2][1], hex::encode(nonce));
        // The broker's whole check, replayed through the nostr
        // library's own verifier: parse the JSON back into an
        // Event and verify id + BIP-340 signature.
        let parsed: nostr_sdk::prelude::Event = serde_json::from_value(ev.clone()).unwrap();
        assert!(parsed.verify_id(), "event id must be the NIP-01 hash of its fields");
        assert!(parsed.verify_signature(), "signature must verify under the room key");
        assert!(parsed.verify().is_ok());
    }

    #[test]
    fn distinct_nonces_make_distinct_grants() {
        // The anti-replay point of the nonce: same key, same
        // second, different nonce → different event id.
        let (keys, room) = voice_keys();
        let a = build_grant_event(&keys, "https://av.armada.buzz", &room, &[1u8; 32], 42);
        let b = build_grant_event(&keys, "https://av.armada.buzz", &room, &[2u8; 32], 42);
        assert_ne!(a["id"], b["id"]);
        assert_ne!(a["sig"], b["sig"]);
    }

    #[test]
    fn broker_choice_follows_the_tiebreak() {
        let room = [5u8; 32];
        assert_eq!(pick_broker(&room, &[], "https://av.armada.buzz"), "https://av.armada.buzz");
        let observed = vec![
            "https://b.example".to_string(),
            "https://a.example".to_string(),
            "https://c.example:8443".to_string(),
        ];
        let picked = pick_broker(&room, &observed, "https://av.armada.buzz");
        assert!(observed.iter().any(|o| canonical_origin(o) == picked));
        // Deterministic: every client computes the same winner.
        assert_eq!(picked, pick_broker(&room, &observed, "https://other.example"));
        // The winner really is the sha256 minimum.
        let mut scores: Vec<(String, [u8; 32])> = observed
            .iter()
            .map(|o| {
                let o = canonical_origin(o);
                let mut h = Sha256::new();
                h.update(room);
                h.update(o.as_bytes());
                (o, h.finalize().into())
            })
            .collect();
        scores.sort_by_key(|(_, s)| *s);
        assert_eq!(picked, scores[0].0);
    }

    #[test]
    fn origins_canonicalize() {
        assert_eq!(canonical_origin("HTTPS://AV.Armada.Buzz/"), "https://av.armada.buzz");
        assert_eq!(canonical_origin("https://av.armada.buzz:443"), "https://av.armada.buzz");
        assert_eq!(canonical_origin("http://x.example:80/path"), "http://x.example");
        assert_eq!(canonical_origin("https://x.example:8443"), "https://x.example:8443");
    }
}

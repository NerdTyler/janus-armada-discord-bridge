//! CORD-07 §3 media encryption: the AES-256-GCM frame cryptor
//! Armada's clients speak (LiveKit's insertable-streams frame
//! format, `livekit-client` 2.19.2 `FrameCryptor`), keyed with
//! Concord material instead of a room password.
//!
//! Reimplemented from CORD-07 §3 + LiveKit's published frame
//! format — NOT copied from any existing client. The wire shape:
//!
//! ```text
//! sender_key = hkdf(voice_media_key, "concord/voice-sender", sha256(utf8(identity)))
//! frame_key  = HKDF-SHA256(ikm=sender_key, salt="LKFrameEncryptionKey", info=[0u8; 128], L=32)
//! payload    = opus[0] (TOC, clear, the AEAD AAD)
//!            || AES-256-GCM(frame_key, iv, opus[1..]) || tag(16)
//!            || iv(12) || [12, key_index]
//! iv         = ssrc_be32 || rtp_ts_be32 || (rtp_ts - (send_count % 0xffff))_be32
//! ```
//!
//! The receiver derives every publisher's key from the identity
//! the SFU presents for the track — there is no in-band key
//! exchange — and reads the IV from the frame trailer, so the
//! header values an encryptor chose only need to keep IVs unique
//! under one frame key, which the per-sender monotonic counter
//! does. Key index is 0 (no ratcheting on the Concord side).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Result};
use hkdf::Hkdf;
use sha2::Sha256;

use super::keys::sender_key;

/// LiveKit's frame-key derivation salt (`deriveKeys` in
/// livekit-client's e2ee utils, AES-256 variant).
pub const LK_KEY_DERIVATION_SALT: &[u8] = b"LKFrameEncryptionKey";
pub const IV_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Audio leaves exactly the Opus TOC byte in the clear.
pub const UNENCRYPTED_BYTES: usize = 1;

/// `frame_key` from one publisher's CORD-07 sender key: HKDF-SHA256
/// with LiveKit's fixed salt and 128 zero bytes of info.
pub fn derive_frame_key(sender_key: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(LK_KEY_DERIVATION_SALT), sender_key);
    let mut out = [0u8; 32];
    hk.expand(&[0u8; 128], &mut out).expect("32-byte HKDF expand cannot fail");
    out
}

/// Stateful per-publisher frame encryptor (one per published
/// track). The send counter feeds the IV's tail so no IV repeats
/// under the frame key.
pub struct FrameCryptor {
    cipher: Aes256Gcm,
    key_index: u8,
    send_count: u32,
}

impl FrameCryptor {
    /// From a CORD-07 sender key (already derived from the media
    /// key + this publisher's SFU identity).
    pub fn from_sender_key(sender_key: &[u8; 32], key_index: u8) -> Self {
        let frame_key = derive_frame_key(sender_key);
        Self {
            cipher: Aes256Gcm::new((&frame_key).into()),
            key_index,
            send_count: 0,
        }
    }

    /// From the channel media key + the publisher's SFU identity.
    pub fn from_media_key(media_key: &[u8; 32], identity: &str, key_index: u8) -> Self {
        Self::from_sender_key(&sender_key(media_key, identity), key_index)
    }

    /// The IV for the next frame: `ssrc || ts || ts - (count % 0xffff)`,
    /// all big-endian; the counter advances after use, wrapping at
    /// 0xffff exactly as the reference algorithm does.
    pub fn next_iv(&mut self, ssrc: u32, rtp_timestamp: u32) -> [u8; IV_LEN] {
        let counter_mod = self.send_count % 0xffff;
        self.send_count = (counter_mod + 1) % 0xffff;
        let tail = rtp_timestamp.wrapping_sub(counter_mod);
        let mut iv = [0u8; IV_LEN];
        iv[0..4].copy_from_slice(&ssrc.to_be_bytes());
        iv[4..8].copy_from_slice(&rtp_timestamp.to_be_bytes());
        iv[8..12].copy_from_slice(&tail.to_be_bytes());
        iv
    }

    /// Encrypt one Opus packet into an encrypted RTP payload.
    pub fn encrypt_frame(&mut self, ssrc: u32, rtp_timestamp: u32, opus_packet: &[u8]) -> Result<Vec<u8>> {
        if opus_packet.is_empty() {
            return Err(anyhow!("cannot encrypt an empty Opus packet"));
        }
        let iv = self.next_iv(ssrc, rtp_timestamp);
        let toc = &opus_packet[..UNENCRYPTED_BYTES];
        let ct_and_tag = self
            .cipher
            .encrypt(
                Nonce::from_slice(&iv),
                Payload { msg: &opus_packet[UNENCRYPTED_BYTES..], aad: toc },
            )
            .map_err(|_| anyhow!("AES-256-GCM frame encryption failed"))?;
        let mut out = Vec::with_capacity(UNENCRYPTED_BYTES + ct_and_tag.len() + IV_LEN + 2);
        out.extend_from_slice(toc);
        out.extend_from_slice(&ct_and_tag);
        out.extend_from_slice(&iv);
        out.push(IV_LEN as u8);
        out.push(self.key_index);
        Ok(out)
    }
}

/// Decrypt one encrypted RTP payload under a publisher's sender
/// key. `expected_key_index` is the index this room runs (0).
/// The IV comes from the frame trailer, never from the RTP header.
pub fn decrypt_frame(sender_key: &[u8; 32], expected_key_index: u8, payload: &[u8]) -> Result<Vec<u8>> {
    // Minimum: TOC(1) + tag(16) + IV(12) + trailer(2).
    if payload.len() < UNENCRYPTED_BYTES + TAG_LEN + IV_LEN + 2 {
        return Err(anyhow!("encrypted audio frame too short ({} bytes)", payload.len()));
    }
    let n = payload.len();
    let key_index = payload[n - 1] & 0x7f;
    if key_index != expected_key_index {
        return Err(anyhow!("unexpected E2EE key index {key_index} (expected {expected_key_index})"));
    }
    let iv_len = payload[n - 2] as usize;
    if iv_len != IV_LEN {
        return Err(anyhow!("unexpected E2EE IV length {iv_len} (expected {IV_LEN})"));
    }
    let iv_start = n - 2 - iv_len;
    let iv = &payload[iv_start..n - 2];
    let toc = &payload[..UNENCRYPTED_BYTES];
    let frame_key = derive_frame_key(sender_key);
    let cipher = Aes256Gcm::new((&frame_key).into());
    let plain = cipher
        .decrypt(
            Nonce::from_slice(iv),
            Payload { msg: &payload[UNENCRYPTED_BYTES..iv_start], aad: toc },
        )
        .map_err(|_| anyhow!("AES-256-GCM frame decryption / tag verification failed"))?;
    let mut out = Vec::with_capacity(UNENCRYPTED_BYTES + plain.len());
    out.extend_from_slice(toc);
    out.extend_from_slice(&plain);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::sender_key as derive_sender_key;

    // The reference roundtrip shape: media key from the frozen
    // CORD vectors (secret [1;32], channel [2;32], epoch 0), a
    // 64-hex SFU identity, a stereo-music-style Opus packet.
    fn media_key() -> [u8; 32] {
        use vector_sdk::vector_core::community::v2::derive;
        use vector_sdk::vector_core::community::{ChannelId, Epoch};
        derive::voice_media_key(&[1u8; 32], &ChannelId([2u8; 32]), Epoch(0))
    }

    #[test]
    fn frame_roundtrip_restores_the_packet() {
        let media = media_key();
        let identity = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let sender = derive_sender_key(&media, identity);
        let mut enc = FrameCryptor::from_media_key(&media, identity, 0);
        let mut opus = vec![0xfcu8];
        opus.extend((0..160u8).map(|i| i.wrapping_mul(7)));

        let encrypted = enc.encrypt_frame(0xCAFE_BABE, 96_000, &opus).unwrap();
        // Layout: TOC clear, total = packet + tag + iv + trailer.
        assert_eq!(encrypted[0], 0xfc);
        assert_eq!(encrypted.len(), opus.len() + TAG_LEN + IV_LEN + 2);
        assert_eq!(encrypted[encrypted.len() - 2], IV_LEN as u8);
        assert_eq!(encrypted[encrypted.len() - 1], 0);

        let decrypted = decrypt_frame(&sender, 0, &encrypted).unwrap();
        assert_eq!(decrypted, opus);
    }

    #[test]
    fn iv_layout_and_counter_match_the_reference_algorithm() {
        let mut enc = FrameCryptor::from_sender_key(&[7u8; 32], 0);
        // First frame: counter 0, tail == timestamp.
        let iv = enc.next_iv(0x0102_0304, 96_000);
        assert_eq!(&iv[0..4], &0x0102_0304u32.to_be_bytes());
        assert_eq!(&iv[4..8], &96_000u32.to_be_bytes());
        assert_eq!(&iv[8..12], &96_000u32.to_be_bytes());
        // Second frame: counter 1, tail == timestamp - 1.
        let iv = enc.next_iv(0x0102_0304, 96_960);
        assert_eq!(&iv[8..12], &96_959u32.to_be_bytes());
        // Counter period is 0xffff: the two calls above used 0 and
        // 1, so after 0xfffd more the state is back at 0xfffe's end —
        // the 65535th call overall uses counter 0 (tail == ts).
        let mut last = [0u8; IV_LEN];
        for i in 0..0xfffeu32 {
            last = enc.next_iv(1, 1_000_000 + i * 960);
        }
        // The final loop call is the 65536th overall; before it the
        // counter had wrapped to 0, so its tail is its own timestamp.
        assert_eq!(&last[8..12], &(1_000_000u32 + 0xfffd * 960).to_be_bytes());
    }

    #[test]
    fn tampering_with_the_clear_toc_fails_the_tag_check() {
        // The TOC byte is AAD: flipping it must fail authentication,
        // which is what makes the clear header safe to leave open.
        let media = media_key();
        let identity = "alice-identity";
        let sender = derive_sender_key(&media, identity);
        let mut enc = FrameCryptor::from_media_key(&media, identity, 0);
        let opus = vec![0xf8u8, 1, 2, 3, 4, 5];
        let encrypted = enc.encrypt_frame(42, 4800, &opus).unwrap();
        let mut tampered = encrypted.clone();
        tampered[0] ^= 0x04;
        assert!(decrypt_frame(&sender, 0, &tampered).is_err());
        // And a bit flip in the ciphertext fails too.
        let mut tampered = encrypted;
        tampered[3] ^= 0x01;
        assert!(decrypt_frame(&sender, 0, &tampered).is_err());
    }

    #[test]
    fn wrong_key_or_index_is_rejected() {
        let media = media_key();
        let mut enc = FrameCryptor::from_media_key(&media, "alice-identity", 0);
        let opus = vec![0xf8u8, 9, 9, 9];
        let encrypted = enc.encrypt_frame(1, 960, &opus).unwrap();
        let other_sender = derive_sender_key(&media, "mallory-identity");
        assert!(decrypt_frame(&other_sender, 0, &encrypted).is_err());
        let alice_sender = derive_sender_key(&media, "alice-identity");
        assert!(decrypt_frame(&alice_sender, 1, &encrypted).is_err());
        assert!(decrypt_frame(&alice_sender, 0, &encrypted[..20]).is_err());
        assert!(FrameCryptor::from_sender_key(&alice_sender, 0)
            .encrypt_frame(1, 960, &[])
            .is_err());
    }

    #[test]
    fn successive_frames_get_distinct_ivs() {
        let mut enc = FrameCryptor::from_sender_key(&[3u8; 32], 0);
        let a = enc.encrypt_frame(7, 0, &[0xf8, 1]).unwrap();
        let b = enc.encrypt_frame(7, 960, &[0xf8, 1]).unwrap();
        let iv_a = &a[a.len() - 14..a.len() - 2];
        let iv_b = &b[b.len() - 14..b.len() - 2];
        assert_ne!(iv_a, iv_b, "IVs must never repeat under one frame key");
    }
}

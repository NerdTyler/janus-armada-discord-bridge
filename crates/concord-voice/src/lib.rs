//! Concord voice transport for the Janus bridge: everything
//! needed to join an Armada (Concord) voice channel on the
//! LiveKit SFU as one named participant — publish mixed PCM,
//! receive per-participant PCM.
//!
//! Shape of the leg:
//! - [`keys`] resolves the channel's CORD-07 key set from the
//!   bridge's own community membership (the vendored core
//!   exposes everything through the SDK facade — see keys.rs).
//! - [`broker`] runs the CORD-07 §2 grant flow against a broker
//!   (default `https://av.armada.buzz`, chosen by the §5
//!   rendezvous when live presence names others).
//! - [`presence`] heartbeats the bridge's brokered presence —
//!   without it Janus is connected but silently inaudible — and
//!   observes everyone else's, building the identity map.
//! - [`session`] runs the WebRTC room: publish the mixer's
//!   frames (E2EE per [`cryptor`]), receive everyone else as
//!   raw 20 ms PCM frames.
//! - [`supervisor`] owns the lifecycle: grant → presence →
//!   room, rekey rejoins, bounded retries.
//!
//! This code is lifted from the reference bot's
//! `src/livekit/` module, where it went live-proven against
//! this SFU on 2026-11-22 — including the RFC 4571 ICE-TCP
//! bridge in session.rs, without which plain UDP checks never
//! validate against the SFU. Reference-bot-specific couplings (its
//! call fan-out, whisper transcription, dual-transport dedupe)
//! are replaced by the bridge's PCM channel contract.
//!
//! Failure isolation is absolute: the leg is spawned per
//! session, and every error is logged and ends (or retries)
//! the leg only.

pub mod broker;
pub mod cryptor;
pub mod dedupe;
pub mod keys;
pub mod presence;
pub mod session;
pub mod signal;
pub mod supervisor;
pub mod util;

pub use keys::ChannelKeys;
pub use session::RoomEnd;
pub use supervisor::{spawn_leg, Leg};

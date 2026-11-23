//! The leg supervisor: connect → run → (rekey: rejoin at once
//! | failure: back off, give up after 3 | stopped: exit).
//! Owns the per-attempt side tasks (presence heartbeat +
//! observer) and the relay that feeds mixed PCM into whichever
//! room session is live.
//!
//! Adapted from the reference bot's `livekit::supervise`
//! with the bot-runtime couplings (VectorBot / Ctx / CallShared)
//! replaced by plain inputs: the bridge member's own author key
//! and two PCM channels. Failure isolation is absolute: every
//! error is logged and ends (or retries) the leg only.

use log::{info, warn};
use nostr_sdk::prelude::PublicKey;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::broker;
use crate::keys::{self, ChannelKeys};
use crate::presence::{self, ObservedRoom};
use crate::session::{self, RoomEnd};

/// A running Armada leg for one voice channel.
pub struct Leg {
    /// Set to end the leg (the supervisor and every side task
    /// watch it).
    pub stop: Arc<AtomicBool>,
    /// Feed mixed 20 ms frames here; a relay forwards them into
    /// whichever room session is currently live (frames fed
    /// while no room is up are dropped by the relay).
    pub pcm_in: mpsc::UnboundedSender<Vec<i16>>,
    /// Inbound per-participant frames: `(sfu_identity, frame)`.
    pub pcm_out: mpsc::UnboundedReceiver<(String, Vec<i16>)>,
    /// Presence observation for this channel (identity map +
    /// broker rendezvous record).
    pub observed: Arc<ObservedRoom>,
    /// The supervisor task.
    pub task: tokio::task::JoinHandle<()>,
}

impl Leg {
    /// End the leg and wait for the supervisor to finish.
    pub async fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.await;
    }
}

/// Start the leg for one voice channel (hex id, as the SDK
/// names channels). Fire-and-forget: the supervisor owns the
/// leg's whole lifecycle and can only ever hurt itself.
pub fn spawn_leg(channel_hex: String, author: PublicKey) -> Leg {
    let stop = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(ObservedRoom::new());
    let (pcm_in_tx, mut pcm_in_rx) = mpsc::unbounded_channel::<Vec<i16>>();
    let (pcm_out_tx, pcm_out_rx) = mpsc::unbounded_channel::<(String, Vec<i16>)>();

    // Relay: the caller's queue is created once, rooms come and
    // go (rekeys) — forward into whichever room is live.
    let room_slot: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<i16>>>>> = Arc::new(Mutex::new(None));
    {
        let room_slot = Arc::clone(&room_slot);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while let Some(frame) = pcm_in_rx.recv().await {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let target = room_slot.lock().unwrap().clone();
                if let Some(tx) = target {
                    let _ = tx.send(frame);
                }
            }
        });
    }

    let task = tokio::spawn(supervise(
        channel_hex,
        author,
        Arc::clone(&stop),
        Arc::clone(&observed),
        room_slot,
        pcm_out_tx,
    ));
    Leg { stop, pcm_in: pcm_in_tx, pcm_out: pcm_out_rx, observed, task }
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    channel: String,
    author: PublicKey,
    stop: Arc<AtomicBool>,
    observed: Arc<ObservedRoom>,
    room_slot: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<i16>>>>>,
    pcm_out: mpsc::UnboundedSender<(String, Vec<i16>)>,
) {
    info!("[janus] leg starting for voice in {channel}");
    let mut failures = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let keys = match keys::resolve_channel_keys(&channel) {
            Ok(k) => Arc::new(k),
            Err(e) => {
                warn!("[janus] cannot resolve channel keys for {channel}: {e:#}");
                failures += 1;
                if failures >= 3 || !backoff(&stop).await {
                    break;
                }
                continue;
            }
        };

        // Rendezvous (CORD-07 §5): learn who is live from
        // presence, then pick; nobody observed → our default
        // broker.
        {
            let transport = vector_sdk::vector_core::community::transport::LiveTransport::new();
            let _ = presence::observe_once(&transport, &keys, &observed).await;
        }
        let observed_brokers = observed.brokers.lock().unwrap().clone();
        let broker_origin = broker::pick_broker(
            &room_bytes(&keys.room),
            &observed_brokers,
            &broker::default_broker(),
        );
        if !broker::probe_broker(&broker_origin).await {
            warn!("[janus] broker {broker_origin} did not answer the probe; trying the grant anyway");
        }
        let grant = match broker::fetch_grant(&keys.voice_keys, &broker_origin, &keys.room).await {
            Ok(g) => g,
            Err(e) => {
                warn!("[janus] grant from {broker_origin} failed: {e:#}");
                failures += 1;
                if failures >= 3 || !backoff(&stop).await {
                    break;
                }
                continue;
            }
        };
        info!("[janus] grant ok from {broker_origin}: identity {}", grant.identity);

        // Per-attempt side tasks: presence heartbeat (the
        // audibility load-bearer) + presence observer. The
        // bridge never mutes itself, so the heartbeat's muted
        // flag is a constant false.
        let mut side_tasks = Vec::new();
        let muted_flag = Arc::new(AtomicBool::new(false));
        side_tasks.push(tokio::spawn(presence::run_heartbeat(
            Arc::clone(&keys),
            author,
            grant.identity.clone(),
            broker_origin.clone(),
            muted_flag,
            Arc::clone(&stop),
        )));
        side_tasks.push(tokio::spawn(presence::run_observer(
            Arc::clone(&keys),
            Arc::clone(&observed),
            Arc::clone(&stop),
        )));

        let (room_tx, room_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        *room_slot.lock().unwrap() = Some(room_tx);
        let end = session::run_room(
            Arc::clone(&keys),
            grant.url.clone(),
            grant.token.clone(),
            grant.identity.clone(),
            Arc::clone(&stop),
            room_rx,
            pcm_out.clone(),
        )
        .await;
        *room_slot.lock().unwrap() = None;
        for task in side_tasks {
            task.abort();
        }

        match end {
            RoomEnd::Stopped => break,
            RoomEnd::Rekey => {
                failures = 0;
                info!("[janus] rejoining after rekey");
            }
            RoomEnd::Failed(e) => {
                warn!("[janus] room session failed: {e:#}");
                failures += 1;
                if failures >= 3 {
                    warn!("[janus] giving up on the Armada leg for {channel} after {failures} failures");
                    break;
                }
                if !backoff(&stop).await {
                    break;
                }
            }
        }
    }
    info!("[janus] leg ended for voice in {channel}");
}

/// ~5 s between retries, in slices so a stop lands promptly.
/// Returns false when the leg was stopped during the wait.
async fn backoff(stop: &AtomicBool) -> bool {
    for _ in 0..20 {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    !stop.load(Ordering::Relaxed)
}

/// The room name is the voice key's x-only pubkey hex; the §5
/// hash wants the raw 32 bytes.
fn room_bytes(room_hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    let bytes = room_hex.as_bytes();
    if bytes.len() == 64 {
        for i in 0..32 {
            let hi = (bytes[2 * i] as char).to_digit(16);
            let lo = (bytes[2 * i + 1] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out[i] = ((hi << 4) | lo) as u8;
            }
        }
    }
    out
}

/// Resolve the key set for a channel without starting a leg —
/// the membership/wiring layer uses this to fail fast with a
/// clear error when the bridge does not (yet) hold the channel.
pub fn probe_channel_keys(channel_hex: &str) -> anyhow::Result<ChannelKeys> {
    keys::resolve_channel_keys(channel_hex)
}

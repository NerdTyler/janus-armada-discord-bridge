//! Janus — Armada leg + mixer daemon (host side).
//!
//! Resident process (janus-armada.service). Owns:
//! - the PCM Unix socket the Discord leg connects to,
//! - the mixer (both directions, 20 ms tick, mix-minus),
//! - the Armada (Concord) voice leg via concord-voice,
//! - Armada-side summon/leave: conversational triggers
//!   ("janus join", "janus leave" — converse.rs) with
//!   !summon / !leave as backstops, and the Armada-side
//!   consent announcements.
//!
//! Participation model : the daemon is
//! always running but joins voice ONLY when summoned, from
//! either side. Summon access follows SUMMON_ACCESS (see
//! auth.rs): the owner's deployment is OPEN — anyone may summon
//! (his final call) — with a whitelist mode
//! available in config. The summon side joins the voice
//! channel the
//! SUMMONER is in; the far side lands by resolution
//! (named -> the one occupied channel -> configured
//! default). Leave is explicit, or when NO HUMANS remain in
//! either channel of the session (the owner's rule, 2026-11-22:
//! Janus's staying is anchored to human occupancy — a
//! summoner or bot leaving never ends it while people
//! remain on either side, and bots never count as people).
//! No LLM anywhere: this is pure audio transport.
//!
//! Without ARMADA_BRIDGE_NSEC the daemon still runs the
//! socket + mixer (Discord-leg and protocol testing), and
//! summons fail with a clear "not configured" error.

mod auth;
mod converse;
mod resolve;

use anyhow::{Context, Result};
use log::{error, info, warn};
use nostr_sdk::prelude::{FromBech32, PublicKey};
use pcm_protocol::{
    fnv1a_64, Frame, FrameType, Mixer, Side, StreamDecoder, FRAME_SAMPLES,
};
use resolve::{
    humans_only, is_voice_channel, resolve_landing, should_end_for_emptiness,
    snapshot_summary, ChannelOccupancy, Landing,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use vector_sdk::{BotEvent, IncomingMessage, VectorBot};

const EX_CONFIG: i32 = 78;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Config {
    socket_path: String,
    data_dir: String,
    nsec: Option<String>,
    owner_npub: Option<String>,
    /// Summon access mode for !summon / !leave (SUMMON_ACCESS):
    /// Open = anyone, Whitelist = the list below.
    summon_access: auth::SummonAccess,
    /// Summon whitelist for !summon / !leave in whitelist mode:
    /// ARMADA_OWNER_NPUB (always included) ∪ ARMADA_SUMMON_NPUBS.
    /// Invite auto-accept does NOT use this — it stays
    /// owner-only in both modes.
    summon_whitelist: HashSet<String>,
    /// Npubs that never count as "someone" for the session
    /// emptiness decision (ARMADA_BOT_NPUBS): bots sharing
    /// the room — a music bot, which auto-summons Janus when its
    /// music starts, is the deployment case. Janus himself
    /// is always excluded separately (channel_occupants).
    bot_npubs: HashSet<String>,
    default_voice_channel: Option<String>,
    text_channel: Option<String>,
    /// Chat-visible session start/stop announcements
    /// (JANUS_ANNOUNCE): only the exact value "on" enables
    /// them. The owner's order (2026-11-22): Janus stops
    /// announcing every session in chat — the deployment
    /// sets JANUS_ANNOUNCE=off, and unset/invalid is silent
    /// too. Journal logging is unaffected. The Discord leg
    /// parses the same variable the same way.
    announce: bool,
    bridge_name: String,
    selftest_tone: bool,
    speaking_relay: bool,
}

impl Config {
    fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let owner_npub = env("ARMADA_OWNER_NPUB");
        let summon_access = auth::parse_summon_access(env("SUMMON_ACCESS").as_deref());
        let summon_whitelist =
            auth::build_whitelist(owner_npub.as_deref(), env("ARMADA_SUMMON_NPUBS").as_deref());
        let bot_npubs: HashSet<String> =
            auth::parse_id_list(env("ARMADA_BOT_NPUBS").as_deref()).into_iter().collect();
        Self {
            socket_path: env("BRIDGE_SOCKET_PATH")
                .unwrap_or_else(|| format!("{runtime}/janus/pcm.sock")),
            data_dir: env("BRIDGE_DATA_DIR")
                .unwrap_or_else(|| format!("{home}/.local/share/janus")),
            nsec: env("ARMADA_BRIDGE_NSEC"),
            owner_npub,
            summon_access,
            summon_whitelist,
            bot_npubs,
            default_voice_channel: env("ARMADA_VOICE_CHANNEL_ID"),
            text_channel: env("ARMADA_TEXT_CHANNEL_ID"),
            announce: env("JANUS_ANNOUNCE")
                .as_deref()
                .map(|v| v.trim().eq_ignore_ascii_case("on"))
                .unwrap_or(false),
            bridge_name: env("BRIDGE_NAME_ARMADA").unwrap_or_else(|| "Janus".into()),
            selftest_tone: env("BRIDGE_SELFTEST_TONE").as_deref() == Some("1"),
            speaking_relay: env("SPEAKING_RELAY").as_deref() == Some("on"),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Joining,
    Live,
}

struct Shared {
    phase: Phase,
    origin: Option<&'static str>, // "discord" | "armada"
    armada_channel_id: Option<String>,
    armada_channel_name: Option<String>,
    discord_channel_name: Option<String>,
    /// Stop flag + PCM sink of the live leg, while Live.
    leg_stop: Option<Arc<AtomicBool>>,
    leg_pcm_in: Option<mpsc::UnboundedSender<Vec<i16>>>,
    /// Pending far-side landing lookups: id -> responder.
    pending_landings: HashMap<u64, oneshot::Sender<Value>>,
    /// Pending far-side occupancy lookups: id -> responder
    /// (the human count of the Discord channel being
    /// bridged; None when the leg answers without one).
    pending_occupancy: HashMap<u64, oneshot::Sender<Option<usize>>>,
    /// Pending session_started confirmation (Armada-origin).
    pending_started: Option<oneshot::Sender<Result<(), String>>>,
    next_id: u64,
    /// Discord leg socket sink (current connection).
    discord_out: Option<mpsc::UnboundedSender<Frame>>,
    bot_ready: bool,
}

impl Shared {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            origin: None,
            armada_channel_id: None,
            armada_channel_name: None,
            discord_channel_name: None,
            leg_stop: None,
            leg_pcm_in: None,
            pending_landings: HashMap::new(),
            pending_occupancy: HashMap::new(),
            pending_started: None,
            next_id: 1,
            discord_out: None,
            bot_ready: false,
        }
    }

    fn status_json(&self) -> Value {
        json!({
            "cmd": "status",
            "state": match self.phase {
                Phase::Idle => "idle",
                Phase::Joining => "joining",
                Phase::Live => "live",
            },
            "origin": self.origin,
            "armada_channel_name": self.armada_channel_name,
            "discord_channel_name": self.discord_channel_name,
            "discord_connected": self.discord_out.is_some(),
        })
    }
}

struct App {
    cfg: Arc<Config>,
    shared: Mutex<Shared>,
    mixer: Mutex<Mixer>,
    bot: Mutex<Option<VectorBot>>,
    own_pubkey: Mutex<Option<PublicKey>>,
}

impl App {
    fn send_to_discord(&self, frame: Frame) {
        let sink = self.shared.lock().unwrap().discord_out.clone();
        if let Some(sink) = sink {
            let _ = sink.send(frame);
        }
    }

    fn send_control(&self, value: Value) {
        self.send_to_discord(Frame::control(&value.to_string()));
    }

    fn push_status(&self) {
        let status = self.shared.lock().unwrap().status_json();
        self.send_control(status);
    }
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cfg = Arc::new(Config::from_env());
    let mixer = Mixer::new();
    info!("[janus] audio path: unity passthrough (no gain staging, no limiter; hard clamp at full scale only)");
    let app = Arc::new(App {
        cfg: cfg.clone(),
        shared: Mutex::new(Shared::new()),
        mixer: Mutex::new(mixer),
        bot: Mutex::new(None),
        own_pubkey: Mutex::new(None),
    });

    if cfg.nsec.is_none() {
        warn!(
            "[janus] ARMADA_BRIDGE_NSEC not set — running WITHOUT the Armada identity: \
             socket + mixer only, summons will fail with 'not configured'. \
             (The nsec is an operator input; the kind-3313 invite is redeemed against it.)"
        );
    }
    if cfg.announce {
        info!("[janus] session announcements ON (JANUS_ANNOUNCE=on) — start/stop posts go to the Armada text channel.");
    } else {
        info!("[janus] session announcements OFF — JANUS_ANNOUNCE is not \"on\" ; sessions start/stop silently (journal only).");
    }
    if cfg.summon_access == auth::SummonAccess::Open {
        info!("[janus] SUMMON_ACCESS=open — anyone may summon/leave from the Armada side . Invite auto-accept and !bridge status stay owner-only.");
    } else if cfg.summon_whitelist.is_empty() {
        warn!("[janus] SUMMON_ACCESS=whitelist and no summon whitelist — set ARMADA_OWNER_NPUB (always authorized) and/or ARMADA_SUMMON_NPUBS; Armada-side summon commands are disabled until then (nobody is authorized).");
    }

    // Socket server + mixer tick.
    tokio::spawn(socket_server(app.clone()));
    tokio::spawn(mixer_tick(app.clone()));
    if cfg.selftest_tone {
        info!("[janus] BRIDGE_SELFTEST_TONE=1 — a synthetic Armada-side tone feeds the mixer");
        tokio::spawn(selftest_tone(app.clone()));
    }

    // Armada bot (identity) — the event loop is the process's
    // main task when configured.
    if cfg.nsec.is_some() {
        if let Err(e) = run_bot(app.clone()).await {
            error!("[janus] bot loop ended: {e:#}");
            std::process::exit(1);
        }
    } else {
        // No identity: park forever serving the socket.
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// PCM socket server
// ---------------------------------------------------------------------------

async fn socket_server(app: Arc<App>) {
    let path = app.cfg.socket_path.clone();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::remove_file(&path).await; // stale socket from a previous run
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            error!("[janus] cannot bind PCM socket {path}: {e}");
            std::process::exit(EX_CONFIG);
        }
    };
    info!("[janus] PCM socket listening at {path}");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                info!("[janus] discord leg connected");
                tokio::spawn(handle_discord_conn(app.clone(), stream));
            }
            Err(e) => warn!("[janus] socket accept failed: {e}"),
        }
    }
}

async fn handle_discord_conn(app: Arc<App>, stream: UnixStream) {
    let (mut read_half, mut write_half) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    {
        let mut sh = app.shared.lock().unwrap();
        sh.discord_out = Some(out_tx);
    }
    app.push_status();

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write_half.write_all(&frame.encode()).await.is_err() {
                break;
            }
        }
    });

    let mut decoder = StreamDecoder::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match read_half.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                decoder.feed(&buf[..n]);
                while let Some(frame) = decoder.next_frame() {
                    match frame.kind {
                        FrameType::PcmDown => {
                            let samples = frame.pcm_samples();
                            if samples.len() == FRAME_SAMPLES {
                                app.mixer.lock().unwrap().push(Side::Discord, frame.speaker_id, samples);
                            }
                        }
                        FrameType::Speak => {
                            // SPEAKING_RELAY is off in v1; edges are noted, not forwarded.
                        }
                        FrameType::Control => {
                            if let Ok(value) = serde_json::from_slice::<Value>(&frame.payload) {
                                handle_control(app.clone(), value).await;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    writer.abort();
    {
        let mut sh = app.shared.lock().unwrap();
        sh.discord_out = None;
    }
    // The Discord leg vanishing mid-session ends the session:
    // a bridge with one leg is a person talking to a wall.
    if app.shared.lock().unwrap().phase == Phase::Live {
        end_session(&app, "the discord leg disconnected", false).await;
    }
    info!("[janus] discord leg disconnected");
}

// ---------------------------------------------------------------------------
// Mixer tick (20 ms)
// ---------------------------------------------------------------------------

async fn mixer_tick(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seq: u32 = 0;
    let start = std::time::Instant::now();
    // Mixer telemetry (2026-11-23): the drop/conceal/underrun
    // counters existed but were never read, which is why the
    // loss-driven underrun defect (diagnosis round 2) was
    // invisible in the journal. Surface them every 30 s
    // whenever they moved.
    let mut ticks: u64 = 0;
    let mut last_telemetry = (0u64, 0u64, 0u64);
    loop {
        ticker.tick().await;
        ticks += 1;
        if ticks % 1500 == 0 {
            let m = app.mixer.lock().unwrap();
            let now = (m.dropped_frames, m.concealed_frames, m.underrun_ticks);
            if now != last_telemetry {
                info!(
                    "[janus] mixer telemetry: dropped={} concealed={} underruns={}",
                    now.0, now.1, now.2
                );
                last_telemetry = now;
            }
        }
        let (to_discord, to_armada) = {
            let mut mixer = app.mixer.lock().unwrap();
            let out = mixer.tick();
            if app.cfg.speaking_relay {
                for (side, speaker, started) in mixer.take_speaking_edges() {
                    // Relay only crosses sides: an Armada speaker's
                    // indicator belongs on Discord and vice versa.
                    // v1 ships with the relay off (spec §7).
                    let _ = (side, speaker, started);
                }
            }
            out
        };
        if let Some(frame) = to_discord {
            seq = seq.wrapping_add(1);
            let ts = start.elapsed().as_millis() as u32;
            app.send_to_discord(Frame::pcm(FrameType::MixOut, 0, seq, ts, &frame));
        }
        if let Some(frame) = to_armada {
            let sink = app.shared.lock().unwrap().leg_pcm_in.clone();
            if let Some(sink) = sink {
                let _ = sink.send(frame);
            }
        }
    }
}

/// Synthetic Armada-side input for local exercise: a 440 Hz
/// tone as a fake participant, so MIX_OUT flows to the Discord
/// leg with no Armada identity and no live call.
async fn selftest_tone(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    let mut phase = 0f64;
    loop {
        ticker.tick().await;
        let mut frame = Vec::with_capacity(FRAME_SAMPLES);
        for _ in 0..FRAME_SAMPLES {
            frame.push((phase.sin() * 6000.0) as i16);
            phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
        }
        app.mixer.lock().unwrap().push(Side::Armada, 0xA11CE, frame);
    }
}

// ---------------------------------------------------------------------------
// Control plane (Discord leg <-> this process)
// ---------------------------------------------------------------------------

async fn handle_control(app: Arc<App>, msg: Value) {
    match msg.get("cmd").and_then(|c| c.as_str()) {
        Some("hello") => {
            info!("[janus] discord leg hello: {}", msg.get("role").and_then(|r| r.as_str()).unwrap_or("?"));
            app.push_status();
        }
        Some("session_start") if msg.get("origin").and_then(|o| o.as_str()) == Some("discord") => {
            // Summoned from Discord: Node already joined the
            // summoner's channel there. Resolve OUR landing and join.
            let named = msg.get("armada_named").and_then(|n| n.as_str()).map(str::to_string);
            let discord_name = msg.get("discord_channel_name").and_then(|n| n.as_str()).map(str::to_string);
            tokio::spawn(async move {
                match summon_from_discord(app.clone(), named, discord_name).await {
                    Ok(channel_name) => {
                        app.send_control(json!({
                            "cmd": "session_started",
                            "origin": "discord",
                            "armada_channel_name": channel_name,
                        }));
                    }
                    Err(e) => {
                        app.send_control(json!({ "cmd": "session_failed", "error": e.to_string() }));
                    }
                }
            });
        }
        Some("landing") => {
            // Reply to a resolve_landing we issued (Armada-origin summon).
            if let Some(id) = msg.get("id").and_then(|i| i.as_u64()) {
                if let Some(tx) = app.shared.lock().unwrap().pending_landings.remove(&id) {
                    let _ = tx.send(msg.clone());
                }
            }
        }
        Some("occupancy") => {
            // Reply to an occupancy lookup we issued (the
            // session-presence watcher): the human count of
            // the Discord channel being bridged.
            if let Some(id) = msg.get("id").and_then(|i| i.as_u64()) {
                if let Some(tx) = app.shared.lock().unwrap().pending_occupancy.remove(&id) {
                    let humans = msg.get("humans").and_then(|h| h.as_u64()).map(|h| h as usize);
                    let _ = tx.send(humans);
                }
            }
        }
        Some("session_started") => {
            // Armada-origin summon confirmed by the Discord leg.
            if let Some(tx) = app.shared.lock().unwrap().pending_started.take() {
                let _ = tx.send(Ok(()));
            }
        }
        Some("session_failed") => {
            let error = msg.get("error").and_then(|e| e.as_str()).unwrap_or("unknown").to_string();
            if let Some(tx) = app.shared.lock().unwrap().pending_started.take() {
                let _ = tx.send(Err(error.clone()));
            }
            if app.shared.lock().unwrap().phase != Phase::Idle {
                end_session(&app, &format!("the discord side failed: {error}"), false).await;
            }
        }
        Some("session_ended") => {
            let reason = msg.get("reason").and_then(|r| r.as_str()).unwrap_or("ended from the discord side").to_string();
            end_session(&app, &reason, false).await;
        }
        Some("status_request") => app.push_status(),
        other => warn!("[janus] unknown control cmd: {other:?}"),
    }
}

/// Discord-origin summon: resolve the Armada landing, join
/// it, announce. Returns the Armada channel name.
async fn summon_from_discord(
    app: Arc<App>,
    named: Option<String>,
    discord_channel_name: Option<String>,
) -> Result<String> {
    {
        let mut sh = app.shared.lock().unwrap();
        if sh.phase != Phase::Idle {
            anyhow::bail!("a session is already active");
        }
        if !sh.bot_ready {
            anyhow::bail!("the armada identity is not connected yet (or not configured)");
        }
        sh.phase = Phase::Joining;
        sh.discord_channel_name = discord_channel_name;
    }
    let result = async {
        let channels = snapshot_voice_channels(&app).await;
        info!(
            "[janus] discord summon: armada landing snapshot [{}]",
            snapshot_summary(&channels)
        );
        let (id, name) = match resolve_landing(
            named.as_deref(),
            &channels,
            app.cfg.default_voice_channel.as_deref(),
        ) {
            Landing::Channel { id, .. } => {
                let name = channels.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default();
                (id, name)
            }
            Landing::Error(e) => anyhow::bail!("{e}"),
        };
        start_armada_leg(&app, &id, &name, "discord").await?;
        Ok::<(String, String), anyhow::Error>((id, name))
    }
    .await;
    match result {
        Ok((_id, name)) => {
            announce(&app, true).await;
            app.push_status();
            Ok(name)
        }
        Err(e) => {
            let mut sh = app.shared.lock().unwrap();
            if sh.phase == Phase::Joining {
                sh.phase = Phase::Idle;
            }
            Err(e)
        }
    }
}

/// Start the concord-voice leg for `channel_id` and wire its
/// PCM into the mixer. On success the session is Live.
async fn start_armada_leg(app: &Arc<App>, channel_id: &str, channel_name: &str, origin: &'static str) -> Result<()> {
    let author = app
        .own_pubkey
        .lock()
        .unwrap()
        .clone()
        .context("armada identity not configured (ARMADA_BRIDGE_NSEC)")?;
    let leg = concord_voice::spawn_leg(channel_id.to_string(), author);
    let concord_voice::Leg { stop, pcm_in, mut pcm_out, observed: _, task } = leg;

    // Inbound participant frames -> mixer (Armada side).
    {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some((identity, frame)) = pcm_out.recv().await {
                app.mixer.lock().unwrap().push(Side::Armada, fnv1a_64(&identity), frame);
            }
        });
    }
    // Leg end -> session end (unless we ended it ourselves).
    {
        let app = app.clone();
        let channel_id = channel_id.to_string();
        tokio::spawn(async move {
            let _ = task.await;
            let live_here = {
                let sh = app.shared.lock().unwrap();
                sh.phase == Phase::Live && sh.armada_channel_id.as_deref() == Some(channel_id.as_str())
            };
            if live_here {
                warn!("[janus] armada leg ended on its own; closing the session");
                end_session(&app, "the armada leg ended", true).await;
            }
        });
    }
    {
        let mut sh = app.shared.lock().unwrap();
        sh.phase = Phase::Live;
        sh.origin = Some(origin);
        sh.armada_channel_id = Some(channel_id.to_string());
        sh.armada_channel_name = Some(channel_name.to_string());
        sh.leg_stop = Some(stop);
        sh.leg_pcm_in = Some(pcm_in);
    }
    // Session-presence watcher:
    // Janus stays while ANY human remains in EITHER channel
    // of the session — the summoner leaving (a music bot after
    // its music summon, anyone) never ends it by itself,
    // one side emptying while the other still has people
    // never ends it, and bots never count as people (Janus
    // himself is excluded by channel_occupants; ARMADA_BOT_NPUBS
    // names the rest — bots). The session ends on its own
    // only when BOTH sides are known human-empty; a side
    // whose occupancy can't be determined counts as
    // occupied for this decision (see resolve.rs). Runs for
    // both summon origins — the Discord leg no longer ends
    // sessions unilaterally on its own side emptying.
    {
        let app = app.clone();
        let channel_id = channel_id.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let still = {
                    let sh = app.shared.lock().unwrap();
                    sh.phase == Phase::Live && sh.armada_channel_id.as_deref() == Some(channel_id.as_str())
                };
                if !still {
                    return;
                }
                let Some(bot) = app.bot.lock().unwrap().clone() else { return };
                let armada_humans =
                    humans_only(&channel_occupants(&bot, &channel_id), &app.cfg.bot_npubs).len();
                let discord_humans = discord_human_count(&app).await;
                if should_end_for_emptiness(armada_humans, discord_humans) {
                    info!("[janus] no humans left on either side of the bridge; closing the session");
                    end_session(&app, "both sides of the bridge emptied", true).await;
                    return;
                }
            }
        });
    }
    info!("[janus] armada leg live in {channel_name} ({channel_id})");
    Ok(())
}

/// Tear the session down: stop the leg, clear state, tell the
/// Discord leg (unless it already knows), announce on Armada.
async fn end_session(app: &Arc<App>, reason: &str, notify_discord: bool) {
    let (stop, was_live) = {
        let mut sh = app.shared.lock().unwrap();
        let was_live = sh.phase != Phase::Idle;
        let stop = sh.leg_stop.take();
        sh.leg_pcm_in = None;
        sh.phase = Phase::Idle;
        sh.origin = None;
        sh.armada_channel_id = None;
        sh.armada_channel_name = None;
        sh.discord_channel_name = None;
        (stop, was_live)
    };
    if let Some(stop) = stop {
        stop.store(true, Ordering::Relaxed);
    }
    app.mixer.lock().unwrap().clear();
    if was_live {
        info!("[janus] session ended: {reason}");
        if notify_discord {
            app.send_control(json!({ "cmd": "session_ended", "reason": reason }));
        }
        announce(app, false).await;
        app.push_status();
    }
}

// ---------------------------------------------------------------------------
// Armada presence snapshots (landing resolution + occupancy)
// ---------------------------------------------------------------------------

/// Snapshot every voice channel's occupancy. Occupants are
/// member npubs vector-core currently sees in the call,
/// the bridge itself excluded.
///
/// Voice-channel detection uses [`is_voice_channel`] —
/// native view OR the Discord-import "-voice" suffix. The
/// original view-only filter matched NOTHING in the owner's
/// imported community, so every snapshot was empty and
/// every Armada summon failed "not in a voice channel"
/// (live-diagnosed 2026-11-22).
async fn snapshot_voice_channels(app: &Arc<App>) -> Vec<ChannelOccupancy> {
    let bot = app.bot.lock().unwrap().clone();
    let Some(bot) = bot else {
        warn!("[janus] occupancy snapshot asked for before the bot exists");
        return Vec::new();
    };
    let mut out = Vec::new();
    for community in bot.communities().await {
        let infos = match community.channel_infos() {
            Ok(infos) => infos,
            Err(e) => {
                warn!("[janus] channel_infos failed for a community: {e}");
                continue;
            }
        };
        for info in infos.iter().filter(|i| is_voice_channel(&i.name, &i.view)) {
            let occupants = channel_occupants(&bot, &info.channel_id);
            out.push(ChannelOccupancy {
                id: info.channel_id.clone(),
                name: info.name.clone(),
                occupants,
            });
        }
    }
    out
}

/// Ask the Discord leg how many HUMANS are in the Discord
/// channel this session is bridging (its count excludes
/// bots, itself included). None when the leg is gone or
/// doesn't answer in time — the caller treats unknown as
/// "not known empty", never as zero.
async fn discord_human_count(app: &Arc<App>) -> Option<usize> {
    let (id, rx) = {
        let mut sh = app.shared.lock().unwrap();
        if sh.discord_out.is_none() {
            return None;
        }
        let id = sh.next_id;
        sh.next_id += 1;
        let (tx, rx) = oneshot::channel();
        sh.pending_occupancy.insert(id, tx);
        (id, rx)
    };
    app.send_control(json!({ "cmd": "occupancy", "id": id }));
    match tokio::time::timeout(Duration::from_secs(3), rx).await {
        Ok(Ok(count)) => count,
        _ => {
            app.shared.lock().unwrap().pending_occupancy.remove(&id);
            None
        }
    }
}

/// Who is in one channel's call now (npubs), bridge
/// excluded. The source is the SDK's `voice_members()` —
/// vector-core's folded kind-23313 voice presence, kept
/// current by the bot's own subscription while it listens
/// (the same source the reference bot reads). The hand-rolled relay
/// fetch this replaced (probe_channel_keys + a cold
/// observe_once) could never see anyone: presence wraps
/// are kind-21059 EPHEMERAL events, relays do not store
/// them, and the EOSE fetch folded zero wraps on every
/// pass — live probe, 2026-11-22, with the owner verifiably
/// in the channel the whole time.
fn channel_occupants(bot: &VectorBot, channel_hex: &str) -> Vec<String> {
    let own = bot.npub();
    bot.channel(channel_hex)
        .voice_members()
        .into_iter()
        .map(|m| m.npub)
        .filter(|npub| npub != &own)
        .collect()
}

// ---------------------------------------------------------------------------
// Announcements (Armada text channel)
// ---------------------------------------------------------------------------

async fn announce(app: &Arc<App>, started: bool) {
    if !app.cfg.announce {
        info!("[janus] session announcement suppressed (JANUS_ANNOUNCE is not \"on\")");
        return;
    }
    let bot = app.bot.lock().unwrap().clone();
    let Some(bot) = bot else { return };
    let (armada_name, discord_name) = {
        let sh = app.shared.lock().unwrap();
        (sh.armada_channel_name.clone(), sh.discord_channel_name.clone())
    };
    let name = app.cfg.bridge_name.clone();
    let text = if started {
        format!(
            "🔊 **{name} is live** — Armada voice{} is now bridged with Discord voice{}. \
             While the bridge is up, audio leaves both platforms' end-to-end encryption.",
            armada_name.as_deref().map(|n| format!(" (**{n}**)")).unwrap_or_default(),
            discord_name.as_deref().map(|n| format!(" (**{n}**)")).unwrap_or_default(),
        )
    } else {
        format!("🔇 **{name} left** — the voice bridge is closed. This channel is private again.")
    };
    // Configured text channel, else the community's
    // "general-chat" if there is one.
    let target = match &app.cfg.text_channel {
        Some(id) => Some(id.clone()),
        None => find_text_channel(&bot, "general-chat").await,
    };
    if let Some(id) = target {
        if let Err(e) = bot.channel(&id).send(&text).await {
            warn!("[janus] armada announcement failed: {e}");
        }
    }
}

async fn find_text_channel(bot: &VectorBot, name: &str) -> Option<String> {
    for community in bot.communities().await {
        if let Ok(infos) = community.channel_infos() {
            if let Some(info) = infos.iter().find(|i| i.view == "chat" && i.name.eq_ignore_ascii_case(name)) {
                return Some(info.channel_id.clone());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The Armada bot: identity, profile, summon commands
// ---------------------------------------------------------------------------

async fn run_bot(app: Arc<App>) -> Result<()> {
    let cfg = app.cfg.clone();
    let nsec = cfg.nsec.clone().context("run_bot without nsec")?;
    let mut builder = VectorBot::builder().data_dir(&cfg.data_dir).key(nsec);
    if let Some(owner) = &cfg.owner_npub {
        // Auto-accept community invites from the owner only — this
        // is how the kind-3313 invite he generates gets
        // redeemed with no human in the loop.
        builder = builder.whitelist(vec![owner.clone()]);
    }
    let bot = builder.build().await.context("building the armada identity")?;
    info!("[janus] armada identity online as {}", bot.npub());
    if let Ok(pk) = PublicKey::from_bech32(bot.npub()) {
        *app.own_pubkey.lock().unwrap() = Some(pk);
    }
    *app.bot.lock().unwrap() = Some(bot.clone());
    if let Err(e) = bot
        .update_profile(&cfg.bridge_name, "", "", "Janus — the Discord ↔ Armada voice bridge. I carry audio when summoned; I do not listen otherwise.")
        .await
    {
        warn!("[janus] profile publish failed: {e}");
    }

    let app2 = app.clone();
    bot.on_event(move |bot, event| {
        let app = app2.clone();
        async move {
            match event {
                BotEvent::Ready { communities } => {
                    app.shared.lock().unwrap().bot_ready = true;
                    info!("[janus] subscribed and listening ({communities} communities)");
                    for community in bot.communities().await {
                        if let Ok(infos) = community.channel_infos() {
                            let voice: Vec<&str> =
                                infos.iter().filter(|i| is_voice_channel(&i.name, &i.view)).map(|i| i.name.as_str()).collect();
                            info!("[janus] community {} voice channels: {:?}", &community.id()[..8], voice);
                        }
                    }
                }
                BotEvent::Message(msg) => handle_chat_message(app, bot, msg).await,
                _ => {}
            }
        }
    })
    .await?;
    Ok(())
}

/// What a community-channel message asked for, if anything.
enum ChatAction {
    Leave,
    Status,
    Summon(Option<String>),
}

/// Chat commands in community channels:
/// `!summon [discord channel name]`, `!leave`, `!bridge status`
/// — plus conversational triggers (the owner's order, 2026-11-22:
/// "the command for janus to join and leave should be
/// conversational"): plain messages like "janus join VC 2"
/// or "janus leave" work too, via converse.rs. Conversational
/// is the primary surface; the `!` commands are backstops and
/// behave identically (same session flow, same announcements,
/// same authorization). There is NO conversational status:
/// `!bridge status` stays command-only and owner-only.
/// Summon/leave authorization follows SUMMON_ACCESS (see
/// auth.rs): open mode (the owner's deployment) lets ANYONE
/// summon/leave; whitelist mode restricts them to the summon
/// whitelist (the owner + ARMADA_SUMMON_NPUBS friends).
/// Non-command, non-trigger messages from anyone are ignored
/// silently — the denial below only answers an actual
/// summon/leave attempt (command or conversational) by a
/// non-whitelisted member in whitelist mode (it never fires
/// in open mode).
async fn handle_chat_message(app: Arc<App>, _bot: VectorBot, msg: IncomingMessage) {
    if msg.is_mine() || !msg.is_group {
        return;
    }
    let sender = msg.message.npub.clone();
    let is_owner = matches!(
        (sender.as_deref(), app.cfg.owner_npub.as_deref()),
        (Some(s), Some(t)) if s == t
    );
    let authorized =
        auth::is_authorized(app.cfg.summon_access, &app.cfg.summon_whitelist, sender.as_deref());
    let text = msg.text().trim().to_string();
    let lower = text.to_lowercase();
    let channel = msg.channel();
    const DENIAL: &str =
        "Bridge summon and leave are for whitelisted members only — ask the owner to add your npub to the whitelist.";

    // Resolve the message to an action: explicit `!` commands
    // first (backstops), then the conversational matcher.
    let action = if lower == "!leave" || lower == "!bridge leave" {
        Some(ChatAction::Leave)
    } else if lower == "!bridge status" {
        Some(ChatAction::Status)
    } else if lower == "!summon" || lower.starts_with("!summon ") {
        let named = text["!summon".len()..].trim().trim_start_matches('#').to_string();
        Some(ChatAction::Summon(if named.is_empty() { None } else { Some(named) }))
    } else {
        match converse::parse_conversational(&text) {
            Some(converse::ConverseCommand::Leave) => Some(ChatAction::Leave),
            Some(converse::ConverseCommand::Join { named }) => Some(ChatAction::Summon(named)),
            None => None,
        }
    };

    match action {
        None => {}
        Some(ChatAction::Leave) => {
            if !authorized {
                let _ = channel.send(DENIAL).await;
                return;
            }
            if app.shared.lock().unwrap().phase == Phase::Idle {
                let _ = channel.send("No bridge session is running.").await;
            } else {
                end_session(&app, "leave command (armada side)", true).await;
                let _ = channel.send("Bridge closed.").await;
            }
        }
        Some(ChatAction::Status) => {
            if !is_owner {
                return;
            }
            let status = app.shared.lock().unwrap().status_json();
            let _ = channel
                .send(&format!(
                    "Janus: {} (armada: {}, discord: {})",
                    status["state"].as_str().unwrap_or("?"),
                    status["armada_channel_name"].as_str().unwrap_or("—"),
                    status["discord_channel_name"].as_str().unwrap_or("—"),
                ))
                .await;
        }
        Some(ChatAction::Summon(named)) => {
            if !authorized {
                let _ = channel.send(DENIAL).await;
                return;
            }
            let Some(summoner) = sender else { return };
            let reply_channel = channel;
            tokio::spawn(async move {
                match summon_from_armada(app.clone(), named, summoner).await {
                    Ok((armada_name, discord_name)) => {
                        let _ = reply_channel
                            .send(&format!("🔊 Bridged: Armada **{armada_name}** ↔ Discord **{discord_name}**. Audio is leaving both platforms' encryption while this runs. `!leave` ends it."))
                            .await;
                    }
                    Err(e) => {
                        let _ = reply_channel.send(&format!("Could not summon the bridge: {e}")).await;
                    }
                }
            });
        }
    }
}

/// Armada-origin summon: join the voice channel the SUMMONER
/// (whoever summoned — in open mode, anyone) is in here, ask
/// the Discord leg to resolve its landing, confirm.
async fn summon_from_armada(
    app: Arc<App>,
    named: Option<String>,
    summoner_npub: String,
) -> Result<(String, String)> {
    {
        let mut sh = app.shared.lock().unwrap();
        if sh.phase != Phase::Idle {
            anyhow::bail!("a session is already active");
        }
        if !sh.bot_ready {
            anyhow::bail!("the armada identity is not connected yet");
        }
        if sh.discord_out.is_none() {
            anyhow::bail!("the discord leg is not connected");
        }
        sh.phase = Phase::Joining;
    }
    let outcome = async {
        // Where is the summoner? The voice channel whose
        // occupancy includes them. The snapshot is ALWAYS
        // logged: the 2026-11-22 blindness (empty snapshot,
        // every summon failing) hid behind silence here.
        let channels = snapshot_voice_channels(&app).await;
        let mine = channels
            .iter()
            .find(|c| c.occupants.iter().any(|o| o == &summoner_npub));
        info!(
            "[janus] armada summon by {summoner_npub}: snapshot [{}]; summoner {}",
            snapshot_summary(&channels),
            if mine.is_some() { "found" } else { "NOT found" },
        );
        let mine = mine.context("you are not in a voice channel here — join one, then summon")?;
        let (my_channel_id, my_channel_name) = (mine.id.clone(), mine.name.clone());

        // Ask the Discord leg where IT lands.
        let (id, landing_rx) = {
            let mut sh = app.shared.lock().unwrap();
            let id = sh.next_id;
            sh.next_id += 1;
            let (tx, rx) = oneshot::channel();
            sh.pending_landings.insert(id, tx);
            (id, rx)
        };
        app.send_control(json!({ "cmd": "resolve_landing", "id": id, "named": named }));
        let landing = tokio::time::timeout(Duration::from_secs(5), landing_rx)
            .await
            .map_err(|_| anyhow::anyhow!("the discord leg did not answer the landing lookup"))?
            .map_err(|_| anyhow::anyhow!("the discord leg went away mid-summon"))?;
        if let Some(err) = landing.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("discord landing: {err}");
        }
        let discord_channel_id = landing
            .get("channel_id")
            .and_then(|c| c.as_str())
            .context("discord leg answered without a channel")?
            .to_string();
        let discord_channel_name = landing
            .get("channel_name")
            .and_then(|c| c.as_str())
            .unwrap_or("voice")
            .to_string();

        // Join our side, then tell Discord to join theirs.
        start_armada_leg(&app, &my_channel_id, &my_channel_name, "armada").await?;
        app.shared.lock().unwrap().discord_channel_name = Some(discord_channel_name.clone());
        let (started_tx, started_rx) = oneshot::channel();
        app.shared.lock().unwrap().pending_started = Some(started_tx);
        app.send_control(json!({
            "cmd": "session_start",
            "origin": "armada",
            "armada_channel_name": my_channel_name,
            "discord_channel_id": discord_channel_id,
        }));
        match tokio::time::timeout(Duration::from_secs(10), started_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => anyhow::bail!("discord side failed: {e}"),
            _ => anyhow::bail!("the discord leg did not confirm the session start"),
        }
        Ok::<(String, String), anyhow::Error>((my_channel_name, discord_channel_name))
    }
    .await;
    match outcome {
        Ok(names) => {
            announce(&app, true).await;
            app.push_status();
            Ok(names)
        }
        Err(e) => {
            // Roll back any half-started leg.
            end_session(&app, "summon failed", true).await;
            Err(e)
        }
    }
}

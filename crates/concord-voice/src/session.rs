//! One LiveKit room session: the webrtc-rs peer connections,
//! the signaling conversation that drives them, the outbound
//! frame clock (PCM → Opus → E2EE → RTP), and the inbound path
//! (RTP → E2EE → Opus → 20 ms PCM frames out).
//!
//! Lifted from the reference bot's `livekit::room`
//! (live-proven against this SFU on 2026-11-22, including the
//! RFC 4571 ICE-TCP bridge below) and re-pointed at the
//! bridge's PCM contract: inbound audio leaves as raw
//! per-participant 20 ms frames (no transcription, no
//! segmentation), outbound audio arrives as mixed 20 ms frames
//! from the bridge mixer.
//!
//! The peer-connection choreography follows the LiveKit
//! protocol (Apache-2.0 spec): the client publishes on one PC
//! (AddTrackRequest first; the offer goes out when the SFU
//! confirms the track — TrackPublished — with a timed
//! fallback) and receives on a second PC the SFU offers to.
//! A failure anywhere in this module ends the session only;
//! the supervisor (see supervisor.rs) decides what happens
//! next.

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;
use livekit_protocol::{signal_response, ParticipantInfo, SignalTarget};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::header::Header as RtpHeader;
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use super::cryptor::{decrypt_frame, FrameCryptor};
use super::keys::ChannelKeys;
use super::signal::SignalClient;

/// Samples in one 20 ms frame at 48 kHz — the bridge's one
/// clock (spec §4.1). Everything PCM in this crate moves in
/// frames of exactly this many mono samples.
pub const FRAME_SAMPLES: usize = 960;

/// How a room session ended — the supervisor decides what each
/// means (stop: done; rekey: rejoin at once; failure: back off).
pub enum RoomEnd {
    Stopped,
    Rekey,
    Failed(anyhow::Error),
}

/// The Opus payload type a negotiated SDP picked (default 111).
pub fn opus_payload_type(sdp: &str) -> u8 {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if rest.contains("opus/48000") {
                if let Some((pt, _)) = rest.split_once(' ') {
                    if let Ok(pt) = pt.trim().parse::<u8>() {
                        return pt;
                    }
                }
            }
        }
    }
    111
}

/// The `a=mid:` of the (single) audio m-section in an SDP.
pub fn audio_mid(sdp: &str) -> Option<String> {
    let mut in_audio = false;
    for line in sdp.lines() {
        let line = line.trim();
        if line.starts_with("m=") {
            in_audio = line.starts_with("m=audio");
            continue;
        }
        if in_audio {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                return Some(mid.trim().to_string());
            }
        }
    }
    None
}

/// The one Opus capability this leg speaks. It is registered
/// in the media engine AND handed to the publish track, so the
/// two can never disagree. (The 2026-11-22 live failure: the
/// API was built bare — `APIBuilder::new().build()` registers
/// NO codecs at all — so the publisher transceiver had no
/// codecs and `create_offer` failed with
/// `ErrSenderWithNoCodecs`, hidden behind a context wrap.)
fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_string(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
        rtcp_feedback: Vec::new(),
    }
}

/// Which local IPs may become ICE host candidates: routable
/// IPv4, plus loopback. Loopback is never trickled to the SFU
/// (see candidate_json) — it exists so the agent can pair
/// with the synthetic loopback candidate the ICE-TCP bridge
/// registers for the SFU's passive TCP candidate (see
/// add_remote_candidate). Link-local/APIPA addresses belong
/// to dead interfaces, and IPv6 is excluded wholesale (the
/// setting engine is UDPv4-only — see below).
fn is_usable_ice_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || (!v4.is_link_local()
                    && !v4.is_unspecified()
                    && !v4.is_multicast()
                    && !v4.is_broadcast())
        }
        IpAddr::V6(_) => false,
    }
}

/// Build the webrtc API for both room PCs:
/// - media engine with exactly our Opus codec registered
///   (payload type 111, as LiveKit numbers it);
/// - the default interceptor chain (RTCP reports, NACK —
///   without it the SFU sees a track that never answers RTCP);
/// - a setting engine restricted to UDP/IPv4 with mDNS off.
///   The default engine also gathers IPv6 + TCP: against the
///   SFU's IPv4-literal STUN server that produced
///   "failed to resolve stun host … No available ipv6 IP
///   address found" and fe80 bind errors in the live log, and
///   on a bad day leaves the agent with no usable pairs.
///   Loopback candidates are included (only) so the ICE-TCP
///   bridge's synthetic candidate has a local partner.
fn build_webrtc_api() -> Result<(webrtc::api::API, RTCRtpCodecCapability)> {
    let opus_cap = opus_capability();
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                capability: opus_cap.clone(),
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .context("registering the opus codec")?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .context("registering the default interceptors")?;
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_network_types(vec![NetworkType::Udp4]);
    setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    setting_engine.set_include_loopback_candidate(true);
    setting_engine.set_ip_filter(Box::new(is_usable_ice_ip));
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();
    Ok((api, opus_cap))
}

fn ice_config(join: &livekit_protocol::JoinResponse) -> RTCConfiguration {
    // UDPv4-only (see build_webrtc_api): bracketed IPv6-literal
    // STUN/TURN URLs from the join can only fail resolution, so
    // drop them — and drop a server left with no URLs at all
    // (an empty server fails PC configuration validation).
    let ice_servers = join
        .ice_servers
        .iter()
        .filter_map(|s| {
            let urls: Vec<String> = s.urls.iter().filter(|u| !u.contains('[')).cloned().collect();
            if urls.is_empty() {
                None
            } else {
                Some(RTCIceServer {
                    urls,
                    username: s.username.clone(),
                    credential: s.credential.clone(),
                    ..Default::default()
                })
            }
        })
        .collect();
    RTCConfiguration { ice_servers, ..Default::default() }
}

/// webrtc-rs emits `sdp_mid: Some("")` for gathered candidates;
/// the SFU (Pion) matches trickled candidates against the
/// negotiated mids and rejects an empty one. Each of our PCs
/// has exactly one m-section, mid "0".
fn normalize_trickle_init(mut init: RTCIceCandidateInit) -> RTCIceCandidateInit {
    if init.sdp_mid.as_deref().map(str::is_empty).unwrap_or(false) {
        init.sdp_mid = Some("0".to_string());
    }
    init
}

fn candidate_json(candidate: &RTCIceCandidate) -> Option<String> {
    // Loopback candidates are strictly local (they pair with
    // the ICE-TCP bridge's synthetic candidate); the SFU must
    // never see them.
    if candidate
        .address
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
    {
        return None;
    }
    candidate
        .to_json()
        .ok()
        .map(normalize_trickle_init)
        .and_then(|init| serde_json::to_string(&init).ok())
}

/// The mid→cid map a publisher offer carries: every local
/// transceiver mid points at our one track. Falls back to the
/// SDP's own audio mid, then to "0" — an offer must never go
/// out with an empty map (the SFU binds the track by it).
fn mid_map_from_mids(mids: &[String], sdp: &str, cid: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for mid in mids {
        map.insert(mid.clone(), cid.to_string());
    }
    if map.is_empty() {
        if let Some(mid) = audio_mid(sdp) {
            map.insert(mid, cid.to_string());
        }
    }
    if map.is_empty() {
        map.insert("0".to_string(), cid.to_string());
    }
    map
}

/// Add one of the SFU's candidates to a PC, logging the
/// outcome at info (a silent drop here cost us a whole live
/// test in round 3 — the log must show every candidate's
/// fate).
///
/// webrtc-ice speaks UDP only, so the SFU's passive TCP host
/// candidate (`... 1 tcp ... 164.92.99.77 7881 typ host
/// tcptype passive`) can never pair as-is. For those we spawn
/// a loopback UDP ↔ RFC 4571 length-prefixed TCP bridge and
/// register a synthetic srflx candidate pointing at the
/// bridge's local port: the agent's ordinary UDP checks then
/// flow over TCP to the SFU. When the direct UDP path works
/// the host pairs win and the bridge sits idle; when UDP is
/// blackholed (the 2026-11-22 round-3 failure: pairs formed
/// on both PCs, no check ever succeeded, SFU killed the
/// session at its establishment timeout) the TCP path is an
/// independent second route in.
async fn add_remote_candidate(
    pc: &Arc<RTCPeerConnection>,
    init: RTCIceCandidateInit,
    which: &str,
    bridges: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    if let Some((foundation, priority, host, port)) = parse_passive_tcp_candidate(&init.candidate) {
        match spawn_ice_tcp_bridge(host.clone(), port).await {
            Ok((bridge_port, handle)) => {
                bridges.push(handle);
                let synth = RTCIceCandidateInit {
                    candidate: format!(
                        "candidate:{foundation} 1 udp {priority} 127.0.0.1 {bridge_port} typ srflx raddr 127.0.0.1 rport {bridge_port}"
                    ),
                    sdp_mid: Some("0".to_string()),
                    sdp_mline_index: Some(0),
                    username_fragment: init.username_fragment.clone(),
                };
                match pc.add_ice_candidate(synth).await {
                    Ok(()) => info!(
                        "[janus] {which} remote TCP candidate {host}:{port} bridged via 127.0.0.1:{bridge_port}"
                    ),
                    Err(e) => warn!(
                        "[janus] adding the bridged {which} candidate (for {host}:{port}) failed: {e}"
                    ),
                }
            }
            Err(e) => {
                warn!("[janus] spawning the ICE-TCP bridge for the {which} candidate {host}:{port} failed: {e:#}")
            }
        }
        return;
    }
    match pc.add_ice_candidate(init.clone()).await {
        Ok(()) => info!("[janus] {which} remote ICE candidate added: {}", init.candidate),
        Err(e) => {
            warn!("[janus] adding a {which} remote ICE candidate failed: {e} (candidate: {})", init.candidate)
        }
    }
}

/// Parse an SDP candidate string if it is a component-1
/// passive (or simultaneous-open) IPv4 TCP host candidate —
/// the shape an SFU's ICE-TCP mux advertises. Returns
/// (foundation, priority, host, port).
fn parse_passive_tcp_candidate(raw: &str) -> Option<(String, u32, String, u16)> {
    let body = raw.trim().strip_prefix("candidate:").unwrap_or(raw.trim());
    let parts: Vec<&str> = body.split_whitespace().collect();
    if parts.len() < 8 {
        return None;
    }
    let foundation = parts[0].to_string();
    let component: u16 = parts[1].parse().ok()?;
    if component != 1 || !parts[2].eq_ignore_ascii_case("tcp") {
        return None;
    }
    let priority: u32 = parts[3].parse().ok()?;
    let host = parts[4].to_string();
    if host.contains(':') {
        return None; // IPv6 candidate; this leg is IPv4-only
    }
    let port: u16 = parts[5].parse().ok()?;
    if port == 0 || port == 9 {
        return None;
    }
    // A `tcptype active` candidate wants US to listen; there
    // is nothing to dial, so it cannot be bridged.
    let mut i = 8;
    while i + 1 < parts.len() {
        if parts[i].eq_ignore_ascii_case("tcptype") {
            if parts[i + 1].eq_ignore_ascii_case("active") {
                return None;
            }
            break;
        }
        i += 1;
    }
    Some((foundation, priority, host, port))
}

/// Spawn the loopback half of an ICE-TCP bridge: a UDP socket
/// on 127.0.0.1 the local ICE agent talks to, forwarded over
/// one TCP connection to the SFU's ICE-TCP port with RFC 4571
/// 2-byte big-endian length framing. The TCP connection is
/// dialed lazily on the first packet from the agent (an idle
/// bridge never touches the network) and the bridge dies with
/// its task handle at session teardown.
async fn spawn_ice_tcp_bridge(
    host: String,
    port: u16,
) -> Result<(u16, tokio::task::JoinHandle<()>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let udp = Arc::new(
        tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .context("binding the ICE-TCP bridge's loopback UDP socket")?,
    );
    let bridge_port = udp.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let mut ubuf = vec![0u8; 16_384];
        // Wait for the agent's first check; if it never comes
        // (native UDP won, or the session ended) die quietly.
        let Ok(Ok((first_n, owner_addr))) =
            tokio::time::timeout(Duration::from_secs(30), udp.recv_from(&mut ubuf)).await
        else {
            return;
        };
        let Ok(Ok(stream)) =
            tokio::time::timeout(Duration::from_secs(8), tokio::net::TcpStream::connect((host.as_str(), port)))
                .await
        else {
            return;
        };
        let _ = stream.set_nodelay(true);
        let (mut tcp_read, mut tcp_write) = stream.into_split();
        // The first packet goes out in one coalesced framed write.
        let mut frame_out = Vec::with_capacity(2 + first_n);
        frame_out.extend_from_slice(&(first_n as u16).to_be_bytes());
        frame_out.extend_from_slice(&ubuf[..first_n]);
        if tcp_write.write_all(&frame_out).await.is_err() {
            return;
        }
        let udp_to_tcp = {
            let udp = Arc::clone(&udp);
            async move {
                let mut pkt_buf = vec![0u8; 16_384];
                let mut out_buf = Vec::with_capacity(2048);
                loop {
                    let Ok(Ok((n, src))) =
                        tokio::time::timeout(Duration::from_secs(60), udp.recv_from(&mut pkt_buf))
                            .await
                    else {
                        break;
                    };
                    // Bound 1-to-1 to the agent socket that
                    // claimed this bridge.
                    if src != owner_addr || n == 0 || n > u16::MAX as usize {
                        continue;
                    }
                    out_buf.clear();
                    out_buf.extend_from_slice(&(n as u16).to_be_bytes());
                    out_buf.extend_from_slice(&pkt_buf[..n]);
                    if tcp_write.write_all(&out_buf).await.is_err() {
                        break;
                    }
                }
            }
        };
        let tcp_to_udp = {
            let udp = Arc::clone(&udp);
            async move {
                let mut len_hdr = [0u8; 2];
                let mut frame_buf = vec![0u8; 65_536];
                loop {
                    let Ok(Ok(_)) = tokio::time::timeout(
                        Duration::from_secs(60),
                        tcp_read.read_exact(&mut len_hdr),
                    )
                    .await
                    else {
                        break;
                    };
                    let frame_len = u16::from_be_bytes(len_hdr) as usize;
                    if frame_len == 0 {
                        continue;
                    }
                    if tcp_read.read_exact(&mut frame_buf[..frame_len]).await.is_err() {
                        break;
                    }
                    if udp.send_to(&frame_buf[..frame_len], owner_addr).await.is_err() {
                        break;
                    }
                }
            }
        };
        tokio::select! {
            _ = udp_to_tcp => {}
            _ = tcp_to_udp => {}
        }
    });
    Ok((bridge_port, handle))
}

/// Which peer connection a trickled candidate belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IcePc {
    Publisher,
    Subscriber,
}

/// The remote-candidate gate: candidates that arrive before
/// their PC's remote description exists are queued (adding
/// them early is an error) and drained, in arrival order,
/// the moment it lands. Routing follows the LiveKit signal
/// target: SUBSCRIBER (1) → the subscriber PC, anything else
/// (PUBLISHER=0) → the publisher PC.
#[derive(Default)]
struct CandidateGate {
    pub_remote_set: bool,
    sub_remote_set: bool,
    pub_queued: Vec<RTCIceCandidateInit>,
    sub_queued: Vec<RTCIceCandidateInit>,
}

impl CandidateGate {
    /// Route one incoming trickle. Returns Some((pc, init))
    /// when the candidate may be added immediately, None when
    /// it was queued behind the remote description.
    fn push(&mut self, target: i32, init: RTCIceCandidateInit) -> Option<(IcePc, RTCIceCandidateInit)> {
        if target == SignalTarget::Subscriber as i32 {
            if self.sub_remote_set {
                Some((IcePc::Subscriber, init))
            } else {
                self.sub_queued.push(init);
                None
            }
        } else if self.pub_remote_set {
            Some((IcePc::Publisher, init))
        } else {
            self.pub_queued.push(init);
            None
        }
    }

    /// Mark a PC's remote description as applied and drain
    /// everything queued for it, in arrival order.
    fn remote_set(&mut self, pc: IcePc) -> Vec<RTCIceCandidateInit> {
        match pc {
            IcePc::Publisher => {
                self.pub_remote_set = true;
                self.pub_queued.drain(..).collect()
            }
            IcePc::Subscriber => {
                self.sub_remote_set = true;
                self.sub_queued.drain(..).collect()
            }
        }
    }
}

/// One-line ICE diagnostics for a PC: every candidate pair
/// with its endpoints, state, and STUN request/response
/// counters — the difference between "checks are leaving"
/// and "answers are coming back" is visible at a glance.
async fn ice_stats_summary(pc: &Arc<RTCPeerConnection>) -> String {
    let report = pc.get_stats().await;
    let mut locals: HashMap<String, String> = HashMap::new();
    let mut remotes: HashMap<String, String> = HashMap::new();
    let mut pairs: Vec<String> = Vec::new();
    for (_, t) in &report.reports {
        match t {
            webrtc::stats::StatsReportType::LocalCandidate(c) => {
                locals.insert(
                    c.id.clone(),
                    format!("{}:{} {:?}", c.ip, c.port, c.candidate_type),
                );
            }
            webrtc::stats::StatsReportType::RemoteCandidate(c) => {
                remotes.insert(
                    c.id.clone(),
                    format!("{}:{} {:?}", c.ip, c.port, c.candidate_type),
                );
            }
            webrtc::stats::StatsReportType::CandidatePair(p) => {
                let l = locals
                    .get(&p.local_candidate_id)
                    .map(String::as_str)
                    .unwrap_or("?");
                let r = remotes
                    .get(&p.remote_candidate_id)
                    .map(String::as_str)
                    .unwrap_or("?");
                pairs.push(format!(
                    "{l} -> {r}: {:?} nominated={} req_sent={} req_recv={} resp_sent={} resp_recv={}",
                    p.state, p.nominated, p.requests_sent, p.requests_received, p.responses_sent,
                    p.responses_received
                ));
            }
            _ => {}
        }
    }
    if pairs.is_empty() {
        "no candidate pairs".to_string()
    } else {
        pairs.join("; ")
    }
}

/// Count `a=candidate:` lines in an SDP (the audit that tells
/// us whether the SFU embeds candidates or trickles only).
fn sdp_candidate_lines(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.trim_start().starts_with("a=candidate:")).count()
}

/// Log one received signal message in the session tally.
fn note_signal(counts: &mut HashMap<&'static str, u64>, total: &mut u64, kind: &'static str) {
    *total += 1;
    let n = counts.entry(kind).or_insert(0);
    *n += 1;
    info!("[janus] signal #{total} {kind} (×{n} this session)");
}

/// Run one room session to its end. `pcm_in` carries mixed
/// 20 ms frames to publish (fed by the bridge mixer); `pcm_out`
/// receives one `(sfu_identity, frame)` per inbound 20 ms frame
/// per participant. `keys` is the channel key set this session
/// joined with — a rekey ends the session so the supervisor
/// can rejoin on the new room.
#[allow(clippy::too_many_arguments)]
pub async fn run_room(
    keys: Arc<ChannelKeys>,
    sfu_url: String,
    token: String,
    own_identity: String,
    stop: Arc<AtomicBool>,
    pcm_in: mpsc::UnboundedReceiver<Vec<i16>>,
    pcm_out: mpsc::UnboundedSender<(String, Vec<i16>)>,
) -> RoomEnd {
    match run_room_inner(keys, sfu_url, token, own_identity, stop, pcm_in, pcm_out).await {
        Ok(end) => end,
        Err(e) => RoomEnd::Failed(e),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_room_inner(
    keys: Arc<ChannelKeys>,
    sfu_url: String,
    token: String,
    own_identity: String,
    stop: Arc<AtomicBool>,
    pcm_in: mpsc::UnboundedReceiver<Vec<i16>>,
    pcm_out: mpsc::UnboundedSender<(String, Vec<i16>)>,
) -> Result<RoomEnd> {
    let channel = keys.channel_id.0.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut signal = SignalClient::connect(&sfu_url, &token).await?;
    info!(
        "[janus] joined room {} as {own_identity} ({} other participant(s))",
        keys.room,
        signal.join.other_participants.len()
    );
    {
        let servers: Vec<String> = signal
            .join
            .ice_servers
            .iter()
            .flat_map(|s| s.urls.iter().cloned())
            .collect();
        info!(
            "[janus] join: subscriber_primary={} ping_interval={}s ice_servers={servers:?}",
            signal.join.subscriber_primary, signal.join.ping_interval
        );
    }

    // track sid → SFU identity, from the join + live updates.
    let track_owners: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut map = track_owners.lock().unwrap();
        for p in &signal.join.other_participants {
            note_participant(&mut map, p);
        }
    }

    let (api, opus_cap) = build_webrtc_api()?;
    let config = ice_config(&signal.join);

    // ── Publisher PC ─────────────────────────────────────────
    let pub_connected = Arc::new(AtomicBool::new(false));
    let pub_pc = Arc::new(api.new_peer_connection(config.clone()).await?);
    {
        let pub_connected = Arc::clone(&pub_connected);
        pub_pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            if s == RTCPeerConnectionState::Connected {
                pub_connected.store(true, Ordering::Relaxed);
            }
            match s {
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected => {
                    warn!("[janus] publisher PC state: {s:?}");
                }
                _ => info!("[janus] publisher PC state: {s:?}"),
            }
            Box::pin(async move {})
        }));
    }
    pub_pc.on_ice_connection_state_change(Box::new(move |s| {
        info!("[janus] publisher ICE state: {s:?}");
        Box::pin(async move {})
    }));
    let (negotiate_tx, mut negotiate_rx) = mpsc::unbounded_channel::<()>();
    {
        let negotiate_tx = negotiate_tx.clone();
        pub_pc.on_negotiation_needed(Box::new(move || {
            let negotiate_tx = negotiate_tx.clone();
            Box::pin(async move {
                let _ = negotiate_tx.send(());
            })
        }));
    }
    // ICE trickle needs the signal sender before `signal` moves
    // into the loop: route through a channel the loop drains.
    let (out_sig_tx, mut out_sig_rx) = mpsc::unbounded_channel::<livekit_protocol::signal_request::Message>();
    wire_ice(&pub_pc, SignalTarget::Publisher, "publisher", out_sig_tx.clone());
    let track = Arc::new(TrackLocalStaticRTP::new(
        opus_cap,
        "janus-audio".to_string(),
        "janus-stream".to_string(),
    ));
    let cid = format!("TR_{}", rand::random::<u64>());
    signal.send_add_audio_track(&cid, "microphone")?;
    let rtp_sender = pub_pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .context("adding the publish track")?;
    // Drain the sender's RTCP reads: left unread, the RTCP
    // channel behind the interceptor chain can fill and stall
    // the send path. Contents are for the interceptors; we just
    // keep the pipe moving until the session stops or the PC
    // closes under us.
    {
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            while !stop.load(Ordering::Relaxed) {
                if rtp_sender.read(&mut buf).await.is_err() {
                    break;
                }
            }
        });
    }

    // ── Subscriber PC ────────────────────────────────────────
    let sub_connected = Arc::new(AtomicBool::new(false));
    let sub_pc = Arc::new(api.new_peer_connection(config).await?);
    {
        let sub_connected = Arc::clone(&sub_connected);
        sub_pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            if s == RTCPeerConnectionState::Connected {
                sub_connected.store(true, Ordering::Relaxed);
            }
            match s {
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected => {
                    warn!("[janus] subscriber PC state: {s:?}");
                }
                _ => info!("[janus] subscriber PC state: {s:?}"),
            }
            Box::pin(async move {})
        }));
    }
    sub_pc.on_ice_connection_state_change(Box::new(move |s| {
        info!("[janus] subscriber ICE state: {s:?}");
        Box::pin(async move {})
    }));
    wire_ice(&sub_pc, SignalTarget::Subscriber, "subscriber", out_sig_tx.clone());
    {
        let track_owners = Arc::clone(&track_owners);
        let media_key = keys.media_key;
        let stop = Arc::clone(&stop);
        sub_pc.on_track(Box::new(move |remote: Arc<TrackRemote>, _, _| {
            let track_owners = Arc::clone(&track_owners);
            let pcm_out = pcm_out.clone();
            let stop = Arc::clone(&stop);
            Box::pin(async move {
                let sid = remote.id();
                // The owner mapping can land a beat after the
                // track does; wait briefly for it.
                let mut identity = None;
                for _ in 0..50 {
                    identity = track_owners.lock().unwrap().get(&sid).cloned();
                    if identity.is_some() || stop.load(Ordering::Relaxed) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let Some(identity) = identity else {
                    warn!("[janus] remote track {sid} never mapped to an identity; dropping it");
                    return;
                };
                debug!("[janus] receiving track {sid} from {identity}");
                run_track_reader(remote, identity, media_key, pcm_out, stop).await;
            })
        }));
    }

    // Publisher task starts once the offer is out; give it the
    // track + a payload type learned from the local SDP below.
    let mut publisher: Option<tokio::task::JoinHandle<()>> = None;
    let mut pcm_in = Some(pcm_in);
    let mut offer_id: u32 = 0;
    // Publisher-offer sequencing (the livekit-client order):
    // the offer goes out once the SFU confirms our track
    // (TrackPublished) — or after a 1.5 s fallback if that
    // confirmation never comes — and only when the PC has
    // asked to negotiate. Attempts happen on the housekeeping
    // tick (≤250 ms cadence); a failed attempt retries a
    // second later instead of sitting dead until the SFU's
    // establishment timeout ends the session.
    let mut offer_armed = false;
    let mut offer_sent = false;
    let mut negotiation_pending = false;
    let mut fallback_done = false;
    let mut next_offer_retry = tokio::time::Instant::now();
    let mut offer_fallback = Box::pin(tokio::time::sleep(Duration::from_millis(1500)));
    // Remote ICE candidates are gated per PC until that PC's
    // remote description exists — add_ice_candidate before it
    // is an error, and the old code swallowed it, silently
    // dropping the SFU's early candidates.
    let mut ice_gate = CandidateGate::default();
    // ICE-TCP bridge tasks live for the session.
    let mut ice_bridges: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // Signal-message tally for the session log.
    let mut sig_counts: HashMap<&'static str, u64> = HashMap::new();
    let mut sig_total: u64 = 0;
    // Housekeeping ticks between ICE stats dumps (~5 s).
    let mut stats_ticks: u32 = 0;
    let mut epoch_watch = tokio::time::interval(Duration::from_secs(15));
    epoch_watch.tick().await;
    let mut stop_watch = tokio::time::interval(Duration::from_millis(250));

    let end = loop {
        tokio::select! {
            _ = stop_watch.tick() => {
                if stop.load(Ordering::Relaxed) {
                    break RoomEnd::Stopped;
                }
                // ICE diagnostics: while either PC is still
                // unconnected, dump the candidate-pair picture
                // every ~5 s so a failed session explains
                // itself (pairs? checks leaving? answers back?).
                stats_ticks = stats_ticks.wrapping_add(1);
                if stats_ticks % 20 == 0
                    && !(pub_connected.load(Ordering::Relaxed)
                        && sub_connected.load(Ordering::Relaxed))
                {
                    info!(
                        "[janus] ICE stats publisher: {}",
                        ice_stats_summary(&pub_pc).await
                    );
                    info!(
                        "[janus] ICE stats subscriber: {}",
                        ice_stats_summary(&sub_pc).await
                    );
                }
                if offer_sent {
                    negotiation_pending = false; // one static track: nothing renegotiates
                } else if offer_armed
                    && negotiation_pending
                    && tokio::time::Instant::now() >= next_offer_retry
                {
                    match send_publisher_offer(&pub_pc, &signal, &cid, &mut offer_id).await {
                        Ok(pt) => {
                            offer_sent = true;
                            negotiation_pending = false;
                            info!("[janus] publisher offer sent (id {offer_id})");
                            if publisher.is_none() {
                                let handle = tokio::spawn(run_publisher(
                                    Arc::clone(&track),
                                    keys.media_key,
                                    own_identity.clone(),
                                    pt,
                                    pcm_in.take().expect("publisher starts once"),
                                    Arc::clone(&stop),
                                ));
                                publisher = Some(handle);
                            }
                        }
                        Err(e) => {
                            warn!("[janus] publisher offer failed: {e:#}");
                            next_offer_retry = tokio::time::Instant::now() + Duration::from_secs(1);
                        }
                    }
                }
            }
            _ = &mut offer_fallback, if !fallback_done => {
                fallback_done = true;
                if !offer_armed {
                    debug!("[janus] no TrackPublished after 1.5s; arming the publisher offer anyway");
                    offer_armed = true;
                }
            }
            _ = epoch_watch.tick() => {
                if let Ok(fresh) = super::keys::resolve_channel_keys(&channel) {
                    if fresh.epoch != keys.epoch || fresh.room != keys.room {
                        info!("[janus] channel rekeyed (epoch {} → {}); rejoining the new room", keys.epoch, fresh.epoch);
                        break RoomEnd::Rekey;
                    }
                }
            }
            Some(out_msg) = out_sig_rx.recv() => {
                if signal.send(out_msg).is_err() {
                    break RoomEnd::Failed(anyhow!("signaling channel closed"));
                }
            }
            Some(_) = negotiate_rx.recv() => {
                // The PC wants to negotiate (the track was
                // added). The offer itself waits for the SFU's
                // TrackPublished (or the fallback) — the
                // housekeeping tick sends it.
                negotiation_pending = true;
            }
            maybe_msg = signal.rx.recv() => {
                let Some(msg) = maybe_msg else {
                    break RoomEnd::Failed(anyhow!("signaling stream ended"));
                };
                match msg {
                    signal_response::Message::Offer(sd) => {
                        note_signal(&mut sig_counts, &mut sig_total, "Offer");
                        info!(
                            "[janus] subscriber offer (id {}) carries {} embedded candidate line(s)",
                            sd.id,
                            sdp_candidate_lines(&sd.sdp)
                        );
                        match subscriber_answer(&sub_pc, sd.sdp).await {
                            Ok(answer_sdp) => {
                                if signal.send_subscriber_answer(answer_sdp, sd.id).is_err() {
                                    break RoomEnd::Failed(anyhow!("signaling channel closed"));
                                }
                                for cand in ice_gate.remote_set(IcePc::Subscriber) {
                                    add_remote_candidate(&sub_pc, cand, "subscriber", &mut ice_bridges).await;
                                }
                            }
                            Err(e) => warn!("[janus] subscriber answer failed: {e:#}"),
                        }
                    }
                    signal_response::Message::Answer(sd) => {
                        note_signal(&mut sig_counts, &mut sig_total, "Answer");
                        info!(
                            "[janus] publisher answer carries {} embedded candidate line(s)",
                            sdp_candidate_lines(&sd.sdp)
                        );
                        match RTCSessionDescription::answer(sd.sdp) {
                            Ok(desc) => match pub_pc.set_remote_description(desc).await {
                                Ok(()) => {
                                    info!("[janus] publisher answer applied");
                                    for cand in ice_gate.remote_set(IcePc::Publisher) {
                                        add_remote_candidate(&pub_pc, cand, "publisher", &mut ice_bridges).await;
                                    }
                                }
                                Err(e) => {
                                    warn!("[janus] applying the SFU's publisher answer failed: {e}")
                                }
                            },
                            Err(e) => {
                                warn!("[janus] the SFU's publisher answer did not parse: {e}")
                            }
                        }
                    }
                    signal_response::Message::Trickle(t) => {
                        note_signal(&mut sig_counts, &mut sig_total, "Trickle");
                        if t.candidate_init.is_empty() {
                            info!("[janus] trickle (target {}) end-of-candidates", t.target);
                        } else {
                            match serde_json::from_str::<RTCIceCandidateInit>(&t.candidate_init) {
                                Ok(init) => {
                                    info!(
                                        "[janus] trickle in (target {}): {}",
                                        t.target, init.candidate
                                    );
                                    match ice_gate.push(t.target, init) {
                                        Some((IcePc::Subscriber, init)) => {
                                            add_remote_candidate(&sub_pc, init, "subscriber", &mut ice_bridges).await;
                                        }
                                        Some((IcePc::Publisher, init)) => {
                                            add_remote_candidate(&pub_pc, init, "publisher", &mut ice_bridges).await;
                                        }
                                        None => {
                                            info!("[janus] trickle queued until the remote description lands");
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "[janus] undecodable trickled candidate: {e} (raw: {})",
                                        t.candidate_init
                                    )
                                }
                            }
                        }
                    }
                    signal_response::Message::TrackPublished(tp) => {
                        note_signal(&mut sig_counts, &mut sig_total, "TrackPublished");
                        if tp.cid == cid {
                            let (sid, sfu_muted) = tp
                                .track
                                .as_ref()
                                .map(|t| (t.sid.as_str(), t.muted))
                                .unwrap_or(("?", false));
                            info!("[janus] audio track published ({sid}, sfu_muted={sfu_muted})");
                            // The SFU has the track: the
                            // publisher offer may go out now.
                            offer_armed = true;
                        }
                    }
                    signal_response::Message::Update(pu) => {
                        note_signal(&mut sig_counts, &mut sig_total, "Update");
                        let mut map = track_owners.lock().unwrap();
                        for p in &pu.participants {
                            note_participant(&mut map, p);
                        }
                    }
                    signal_response::Message::Leave(_) => {
                        note_signal(&mut sig_counts, &mut sig_total, "Leave");
                        info!("[janus] the SFU ended the session");
                        break RoomEnd::Stopped;
                    }
                    _ => {
                        note_signal(&mut sig_counts, &mut sig_total, "Other");
                    }
                }
            }
        }
    };

    // Teardown: polite leave, close the PCs, stop the publisher.
    signal.send_leave();
    if let Some(handle) = publisher {
        handle.abort();
    }
    for bridge in ice_bridges {
        bridge.abort();
    }
    let _ = pub_pc.close().await;
    let _ = sub_pc.close().await;
    Ok(end)
}

fn note_participant(map: &mut HashMap<String, String>, p: &ParticipantInfo) {
    for t in &p.tracks {
        map.insert(t.sid.clone(), p.identity.clone());
    }
}

/// Forward a PC's local ICE candidates to the SFU as
/// trickles, logging each one: the outbound half of the
/// candidate path must be as visible as the inbound half.
fn wire_ice(
    pc: &Arc<RTCPeerConnection>,
    target: SignalTarget,
    which: &'static str,
    out: mpsc::UnboundedSender<livekit_protocol::signal_request::Message>,
) {
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        let out = out.clone();
        Box::pin(async move {
            match candidate {
                Some(candidate) => {
                    let is_loopback = candidate
                        .address
                        .parse::<IpAddr>()
                        .map(|ip| ip.is_loopback())
                        .unwrap_or(false);
                    if is_loopback {
                        info!(
                            "[janus] {which} local ICE candidate {candidate} (loopback; kept local for the ICE-TCP bridge, not trickled)"
                        );
                        return;
                    }
                    match candidate_json(&candidate) {
                        Some(json) => {
                            info!("[janus] {which} local ICE candidate trickled out: {json}");
                            let _ = out.send(livekit_protocol::signal_request::Message::Trickle(
                                livekit_protocol::TrickleRequest {
                                    candidate_init: json,
                                    target: target as i32,
                                    r#final: false,
                                },
                            ));
                        }
                        None => {
                            warn!("[janus] {which} local ICE candidate {candidate} could not be serialized for trickle")
                        }
                    }
                }
                None => {
                    info!("[janus] {which} local ICE gathering complete");
                }
            }
        })
    }));
}

/// Create + set the publisher offer; returns (sdp, mid→cid).
/// The mid map comes from the PC's transceivers (their mids
/// are assigned during create_offer); the SDP parse and a
/// bare "0" are fallbacks so the map is never empty.
async fn publisher_offer(
    pc: &Arc<RTCPeerConnection>,
    cid: &str,
) -> Result<(String, HashMap<String, String>)> {
    let offer = pc.create_offer(None).await.context("creating publisher offer")?;
    pc.set_local_description(offer).await.context("setting publisher local description")?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("publisher local description missing after set"))?;
    let mids: Vec<String> = pc
        .get_transceivers()
        .await
        .iter()
        .filter_map(|t| t.mid().map(|m| m.to_string()))
        .collect();
    Ok((local.sdp.clone(), mid_map_from_mids(&mids, &local.sdp, cid)))
}

/// One publisher (re)negotiation: build the offer and hand it
/// to the SFU. Returns the negotiated Opus payload type.
async fn send_publisher_offer(
    pc: &Arc<RTCPeerConnection>,
    signal: &SignalClient,
    cid: &str,
    offer_id: &mut u32,
) -> Result<u8> {
    *offer_id += 1;
    let (sdp, mid_map) = publisher_offer(pc, cid).await?;
    let pt = opus_payload_type(&sdp);
    signal
        .send_publisher_offer(sdp, *offer_id, mid_map)
        .context("sending the publisher offer")?;
    Ok(pt)
}

/// Set the SFU's subscriber offer and create our answer SDP.
async fn subscriber_answer(pc: &Arc<RTCPeerConnection>, offer_sdp: String) -> Result<String> {
    let desc = RTCSessionDescription::offer(offer_sdp).context("parsing subscriber offer")?;
    pc.set_remote_description(desc).await.context("setting subscriber remote description")?;
    let answer = pc.create_answer(None).await.context("creating subscriber answer")?;
    pc.set_local_description(answer).await.context("setting subscriber local description")?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("subscriber local description missing after set"))?;
    Ok(local.sdp)
}

/// The outbound frame clock: drain the mixer's frame queue,
/// pacing 20 ms frames in real time. Each frame is
/// Opus-encoded then E2EE-encrypted under Janus's own sender
/// key. When the queue is empty the tick publishes NOTHING
/// (spec §4.1: silence is not streamed) — the RTP timestamp
/// only advances on frames actually sent.
async fn run_publisher(
    track: Arc<TrackLocalStaticRTP>,
    media_key: [u8; 32],
    own_identity: String,
    payload_type: u8,
    mut rx: mpsc::UnboundedReceiver<Vec<i16>>,
    stop: Arc<AtomicBool>,
) {
    // The SSRC fed to the cryptor is an IV ingredient only:
    // decryption reads the IV from the frame trailer, never
    // from the RTP header — and webrtc-rs rewrites the header
    // SSRC from its binding anyway. A stable random value per
    // publisher is all the format asks for.
    let ssrc: u32 = rand::random::<u32>();
    let mut cryptor = FrameCryptor::from_media_key(&media_key, &own_identity, 0);
    let encoder = mousiki::EncoderBuilder::new(48_000, mousiki::Channels::Mono, mousiki::Application::Voip)
        .bitrate(mousiki::Bitrate::Bits(64_000))
        .build();
    let mut encoder = match encoder {
        Ok(e) => e,
        Err(e) => {
            warn!("[janus] opus encoder failed to build: {e:?}");
            return;
        }
    };
    let mut seq: u16 = rand::random::<u16>();
    let mut timestamp: u32 = rand::random::<u32>();
    let mut queue: std::collections::VecDeque<Vec<i16>> = std::collections::VecDeque::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut first_frame = true;

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        tokio::select! {
            biased;
            item = rx.recv() => {
                match item {
                    None => return,
                    Some(frame) => {
                        // The mixer contract is one 960-sample
                        // frame per item; bound the queue so a
                        // stalled PC can never grow it without
                        // limit (drop-oldest, like the socket).
                        if queue.len() >= 10 {
                            queue.pop_front();
                        }
                        queue.push_back(frame);
                    }
                }
            }
            _ = ticker.tick() => {
                let Some(pcm) = queue.pop_front() else { continue };
                let mut frame = [0i16; FRAME_SAMPLES];
                let take = FRAME_SAMPLES.min(pcm.len());
                frame[..take].copy_from_slice(&pcm[..take]);
                let marker = first_frame;
                first_frame = false;
                match encoder.encode_vec(&frame, 4000) {
                    Ok(opus) => {
                        if let Ok(wire) = cryptor.encrypt_frame(ssrc, timestamp, &opus) {
                            let packet = RtpPacket {
                                header: RtpHeader {
                                    version: 2,
                                    marker,
                                    payload_type,
                                    sequence_number: seq,
                                    timestamp,
                                    ssrc,
                                    ..Default::default()
                                },
                                payload: Bytes::from(wire),
                            };
                            if let Err(e) = track.write_rtp(&packet).await {
                                debug!("[janus] RTP write failed: {e}");
                            }
                        }
                    }
                    Err(e) => debug!("[janus] opus encode failed: {e:?}"),
                }
                seq = seq.wrapping_add(1);
                timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
            }
        }
    }
}

/// The track reader's Opus decoders (48 kHz mono + stereo),
/// FFI libopus via the `opus` crate — NOT mousiki. Measured
/// 2026-11-23 (spot-tone sweep through the real code paths):
/// mousiki's ENCODER bitstream is flat to 16 kHz (proven by
/// decoding it with libopus), but mousiki's DECODER is the
/// "Xbox Live from MW2" choke point — mono output falls off
/// a cliff above ~6 kHz (−11…−39 dB at 7–16 kHz) and stereo
/// output is ragged −15…−31 dB nearly everywhere above
/// 500 Hz. libopus decode of the same packets is flat
/// (±0.2 dB, 250 Hz–16 kHz, both channel counts). The
/// publisher keeps the mousiki encoder; only decode moved.
fn build_decoders() -> Option<(opus::Decoder, opus::Decoder)> {
    let mono = opus::Decoder::new(48_000, opus::Channels::Mono).ok()?;
    let stereo = opus::Decoder::new(48_000, opus::Channels::Stereo).ok()?;
    Some((mono, stereo))
}

/// Read one remote track to its end: decrypt, decode, reframe
/// to the bridge's 20 ms clock, and hand each frame to the
/// mixer sink tagged with the participant's SFU identity.
async fn run_track_reader(
    track: Arc<TrackRemote>,
    identity: String,
    media_key: [u8; 32],
    sink: mpsc::UnboundedSender<(String, Vec<i16>)>,
    stop: Arc<AtomicBool>,
) {
    let Some((mut dec_mono, mut dec_stereo)) = build_decoders() else {
        warn!("[janus] opus decoder failed to build for {identity}");
        return;
    };
    let mut carry: Vec<i16> = Vec::new();
    let mut decode_buf = vec![0i16; 5760 * 2];
    // The sender's frame key derives from THEIR identity under
    // the channel media key (CORD-07 §3).
    let sender_key = super::keys::sender_key(&media_key, &identity);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let (packet, _) = match track.read_rtp().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        if packet.payload.is_empty() {
            continue;
        }
        let Ok(plain) = decrypt_frame(&sender_key, 0, &packet.payload) else {
            continue; // not for us / wrong epoch / comfort noise
        };
        if plain.is_empty() {
            continue;
        }
        // Opus TOC: the `s` bit (0b100) marks a stereo packet.
        let stereo = plain[0] & 0b0000_0100 != 0;
        let decoded: Vec<i16> = if stereo {
            match dec_stereo.decode(&plain, &mut decode_buf, false) {
                Ok(n) => decode_buf[..n * 2]
                    .chunks_exact(2)
                    .map(|pair| ((pair[0] as i32 + pair[1] as i32) / 2) as i16)
                    .collect(),
                Err(_) => continue,
            }
        } else {
            match dec_mono.decode(&plain, &mut decode_buf, false) {
                Ok(n) => decode_buf[..n].to_vec(),
                Err(_) => continue,
            }
        };
        carry.extend_from_slice(&decoded);
        while carry.len() >= FRAME_SAMPLES {
            let frame: Vec<i16> = carry.drain(..FRAME_SAMPLES).collect();
            if sink.send((identity.clone(), frame)).is_err() {
                return;
            }
        }
    }
    // A trailing partial frame (<20 ms) is dropped: on a live
    // bridge it is inaudible, and padding it would add a tick
    // of latency debt to the mixer.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goertzel magnitude at `freq` over samples[start..end]
    /// (the measurement the 2026-11-23 audio harness used).
    fn goertzel(samples: &[i16], freq: f64, start: usize, end: usize) -> f64 {
        let seg = &samples[start..end.min(samples.len())];
        let n = seg.len() as f64;
        let k = freq * n / 48_000.0;
        let w = 2.0 * std::f64::consts::PI * k / n;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in seg {
            let s = x as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / n
    }

    fn tone_gain_db(freq: f64, input: &[i16], output: &[i16]) -> f64 {
        let start = 3 * FRAME_SAMPLES;
        let gin = goertzel(input, freq, start, input.len() - FRAME_SAMPLES);
        let gout = goertzel(output, freq, start, output.len() - FRAME_SAMPLES);
        20.0 * (gout / gin).log10()
    }

    /// REGRESSION (2026-11-23, "Xbox Live from MW2"): the
    /// production decoders must pass high frequencies. The
    /// old mousiki decoder measured −11…−39 dB at 7–16 kHz
    /// mono and −15…−31 dB stereo; libopus is flat. If this
    /// test fails, someone put a band-limiting decoder back.
    #[test]
    fn production_decoders_pass_high_frequencies() {
        let (mut dec_mono, mut dec_stereo) = build_decoders().expect("decoders build");
        // Mono: packets from the production publisher encoder
        // (mousiki, 48 kHz mono Voip 64 kbps).
        let mut enc = mousiki::EncoderBuilder::new(48_000, mousiki::Channels::Mono, mousiki::Application::Voip)
            .bitrate(mousiki::Bitrate::Bits(64_000))
            .build()
            .expect("mousiki enc");
        for freq in [1_000.0f64, 8_000.0, 12_000.0] {
            let mut input: Vec<i16> = Vec::new();
            let mut output: Vec<i16> = Vec::new();
            let mut buf = vec![0i16; 5760];
            for f in 0..15 {
                let frame: Vec<i16> = (0..FRAME_SAMPLES)
                    .map(|i| {
                        let t = (f * FRAME_SAMPLES + i) as f64 / 48_000.0;
                        (12_000.0 * (2.0 * std::f64::consts::PI * freq * t).sin()) as i16
                    })
                    .collect();
                input.extend_from_slice(&frame);
                let pkt = enc.encode_vec(&frame, 4000).expect("encode");
                let n = dec_mono.decode(&pkt, &mut buf, false).expect("decode");
                output.extend_from_slice(&buf[..n]);
            }
            let db = tone_gain_db(freq, &input, &output);
            assert!(db > -3.0, "mono {freq} Hz gain {db:.1} dB — decoder is band-limiting");
        }
        // Stereo: packets from a libopus stereo encoder (what
        // real stereo clients send), decoded + downmixed the
        // way run_track_reader does it.
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
            .expect("ffi enc");
        enc.set_bitrate(opus::Bitrate::Bits(64_000)).expect("bitrate");
        for freq in [1_000.0f64, 8_000.0] {
            let mut input: Vec<i16> = Vec::new();
            let mut output: Vec<i16> = Vec::new();
            let mut buf = vec![0i16; 5760 * 2];
            for f in 0..15 {
                let mut frame: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
                let mut mono_frame: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
                for i in 0..FRAME_SAMPLES {
                    let t = (f * FRAME_SAMPLES + i) as f64 / 48_000.0;
                    let s = (12_000.0 * (2.0 * std::f64::consts::PI * freq * t).sin()) as i16;
                    frame.push(s);
                    frame.push(s);
                    mono_frame.push(s);
                }
                input.extend_from_slice(&mono_frame);
                let pkt = enc.encode_vec(&frame, 4000).expect("encode");
                let n = dec_stereo.decode(&pkt, &mut buf, false).expect("decode");
                output.extend(
                    buf[..n * 2]
                        .chunks_exact(2)
                        .map(|pair| ((pair[0] as i32 + pair[1] as i32) / 2) as i16),
                );
            }
            let db = tone_gain_db(freq, &input, &output);
            assert!(db > -3.0, "stereo {freq} Hz gain {db:.1} dB — decoder is band-limiting");
        }
    }

    #[test]
    fn opus_pt_parsed_from_sdp() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:63 red/48000/2\r\n";
        assert_eq!(opus_payload_type(sdp), 111);
        let sdp2 = "m=audio 9 UDP/TLS/RTP/SAVPF 120\r\na=rtpmap:120 opus/48000/2\r\n";
        assert_eq!(opus_payload_type(sdp2), 120);
        assert_eq!(opus_payload_type("m=video 9 AVP 96\r\n"), 111);
    }

    #[test]
    fn audio_mid_found_in_its_own_section() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=ssrc:1 cname:x\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\n";
        assert_eq!(audio_mid(sdp).as_deref(), Some("0"));
        let sdp2 = "m=video 9 AVP 96\r\na=mid:7\r\n";
        assert_eq!(audio_mid(sdp2), None);
    }

    /// THE regression test for the 2026-11-22 live failure:
    /// with the old bare `APIBuilder::new().build()` (no codecs
    /// registered) this create_offer fails with
    /// ErrSenderWithNoCodecs. With build_webrtc_api it must
    /// succeed and describe our track.
    #[tokio::test]
    async fn api_builds_and_publisher_offer_succeeds() {
        let (api, opus_cap) = build_webrtc_api().expect("build_webrtc_api");
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("new_peer_connection"),
        );
        let track = Arc::new(TrackLocalStaticRTP::new(
            opus_cap,
            "janus-audio".to_string(),
            "janus-stream".to_string(),
        ));
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .expect("add_track");
        let (sdp, mid_map) = publisher_offer(&pc, "TR_test").await.expect("publisher offer");
        assert!(sdp.contains("opus/48000"), "{sdp}");
        assert!(sdp.contains("a=msid:janus-stream janus-audio"), "{sdp}");
        assert_eq!(mid_map.get("0").map(String::as_str), Some("TR_test"), "{mid_map:?}");
        assert_eq!(opus_payload_type(&sdp), 111);
        pc.close().await.expect("close");
    }

    #[test]
    fn ice_ip_filter_keeps_routable_ipv4_and_loopback() {
        assert!(is_usable_ice_ip("172.16.1.42".parse().unwrap()));
        assert!(is_usable_ice_ip("10.0.0.15".parse().unwrap()));
        assert!(is_usable_ice_ip("164.92.99.77".parse().unwrap()));
        // Loopback is allowed so the agent can pair with the
        // ICE-TCP bridge's synthetic candidate; it is never
        // trickled (see candidate_json_never_trickles_loopback).
        assert!(is_usable_ice_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_usable_ice_ip("169.254.175.101".parse().unwrap())); // APIPA link-local
        assert!(!is_usable_ice_ip("0.0.0.0".parse().unwrap()));
        assert!(!is_usable_ice_ip("224.0.0.251".parse().unwrap()));
        assert!(!is_usable_ice_ip("255.255.255.255".parse().unwrap()));
        assert!(!is_usable_ice_ip("fe80::25cf:1a79:33ae:f129".parse().unwrap()));
        assert!(!is_usable_ice_ip("::1".parse().unwrap()));
    }

    #[test]
    fn ice_config_drops_ipv6_literal_urls() {
        let join = livekit_protocol::JoinResponse {
            ice_servers: vec![
                livekit_protocol::IceServer {
                    urls: vec![
                        "stun:[2001:db8::1]:3478".to_string(),
                        "stun:164.92.99.77:3478".to_string(),
                    ],
                    username: String::new(),
                    credential: String::new(),
                },
                livekit_protocol::IceServer {
                    urls: vec!["turn:[2001:db8::2]:3478".to_string()],
                    username: "u".to_string(),
                    credential: "p".to_string(),
                },
            ],
            ..Default::default()
        };
        let config = ice_config(&join);
        assert_eq!(config.ice_servers.len(), 1, "{:?}", config.ice_servers);
        assert_eq!(config.ice_servers[0].urls, vec!["stun:164.92.99.77:3478".to_string()]);
    }

    #[test]
    fn trickle_init_empty_mid_normalized() {
        let init = RTCIceCandidateInit {
            candidate: "candidate:1 1 udp 1 172.16.1.2 5000 typ host".to_string(),
            sdp_mid: Some(String::new()),
            sdp_mline_index: Some(0),
            username_fragment: None,
        };
        assert_eq!(normalize_trickle_init(init).sdp_mid.as_deref(), Some("0"));
        let init2 = RTCIceCandidateInit { sdp_mid: Some("0".to_string()), ..Default::default() };
        assert_eq!(normalize_trickle_init(init2).sdp_mid.as_deref(), Some("0"));
        let init3 = RTCIceCandidateInit { sdp_mid: None, ..Default::default() };
        assert_eq!(normalize_trickle_init(init3).sdp_mid, None);
    }

    #[test]
    fn mid_map_prefers_transceivers_then_sdp_then_zero() {
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:3\r\n";
        let from_tr = mid_map_from_mids(&["0".to_string()], sdp, "TR_x");
        assert_eq!(from_tr.get("0").map(String::as_str), Some("TR_x"));
        assert_eq!(from_tr.len(), 1);
        let from_sdp = mid_map_from_mids(&[], sdp, "TR_x");
        assert_eq!(from_sdp.get("3").map(String::as_str), Some("TR_x"));
        let bare = mid_map_from_mids(&[], "v=0\r\n", "TR_x");
        assert_eq!(bare.get("0").map(String::as_str), Some("TR_x"));
    }

    fn strip_sdp_candidates(sdp: &str) -> String {
        let mut out: String = sdp
            .lines()
            .filter(|l| !l.starts_with("a=candidate") && !l.starts_with("a=end-of-candidates"))
            .collect::<Vec<_>>()
            .join("\r\n");
        out.push_str("\r\n");
        out
    }

    async fn candidate_pair_count(pc: &Arc<RTCPeerConnection>) -> usize {
        pc.get_stats()
            .await
            .reports
            .values()
            .filter(|t| matches!(t, webrtc::stats::StatsReportType::CandidatePair(_)))
            .count()
    }

    /// Two PCs negotiated with candidate-stripped SDPs (the
    /// LiveKit SFU's trickle-only style), candidates moved
    /// solely through the production candidate_json → parse →
    /// add_ice_candidate path. Returns (a_connected,
    /// b_connected, a_local_cands, b_local_cands, a_pairs,
    /// b_pairs). Pair formation is pure bookkeeping and works
    /// everywhere; Connected additionally needs the sandbox to
    /// let UDP packets flow.
    async fn run_trickle_only_pair(
        api: &webrtc::api::API,
        opus_cap: RTCRtpCodecCapability,
        config: RTCConfiguration,
    ) -> (bool, bool, usize, usize, usize, usize) {
        use std::sync::atomic::AtomicUsize;
        let a = Arc::new(api.new_peer_connection(config.clone()).await.unwrap());
        let b = Arc::new(api.new_peer_connection(config).await.unwrap());
        let track = Arc::new(TrackLocalStaticRTP::new(opus_cap, "t".to_string(), "s".to_string()));
        a.add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .unwrap();

        let a_count = Arc::new(AtomicUsize::new(0));
        let b_count = Arc::new(AtomicUsize::new(0));
        let (atx, mut arx) = mpsc::unbounded_channel::<String>();
        let (btx, mut brx) = mpsc::unbounded_channel::<String>();
        {
            let atx = atx.clone();
            let a_count = Arc::clone(&a_count);
            a.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let atx = atx.clone();
                let a_count = Arc::clone(&a_count);
                Box::pin(async move {
                    if let Some(c) = c {
                        a_count.fetch_add(1, Ordering::SeqCst);
                        if let Some(j) = candidate_json(&c) {
                            let _ = atx.send(j);
                        }
                    }
                })
            }));
        }
        {
            let btx = btx.clone();
            let b_count = Arc::clone(&b_count);
            b.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let btx = btx.clone();
                let b_count = Arc::clone(&b_count);
                Box::pin(async move {
                    if let Some(c) = c {
                        b_count.fetch_add(1, Ordering::SeqCst);
                        if let Some(j) = candidate_json(&c) {
                            let _ = btx.send(j);
                        }
                    }
                })
            }));
        }
        let (a_conn_tx, mut a_conn_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
        a.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let _ = a_conn_tx.send(s);
            Box::pin(async {})
        }));
        let (b_conn_tx, mut b_conn_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
        b.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let _ = b_conn_tx.send(s);
            Box::pin(async {})
        }));

        let offer = a.create_offer(None).await.unwrap();
        a.set_local_description(offer.clone()).await.unwrap();
        b.set_remote_description(
            RTCSessionDescription::offer(strip_sdp_candidates(&offer.sdp)).unwrap(),
        )
        .await
        .unwrap();
        let answer = b.create_answer(None).await.unwrap();
        b.set_local_description(answer.clone()).await.unwrap();
        a.set_remote_description(
            RTCSessionDescription::answer(strip_sdp_candidates(&answer.sdp)).unwrap(),
        )
        .await
        .unwrap();

        let b2 = Arc::clone(&b);
        let a2 = Arc::clone(&a);
        let drain = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(j) = arx.recv() => {
                        if let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&j) {
                            let _ = b2.add_ice_candidate(init).await;
                        }
                    }
                    Some(j) = brx.recv() => {
                        if let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&j) {
                            let _ = a2.add_ice_candidate(init).await;
                        }
                    }
                    else => break,
                }
            }
        });

        let wait = async {
            let (mut ac, mut bc) = (false, false);
            while !(ac && bc) {
                tokio::select! {
                    Some(s) = a_conn_rx.recv() => {
                        if s == RTCPeerConnectionState::Connected { ac = true; }
                    }
                    Some(s) = b_conn_rx.recv() => {
                        if s == RTCPeerConnectionState::Connected { bc = true; }
                    }
                    else => break,
                }
            }
            (ac, bc)
        };
        let (ac, bc) = tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or((false, false));
        drain.abort();
        let (ap, bp) = (candidate_pair_count(&a).await, candidate_pair_count(&b).await);
        (
            ac,
            bc,
            a_count.load(Ordering::SeqCst),
            b_count.load(Ordering::SeqCst),
            ap,
            bp,
        )
    }

    /// Can this environment actually move a UDP packet? (The
    /// Some sandboxes refuse every UDP send, even loopback —
    /// there, pair formation is all a test may assert.)
    fn udp_flows() -> bool {
        let Ok(sock) = std::net::UdpSocket::bind("127.0.0.1:0") else { return false };
        let Ok(addr) = sock.local_addr() else { return false };
        if sock.send_to(b"probe", addr).is_err() {
            return false;
        }
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        let mut buf = [0u8; 8];
        sock.recv_from(&mut buf).is_ok()
    }

    /// THE round-3 regression test: with the production API +
    /// ICE config, a trickle-only negotiation (the SFU's style:
    /// no candidates in SDPs, everything via trickle) must
    /// gather local candidates, form candidate pairs on both
    /// PCs, and — wherever UDP actually flows — connect.
    /// (Verified on the gaming PC 2026-11-22: both sides
    /// Connected in <1 s with 3 local candidates each.)
    #[tokio::test]
    async fn trickle_only_pair_forms_pairs_and_connects() {
        let (api, opus_cap) = build_webrtc_api().unwrap();
        let join = livekit_protocol::JoinResponse {
            ice_servers: vec![livekit_protocol::IceServer {
                urls: vec!["stun:164.92.99.77:3478".to_string()],
                username: String::new(),
                credential: String::new(),
            }],
            ..Default::default()
        };
        let config = ice_config(&join);
        let (ac, bc, na, nb, ap, bp) = run_trickle_only_pair(&api, opus_cap, config).await;
        assert!(na >= 1, "publisher side gathered no local candidates");
        assert!(nb >= 1, "subscriber side gathered no local candidates");
        assert!(ap >= 1, "publisher side formed no candidate pairs");
        assert!(bp >= 1, "subscriber side formed no candidate pairs");
        if udp_flows() {
            assert!(ac && bc, "trickle-only pair did not connect though UDP flows");
        }
    }

    #[test]
    fn parse_passive_tcp_candidate_matches_the_sfu_shape() {
        // The SFU's real advertised candidates (captured shape).
        let tcp = "candidate:1849495994 1 tcp 1671430143 164.92.99.77 7881 typ host tcptype passive ufrag XvPDbIvVrpmoRAgu";
        assert_eq!(
            parse_passive_tcp_candidate(tcp),
            Some(("1849495994".to_string(), 1_671_430_143, "164.92.99.77".to_string(), 7881))
        );
        let udp = "candidate:3553517658 1 udp 2130706431 164.92.99.77 7882 typ host ufrag XvPDbIvVrpmoRAgu";
        assert_eq!(parse_passive_tcp_candidate(udp), None);
        // Active TCP wants us to listen — nothing to bridge.
        let active = "candidate:1 1 tcp 1671430143 164.92.99.77 7881 typ host tcptype active";
        assert_eq!(parse_passive_tcp_candidate(active), None);
        // Discard port and IPv6 are not bridgeable either.
        let discard = "candidate:1 1 tcp 1671430143 164.92.99.77 9 typ host tcptype passive";
        assert_eq!(parse_passive_tcp_candidate(discard), None);
        let v6 = "candidate:1 1 tcp 1671430143 2001:db8::1 7881 typ host tcptype passive";
        assert_eq!(parse_passive_tcp_candidate(v6), None);
        // No `candidate:` prefix is fine too.
        let bare = "1849495994 1 tcp 1671430143 164.92.99.77 7881 typ host tcptype passive";
        assert!(parse_passive_tcp_candidate(bare).is_some());
    }

    #[test]
    fn candidate_gate_queues_until_remote_set_then_drains_in_order() {
        let mk = |c: &str| RTCIceCandidateInit {
            candidate: c.to_string(),
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
            username_fragment: None,
        };
        let mut gate = CandidateGate::default();
        // Trickles for both PCs arrive before either remote
        // description: both queue, nothing is addable yet.
        assert!(gate.push(SignalTarget::Subscriber as i32, mk("sub-1")).is_none());
        assert!(gate.push(SignalTarget::Publisher as i32, mk("pub-1")).is_none());
        assert!(gate.push(SignalTarget::Subscriber as i32, mk("sub-2")).is_none());
        // The subscriber's description lands: ONLY its queue
        // drains, in arrival order; the publisher's stays put.
        let drained = gate.remote_set(IcePc::Subscriber);
        assert_eq!(
            drained.iter().map(|c| c.candidate.as_str()).collect::<Vec<_>>(),
            vec!["sub-1", "sub-2"]
        );
        // After that, subscriber trickles route immediately —
        // to the subscriber PC.
        match gate.push(SignalTarget::Subscriber as i32, mk("sub-3")) {
            Some((IcePc::Subscriber, init)) => assert_eq!(init.candidate, "sub-3"),
            other => panic!("expected immediate subscriber routing, got {other:?}"),
        }
        // Publisher trickles still queue until ITS description.
        assert!(gate.push(SignalTarget::Publisher as i32, mk("pub-2")).is_none());
        let drained = gate.remote_set(IcePc::Publisher);
        assert_eq!(
            drained.iter().map(|c| c.candidate.as_str()).collect::<Vec<_>>(),
            vec!["pub-1", "pub-2"]
        );
        match gate.push(SignalTarget::Publisher as i32, mk("pub-3")) {
            Some((IcePc::Publisher, init)) => assert_eq!(init.candidate, "pub-3"),
            other => panic!("expected immediate publisher routing, got {other:?}"),
        }
        // Any non-subscriber target value routes to the publisher.
        match gate.push(0, mk("pub-4")) {
            Some((IcePc::Publisher, _)) => {}
            other => panic!("expected publisher routing for target 0, got {other:?}"),
        }
    }

    #[test]
    fn sfu_trickle_json_parses_into_candidate_init() {
        // The exact JSON envelope the SFU trickles (Pion shape).
        let udp_json = "{\"candidate\":\"candidate:3553517658 1 udp 2130706431 164.92.99.77 7882 typ host\",\"sdpMid\":\"0\",\"sdpMLineIndex\":0}";
        let init: RTCIceCandidateInit = serde_json::from_str(udp_json).unwrap();
        assert!(init.candidate.contains("164.92.99.77 7882"));
        assert_eq!(init.sdp_mid.as_deref(), Some("0"));
        assert_eq!(init.sdp_mline_index, Some(0));
        let tcp_json = "{\"candidate\":\"candidate:1849495994 1 tcp 1671430143 164.92.99.77 7881 typ host tcptype passive\",\"sdpMid\":\"0\",\"sdpMLineIndex\":0,\"usernameFragment\":\"XvPDbIvVrpmoRAgu\"}";
        let init: RTCIceCandidateInit = serde_json::from_str(tcp_json).unwrap();
        assert!(parse_passive_tcp_candidate(&init.candidate).is_some());
        assert_eq!(init.username_fragment.as_deref(), Some("XvPDbIvVrpmoRAgu"));
    }

    #[test]
    fn candidate_json_never_trickles_loopback() {
        let loopback = RTCIceCandidate {
            address: "127.0.0.1".to_string(),
            port: 51234,
            protocol: webrtc::ice_transport::ice_protocol::RTCIceProtocol::Udp,
            typ: webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType::Host,
            component: 1,
            foundation: "1".to_string(),
            priority: 100,
            ..Default::default()
        };
        assert!(candidate_json(&loopback).is_none());
        let lan = RTCIceCandidate {
            address: "172.16.0.170".to_string(),
            ..loopback.clone()
        };
        let json = candidate_json(&lan).expect("LAN candidate must trickle");
        assert!(json.contains("172.16.0.170"), "{json}");
        assert!(json.contains("\"sdpMid\":\"0\""), "{json}");
    }
}

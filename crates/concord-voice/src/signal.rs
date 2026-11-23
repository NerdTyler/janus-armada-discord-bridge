//! LiveKit `/rtc` signaling: binary protobuf (LiveKit protocol
//! 15) over a WebSocket, per the Apache-2.0 `livekit-protocol`
//! definitions. The flow: connect with the broker JWT in the
//! query string → the SFU's first message is the JoinResponse →
//! SignalRequest/SignalResponse exchange drives the two peer
//! connections (offers, answers, trickle ICE) — see room.rs.
//! A client keepalive (PingReq on the join's interval) runs in
//! the pump, and WS-level pings are answered with pongs.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use livekit_protocol::{
    signal_request, signal_response, AddTrackRequest, LeaveRequest, Ping, SessionDescription,
    SignalRequest, SignalResponse,
};
use prost::Message as _;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Build the `/rtc` WebSocket URL for a join. `auto_subscribe`
/// on: the SFU subscribes the bot to every published track, which
/// is what a call participant wants.
pub fn build_rtc_ws_url(sfu_url: &str, token: &str, auto_subscribe: bool) -> Result<String> {
    let trimmed = sfu_url.trim().trim_end_matches('/');
    let ws_base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    };
    let mut url = url::Url::parse(&format!("{ws_base}/rtc"))
        .with_context(|| format!("invalid LiveKit SFU URL {sfu_url}"))?;
    url.query_pairs_mut()
        .append_pair("access_token", token)
        .append_pair("auto_subscribe", if auto_subscribe { "1" } else { "0" })
        .append_pair("sdk", "rust")
        .append_pair("version", "0.8.2")
        .append_pair("protocol", "15");
    Ok(url.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A live signaling channel to one SFU room.
pub struct SignalClient {
    pub join: livekit_protocol::JoinResponse,
    tx: mpsc::UnboundedSender<signal_request::Message>,
    pub rx: mpsc::UnboundedReceiver<signal_response::Message>,
}

impl SignalClient {
    /// Connect and await the JoinResponse (the SFU's first
    /// message), then spawn the pump: outbound requests, the
    /// keepalive ticker, WS-level ping/pong, and response
    /// forwarding (Pong/PongResp swallowed — they're liveness
    /// noise for the room layer).
    pub async fn connect(sfu_url: &str, token: &str) -> Result<Self> {
        let rtc_url = build_rtc_ws_url(sfu_url, token, true)?;
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(&rtc_url),
        )
        .await
        .context("timed out connecting to LiveKit /rtc")?
        .context("LiveKit /rtc WebSocket handshake failed")?;
        let (mut ws_write, mut ws_read) = ws.split();

        let join = tokio::time::timeout(Duration::from_secs(12), async {
            while let Some(frame) = ws_read.next().await {
                match frame? {
                    WsMessage::Binary(bytes) => {
                        let resp = SignalResponse::decode(bytes.as_ref())
                            .context("decoding the SFU's first message")?;
                        if let Some(signal_response::Message::Join(j)) = resp.message {
                            return Ok::<livekit_protocol::JoinResponse, anyhow::Error>(j);
                        }
                    }
                    WsMessage::Close(c) => {
                        return Err(anyhow!("LiveKit closed before JoinResponse: {c:?}"));
                    }
                    _ => {}
                }
            }
            Err(anyhow!("LiveKit /rtc stream ended before JoinResponse"))
        })
        .await
        .context("timed out waiting for JoinResponse")??;

        let ping_secs = if join.ping_interval > 0 {
            (join.ping_interval as u64).clamp(5, 30)
        } else {
            15
        };
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<signal_request::Message>();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel::<signal_response::Message>();
        tokio::spawn(async move {
            let mut ping = tokio::time::interval(Duration::from_secs(ping_secs));
            ping.tick().await; // consume the immediate tick
            loop {
                tokio::select! {
                    maybe_out = req_rx.recv() => {
                        let Some(msg) = maybe_out else {
                            let _ = ws_write.close().await;
                            break;
                        };
                        let is_leave = matches!(msg, signal_request::Message::Leave(_));
                        let wire = SignalRequest { message: Some(msg) }.encode_to_vec();
                        if ws_write.send(WsMessage::Binary(wire.into())).await.is_err() {
                            break;
                        }
                        if is_leave {
                            let _ = ws_write.close().await;
                            break;
                        }
                    }
                    _ = ping.tick() => {
                        let wire = SignalRequest {
                            message: Some(signal_request::Message::PingReq(Ping {
                                timestamp: now_ms(),
                                rtt: 0,
                            })),
                        }
                        .encode_to_vec();
                        if ws_write.send(WsMessage::Binary(wire.into())).await.is_err() {
                            break;
                        }
                    }
                    maybe_in = ws_read.next() => {
                        match maybe_in {
                            Some(Ok(WsMessage::Binary(bytes))) => {
                                match SignalResponse::decode(bytes.as_ref()) {
                                    Ok(SignalResponse { message: Some(m) }) => {
                                        match &m {
                                            // Liveness replies to our
                                            // keepalive: swallowed.
                                            // (Protocol 15 has no
                                            // server-initiated signal
                                            // ping; the WS layer's own
                                            // ping/pong below covers
                                            // that direction.)
                                            signal_response::Message::Pong(_)
                                            | signal_response::Message::PongResp(_) => continue,
                                            _ => {}
                                        }
                                        let is_leave = matches!(m, signal_response::Message::Leave(_));
                                        let _ = resp_tx.send(m);
                                        if is_leave {
                                            break;
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        log::debug!("[janus] undecodable SignalResponse: {e}");
                                    }
                                }
                            }
                            Some(Ok(WsMessage::Ping(p))) => {
                                let _ = ws_write.send(WsMessage::Pong(p)).await;
                            }
                            Some(Ok(WsMessage::Close(_))) | None => break,
                            Some(Err(e)) => {
                                log::warn!("[janus] signaling read error: {e}");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self { join, tx: req_tx, rx: resp_rx })
    }

    pub fn send(&self, msg: signal_request::Message) -> Result<()> {
        self.tx.send(msg).map_err(|_| anyhow!("LiveKit signaling channel closed"))
    }

    /// Announce the audio track the bridge will publish, BEFORE the
    /// publisher offer. E2EE is declared (`encryption: Gcm`) so
    /// the SFU routes the frames as encrypted; DTX and RED are
    /// off (a continuous, simply-decodable stream for receivers).
    pub fn send_add_audio_track(&self, cid: &str, name: &str) -> Result<()> {
        #[allow(deprecated)]
        let req = AddTrackRequest {
            cid: cid.to_string(),
            name: name.to_string(),
            r#type: livekit_protocol::TrackType::Audio as i32,
            width: 0,
            height: 0,
            muted: false,
            disable_dtx: true,
            source: livekit_protocol::TrackSource::Microphone as i32,
            layers: Vec::new(),
            simulcast_codecs: Vec::new(),
            sid: String::new(),
            stereo: false,
            disable_red: true,
            encryption: livekit_protocol::encryption::Type::Gcm as i32,
            stream: String::new(),
            backup_codec_policy: 0,
            audio_features: vec![livekit_protocol::AudioTrackFeature::TfNoDtx as i32],
            packet_trailer_features: Vec::new(),
        };
        self.send(signal_request::Message::AddTrack(req))
    }

    pub fn send_publisher_offer(
        &self,
        sdp: String,
        offer_id: u32,
        mid_to_track_id: HashMap<String, String>,
    ) -> Result<()> {
        self.send(signal_request::Message::Offer(SessionDescription {
            r#type: "offer".to_string(),
            sdp,
            id: offer_id,
            mid_to_track_id,
        }))
    }

    pub fn send_subscriber_answer(&self, sdp: String, offer_id: u32) -> Result<()> {
        self.send(signal_request::Message::Answer(SessionDescription {
            r#type: "answer".to_string(),
            sdp,
            id: offer_id,
            mid_to_track_id: HashMap::new(),
        }))
    }

    pub fn send_leave(&self) {
        let _ = self.tx.send(signal_request::Message::Leave(LeaveRequest {
            can_reconnect: false,
            reason: 1, // CLIENT_INITIATED
            action: 0,
            regions: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtc_url_shape() {
        let u = build_rtc_ws_url("https://sfu.example.com/", "JWT", true).unwrap();
        assert!(u.starts_with("wss://sfu.example.com/rtc?"), "{u}");
        assert!(u.contains("access_token=JWT"), "{u}");
        assert!(u.contains("auto_subscribe=1"), "{u}");
        assert!(u.contains("protocol=15"), "{u}");
        let u2 = build_rtc_ws_url("wss://sfu.example.com", "JWT", false).unwrap();
        assert!(u2.contains("auto_subscribe=0"), "{u2}");
        let u3 = build_rtc_ws_url("sfu.example.com", "JWT", true).unwrap();
        assert!(u3.starts_with("wss://sfu.example.com/rtc?"), "{u3}");
    }
}

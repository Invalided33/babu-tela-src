//! One transfer's connection: WebRTC with nothing but a data channel,
//! through the same ICE servers as the calls (voice/rtc.rs, `ice_servers`):
//! straight between the two PCs when there is a way, through the relay
//! (TURN) when there is not. The person downloading makes the offer, the
//! sender answers; there is never a second offer, so none of the calls'
//! negotiation dance is needed. Both descriptions carry a half of the
//! secret the DTLS mixes in (e2e/dtls.rs), sealed with the rest.

use std::sync::Arc;

use rtc::peer_connection::configuration::setting_engine::SctpMaxMessageSize;
use rtc::statistics::StatsSelector;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use webrtc::data_channel::{DataChannel, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSdpType, RTCSessionDescription, SettingEngineBuilder,
};

use crate::e2e::dtls::DtlsSecret;
use crate::model::IceConfig;
use crate::voice::rtc::{ice_servers, sdp_attr};

/// A description that came without the other side's half of the secret
/// the DTLS mixes in (e2e/dtls.rs): no connection goes without it.
const NO_SECRET: &str = "descrição sem o segredo pós-quântico do DTLS";

/// The data channel, negotiated (both sides open it with this id, so it
/// needs no announcing).
const LABEL: &str = "arquivo";
const CHANNEL_ID: u16 = 0;

/// What the receiving side may hold before reading it (SCTP's window). The
/// stack's 1 MiB would cap a transfer at 1 MiB per round trip (20 MB/s at
/// 50 ms); this leaves room for a fast line far away.
const RECEIVE_WINDOW: u32 = 16 << 20;

/// What may wait to go out before `send` waits for room: enough to keep a
/// fast line busy, and the sender's memory bounded.
const SEND_BUFFER: usize = 8 << 20;

/// The largest message either side takes: a chunk and its header
/// (wire.rs). The stack's default is 64 KiB, the size of the chunk alone;
/// both sides say this one in their descriptions, and the smaller wins.
const MAX_MESSAGE: u32 = 128 << 10;

/// What the connection says, for the transfer's task.
pub enum LinkEv {
    /// One of my ICE candidates, to tell the other side.
    Candidate(RTCIceCandidateInit),
    State(RTCPeerConnectionState),
}

/// Forwards the connection's events; it must return at once (it runs on
/// the connection's own driver).
struct Handler {
    tx: UnboundedSender<LinkEv>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.tx.send(LinkEv::Candidate(init));
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.tx.send(LinkEv::State(state));
    }
}

pub struct Link {
    pc: Arc<dyn PeerConnection>,
    pub channel: Arc<dyn DataChannel>,
    pub events: UnboundedReceiver<LinkEv>,
    /// Candidates that came before the other side's description.
    pending: Vec<RTCIceCandidateInit>,
    /// The halves of the secret the DTLS mixes in, which the descriptions
    /// carry (e2e/dtls.rs).
    pq: Arc<DtlsSecret>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Link {
    pub async fn new(ice: Option<&IceConfig>, relay: &[RTCIceServer]) -> Result<Link, String> {
        let (tx, events) = unbounded_channel();
        let (servers, policy) = ice_servers(ice, relay);
        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(servers)
            .with_ice_transport_policy(policy)
            .build();
        let pq = DtlsSecret::new();
        let settings = SettingEngineBuilder::new()
            .with_sctp_max_receive_buffer_size(RECEIVE_WINDOW)
            .with_sctp_max_message_size(SctpMaxMessageSize::Bounded(MAX_MESSAGE))
            .with_dtls_extra_secret(pq.hook())
            .build();
        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_setting_engine(settings)
            .with_handler(Arc::new(Handler { tx }))
            // Roomy UDP sockets, as the calls have (voice/udp.rs).
            .with_runtime(Arc::new(crate::voice::udp::Runtime))
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .with_data_channel_send_buffer_limit(SEND_BUFFER)
            .build()
            .await
            .map_err(err)?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);
        let channel = pc
            .create_data_channel(
                LABEL,
                Some(RTCDataChannelInit {
                    negotiated: Some(CHANNEL_ID),
                    ..Default::default()
                }),
            )
            .await
            .map_err(err)?;
        Ok(Link {
            pc,
            channel,
            events,
            pending: Vec::new(),
            pq,
        })
    }

    /// My offer, to signal (the person downloading).
    pub async fn offer(&self) -> Result<Value, String> {
        let offer = self.pc.create_offer(None).await.map_err(err)?;
        self.pc
            .set_local_description(offer.clone())
            .await
            .map_err(err)?;
        let desc = self.pc.local_description().await.unwrap_or(offer);
        let mut out = json!({ "description": desc });
        self.pq.attach(&mut out);
        Ok(out)
    }

    /// The answer to their offer, to signal (the sender).
    pub async fn answer(&mut self, data: &Value) -> Result<Value, String> {
        let desc: RTCSessionDescription =
            serde_json::from_value(data["description"].clone()).map_err(err)?;
        if desc.sdp_type != RTCSdpType::Offer {
            return Err("não é uma oferta".into());
        }
        if !self.pq.take(data) {
            return Err(NO_SECRET.into());
        }
        self.pc.set_remote_description(desc).await.map_err(err)?;
        self.take_pending().await;
        let answer = self.pc.create_answer(None).await.map_err(err)?;
        self.pc
            .set_local_description(answer.clone())
            .await
            .map_err(err)?;
        let desc = self.pc.local_description().await.unwrap_or(answer);
        let mut out = json!({ "description": desc });
        self.pq.attach(&mut out);
        Ok(out)
    }

    /// What the other side signalled: the answer to my offer, or one of
    /// their candidates.
    pub async fn remote(&mut self, data: &Value) -> Result<(), String> {
        if let Some(d) = data.get("description") {
            let desc: RTCSessionDescription = serde_json::from_value(d.clone()).map_err(err)?;
            // An answer only, and once (a copy that came twice is the same).
            if desc.sdp_type != RTCSdpType::Answer || self.pc.remote_description().await.is_some() {
                return Ok(());
            }
            if !self.pq.take(data) {
                return Err(NO_SECRET.into());
            }
            self.pc.set_remote_description(desc).await.map_err(err)?;
            self.take_pending().await;
            return Ok(());
        }
        if let Some(c) = data.get("candidate") {
            let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(c.clone()) else {
                return Ok(());
            };
            if init.candidate.is_empty() {
                return Ok(());
            }
            if self.pc.remote_description().await.is_none() {
                self.pending.push(init);
            } else {
                // One the stack cannot use (another network's) is no reason
                // to give up: the others may do.
                let _ = self.pc.add_ice_candidate(init).await;
            }
        }
        Ok(())
    }

    async fn take_pending(&mut self) {
        for c in std::mem::take(&mut self.pending) {
            let _ = self.pc.add_ice_candidate(c).await;
        }
    }

    /// One of my candidates as it goes out: with the real `sdpMid` and my
    /// ICE user name, as a browser matches them (voice/rtc.rs does the same).
    pub async fn candidate(&self, mut c: RTCIceCandidateInit) -> Value {
        if let Some(local) = self.pc.local_description().await {
            if let Some(mid) = sdp_attr(&local.sdp, "mid") {
                c.sdp_mid = Some(mid.to_owned());
                c.sdp_mline_index = Some(0);
            }
            if let Some(ufrag) = sdp_attr(&local.sdp, "ice-ufrag") {
                c.username_fragment = Some(ufrag.to_owned());
            }
        }
        if c.sdp_mid.as_deref().is_none_or(str::is_empty) {
            c.sdp_mid = Some("0".into());
        }
        json!({ "candidate": {
            "candidate": c.candidate,
            "sdpMid": c.sdp_mid,
            "sdpMLineIndex": c.sdp_mline_index.unwrap_or(0),
            "usernameFragment": c.username_fragment,
        }})
    }

    /// Whether the connection goes through the relay (TURN), once it is up.
    pub async fn relayed(&self) -> bool {
        let report = self
            .pc
            .get_stats(std::time::Instant::now(), StatsSelector::None)
            .await;
        crate::voice::rtc::relayed(&report)
    }

    pub async fn close(&self) {
        let _ = self.channel.close().await;
        let _ = self.pc.close().await;
    }
}

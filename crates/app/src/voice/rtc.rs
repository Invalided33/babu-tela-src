//! The network side of a call: one WebRTC connection per person (a mesh),
//! negotiated over the gateway with "perfect negotiation" so either side may
//! start, exactly like voice.js. The media goes straight between the two:
//! the server only relays the signalling, and Cloudflare's TURN relays the
//! packets of the few connections that find no direct path.
//!
//! Each connection carries my voice (Opus), my video while I share (one of
//! its two copies: whichever that person asked for), the PC's sound of my
//! screen share (Opus stereo, on a track of its own, to those who know it
//! and did not silence it), and a data channel of its own, "controle", over
//! which each side says which video codecs it decodes, which copy of the
//! other's video it wants and whether it takes the sound of the other's
//! screen, and asks for full pictures. plan.rs decides who gets what; this
//! file measures (each connection's RTCP reports) and carries it out.
//!
//! Every connection runs in a task of its own that handles its signalling
//! one step at a time, which is what voice.js's promise queue did. Video to
//! each person goes through a writer of its own, paced together with
//! everyone else's, so a big picture copied to many people never holds up
//! the voice, which goes out straight away.
//!
//! A connection is made again when the other side made a new one (their
//! description comes with another DTLS certificate: they hung up on mine,
//! say, because the server told them I had left when my gateway dropped),
//! when it fails for good, and when the gateway comes back while it is not
//! up (what it was waiting for may have been lost). Its first offer carries
//! a new certificate in turn, so the other side makes theirs again too.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rtc::media::Sample;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::transport::RTCIceCandidateType;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::statistics::StatsSelector;
use rtc::statistics::report::{RTCStatsReport, RTCStatsReportEntry};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer, RTCIceTransportPolicy,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSdpType, RTCSessionDescription,
    RTCSignalingState, Registry, SettingEngineBuilder, register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

use super::assemble::Assembler;
use super::audio::{self, AudioShared, Decoder};
use super::plan::{self, Allocation, Budget, Codec, Copies, Health, Layer, Link, VoicePlan};
use super::video::{EncodedFrame, LayerControl, VideoShared};
use crate::gateway::{self, op};
use crate::ink::Ink;
use crate::model::IceConfig;
use crate::slides::{self, Slides};

const OPUS_PT: u8 = 111;
const H264_PT: u8 = 125;
const AV1_PT: u8 = 45;

/// The public STUN server the browser version fell back to.
const FALLBACK_STUN: &str = "stun:stun.l.google.com:19302";

/// The data channel each pair of people talks over. Negotiated (both sides
/// open it with the same id), so it needs no announcing and neither side has
/// to be the one that starts.
const CONTROL_LABEL: &str = "controle";
const CONTROL_ID: u16 = 0;

/// How often the reports are read and the video shared out again.
const TICK: Duration = Duration::from_secs(1);

/// Pictures waiting for one person past this many mean their writer is
/// behind (the budget is too small, or their path too slow): what follows
/// is dropped until the next full picture rather than delaying it all.
const MAX_BACKLOG: usize = 6;

/// How long a picture with a missing packet waits for it to be resent
/// (NACK goes out every 100 ms, and the copy takes a round trip) before
/// it is given up and a full picture asked for.
const VIDEO_PATIENCE: Duration = Duration::from_millis(300);

/// Full pictures are asked for at most this often.
const KEY_ASK_GAP: Duration = Duration::from_millis(700);

/// The longest a hang-up waits for the connections to close. Each one takes
/// about 100 ms: its close() returns once the release of its relay (TURN)
/// allocation is on its way to the server.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// What the call's controls ask of the network side.
pub enum Cmd {
    /// The people in the call now (from VOICE_STATE).
    Sync(Vec<i64>),
    /// Signalling from someone (SIGNAL).
    Signal(i64, Value),
    /// Start sending video, in these copies, or stop (None).
    Video(Option<Copies>),
    /// Which copy of each person's video I show (anyone missing: none).
    Wants(HashMap<i64, Layer>),
    /// The PC's sound goes with my screen share (true), or stops.
    ScreenAudio(bool),
    /// Whose screen's sound I silenced here: they need not send it.
    ScreenMuted(HashSet<i64>),
    /// The screen I enlarged in the call view, while one is: only its
    /// sound comes then (see `Call::takes_screen`).
    ScreenFocus(Option<i64>),
    /// The gateway came back after a drop (and the voice_join that keeps
    /// me in the call went out on it).
    Reconnected,
    /// Hang up.
    Close,
}

pub struct Ctx {
    pub me: i64,
    pub ice: Option<IceConfig>,
    /// The local ends of the relay over TCP and TLS (relay.rs).
    pub relay: Vec<RTCIceServer>,
    pub gw: gateway::Sender,
    /// The signalling goes sealed and signed (e2e/signals.rs), in the
    /// context of this voice channel.
    pub signals: Arc<crate::e2e::signals::Signals>,
    pub channel: i64,
    pub audio: Arc<AudioShared>,
    pub video: Arc<VideoShared>,
    /// Which copies the encoder makes, at what bitrate, and when a full
    /// picture is due.
    pub layers: Arc<LayerControl>,
    /// How the call is going, for the tests' report.
    pub report: Arc<Mutex<Report>>,
    /// The presentations: mine going out, everyone else's coming in.
    pub slides: Arc<Slides>,
    /// The annotations over what is shared (ink/).
    pub ink: Arc<Ink>,
    /// The music in the call: the queue, and the YouTube/SoundCloud reader.
    pub music: Arc<crate::music::Music>,
}

/// How the call is going: per person, and the video budget.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub peers: Vec<PeerReport>,
    /// The video budget (kb/s) and what each copy is allotted.
    pub budget_kbps: u32,
    pub low_kbps: u32,
    pub high_kbps: u32,
    pub voice: VoicePlan,
    /// What my video goes out in.
    pub codec: Codec,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerReport {
    pub user: i64,
    pub connected: bool,
    /// Through Cloudflare's TURN relay instead of straight to them.
    pub relayed: bool,
    /// My video to them: what they asked for, what their connection
    /// allows, and what goes.
    pub asked: Layer,
    pub cap: Layer,
    pub sending: Layer,
    /// What I asked of their video.
    pub showing: Layer,
    pub health: Option<Health>,
    pub loss: f64,
    pub rtt_ms: Option<u32>,
    /// Voice packets and pictures received from them, and the size of the
    /// last picture.
    pub audio_in: u64,
    pub video_in: u64,
    pub video_size: Option<[usize; 2]>,
    /// Packets of their screen's sound received, and whether mine goes to
    /// them.
    pub screen_audio_in: u64,
    pub screen_audio_out: bool,
}

// ---------------------------------------------------------------- codecs

fn opus() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            // usedtx: this side sends nothing while quiet (and the other
            // may do the same).
            sdp_fmtp_line: "minptime=10;useinbandfec=1;usedtx=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: OPUS_PT,
    }
}

/// Constrained baseline with packetization mode 1, which every browser
/// offers and openh264 produces.
fn h264() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            // NACK: a lost packet is sent again, which costs a round trip
            // instead of a full picture.
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: String::new(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
        },
        payload_type: H264_PT,
    }
}

/// AV1 main profile, as the cards' encoders make it (hw.rs), with the same
/// feedback as H.264. Offered by every copy of the app that reads it; sent
/// only when everyone in the call does (plan::pick_codec).
fn av1() -> RTCRtpCodecParameters {
    let h264 = h264();
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_AV1.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-idx=5;profile=0;tier=0".to_owned(),
            rtcp_feedback: h264.rtp_codec.rtcp_feedback,
        },
        payload_type: AV1_PT,
    }
}

/// The codec parameters my video goes out with.
fn video_codec(codec: Codec) -> RTCRtpCodecParameters {
    match codec {
        Codec::H264 => h264(),
        Codec::Av1 => av1(),
    }
}

/// For tests of networks that block UDP: only the relay over TCP and TLS,
/// and nothing direct, so a call that connects at all went through it.
const TCP_ONLY_ENV: &str = "BABUTELA_TURN_SO_TCP";

/// "Esconder meu IP" (the settings): the connections made from now on, of
/// calls and of files sent directly, offer only the relay's addresses, so
/// the people on the other end see the relay's IP instead of this PC's.
static HIDE_IP: AtomicBool = AtomicBool::new(false);

pub fn set_hide_ip(on: bool) {
    HIDE_IP.store(on, Ordering::Relaxed);
}

/// The ICE servers the server handed out that this WebRTC stack reaches by
/// itself (STUN, and TURN over UDP), plus `relay`, the local ends through
/// which it reaches TURN over TCP and TLS. With nothing usable it falls back
/// to plain STUN.
pub(crate) fn ice_servers(
    ice: Option<&IceConfig>,
    relay: &[RTCIceServer],
) -> (Vec<RTCIceServer>, RTCIceTransportPolicy) {
    if std::env::var_os(TCP_ONLY_ENV).is_some_and(|v| v == "1") && !relay.is_empty() {
        return (relay.to_vec(), RTCIceTransportPolicy::Relay);
    }
    // Through Tor (tor.rs), which carries no UDP: only the relay over TCP
    // and TLS, whose connections go through it, and nothing direct.
    if crate::tor::active() {
        if relay.is_empty() {
            crate::logln!("rtc: pelo Tor, sem o relé por TCP: não vai conectar");
        }
        return (relay.to_vec(), RTCIceTransportPolicy::Relay);
    }
    let mut out = Vec::new();
    if let Some(ice) = ice {
        for s in &ice.ice_servers {
            let urls: Vec<String> = s
                .urls
                .iter()
                .filter(|u| usable_ice_url(u))
                .cloned()
                .collect();
            let needs_auth = urls.iter().any(|u| u.starts_with("turn:"));
            let (user, pass) = (
                s.username.clone().unwrap_or_default(),
                s.credential.clone().unwrap_or_default(),
            );
            if urls.is_empty() || (needs_auth && (user.is_empty() || pass.is_empty())) {
                continue;
            }
            out.push(RTCIceServer {
                urls,
                username: user,
                credential: pass,
            });
        }
    }
    if out.is_empty() {
        out.push(RTCIceServer {
            urls: vec![FALLBACK_STUN.into()],
            ..Default::default()
        });
    }
    out.extend_from_slice(relay);
    let hide = HIDE_IP.load(Ordering::Relaxed);
    if hide
        && !out
            .iter()
            .any(|s| s.urls.iter().any(|u| u.starts_with("turn")))
    {
        // Nothing to relay through: no connection, rather than one that
        // shows my address.
        crate::logln!("rtc: esconder o IP está ligado e não há relay: não vai conectar");
    }
    let policy = if hide || ice.is_some_and(|i| i.relay_only) {
        RTCIceTransportPolicy::Relay
    } else {
        RTCIceTransportPolicy::All
    };
    (out, policy)
}

fn usable_ice_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("stun:") {
        return true;
    }
    lower.starts_with("turn:") && !lower.contains("transport=tcp")
}

/// The track of the PC's sound that goes with a screen share: its id starts
/// with this, which is how the other side tells it from the voice.
const SCREEN_TRACK: &str = "tela";

fn media_track(kind: RtpCodecKind, codec: RTCRtpCodecParameters, ssrc: u32) -> MediaStreamTrack {
    let (id, label) = match kind {
        RtpCodecKind::Audio => ("audio", "voz"),
        _ => ("video", "video"),
    };
    named_track(id, label, kind, codec, ssrc)
}

fn named_track(
    id: &str,
    label: &str,
    kind: RtpCodecKind,
    codec: RTCRtpCodecParameters,
    ssrc: u32,
) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "babutela".to_owned(),
        format!("{id}-{ssrc}"),
        label.to_owned(),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: codec.rtp_codec,
            ..Default::default()
        }],
    )
}

pub(crate) fn random_u32() -> u32 {
    // Good enough for an SSRC or a starting point, which only have to
    // differ between streams.
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish() as u32
}

fn random_ssrc() -> u32 {
    random_u32() | 1
}

// ---------------------------------------------------------------- what goes out

/// My voice to one person, packetized here rather than by the track: with
/// DTX the sequence numbers must run on without a gap (a gap reads as loss)
/// while the timestamps jump over the silence.
struct AudioOut {
    track: Arc<TrackLocalStaticRTP>,
    ssrc: u32,
    pt: AtomicU8,
    seq: AtomicU16,
    /// Each stream starts its timestamps somewhere of its own (RFC 3550).
    offset: u32,
    sender: Arc<dyn RtpSender>,
}

impl AudioOut {
    async fn write(&self, p: &audio::Packet) {
        let packet = rtc::rtp::Packet {
            header: rtc::rtp::Header {
                version: 2,
                marker: p.start,
                payload_type: self.pt.load(Ordering::Relaxed),
                sequence_number: self.seq.fetch_add(1, Ordering::Relaxed),
                timestamp: self.offset.wrapping_add(p.timestamp),
                ssrc: self.ssrc,
                ..Default::default()
            },
            payload: Bytes::copy_from_slice(&p.data),
        };
        let _ = self.track.write_rtp(packet).await;
    }

    async fn refresh_pt(&self) {
        if let Ok(params) = self.sender.get_parameters().await
            && let Some(c) = params.rtp_parameters.codecs.first()
        {
            self.pt.store(c.payload_type, Ordering::Relaxed);
        }
    }
}

/// Where my voice goes (everyone connected now), or the sound of my screen
/// share (everyone who takes it).
type Ears = Arc<Mutex<Vec<Arc<AudioOut>>>>;

/// Sends each packet of `sound` to everyone in `ears`, counting them in
/// `sent`.
async fn send_sound(
    mut sound: UnboundedReceiver<audio::Packet>,
    ears: Ears,
    shared: Arc<AudioShared>,
    sent: fn(&AudioShared) -> &AtomicU64,
) {
    while let Some(packet) = sound.recv().await {
        let outs = ears.lock().unwrap_or_else(PoisonError::into_inner).clone();
        for out in &outs {
            out.write(&packet).await;
        }
        sent(&shared).fetch_add(outs.len() as u64, Ordering::Relaxed);
    }
}

/// Makes `ears` hold `now`, unless it already does.
fn set_ears(ears: &Ears, now: Vec<Arc<AudioOut>>) {
    let mut ears = ears.lock().unwrap_or_else(PoisonError::into_inner);
    let same = ears.len() == now.len() && ears.iter().zip(&now).all(|(a, b)| Arc::ptr_eq(a, b));
    if !same {
        *ears = now;
    }
}

/// My video to one person, in one codec (its track packetizes that one),
/// with the payload type the other side agreed on.
struct Out {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    pt: AtomicU8,
    sender: Arc<dyn RtpSender>,
    codec: Codec,
}

impl Out {
    async fn write(&self, data: &[u8], duration: Duration) {
        let sample = Sample {
            data: Bytes::copy_from_slice(data),
            duration,
            ..Sample::new(Instant::now())
        };
        let _ = self
            .track
            .write_sample(self.ssrc, self.pt.load(Ordering::Relaxed), &sample, &[])
            .await;
    }

    /// Learns the payload type from the finished negotiation: the one of
    /// this track's codec (the line may carry H.264 and AV1 both).
    async fn refresh_pt(&self) {
        let mime = video_codec(self.codec).rtp_codec.mime_type;
        if let Ok(params) = self.sender.get_parameters().await
            && let Some(c) = params
                .rtp_parameters
                .codecs
                .iter()
                .find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(&mime))
        {
            self.pt.store(c.payload_type, Ordering::Relaxed);
        }
    }
}

/// Spreads my video to everyone at the rate the budget allows (with room
/// for full pictures, which are several times an ordinary one), so a picture
/// copied to many people does not leave in one burst ahead of the voice.
/// The voice never waits here.
struct Pacer {
    /// Bytes that may go now (below zero: owed), and when that was so.
    bucket: tokio::sync::Mutex<(f64, Instant)>,
    /// Bytes a second.
    rate: AtomicU64,
    /// Bytes released since the budget last looked.
    sent: AtomicU64,
}

impl Pacer {
    fn new(kbps: u32) -> Pacer {
        let p = Pacer {
            bucket: tokio::sync::Mutex::new((0.0, Instant::now())),
            rate: AtomicU64::new(0),
            sent: AtomicU64::new(0),
        };
        p.set_kbps(kbps);
        p
    }

    /// Half again the budget: the encoders aim at the budget on average,
    /// and a full picture needs to get out quickly.
    fn set_kbps(&self, kbps: u32) {
        let bytes = u64::from(kbps.max(100)) * 1000 / 8 * 3 / 2;
        self.rate.store(bytes, Ordering::Relaxed);
    }

    async fn take(&self, bytes: usize) {
        loop {
            let wait = {
                let mut b = self.bucket.lock().await;
                let rate = self.rate.load(Ordering::Relaxed) as f64;
                let now = Instant::now();
                // At most a quarter of a second saved up.
                b.0 =
                    (b.0 + rate * now.saturating_duration_since(b.1).as_secs_f64()).min(rate / 4.0);
                b.1 = now;
                if b.0 >= 0.0 {
                    b.0 -= bytes as f64;
                    None
                } else {
                    Some(Duration::from_secs_f64(-b.0 / rate.max(1.0)))
                }
            };
            match wait {
                None => {
                    self.sent.fetch_add(bytes as u64, Ordering::Relaxed);
                    return;
                }
                Some(d) => tokio::time::sleep(d.min(Duration::from_millis(250))).await,
            }
        }
    }
}

/// One person's video writer: pictures in the order they came, at the
/// pace the budget sets.
async fn write_video(
    video: Arc<Mutex<Option<Arc<Out>>>>,
    mut frames: UnboundedReceiver<Arc<EncodedFrame>>,
    backlog: Arc<AtomicUsize>,
    pacer: Arc<Pacer>,
) {
    while let Some(frame) = frames.recv().await {
        let waiting = Instant::now();
        pacer.take(frame.data.len()).await;
        crate::probe::since("video.ritmo", waiting);
        let out = video.lock().unwrap_or_else(PoisonError::into_inner).clone();
        // Only into a track of its codec: while the call moves to another,
        // the track and the encoder change at different moments.
        if let Some(out) = out.filter(|o| o.codec == frame.codec) {
            out.write(&frame.data, frame.duration).await;
        }
        // From the capture until the last of it went to the network.
        crate::probe::since("video.saida", frame.captured);
        backlog.fetch_sub(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- the data channel

/// The data channel to one person.
struct Control {
    channel: Arc<dyn DataChannel>,
    open: AtomicBool,
    /// Rung when what I sent them has mostly gone (the channel's buffered
    /// amount fell to `DRAINED`), for whoever waits for room (`room`).
    drained: tokio::sync::Notify,
    /// Messages that must arrive in the order they were handed over (a
    /// stroke's start, its points, its end), all sent by one task
    /// (`ordered`); `post` makes a task for each.
    ordered: UnboundedSender<Value>,
}

/// The channel says it drained when what is buffered falls to this.
const DRAINED: u32 = 16 * 1024;

/// Waiting for room checks again after this even if the word that the
/// channel drained never came (it is a backstop: the word normally comes
/// first).
const ROOM_RECHECK: Duration = Duration::from_millis(100);

/// Each connection gets a number, so what one brought (a presentation) is
/// not forgotten when an older one with the same person closes after it.
static LINKS: AtomicU64 = AtomicU64::new(1);

impl Control {
    async fn send(&self, msg: &Value) {
        if self.open.load(Ordering::Relaxed) {
            let _ = self.channel.send_text(&msg.to_string()).await;
        }
    }

    /// Waits until fewer than `limit` of the bytes handed to the channel
    /// are still on their way (sent and not yet acknowledged).
    async fn room(&self, limit: usize) {
        loop {
            let on_the_way = self.channel.outstanding_bytes().await.unwrap_or(0);
            if on_the_way < limit {
                return;
            }
            let _ = tokio::time::timeout(ROOM_RECHECK, self.drained.notified()).await;
        }
    }

    /// Sends from wherever, without waiting.
    fn post(self: &Arc<Self>, msg: Value) {
        self.post_all(vec![msg]);
    }

    /// As `post`, several messages that must arrive in this order.
    fn post_all(self: &Arc<Self>, msgs: Vec<Value>) {
        let me = self.clone();
        tokio::spawn(async move {
            for msg in &msgs {
                me.send(msg).await;
            }
        });
    }

    /// Sends `msg` after every message handed over this way before it.
    fn post_ordered(&self, msg: Value) {
        let _ = self.ordered.send(msg);
    }
}

/// Sends what `post_ordered` hands over, one after the other, for as long
/// as the connection's `Control` lasts.
async fn ordered(control: std::sync::Weak<Control>, mut rx: UnboundedReceiver<Value>) {
    while let Some(msg) = rx.recv().await {
        let Some(control) = control.upgrade() else {
            return;
        };
        control.send(&msg).await;
    }
}

fn want_message(layer: Layer) -> Value {
    json!({ "t": "want", "video": layer.wire() })
}

/// The codecs this copy of the app reads, said before anything else when
/// the channel opens: a copy that says "want" first is an older one, which
/// reads H.264 only.
fn codecs_message() -> Value {
    json!({ "t": "codecs", "video": [Codec::Av1.wire(), Codec::H264.wire()] })
}

/// "I take the PC's sound of your screen share" (`want`), or "not now"
/// (silenced here). Saying it at all is what says this copy of the app
/// knows that sound: an older one would play it as the voice, so it only
/// goes to those who said.
fn screen_audio_message(want: bool) -> Value {
    json!({ "t": "screenAudio", "want": want })
}

/// Milliseconds on this process' own clock, for the round trips: a ping
/// carries it and the pong brings it back.
fn clock_ms() -> f64 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    START.elapsed().as_secs_f64() * 1000.0
}

/// How often each side reports what it received of the other's video, and
/// measures the round trip. (The RTCP reports would say the same, but the
/// WebRTC stack never hands the ones that arrive to its statistics.)
const FEEDBACK: Duration = Duration::from_millis(500);

/// Counts one person's video packets as they arrive, for the reports.
#[derive(Default)]
struct Received {
    /// The first and the highest sequence number, extended past 16 bits.
    first: Option<u64>,
    highest: u64,
    count: u64,
}

impl Received {
    fn packet(&mut self, seq: u16) {
        let ext = match self.first {
            None => {
                let ext = (1u64 << 32) + u64::from(seq);
                self.first = Some(ext);
                ext
            }
            Some(_) => {
                let delta = i64::from(seq.wrapping_sub(self.highest as u16) as i16);
                (self.highest as i64 + delta) as u64
            }
        };
        self.highest = self.highest.max(ext);
        self.count += 1;
    }

    /// `{t: "rx", n: received, e: expected}`, both since the start.
    fn report(&self) -> Option<Value> {
        let first = self.first?;
        Some(json!({ "t": "rx", "n": self.count, "e": self.highest + 1 - first }))
    }
}

fn key_message() -> Value {
    json!({ "t": "key" })
}

/// Reads one person's data channel until it closes. What they present goes
/// straight into `slides`, as having come over connection `link`.
async fn watch_control(
    uid: i64,
    link: u64,
    control: Arc<Control>,
    events: Events,
    slides: Arc<Slides>,
    ink: Arc<Ink>,
    music: Arc<crate::music::Music>,
) {
    let tell = |ev| {
        let _ = events.send((link, ev));
    };
    // They said they show presentations (once is enough for the call).
    let mut shows_slides = false;
    while let Some(ev) = control.channel.poll().await {
        match ev {
            DataChannelEvent::OnOpen => {
                control.open.store(true, Ordering::Relaxed);
                tell(Event::Open(uid));
            }
            // The only binary messages: pieces of the pictures of what
            // they present.
            DataChannelEvent::OnMessage(msg) if !msg.is_string => {
                slides.arrived_piece(uid, &msg.data);
            }
            DataChannelEvent::OnMessage(msg) => {
                let Ok(v) = serde_json::from_slice::<Value>(&msg.data) else {
                    continue;
                };
                match v["t"].as_str() {
                    Some("want") => {
                        if let Some(layer) = v["video"].as_str().and_then(Layer::from_wire) {
                            tell(Event::Want(uid, layer));
                        }
                    }
                    Some("key") => {
                        tell(Event::Key(uid));
                    }
                    Some("codecs") => {
                        let av1 = v["video"].as_array().is_some_and(|list| {
                            list.iter()
                                .any(|c| c.as_str().and_then(Codec::from_wire) == Some(Codec::Av1))
                        });
                        tell(Event::Codecs(uid, av1));
                    }
                    Some("screenAudio") => {
                        let want = v["want"].as_bool().unwrap_or(true);
                        tell(Event::ScreenWant(uid, want));
                    }
                    // Back at once, for their round trip.
                    Some("ping") => {
                        control.send(&json!({ "t": "pong", "at": v["at"] })).await;
                        if !shows_slides && v["slides"].as_u64().is_some() {
                            shows_slides = true;
                            tell(Event::SlidesHello(uid));
                        }
                    }
                    Some("pong") => {
                        if let Some(at) = v["at"].as_f64() {
                            let rtt = (clock_ms() - at).max(0.0) / 1000.0;
                            tell(Event::Rtt(uid, rtt));
                        }
                    }
                    Some("rx") => {
                        if let (Some(n), Some(e)) = (v["n"].as_u64(), v["e"].as_u64()) {
                            tell(Event::Received(uid, n, e));
                        }
                    }
                    // They show presentations: mine can go to them.
                    Some("slides") => {
                        if !shows_slides {
                            shows_slides = true;
                            tell(Event::SlidesHello(uid));
                        }
                    }
                    Some("deck") => {
                        if let Some(deck) = slides::wire::read_announce(&v) {
                            slides.arrived_deck(uid, link, deck);
                        }
                    }
                    Some("slide") => {
                        if let Some((id, at)) = slides::wire::read_moved(&v) {
                            slides.arrived_move(uid, id, at);
                        }
                    }
                    Some("deckEnd") => {
                        if let Some(id) = slides::wire::read_ended(&v) {
                            slides.arrived_end(uid, id);
                        }
                    }
                    // Annotations over what someone shares (ink/).
                    Some(t) if t.starts_with("ink") => ink.arrived(uid, &v),
                    // The music in the call: add, skip, pause/play, state.
                    Some(t) if crate::music::handles(t) => music.arrived(uid, &v),
                    _ => {}
                }
            }
            DataChannelEvent::OnBufferedAmountLow => control.drained.notify_one(),
            DataChannelEvent::OnClose => break,
            _ => {}
        }
    }
    control.open.store(false, Ordering::Relaxed);
}

/// Sends my presentation to one person who shows presentations (they said
/// so), for as long as their connection lasts: what I present, which slide
/// I am on, and the pictures they lack, nearest my slide first. Only so
/// much is on its way at a time (`Slides::room`), so the pings and the
/// requests for full pictures behind it on the same channel are not held
/// up for long, and a slow connection is not flooded.
async fn send_slides(uid: i64, control: Arc<Control>, slides: Arc<Slides>) {
    /// Whatever ends the sender (the connection closed, or it was
    /// stopped): the presentation stops counting this person.
    struct Gone(i64, Arc<Slides>);
    impl Drop for Gone {
        fn drop(&mut self) {
            self.1.sender_gone(self.0);
        }
    }
    let _gone = Gone(uid, slides.clone());
    let mut changed = slides.subscribe();
    let mut progress = slides::Progress::default();
    loop {
        // Seen now: a change while this step is worked out wakes the wait.
        changed.borrow_and_update();
        match slides.next_step(uid, &mut progress) {
            slides::Step::Say(msg) => {
                if control.channel.send_text(&msg.to_string()).await.is_err() {
                    return;
                }
            }
            slides::Step::Piece { bytes, urgent } => {
                control.room(slides.room(urgent)).await;
                if control
                    .channel
                    .send(bytes::BytesMut::from(&bytes[..]))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            slides::Step::Wait => {
                if changed.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- one connection

/// Where each connection's tasks tell the call things, with the number of
/// the connection they come from (see `LINKS`): one the call has since
/// hung up on (to make it again, say) is no longer listened to.
type Events = UnboundedSender<(u64, Event)>;

/// What each connection's tasks tell the call.
enum Event {
    /// Their data channel opened: they can hear what I want.
    Open(i64),
    /// What they want of my video.
    Want(i64, Layer),
    /// They need a full picture of what I send them.
    Key(i64),
    /// The connection came up (true) or went down.
    Connected(i64, bool),
    /// The connection failed for good (ICE found no way through, or DTLS
    /// broke): nothing in it recovers from that, only a new one gets
    /// through again.
    Failed(i64),
    /// My video track to them is negotiated and ready.
    VideoReady(i64),
    /// A second's worth of numbers about the connection.
    Stats(i64, LinkStats),
    /// A round trip to them, in seconds (over the data channel, so behind
    /// whatever queue my video built on the way).
    Rtt(i64, f64),
    /// Their report on my video: packets received and expected, so far.
    Received(i64, u64, u64),
    /// Whether they take the sound of my screen share (they know it).
    ScreenWant(i64, bool),
    /// Which codecs they read: whether AV1 is one.
    Codecs(i64, bool),
    /// They show presentations.
    SlidesHello(i64),
}

impl Event {
    /// The person whose connection it came from.
    fn uid(&self) -> i64 {
        match self {
            Event::Open(uid)
            | Event::Want(uid, _)
            | Event::Key(uid)
            | Event::Connected(uid, _)
            | Event::Failed(uid)
            | Event::VideoReady(uid)
            | Event::Stats(uid, _)
            | Event::Rtt(uid, _)
            | Event::Received(uid, _, _)
            | Event::ScreenWant(uid, _)
            | Event::Codecs(uid, _)
            | Event::SlidesHello(uid) => *uid,
        }
    }
}

/// What a connection's statistics say.
#[derive(Clone, Copy, Debug, Default)]
struct LinkStats {
    relayed: bool,
}

/// Whether the connection goes through the relay. (Loss and round trips
/// come over the data channel instead: the stack's statistics never see
/// the reports that arrive.)
fn read_stats(report: &RTCStatsReport) -> LinkStats {
    LinkStats {
        relayed: relayed(report),
    }
}

/// Whether the candidate pair in use starts at one of my relay (TURN)
/// candidates (direct/link.rs asks it too).
pub(crate) fn relayed(report: &RTCStatsReport) -> bool {
    // The pair in use is the one the transport names: a pair stays marked
    // nominated after ICE moves on to another.
    let mut selected: Option<&str> = None;
    let mut pairs: HashMap<&str, &str> = HashMap::new();
    let mut relays: HashSet<&str> = HashSet::new();
    for entry in report.iter() {
        match entry {
            RTCStatsReportEntry::Transport(t) if !t.selected_candidate_pair_id.is_empty() => {
                selected = Some(t.selected_candidate_pair_id.as_str());
            }
            RTCStatsReportEntry::IceCandidatePair(p) => {
                pairs.insert(p.stats.id.as_str(), p.local_candidate_id.as_str());
            }
            RTCStatsReportEntry::LocalCandidate(c)
                if c.candidate_type == RTCIceCandidateType::Relay =>
            {
                relays.insert(bare_candidate_id(&c.stats.id));
            }
            _ => {}
        }
    }
    selected
        .and_then(|pair| pairs.get(pair))
        .is_some_and(|local| relays.contains(bare_candidate_id(local)))
}

/// A local candidate's id as candidate pairs name it: its own statistics
/// carry a prefix the pairs don't.
fn bare_candidate_id(id: &str) -> &str {
    id.strip_prefix("RTCLocalIceCandidate_").unwrap_or(id)
}

/// What the main loop keeps of each person.
struct Peer {
    tx: UnboundedSender<PeerEv>,
    /// The connection's task, which ends once the connection has closed.
    task: JoinHandle<()>,
    /// The connection's number (see `LINKS`).
    id: u64,
    /// Whether the connection may still signal: cleared as the call hangs
    /// up on it, so nothing it still had queued reaches the connection that
    /// replaces it on their side.
    alive: Arc<AtomicBool>,
    /// The DTLS certificate's fingerprint in their descriptions: who this
    /// connection talks to. A description with another one comes from a
    /// new connection of theirs (see `Cmd::Signal` in `run`).
    remote_fp: Option<String>,
    audio: Arc<AudioOut>,
    /// To their video writer.
    frames: UnboundedSender<Arc<EncodedFrame>>,
    backlog: Arc<AtomicUsize>,
    control: Arc<Control>,
    connected: bool,
    relayed: bool,
    /// What they asked of my video. Until they say, everything (an older
    /// copy of the app never says).
    asked: Layer,
    /// What I asked of theirs.
    showing: Layer,
    link: Link,
    health: Option<Health>,
    /// A report arrived since the last tick.
    fresh: bool,
    /// Their last report on my video: packets received and expected.
    received: (u64, u64),
    loss: f64,
    rtt: Option<f64>,
    /// What they get of my video now, and what they move to at the next
    /// full picture of that copy.
    sending: Layer,
    switching: Option<Layer>,
    /// Pictures were dropped (or their track was renegotiated): nothing more
    /// until a full one.
    broken: bool,
    /// The track of my screen's sound to them, while there is one.
    screen_out: Arc<Mutex<Option<Arc<AudioOut>>>>,
    /// Whether their connection was asked to have that track.
    screen_track: bool,
    /// They know the sound of a screen share (they said so), and whether
    /// they take mine now.
    screen_capable: bool,
    screen_wanted: bool,
    /// Whether they read AV1: None until they say (see `codecs_message`).
    av1: Option<bool>,
    /// What sends them my presentation, once they said they show them.
    slides_task: Option<JoinHandle<()>>,
}

enum PeerEv {
    Negotiate,
    Remote(Value),
    Candidate(RTCIceCandidateInit),
    Track(Arc<dyn TrackRemote>),
    Stable,
    State(RTCPeerConnectionState),
    /// My video track to them, in this codec, or none.
    Video(Option<Codec>),
    /// Add or take away the track of my screen's sound.
    Screen(bool),
    /// The gateway came back: an offer of mine still unanswered goes again.
    Resend,
    Close,
}

/// Forwards the connection's events to its task; the handler itself must
/// return at once, because it runs on the connection's own driver.
struct Handler {
    tx: UnboundedSender<PeerEv>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_negotiation_needed(&self) {
        let _ = self.tx.send(PeerEv::Negotiate);
    }

    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.tx.send(PeerEv::Candidate(init));
        }
    }

    async fn on_signaling_state_change(&self, state: RTCSignalingState) {
        if state == RTCSignalingState::Stable {
            let _ = self.tx.send(PeerEv::Stable);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.tx.send(PeerEv::State(state));
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tx.send(PeerEv::Track(track));
    }
}

/// The signalling state, worked out from the pending descriptions (this
/// WebRTC stack has no getter for it).
async fn signaling_state(pc: &dyn PeerConnection) -> RTCSignalingState {
    let local = pc.pending_local_description().await.map(|d| d.sdp_type);
    let remote = pc.pending_remote_description().await.map(|d| d.sdp_type);
    match (local, remote) {
        (None, None) => RTCSignalingState::Stable,
        (Some(RTCSdpType::Offer), None) => RTCSignalingState::HaveLocalOffer,
        (None, Some(RTCSdpType::Offer)) => RTCSignalingState::HaveRemoteOffer,
        (Some(RTCSdpType::Pranswer), Some(_)) => RTCSignalingState::HaveLocalPranswer,
        (Some(_), Some(RTCSdpType::Pranswer)) => RTCSignalingState::HaveRemotePranswer,
        _ => RTCSignalingState::Unspecified,
    }
}

/// The browser's parameterless setLocalDescription(): an answer when there
/// is an offer to answer, an offer otherwise.
async fn set_local_implicit(
    pc: &dyn PeerConnection,
) -> webrtc::error::Result<RTCSessionDescription> {
    let desc = match signaling_state(pc).await {
        RTCSignalingState::HaveRemoteOffer | RTCSignalingState::HaveLocalPranswer => {
            pc.create_answer(None).await?
        }
        _ => pc.create_offer(None).await?,
    };
    pc.set_local_description(desc.clone()).await?;
    Ok(pc.local_description().await.unwrap_or(desc))
}

/// The DTLS certificate's fingerprint in a description someone signalled
/// (None for a candidate): every connection makes a certificate of its own.
fn description_fingerprint(data: &Value) -> Option<String> {
    let sdp = data.get("description")?.get("sdp")?.as_str()?;
    sdp_attr(sdp, "fingerprint").map(str::to_owned)
}

/// The first `a=<name>:` value in an SDP.
pub(crate) fn sdp_attr<'a>(sdp: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("a={name}:");
    sdp.lines()
        .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .map(str::trim)
}

struct PeerTask {
    uid: i64,
    polite: bool,
    pc: Arc<dyn PeerConnection>,
    gw: gateway::Sender,
    signals: Arc<crate::e2e::signals::Signals>,
    /// The voice channel: the context the signalling is sealed in.
    channel: i64,
    /// The halves of the secret this connection's DTLS mixes in, which its
    /// descriptions carry (e2e/dtls.rs).
    pq: Arc<crate::e2e::dtls::DtlsSecret>,
    audio_out: Arc<AudioOut>,
    video_out: Arc<Mutex<Option<Arc<Out>>>>,
    screen_out: Arc<Mutex<Option<Arc<AudioOut>>>>,
    control: Arc<Control>,
    events: Events,
    audio: Arc<AudioShared>,
    video: Arc<VideoShared>,
    slides: Arc<Slides>,
    /// This connection's number (see `LINKS`).
    link: u64,
    /// Cleared when the call hangs up on this connection: from then on it
    /// says nothing more to them (see `Peer::alive`).
    alive: Arc<AtomicBool>,
    dirty: bool,
    making_offer: bool,
    ignore_offer: bool,
    pending: Vec<RTCIceCandidateInit>,
    /// My video track was added or removed and that is not negotiated yet.
    video_changed: bool,
}

impl PeerTask {
    /// To them, signed and sealed (e2e/signals.rs): the server carries it
    /// and can neither read it nor swap the DTLS fingerprint in it.
    fn signal(&self, mut data: Value) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        self.pq.attach(&mut data);
        let (gw, uid, alive) = (self.gw.clone(), self.uid, self.alive.clone());
        self.signals.seal(
            uid,
            crate::e2e::signals::call_context(self.channel),
            data,
            Box::new(move |sealed| {
                if alive.load(Ordering::SeqCst) {
                    gw.send(&op::signal(uid, sealed));
                }
            }),
        );
    }

    fn tell(&self, ev: Event) {
        let _ = self.events.send((self.link, ev));
    }

    async fn run(mut self, mut rx: UnboundedReceiver<PeerEv>) {
        let mut stats = tokio::time::interval(TICK);
        stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pings = tokio::time::interval(FEEDBACK);
        pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
                _ = stats.tick() => {
                    let report = self.pc.get_stats(Instant::now(), StatsSelector::None).await;
                    self.tell(Event::Stats(self.uid, read_stats(&report)));
                    continue;
                }
                _ = pings.tick() => {
                    // Saying again that this copy shows presentations: the
                    // word said as the channel opened can go missing.
                    self.control
                        .post(json!({ "t": "ping", "at": clock_ms(), "slides": 1 }));
                    continue;
                }
            };
            match ev {
                PeerEv::Negotiate => {
                    self.dirty = true;
                    self.negotiate().await;
                }
                PeerEv::Stable => {
                    self.audio_out.refresh_pt().await;
                    let screen = self
                        .screen_out
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    if let Some(s) = screen {
                        s.refresh_pt().await;
                    }
                    if let Some(v) = self.current_video() {
                        v.refresh_pt().await;
                        if std::mem::take(&mut self.video_changed) {
                            self.tell(Event::VideoReady(self.uid));
                        }
                    }
                    self.negotiate().await;
                }
                PeerEv::Remote(data) => {
                    if let Err(e) = self.remote(data).await {
                        crate::logln!("voz: sinal de {}: {e}", self.uid);
                    }
                }
                PeerEv::Candidate(c) => self.send_candidate(c).await,
                PeerEv::Track(track) => receive(
                    self.uid,
                    track,
                    self.audio.clone(),
                    self.video.clone(),
                    self.control.clone(),
                ),
                PeerEv::State(state) => {
                    let up = match state {
                        RTCPeerConnectionState::Connected => Some(true),
                        RTCPeerConnectionState::Disconnected
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed => Some(false),
                        _ => None,
                    };
                    if let Some(up) = up {
                        self.tell(Event::Connected(self.uid, up));
                    }
                    if state == RTCPeerConnectionState::Failed {
                        self.tell(Event::Failed(self.uid));
                    }
                }
                PeerEv::Video(on) => self.set_video(on).await,
                PeerEv::Screen(on) => self.set_screen(on).await,
                // It may have gone out while the gateway was down, and been
                // lost: they get it again. If they had answered it already,
                // they answer the copy too, and that second answer finds
                // nothing to answer here and is dropped (`remote`).
                PeerEv::Resend => {
                    if signaling_state(&*self.pc).await == RTCSignalingState::HaveLocalOffer
                        && let Some(offer) = self.pc.pending_local_description().await
                    {
                        self.signal(json!({ "description": offer }));
                    }
                }
                PeerEv::Close => break,
            }
        }
        let _ = self.pc.close().await;
        // What came in from them (their sound's buffer, their last
        // picture) went when the call hung up (`Call::hang_up`): by now a
        // newer connection with them may be filling it.
        self.slides.forget(self.uid, self.link);
    }

    fn current_video(&self) -> Option<Arc<Out>> {
        self.video_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Sends an offer when something changed and nothing else is under way.
    async fn negotiate(&mut self) {
        if !self.dirty || signaling_state(&*self.pc).await != RTCSignalingState::Stable {
            return;
        }
        self.making_offer = true;
        match set_local_implicit(&*self.pc).await {
            Ok(desc) => {
                self.dirty = false;
                self.signal(json!({ "description": desc }));
            }
            Err(e) => crate::logln!("voz: oferta para {}: {e}", self.uid),
        }
        self.making_offer = false;
    }

    /// Perfect negotiation, as voice.js did: on a collision the polite side
    /// (the lower user id) gives way, the other ignores the incoming offer.
    async fn remote(&mut self, data: Value) -> Result<(), String> {
        if let Some(desc) = data.get("description") {
            // Their half of the secret the DTLS mixes in, which every
            // description of theirs carries: none, no connection.
            if !self.pq.take(&data) {
                return Err("descrição sem o segredo pós-quântico do DTLS".into());
            }
            let desc: RTCSessionDescription =
                serde_json::from_value(desc.clone()).map_err(|e| e.to_string())?;
            let state = signaling_state(&*self.pc).await;
            let offer = desc.sdp_type == RTCSdpType::Offer;
            let collision = offer && (self.making_offer || state != RTCSignalingState::Stable);
            self.ignore_offer = !self.polite && collision;
            if self.ignore_offer {
                return Ok(());
            }
            if collision {
                let rollback = RTCSessionDescription::rollback(None).map_err(|e| e.to_string())?;
                if state == RTCSignalingState::HaveRemoteOffer {
                    self.pc.set_remote_description(rollback).await
                } else {
                    self.pc.set_local_description(rollback).await
                }
                .map_err(|e| e.to_string())?;
                // Our own change still has to go out once this settles.
                self.dirty = true;
            }
            self.pc
                .set_remote_description(desc)
                .await
                .map_err(|e| e.to_string())?;
            for c in std::mem::take(&mut self.pending) {
                let _ = self.pc.add_ice_candidate(c).await;
            }
            if offer {
                let answer = set_local_implicit(&*self.pc)
                    .await
                    .map_err(|e| e.to_string())?;
                self.signal(json!({ "description": answer }));
            }
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
            } else if let Err(e) = self.pc.add_ice_candidate(init).await
                && !self.ignore_offer
            {
                return Err(e.to_string());
            }
        }
        Ok(())
    }

    /// A browser matches a candidate to its media by `sdpMid`, so it goes
    /// out with the real one, and with our ICE user name.
    async fn send_candidate(&self, mut c: RTCIceCandidateInit) {
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
        self.signal(json!({ "candidate": {
            "candidate": c.candidate,
            "sdpMid": c.sdp_mid,
            "sdpMLineIndex": c.sdp_mline_index.unwrap_or(0),
            "usernameFragment": c.username_fragment,
        }}));
    }

    /// My video track to them in `codec`, or none. Another codec is another
    /// track (its packetizer is that codec's): the old one goes, the new one
    /// comes, in one negotiation.
    async fn set_video(&mut self, codec: Option<Codec>) {
        let current = self.current_video();
        if current.as_ref().map(|o| o.codec) == codec {
            return;
        }
        if let Some(out) = current {
            *self
                .video_out
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            let _ = self.pc.remove_track(&out.sender).await;
            self.dirty = true;
        }
        if let Some(codec) = codec {
            let ssrc = random_ssrc();
            let params = video_codec(codec);
            let pt = params.payload_type;
            let made = TrackLocalStaticSample::new(
                Instant::now(),
                media_track(RtpCodecKind::Video, params, ssrc),
            )
            .map(Arc::new);
            match made {
                Ok(track) => match self
                    .pc
                    .add_track(track.clone() as Arc<dyn TrackLocal>)
                    .await
                {
                    Ok(sender) => {
                        offer_both_codecs(&*self.pc, &sender).await;
                        *self
                            .video_out
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(Out {
                            track,
                            ssrc,
                            pt: AtomicU8::new(pt),
                            sender,
                            codec,
                        }));
                        self.video_changed = true;
                        self.dirty = true;
                    }
                    Err(e) => crate::logln!("voz: vídeo para {}: {e}", self.uid),
                },
                Err(e) => crate::logln!("voz: vídeo para {}: {e}", self.uid),
            }
        }
        if self.dirty {
            self.negotiate().await;
        }
    }

    /// The track of my screen's sound, added or taken away (the connection
    /// negotiates again either way).
    async fn set_screen(&mut self, on: bool) {
        let current = self
            .screen_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match (on, current) {
            (true, None) => {
                let ssrc = random_ssrc();
                let track = Arc::new(TrackLocalStaticRTP::new(named_track(
                    SCREEN_TRACK,
                    SCREEN_TRACK,
                    RtpCodecKind::Audio,
                    opus(),
                    ssrc,
                )));
                match self
                    .pc
                    .add_track(track.clone() as Arc<dyn TrackLocal>)
                    .await
                {
                    Ok(sender) => {
                        *self
                            .screen_out
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(AudioOut {
                            track,
                            ssrc,
                            pt: AtomicU8::new(OPUS_PT),
                            seq: AtomicU16::new(random_u32() as u16),
                            offset: random_u32(),
                            sender,
                        }));
                        self.dirty = true;
                        self.negotiate().await;
                    }
                    Err(e) => crate::logln!("voz: som da tela para {}: {e}", self.uid),
                }
            }
            (false, Some(out)) => {
                *self
                    .screen_out
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                let _ = self.pc.remove_track(&out.sender).await;
                self.dirty = true;
                self.negotiate().await;
            }
            _ => {}
        }
    }
}

/// Plays what arrives on `track` as `uid`'s: sound to the speaker, video to
/// the screen. A picture that fails to decode asks them, over the data
/// channel, for a full one.
fn receive(
    uid: i64,
    track: Arc<dyn TrackRemote>,
    shared: Arc<AudioShared>,
    video: Arc<VideoShared>,
    control: Arc<Control>,
) {
    tokio::spawn(async move {
        let Some(ssrc) = track.ssrcs().await.first().copied() else {
            return;
        };
        let mime = track
            .codec(ssrc)
            .await
            .map(|c| c.mime_type.to_ascii_lowercase())
            .unwrap_or_default();
        if mime == MIME_TYPE_OPUS.to_ascii_lowercase() {
            // Their voice, or the PC's sound of their screen share.
            let screen = track
                .track_id()
                .await
                .starts_with(&format!("{SCREEN_TRACK}-"));
            let (audio, decoder, counter) = if screen {
                (
                    shared.screen_peer(uid),
                    Decoder::stereo(),
                    &shared.screen_received,
                )
            } else {
                (shared.peer(uid), Decoder::new(), &shared.received)
            };
            let Ok(mut decoder) = decoder else {
                return;
            };
            while let Some(ev) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(pkt) = ev {
                    counter.fetch_add(1, Ordering::Relaxed);
                    audio.packets.fetch_add(1, Ordering::Relaxed);
                    decoder.packet(
                        pkt.header.sequence_number,
                        pkt.header.timestamp,
                        &pkt.payload,
                        &audio,
                        Instant::now(),
                    );
                }
            }
        } else if mime == MIME_TYPE_H264.to_ascii_lowercase() {
            read_video(uid, ssrc, track, video, control, Codec::H264).await;
        } else if mime == MIME_TYPE_AV1.to_ascii_lowercase() {
            read_video(uid, ssrc, track, video, control, Codec::Av1).await;
        }
    });
}

/// Reassembles pictures from the packets and hands them to a decoder
/// thread, asking for a full picture now and whenever one fails to decode.
async fn read_video(
    uid: i64,
    ssrc: u32,
    track: Arc<dyn TrackRemote>,
    video: Arc<VideoShared>,
    control: Arc<Control>,
    codec: Codec,
) {
    let ask = |track: Arc<dyn TrackRemote>, control: Arc<Control>| async move {
        control.send(&key_message()).await;
        // The standard way too, for a sender that is not this app.
        let _ = track
            .write_rtcp(vec![Box::new(PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: ssrc,
            })])
            .await;
    };
    ask(track.clone(), control.clone()).await;
    let (frames, wants_key) = super::video::decoder_thread(uid, video, codec);
    let mut pictures = Assembler::new(VIDEO_PATIENCE, codec);
    let mut last_ask = Instant::now();
    // What arrived of their video, reported back to them.
    let mut received = Received::default();
    let mut reported = 0;
    let mut reports = tokio::time::interval(FEEDBACK);
    reports.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let ev = tokio::select! {
            ev = track.poll() => match ev {
                Some(ev) => ev,
                None => break,
            },
            _ = reports.tick() => {
                // Only while packets come: a track the other side swapped
                // for one in another codec goes quiet, and its old numbers
                // in between the new track's would read as no loss at all.
                if received.count != reported
                    && let Some(report) = received.report()
                {
                    reported = received.count;
                    control.post(report);
                }
                continue;
            }
        };
        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
            continue;
        };
        received.packet(pkt.header.sequence_number);
        let h = &pkt.header;
        let out = pictures.push(h.sequence_number, h.marker, pkt.payload, Instant::now());
        for picture in out.pictures {
            crate::probe::since("video.montagem", picture.first_at);
            if picture.key {
                crate::probe::count("video.chaves", 1);
            }
            if frames.send(picture.data).is_err() {
                return;
            }
        }
        if out.lost {
            crate::probe::count("video.perdidas", 1);
        }
        let broken = out.lost | wants_key.swap(false, Ordering::Relaxed);
        if broken && last_ask.elapsed() > KEY_ASK_GAP {
            last_ask = Instant::now();
            ask(track.clone(), control.clone()).await;
        }
    }
}

/// A WebRTC connection with the call's codecs and ICE servers, whose events
/// go to `handler`, and whose DTLS mixes in the secret of `pq`.
async fn new_pc(
    ice: Option<&IceConfig>,
    relay: &[RTCIceServer],
    handler: Arc<dyn PeerConnectionEventHandler>,
    pq: &Arc<crate::e2e::dtls::DtlsSecret>,
) -> Result<Arc<dyn PeerConnection>, String> {
    let mut media = MediaEngine::default();
    media
        .register_codec(opus(), RtpCodecKind::Audio)
        .map_err(|e| e.to_string())?;
    media
        .register_codec(h264(), RtpCodecKind::Video)
        .map_err(|e| e.to_string())?;
    // Offered too: this copy reads AV1 (voice/av1.rs). Which one my video
    // goes out in is my track's codec (plan::pick_codec).
    media
        .register_codec(av1(), RtpCodecKind::Video)
        .map_err(|e| e.to_string())?;
    let registry =
        register_default_interceptors(Registry::new(), &mut media).map_err(|e| e.to_string())?;
    let (servers, policy) = ice_servers(ice, relay);
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(servers)
        .with_ice_transport_policy(policy)
        .build();
    let settings = SettingEngineBuilder::new()
        .with_dtls_extra_secret(pq.hook())
        .build();
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_setting_engine(settings)
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        // UDP sockets with room for a burst of video (udp.rs).
        .with_runtime(Arc::new(super::udp::Runtime))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(pc))
}

/// The line of `sender`'s track offers both video codecs, whatever its
/// track's: the other side's answer then allows both, and a later track in
/// the other codec fits the connection. (This stack takes a track only in a
/// codec the last negotiation allowed, and a line otherwise offers just its
/// track's.) An older copy of the app answers H.264 alone, and gets nothing
/// else (plan::pick_codec).
async fn offer_both_codecs(pc: &dyn PeerConnection, sender: &Arc<dyn RtpSender>) {
    for t in pc.get_transceivers().await {
        if let Ok(Some(s)) = t.sender().await
            && s.id() == sender.id()
        {
            if let Err(e) = t.set_codec_preferences(vec![h264(), av1()]).await {
                crate::logln!("voz: os codecs do vídeo: {e}");
            }
            return;
        }
    }
}

/// Everything the call shares with each new connection.
struct Shared<'a> {
    ctx: &'a Ctx,
    events: &'a Events,
    pacer: &'a Arc<Pacer>,
}

async fn new_peer(
    uid: i64,
    s: &Shared<'_>,
    video: Option<Codec>,
    showing: Layer,
) -> Result<Peer, String> {
    let ctx = s.ctx;
    let (tx, rx) = unbounded_channel();
    let pq = crate::e2e::dtls::DtlsSecret::new();
    let pc = new_pc(
        ctx.ice.as_ref(),
        &ctx.relay,
        Arc::new(Handler { tx: tx.clone() }),
        &pq,
    )
    .await?;

    let ssrc = random_ssrc();
    let track = Arc::new(TrackLocalStaticRTP::new(media_track(
        RtpCodecKind::Audio,
        opus(),
        ssrc,
    )));
    let sender = pc
        .add_track(track.clone() as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| e.to_string())?;
    let audio_out = Arc::new(AudioOut {
        track,
        ssrc,
        pt: AtomicU8::new(OPUS_PT),
        seq: AtomicU16::new(random_u32() as u16),
        offset: random_u32(),
        sender,
    });
    let channel = pc
        .create_data_channel(
            CONTROL_LABEL,
            Some(RTCDataChannelInit {
                negotiated: Some(CONTROL_ID),
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    // Says when what I sent has mostly gone, for the pictures of my
    // presentation (`Control::room`).
    let _ = channel.set_buffered_amount_low_threshold(DRAINED).await;
    let (ordered_tx, ordered_rx) = unbounded_channel();
    let control = Arc::new(Control {
        channel,
        open: AtomicBool::new(false),
        drained: tokio::sync::Notify::new(),
        ordered: ordered_tx,
    });
    tokio::spawn(ordered(Arc::downgrade(&control), ordered_rx));
    let link = LINKS.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(watch_control(
        uid,
        link,
        control.clone(),
        s.events.clone(),
        ctx.slides.clone(),
        ctx.ink.clone(),
        ctx.music.clone(),
    ));

    let video_out = Arc::new(Mutex::new(None));
    let screen_out = Arc::new(Mutex::new(None));
    let (frames, frames_rx) = unbounded_channel();
    let backlog = Arc::new(AtomicUsize::new(0));
    tokio::spawn(write_video(
        video_out.clone(),
        frames_rx,
        backlog.clone(),
        s.pacer.clone(),
    ));
    let alive = Arc::new(AtomicBool::new(true));
    let worker = PeerTask {
        uid,
        polite: ctx.me < uid,
        pc,
        gw: ctx.gw.clone(),
        signals: ctx.signals.clone(),
        channel: ctx.channel,
        pq,
        audio_out: audio_out.clone(),
        video_out: video_out.clone(),
        screen_out: screen_out.clone(),
        control: control.clone(),
        events: s.events.clone(),
        audio: ctx.audio.clone(),
        video: ctx.video.clone(),
        slides: ctx.slides.clone(),
        link,
        alive: alive.clone(),
        dirty: true,
        making_offer: false,
        ignore_offer: false,
        pending: Vec::new(),
        video_changed: false,
    };
    let task = tokio::spawn(worker.run(rx));
    // A fresh connection does not always say it needs negotiating.
    let _ = tx.send(PeerEv::Negotiate);
    if video.is_some() {
        let _ = tx.send(PeerEv::Video(video));
    }
    Ok(Peer {
        tx,
        task,
        id: link,
        alive,
        remote_fp: None,
        audio: audio_out,
        frames,
        backlog,
        control,
        connected: false,
        relayed: false,
        asked: Layer::High,
        showing,
        link: Link::new(Instant::now()),
        health: None,
        fresh: false,
        received: (0, 0),
        loss: 0.0,
        rtt: None,
        sending: Layer::Off,
        switching: None,
        broken: true,
        screen_out,
        screen_track: false,
        screen_capable: false,
        screen_wanted: false,
        av1: None,
        slides_task: None,
    })
}

// ---------------------------------------------------------------- the call

/// The call as the main loop keeps it.
struct Call {
    peers: HashMap<i64, Peer>,
    /// The copies of what I share, while I share.
    copies: Option<Copies>,
    /// What I want of each person's video, for people who arrive later.
    wants: HashMap<i64, Layer>,
    budget: Budget,
    alloc: Allocation,
    last_tick: Instant,
    /// The tasks of connections hung up on and maybe still closing: the
    /// call is only over once they are.
    closing: Vec<JoinHandle<()>>,
    /// The PC's sound goes with my screen share.
    screen_audio: bool,
    /// Whose screen's sound I silenced here.
    screen_muted: HashSet<i64>,
    /// Whose screen I enlarged, while I watch one.
    screen_focus: Option<i64>,
    /// What my video goes out in.
    codec: Codec,
}

impl Call {
    /// My video's tracks, in the codec it goes out in, while I share.
    fn video(&self) -> Option<Codec> {
        self.copies.map(|_| self.codec)
    }

    /// Works out the codec again (the encoder running, who is in the call
    /// and what they read). When it changes while I share, everyone's track
    /// is made again in the new one, and each waits for its first full
    /// picture: the encoder starts the new codec with one.
    fn pick_codec(&mut self, layers: &LayerControl) {
        let codec = plan::pick_codec(layers.encodes_av1(), self.peers.values().map(|p| p.av1));
        layers.set_codec(codec);
        if codec == self.codec {
            return;
        }
        crate::logln!("vídeo: a chamada passa para {}", codec.wire());
        self.codec = codec;
        if let Some(video) = self.video() {
            for p in self.peers.values_mut() {
                p.broken = true;
                let _ = p.tx.send(PeerEv::Video(Some(video)));
            }
        }
    }

    /// Hangs up on one person: their connection closes on its own task,
    /// which the call keeps until it is done. It says nothing more to them
    /// from now on, and what came in over it (their sound's buffer, their
    /// last picture) goes now, before any new connection with them starts
    /// filling it again.
    fn hang_up(&mut self, uid: i64, p: Peer, ctx: &Ctx) {
        p.alive.store(false, Ordering::SeqCst);
        if let Some(sender) = &p.slides_task {
            sender.abort();
        }
        let _ = p.tx.send(PeerEv::Close);
        ctx.audio.forget(uid);
        ctx.video.forget(uid);
        self.closing.retain(|t| !t.is_finished());
        self.closing.push(p.task);
    }

    /// A connection with `uid`, in the call from now on. False if it
    /// could not be made.
    async fn connect(&mut self, uid: i64, s: &Shared<'_>) -> bool {
        let showing = self.wants.get(&uid).copied().unwrap_or_default();
        match new_peer(uid, s, self.video(), showing).await {
            Ok(p) => {
                self.peers.insert(uid, p);
                true
            }
            Err(e) => {
                crate::logln!("voz: conexão com {uid}: {e}");
                false
            }
        }
    }

    /// Hangs up on `uid`'s connection and makes a new one: its first offer
    /// carries a new DTLS certificate, which makes them replace theirs too
    /// (see `Cmd::Signal` in `run`).
    async fn renew(&mut self, uid: i64, s: &Shared<'_>) {
        if let Some(p) = self.peers.remove(&uid) {
            self.hang_up(uid, p, s.ctx);
        }
        self.connect(uid, s).await;
        self.pick_codec(&s.ctx.layers);
        self.reallocate(&s.ctx.layers, s.pacer);
    }

    /// Who my voice goes to, as the call is now, and who gets the sound of
    /// my screen share: whoever has its track and did not silence it.
    fn hear(&self, ears: &Ears, screen_ears: &Ears) {
        let voice = self
            .peers
            .values()
            .filter(|p| p.connected)
            .map(|p| p.audio.clone())
            .collect();
        set_ears(ears, voice);
        let screen = self
            .peers
            .values()
            .filter(|p| p.connected && p.screen_wanted)
            .filter_map(|p| {
                p.screen_out
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            })
            .collect();
        set_ears(screen_ears, screen);
    }

    /// Whether I take the sound of `uid`'s screen share: it is not
    /// silenced here, and while I watch one screen enlarged, it is that
    /// one. Of several screens shared, the one being watched is the one
    /// heard; with none enlarged, all of them are.
    fn takes_screen(&self, uid: i64) -> bool {
        !self.screen_muted.contains(&uid) && self.screen_focus.is_none_or(|f| f == uid)
    }

    fn screen_wants(&self) -> HashMap<i64, bool> {
        self.peers
            .keys()
            .map(|uid| (*uid, self.takes_screen(*uid)))
            .collect()
    }

    /// Tells everyone whose answer to `takes_screen` changed since
    /// `before`: they stop or start sending me their screen's sound.
    fn tell_screen_wants(&self, before: &HashMap<i64, bool>) {
        for (uid, p) in &self.peers {
            let now = self.takes_screen(*uid);
            if before.get(uid) != Some(&now) {
                p.control.post(screen_audio_message(now));
            }
        }
    }

    /// The track of my screen's sound goes to those who know it, while it
    /// is shared.
    fn screen_tracks(&mut self) {
        for p in self.peers.values_mut() {
            let want = self.screen_audio && p.screen_capable;
            if want != p.screen_track {
                p.screen_track = want;
                let _ = p.tx.send(PeerEv::Screen(want));
            }
        }
    }

    /// Shares my video out again: who gets which copy, each copy's bitrate,
    /// and the pace.
    fn reallocate(&mut self, layers: &LayerControl, pacer: &Pacer) {
        let Some(copies) = self.copies else {
            for p in self.peers.values_mut() {
                p.sending = Layer::Off;
                p.switching = None;
            }
            layers.reset();
            self.alloc = Allocation::default();
            return;
        };
        // Who looks at me enlarged comes first, then the rest, in a fixed
        // order so the same people keep their copies when the budget is
        // tight.
        let mut asks: Vec<(i64, Layer)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(uid, p)| (*uid, p.asked.min(p.link.cap)))
            .collect();
        asks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let alloc = plan::allocate(&asks, self.budget.kbps, &copies);
        for (uid, p) in &mut self.peers {
            let target = alloc.layer(*uid);
            if target == Layer::Off {
                p.sending = Layer::Off;
                p.switching = None;
            } else if target == p.sending {
                p.switching = None;
            } else {
                // From the next full picture of the new copy.
                p.switching = Some(target);
                layers.want_key(target);
            }
        }
        for layer in Layer::COPIES {
            let used = self
                .peers
                .values()
                .any(|p| p.sending == layer || p.switching == Some(layer));
            let kbps = if layer == Layer::Low {
                alloc.low_kbps
            } else {
                alloc.high_kbps
            };
            layers.set(layer, used, kbps);
        }
        pacer.set_kbps(self.budget.kbps.max(alloc.kbps()));
        self.alloc = alloc;
    }

    /// Once a second: reads what the reports said, moves the budget and
    /// each person's ceiling, and shares the video out again.
    fn tick(&mut self, ctx: &Ctx, pacer: &Pacer) {
        let now = Instant::now();
        let secs = now
            .saturating_duration_since(self.last_tick)
            .as_secs_f64()
            .max(0.1);
        self.last_tick = now;
        let sent_kbps = (pacer.sent.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1000.0 / secs) as u32;
        if self.copies.is_some() {
            let receiving: Vec<&Peer> = self
                .peers
                .values()
                .filter(|p| p.sending != Layer::Off || p.switching.is_some())
                .collect();
            let congested = receiving
                .iter()
                .filter(|p| p.fresh && p.health == Some(Health::Congested))
                .count();
            let total = receiving.len();
            let shared = plan::shared_congestion(congested, total);
            for p in self.peers.values_mut() {
                if std::mem::take(&mut p.fresh)
                    && let Some(health) = p.health
                {
                    p.link.update(now, health, shared, p.asked);
                }
            }
            self.budget
                .update(now, sent_kbps, congested, total, self.alloc.short);
        }
        // The encoder may have changed (the card stopped and the processor
        // took over: H.264 only).
        self.pick_codec(&ctx.layers);
        self.reallocate(&ctx.layers, pacer);
        let voice = plan::voice_plan(self.peers.len());
        ctx.audio.set_plan(voice);
        self.report(ctx, voice);
    }

    fn report(&self, ctx: &Ctx, voice: VoicePlan) {
        let mut peers: Vec<PeerReport> = self
            .peers
            .iter()
            .map(|(uid, p)| {
                let (video_in, video_size) = ctx.video.seen(*uid);
                let audio_in = ctx
                    .audio
                    .peers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(uid)
                    .map_or(0, |a| a.packets.load(Ordering::Relaxed));
                let screen_audio_in = ctx
                    .audio
                    .screens
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(uid)
                    .map_or(0, |a| a.packets.load(Ordering::Relaxed));
                let screen_audio_out = p.screen_wanted
                    && p.screen_out
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .is_some();
                PeerReport {
                    user: *uid,
                    connected: p.connected,
                    relayed: p.relayed,
                    asked: p.asked,
                    cap: p.link.cap,
                    sending: p.sending,
                    showing: p.showing,
                    health: p.health,
                    loss: p.loss,
                    rtt_ms: p.rtt.map(|r| (r * 1000.0) as u32),
                    audio_in,
                    video_in,
                    video_size,
                    screen_audio_in,
                    screen_audio_out,
                }
            })
            .collect();
        peers.sort_by_key(|p| p.user);
        *ctx.report.lock().unwrap_or_else(PoisonError::into_inner) = Report {
            peers,
            budget_kbps: self.budget.kbps,
            low_kbps: self.alloc.low_kbps,
            high_kbps: self.alloc.high_kbps,
            voice,
            codec: self.codec,
        };
    }

    /// Hands a picture to everyone who gets that copy: from its first full
    /// picture on for someone moving to it, and never into a writer that
    /// has fallen behind.
    fn route(&mut self, frame: &Arc<EncodedFrame>, layers: &LayerControl) {
        // The encoder's last pictures in a codec the call has left: as if
        // they never came (they would end someone's wait for a full one).
        if frame.codec != self.codec {
            return;
        }
        for p in self.peers.values_mut() {
            if p.switching == Some(frame.layer) && frame.key {
                p.sending = frame.layer;
                p.switching = None;
                p.broken = false;
            }
            if p.sending != frame.layer || !p.connected {
                continue;
            }
            if p.broken && !frame.key {
                continue;
            }
            if p.backlog.load(Ordering::Relaxed) >= MAX_BACKLOG {
                if !p.broken {
                    p.broken = true;
                    layers.want_key(frame.layer);
                }
                continue;
            }
            p.broken = false;
            p.backlog.fetch_add(1, Ordering::Relaxed);
            let _ = p.frames.send(frame.clone());
        }
    }

    fn event(&mut self, ev: Event, ctx: &Ctx, pacer: &Pacer) {
        match ev {
            Event::Open(uid) => {
                if let Some(p) = self.peers.get(&uid) {
                    // The codecs first: whoever says "want" before them is
                    // an older copy (see `codecs_message`).
                    p.control.post_all(vec![
                        codecs_message(),
                        want_message(p.showing),
                        screen_audio_message(self.takes_screen(uid)),
                        slides::wire::hello(),
                    ]);
                    // What I share that can be annotated, and its strokes,
                    // in line with the strokes still going out.
                    for msg in ctx.ink.hello() {
                        p.control.post_ordered(msg);
                    }
                    // The music queue, so the new person sees what is
                    // playing and what comes next.
                    let snapshot = ctx.music.snapshot();
                    if snapshot["queue"].as_array().is_some_and(|a| !a.is_empty()) {
                        p.control.post(snapshot);
                    }
                }
            }
            Event::SlidesHello(uid) => {
                if let Some(p) = self.peers.get_mut(&uid)
                    && p.slides_task.is_none()
                {
                    p.slides_task = Some(tokio::spawn(send_slides(
                        uid,
                        p.control.clone(),
                        ctx.slides.clone(),
                    )));
                }
            }
            Event::Codecs(uid, av1) => {
                if let Some(p) = self.peers.get_mut(&uid) {
                    p.av1 = Some(av1);
                }
                self.pick_codec(&ctx.layers);
            }
            Event::ScreenWant(uid, want) => {
                if let Some(p) = self.peers.get_mut(&uid) {
                    p.screen_capable = true;
                    p.screen_wanted = want;
                }
                self.screen_tracks();
            }
            Event::Want(uid, layer) => {
                let mut older = false;
                if let Some(p) = self.peers.get_mut(&uid) {
                    // A copy of the app that reads AV1 says so before this.
                    if p.av1.is_none() {
                        p.av1 = Some(false);
                        older = true;
                    }
                    if p.asked != layer {
                        p.asked = layer;
                        self.reallocate(&ctx.layers, pacer);
                    }
                }
                if older {
                    self.pick_codec(&ctx.layers);
                }
            }
            Event::Key(uid) => {
                if let Some(p) = self.peers.get(&uid) {
                    if p.sending != Layer::Off {
                        ctx.layers.want_key(p.sending);
                    }
                    if let Some(next) = p.switching {
                        ctx.layers.want_key(next);
                    }
                }
            }
            Event::Connected(uid, up) => {
                if let Some(p) = self.peers.get_mut(&uid)
                    && p.connected != up
                {
                    p.connected = up;
                    p.broken = true;
                    self.reallocate(&ctx.layers, pacer);
                }
            }
            // Made again by the main loop (`renew`), which can wait.
            Event::Failed(_) => {}
            Event::VideoReady(uid) => {
                if let Some(p) = self.peers.get_mut(&uid) {
                    // Whatever went before the track was ready is lost.
                    p.broken = true;
                    if p.sending != Layer::Off {
                        ctx.layers.want_key(p.sending);
                    }
                }
            }
            Event::Stats(uid, s) => {
                if let Some(p) = self.peers.get_mut(&uid) {
                    p.relayed = s.relayed;
                }
            }
            Event::Rtt(uid, rtt) => {
                if let Some(p) = self.peers.get_mut(&uid) {
                    p.rtt = Some(rtt);
                }
            }
            Event::Received(uid, got, expected) => {
                if let Some(p) = self.peers.get_mut(&uid) {
                    // Over what they received since their last report.
                    let (was_got, was_expected) = p.received;
                    p.received = (got, expected);
                    let span = expected.saturating_sub(was_expected);
                    if span == 0 || expected < was_expected {
                        return;
                    }
                    let lost = span.saturating_sub(got.saturating_sub(was_got));
                    let loss = (lost as f64 / span as f64).clamp(0.0, 1.0);
                    p.loss = loss;
                    p.health = Some(p.link.health(Some(loss), p.rtt));
                    p.fresh = true;
                }
            }
        }
    }
}

/// Runs a call until it is closed: a connection to each person, the
/// microphone's packets to all of them, the PC's sound of my screen share
/// to those who take it, and my video to each in the copy they get.
pub async fn run(
    ctx: Ctx,
    mut cmds: UnboundedReceiver<Cmd>,
    sound: UnboundedReceiver<audio::Packet>,
    screen_sound: UnboundedReceiver<audio::Packet>,
    mut pictures: UnboundedReceiver<EncodedFrame>,
    mut music_out: UnboundedReceiver<crate::music::Outgoing>,
) {
    let (events, mut events_rx) = unbounded_channel();
    let pacer = Arc::new(Pacer::new(plan::START_BUDGET));
    // What I annotate goes to everyone (each connection's data channel).
    let (ink_out, mut ink_rx) = unbounded_channel();
    ctx.ink.attach(ink_out);
    // The voice goes out from a task of its own, so nothing here (a new
    // connection being set up, say) ever holds it back; the screen's sound
    // from another.
    let ears = Ears::default();
    let voice = tokio::spawn(send_sound(sound, ears.clone(), ctx.audio.clone(), |s| {
        &s.sent
    }));
    let screen_ears = Ears::default();
    let screen = tokio::spawn(send_sound(
        screen_sound,
        screen_ears.clone(),
        ctx.audio.clone(),
        |s| &s.screen_sent,
    ));
    let mut call = Call {
        peers: HashMap::new(),
        copies: None,
        wants: HashMap::new(),
        budget: Budget::default(),
        alloc: Allocation::default(),
        last_tick: Instant::now(),
        closing: Vec::new(),
        screen_audio: false,
        screen_muted: HashSet::new(),
        screen_focus: None,
        codec: Codec::H264,
    };
    ctx.layers.set_codec(Codec::H264);
    let mut ticks = tokio::time::interval(TICK);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Sync(members)) => {
                    let others: Vec<i64> = members.into_iter().filter(|uid| *uid != ctx.me).collect();
                    let shared = Shared { ctx: &ctx, events: &events, pacer: &pacer };
                    for &uid in &others {
                        if !call.peers.contains_key(&uid) {
                            call.connect(uid, &shared).await;
                        }
                    }
                    let gone: Vec<(i64, Peer)> = call
                        .peers
                        .extract_if(|uid, _| !others.contains(uid))
                        .collect();
                    for (uid, p) in gone {
                        call.hang_up(uid, p, &ctx);
                    }
                    ctx.audio.set_plan(plan::voice_plan(call.peers.len()));
                    // Someone who read only H.264 may have left.
                    call.pick_codec(&ctx.layers);
                    call.reallocate(&ctx.layers, &pacer);
                }
                Some(Cmd::Signal(from, data)) => {
                    if from == ctx.me {
                        continue;
                    }
                    let shared = Shared { ctx: &ctx, events: &events, pacer: &pacer };
                    // A description with another DTLS certificate than the
                    // one this connection knows comes from a new connection
                    // of theirs: they hung up on the one mine talks to (the
                    // server told them I had left, say, when my gateway
                    // dropped), and that is a new DTLS association (RFC
                    // 8842). This stack would take it as a renegotiation of
                    // mine, restart ICE and keep the old association, which
                    // they no longer have: ICE connects and nothing ever
                    // flows again, either way. Mine is made again instead,
                    // and the new one gets the description.
                    let fp = description_fingerprint(&data);
                    let replaced = call.peers.get(&from).is_some_and(|p| {
                        matches!((&p.remote_fp, &fp), (Some(had), Some(new)) if had != new)
                    });
                    if replaced {
                        crate::logln!("voz: {from} fez uma conexão nova; a minha com ele também é refeita");
                        if let Some(p) = call.peers.remove(&from) {
                            call.hang_up(from, p, &ctx);
                        }
                    }
                    // Someone may signal before their VOICE_STATE arrives.
                    if !call.peers.contains_key(&from) {
                        if !call.connect(from, &shared).await {
                            continue;
                        }
                        if replaced {
                            call.pick_codec(&ctx.layers);
                            call.reallocate(&ctx.layers, &pacer);
                        }
                    }
                    // Their new connection answered an offer of my old one:
                    // that answer is of no use to the new one, whose own
                    // offer goes out instead (its new certificate makes
                    // them start theirs over too).
                    let offer = data["description"]["type"].as_str() == Some("offer");
                    if replaced && !offer {
                        continue;
                    }
                    if let Some(p) = call.peers.get_mut(&from) {
                        if fp.is_some() {
                            p.remote_fp = fp;
                        }
                        let _ = p.tx.send(PeerEv::Remote(data));
                    }
                }
                // The gateway is back after a drop. What I sent over it in
                // between (an offer, an answer) was lost: the server kept
                // what came for me (while it held my place), not what I
                // sent. A connection that is not up may be waiting for
                // something that will never come: those are made again, and
                // their new offers go over the live gateway (after the
                // voice_join that went just before this, so the server
                // passes them on). The ones that are up send again an offer
                // still unanswered.
                Some(Cmd::Reconnected) => {
                    let down: Vec<i64> = call
                        .peers
                        .iter()
                        .filter(|(_, p)| !p.connected)
                        .map(|(uid, _)| *uid)
                        .collect();
                    for p in call.peers.values().filter(|p| p.connected) {
                        let _ = p.tx.send(PeerEv::Resend);
                    }
                    let shared = Shared { ctx: &ctx, events: &events, pacer: &pacer };
                    for uid in down {
                        crate::logln!("voz: o gateway voltou e a conexão com {uid} não está de pé; fazendo outra");
                        call.renew(uid, &shared).await;
                    }
                }
                Some(Cmd::Video(copies)) => {
                    let was_on = call.copies.is_some();
                    // The codec for what the new encoder makes, before any
                    // track is made (`pick_codec` only remakes tracks that
                    // exist, and there are none yet).
                    call.copies = None;
                    call.pick_codec(&ctx.layers);
                    call.copies = copies;
                    if was_on || copies.is_some() {
                        let video = call.video();
                        for p in call.peers.values_mut() {
                            p.broken = true;
                            let _ = p.tx.send(PeerEv::Video(video));
                        }
                    }
                    call.reallocate(&ctx.layers, &pacer);
                }
                Some(Cmd::Wants(wants)) => {
                    for (uid, p) in &mut call.peers {
                        let want = wants.get(uid).copied().unwrap_or_default();
                        if want != p.showing {
                            p.showing = want;
                            p.control.post(want_message(want));
                            if want == Layer::Off {
                                ctx.video.forget(*uid);
                            }
                        }
                    }
                    call.wants = wants;
                }
                Some(Cmd::ScreenAudio(on)) => {
                    call.screen_audio = on;
                    call.screen_tracks();
                }
                Some(Cmd::ScreenMuted(muted)) => {
                    let before = call.screen_wants();
                    call.screen_muted = muted;
                    call.tell_screen_wants(&before);
                }
                Some(Cmd::ScreenFocus(focus)) => {
                    let before = call.screen_wants();
                    call.screen_focus = focus;
                    call.tell_screen_wants(&before);
                }
                Some(Cmd::Close) | None => break,
            },
            Some((link, ev)) = events_rx.recv() => {
                let uid = ev.uid();
                // From a connection hung up on since (made again, say): it
                // no longer speaks for them.
                if !call.peers.get(&uid).is_some_and(|p| p.id == link) {
                    continue;
                }
                if let Event::Failed(_) = ev {
                    crate::logln!("voz: a conexão com {uid} falhou; fazendo outra");
                    let shared = Shared { ctx: &ctx, events: &events, pacer: &pacer };
                    call.renew(uid, &shared).await;
                } else {
                    call.event(ev, &ctx, &pacer);
                }
            }
            Some(frame) = pictures.recv() => {
                call.route(&Arc::new(frame), &ctx.layers);
            }
            Some(msg) = ink_rx.recv() => {
                for p in call.peers.values() {
                    p.control.post_ordered(msg.clone());
                }
            }
            Some(out) = music_out.recv() => {
                match out.to {
                    Some(uid) => if let Some(p) = call.peers.get(&uid) {
                        p.control.post(out.msg);
                    }
                    None => for p in call.peers.values() {
                        p.control.post(out.msg.clone());
                    }
                }
            }
            _ = ticks.tick() => call.tick(&ctx, &pacer),
        }
        call.hear(&ears, &screen_ears);
    }
    ctx.ink.detach();
    voice.abort();
    screen.abort();
    let everyone: Vec<(i64, Peer)> = call.peers.drain().collect();
    for (uid, p) in everyone {
        call.hang_up(uid, p, &ctx);
    }
    ctx.layers.reset();
    // The call is over once every connection has closed, which is also when
    // each one has sent the release of its relay allocation (the relay over
    // TCP and TLS, closed after this, still carries it to the server).
    let closing = futures_util::future::join_all(std::mem::take(&mut call.closing));
    if tokio::time::timeout(CLOSE_WAIT, closing).await.is_err() {
        crate::logln!("voz: as conexões não fecharam em {CLOSE_WAIT:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IceServer;

    #[test]
    fn only_usable_ice_addresses_are_kept() {
        assert!(usable_ice_url("stun:stun.cloudflare.com:3478"));
        assert!(usable_ice_url(
            "turn:turn.cloudflare.com:3478?transport=udp"
        ));
        assert!(usable_ice_url("turn:turn.cloudflare.com:3478"));
        assert!(!usable_ice_url(
            "turn:turn.cloudflare.com:3478?transport=tcp"
        ));
        assert!(!usable_ice_url(
            "turns:turn.cloudflare.com:5349?transport=tcp"
        ));

        let ice = IceConfig {
            ice_servers: vec![
                IceServer {
                    urls: vec!["stun:a".into()],
                    ..Default::default()
                },
                IceServer {
                    urls: vec![
                        "turn:b?transport=udp".into(),
                        "turns:b:5349?transport=tcp".into(),
                    ],
                    username: Some("u".into()),
                    credential: Some("p".into()),
                },
                // A relay without credentials would make the stack fail.
                IceServer {
                    urls: vec!["turn:c".into()],
                    ..Default::default()
                },
            ],
            relay_only: true,
        };
        // The TLS address reaches the stack through the local relay instead.
        let relay = [RTCIceServer {
            urls: vec!["turn:127.0.0.1:50000?transport=udp".into()],
            username: "u".into(),
            credential: "p".into(),
        }];
        let (servers, policy) = ice_servers(Some(&ice), &relay);
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[1].urls, ["turn:b?transport=udp"]);
        assert_eq!(servers[1].username, "u");
        assert_eq!(servers[2].urls, ["turn:127.0.0.1:50000?transport=udp"]);
        assert_eq!(policy, RTCIceTransportPolicy::Relay);

        let (servers, policy) = ice_servers(None, &[]);
        assert_eq!(servers[0].urls, [FALLBACK_STUN]);
        assert_eq!(policy, RTCIceTransportPolicy::All);
    }

    #[test]
    fn sdp_attributes_are_found() {
        let sdp =
            "v=0\r\na=group:BUNDLE 0 1\r\nm=audio 9 UDP 111\r\na=mid:0\r\na=ice-ufrag:abcd\r\n";
        assert_eq!(sdp_attr(sdp, "mid"), Some("0"));
        assert_eq!(sdp_attr(sdp, "ice-ufrag"), Some("abcd"));
        assert_eq!(sdp_attr(sdp, "ice-pwd"), None);
    }

    #[test]
    fn descriptions_have_the_browser_shape() {
        let json = r#"{"type":"offer","sdp":"v=0\r\n"}"#;
        let d: RTCSessionDescription = serde_json::from_str(json).unwrap();
        assert_eq!(d.sdp_type, RTCSdpType::Offer);
        let back = serde_json::to_value(&d).unwrap();
        assert_eq!(back["type"], "offer");
        assert_eq!(back["sdp"], "v=0\r\n");
        let c: RTCIceCandidateInit = serde_json::from_str(
            r#"{"candidate":"candidate:1 1 udp 1 1.2.3.4 5 typ host","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"x"}"#,
        )
        .unwrap();
        assert_eq!(c.sdp_mid.as_deref(), Some("0"));
        assert_eq!(c.sdp_mline_index, Some(0));
    }

    #[test]
    fn control_messages_read_back() {
        assert_eq!(want_message(Layer::Low)["video"], "low");
        assert_eq!(want_message(Layer::Off)["t"], "want");
        assert_eq!(key_message()["t"], "key");
        let back = Layer::from_wire(want_message(Layer::High)["video"].as_str().unwrap());
        assert_eq!(back, Some(Layer::High));
    }

    #[tokio::test]
    async fn the_pacer_spreads_bursts_over_time() {
        // 800 kb/s of budget: half again that is 150 000 bytes a second.
        let pacer = Pacer::new(800);
        let started = Instant::now();
        // A quarter second's worth goes at once, then the rest waits.
        for _ in 0..6 {
            pacer.take(25_000).await;
        }
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(600), "{took:?}");
        assert!(took < Duration::from_millis(1600), "{took:?}");
        assert_eq!(pacer.sent.load(Ordering::Relaxed), 150_000);
    }
}

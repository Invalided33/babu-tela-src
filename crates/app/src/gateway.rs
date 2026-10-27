//! The live connection. One WebSocket carries everything that happens while
//! the app is open: new messages, who is online, who is in a call, and the
//! signalling for the calls themselves.
//!
//! The server answers our heartbeat without waking the object that holds the
//! database, but only if the frame is byte for byte the string it registered,
//! so `PING` below is sent exactly as it is.
//!
//! Every connection the app opens gets a number (its generation). Signing out
//! and in again starts a new one, and anything still arriving from an older
//! one is ignored, so two sessions can never mix.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::net::Api;

/// The heartbeat. The runtime replies `{"t":"PONG"}` to this exact string.
const PING: &str = r#"{"op":"ping"}"#;

/// How often the heartbeat goes out.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// Longest wait between attempts to reconnect.
const MAX_BACKOFF: Duration = Duration::from_secs(15);

/// Gives up on a handshake that hangs, so the wait-and-retry takes over.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(90);

/// What the WebSocket runs over: a TCP connection, or a Tor stream.
trait Transport: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Transport for T {}

/// What the connection reports back to the interface.
#[derive(Clone, Debug)]
pub enum GwEvent {
    /// One event from the server: `t` and its payload (`Null` when the frame
    /// had none, as PONG, AUTH_FAIL and VOICE_KICK do).
    Event(String, Value),
    /// The connection dropped and will be retried after this long.
    Closed(Duration),
}

/// The wait before attempt number `tries` (0 for the first retry):
/// 1, 2, 4, 8 and then 15 seconds.
pub fn backoff(tries: u32) -> Duration {
    MAX_BACKOFF.min(Duration::from_secs(1) * 2u32.pow(tries.min(5)))
}

struct Slot {
    generation: u64,
    tx: Option<mpsc::UnboundedSender<Message>>,
}

/// The way into the live connection, shared by the interface and the calls.
/// Operations sent while the connection is down are dropped, as the browser
/// version did: everything the interface needs arrives again in the next
/// READY, and a call makes again the connections whose signalling may have
/// been lost in between (voice/rtc.rs, `Cmd::Reconnected`).
#[derive(Clone)]
pub struct Sender {
    slot: Arc<Mutex<Slot>>,
    current: Arc<AtomicU64>,
    kick: Arc<Notify>,
}

impl Default for Sender {
    fn default() -> Sender {
        Sender {
            slot: Arc::new(Mutex::new(Slot {
                generation: 0,
                tx: None,
            })),
            current: Arc::new(AtomicU64::new(0)),
            kick: Arc::new(Notify::new()),
        }
    }
}

impl Sender {
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether it went into a connection (which may still fall before the
    /// server reads it).
    pub fn send(&self, op: &Value) -> bool {
        self.slot()
            .tx
            .as_ref()
            .is_some_and(|tx| tx.send(Message::text(op.to_string())).is_ok())
    }

    /// Whether there is a connection to send through right now.
    #[cfg(test)]
    fn live(&self) -> bool {
        self.slot().tx.is_some()
    }

    /// Starts a new generation: the previous connection loses its way out
    /// at once, which also ends its writing half.
    pub fn begin(&self) -> u64 {
        let generation = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let mut slot = self.slot();
        slot.generation = generation;
        slot.tx = None;
        generation
    }

    /// Forgets the connection for good (signing out).
    pub fn end(&self) {
        self.begin();
    }

    pub fn generation(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    /// Cuts short the wait before the next attempt ("Tentar agora").
    pub fn retry_now(&self) {
        self.kick.notify_one();
    }

    fn attach(&self, generation: u64, tx: mpsc::UnboundedSender<Message>) -> bool {
        let mut slot = self.slot();
        if slot.generation != generation {
            return false;
        }
        slot.tx = Some(tx);
        true
    }

    fn detach(&self, generation: u64) {
        let mut slot = self.slot();
        if slot.generation == generation {
            slot.tx = None;
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }
}

/// Keeps a connection up for as long as `generation` is the current one,
/// reconnecting with a growing wait. `out` receives every event and returns
/// `false` once nobody is listening any more.
pub async fn run(
    api: Arc<Api>,
    sender: Sender,
    generation: u64,
    out: impl Fn(GwEvent) -> bool + Send + Sync + 'static,
) {
    let mut tries: u32 = 0;
    loop {
        if !sender.is_current(generation) {
            return;
        }
        let Some(token) = api.token() else { return };
        // A connection that reached READY resets the wait.
        if once(&api, &token, &sender, generation, &out).await {
            tries = 0;
        }
        sender.detach(generation);
        if !sender.is_current(generation) || api.token().is_none() {
            return;
        }
        let delay = backoff(tries);
        tries = tries.saturating_add(1);
        if !out(GwEvent::Closed(delay)) {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = sender.kick.notified() => tries = 0,
        }
    }
}

/// One connection, from the handshake to the close. Returns whether the
/// server accepted the session (a READY arrived).
async fn once(
    api: &Api,
    token: &str,
    sender: &Sender,
    generation: u64,
    out: &(impl Fn(GwEvent) -> bool + Send + Sync),
) -> bool {
    let Ok(mut request) = api.gateway_url().into_client_request() else {
        return false;
    };
    if let Some((name, value)) = api.test_header() {
        let Ok(value) = value.parse() else {
            return false;
        };
        request.headers_mut().insert(name, value);
    }
    let uri = request.uri().clone();
    let host = uri.host().unwrap_or_default().to_owned();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let connect = async move {
        let stream: Box<dyn Transport> = if crate::tor::active() {
            // Through Tor (tor.rs): the name is looked up at the exit.
            Box::new(crate::tor::connect(&host, port).await.ok()?)
        } else {
            let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .ok()?;
            // Without Nagle: what goes out here is small (typing, call
            // signalling, heartbeats), and the kernel would otherwise hold
            // one back until the server acknowledges the one before.
            tcp.set_nodelay(true).ok()?;
            Box::new(tcp)
        };
        tokio_tungstenite::client_async_tls_with_config(request, stream, None, None)
            .await
            .ok()
    };
    // Tor's circuits take longer to open (and the first wait for it to
    // join the network).
    let timeout = if crate::tor::active() {
        TOR_CONNECT_TIMEOUT
    } else {
        CONNECT_TIMEOUT
    };
    let Ok(Some((stream, _))) = tokio::time::timeout(timeout, connect).await else {
        return false;
    };
    let (mut write, mut read) = stream.split();

    // Say who we are straight away: the server closes a connection that
    // stays quiet for ten seconds.
    let hello = json!({ "op": "identify", "token": token }).to_string();
    if write.send(Message::text(hello)).await.is_err() {
        return false;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    if !sender.attach(generation, tx) {
        return false;
    }

    // One task owns the writing half: the heartbeat and whatever the
    // interface asks to send. It ends when the sender is replaced or
    // dropped, which closes the socket.
    let writer = tokio::spawn(async move {
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate, and identify just went out.
        beat.tick().await;
        loop {
            let frame = tokio::select! {
                _ = beat.tick() => Message::text(PING),
                msg = rx.recv() => match msg {
                    Some(msg) => msg,
                    None => break,
                },
            };
            if write.send(frame).await.is_err() {
                break;
            }
        }
        let _ = write.close().await;
    });

    let mut ready = false;
    while let Some(Ok(frame)) = read.next().await {
        if !sender.is_current(generation) {
            break;
        }
        let text = match frame {
            Message::Text(t) => t.as_str().to_owned(),
            Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Message::Close(_) => break,
            _ => continue,
        };
        let Some((t, d)) = parse(&text) else { continue };
        // The heartbeat's answer carries nothing and needs no one.
        if t == "PONG" {
            continue;
        }
        if t == "READY" {
            ready = true;
        }
        if !out(GwEvent::Event(t, d)) {
            break;
        }
    }
    sender.detach(generation);
    writer.abort();
    ready
}

/// Splits a frame into its event name and payload.
fn parse(text: &str) -> Option<(String, Value)> {
    let Ok(Value::Object(mut frame)) = serde_json::from_str::<Value>(text) else {
        return None;
    };
    let Some(Value::String(t)) = frame.remove("t") else {
        return None;
    };
    Some((t, frame.remove("d").unwrap_or(Value::Null)))
}

/// The operations the interface sends up the connection.
pub mod op {
    use serde_json::{Value, json};

    use crate::model::Video;

    pub fn typing(channel_id: i64) -> Value {
        json!({ "op": "typing", "channelId": channel_id })
    }

    /// A message, the way POST /channels/{id}/messages takes it: its
    /// envelope (e2e/msg.rs: text, sticker, quote and files, encrypted), the
    /// files the server keeps track of, and the sender's mark.
    pub fn message(channel_id: i64, envelope: &Value, files: &[Value], nonce: &str) -> Value {
        json!({
            "op": "message", "channelId": channel_id, "e2e": envelope, "files": files,
            "nonce": nonce,
        })
    }

    pub fn status(status: &str) -> Value {
        json!({ "op": "status", "status": status })
    }

    /// Deafening implies muting on the wire: the server keeps one flag for
    /// "cannot hear me", while the interface keeps the two apart.
    pub fn voice_join(channel_id: i64, mute: bool, deaf: bool, video: Option<Video>) -> Value {
        json!({
            "op": "voice_join", "channelId": channel_id,
            "mute": mute || deaf, "deaf": deaf, "video": video.map(Video::wire),
        })
    }

    pub fn voice_state(mute: bool, deaf: bool, video: Option<Video>) -> Value {
        json!({
            "op": "voice_state",
            "mute": mute || deaf, "deaf": deaf, "video": video.map(Video::wire),
        })
    }

    pub fn voice_leave() -> Value {
        json!({ "op": "voice_leave" })
    }

    pub fn signal(to: i64, data: Value) -> Value {
        json!({ "op": "signal", "to": to, "data": data })
    }

    /// One step in setting up the transfer of a file sent directly
    /// (direct/): to `to`, about file `file` of message `msg`. The server
    /// only carries it between that message's author and someone who may
    /// read it (worker/src/direct.rs).
    pub fn direct(to: i64, msg: i64, file: &str, data: Value) -> Value {
        json!({ "op": "direct", "to": to, "msg": msg, "file": file, "data": data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_heartbeat_is_the_exact_string_the_server_registered() {
        assert_eq!(PING, "{\"op\":\"ping\"}");
    }

    #[test]
    fn messages_carry_only_their_envelope() {
        let env = serde_json::json!({ "v": 1, "s": "a", "n": "b", "c": "c" });
        let v = op::message(
            3,
            &env,
            &[serde_json::json!({ "url": "/uploads/x.bin" })],
            "n1",
        );
        assert_eq!(
            (v["op"].as_str(), v["nonce"].as_str()),
            (Some("message"), Some("n1"))
        );
        assert_eq!(v["e2e"], env);
        assert_eq!(v["files"][0]["url"], "/uploads/x.bin");
        // Nothing in plain text next to it.
        for k in ["content", "sticker", "replyTo", "attachments"] {
            assert!(v.get(k).is_none(), "{k}");
        }
    }

    #[test]
    fn muting_is_implied_by_deafening() {
        let v = op::voice_state(false, true, None);
        assert_eq!(v["mute"], true);
        assert_eq!(v["deaf"], true);
        assert!(v["video"].is_null());
        let v = op::voice_join(7, false, false, Some(crate::model::Video::Screen));
        assert_eq!(v["channelId"], 7);
        assert_eq!(v["mute"], false);
        assert_eq!(v["video"], "screen");
    }

    #[test]
    fn frames_without_a_payload_still_parse() {
        assert_eq!(
            parse(r#"{"t":"AUTH_FAIL"}"#),
            Some(("AUTH_FAIL".into(), Value::Null))
        );
        let (t, d) = parse(r#"{"t":"TYPING","d":{"channelId":1,"userId":2}}"#).unwrap();
        assert_eq!(t, "TYPING");
        assert_eq!(d["userId"], 2);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"d":1}"#), None);
        assert_eq!(parse(r#"[1,2]"#), None);
    }

    #[test]
    fn an_old_generation_cannot_take_over_the_connection() {
        let sender = Sender::default();
        assert!(!sender.live());
        // Nothing to send through, and nothing blows up.
        sender.send(&op::voice_leave());

        let first = sender.begin();
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(sender.attach(first, tx));
        assert!(sender.live());

        // Signing in again replaces it...
        let second = sender.begin();
        assert!(!sender.live());
        // ...and the old connection can neither come back nor clear the new one.
        let (tx_old, _rx_old) = mpsc::unbounded_channel();
        assert!(!sender.attach(first, tx_old));
        let (tx_new, mut rx_new) = mpsc::unbounded_channel();
        assert!(sender.attach(second, tx_new));
        sender.detach(first);
        assert!(sender.live());
        sender.send(&op::typing(3));
        let Ok(Message::Text(sent)) = rx_new.try_recv() else {
            panic!("a operação não chegou à conexão atual");
        };
        assert!(sent.as_str().contains("\"typing\""));

        sender.end();
        assert!(!sender.live());
        assert!(!sender.is_current(second));
    }

    #[test]
    fn the_wait_grows_and_then_stops_growing() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), MAX_BACKOFF);
        assert_eq!(backoff(50), MAX_BACKOFF);
    }
}

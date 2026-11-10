//! The real-time gateway: one WebSocket per open app, carrying events from
//! the server and presence, typing and voice signalling from the app.
//!
//! The object may be put to sleep between messages while the sockets stay
//! open, so each connection keeps its own data (see [`Sock`]) and the lists
//! of who is online or in a call are read back from the sockets.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use worker::{Request, Response, Result, WebSocket, WebSocketPair};

use crate::db::RowExt;
use crate::model::{P_CONNECT, P_SEND, event};
use crate::params;
use crate::routes::{Req, send_message};
use crate::state::{Sock, State};
use crate::util::{
    ApiError, ApiResult, INTERNAL, fail, hash_token, now, random_bytes, str, to_int, truthy,
};

/// A connection that never identifies is closed after this long.
const IDENTIFY_WAIT: i64 = 10_000;

/// An app sends its heartbeat every 5 s (the app's gateway.rs). One not
/// heard from for this long is gone, not slow: a PC that lost power or its
/// network, or shut down with the app open, says no goodbye, and the
/// runtime can keep listing its socket for a long time, its owner online
/// and in their call all along.
const SILENT: i64 = 25_000;

/// How often the connections are checked for apps gone silent: on the
/// alarm while anyone is in a call, and whenever the object is awake
/// anyway.
const SWEEP_EVERY: i64 = 10_000;

/// How often a connection notes that something of it arrived (`Sock::seen`).
const SEEN_EVERY: i64 = 10_000;

/// Largest frame the gateway reads, as in the earlier servers.
const MAX_FRAME: usize = 256 * 1024;

pub const PONG: &str = r#"{"t":"PONG"}"#;
const AUTH_FAIL: &str = r#"{"t":"AUTH_FAIL"}"#;
const VOICE_KICK: &str = r#"{"t":"VOICE_KICK"}"#;

/// The client's heartbeat, which the runtime answers on its own so the
/// object can stay asleep.
pub const PING: &str = r#"{"op":"ping"}"#;

/// A connection number. Only 48 bits, because what a connection knows about
/// itself travels through JavaScript, which loses whole numbers past 2^53.
fn new_id() -> u64 {
    let mut b = [0u8; 8];
    b[..6].copy_from_slice(&random_bytes::<6>());
    u64::from_le_bytes(b)
}

/// Accepts a new connection and hands the other end to the browser.
pub fn upgrade(st: &State, _req: &Request) -> Result<Response> {
    let pair = WebSocketPair::new()?;
    st.do_state().accept_web_socket(&pair.server);
    let data = Sock {
        id: new_id(),
        opened: now(),
        ..Default::default()
    };
    pair.server.serialize_attachment(&data)?;
    st.refresh();
    Response::from_websocket(pair.client)
}

/// Runs one gateway message; a failure becomes an ERROR event. Returns when
/// the object should wake up (someone joined a call: `next_wake`).
pub fn message(st: &mut State, ws: &WebSocket, raw: &[u8]) -> Option<i64> {
    // Noted first: a connection talking right now is never taken for one
    // gone silent.
    let mut me = ws.deserialize_attachment::<Sock>().ok().flatten()?;
    let t = now();
    if t - me.seen >= SEEN_EVERY {
        me.seen = t;
        st.save_sock(me.id, me.clone());
    }
    drop_silent(st);
    sweep_sometimes(st);
    // The earlier servers closed a connection that sent a frame this big.
    if raw.len() > MAX_FRAME {
        let _ = ws.close(Some(1009), Some("mensagem grande demais"));
        return None;
    }
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(raw) else {
        return None;
    };
    let joining = str(m.get("op")) == "voice_join";
    if let Err(ApiError { msg, .. }) = dispatch(st, ws, &me, &m) {
        let _ = ws.send_with_str(event("ERROR", &json!({ "message": msg })));
    }
    if joining { next_wake(st) } else { None }
}

/// Closes connections that never said who they are. Called whenever the
/// object is awake anyway: on a gateway message and on each request.
pub fn drop_silent(st: &State) {
    let t = now();
    let stale: Vec<u64> = st
        .conns_of(0)
        .into_iter()
        .filter(|s| t - s.opened > IDENTIFY_WAIT)
        .map(|s| s.id)
        .collect();
    for id in stale {
        st.close_sock(id, None);
    }
}

/// Takes out the connections of apps not heard from for SILENT: as if they
/// had closed (`closed`), but with no place held in their call, which an
/// app still running would have taken back by now on a new connection (it
/// gives up on a silent one in 15 s). Returns the calls they were in and
/// whose they were, for `tell`.
fn sweep(st: &State) -> (Vec<i64>, Vec<i64>) {
    let t = now();
    st.last_sweep.set(t);
    let (mut calls, mut people) = (Vec::new(), Vec::new());
    for (ws, s) in st.silent_socks(t - SILENT) {
        // Gone from here on, whatever the runtime does with the socket
        // later (its close then finds nobody: `closed`).
        let _ = ws.serialize_attachment(&Sock {
            id: s.id,
            opened: s.opened,
            ..Default::default()
        });
        st.forget_sock(s.id);
        let _ = ws.close(Some(1000), Some("sem sinal"));
        if s.voice != 0 {
            calls.push(s.voice);
        }
        people.push(s.uid);
    }
    (calls, people)
}

/// Tells each call whose people changed who is in it now, and the contacts
/// of those of `people` no longer online.
fn tell(st: &State, mut calls: Vec<i64>, mut people: Vec<i64>) {
    calls.sort_unstable();
    calls.dedup();
    for cid in calls {
        log_err(st.announce_voice(cid));
    }
    people.sort_unstable();
    people.dedup();
    for uid in people {
        if !st.online(uid) {
            log_err(st.emit_presence(uid));
        }
    }
}

/// `sweep` at most every SWEEP_EVERY, from wherever the object is awake
/// anyway: on a gateway message and on each request.
pub fn sweep_sometimes(st: &State) {
    if now() - st.last_sweep.get() >= SWEEP_EVERY {
        let (calls, people) = sweep(st);
        tell(st, calls, people);
    }
}

/// When the object should wake up next: when the next held place runs out,
/// and while anyone is in a call, to look for apps gone silent again (no
/// message may come to wake it otherwise).
pub fn next_wake(st: &State) -> Option<i64> {
    let check = st.anyone_in_voice().then(|| now() + SWEEP_EVERY);
    [st.next_hold_expiry(), check].into_iter().flatten().min()
}

fn dispatch(st: &mut State, ws: &WebSocket, me: &Sock, m: &Map<String, Value>) -> ApiResult<()> {
    let op = str(m.get("op"));
    if me.uid == 0 {
        if op == "identify" {
            let in_call = m.get("inCall").and_then(Value::as_bool);
            identify(st, ws, me, &str(m.get("token")), in_call)?;
        }
        return Ok(());
    }
    let uid = me.uid;
    match op.as_str() {
        // Normally answered by the runtime without waking us up.
        "ping" => {
            let _ = ws.send_with_str(PONG);
        }
        // The same as POST /channels/{cid}/messages without the hop through
        // the Worker (about 15 ms from Brazil). The MESSAGE_CREATE that goes
        // to everyone, this connection too, is the answer; a refusal goes
        // back to this connection alone.
        "message" => {
            let mut q = Req {
                body: m.clone(),
                params: HashMap::from([(
                    "cid".to_owned(),
                    to_int(m.get("channelId"))?.to_string(),
                )]),
                query: Vec::new(),
                uid,
                token_hash: me.token_hash.clone(),
                // Only the test route looks at it, and this is a message.
                test: false,
                remove: Vec::new(),
            };
            if let Err(e) = send_message(st, &mut q) {
                let failed = json!({ "nonce": str(m.get("nonce")), "message": e.msg });
                let _ = ws.send_with_str(event("SEND_FAILED", &failed));
            }
        }
        "typing" => {
            if !st.hit(&format!("typing:{uid}"), 30, 60_000) {
                return Ok(());
            }
            let a = st.access(uid, to_int(m.get("channelId"))?)?;
            if a.perms & P_SEND != 0 {
                st.emit_channel(
                    &a.c,
                    "TYPING",
                    &json!({ "channelId": a.c.val("id"), "userId": uid }),
                    uid,
                )?;
            }
        }
        "status" => {
            let picked = str(m.get("status"));
            if !matches!(picked.as_str(), "online" | "idle" | "dnd" | "invisible") {
                return Ok(());
            }
            st.set_picked_status(uid, &picked)?;
            st.send(uid, "STATUS", &json!({ "status": picked }));
            st.emit_presence(uid)?;
        }
        "voice_join" => {
            let a = st.access(uid, to_int(m.get("channelId"))?)?;
            if a.c.str("type") != "voice" {
                return fail(400, "Esse não é um canal de voz");
            }
            if a.perms & P_CONNECT == 0 {
                return fail(403, "Você não pode entrar neste canal de voz");
            }
            let new_cid = a.c.int("id");
            // A voice session the user had before: another device the user
            // left behind, the same device whose gateway blinked before the
            // server noticed, or a place held since its connection closed
            // (VOICE_HOLD). For the same channel, move the voice over
            // silently (same place in the arrival order, no leave event in
            // between) so peers never see this user bounce from a
            // transient drop; for a different channel, tell the old one its
            // membership shrank.
            let (keep_joined, moved_from, resumed) = match st.voice_conn_of(uid) {
                Some(old) => {
                    if old.id != me.id {
                        st.send_to(&[old.id], VOICE_KICK);
                    }
                    // Clears the old sock's voice attribute without
                    // telling the audience: a VOICE_STATE for the new
                    // state goes out below.
                    let old_cid = old.voice;
                    let old_id = old.id;
                    let old_joined = old.joined;
                    let mut cleared = old;
                    cleared.voice = 0;
                    cleared.video = None;
                    st.save_sock(old_id, cleared);
                    if old_cid == new_cid {
                        (Some(old_joined), None, false)
                    } else {
                        (None, Some(old_cid), false)
                    }
                }
                None => match st.take_hold(uid) {
                    Some(held) if held.voice == new_cid => (Some(held.joined), None, true),
                    Some(held) => {
                        st.drop_mail(uid);
                        (None, Some(held.voice), false)
                    }
                    None => (None, None, false),
                },
            };
            let mut mine = st.sock(me.id).unwrap_or_else(|| me.clone());
            mine.voice = new_cid;
            mine.joined = keep_joined.unwrap_or_else(now);
            set_flags(&mut mine, m);
            st.save_sock(mine.id, mine);
            if let Some(old_cid) = moved_from {
                st.announce_voice(old_cid)?;
            }
            st.emit_voice(&a.c)?;
            // Back in the place held for them: what the others signalled
            // to them meanwhile, in order, as if they had never gone.
            if resumed {
                for frame in st.take_mail(uid) {
                    st.send_to(&[me.id], &frame);
                }
            }
        }
        "voice_state" => {
            let Some(mut mine) = st.voice_conn_of(uid).filter(|s| s.id == me.id) else {
                return Ok(());
            };
            let cid = mine.voice;
            set_flags(&mut mine, m);
            st.save_sock(mine.id, mine);
            let ch = st
                .db
                .get("SELECT * FROM channels WHERE id = ?", params![cid])?
                .unwrap_or_default();
            st.emit_voice(&ch)?;
        }
        "voice_leave" => {
            if st.voice_conn_of(uid).is_some_and(|s| s.id == me.id) {
                st.voice_leave(uid)?;
            } else {
                // The app is in its call through a connection of it that
                // is gone (or a place held for one): it left, so that goes.
                let calls = st.end_session_voice(uid, &me.token_hash, me.id);
                tell(st, calls, Vec::new());
            }
        }
        "signal" => {
            let to = to_int(m.get("to"))?;
            if !st.voice_conn_of(uid).is_some_and(|s| s.id == me.id) {
                return Ok(());
            }
            let mine = st.voice_channel_of(uid);
            let data = m.get("data").cloned().unwrap_or(Value::Null);
            let frame = event("SIGNAL", &json!({ "from": uid, "data": data }));
            match st.voice_conn_of(to) {
                Some(peer) if peer.voice == mine => st.send_to(&[peer.id], &frame),
                Some(_) => {}
                // Their connection dropped and their place is held: it
                // waits for them (an offer lost here would leave the two
                // connections stuck halfway through a change).
                None => {
                    if st.held(to).is_some_and(|h| h.voice == mine) {
                        st.queue_mail(to, &frame)?;
                    }
                }
            }
        }
        // The signalling of a file sent directly, outside calls: only
        // between its message's author and someone who may read it
        // (direct.rs).
        "direct" => crate::direct::relay(st, uid, m)?,
        // A new PC of this account asking an old one for the keys, and the
        // answer: only between this account's own connections (e2e.rs).
        "provision" => crate::e2e::provision(st, me, m)?,
        _ => {}
    }
    Ok(())
}

fn set_flags(s: &mut Sock, m: &Map<String, Value>) {
    let video = str(m.get("video"));
    s.mute = truthy(m.get("mute"));
    s.deaf = truthy(m.get("deaf"));
    s.video = (video == "camera" || video == "screen").then_some(video);
}

/// `in_call`: whether the app is in a call it will take up again (its
/// voice_join after READY), as apps from 0.14.2 on say; None from older
/// ones.
fn identify(
    st: &mut State,
    ws: &WebSocket,
    me: &Sock,
    token: &str,
    in_call: Option<bool>,
) -> ApiResult<()> {
    let uid = st.user_for_token(token)?;
    if uid == 0 {
        let _ = ws.send_with_str(AUTH_FAIL);
        st.close_sock(me.id, None);
        return Ok(());
    }
    let first = !st.online(uid);
    let mut mine = me.clone();
    mine.uid = uid;
    mine.token_hash = hash_token(token);
    let session = mine.token_hash.clone();
    st.save_sock(mine.id, mine);
    // Before READY, so it shows things as they are. The apps gone silent
    // are gone (a PC that shut down in a call, say), and an app back out of
    // a call (opened again after it closed without a goodbye) takes its own
    // place in it out: nobody sees it there any more, itself included. An
    // app still in its call keeps the place, for its voice_join to resume.
    let (mut calls, people) = sweep(st);
    if in_call == Some(false) {
        calls.extend(st.end_session_voice(uid, &session, me.id));
    }
    let ready = match st.ready(uid) {
        Ok(r) => r,
        Err(e) => {
            let msg = if e.status == 500 {
                INTERNAL.to_owned()
            } else {
                e.msg
            };
            let _ = ws.send_with_str(event("ERROR", &json!({ "message": msg })));
            return Ok(());
        }
    };
    let _ = ws.send_with_str(event("READY", &ready));
    tell(st, calls, people);
    if first {
        st.emit_presence(uid)?;
    }
    Ok(())
}

/// A connection went away: update presence, and hold its place in a call
/// for a few seconds (VOICE_HOLD) instead of taking it out at once. Returns
/// when that place goes, for the object to wake up then (`alarm`).
pub fn closed(st: &mut State, ws: &WebSocket) -> Option<i64> {
    let me = ws.deserialize_attachment::<Sock>().ok().flatten()?;
    st.refresh();
    // The runtime still lists a socket that is closing, for a while, and
    // later messages read it back from there: from here on it counts as
    // gone, with no user and no call (a signal for them would go to it and
    // be lost, instead of waiting in their held place).
    let _ = ws.serialize_attachment(&Sock {
        id: me.id,
        opened: me.opened,
        ..Default::default()
    });
    st.forget_sock(me.id);
    if me.uid == 0 {
        return None;
    }
    // The call goes on without the gateway, peer to peer: the others keep
    // seeing them (no VOICE_STATE), and an app that reconnects in time
    // takes the place again with its voice_join.
    let mut expires = None;
    if me.voice != 0 {
        match st.hold_voice(&me) {
            Ok(at) => expires = Some(at),
            Err(_) => log_err(st.announce_voice(me.voice)),
        }
    }
    if !st.online(me.uid) {
        log_err(st.emit_presence(me.uid));
    }
    expires
}

/// The object woke for the places held in calls: those whose time ran out
/// go, and their calls are told. Returns when the next one runs out.
pub fn alarm(st: &mut State) -> Option<i64> {
    st.refresh();
    let held: Vec<i64> = st.expire_holds().into_iter().map(|h| h.voice).collect();
    let (calls, people) = sweep(st);
    tell(st, held.into_iter().chain(calls).collect(), people);
    next_wake(st)
}

fn log_err(r: ApiResult<()>) {
    if let Err(e) = r
        && e.status == 500
    {
        worker::console_error!("gateway: {}", e.msg);
    }
}

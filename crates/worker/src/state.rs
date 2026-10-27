//! Everything requests and gateway messages touch.
//!
//! The single-process servers kept the open connections, who is online and
//! who is in a call in memory. Here the object can be put to sleep between
//! messages while the WebSockets stay open, so each connection carries its
//! own data (see [`Sock`]) and those lists are read back from the sockets.
//! The place in a call of someone whose connection just closed is kept a
//! few seconds longer (`VOICE_HOLD`), in the database: there is no socket
//! to keep it on.

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Env, State as DoState, WebSocket};

use crate::db::{Db, RowExt};
use crate::params;
use crate::util::{ApiResult, fail, hash_token, now, random_hex};

/// Sessions unused for 30 days expire.
const SESSION_IDLE: i64 = 30 * 24 * 3_600_000;
const HOUR: i64 = 3_600_000;

/// Rate-limit windows shorter than this live in memory: losing them when the
/// object sleeps costs nothing, because sleeping means nobody is knocking.
/// The longer ones (new servers, invites) go to the database.
const KEEP_WINDOW: i64 = 60_000;

/// How long a call keeps the place of someone whose gateway connection
/// closed. The call itself goes on without the gateway (its sound and video
/// go straight between the people), so an app that is back within this
/// takes the place again as if it had never left, and nobody hangs up on
/// it. Its first attempt to reconnect comes a second after the drop, the
/// next ones 2, 4 and 8 seconds after that.
pub const VOICE_HOLD: i64 = 15_000;

/// The most signalling kept for someone whose place is held (a change in a
/// call is an offer, an answer and a few candidates).
const HELD_MAIL: i64 = 64;

/// One rate-limit window.
pub struct Bucket {
    pub start: i64,
    pub n: u32,
}

/// A file to take out of R2 once the request is answered: handlers do not
/// wait on the network while they hold the state.
pub struct Trash {
    /// The stored name ("<32 hex>.ext").
    pub name: String,
    /// R2's id of an unfinished multipart upload, "" otherwise.
    pub upload: String,
    /// Whether the file is whole (deleted) or still in parts (aborted).
    pub done: bool,
}

/// What one gateway connection knows about itself, stored on the socket so
/// it survives the object being put to sleep.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Sock {
    /// Tells connections of the same user apart.
    pub id: u64,
    /// 0 until the connection identifies.
    pub uid: i64,
    pub token_hash: String,
    /// The voice channel this connection is in (0 for none).
    pub voice: i64,
    /// When it joined the call, so the list keeps the order people arrived.
    pub joined: i64,
    pub mute: bool,
    pub deaf: bool,
    pub video: Option<String>,
    /// When the connection arrived, to drop the ones that never identify.
    pub opened: i64,
}

pub struct State {
    pub db: Db,
    pub env: Env,
    ctx: DoState,
    /// Short rate-limit windows.
    pub lim: HashMap<String, Bucket>,
    /// The open connections, read from the sockets once per message.
    socks: RefCell<Option<Vec<(WebSocket, Sock)>>>,
    /// The places held in calls (see `VOICE_HOLD`), read from the database
    /// once per message too.
    holds: RefCell<Option<Vec<Sock>>>,
    /// Uploads the sweep found abandoned, removed after the request.
    pub trash: Vec<Trash>,
}

impl State {
    pub fn new(ctx: DoState, env: Env) -> State {
        State {
            db: Db::new(ctx.storage()),
            env,
            ctx,
            lim: HashMap::new(),
            socks: RefCell::new(None),
            holds: RefCell::new(None),
            trash: Vec::new(),
        }
    }

    pub fn do_state(&self) -> &DoState {
        &self.ctx
    }

    // ------------------------------------------------------------ connections

    /// Forgets the cached connections, at the start of each message.
    pub fn refresh(&self) {
        *self.socks.borrow_mut() = None;
        *self.holds.borrow_mut() = None;
    }

    /// Runs `f` over the open connections. The borrow is short on purpose:
    /// `f` must not call anything that reads them again.
    fn with_socks<T>(&self, f: impl FnOnce(&mut Vec<(WebSocket, Sock)>) -> T) -> T {
        let mut slot = self.socks.borrow_mut();
        let socks = slot.get_or_insert_with(|| {
            self.ctx
                .get_websockets()
                .into_iter()
                .map(|ws| {
                    let data = ws
                        .deserialize_attachment()
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    (ws, data)
                })
                .collect()
        });
        f(socks)
    }

    /// Writes a connection's data back to its socket.
    pub fn save_sock(&self, id: u64, data: Sock) {
        self.with_socks(|socks| {
            if let Some(slot) = socks.iter_mut().find(|(_, s)| s.id == id) {
                let _ = slot.0.serialize_attachment(&data);
                slot.1 = data;
            }
        });
    }

    pub fn sock(&self, id: u64) -> Option<Sock> {
        self.with_socks(|socks| {
            socks
                .iter()
                .find(|(_, s)| s.id == id)
                .map(|(_, s)| s.clone())
        })
    }

    /// Every connection of a user, identified or not.
    pub fn conns_of(&self, uid: i64) -> Vec<Sock> {
        self.with_socks(|socks| {
            socks
                .iter()
                .filter(|(_, s)| s.uid == uid)
                .map(|(_, s)| s.clone())
                .collect()
        })
    }

    pub fn online(&self, uid: i64) -> bool {
        self.with_socks(|socks| socks.iter().any(|(_, s)| s.uid == uid))
    }

    /// Drops a connection from the list. A socket that is closing is often
    /// still reported by the runtime, and it should count as gone.
    pub fn forget_sock(&self, id: u64) {
        self.with_socks(|socks| socks.retain(|(_, s)| s.id != id));
    }

    /// Who is in one voice channel, in the order people joined: their
    /// connections, and the places held for those whose connection dropped
    /// (with no socket behind them: id 0).
    pub fn voice_socks(&self, cid: i64) -> Vec<Sock> {
        if cid == 0 {
            return Vec::new();
        }
        let mut out: Vec<Sock> = self.with_socks(|socks| {
            socks
                .iter()
                .filter(|(_, s)| s.voice == cid && s.uid != 0)
                .map(|(_, s)| s.clone())
                .collect()
        });
        let held: Vec<Sock> =
            self.with_holds(|holds| holds.iter().filter(|h| h.voice == cid).cloned().collect());
        for h in held {
            if !out.iter().any(|s| s.uid == h.uid) {
                out.push(h);
            }
        }
        out.sort_by_key(|s| (s.joined, s.id));
        out
    }

    /// The connection a user is in a call through.
    pub fn voice_conn_of(&self, uid: i64) -> Option<Sock> {
        self.with_socks(|socks| {
            socks
                .iter()
                .find(|(_, s)| s.uid == uid && s.voice != 0)
                .map(|(_, s)| s.clone())
        })
    }

    /// Sends one already encoded event to a set of connections.
    pub fn send_to(&self, ids: &[u64], msg: &str) {
        self.with_socks(|socks| {
            for (ws, s) in socks.iter() {
                if ids.contains(&s.id) {
                    let _ = ws.send_with_str(msg);
                }
            }
        });
    }

    // ------------------------------------------------------------ held places

    /// Runs `f` over the places held now (see `VOICE_HOLD`), as connections
    /// with no socket behind them. The same short borrow as `with_socks`.
    fn with_holds<T>(&self, f: impl FnOnce(&[Sock]) -> T) -> T {
        let mut slot = self.holds.borrow_mut();
        let holds = slot.get_or_insert_with(|| {
            self.db
                .all(
                    "SELECT user_id, channel_id, joined, mute, deaf, video FROM voice_holds
                     WHERE expires > ?",
                    params![now()],
                )
                .unwrap_or_default()
                .iter()
                .map(|r| Sock {
                    uid: r.int("user_id"),
                    voice: r.int("channel_id"),
                    joined: r.int("joined"),
                    mute: r.int("mute") != 0,
                    deaf: r.int("deaf") != 0,
                    video: Some(r.str("video").to_owned()).filter(|v| !v.is_empty()),
                    ..Default::default()
                })
                .collect()
        });
        f(holds)
    }

    /// Keeps the place of `s`, a connection in a call that just closed, for
    /// VOICE_HOLD. Returns when the place goes, unless it is taken first.
    pub fn hold_voice(&self, s: &Sock) -> ApiResult<i64> {
        let expires = now() + VOICE_HOLD;
        self.drop_mail(s.uid);
        self.db.run(
            "INSERT INTO voice_holds(user_id, channel_id, joined, mute, deaf, video, expires)
             VALUES(?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET channel_id = excluded.channel_id,
               joined = excluded.joined, mute = excluded.mute, deaf = excluded.deaf,
               video = excluded.video, expires = excluded.expires",
            params![s.uid, s.voice, s.joined, s.mute, s.deaf, s.video, expires],
        )?;
        *self.holds.borrow_mut() = None;
        Ok(expires)
    }

    /// The place held for `uid`, if there is one.
    pub fn held(&self, uid: i64) -> Option<Sock> {
        self.with_holds(|holds| holds.iter().find(|h| h.uid == uid).cloned())
    }

    /// Gives up the place held for `uid` (they came back, or left for good)
    /// and returns it. What waited for them stays, for the caller to take.
    pub fn take_hold(&self, uid: i64) -> Option<Sock> {
        let held = self.held(uid)?;
        let _ = self
            .db
            .run("DELETE FROM voice_holds WHERE user_id = ?", params![uid]);
        *self.holds.borrow_mut() = None;
        Some(held)
    }

    /// Gives up the places whose time ran out (and what waited for them),
    /// and returns them.
    pub fn expire_holds(&self) -> Vec<Sock> {
        let t = now();
        let gone: Vec<Sock> = self
            .db
            .all(
                "SELECT user_id, channel_id FROM voice_holds WHERE expires <= ?",
                params![t],
            )
            .unwrap_or_default()
            .iter()
            .map(|r| Sock {
                uid: r.int("user_id"),
                voice: r.int("channel_id"),
                ..Default::default()
            })
            .collect();
        for h in &gone {
            let _ = self.db.run(
                "DELETE FROM voice_holds WHERE user_id = ? AND expires <= ?",
                params![h.uid, t],
            );
            self.drop_mail(h.uid);
        }
        *self.holds.borrow_mut() = None;
        gone
    }

    /// When the next place held runs out.
    pub fn next_hold_expiry(&self) -> Option<i64> {
        self.db
            .get("SELECT MIN(expires) AS t FROM voice_holds", params![])
            .ok()
            .flatten()
            .map(|r| r.int("t"))
            .filter(|t| *t > 0)
    }

    /// Keeps a SIGNAL event for `uid`, whose place is held, to hand to
    /// their connection when they are back, in the order it came.
    pub fn queue_mail(&self, uid: i64, frame: &str) -> ApiResult<()> {
        let kept = self
            .db
            .get(
                "SELECT COUNT(*) AS n FROM voice_mail WHERE user_id = ?",
                params![uid],
            )?
            .map_or(0, |r| r.int("n"));
        if kept < HELD_MAIL {
            self.db.run(
                "INSERT INTO voice_mail(user_id, frame) VALUES(?, ?)",
                params![uid, frame],
            )?;
        }
        Ok(())
    }

    /// What waited for `uid`, oldest first. Taken: it is not kept after.
    pub fn take_mail(&self, uid: i64) -> Vec<String> {
        let frames = self
            .db
            .all(
                "SELECT frame FROM voice_mail WHERE user_id = ? ORDER BY id",
                params![uid],
            )
            .unwrap_or_default()
            .iter()
            .map(|r| r.str("frame").to_owned())
            .collect();
        self.drop_mail(uid);
        frames
    }

    pub fn drop_mail(&self, uid: i64) {
        let _ = self
            .db
            .run("DELETE FROM voice_mail WHERE user_id = ?", params![uid]);
    }

    /// Closes a connection, after flushing `last` if there is one.
    pub fn close_sock(&self, id: u64, last: Option<&str>) {
        self.with_socks(|socks| {
            if let Some((ws, _)) = socks.iter().find(|(_, s)| s.id == id) {
                if let Some(msg) = last {
                    let _ = ws.send_with_str(msg);
                }
                let _ = ws.close(Some(1000), Some("encerrado"));
            }
        });
    }

    // ------------------------------------------------------------ rate limits

    pub fn hit(&mut self, key: &str, max: u32, window_ms: i64) -> bool {
        let t = now();
        if window_ms > KEEP_WINDOW {
            return self.hit_saved(key, max, window_ms, t).unwrap_or(true);
        }
        let b = self
            .lim
            .entry(key.to_owned())
            .or_insert(Bucket { start: t, n: 0 });
        if t - b.start > window_ms {
            *b = Bucket { start: t, n: 0 };
        }
        b.n += 1;
        b.n <= max
    }

    /// The same count, kept in the database. A failure here must not lock
    /// anyone out, so it counts as allowed.
    fn hit_saved(&mut self, key: &str, max: u32, window_ms: i64, t: i64) -> ApiResult<bool> {
        let row = self
            .db
            .get("SELECT start, n FROM limits WHERE k = ?", params![key])?;
        let (start, n) = match row {
            Some(r) if t - r.int("start") <= window_ms => (r.int("start"), r.int("n")),
            _ => (t, 0),
        };
        let n = n + 1;
        self.db.run(
            "INSERT INTO limits(k, start, n) VALUES(?, ?, ?)
             ON CONFLICT(k) DO UPDATE SET start = excluded.start, n = excluded.n",
            params![key, start, n],
        )?;
        Ok(n <= i64::from(max))
    }

    pub fn limit(&mut self, key: &str, max: u32, window_ms: i64) -> ApiResult<()> {
        self.limit_msg(key, max, window_ms, crate::routes::TOO_MANY)
    }

    pub fn limit_msg(&mut self, key: &str, max: u32, window_ms: i64, msg: &str) -> ApiResult<()> {
        if !self.hit(key, max, window_ms) {
            return fail(429, msg);
        }
        Ok(())
    }

    /// Cleans up once in a while, instead of the janitor the earlier
    /// servers ran on a timer.
    pub fn sweep_sometimes(&mut self) {
        // One request in about 500.
        if u16::from_le_bytes(crate::util::random_bytes::<2>()) < 131 {
            self.sweep();
        }
    }

    /// Drops rate-limit windows nobody is using any more, the marks of
    /// messages sent long ago, and uploads nobody put on a message (a
    /// cancelled send, an app closed halfway): a finished one gets a day to
    /// be sent, an unfinished one as long as its ticket lasts.
    pub fn sweep(&mut self) {
        let t = now();
        self.lim.retain(|_, b| t - b.start <= HOUR);
        let _ = self
            .db
            .run("DELETE FROM limits WHERE ? - start > ?", params![t, HOUR]);
        // A message sent again comes within seconds; a day is plenty.
        let _ = self
            .db
            .run("DELETE FROM sent WHERE created < ?", params![t - 24 * HOUR]);
        let stale = self.db.all(
            "SELECT name, upload_id, done FROM uploads WHERE used = 0
               AND ((done = 1 AND created < ?) OR (done = 0 AND created < ?)) LIMIT 50",
            params![t - 24 * HOUR, t - 7 * 24 * HOUR],
        );
        for r in stale.unwrap_or_default() {
            self.trash.push(Trash {
                name: r.str("name").to_owned(),
                upload: r.str("upload_id").to_owned(),
                done: r.int("done") == 1,
            });
        }
        // What fails here is tried again by the next sweep.
        let _ = self.sweep_expired(t);
    }

    /// The messages of conversations whose messages disappear, once their
    /// time is up: gone, with their files (whose uploads the sweep above
    /// deletes a day later, as unused ones), their reactions, and the
    /// sealed session keys of those conversations a few days old. The
    /// newest message of all is emptied instead, so its id is never given
    /// again, and goes in a later sweep, once another is newer.
    fn sweep_expired(&mut self, t: i64) -> ApiResult<()> {
        let max = self
            .db
            .get("SELECT COALESCE(MAX(id), 0) AS n FROM messages", params![])?
            .map_or(0, |r| r.int("n"));
        let gone = self.db.all(
            "SELECT id, channel_id, attachments, (envelope IS NULL AND content = '') AS emptied
               FROM messages WHERE gone_at IS NOT NULL AND gone_at <= ?
              AND (id != ? OR envelope IS NOT NULL OR content != '') LIMIT 200",
            params![t, max],
        )?;
        for m in &gone {
            // Emptied by an earlier sweep (and announced then).
            if m.int("emptied") == 1 {
                self.db
                    .run("DELETE FROM messages WHERE id = ?", params![m.int("id")])?;
                continue;
            }
            if let Ok(Value::Array(atts)) = serde_json::from_str::<Value>(m.str("attachments")) {
                for url in atts.iter().filter_map(|a| a.get("url")?.as_str()) {
                    if let Some(name) = url.strip_prefix("/uploads/") {
                        self.db.run(
                            "UPDATE uploads SET used = 0, created = ? WHERE name = ?",
                            params![t - 24 * HOUR, name],
                        )?;
                    }
                }
            }
            if m.int("id") == max {
                self.db.run(
                    "UPDATE messages SET content = '', attachments = '[]', envelope = NULL, sticker = NULL WHERE id = ?",
                    params![max],
                )?;
                self.db
                    .run("DELETE FROM reactions WHERE message_id = ?", params![max])?;
            } else {
                self.db
                    .run("DELETE FROM messages WHERE id = ?", params![m.int("id")])?;
            }
            if let Some(c) = self.db.get(
                "SELECT * FROM channels WHERE id = ?",
                params![m.int("channel_id")],
            )? {
                self.emit_channel(
                    &c,
                    "MESSAGE_DELETE",
                    &json!({ "id": m.int("id"), "channel_id": m.int("channel_id") }),
                    0,
                )?;
            }
        }
        // A session lasts a day in these conversations: two days past its
        // messages' time, nobody needs its key.
        self.db.run(
            "DELETE FROM e2e_sessions WHERE channel_id IN (SELECT id FROM channels WHERE ttl > 0)
              AND created < ? - (SELECT ttl FROM channels WHERE id = e2e_sessions.channel_id) * 1000 - 2 * 86400000",
            params![t],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ sessions

    pub fn new_session(&self, uid: i64) -> ApiResult<String> {
        let token = random_hex(32);
        self.db.run(
            "INSERT INTO sessions(token_hash, user_id, created, last_used) VALUES(?, ?, ?, ?)",
            params![hash_token(&token), uid, now(), now()],
        )?;
        Ok(token)
    }

    pub fn user_for_token(&self, token: &str) -> ApiResult<i64> {
        if token.is_empty() {
            return Ok(0);
        }
        let th = hash_token(token);
        let Some(row) = self.db.get(
            "SELECT user_id, last_used FROM sessions WHERE token_hash = ?",
            params![th],
        )?
        else {
            return Ok(0);
        };
        let t = now();
        if t - row.int("last_used") > SESSION_IDLE {
            self.db
                .run("DELETE FROM sessions WHERE token_hash = ?", params![th])?;
            return Ok(0);
        }
        if t - row.int("last_used") > HOUR {
            self.db.run(
                "UPDATE sessions SET last_used = ? WHERE token_hash = ?",
                params![t, th],
            )?;
        }
        Ok(row.int("user_id"))
    }

    /// Signs out a user's connections, all but the session `keep_hash`
    /// ("" for all of them, when the account is deleted).
    pub fn drop_sockets(&self, uid: i64, keep_hash: &str) {
        let targets: Vec<WebSocket> = self.with_socks(|socks| {
            socks
                .iter()
                .filter(|(_, s)| {
                    s.uid == uid && (keep_hash.is_empty() || s.token_hash != keep_hash)
                })
                .map(|(ws, _)| ws.clone())
                .collect()
        });
        for ws in targets {
            let _ = ws.send_with_str(r#"{"t":"AUTH_FAIL"}"#);
            let _ = ws.close(Some(1000), Some("sessão encerrada"));
        }
    }

    // ------------------------------------------------------------ settings

    /// The key upload tickets are signed with (files.rs), from the Worker's
    /// secrets. Without it no file could go up, so it must be set.
    pub fn pepper(&self) -> ApiResult<Vec<u8>> {
        match self.env.secret("PEPPER") {
            Ok(s) => Ok(s.to_string().into_bytes()),
            Err(_) => {
                worker::console_error!("falta o segredo PEPPER");
                fail(500, crate::util::INTERNAL)
            }
        }
    }

    pub fn var(&self, name: &str) -> String {
        self.env
            .var(name)
            .map(|v| v.to_string())
            .or_else(|_| self.env.secret(name).map(|v| v.to_string()))
            .unwrap_or_default()
    }
}

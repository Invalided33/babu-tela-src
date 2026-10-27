//! Permissions, snapshots, events, voice rooms and messages: the shared logic
//! behind the API routes and the gateway.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::db::{Row, RowExt, ids};
use crate::params;
use crate::state::State;
use crate::util::{ApiResult, extension, fail, truncate};

// ---------------------------------------------------------------- permissions

pub const P_VIEW: i64 = 1;
pub const P_SEND: i64 = 2;
pub const P_MANAGE_MESSAGES: i64 = 4;
pub const P_MANAGE_CHANNELS: i64 = 8;
pub const P_MANAGE_ROLES: i64 = 16;
pub const P_KICK: i64 = 32;
pub const P_BAN: i64 = 64;
pub const P_ADMIN: i64 = 128;
pub const P_INVITE: i64 = 256;
pub const P_ATTACH: i64 = 512;
pub const P_CONNECT: i64 = 1024;
pub const P_MANAGE_SERVER: i64 = 2048;

pub const P_ALL: i64 = 4095;
pub const DEFAULT_PERMS: i64 = P_VIEW | P_SEND | P_INVITE | P_ATTACH | P_CONNECT;
pub const CHANNEL_BITS: i64 = P_VIEW | P_SEND | P_ATTACH | P_CONNECT;

pub const COLORS: &[&str] = &[
    "#8590ff", "#a6b0ff", "#c58af9", "#6fa8f5", "#58c7d9", "#f27ba5", "#f5b95c", "#43d39e",
];

/// A channel the user can see, with its server (`None` for direct messages)
/// and the user's permissions in it.
pub struct Access {
    pub c: Row,
    pub srv: Option<Row>,
    pub perms: i64,
}

pub struct MessageAccess {
    pub m: Row,
    pub a: Access,
}

/// `{ "t": t, "d": d }`, the shape of every gateway event.
pub fn event(t: &str, d: &Value) -> String {
    format!("{{\"t\":{},\"d\":{}}}", Value::from(t), d)
}

/// The public part of a user (`null` for none).
pub fn pub_user(u: Option<&Row>) -> Value {
    match u {
        None => Value::Null,
        Some(u) => json!({
            "id": u.val("id"), "username": u.val("username"), "display_name": u.val("display_name"),
            "color": u.val("color"), "avatar": u.val("avatar"),
        }),
    }
}

fn copy(row: &Row) -> serde_json::Map<String, Value> {
    row.clone()
}

/// What only its owner sees of an account: the e-mail of the Google
/// account it signs in with.
pub fn account(u: Option<&Row>) -> Value {
    json!({ "email": u.map_or("", |u| u.str("email")) })
}

impl State {
    pub fn member_roles(&self, sid: i64, uid: i64) -> ApiResult<Vec<Row>> {
        self.db.all(
            "SELECT * FROM roles WHERE server_id = ? AND (is_everyone = 1 OR id IN
              (SELECT role_id FROM member_roles WHERE server_id = ? AND user_id = ?))",
            params![sid, sid, uid],
        )
    }

    /// A member's permissions in a server, or in one of its channels when
    /// `channel_id` is not 0: roles first, then the channel's @everyone
    /// override, then the overrides of the member's other roles.
    pub fn perms_for(&self, server: &Row, uid: i64, channel_id: i64) -> ApiResult<i64> {
        if server.int("owner_id") == uid {
            return Ok(P_ALL);
        }
        let roles = self.member_roles(server.int("id"), uid)?;
        let mut p = roles.iter().fold(0, |p, r| p | r.int("perms"));
        if p & P_ADMIN != 0 {
            return Ok(P_ALL);
        }
        if channel_id != 0 {
            let ovs = self.db.all(
                "SELECT * FROM overrides WHERE channel_id = ?",
                params![channel_id],
            )?;
            let mut everyone = 0;
            let mut mine = HashSet::new();
            for r in &roles {
                mine.insert(r.int("id"));
                if r.int("is_everyone") == 1 {
                    everyone = r.int("id");
                }
            }
            for o in ovs.iter().filter(|o| o.int("role_id") == everyone) {
                p = (p & !o.int("deny")) | o.int("allow");
            }
            let (mut allow, mut deny) = (0, 0);
            for o in ovs
                .iter()
                .filter(|o| o.int("role_id") != everyone && mine.contains(&o.int("role_id")))
            {
                allow |= o.int("allow");
                deny |= o.int("deny");
            }
            p = (p & !deny) | allow;
        }
        Ok(p)
    }

    pub fn server_for(&self, uid: i64, sid: i64) -> ApiResult<Row> {
        if let Some(srv) = self
            .db
            .get("SELECT * FROM servers WHERE id = ?", params![sid])?
            && self
                .db
                .get(
                    "SELECT 1 FROM members WHERE server_id = ? AND user_id = ?",
                    params![sid, uid],
                )?
                .is_some()
        {
            return Ok(srv);
        }
        fail(404, "Servidor não encontrado")
    }

    pub fn need(&self, server: &Row, uid: i64, bit: i64) -> ApiResult<()> {
        if self.perms_for(server, uid, 0)? & bit == 0 {
            return fail(403, "Você não tem permissão para isso");
        }
        Ok(())
    }

    /// The owner outranks everyone, and admins outrank members who are not admins.
    pub fn can_moderate(&self, server: &Row, actor: i64, target: i64) -> ApiResult<bool> {
        if target == server.int("owner_id") || actor == target {
            return Ok(false);
        }
        if actor == server.int("owner_id") {
            return Ok(true);
        }
        Ok(self.perms_for(server, target, 0)? != P_ALL)
    }

    pub fn access(&self, uid: i64, cid: i64) -> ApiResult<Access> {
        let Some(c) = self
            .db
            .get("SELECT * FROM channels WHERE id = ?", params![cid])?
        else {
            return fail(404, "Canal não encontrado");
        };
        if c.str("type") == "dm" {
            if self
                .db
                .get(
                    "SELECT 1 FROM dm_members WHERE channel_id = ? AND user_id = ?",
                    params![cid, uid],
                )?
                .is_none()
            {
                return fail(404, "Canal não encontrado");
            }
            return Ok(Access {
                c,
                srv: None,
                perms: P_VIEW | P_SEND | P_ATTACH,
            });
        }
        let srv = self.server_for(uid, c.int("server_id"))?;
        let perms = self.perms_for(&srv, uid, cid)?;
        if perms & P_VIEW == 0 {
            return fail(404, "Canal não encontrado");
        }
        Ok(Access {
            c,
            srv: Some(srv),
            perms,
        })
    }

    // ------------------------------------------------------------ users and presence

    pub fn user_by_id(&self, id: i64) -> ApiResult<Option<Row>> {
        self.db.get("SELECT * FROM users WHERE id = ?", params![id])
    }

    /// The status a user picked, "" when they picked nothing. Saved, so
    /// someone who chose "invisible" is still invisible after reopening the
    /// app, as it worked before.
    pub fn picked_status(&self, uid: i64) -> String {
        self.db
            .get(
                "SELECT status FROM presence WHERE user_id = ?",
                params![uid],
            )
            .ok()
            .flatten()
            .map_or_else(String::new, |r| r.str("status").to_owned())
    }

    pub fn set_picked_status(&self, uid: i64, status: &str) -> ApiResult<()> {
        self.db.run(
            "INSERT INTO presence(user_id, status) VALUES(?, ?)
             ON CONFLICT(user_id) DO UPDATE SET status = excluded.status",
            params![uid, status],
        )?;
        Ok(())
    }

    pub fn status_of(&self, uid: i64) -> String {
        if !self.online(uid) {
            return "offline".into();
        }
        match self.picked_status(uid).as_str() {
            "" => "online".into(),
            "invisible" => "offline".into(),
            st => st.into(),
        }
    }

    pub fn with_status(&self, u: Option<&Row>) -> Value {
        let mut o = pub_user(u);
        if let (Value::Object(m), Some(u)) = (&mut o, u) {
            m.insert("status".into(), self.status_of(u.int("id")).into());
        }
        o
    }

    /// Everyone who shares a server, a friendship or a direct conversation
    /// with `uid`: the people who see their presence.
    pub fn related(&self, uid: i64) -> ApiResult<Vec<i64>> {
        let rows = self.db.all(
            "SELECT m2.user_id AS id FROM members m1 JOIN members m2 ON m1.server_id = m2.server_id WHERE m1.user_id = ?
              UNION SELECT CASE WHEN a = ? THEN b ELSE a END FROM friends WHERE a = ? OR b = ?
              UNION SELECT d2.user_id FROM dm_members d1 JOIN dm_members d2 ON d1.channel_id = d2.channel_id WHERE d1.user_id = ?",
            params![uid, uid, uid, uid, uid],
        )?;
        Ok(ids(&rows, "id"))
    }

    // ------------------------------------------------------------ snapshots

    pub fn server_snapshot(&self, sid: i64, uid: i64) -> ApiResult<Value> {
        let srv = self
            .db
            .get("SELECT * FROM servers WHERE id = ?", params![sid])?
            .unwrap_or_default();
        let perms = self.perms_for(&srv, uid, 0)?;
        let mut members = Vec::new();
        for u in self.db.all(
            "SELECT u.*, m.certs AS member_certs FROM members m JOIN users u ON u.id = m.user_id WHERE m.server_id = ? ORDER BY u.display_name COLLATE NOCASE",
            params![sid],
        )? {
            let mut m = self.with_status(Some(&u));
            let roles = self.db.all("SELECT role_id FROM member_roles WHERE server_id = ? AND user_id = ?", params![sid, u.int("id")])?;
            m["roles"] = json!(ids(&roles, "role_id"));
            // Who let them in, signed (crates/app/src/e2e/members.rs).
            m["certs"] = crate::e2e::certs_of(u.str("member_certs"));
            members.push(m);
        }
        let mut voice = serde_json::Map::new();
        let mut channels = Vec::new();
        for c in self.db.all(
            "SELECT * FROM channels WHERE server_id = ? ORDER BY position, id",
            params![sid],
        )? {
            let cp = self.perms_for(&srv, uid, c.int("id"))?;
            if cp & P_VIEW == 0 {
                continue;
            }
            let mut ch = copy(&c);
            ch.insert("perms".into(), cp.into());
            let overrides = self.db.all(
                "SELECT role_id, allow, deny FROM overrides WHERE channel_id = ?",
                params![c.int("id")],
            )?;
            ch.insert("overrides".into(), json!(overrides));
            channels.push(Value::Object(ch));
            if c.str("type") == "voice" {
                voice.insert(
                    c.int("id").to_string(),
                    json!(self.voice_users(c.int("id"))),
                );
            }
        }
        let mut bans = Vec::new();
        if perms & P_BAN != 0 {
            for u in self.db.all(
                "SELECT u.* FROM bans b JOIN users u ON u.id = b.user_id WHERE b.server_id = ?",
                params![sid],
            )? {
                bans.push(pub_user(Some(&u)));
            }
        }
        let mut reports = Vec::new();
        if perms & P_MANAGE_MESSAGES != 0 {
            for r in self.db.all(
                "SELECT * FROM reports WHERE server_id = ? ORDER BY id DESC LIMIT 50",
                params![sid],
            )? {
                let m = self
                    .db
                    .get(
                        "SELECT * FROM messages WHERE id = ?",
                        params![r.int("message_id")],
                    )?
                    .unwrap_or_default();
                let channel = self
                    .db
                    .get(
                        "SELECT name FROM channels WHERE id = ?",
                        params![m.int("channel_id")],
                    )?
                    .map_or(Value::Null, |c| c.val("name"));
                reports.push(json!({
                    "id": r.val("id"), "reason": r.val("reason"), "created": r.val("created"),
                    "reporter": pub_user(self.user_by_id(r.int("reporter_id"))?.as_ref()),
                    "channel": channel, "message": self.serialize(&m)?,
                }));
            }
        }
        // Who asked to come in, for those who may let them (P_KICK).
        let mut requests = Vec::new();
        if perms & P_KICK != 0 {
            for r in self.db.all(
                "SELECT user_id, created FROM join_requests WHERE server_id = ? ORDER BY created, user_id",
                params![sid],
            )? {
                requests.push(json!({
                    "user": pub_user(self.user_by_id(r.int("user_id"))?.as_ref()),
                    "created": r.val("created"),
                }));
            }
        }
        let mut out = copy(&srv);
        out.insert("private".into(), (srv.int("private") != 0).into());
        out.insert("requests".into(), Value::Array(requests));
        out.insert("perms".into(), perms.into());
        let roles = self.db.all(
            "SELECT * FROM roles WHERE server_id = ? ORDER BY is_everyone, position DESC",
            params![sid],
        )?;
        out.insert("roles".into(), json!(roles));
        out.insert("members".into(), Value::Array(members));
        out.insert("channels".into(), Value::Array(channels));
        out.insert("voice".into(), Value::Object(voice));
        out.insert("bans".into(), Value::Array(bans));
        out.insert("reports".into(), Value::Array(reports));
        Ok(Value::Object(out))
    }

    pub fn sync_server(&self, sid: i64) -> ApiResult<()> {
        let rows = self.db.all(
            "SELECT user_id FROM members WHERE server_id = ?",
            params![sid],
        )?;
        for uid in ids(&rows, "user_id") {
            if self.online(uid) {
                self.send(uid, "SERVER_UPDATE", &self.server_snapshot(sid, uid)?);
            }
        }
        Ok(())
    }

    /// The new snapshot only to the members who see the join requests:
    /// for the others, a request changes nothing.
    pub fn sync_approvers(&self, sid: i64) -> ApiResult<()> {
        let Some(srv) = self
            .db
            .get("SELECT * FROM servers WHERE id = ?", params![sid])?
        else {
            return Ok(());
        };
        let rows = self.db.all(
            "SELECT user_id FROM members WHERE server_id = ?",
            params![sid],
        )?;
        for uid in ids(&rows, "user_id") {
            if self.online(uid) && self.perms_for(&srv, uid, 0)? & P_KICK != 0 {
                self.send(uid, "SERVER_UPDATE", &self.server_snapshot(sid, uid)?);
            }
        }
        Ok(())
    }

    /// The servers `uid` asked to join and is waiting on: each one's name,
    /// colour and picture (they cannot see the rest yet), and when.
    pub fn outgoing_requests(&self, uid: i64) -> ApiResult<Vec<Value>> {
        let mut out = Vec::new();
        for r in self.db.all(
            "SELECT s.id, s.name, s.color, s.icon, r.created FROM join_requests r
              JOIN servers s ON s.id = r.server_id WHERE r.user_id = ? ORDER BY r.created",
            params![uid],
        )? {
            out.push(json!({
                "server": {
                    "id": r.val("id"), "name": r.val("name"), "color": r.val("color"), "icon": r.val("icon"),
                },
                "created": r.val("created"),
            }));
        }
        Ok(out)
    }

    /// Tells `uid` which of their join requests are still waiting.
    pub fn emit_requests(&self, uid: i64) -> ApiResult<()> {
        let d = json!({ "requests": self.outgoing_requests(uid)? });
        self.send(uid, "JOIN_REQUESTS", &d);
        Ok(())
    }

    pub fn dm_snapshot(&self, cid: i64, uid: i64) -> ApiResult<Value> {
        let other = self.db.get(
            "SELECT u.* FROM dm_members d JOIN users u ON u.id = d.user_id WHERE d.channel_id = ? AND d.user_id != ?",
            params![cid, uid],
        )?;
        let ttl = self
            .db
            .get("SELECT ttl FROM channels WHERE id = ?", params![cid])?
            .map_or(0, |r| r.int("ttl"));
        Ok(json!({ "id": cid, "type": "dm", "user": self.with_status(other.as_ref()), "ttl": ttl }))
    }

    pub fn friend_list(&self, uid: i64) -> ApiResult<Vec<Value>> {
        let mut out = Vec::new();
        for f in self.db.all(
            "SELECT * FROM friends WHERE a = ? OR b = ?",
            params![uid, uid],
        )? {
            let other = if f.int("a") == uid {
                f.int("b")
            } else {
                f.int("a")
            };
            let state = if f.str("status") == "accepted" {
                "accepted"
            } else if f.int("requester") == uid {
                "outgoing"
            } else {
                "incoming"
            };
            out.push(json!({ "user": self.with_status(self.user_by_id(other)?.as_ref()), "state": state }));
        }
        Ok(out)
    }

    pub fn block_list(&self, uid: i64) -> ApiResult<Vec<i64>> {
        Ok(ids(
            &self.db.all(
                "SELECT blocked_id FROM blocks WHERE user_id = ?",
                params![uid],
            )?,
            "blocked_id",
        ))
    }

    pub fn blocked_between(&self, x: i64, y: i64) -> ApiResult<bool> {
        Ok(self
            .db
            .get(
                "SELECT 1 FROM blocks WHERE (user_id = ? AND blocked_id = ?) OR (user_id = ? AND blocked_id = ?)",
                params![x, y, y, x],
            )?
            .is_some())
    }

    pub fn ready(&self, uid: i64) -> ApiResult<Value> {
        let me = self.user_by_id(uid)?;
        let account = account(me.as_ref());
        let mut user = pub_user(me.as_ref());
        let Value::Object(fields) = &mut user else {
            return fail(500, crate::util::INTERNAL);
        };
        let picked = self.picked_status(uid);
        let st = if picked.is_empty() { "online" } else { &picked };
        fields.insert("status".into(), st.into());
        let mut servers = Vec::new();
        for sid in ids(
            &self.db.all(
                "SELECT server_id FROM members WHERE user_id = ? ORDER BY joined",
                params![uid],
            )?,
            "server_id",
        ) {
            servers.push(self.server_snapshot(sid, uid)?);
        }
        let mut dms = Vec::new();
        let rows = self.db.all(
            "SELECT d.channel_id FROM dm_members d WHERE d.user_id = ?
              ORDER BY (SELECT MAX(id) FROM messages WHERE channel_id = d.channel_id) DESC",
            params![uid],
        )?;
        for cid in ids(&rows, "channel_id") {
            dms.push(self.dm_snapshot(cid, uid)?);
        }
        Ok(json!({
            "user": user, "servers": servers, "dms": dms, "friends": self.friend_list(uid)?,
            "blocks": self.block_list(uid)?, "version": babutela_buildinfo::VERSION,
            "stickers": self.sticker_list(uid)?, "account": account,
            "requests": self.outgoing_requests(uid)?,
            // GIPHY wants its API called by the apps themselves, not through
            // a server, so the key goes to them; without one, the apps hide
            // the stickers.
            "giphy": self.var("GIPHY_API_KEY"),
        }))
    }

    // ------------------------------------------------------------ events

    /// Sends an event to every connection of a user.
    pub fn send(&self, uid: i64, t: &str, d: &Value) {
        let ids: Vec<u64> = self.conns_of(uid).into_iter().map(|s| s.id).collect();
        if ids.is_empty() {
            return;
        }
        self.send_to(&ids, &event(t, d));
    }

    /// Who should receive events about a channel right now.
    pub fn audience(&self, c: &Row) -> ApiResult<Vec<i64>> {
        if c.str("type") == "dm" {
            return Ok(ids(
                &self.db.all(
                    "SELECT user_id FROM dm_members WHERE channel_id = ?",
                    params![c.int("id")],
                )?,
                "user_id",
            ));
        }
        let srv = self
            .db
            .get(
                "SELECT * FROM servers WHERE id = ?",
                params![c.int("server_id")],
            )?
            .unwrap_or_default();
        let mut out = Vec::new();
        for uid in ids(
            &self.db.all(
                "SELECT user_id FROM members WHERE server_id = ?",
                params![c.int("server_id")],
            )?,
            "user_id",
        ) {
            if self.online(uid) && self.perms_for(&srv, uid, c.int("id"))? & P_VIEW != 0 {
                out.push(uid);
            }
        }
        Ok(out)
    }

    pub fn emit_channel(&self, c: &Row, t: &str, d: &Value, except: i64) -> ApiResult<()> {
        for uid in self.audience(c)? {
            if uid != except {
                self.send(uid, t, d);
            }
        }
        Ok(())
    }

    pub fn emit_presence(&self, uid: i64) -> ApiResult<()> {
        let d = json!({ "userId": uid, "status": self.status_of(uid) });
        for id in self.related(uid)? {
            if id != uid {
                self.send(id, "PRESENCE", &d);
            }
        }
        Ok(())
    }

    pub fn emit_friend(&self, uid: i64, other: i64) -> ApiResult<()> {
        for f in self.friend_list(uid)? {
            if f["user"]["id"].as_i64() == Some(other) {
                self.send(uid, "FRIEND_UPDATE", &f);
                return Ok(());
            }
        }
        self.send(uid, "FRIEND_REMOVE", &json!({ "userId": other }));
        Ok(())
    }

    // ------------------------------------------------------------ voice

    pub fn voice_users(&self, cid: i64) -> Vec<Value> {
        self.voice_socks(cid)
            .into_iter()
            .map(|s| json!({ "userId": s.uid, "mute": s.mute, "deaf": s.deaf, "video": s.video }))
            .collect()
    }

    pub fn voice_members(&self, cid: i64) -> Vec<i64> {
        self.voice_socks(cid).into_iter().map(|s| s.uid).collect()
    }

    pub fn emit_voice(&self, c: &Row) -> ApiResult<()> {
        let d = json!({ "serverId": c.val("server_id"), "channelId": c.val("id"), "users": self.voice_users(c.int("id")) });
        for uid in self.audience(c)? {
            self.send(uid, "VOICE_STATE", &d);
        }
        Ok(())
    }

    pub fn voice_leave(&self, uid: i64) -> ApiResult<()> {
        if let Some(mut sock) = self.voice_conn_of(uid) {
            let cid = sock.voice;
            sock.voice = 0;
            sock.video = None;
            self.save_sock(sock.id, sock);
            return self.announce_voice(cid);
        }
        // Their connection had dropped and their place was held: it goes.
        if let Some(held) = self.take_hold(uid) {
            self.drop_mail(uid);
            return self.announce_voice(held.voice);
        }
        Ok(())
    }

    /// Tells a voice channel who is in it now.
    pub fn announce_voice(&self, cid: i64) -> ApiResult<()> {
        if cid == 0 {
            return Ok(());
        }
        if let Some(ch) = self
            .db
            .get("SELECT * FROM channels WHERE id = ?", params![cid])?
        {
            self.emit_voice(&ch)?;
        }
        Ok(())
    }

    /// The voice channel `uid` is in, through its voice connection or the
    /// place held for it (0 for none).
    pub fn voice_channel_of(&self, uid: i64) -> i64 {
        self.voice_conn_of(uid)
            .or_else(|| self.held(uid))
            .map_or(0, |s| s.voice)
    }

    pub fn remove_member(&self, sid: i64, uid: i64) -> ApiResult<()> {
        let in_call = self.voice_channel_of(uid);
        if in_call != 0
            && self
                .db
                .get(
                    "SELECT 1 FROM channels WHERE id = ? AND server_id = ?",
                    params![in_call, sid],
                )?
                .is_some()
        {
            self.voice_leave(uid)?;
        }
        self.db.run(
            "DELETE FROM member_roles WHERE server_id = ? AND user_id = ?",
            params![sid, uid],
        )?;
        self.db.run(
            "DELETE FROM members WHERE server_id = ? AND user_id = ?",
            params![sid, uid],
        )?;
        self.send(uid, "SERVER_REMOVE", &json!({ "id": sid }));
        self.sync_server(sid)
    }

    // ------------------------------------------------------------ messages

    pub fn serialize(&self, m: &Row) -> ApiResult<Value> {
        let mut order: Vec<String> = Vec::new();
        let mut grouped: HashMap<String, Vec<i64>> = HashMap::new();
        // An encrypted message's reactions: each person's sealed emoji
        // goes with its mark, for the apps to open and check.
        let mut sealed: HashMap<String, Vec<Value>> = HashMap::new();
        for r in self.db.all(
            "SELECT emoji, user_id, data FROM reactions WHERE message_id = ? ORDER BY rowid",
            params![m.int("id")],
        )? {
            let e = r.str("emoji").to_owned();
            if !grouped.contains_key(&e) {
                order.push(e.clone());
            }
            grouped.entry(e.clone()).or_default().push(r.int("user_id"));
            if !r.str("data").is_empty() {
                sealed
                    .entry(e)
                    .or_default()
                    .push(json!([r.int("user_id"), r.str("data")]));
            }
        }
        let reactions: Vec<Value> = order
            .into_iter()
            .map(|e| {
                let data = sealed.remove(&e).unwrap_or_default();
                json!({ "users": grouped[&e], "emoji": e, "data": data })
            })
            .collect();
        let e2e = match m.str("envelope") {
            "" => Value::Null,
            s => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        };
        let mut reply = Value::Null;
        if m.int("reply_to") != 0
            && let Some(r) = self.db.get(
                "SELECT id, author_id, content, sticker FROM messages WHERE id = ?",
                params![m.int("reply_to")],
            )?
        {
            reply = json!({
                "id": r.val("id"), "author": pub_user(self.user_by_id(r.int("author_id"))?.as_ref()),
                "content": truncate(r.str("content"), 140), "sticker": sticker_of(&r),
            });
        }
        // Like Go's json.Unmarshal into a slice: a list stays a list, null
        // stays null and anything else leaves the list empty.
        let attachments = match m.str("attachments") {
            "" => json!([]),
            a => match serde_json::from_str::<Value>(a) {
                Ok(v @ (Value::Array(_) | Value::Null)) => v,
                _ => json!([]),
            },
        };
        Ok(json!({
            "id": m.val("id"), "channel_id": m.val("channel_id"),
            "author": pub_user(self.user_by_id(m.int("author_id"))?.as_ref()),
            "content": m.val("content"), "created": m.val("created"), "edited": m.val("edited"), "reply": reply,
            "attachments": attachments, "reactions": reactions, "sticker": sticker_of(m), "e2e": e2e,
        }))
    }

    /// The GIPHY ids of the stickers `uid` keeps as favourites, newest first.
    pub fn sticker_list(&self, uid: i64) -> ApiResult<Vec<String>> {
        Ok(self
            .db
            .all(
                "SELECT giphy_id FROM stickers WHERE user_id = ? ORDER BY added DESC, rowid DESC",
                params![uid],
            )?
            .iter()
            .map(|r| r.str("giphy_id").to_owned())
            .collect())
    }

    pub fn message_for(&self, uid: i64, mid: i64) -> ApiResult<MessageAccess> {
        let Some(m) = self
            .db
            .get("SELECT * FROM messages WHERE id = ?", params![mid])?
        else {
            return fail(404, "Mensagem não encontrada");
        };
        let a = self.access(uid, m.int("channel_id"))?;
        Ok(MessageAccess { m, a })
    }
}

/// A message row's sticker, as the API sends it: the GIPHY id, or null.
fn sticker_of(m: &Row) -> Value {
    match m.str("sticker") {
        "" => Value::Null,
        id => json!(id),
    }
}

/// What a GIPHY id looks like: letters and digits (as in "xT4uQulxzV39haRFjG").
pub fn valid_sticker_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------- uploads

/// Matches the names `upload` hands out: "/uploads/" plus 32 hex characters
/// and an optional short extension.
pub fn is_upload_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("/uploads/") else {
        return false;
    };
    let (name, ext) = match rest.find('.') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    if name.len() != 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return false;
    }
    match rest.find('.') {
        None => true,
        Some(_) => {
            (1..=8).contains(&ext.len())
                && ext
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        }
    }
}

/// "image", "video", "audio" or "file", from the extension.
pub fn kind_of(name: &str) -> &'static str {
    let ext = extension(name).trim_start_matches('.');
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "image",
        // What the app's players open (Windows' Media Foundation for video,
        // symphonia and Opus for sound); one it cannot decode still offers
        // to open in the browser.
        "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "wmv" => "video",
        "mp3" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "flac" | "weba" => "audio",
        _ => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticker_ids_are_giphy_ids() {
        assert!(valid_sticker_id("xT4uQulxzV39haRFjG"));
        assert!(valid_sticker_id("3og0IPxMM0erATueVW"));
        assert!(!valid_sticker_id(""));
        assert!(!valid_sticker_id(&"a".repeat(65)));
        // Nothing that could be a path, an address or a query.
        for bad in ["../x", "a/b", "a b", "a?b=c", "https://x", "é"] {
            assert!(!valid_sticker_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_message_without_a_sticker_says_null() {
        let mut row = Row::new();
        assert_eq!(sticker_of(&row), Value::Null);
        row.insert("sticker".into(), Value::Null);
        assert_eq!(sticker_of(&row), Value::Null);
        row.insert("sticker".into(), json!("abc123"));
        assert_eq!(sticker_of(&row), json!("abc123"));
    }
}

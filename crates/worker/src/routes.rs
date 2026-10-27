//! The REST API: accounts, servers, channels, roles, moderation, invites,
//! messages, friends, direct messages and blocks. Signing in is in
//! google.rs.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};

use crate::db::{Row, RowExt, ids};
use crate::model::*;
use crate::params;
use crate::state::State;
use crate::util::*;

const REPORT_REASONS: &[(&str, &str)] = &[
    ("spam", "Spam"),
    ("assedio", "Assédio ou ódio"),
    ("improprio", "Conteúdo impróprio"),
    ("outro", "Outro motivo"),
];

pub const TOO_MANY: &str = "Muitas tentativas seguidas. Espere um pouco e tente de novo.";
const HOUR: i64 = 3_600_000;
const MINUTE: i64 = 60_000;

/// Most attachments on one message.
pub const MAX_ATTACHMENTS: usize = 100;

/// Lengths of the names, in characters (the app's fields stop at the same).
const NAME_LEN: usize = 100;
pub(crate) const DISPLAY_LEN: usize = 64;
const TOPIC_LEN: usize = 1024;
const DETAIL_LEN: usize = 2000;

/// 3 to 32 characters, lowercase letters, digits, `_` or `.`.
pub(crate) fn valid_username(s: &str) -> bool {
    (3..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// "#rrggbb", in either case.
fn valid_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Runs of ASCII whitespace become a single "-", like the Go `\s+`.
fn dashed(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ') {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push('-');
        }
        in_space = false;
        out.push(c);
    }
    out
}

/// One API request, already parsed: the JSON body, path parameters and,
/// for signed-in routes, the session.
pub struct Req {
    pub body: Map<String, Value>,
    pub params: HashMap<String, String>,
    pub query: Vec<(String, String)>,
    pub uid: i64,
    pub token_hash: String,
    /// Whether this request is on a test database (the tests' `x-babu-test`
    /// header with the right secret), never the real chat.
    pub test: bool,
    /// Uploaded files the handler decided to delete, removed afterwards.
    pub remove: Vec<String>,
}

impl Req {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.body.get(key)
    }

    fn has(&self, key: &str) -> bool {
        self.body.contains_key(key)
    }

    pub(crate) fn param(&self, name: &str) -> ApiResult<i64> {
        param_int(self.raw_param(name))
    }

    pub(crate) fn raw_param(&self, name: &str) -> &str {
        self.params.get(name).map_or("", String::as_str)
    }

    pub(crate) fn query(&self, name: &str) -> &str {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

pub type Handler = fn(&mut State, &mut Req) -> ApiResult;

/// Method, path pattern, whether it needs a session, handler.
pub const ROUTES: &[(&str, &str, bool, Handler)] = &[
    ("GET", "/api/version", false, version),
    // accounts: signing in is Google's (google.rs; its POST is in router.rs)
    ("GET", "/api/auth/google", false, crate::google::config),
    ("POST", "/api/logout", true, logout),
    ("DELETE", "/api/me", true, delete_account),
    ("PATCH", "/api/me", true, update_me),
    // what the apps from before passwords went away still call
    ("POST", "/api/register", false, gone),
    ("POST", "/api/login", false, gone),
    // only on a test database: an account and a session, for the tests
    ("POST", "/api/test/session", false, test_session),
    // servers
    ("POST", "/api/servers", true, create_server),
    ("PATCH", "/api/servers/{sid}", true, update_server),
    ("DELETE", "/api/servers/{sid}", true, delete_server),
    ("POST", "/api/servers/{sid}/leave", true, leave_server),
    // join requests, for private servers: accepted, or turned down (or
    // given up by whoever asked)
    (
        "PUT",
        "/api/servers/{sid}/requests/{uid}",
        true,
        accept_request,
    ),
    (
        "DELETE",
        "/api/servers/{sid}/requests/{uid}",
        true,
        drop_request,
    ),
    // channels
    ("POST", "/api/servers/{sid}/channels", true, create_channel),
    ("PATCH", "/api/channels/{cid}", true, update_channel),
    ("DELETE", "/api/channels/{cid}", true, delete_channel),
    (
        "PUT",
        "/api/channels/{cid}/overrides/{rid}",
        true,
        set_override,
    ),
    // roles
    ("POST", "/api/servers/{sid}/roles", true, create_role),
    ("PATCH", "/api/roles/{rid}", true, update_role),
    ("DELETE", "/api/roles/{rid}", true, delete_role),
    (
        "PUT",
        "/api/servers/{sid}/members/{uid}/roles/{rid}",
        true,
        add_member_role,
    ),
    (
        "DELETE",
        "/api/servers/{sid}/members/{uid}/roles/{rid}",
        true,
        remove_member_role,
    ),
    // moderation
    ("DELETE", "/api/servers/{sid}/members/{uid}", true, kick),
    ("PUT", "/api/servers/{sid}/bans/{uid}", true, ban),
    ("DELETE", "/api/servers/{sid}/bans/{uid}", true, unban),
    // invites
    ("POST", "/api/servers/{sid}/invites", true, create_invite),
    ("POST", "/api/invites/{code}/join", true, join),
    // messages
    ("GET", "/api/channels/{cid}/messages", true, list_messages),
    ("POST", "/api/channels/{cid}/messages", true, send_message),
    ("PATCH", "/api/messages/{mid}", true, edit_message),
    ("DELETE", "/api/messages/{mid}", true, delete_message),
    ("POST", "/api/messages/{mid}/report", true, report),
    ("DELETE", "/api/reports/{rid}", true, resolve_report),
    (
        "PUT",
        "/api/messages/{mid}/reactions/{emoji}",
        true,
        add_reaction,
    ),
    (
        "DELETE",
        "/api/messages/{mid}/reactions/{emoji}",
        true,
        remove_reaction,
    ),
    // friends and direct messages
    ("POST", "/api/friends", true, add_friend),
    ("PUT", "/api/friends/{uid}", true, accept_friend),
    ("DELETE", "/api/friends/{uid}", true, remove_friend),
    ("POST", "/api/dms", true, open_dm),
    ("PUT", "/api/blocks/{uid}", true, block),
    ("DELETE", "/api/blocks/{uid}", true, unblock),
    // favourite stickers (GIPHY ids)
    ("PUT", "/api/me/stickers/{gid}", true, add_sticker),
    ("DELETE", "/api/me/stickers/{gid}", true, remove_sticker),
    // end-to-end encryption (e2e.rs): public keys, readers, sealed sessions
    ("PUT", "/api/e2e/keys", true, crate::e2e::put_keys),
    ("GET", "/api/e2e/keys", true, crate::e2e::get_keys),
    ("GET", "/api/e2e/sessions", true, crate::e2e::get_sessions),
    // the public record of keys and of the app's versions
    ("GET", "/api/e2e/log", true, crate::e2e::get_log),
    // who let each member in, signed
    (
        "POST",
        "/api/servers/{sid}/e2e/certs",
        true,
        crate::e2e::put_certs,
    ),
    // messages that disappear: the conversation's timer, the keys of the day
    ("PUT", "/api/channels/{cid}/ttl", true, set_ttl),
    ("PUT", "/api/e2e/prekeys", true, crate::e2e::put_prekeys),
    (
        "POST",
        "/api/e2e/log/release",
        false,
        crate::e2e::put_release,
    ),
    (
        "GET",
        "/api/channels/{cid}/e2e/readers",
        true,
        crate::e2e::readers,
    ),
    (
        "POST",
        "/api/channels/{cid}/e2e/sessions",
        true,
        crate::e2e::put_session,
    ),
];

fn pair(x: i64, y: i64) -> (i64, i64) {
    if x < y { (x, y) } else { (y, x) }
}

fn ok() -> ApiResult {
    Ok(json!({}))
}

// ---------------------------------------------------------------- accounts

fn version(_: &mut State, _: &mut Req) -> ApiResult {
    Ok(json!({
        "name": babutela_buildinfo::NAME,
        "version": babutela_buildinfo::VERSION,
    }))
}

/// The apps from before Google sign-in still try these: they are told to
/// update (they do it on their own, at the next start).
fn gone(_: &mut State, _: &mut Req) -> ApiResult {
    fail(
        410,
        "O Babu Tela agora entra com a conta do Google. Feche e abra o app para atualizar.",
    )
}

fn logout(st: &mut State, q: &mut Req) -> ApiResult {
    st.db.run(
        "DELETE FROM sessions WHERE token_hash = ?",
        params![q.token_hash],
    )?;
    ok()
}

/// The tests' way in: on a test database only (a fresh, empty one per run),
/// an account by that name, made the first time, and a new session for it.
/// The real chat answers like any unknown route.
fn test_session(st: &mut State, q: &mut Req) -> ApiResult {
    if !q.test {
        return fail(404, "Rota não encontrada");
    }
    let username = to_lower(str(q.get("username")).trim());
    if !valid_username(&username) {
        return fail(400, "Usuário inválido");
    }
    let uid = match st
        .db
        .get("SELECT id FROM users WHERE username = ?", params![username])?
    {
        Some(u) => u.int("id"),
        None => {
            let mut display = truncate(str(q.get("displayName")).trim(), DISPLAY_LEN);
            if display.is_empty() {
                display.clone_from(&username);
            }
            st.db.insert(
                "INSERT INTO users(username, display_name, color, created) VALUES(?, ?, ?, ?)",
                params![username, display, pick(COLORS), now()],
            )?
        }
    };
    Ok(json!({ "token": st.new_session(uid)?, "id": uid }))
}

/// Deletes the account and the data tied to it: the user's messages, direct
/// conversations, servers they own and uploaded files. The app asks first;
/// signing in again with the same Google account makes a new, empty one.
fn delete_account(st: &mut State, q: &mut Req) -> ApiResult {
    let u = st.user_by_id(q.uid)?.unwrap_or_default();
    let uid = q.uid;
    st.voice_leave(uid)?;
    let owned = ids(
        &st.db
            .all("SELECT id FROM servers WHERE owner_id = ?", params![uid])?,
        "id",
    );
    let mut owned_members = Vec::new();
    for &sid in &owned {
        owned_members.push((
            sid,
            ids(
                &st.db.all(
                    "SELECT user_id FROM members WHERE server_id = ?",
                    params![sid],
                )?,
                "user_id",
            ),
        ));
    }
    let joined: Vec<i64> = ids(
        &st.db.all(
            "SELECT server_id FROM members WHERE user_id = ?",
            params![uid],
        )?,
        "server_id",
    )
    .into_iter()
    .filter(|sid| !owned.contains(sid))
    .collect();
    let mut dms = Vec::new();
    for cid in ids(
        &st.db.all(
            "SELECT channel_id FROM dm_members WHERE user_id = ?",
            params![uid],
        )?,
        "channel_id",
    ) {
        let other = st
            .db
            .get(
                "SELECT user_id FROM dm_members WHERE channel_id = ? AND user_id != ?",
                params![cid, uid],
            )?
            .map_or(0, |o| o.int("user_id"));
        dms.push((cid, other));
    }
    let audience: Vec<i64> = st
        .related(uid)?
        .into_iter()
        .filter(|id| *id != uid)
        .collect();
    let mut files = Vec::new();
    if !u.str("avatar").is_empty() {
        files.push(u.str("avatar").to_owned());
    }
    // The pictures of the servers that go, the people waiting to get into
    // them, and the servers this one was waiting on.
    for r in st.db.all(
        "SELECT icon FROM servers WHERE owner_id = ? AND icon IS NOT NULL",
        params![uid],
    )? {
        files.push(r.str("icon").to_owned());
    }
    let waiting = ids(
        &st.db.all(
            "SELECT DISTINCT user_id FROM join_requests WHERE server_id IN (SELECT id FROM servers WHERE owner_id = ?)",
            params![uid],
        )?,
        "user_id",
    );
    let asked = ids(
        &st.db.all(
            "SELECT server_id FROM join_requests WHERE user_id = ?",
            params![uid],
        )?,
        "server_id",
    );
    for r in st.db.all(
        "SELECT attachments FROM messages WHERE author_id = ?
          OR channel_id IN (SELECT id FROM channels WHERE server_id IN (SELECT id FROM servers WHERE owner_id = ?))
          OR channel_id IN (SELECT channel_id FROM dm_members WHERE user_id = ?)",
        params![uid, uid, uid],
    )? {
        if let Ok(Value::Array(atts)) = serde_json::from_str::<Value>(r.str("attachments")) {
            files.extend(
                atts.iter()
                    .filter_map(|a| a.get("url")?.as_str().map(str::to_owned)),
            );
        }
    }
    st.db.tx(|| {
        st.db
            .run("DELETE FROM servers WHERE owner_id = ?", params![uid])?;
        for (cid, _) in &dms {
            st.db
                .run("DELETE FROM channels WHERE id = ?", params![cid])?;
        }
        st.db
            .run("DELETE FROM messages WHERE author_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM reactions WHERE user_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM member_roles WHERE user_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM members WHERE user_id = ?", params![uid])?;
        st.db.run(
            "DELETE FROM friends WHERE a = ? OR b = ?",
            params![uid, uid],
        )?;
        st.db.run(
            "DELETE FROM blocks WHERE user_id = ? OR blocked_id = ?",
            params![uid, uid],
        )?;
        st.db
            .run("DELETE FROM reports WHERE reporter_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM invites WHERE creator_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM bans WHERE user_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM join_requests WHERE user_id = ?", params![uid])?;
        st.db
            .run("DELETE FROM stickers WHERE user_id = ?", params![uid])?;
        // The keys it published, and the session keys sealed by it or for it.
        st.db
            .run("DELETE FROM e2e_keys WHERE user_id = ?", params![uid])?;
        st.db.run(
            "DELETE FROM e2e_sessions WHERE user_id = ? OR sender_id = ?",
            params![uid, uid],
        )?;
        st.db.run("DELETE FROM users WHERE id = ?", params![uid])?; // sessions go with it
        Ok(())
    })?;
    q.remove
        .extend(files.into_iter().filter(|f| is_upload_url(f)));
    st.drop_sockets(uid, "");
    for (sid, members) in &owned_members {
        for &m in members.iter().filter(|m| **m != uid) {
            st.send(m, "SERVER_REMOVE", &json!({ "id": sid }));
        }
    }
    for &(cid, other) in &dms {
        if other != 0 {
            st.send(other, "DM_REMOVE", &json!({ "id": cid }));
        }
    }
    for &id in &audience {
        st.send(id, "USER_DELETED", &json!({ "userId": uid }));
    }
    for &sid in &joined {
        st.sync_server(sid)?;
    }
    for &sid in &asked {
        st.sync_approvers(sid)?;
    }
    for &other in waiting.iter().filter(|w| **w != uid) {
        st.emit_requests(other)?;
    }
    ok()
}

fn update_me(st: &mut State, q: &mut Req) -> ApiResult {
    if q.has("displayName") {
        let name = text(q.get("displayName"), DISPLAY_LEN, "O nome")?;
        st.db.run(
            "UPDATE users SET display_name = ? WHERE id = ?",
            params![name, q.uid],
        )?;
    }
    if q.has("avatar") {
        let av = q.get("avatar").cloned().unwrap_or(Value::Null);
        if let Some(url) = av.as_str() {
            if kind_of(url) != "image" || crate::files::finished(st, url)?.is_none() {
                return fail(400, "Avatar inválido");
            }
            st.db.run(
                "UPDATE uploads SET used = 1 WHERE name = ?",
                params![&url["/uploads/".len()..]],
            )?;
        } else if !av.is_null() {
            return fail(400, "Avatar inválido");
        }
        st.db.run(
            "UPDATE users SET avatar = ? WHERE id = ?",
            params![av, q.uid],
        )?;
    }
    let u = pub_user(st.user_by_id(q.uid)?.as_ref());
    let mut seen = HashSet::new();
    for id in std::iter::once(q.uid).chain(st.related(q.uid)?) {
        if seen.insert(id) {
            st.send(id, "USER_UPDATE", &u);
        }
    }
    Ok(u)
}

// ---------------------------------------------------------------- servers

fn create_server(st: &mut State, q: &mut Req) -> ApiResult {
    st.limit(&format!("servers:{}", q.uid), 10, HOUR)?;
    let name = text(q.get("name"), NAME_LEN, "O nome do servidor")?;
    let uid = q.uid;
    let sid = st.db.tx(|| {
        // An id is never given again, even after the newest server goes:
        // the apps pin each server's owner by its id
        // (crates/app/src/e2e/members.rs).
        let given = st
            .db
            .get("SELECT v FROM meta WHERE k = 'server_id'", params![])?
            .and_then(|r| r.str("v").parse::<i64>().ok())
            .unwrap_or(0);
        let max = st
            .db
            .get("SELECT COALESCE(MAX(id), 0) AS n FROM servers", params![])?
            .map_or(0, |r| r.int("n"));
        let sid = given.max(max) + 1;
        st.db.run(
            "INSERT INTO servers(id, name, owner_id, color, created) VALUES(?, ?, ?, ?, ?)",
            params![sid, name, uid, pick(COLORS), now()],
        )?;
        st.db.run(
            "INSERT INTO meta(k, v) VALUES('server_id', ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![sid.to_string()],
        )?;
        st.db.run("INSERT INTO members(server_id, user_id, joined) VALUES(?, ?, ?)", params![sid, uid, now()])?;
        st.db.run(
            "INSERT INTO roles(server_id, name, perms, position, is_everyone) VALUES(?, '@todos', ?, 0, 1)",
            params![sid, DEFAULT_PERMS],
        )?;
        st.db.run(
            "INSERT INTO channels(server_id, name, type, category, position) VALUES(?, 'geral', 'text', 'Canais de texto', 0)",
            params![sid],
        )?;
        st.db.run(
            "INSERT INTO channels(server_id, name, type, category, position) VALUES(?, 'Sala de voz', 'voice', 'Canais de voz', 1)",
            params![sid],
        )?;
        Ok(sid)
    })?;
    st.sync_server(sid)?;
    Ok(json!({ "id": sid }))
}

/// The biggest picture a server may have (the app sends a 256 px one of a
/// few dozen KB): every member's app downloads it.
const ICON_MAX: i64 = 5 << 20;

/// A server's new picture: an image the person just uploaded for it, used
/// nowhere else (so replacing it may delete the old file), or null for
/// none. The one it already has is fine too.
fn icon_input(st: &State, uid: i64, srv: &Row, v: Option<&Value>) -> ApiResult<Value> {
    let url = match v {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(Value::String(url)) => url,
        Some(_) => return fail(400, "Imagem inválida"),
    };
    if url == srv.str("icon") {
        return Ok(Value::String(url.clone()));
    }
    let Some(row) = crate::files::finished(st, url)?
        .filter(|row| kind_of(url) == "image" && row.int("user_id") == uid && row.int("used") == 0)
    else {
        return fail(400, "Imagem inválida");
    };
    if row.int("size") > ICON_MAX {
        return fail(400, "A imagem do servidor pode ter até 5 MB");
    }
    Ok(Value::String(url.clone()))
}

/// Name, picture and whether it is private: whichever of them the body
/// has (apps up to 0.11 only ever send the name).
fn update_server(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_MANAGE_SERVER)?;
    let sid = srv.int("id");
    let name = if q.has("name") {
        Some(text(q.get("name"), NAME_LEN, "O nome do servidor")?)
    } else {
        None
    };
    let icon = if q.has("icon") {
        Some(icon_input(st, q.uid, &srv, q.get("icon"))?)
    } else {
        None
    };
    let private = match (q.has("private"), q.get("private")) {
        (false, _) => None,
        (true, Some(Value::Bool(on))) => Some(*on),
        (true, _) => return fail(400, "“private” precisa ser verdadeiro ou falso"),
    };
    if name.is_none() && icon.is_none() && private.is_none() {
        return fail(400, "Nada para mudar");
    }
    st.db.tx(|| {
        if let Some(name) = &name {
            st.db.run(
                "UPDATE servers SET name = ? WHERE id = ?",
                params![name, sid],
            )?;
        }
        if let Some(icon) = &icon {
            if let Some(url) = icon.as_str() {
                st.db.run(
                    "UPDATE uploads SET used = 1 WHERE name = ?",
                    params![&url["/uploads/".len()..]],
                )?;
            }
            st.db.run(
                "UPDATE servers SET icon = ? WHERE id = ?",
                params![icon, sid],
            )?;
        }
        if let Some(on) = private {
            st.db.run(
                "UPDATE servers SET private = ? WHERE id = ?",
                params![i64::from(on), sid],
            )?;
        }
        Ok(())
    })?;
    // The picture it had is not needed by anyone any more.
    let old = srv.str("icon");
    if icon.is_some() && !old.is_empty() && icon.as_ref().and_then(Value::as_str) != Some(old) {
        q.remove.push(old.to_owned());
    }
    st.sync_server(sid)?;
    ok()
}

fn delete_server(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    if srv.int("owner_id") != q.uid {
        return fail(403, "Só o dono pode excluir o servidor");
    }
    let sid = srv.int("id");
    let members = ids(
        &st.db.all(
            "SELECT user_id FROM members WHERE server_id = ?",
            params![sid],
        )?,
        "user_id",
    );
    for cid in ids(
        &st.db.all(
            "SELECT id FROM channels WHERE server_id = ? AND type = 'voice'",
            params![sid],
        )?,
        "id",
    ) {
        for uid in st.voice_members(cid) {
            st.voice_leave(uid)?;
        }
    }
    let waiting = ids(
        &st.db.all(
            "SELECT user_id FROM join_requests WHERE server_id = ?",
            params![sid],
        )?,
        "user_id",
    );
    st.db
        .run("DELETE FROM servers WHERE id = ?", params![sid])?;
    for m in members {
        st.send(m, "SERVER_REMOVE", &json!({ "id": sid }));
    }
    // Their requests went with it.
    for uid in waiting {
        st.emit_requests(uid)?;
    }
    if is_upload_url(srv.str("icon")) {
        q.remove.push(srv.str("icon").to_owned());
    }
    ok()
}

fn leave_server(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    if srv.int("owner_id") == q.uid {
        return fail(400, "O dono não pode sair. Exclua o servidor.");
    }
    st.remove_member(srv.int("id"), q.uid)?;
    ok()
}

// ---------------------------------------------------------------- channels

fn channel_name(name: Option<&Value>, typ: &str) -> ApiResult<String> {
    let n = text(name, NAME_LEN, "O nome do canal")?;
    if typ == "text" {
        return Ok(dashed(&to_lower(&n)));
    }
    Ok(n)
}

fn managed_channel(st: &State, q: &Req) -> ApiResult<(Row, Row)> {
    let a = st.access(q.uid, q.param("cid")?)?;
    let Some(srv) = a.srv else {
        return fail(400, "Canal inválido");
    };
    st.need(&srv, q.uid, P_MANAGE_CHANNELS)?;
    Ok((a.c, srv))
}

fn create_channel(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_MANAGE_CHANNELS)?;
    let typ = if q.get("type") == Some(&json!("voice")) {
        "voice"
    } else {
        "text"
    };
    let pos = st
        .db
        .get(
            "SELECT COALESCE(MAX(position), 0) + 1 AS p FROM channels WHERE server_id = ?",
            params![srv.int("id")],
        )?
        .unwrap_or_default()
        .int("p");
    let mut category = truncate(str(q.get("category")).trim(), NAME_LEN);
    if category.is_empty() {
        category = if typ == "voice" {
            "Canais de voz"
        } else {
            "Canais de texto"
        }
        .into();
    }
    let name = channel_name(q.get("name"), typ)?;
    let id = st.db.insert(
        "INSERT INTO channels(server_id, name, type, category, position) VALUES(?, ?, ?, ?, ?)",
        params![srv.int("id"), name, typ, category, pos],
    )?;
    st.sync_server(srv.int("id"))?;
    Ok(json!({ "id": id }))
}

fn update_channel(st: &mut State, q: &mut Req) -> ApiResult {
    let (c, srv) = managed_channel(st, q)?;
    let (mut name, mut category, mut topic) = (c.val("name"), c.val("category"), c.val("topic"));
    if q.has("name") {
        name = channel_name(q.get("name"), c.str("type"))?.into();
    }
    if q.has("category") {
        let cat = truncate(str(q.get("category")).trim(), NAME_LEN);
        if !cat.is_empty() {
            category = cat.into();
        }
    }
    if q.has("topic") {
        topic = truncate(str(q.get("topic")).trim(), TOPIC_LEN).into();
    }
    st.db.run(
        "UPDATE channels SET name = ?, category = ?, topic = ? WHERE id = ?",
        params![name, category, topic, c.int("id")],
    )?;
    st.sync_server(srv.int("id"))?;
    ok()
}

fn delete_channel(st: &mut State, q: &mut Req) -> ApiResult {
    let (c, srv) = managed_channel(st, q)?;
    for uid in st.voice_members(c.int("id")) {
        st.voice_leave(uid)?;
    }
    st.db
        .run("DELETE FROM channels WHERE id = ?", params![c.int("id")])?;
    st.sync_server(srv.int("id"))?;
    ok()
}

/// How long the messages of a conversation last: for good (0), a day or
/// seven. In a direct one, either of the two decides; in a server's
/// channel, whoever manages channels. The messages already there keep
/// theirs; the apps say what changed (crates/app/src/e2e/mod.rs keys the
/// new ones so that they can no longer be read once they are gone).
fn set_ttl(st: &mut State, q: &mut Req) -> ApiResult {
    let a = st.access(q.uid, q.param("cid")?)?;
    let ttl = to_int(q.get("ttl"))?;
    if ![0, 86_400, 7 * 86_400].contains(&ttl) {
        return fail(400, "Tempo inválido");
    }
    if let Some(srv) = &a.srv {
        st.need(srv, q.uid, P_MANAGE_CHANNELS)?;
    } else if a.c.str("type") != "dm" {
        return fail(400, "Canal inválido");
    }
    st.db.run(
        "UPDATE channels SET ttl = ? WHERE id = ?",
        params![ttl, a.c.int("id")],
    )?;
    match &a.srv {
        Some(srv) => st.sync_server(srv.int("id"))?,
        None => {
            for uid in st.audience(&a.c)? {
                let dm = st.dm_snapshot(a.c.int("id"), uid)?;
                st.send(uid, "DM_UPDATE", &dm);
            }
        }
    }
    ok()
}

fn set_override(st: &mut State, q: &mut Req) -> ApiResult {
    let (c, srv) = managed_channel(st, q)?;
    st.need(&srv, q.uid, P_MANAGE_ROLES)?;
    let Some(role) = st.db.get(
        "SELECT * FROM roles WHERE id = ? AND server_id = ?",
        params![q.param("rid")?, srv.int("id")],
    )?
    else {
        return fail(404, "Cargo não encontrado");
    };
    let allow = to_int(q.get("allow"))? & CHANNEL_BITS;
    let deny = to_int(q.get("deny"))? & CHANNEL_BITS & !allow;
    if allow != 0 || deny != 0 {
        st.db.run(
            "INSERT INTO overrides(channel_id, role_id, allow, deny) VALUES(?, ?, ?, ?)
              ON CONFLICT(channel_id, role_id) DO UPDATE SET allow = excluded.allow, deny = excluded.deny",
            params![c.int("id"), role.int("id"), allow, deny],
        )?;
    } else {
        st.db.run(
            "DELETE FROM overrides WHERE channel_id = ? AND role_id = ?",
            params![c.int("id"), role.int("id")],
        )?;
    }
    // Anyone who just lost access to a voice channel is dropped from it.
    for uid in st.voice_members(c.int("id")) {
        let p = st.perms_for(&srv, uid, c.int("id"))?;
        if p & P_VIEW == 0 || p & P_CONNECT == 0 {
            st.voice_leave(uid)?;
        }
    }
    st.sync_server(srv.int("id"))?;
    ok()
}

// ---------------------------------------------------------------- roles

/// A role manager can never hand out permissions they do not hold themselves.
fn role_input(
    st: &State,
    srv: &Row,
    uid: i64,
    body: &Map<String, Value>,
    current: Option<&Row>,
) -> ApiResult<(i64, Value)> {
    let mine = st.perms_for(srv, uid, 0)?;
    let perms = match (body.get("perms"), current) {
        (Some(v), _) => to_int(Some(v))? & P_ALL,
        (None, Some(cur)) => cur.int("perms"),
        (None, None) => 0,
    };
    if perms & !mine != 0 {
        return fail(403, "Você não pode conceder permissões que não tem");
    }
    if let Some(cur) = current
        && cur.int("perms") & !mine != 0
    {
        return fail(403, "Esse cargo está acima do seu");
    }
    let color = match (body.get("color"), current) {
        (Some(Value::String(c)), _) if valid_color(c) => Value::String(c.clone()),
        (Some(_), _) => Value::Null,
        (None, Some(cur)) => cur.val("color"),
        (None, None) => Value::Null,
    };
    Ok((perms, color))
}

fn managed_role(st: &State, q: &Req) -> ApiResult<(Row, Row)> {
    let Some(role) = st
        .db
        .get("SELECT * FROM roles WHERE id = ?", params![q.param("rid")?])?
    else {
        return fail(404, "Cargo não encontrado");
    };
    let srv = st.server_for(q.uid, role.int("server_id"))?;
    st.need(&srv, q.uid, P_MANAGE_ROLES)?;
    Ok((role, srv))
}

fn create_role(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_MANAGE_ROLES)?;
    let mut body = Map::new();
    body.insert(
        "perms".into(),
        json!(DEFAULT_PERMS & st.perms_for(&srv, q.uid, 0)?),
    );
    body.extend(q.body.clone());
    let (perms, color) = role_input(st, &srv, q.uid, &body, None)?;
    let pos = st
        .db
        .get(
            "SELECT COALESCE(MAX(position), 0) + 1 AS p FROM roles WHERE server_id = ?",
            params![srv.int("id")],
        )?
        .unwrap_or_default()
        .int("p");
    let name = text(q.get("name"), NAME_LEN, "O nome do cargo")?;
    let id = st.db.insert(
        "INSERT INTO roles(server_id, name, color, perms, position) VALUES(?, ?, ?, ?, ?)",
        params![srv.int("id"), name, color, perms, pos],
    )?;
    st.sync_server(srv.int("id"))?;
    Ok(json!({ "id": id }))
}

fn update_role(st: &mut State, q: &mut Req) -> ApiResult {
    let (role, srv) = managed_role(st, q)?;
    let (perms, color) = role_input(st, &srv, q.uid, &q.body, Some(&role))?;
    let mut name = role.val("name");
    if role.int("is_everyone") == 0 && q.has("name") {
        name = text(q.get("name"), NAME_LEN, "O nome do cargo")?.into();
    }
    st.db.run(
        "UPDATE roles SET name = ?, color = ?, perms = ? WHERE id = ?",
        params![name, color, perms, role.int("id")],
    )?;
    st.sync_server(srv.int("id"))?;
    ok()
}

fn delete_role(st: &mut State, q: &mut Req) -> ApiResult {
    let (role, srv) = managed_role(st, q)?;
    if role.int("is_everyone") == 1 {
        return fail(400, "O cargo @todos não pode ser excluído");
    }
    role_input(st, &srv, q.uid, &Map::new(), Some(&role))?;
    st.db
        .run("DELETE FROM roles WHERE id = ?", params![role.int("id")])?;
    st.sync_server(srv.int("id"))?;
    ok()
}

fn member_role(st: &mut State, q: &mut Req, add: bool) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_MANAGE_ROLES)?;
    let target = q.param("uid")?;
    st.server_for(target, srv.int("id"))?;
    let Some(role) = st.db.get(
        "SELECT * FROM roles WHERE id = ? AND server_id = ? AND is_everyone = 0",
        params![q.param("rid")?, srv.int("id")],
    )?
    else {
        return fail(404, "Cargo não encontrado");
    };
    role_input(st, &srv, q.uid, &Map::new(), Some(&role))?;
    if add {
        st.db.run(
            "INSERT OR IGNORE INTO member_roles(server_id, user_id, role_id) VALUES(?, ?, ?)",
            params![srv.int("id"), target, role.int("id")],
        )?;
    } else {
        st.db.run(
            "DELETE FROM member_roles WHERE user_id = ? AND role_id = ?",
            params![target, role.int("id")],
        )?;
    }
    st.sync_server(srv.int("id"))?;
    ok()
}

fn add_member_role(st: &mut State, q: &mut Req) -> ApiResult {
    member_role(st, q, true)
}

fn remove_member_role(st: &mut State, q: &mut Req) -> ApiResult {
    member_role(st, q, false)
}

// ---------------------------------------------------------------- moderation

fn kick(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_KICK)?;
    let target = q.param("uid")?;
    st.server_for(target, srv.int("id"))?;
    if !st.can_moderate(&srv, q.uid, target)? {
        return fail(403, "Você não pode expulsar esse membro");
    }
    st.remove_member(srv.int("id"), target)?;
    ok()
}

fn ban(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_BAN)?;
    let target = q.param("uid")?;
    if st.user_by_id(target)?.is_none() {
        return fail(404, "Usuário não encontrado");
    }
    if !st.can_moderate(&srv, q.uid, target)? {
        return fail(403, "Você não pode banir esse membro");
    }
    st.db.run(
        "INSERT OR IGNORE INTO bans(server_id, user_id) VALUES(?, ?)",
        params![srv.int("id"), target],
    )?;
    // Someone still asking to come in is turned down too.
    let asked = st.db.run(
        "DELETE FROM join_requests WHERE server_id = ? AND user_id = ?",
        params![srv.int("id"), target],
    )?;
    if asked > 0 {
        st.send(
            target,
            "JOIN_DENIED",
            &json!({ "id": srv.val("id"), "name": srv.val("name") }),
        );
        st.emit_requests(target)?;
    }
    st.remove_member(srv.int("id"), target)?;
    ok()
}

fn unban(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_BAN)?;
    st.db.run(
        "DELETE FROM bans WHERE server_id = ? AND user_id = ?",
        params![srv.int("id"), q.param("uid")?],
    )?;
    st.sync_server(srv.int("id"))?;
    ok()
}

// ---------------------------------------------------------------- invites

/// An invite. An app with end-to-end encryption picks the code itself and
/// signs it with the public half of a key whose secret only the link
/// carries (crates/app/src/e2e/members.rs): whoever comes in with it signs
/// their entry with that key, which the server never sees. Without one
/// (an older app), the code is made here and the people it lets in wait
/// for a member to vouch for them.
fn create_invite(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_INVITE)?;
    st.limit(&format!("invite:{}", q.uid), 30, HOUR)?;
    let sid = srv.int("id");
    let code = match q.get("code").and_then(Value::as_str) {
        Some(c) => {
            if !crate::e2e::is_b64(c, 6) {
                return fail(400, "Código de convite inválido");
            }
            if st
                .db
                .get("SELECT 1 FROM invites WHERE code = ?", params![c])?
                .is_some()
            {
                return fail(409, "Esse código já existe");
            }
            c.to_owned()
        }
        None => URL_SAFE_NO_PAD.encode(random_bytes::<6>()),
    };
    let cert = q
        .get("cert")
        .filter(|c| crate::e2e::valid_invite_cert(sid, &code, q.uid, srv.int("owner_id"), c))
        .map(Value::to_string);
    st.db.run(
        "INSERT INTO invites(code, server_id, creator_id, created, cert) VALUES(?, ?, ?, ?, ?)",
        params![code, sid, q.uid, now(), cert],
    )?;
    Ok(json!({ "code": code }))
}

fn join(st: &mut State, q: &mut Req) -> ApiResult {
    st.limit(&format!("join:{}", q.uid), 20, HOUR)?;
    let code = q
        .raw_param("code")
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_owned();
    let Some(inv) = st
        .db
        .get("SELECT * FROM invites WHERE code = ?", params![code])?
    else {
        return fail(404, "Convite inválido ou expirado");
    };
    if st
        .db
        .get(
            "SELECT 1 FROM bans WHERE server_id = ? AND user_id = ?",
            params![inv.int("server_id"), q.uid],
        )?
        .is_some()
    {
        return fail(403, "Você foi banido desse servidor");
    }
    let sid = inv.int("server_id");
    let srv = st
        .db
        .get("SELECT * FROM servers WHERE id = ?", params![sid])?
        .unwrap_or_default();
    let member = st
        .db
        .get(
            "SELECT 1 FROM members WHERE server_id = ? AND user_id = ?",
            params![sid, q.uid],
        )?
        .is_some();
    // The invite's certificate: the app signs its entry with the key the
    // link carries (e2e.rs, `put_certs`), and takes the server's owner from
    // it, even when it only asks to come in.
    let invite = serde_json::from_str::<Value>(inv.str("cert")).unwrap_or(Value::Null);
    // A private server: the invite only lets them ask, and someone in it
    // (with P_KICK) lets them in.
    if !member && srv.int("private") != 0 {
        let asked = st
            .db
            .get(
                "SELECT 1 FROM join_requests WHERE server_id = ? AND user_id = ?",
                params![sid, q.uid],
            )?
            .is_some();
        if !asked {
            let waiting = st
                .db
                .get(
                    "SELECT COUNT(*) AS n FROM join_requests WHERE server_id = ?",
                    params![sid],
                )?
                .map_or(0, |r| r.int("n"));
            if waiting >= MAX_REQUESTS {
                return fail(
                    429,
                    "Esse servidor tem pedidos demais esperando. Tente de novo mais tarde.",
                );
            }
            st.db.run(
                "INSERT INTO join_requests(server_id, user_id, created) VALUES(?, ?, ?)",
                params![sid, q.uid, now()],
            )?;
            st.sync_approvers(sid)?;
            st.emit_requests(q.uid)?;
        }
        return Ok(
            json!({ "id": sid, "pending": true, "name": srv.val("name"), "invite": invite }),
        );
    }
    st.db.run(
        "INSERT OR IGNORE INTO members(server_id, user_id, joined) VALUES(?, ?, ?)",
        params![sid, q.uid, now()],
    )?;
    // Asked while it was private, and it is not any more.
    let asked = st.db.run(
        "DELETE FROM join_requests WHERE server_id = ? AND user_id = ?",
        params![sid, q.uid],
    )?;
    st.sync_server(sid)?;
    st.emit_presence(q.uid)?;
    if asked > 0 {
        st.emit_requests(q.uid)?;
    }
    Ok(json!({ "id": sid, "invite": invite }))
}

/// Most join requests a server keeps waiting at once.
const MAX_REQUESTS: i64 = 200;

/// Lets someone who asked into the server.
fn accept_request(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    st.need(&srv, q.uid, P_KICK)?;
    let (sid, target) = (srv.int("id"), q.param("uid")?);
    // The approver's signed certificate, from an app with end-to-end
    // encryption (e2e.rs).
    let certs = crate::e2e::approval(sid, target, q.uid, q.get("cert"));
    let found = st.db.tx(|| {
        let n = st.db.run(
            "DELETE FROM join_requests WHERE server_id = ? AND user_id = ?",
            params![sid, target],
        )?;
        if n > 0 {
            st.db.run(
                "INSERT OR IGNORE INTO members(server_id, user_id, joined, certs) VALUES(?, ?, ?, ?)",
                params![sid, target, now(), certs],
            )?;
        }
        Ok(n > 0)
    })?;
    if !found {
        return fail(404, "Pedido não encontrado");
    }
    st.sync_server(sid)?;
    st.emit_presence(target)?;
    st.emit_requests(target)?;
    ok()
}

/// Turns a request down; or, from whoever asked, gives it up (they are not
/// a member, so nothing else of the server is theirs to see).
fn drop_request(st: &mut State, q: &mut Req) -> ApiResult {
    let (sid, target) = (q.param("sid")?, q.param("uid")?);
    let srv = if target == q.uid {
        None
    } else {
        let srv = st.server_for(q.uid, sid)?;
        st.need(&srv, q.uid, P_KICK)?;
        Some(srv)
    };
    let n = st.db.run(
        "DELETE FROM join_requests WHERE server_id = ? AND user_id = ?",
        params![sid, target],
    )?;
    if n == 0 {
        return fail(404, "Pedido não encontrado");
    }
    if let Some(srv) = srv {
        st.send(
            target,
            "JOIN_DENIED",
            &json!({ "id": sid, "name": srv.val("name") }),
        );
    }
    st.sync_approvers(sid)?;
    st.emit_requests(target)?;
    ok()
}

// ---------------------------------------------------------------- messages

fn list_messages(st: &mut State, q: &mut Req) -> ApiResult {
    let a = st.access(q.uid, q.param("cid")?)?;
    let mut before = (1i64 << 53) - 1;
    if !q.query("before").is_empty() {
        before = param_int(q.query("before"))?;
    }
    let mut rows = st.db.all(
        "SELECT * FROM messages WHERE channel_id = ? AND id < ? AND (gone_at IS NULL OR gone_at > ?)
          ORDER BY id DESC LIMIT 51",
        params![a.c.int("id"), before, now()],
    )?;
    let more = rows.len() > 50;
    rows.truncate(50);
    let mut msgs = Vec::with_capacity(rows.len());
    for r in rows.iter().rev() {
        msgs.push(st.serialize(r)?);
    }
    Ok(json!({ "more": more, "messages": msgs }))
}

/// Also reached from the gateway (its "message" op), which skips the trip
/// through the Worker.
pub(crate) fn send_message(st: &mut State, q: &mut Req) -> ApiResult {
    let a = st.access(q.uid, q.param("cid")?)?;
    let c = &a.c;
    if c.str("type") == "voice" {
        return fail(400, "Canais de voz não aceitam mensagens");
    }
    if a.perms & P_SEND == 0 {
        return fail(403, "Você não pode enviar mensagens neste canal");
    }
    if c.str("type") == "dm"
        && let Some(o) = st.db.get(
            "SELECT user_id FROM dm_members WHERE channel_id = ? AND user_id != ?",
            params![c.int("id"), q.uid],
        )?
        && st.blocked_between(q.uid, o.int("user_id"))?
    {
        return fail(403, "Você não pode enviar mensagens para essa pessoa");
    }
    // The sender's own mark: its app matches the copy it already shows to
    // this one by it, and sends it again when it cannot tell whether the
    // first try arrived (the connection fell). A second try gets the
    // message the first one made.
    let nonce = truncate(str(q.get("nonce")).trim(), 64);
    if !nonce.is_empty()
        && let Some(row) = st.db.get(
            "SELECT m.* FROM sent s JOIN messages m ON m.id = s.message_id
             WHERE s.user_id = ? AND s.nonce = ? AND m.channel_id = ?",
            params![q.uid, nonce, c.int("id")],
        )?
    {
        let mut m = st.serialize(&row)?;
        m["nonce"] = json!(nonce);
        return Ok(m);
    }
    st.limit_msg(
        &format!("msg:{}", q.uid),
        10,
        10_000,
        "Você está enviando mensagens rápido demais.",
    )?;
    // What the message says (its text, files, sticker, the message it
    // answers) is in its envelope, encrypted and signed on the sender's PC
    // (crates/app/src/e2e/msg.rs). The server keeps it as it came and reads
    // nothing of it but its shape. A message without one comes from an app
    // from before the encryption: it is turned down, so nothing ever goes
    // in plain text again.
    let Some(env) = q.get("e2e").cloned().filter(valid_envelope) else {
        return fail(400, OLD_APP);
    };
    let raw: Vec<Value> = match q.get("files") {
        Some(Value::Array(list)) => list.clone(),
        _ => Vec::new(),
    };
    if raw.len() > MAX_ATTACHMENTS {
        return fail(400, "Arquivos demais numa mensagem (máximo de 100)");
    }
    if !raw.is_empty() && a.perms & P_ATTACH == 0 {
        return fail(403, "Você não pode enviar arquivos neste canal");
    }
    // Only which files the message holds, for the server's own accounts
    // (which uploads are in use, what to delete with an account, which
    // direct file may be asked for): no name, size or kind, which only
    // the envelope has.
    let mut attachments = Vec::new();
    let mut stored = Vec::new();
    for item in &raw {
        // A file sent directly is not on the server: only its id (direct.rs).
        if let Some(direct) = crate::direct::attachment(item)? {
            attachments.push(direct);
            continue;
        }
        let url = str(item.get("url"));
        // Only files that really finished uploading.
        let Some(row) = crate::files::finished(st, &url)? else {
            return fail(400, "Anexo inválido");
        };
        attachments.push(json!({ "url": url }));
        stored.push(row.str("name").to_owned());
    }
    let content = String::new();
    let sticker = Value::Null;
    let reply_to = Value::Null;
    // The message goes out to everyone first, and is written after. A
    // write holds back everything the object sends until it is confirmed
    // durable (by three of five machines in other buildings), which cost
    // the people receiving it about 40 ms. Nothing else runs in between
    // (the object is single-threaded and this is synchronous), so the id
    // taken here is the one the row gets. Only the answer to the sender
    // still waits for the write: should it fail, the object restarts and
    // everyone loads the conversation again, without the message.
    let id = st
        .db
        .get(
            "SELECT COALESCE(MAX(id), 0) + 1 AS next FROM messages",
            params![],
        )?
        .map_or(1, |r| r.int("next"));
    let attachments = Value::Array(attachments).to_string();
    let env = env.to_string();
    let created = now();
    let mut row = Row::new();
    for (k, v) in [
        ("id", json!(id)),
        ("channel_id", json!(c.int("id"))),
        ("author_id", json!(q.uid)),
        ("content", json!(content)),
        ("attachments", json!(attachments)),
        ("reply_to", reply_to.clone()),
        ("created", json!(created)),
        ("edited", Value::Null),
        ("sticker", sticker.clone()),
        ("envelope", json!(env)),
    ] {
        row.insert(k.to_owned(), v);
    }
    let mut m = st.serialize(&row)?;
    if !nonce.is_empty() {
        m["nonce"] = json!(nonce);
    }
    st.emit_channel(c, "MESSAGE_CREATE", &m, 0)?;
    // A conversation whose messages disappear: when this one goes (the
    // sweep deletes it; the apps drop it at that moment themselves).
    let expires = match c.int("ttl") {
        0 => Value::Null,
        ttl => json!(created + ttl * 1000),
    };
    st.db.run(
        "INSERT INTO messages(id, channel_id, author_id, content, attachments, reply_to, created, sticker, envelope, gone_at) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![id, c.int("id"), q.uid, content, attachments, reply_to, created, sticker, env, expires],
    )?;
    // Written with the message, so it costs no extra wait. The sweep drops
    // it after a day.
    if !nonce.is_empty() {
        st.db.run(
            "INSERT OR IGNORE INTO sent(user_id, nonce, message_id, created) VALUES(?, ?, ?, ?)",
            params![q.uid, nonce, id, created],
        )?;
    }
    // On a message now: the sweep of abandoned uploads leaves them be.
    for name in &stored {
        st.db
            .run("UPDATE uploads SET used = 1 WHERE name = ?", params![name])?;
    }
    Ok(m)
}

/// Most favourite stickers per person: what the apps ask GIPHY about in one
/// request (its limit for ids at once).
const MAX_STICKERS: i64 = 100;

fn add_sticker(st: &mut State, q: &mut Req) -> ApiResult {
    st.limit(&format!("stickers:{}", q.uid), 60, MINUTE)?;
    let id = q.raw_param("gid").to_owned();
    if !valid_sticker_id(&id) {
        return fail(400, "Figurinha inválida");
    }
    let known = st
        .db
        .get(
            "SELECT 1 FROM stickers WHERE user_id = ? AND giphy_id = ?",
            params![q.uid, id],
        )?
        .is_some();
    if !known {
        let n = st
            .db
            .get(
                "SELECT COUNT(*) AS n FROM stickers WHERE user_id = ?",
                params![q.uid],
            )?
            .map_or(0, |r| r.int("n"));
        if n >= MAX_STICKERS {
            return fail(
                400,
                "Você já tem 100 figurinhas favoritas. Tire alguma antes de guardar outra.",
            );
        }
        st.db.run(
            "INSERT INTO stickers(user_id, giphy_id, added) VALUES(?, ?, ?)",
            params![q.uid, id, now()],
        )?;
    }
    stickers_changed(st, q.uid)
}

fn remove_sticker(st: &mut State, q: &mut Req) -> ApiResult {
    st.limit(&format!("stickers:{}", q.uid), 60, MINUTE)?;
    st.db.run(
        "DELETE FROM stickers WHERE user_id = ? AND giphy_id = ?",
        params![q.uid, q.raw_param("gid")],
    )?;
    stickers_changed(st, q.uid)
}

/// The favourites as they are now: the answer, and to every connection of
/// `uid`, so the other devices signed in follow.
fn stickers_changed(st: &mut State, uid: i64) -> ApiResult {
    let body = json!({ "stickers": st.sticker_list(uid)? });
    st.send(uid, "STICKERS", &body);
    Ok(body)
}

/// An app from before the encryption: what it is told when it sends.
const OLD_APP: &str =
    "Atualize o Babu Tela: as mensagens agora são cifradas de ponta a ponta. Feche e abra o app.";

/// The longest envelope: a message of 100 thousand characters, each up to
/// 4 bytes and escaped, padded, then in base64.
const MAX_ENVELOPE: usize = 800_000;

/// Whether `v` has the shape of an envelope (crates/app/src/e2e/msg.rs):
/// its version, session and message ids and the encrypted bytes.
fn valid_envelope(v: &Value) -> bool {
    let Some(o) = v.as_object() else {
        return false;
    };
    o.len() == 4
        && o.get("v").and_then(Value::as_i64) == Some(1)
        && crate::e2e::is_b64(&str(o.get("s")), 16)
        && crate::e2e::is_b64(&str(o.get("n")), 16)
        && crate::e2e::is_b64_text(&str(o.get("c")), MAX_ENVELOPE)
}

/// The author replaces the envelope: an edit, encrypted like the message
/// (it says which message it edits, and the apps check that). An old
/// message in plain text can be replaced this way too, and then it is not
/// readable here any more.
fn edit_message(st: &mut State, q: &mut Req) -> ApiResult {
    let ma = st.message_for(q.uid, q.param("mid")?)?;
    if ma.m.int("author_id") != q.uid {
        return fail(403, "Só o autor pode editar a mensagem");
    }
    let Some(env) = q.get("e2e").cloned().filter(valid_envelope) else {
        return fail(400, OLD_APP);
    };
    st.db.run(
        "UPDATE messages SET content = '', sticker = NULL, reply_to = NULL, envelope = ?, edited = ? WHERE id = ?",
        params![env.to_string(), now(), ma.m.int("id")],
    )?;
    let row = st
        .db
        .get(
            "SELECT * FROM messages WHERE id = ?",
            params![ma.m.int("id")],
        )?
        .unwrap_or_default();
    let m = st.serialize(&row)?;
    st.emit_channel(&ma.a.c, "MESSAGE_UPDATE", &m, 0)?;
    ok()
}

fn delete_message(st: &mut State, q: &mut Req) -> ApiResult {
    let ma = st.message_for(q.uid, q.param("mid")?)?;
    let moderator = ma.a.c.str("type") != "dm" && ma.a.perms & P_MANAGE_MESSAGES != 0;
    if ma.m.int("author_id") != q.uid && !moderator {
        return fail(403, "Você não pode apagar essa mensagem");
    }
    let reported = st
        .db
        .get(
            "SELECT 1 FROM reports WHERE message_id = ?",
            params![ma.m.int("id")],
        )?
        .is_some();
    st.db
        .run("DELETE FROM messages WHERE id = ?", params![ma.m.int("id")])?;
    st.emit_channel(
        &ma.a.c,
        "MESSAGE_DELETE",
        &json!({ "id": ma.m.val("id"), "channel_id": ma.a.c.val("id") }),
        0,
    )?;
    if reported {
        st.sync_server(ma.a.c.int("server_id"))?;
    }
    ok()
}

fn report(st: &mut State, q: &mut Req) -> ApiResult {
    let ma = st.message_for(q.uid, q.param("mid")?)?;
    if ma.a.c.str("type") == "dm" {
        return fail(400, "Em mensagens diretas, use “Bloquear”.");
    }
    if ma.m.int("author_id") == q.uid {
        return fail(400, "Você não pode denunciar a própria mensagem");
    }
    st.limit(&format!("report:{}", q.uid), 20, HOUR)?;
    let given = str(q.get("reason"));
    let label = REPORT_REASONS
        .iter()
        .find(|(k, _)| *k == given)
        .map_or("Outro motivo", |(_, l)| l);
    let detail = truncate(str(q.get("detail")).trim(), DETAIL_LEN);
    let reason = if detail.is_empty() {
        label.to_owned()
    } else {
        format!("{label}: {detail}")
    };
    st.db.run(
        "INSERT OR IGNORE INTO reports(message_id, server_id, reporter_id, reason, created) VALUES(?, ?, ?, ?, ?)",
        params![ma.m.int("id"), ma.a.c.int("server_id"), q.uid, reason, now()],
    )?;
    st.sync_server(ma.a.c.int("server_id"))?;
    ok()
}

fn resolve_report(st: &mut State, q: &mut Req) -> ApiResult {
    let Some(r) = st.db.get(
        "SELECT * FROM reports WHERE id = ?",
        params![q.param("rid")?],
    )?
    else {
        return fail(404, "Denúncia não encontrada");
    };
    let srv = st.server_for(q.uid, r.int("server_id"))?;
    st.need(&srv, q.uid, P_MANAGE_MESSAGES)?;
    st.db
        .run("DELETE FROM reports WHERE id = ?", params![r.int("id")])?;
    st.sync_server(r.int("server_id"))?;
    ok()
}

/// A reaction. To an encrypted message, the path has its mark (the same for
/// everyone who reacts with the same emoji, and meaningless here) and the
/// body `{d}` the emoji, encrypted and signed by whoever reacts. To an old
/// message in plain text, the emoji itself, as before.
fn react(st: &mut State, q: &mut Req, add: bool) -> ApiResult {
    let ma = st.message_for(q.uid, q.param("mid")?)?;
    let emoji = q.raw_param("emoji").to_owned();
    let sealed = !ma.m.str("envelope").is_empty();
    let data = str(q.get("d"));
    if sealed {
        if !crate::e2e::is_b64(&emoji, 16) || (add && !crate::e2e::is_b64_text(&data, 2048)) {
            return fail(400, "Reação inválida");
        }
    } else if emoji.is_empty() || js_len(&emoji) > 16 {
        return fail(400, "Emoji inválido");
    }
    if add {
        st.db.run(
            "INSERT INTO reactions(message_id, user_id, emoji, data) VALUES(?, ?, ?, ?)
             ON CONFLICT(message_id, user_id, emoji) DO UPDATE SET data = excluded.data",
            params![
                ma.m.int("id"),
                q.uid,
                emoji,
                if sealed { json!(data) } else { Value::Null }
            ],
        )?;
    } else {
        st.db.run(
            "DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?",
            params![ma.m.int("id"), q.uid, emoji],
        )?;
    }
    let m = st.serialize(&ma.m)?;
    st.emit_channel(&ma.a.c, "MESSAGE_UPDATE", &m, 0)?;
    ok()
}

fn add_reaction(st: &mut State, q: &mut Req) -> ApiResult {
    react(st, q, true)
}

fn remove_reaction(st: &mut State, q: &mut Req) -> ApiResult {
    react(st, q, false)
}

// ---------------------------------------------------------------- friends and direct messages

fn add_friend(st: &mut State, q: &mut Req) -> ApiResult {
    let lower = to_lower(str(q.get("username")).trim());
    let name = lower.strip_prefix('@').unwrap_or(&lower);
    let Some(target) = st
        .db
        .get("SELECT * FROM users WHERE username = ?", params![name])?
    else {
        return fail(404, "Não encontrei ninguém com esse nome de usuário");
    };
    let tid = target.int("id");
    if tid == q.uid {
        return fail(400, "Você não pode adicionar a si mesmo");
    }
    if st.blocked_between(q.uid, tid)? {
        return fail(403, "Não foi possível enviar o pedido de amizade");
    }
    st.limit(&format!("friend:{}", q.uid), 20, HOUR)?;
    let (a, b) = pair(q.uid, tid);
    let existing = st
        .db
        .get("SELECT * FROM friends WHERE a = ? AND b = ?", params![a, b])?;
    match &existing {
        Some(f) if f.str("status") == "accepted" => return fail(400, "Vocês já são amigos"),
        Some(f) if f.int("requester") == q.uid => return fail(400, "Pedido já enviado"),
        Some(_) => {
            st.db.run(
                "UPDATE friends SET status = 'accepted' WHERE a = ? AND b = ?",
                params![a, b],
            )?;
        }
        None => {
            st.db.run(
                "INSERT INTO friends(a, b, requester, status) VALUES(?, ?, ?, 'pending')",
                params![a, b, q.uid],
            )?;
        }
    }
    st.emit_friend(q.uid, tid)?;
    st.emit_friend(tid, q.uid)?;
    ok()
}

fn accept_friend(st: &mut State, q: &mut Req) -> ApiResult {
    let other = q.param("uid")?;
    let (a, b) = pair(q.uid, other);
    let f = st
        .db
        .get("SELECT * FROM friends WHERE a = ? AND b = ?", params![a, b])?;
    if !f.is_some_and(|f| f.str("status") == "pending" && f.int("requester") != q.uid) {
        return fail(404, "Pedido não encontrado");
    }
    st.db.run(
        "UPDATE friends SET status = 'accepted' WHERE a = ? AND b = ?",
        params![a, b],
    )?;
    st.emit_friend(q.uid, other)?;
    st.emit_friend(other, q.uid)?;
    ok()
}

fn remove_friend(st: &mut State, q: &mut Req) -> ApiResult {
    let other = q.param("uid")?;
    let (a, b) = pair(q.uid, other);
    st.db
        .run("DELETE FROM friends WHERE a = ? AND b = ?", params![a, b])?;
    st.emit_friend(q.uid, other)?;
    st.emit_friend(other, q.uid)?;
    ok()
}

fn open_dm(st: &mut State, q: &mut Req) -> ApiResult {
    let other = to_int(q.get("userId"))?;
    if other == q.uid || st.user_by_id(other)?.is_none() {
        return fail(404, "Usuário não encontrado");
    }
    if let Some(e) = st.db.get(
        "SELECT d1.channel_id AS id FROM dm_members d1 JOIN dm_members d2 ON d1.channel_id = d2.channel_id
          WHERE d1.user_id = ? AND d2.user_id = ?",
        params![q.uid, other],
    )? {
        return st.dm_snapshot(e.int("id"), q.uid);
    }
    if !st.related(q.uid)?.contains(&other) {
        return fail(403, "Vocês precisam ser amigos ou dividir um servidor");
    }
    if st.blocked_between(q.uid, other)? {
        return fail(403, "Não é possível conversar com essa pessoa");
    }
    st.limit(&format!("dm:{}", q.uid), 30, HOUR)?;
    let uid = q.uid;
    let cid = st.db.tx(|| {
        let cid = st
            .db
            .insert("INSERT INTO channels(type) VALUES('dm')", params![])?;
        st.db.run(
            "INSERT INTO dm_members(channel_id, user_id) VALUES(?, ?), (?, ?)",
            params![cid, uid, cid, other],
        )?;
        Ok(cid)
    })?;
    st.send(uid, "DM_CREATE", &st.dm_snapshot(cid, uid)?);
    st.send(other, "DM_CREATE", &st.dm_snapshot(cid, other)?);
    st.dm_snapshot(cid, uid)
}

/// Blocking hides the other person's messages for you, ends any friendship
/// and stops direct messages and friend requests in both directions.
fn block(st: &mut State, q: &mut Req) -> ApiResult {
    let other = q.param("uid")?;
    if other == q.uid || st.user_by_id(other)?.is_none() {
        return fail(404, "Usuário não encontrado");
    }
    st.db.run(
        "INSERT OR IGNORE INTO blocks(user_id, blocked_id) VALUES(?, ?)",
        params![q.uid, other],
    )?;
    let (a, b) = pair(q.uid, other);
    st.db
        .run("DELETE FROM friends WHERE a = ? AND b = ?", params![a, b])?;
    st.emit_friend(q.uid, other)?;
    st.emit_friend(other, q.uid)?;
    st.send(q.uid, "BLOCKS", &json!({ "ids": st.block_list(q.uid)? }));
    ok()
}

fn unblock(st: &mut State, q: &mut Req) -> ApiResult {
    st.db.run(
        "DELETE FROM blocks WHERE user_id = ? AND blocked_id = ?",
        params![q.uid, q.param("uid")?],
    )?;
    st.send(q.uid, "BLOCKS", &json!({ "ids": st.block_list(q.uid)? }));
    ok()
}

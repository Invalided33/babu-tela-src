//! End-to-end encryption, the server's part: keeping and handing out what
//! the apps cannot do without each other, none of which it can open.
//!
//! - Each account's public keys (`e2e_keys`), as its app published them.
//!   Their signature is the apps' to check, against the identity each app
//!   pinned the first time it saw someone: a key the server swaps shows up
//!   there (crates/app/src/e2e/keys.rs).
//! - Who reads a conversation (the readers of a channel), so a sender seals
//!   its session key for each of them.
//! - Those sealed keys (`e2e_sessions`), one row per session and reader.
//! - The step in which a new PC of an account gets the keys from an old
//!   one (the gateway's `provision` op), which only passes between the
//!   account's own connections.
//! - The public record (`e2e_log`): every key published, and every
//!   version of the app, in order, which every app keeps a copy of. An app
//!   takes no key that is not the newest of its account there, watches the
//!   entries of its own account, and compares its copy with everyone it
//!   talks to (crates/app/src/e2e/log.rs): a key slipped in for one person
//!   alone shows.
//!
//! Messages themselves keep their envelope in `messages.envelope`
//! (routes.rs).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::db::{Db, RowExt, ids};
use crate::model::P_VIEW;
use crate::params;
use crate::routes::Req;
use crate::state::State;
use crate::util::*;

// ---------------------------------------------------------------- the public record

/// The labels the apps hash entries with (crates/app/src/e2e/log.rs).
const DS_KEY: &[u8] = b"babutela/e2e/v1/registro/chave";
const DS_RELEASE: &[u8] = b"babutela/e2e/v1/registro/versao";
/// Entries handed out at once.
const LOG_PAGE: i64 = 2000;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A bundle as an entry of the record: the hash the apps compute over its
/// account, identity, X-Wing key and time (None for one that is not whole).
pub fn key_leaf(uid: i64, bundle: &Value) -> Option<String> {
    let identity = URL_SAFE_NO_PAD.decode(bundle.get("id")?.as_str()?).ok()?;
    let kem = URL_SAFE_NO_PAD.decode(bundle.get("kem")?.as_str()?).ok()?;
    let at = bundle.get("at")?.as_i64()?;
    if identity.len() != 32 || kem.len() != 1216 {
        return None;
    }
    let mut h = Sha256::new();
    h.update(DS_KEY);
    h.update(uid.to_be_bytes());
    h.update(&identity);
    h.update(&kem);
    h.update(at.to_be_bytes());
    Some(b64(&h.finalize()))
}

/// A version of the app as an entry (what the update key signs).
fn release_leaf(version: &str, file: &str, sha256: &str) -> String {
    let sha = sha256.to_ascii_lowercase();
    let mut h = Sha256::new();
    h.update(DS_RELEASE);
    for part in [version, file, sha.as_str()] {
        h.update((part.len() as u32).to_be_bytes());
        h.update(part.as_bytes());
    }
    b64(&h.finalize())
}

/// Adds an entry at the end. Nothing ever changes or takes one away: the
/// apps keep a copy, and an entry that changed would show at once.
fn append(db: &Db, kind: &str, uid: i64, leaf: &str, data: Option<&Value>) -> ApiResult<()> {
    let seq = db
        .get(
            "SELECT COALESCE(MAX(seq), 0) + 1 AS n FROM e2e_log",
            params![],
        )?
        .map_or(1, |r| r.int("n"));
    db.run(
        "INSERT INTO e2e_log(seq, kind, user_id, leaf, at, statement) VALUES(?, ?, ?, ?, ?, ?)",
        params![seq, kind, uid, leaf, now(), data.map(Value::to_string)],
    )?;
    Ok(())
}

/// Whether `uid`'s newest key entry is `leaf`.
fn newest_is(db: &Db, uid: i64, leaf: &str) -> ApiResult<bool> {
    Ok(db
        .get(
            "SELECT leaf FROM e2e_log WHERE kind = 'k' AND user_id = ? ORDER BY seq DESC LIMIT 1",
            params![uid],
        )?
        .is_some_and(|r| r.str("leaf") == leaf))
}

/// The keys published before the record existed go into it, once (when the
/// schema that brought it runs).
pub fn backfill_log(db: &Db) -> ApiResult<()> {
    for r in db.all(
        "SELECT user_id, bundle FROM e2e_keys ORDER BY updated, user_id",
        params![],
    )? {
        let uid = r.int("user_id");
        let Some(leaf) = serde_json::from_str::<Value>(r.str("bundle"))
            .ok()
            .and_then(|b| key_leaf(uid, &b))
        else {
            continue;
        };
        if !newest_is(db, uid, &leaf)? {
            append(db, "k", uid, &leaf, None)?;
        }
    }
    Ok(())
}

/// GET /api/e2e/log?from=n: the entries after the first n, in order, and
/// how many there are.
pub fn get_log(st: &mut State, q: &mut Req) -> ApiResult {
    let from = match q.query("from") {
        "" => 0,
        s => param_int(s)?,
    };
    let mut entries = Vec::new();
    for r in st.db.all(
        "SELECT seq, kind, user_id, leaf, at FROM e2e_log WHERE seq > ? ORDER BY seq LIMIT ?",
        params![from, LOG_PAGE],
    )? {
        entries.push(json!([
            r.int("seq"),
            r.str("kind"),
            r.int("user_id"),
            r.str("leaf"),
            r.int("at")
        ]));
    }
    let size = st
        .db
        .get("SELECT COALESCE(MAX(seq), 0) AS n FROM e2e_log", params![])?
        .map_or(0, |r| r.int("n"));
    Ok(json!({ "entries": entries, "size": size }))
}

/// The proof that a version comes from `cargo xtask publicar`: an HMAC of
/// its entry with PEPPER, which only the Worker and the publisher have.
fn publisher_mac(pepper: &str, leaf: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("qualquer tamanho");
    mac.update(b"publicar:");
    mac.update(leaf.as_bytes());
    b64(&mac.finalize().into_bytes())
}

/// POST /api/e2e/log/release {version, file, sha256, mac}: a version of the
/// app into the record, from `cargo xtask publicar` (`mac`, see
/// `publisher_mac`). The apps install nothing that is not there, and check
/// its signatures themselves.
pub fn put_release(st: &mut State, q: &mut Req) -> ApiResult {
    use subtle::ConstantTimeEq;
    let (version, file, sha) = (
        str(q.get("version")),
        str(q.get("file")),
        str(q.get("sha256")),
    );
    let leaf = release_leaf(&version, &file, &sha);
    let pepper = st.var("PEPPER");
    let want = publisher_mac(&pepper, &leaf);
    if pepper.is_empty() || !bool::from(want.as_bytes().ct_eq(str(q.get("mac")).as_bytes())) {
        return fail(403, "Só quem publica as versões registra uma");
    }
    let plain = |s: &str, extra: &[u8]| {
        !s.is_empty()
            && s.len() <= 100
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
    };
    if !plain(&version, b".")
        || !plain(&file, b"._- ")
        || sha.len() != 64
        || !sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return fail(400, "Versão inválida");
    }
    let known = st
        .db
        .get(
            "SELECT 1 FROM e2e_log WHERE kind = 'r' AND leaf = ?",
            params![leaf],
        )?
        .is_some();
    if !known {
        let data = json!({ "version": version, "file": file, "sha256": sha.to_ascii_lowercase() });
        append(&st.db, "r", 0, &leaf, Some(&data))?;
    }
    Ok(json!({}))
}

/// A published bundle is small (an X-Wing key is 1216 bytes, 1622 in
/// base64): anything much bigger is not one.
const MAX_BUNDLE: usize = 4096;
/// A session key sealed for one reader: 8 + 1120 + 32 + 16 bytes, base64.
const MAX_SEALED: usize = 2048;
/// Ids asked for at once.
const MAX_IDS: usize = 200;
/// The keys an old PC seals for a new one: the account's and the identities
/// it pinned, up to 128 KB before sealing (crates/app/src/e2e/mod.rs,
/// `EXPORT_MAX`); under the gateway's 256 KB frames.
const PROVISION_MAX: usize = 192 * 1024;

/// Base64 without padding (URL-safe), of exactly `bytes` bytes.
pub fn is_b64(s: &str, bytes: usize) -> bool {
    s.len() == (bytes * 4).div_ceil(3)
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Base64 without padding (URL-safe), of any length up to `max` characters.
pub fn is_b64_text(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `v` has the shape of a bundle for `uid` (the signature is the
/// apps' to check).
fn valid_bundle(uid: i64, v: &Value) -> bool {
    v.get("v").and_then(Value::as_i64) == Some(1)
        && v.get("uid").and_then(Value::as_i64) == Some(uid)
        && is_b64(&str(v.get("id")), 32)
        && is_b64(&str(v.get("kem")), 1216)
        && is_b64(&str(v.get("sig")), 64)
        && v.get("at").and_then(Value::as_i64).is_some()
        && v.to_string().len() <= MAX_BUNDLE
}

/// PUT /api/e2e/keys {keys}: my public keys. A different identity than
/// before goes to everyone who shares something with me, whose apps then
/// warn that it changed.
pub fn put_keys(st: &mut State, q: &mut Req) -> ApiResult {
    st.limit(&format!("e2e-keys:{}", q.uid), 20, 3_600_000)?;
    let bundle = q.get("keys").cloned().unwrap_or(Value::Null);
    if !valid_bundle(q.uid, &bundle) {
        return fail(400, "Chaves inválidas");
    }
    let before = st
        .db
        .get(
            "SELECT bundle FROM e2e_keys WHERE user_id = ?",
            params![q.uid],
        )?
        .and_then(|r| serde_json::from_str::<Value>(r.str("bundle")).ok());
    st.db.run(
        "INSERT INTO e2e_keys(user_id, bundle, updated) VALUES(?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET bundle = excluded.bundle, updated = excluded.updated",
        params![q.uid, bundle.to_string(), now()],
    )?;
    // Into the public record, unless it is the key the record has for me.
    if let Some(leaf) = key_leaf(q.uid, &bundle)
        && !newest_is(&st.db, q.uid, &leaf)?
    {
        append(&st.db, "k", q.uid, &leaf, None)?;
    }
    if before.as_ref() != Some(&bundle) {
        let d = json!({ "userId": q.uid });
        for uid in st.related(q.uid)? {
            st.send(uid, "KEYS_CHANGED", &d);
        }
        st.send(q.uid, "KEYS_CHANGED", &d);
    }
    Ok(json!({}))
}

fn id_list(s: &str) -> ApiResult<Vec<i64>> {
    let list: Vec<i64> = s
        .split(',')
        .filter(|p| !p.is_empty())
        .map(param_int)
        .collect::<ApiResult<_>>()?;
    if list.len() > MAX_IDS {
        return fail(400, "Ids demais de uma vez");
    }
    Ok(list)
}

fn bundle_of(st: &State, uid: i64) -> ApiResult<Value> {
    Ok(st
        .db
        .get(
            "SELECT bundle FROM e2e_keys WHERE user_id = ?",
            params![uid],
        )?
        .and_then(|r| serde_json::from_str::<Value>(r.str("bundle")).ok())
        .unwrap_or(Value::Null))
}

/// GET /api/e2e/keys?ids=1,2: the public keys of people I share something
/// with (and mine).
pub fn get_keys(st: &mut State, q: &mut Req) -> ApiResult {
    let wanted = id_list(q.query("ids"))?;
    let related: std::collections::HashSet<i64> = st.related(q.uid)?.into_iter().collect();
    let mut out = Map::new();
    for uid in wanted {
        if uid == q.uid || related.contains(&uid) {
            out.insert(uid.to_string(), bundle_of(st, uid)?);
        }
    }
    Ok(json!({ "keys": out }))
}

/// Who can read channel `cid` now: the two people of a direct
/// conversation, or the members of the server who see the channel.
pub fn reader_ids(st: &State, c: &crate::db::Row) -> ApiResult<Vec<i64>> {
    if c.str("type") == "dm" {
        return Ok(ids(
            &st.db.all(
                "SELECT user_id FROM dm_members WHERE channel_id = ?",
                params![c.int("id")],
            )?,
            "user_id",
        ));
    }
    let srv = st
        .db
        .get(
            "SELECT * FROM servers WHERE id = ?",
            params![c.int("server_id")],
        )?
        .unwrap_or_default();
    let mut out = Vec::new();
    for uid in ids(
        &st.db.all(
            "SELECT user_id FROM members WHERE server_id = ?",
            params![c.int("server_id")],
        )?,
        "user_id",
    ) {
        if st.perms_for(&srv, uid, c.int("id"))? & P_VIEW != 0 {
            out.push(uid);
        }
    }
    Ok(out)
}

/// GET /api/channels/{cid}/e2e/readers: each reader with their public keys
/// (null for one whose app has none yet), and, in a conversation whose
/// messages disappear, their key of the day (`eph`, see `put_prekeys`).
pub fn readers(st: &mut State, q: &mut Req) -> ApiResult {
    let a = st.access(q.uid, q.param("cid")?)?;
    let ttl = a.c.int("ttl") > 0;
    let today = now() / DAY_MS;
    let mut out = Vec::new();
    for uid in reader_ids(st, &a.c)? {
        let mut r = json!({ "id": uid, "keys": bundle_of(st, uid)? });
        if ttl {
            // Today's, or the newest before it; tomorrow's (published ahead,
            // for clocks a little apart) only when there is none: the
            // sooner the day of the key, the sooner it is gone.
            r["eph"] = st
                .db
                .get(
                    "SELECT prekey FROM e2e_prekeys WHERE user_id = ? AND day <= ? AND day >= ?
                      ORDER BY day > ?, day DESC LIMIT 1",
                    params![uid, today + 1, today - PREKEY_DAYS, today],
                )?
                .and_then(|r| serde_json::from_str::<Value>(r.str("prekey")).ok())
                .unwrap_or(Value::Null);
        }
        out.push(r);
    }
    Ok(json!({ "readers": out }))
}

const DAY_MS: i64 = 86_400_000;
/// How many days back a key of the day is still handed out (the longest a
/// message lasts, 7 days; the apps keep the secret one day more).
pub const PREKEY_DAYS: i64 = 7;

/// PUT /api/e2e/prekeys {prekeys: [{d, kem, sig}]}: my X-Wing keys of these
/// days, each signed by my identity (the apps check it). The old ones go.
pub fn put_prekeys(st: &mut State, q: &mut Req) -> ApiResult {
    st.limit(&format!("e2e-prekeys:{}", q.uid), 50, 86_400_000)?;
    let Some(Value::Array(list)) = q.get("prekeys").cloned() else {
        return fail(400, "Chaves inválidas");
    };
    let today = now() / DAY_MS;
    if list.len() > 4 {
        return fail(400, "Chaves demais de uma vez");
    }
    for p in &list {
        let day = p.get("d").and_then(Value::as_i64).unwrap_or(0);
        if (day - today).abs() > 2
            || !is_b64(&str(p.get("kem")), 1216)
            || !is_b64(&str(p.get("sig")), 64)
            || p.to_string().len() > MAX_BUNDLE
        {
            return fail(400, "Chave do dia inválida");
        }
        st.db.run(
            "INSERT INTO e2e_prekeys(user_id, day, prekey) VALUES(?, ?, ?)
             ON CONFLICT(user_id, day) DO UPDATE SET prekey = excluded.prekey",
            params![q.uid, day, p.to_string()],
        )?;
    }
    st.db.run(
        "DELETE FROM e2e_prekeys WHERE user_id = ? AND day < ?",
        params![q.uid, today - PREKEY_DAYS - 1],
    )?;
    Ok(json!({}))
}

/// POST /api/channels/{cid}/e2e/sessions {sid, keys: {uid: sealed}}: a new
/// session of mine, its key sealed for each reader. Only readers' copies
/// are kept, and a session id belongs to whoever made it first.
pub fn put_session(st: &mut State, q: &mut Req) -> ApiResult {
    let a = st.access(q.uid, q.param("cid")?)?;
    st.limit(&format!("e2e-session:{}", q.uid), 120, 3_600_000)?;
    let sid = str(q.get("sid"));
    if !is_b64(&sid, 16) {
        return fail(400, "Sessão inválida");
    }
    if let Some(r) = st.db.get(
        "SELECT sender_id, channel_id FROM e2e_sessions WHERE sid = ? LIMIT 1",
        params![sid],
    )? && (r.int("sender_id") != q.uid || r.int("channel_id") != a.c.int("id"))
    {
        return fail(409, "Essa sessão é de outra pessoa");
    }
    let readers: std::collections::HashSet<i64> = reader_ids(st, &a.c)?.into_iter().collect();
    let Some(Value::Object(keys)) = q.get("keys").cloned() else {
        return fail(400, "Sessão inválida");
    };
    let created = now();
    for (uid, sealed) in keys {
        let Ok(uid) = param_int(&uid) else {
            continue;
        };
        let sealed = str(Some(&sealed));
        if !readers.contains(&uid) || !is_b64_text(&sealed, MAX_SEALED) {
            continue;
        }
        st.db.run(
            "INSERT OR REPLACE INTO e2e_sessions(sid, user_id, channel_id, sender_id, created, sealed)
             VALUES(?, ?, ?, ?, ?, ?)",
            params![sid, uid, a.c.int("id"), q.uid, created, sealed],
        )?;
    }
    Ok(json!({}))
}

/// GET /api/e2e/sessions?ids=a,b: the keys of these sessions sealed for me,
/// in conversations I still read.
pub fn get_sessions(st: &mut State, q: &mut Req) -> ApiResult {
    let wanted: Vec<String> = q
        .query("ids")
        .split(',')
        .filter(|s| is_b64(s, 16))
        .map(str::to_owned)
        .collect();
    if wanted.len() > MAX_IDS {
        return fail(400, "Ids demais de uma vez");
    }
    let mut out = Vec::new();
    let mut allowed = std::collections::HashMap::new();
    for sid in wanted {
        let Some(r) = st.db.get(
            "SELECT * FROM e2e_sessions WHERE sid = ? AND user_id = ?",
            params![sid, q.uid],
        )?
        else {
            continue;
        };
        let cid = r.int("channel_id");
        let ok = *allowed
            .entry(cid)
            .or_insert_with(|| st.access(q.uid, cid).is_ok());
        if ok {
            out.push(json!({
                "sid": sid, "cid": cid, "sender": r.int("sender_id"), "key": r.str("sealed"),
            }));
        }
    }
    Ok(json!({ "sessions": out }))
}

// ---------------------------------------------------------------- who is a member

/// Certificates kept per member (the newest), and the size of one.
const MAX_CERTS: usize = 4;
const MAX_CERT: usize = 2048;

/// Whether `c` has the shape of a certificate that member `uid` is in
/// server `sid` (crates/app/src/e2e/members.rs; its signatures are the
/// apps' to check).
fn valid_cert(sid: i64, uid: i64, c: &Value) -> bool {
    c.get("s").and_then(Value::as_i64) == Some(sid)
        && c.get("u").and_then(Value::as_i64) == Some(uid)
        && c.get("by").and_then(Value::as_i64).is_some()
        && c.get("t").and_then(Value::as_i64).is_some()
        && matches!(
            c.get("how").and_then(Value::as_str),
            Some("dono" | "convite" | "aceito" | "garantia")
        )
        && is_b64(&str(c.get("sig")), 64)
        && c.to_string().len() <= MAX_CERT
}

/// Whether `c` has the shape of an invite's certificate: its creator's
/// signature over the server, the code, the invite's public key and the
/// server's owner (whom whoever comes in pins from it).
pub fn valid_invite_cert(sid: i64, code: &str, by: i64, owner: i64, c: &Value) -> bool {
    c.get("s").and_then(Value::as_i64) == Some(sid)
        && c.get("code").and_then(Value::as_str) == Some(code)
        && c.get("by").and_then(Value::as_i64) == Some(by)
        && c.get("t").and_then(Value::as_i64).is_some()
        && c.get("o").and_then(Value::as_i64) == Some(owner)
        && is_b64(&str(c.get("ik")), 32)
        && is_b64(&str(c.get("sig")), 64)
        && c.to_string().len() <= MAX_CERT
}

/// A certificate for one of the server's members, kept with the newest
/// few of theirs (`certs` in the member's row).
fn add_cert(db: &Db, sid: i64, uid: i64, cert: &Value) -> ApiResult<()> {
    let Some(row) = db.get(
        "SELECT certs FROM members WHERE server_id = ? AND user_id = ?",
        params![sid, uid],
    )?
    else {
        return fail(404, "Membro não encontrado");
    };
    let mut list: Vec<Value> = serde_json::from_str(row.str("certs")).unwrap_or_default();
    if list.contains(cert) {
        return Ok(());
    }
    list.push(cert.clone());
    if list.len() > MAX_CERTS {
        list.remove(0);
    }
    db.run(
        "UPDATE members SET certs = ? WHERE server_id = ? AND user_id = ?",
        params![Value::Array(list).to_string(), sid, uid],
    )?;
    Ok(())
}

/// A member's certificates, for the server's snapshot.
pub fn certs_of(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_array)
        .unwrap_or_else(|| json!([]))
}

/// The certificate an approver sends with "Aceitar", if it is one.
pub fn approval(sid: i64, target: i64, approver: i64, c: Option<&Value>) -> Option<String> {
    let c = c?;
    (valid_cert(sid, target, c)
        && c.get("by").and_then(Value::as_i64) == Some(approver)
        && c.get("how").and_then(Value::as_str) == Some("aceito"))
    .then(|| json!([c]).to_string())
}

/// POST /api/servers/{sid}/e2e/certs {certs}: certificates I signed (the
/// owner's own, a member I vouch for), or my own entry through an invite
/// (signed with the key its link carries).
pub fn put_certs(st: &mut State, q: &mut Req) -> ApiResult {
    let srv = st.server_for(q.uid, q.param("sid")?)?;
    let sid = srv.int("id");
    st.limit(&format!("e2e-certs:{}", q.uid), 120, 3_600_000)?;
    let Some(Value::Array(certs)) = q.get("certs").cloned() else {
        return fail(400, "Certificados inválidos");
    };
    if certs.len() > 500 {
        return fail(400, "Certificados demais de uma vez");
    }
    for c in &certs {
        let uid = c.get("u").and_then(Value::as_i64).unwrap_or(0);
        let by = c.get("by").and_then(Value::as_i64).unwrap_or(0);
        let mine = if str(c.get("how")) == "convite" {
            uid == q.uid
        } else {
            by == q.uid
        };
        if !mine || !valid_cert(sid, uid, c) {
            return fail(400, "Certificado inválido");
        }
        add_cert(&st.db, sid, uid, c)?;
    }
    st.sync_server(sid)?;
    Ok(json!({}))
}

/// The gateway's `provision` op: `{data}` goes to my other connections,
/// `{to, data}` to that one of mine. Never to anyone else's.
pub fn provision(st: &mut State, me: &crate::state::Sock, m: &Map<String, Value>) -> ApiResult<()> {
    if !st.hit(&format!("provision:{}", me.uid), 30, 60_000) {
        return Ok(());
    }
    let data = m.get("data").cloned().unwrap_or(Value::Null);
    if !data.is_object() || data.to_string().len() > PROVISION_MAX {
        return fail(400, "Pedido inválido");
    }
    let frame = crate::model::event("PROVISION", &json!({ "from": me.id, "data": data }));
    let mine: Vec<u64> = st
        .conns_of(me.uid)
        .into_iter()
        .map(|s| s.id)
        .filter(|id| *id != me.id)
        .collect();
    let to: Vec<u64> = match m.get("to").and_then(Value::as_u64) {
        Some(to) => mine.into_iter().filter(|id| *id == to).collect(),
        None => mine,
    };
    if !to.is_empty() {
        st.send_to(&to, &frame);
    }
    Ok(())
}

//! Messages: sending sessions, envelopes and reactions.
//!
//! Each PC has a sending session per conversation: a random 256-bit key,
//! sealed (seal.rs) for every account that can read the conversation, mine
//! included (my other PCs read what I sent). A message is encrypted under a
//! key derived from the session's and its own random id, and signed inside
//! by my identity over the conversation, the session, its id and the body:
//! the server can neither read it nor change it, move it to another
//! conversation, put it under someone else's name, or make one up.
//!
//! What the server sees of a message: the session id, the message's random
//! id and the encrypted bytes, padded so their length says little.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::keys::Account;
use super::prim;

const DS_SESSION: &[u8] = b"babutela/e2e/v1/sessao";
const DS_MSG: &[u8] = b"babutela/e2e/v1/mensagem";
const DS_MSG_KEY: &[u8] = b"babutela/e2e/v1/chave-mensagem";
const DS_REACT: &[u8] = b"babutela/e2e/v1/reacao";
const DS_REACT_KEY: &[u8] = b"babutela/e2e/v1/chave-reacao";
const DS_REACT_TOKEN: &[u8] = b"babutela/e2e/v1/marca-reacao";

/// The context a session key is sealed in: the conversation and the session.
pub fn session_context(cid: i64, sid: &[u8; 16]) -> Vec<u8> {
    [DS_SESSION, &cid.to_be_bytes(), sid].concat()
}

/// A file on a message, as only the conversation sees it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRef {
    /// Where its encrypted bytes are ("/uploads/…"); empty for a file sent
    /// directly.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub u: String,
    /// Its name.
    #[serde(default)]
    pub n: String,
    /// Its size, before encryption.
    #[serde(default)]
    pub z: i64,
    /// image, video, audio, file or direct.
    #[serde(default)]
    pub k: String,
    /// The key of its bytes (file.rs), for one on the server.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fk: String,
    /// A file sent directly: its id, its pieces' root hash and piece size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<crate::model::DirectInfo>,
}

/// The message answered, as the sender quoted it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Quote {
    /// The server's id of the message answered.
    pub id: i64,
    /// Its author.
    pub u: i64,
    /// The start of its text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub x: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub st: Option<String>,
}

/// An edit: of which message (the server's id and the original's random id).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditOf {
    pub id: i64,
    pub n: String,
}

/// What a message says, encrypted and signed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Body {
    /// When the sender wrote it (ms), signed.
    pub ts: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub t: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub st: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<Quote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ed: Option<EditOf>,
    /// The sender's copy of the public record of keys, as it was
    /// (record.rs), for the readers to compare with theirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<super::record::Head>,
    /// When it disappears (ms), in a conversation whose messages do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i64>,
}

/// My sending session in one conversation.
#[derive(Clone, Serialize, Deserialize)]
pub struct OutSession {
    #[serde(with = "b64_16")]
    pub sid: [u8; 16],
    #[serde(with = "super::keys::b64_32")]
    pub key: [u8; 32],
    pub cid: i64,
    /// When it was made (ms), and how many messages went with it.
    pub at: i64,
    #[serde(default)]
    pub sent: u32,
    /// The accounts it was sealed for, and the X-Wing key id of each:
    /// someone in or out, or a key changed, means a new session.
    pub readers: Vec<(i64, String)>,
    /// How long the conversation's messages last (s), when they disappear.
    #[serde(default)]
    pub ttl: i64,
}

/// A session someone (maybe me, on another PC) sends with.
#[derive(Clone, Serialize, Deserialize)]
pub struct InSession {
    #[serde(with = "super::keys::b64_32")]
    pub key: [u8; 32],
    pub cid: i64,
    pub sender: i64,
    /// When this PC got it (ms): in a conversation whose messages
    /// disappear, it goes a while after.
    #[serde(default)]
    pub at: i64,
    /// Sealed for a key of the day (a conversation whose messages
    /// disappear): dropped once nothing it carried can still be shown.
    #[serde(default)]
    pub eph: bool,
}

/// A session lasts at most this long, and this many messages; in a
/// conversation whose messages disappear, a day (its key is dropped soon
/// after its messages go, mod.rs `day`).
pub const SESSION_DAYS: i64 = 7;
pub const SESSION_MESSAGES: u32 = 2000;

impl OutSession {
    pub fn new(cid: i64, readers: Vec<(i64, String)>, now: i64) -> OutSession {
        OutSession {
            sid: prim::random(),
            key: prim::random(),
            cid,
            at: now,
            sent: 0,
            readers,
            ttl: 0,
        }
    }

    /// Whether this session can still go to exactly these readers, in a
    /// conversation whose messages last `ttl` seconds (0: for good).
    pub fn fits(&self, readers: &[(i64, String)], now: i64, ttl: i64) -> bool {
        let days = if ttl > 0 { 1 } else { SESSION_DAYS };
        self.readers == readers
            && self.ttl == ttl
            && self.sent < SESSION_MESSAGES
            && now - self.at < days * 86_400_000
    }
}

fn message_key(key: &[u8; 32], cid: i64, sid: &[u8; 16], mid: &[u8; 16]) -> [u8; 32] {
    prim::key32(mid, key, &[DS_MSG_KEY, &cid.to_be_bytes(), sid])
}

fn header(cid: i64, sid: &[u8; 16], mid: &[u8; 16]) -> Vec<u8> {
    [DS_MSG, &cid.to_be_bytes(), sid, mid].concat()
}

fn to_sign(cid: i64, sid: &[u8; 16], mid: &[u8; 16], uid: i64, body: &[u8]) -> Vec<u8> {
    [&header(cid, sid, mid)[..], &uid.to_be_bytes(), body].concat()
}

/// Padmé (Nikitin et al., "Reducing Metadata Leakage from Encrypted Files
/// and Communication with PURBs"): rounds a length up so that it leaks
/// O(log log n) bits, with at most 12 % more bytes.
pub fn padme(len: u64) -> u64 {
    if len < 2 {
        return len;
    }
    let e = 63 - u64::from(len.leading_zeros());
    let s = 64 - u64::from(e.leading_zeros());
    let last = e - s;
    let mask = (1u64 << last) - 1;
    (len + mask) & !mask
}

/// The length a message body is padded to: at least 512 bytes, so the
/// short ones, most of a chat (up to some 300 characters of text), all
/// look alike to the server.
fn padded(len: usize) -> usize {
    (padme(len as u64) as usize).max(512)
}

/// A message, sealed with my session: the envelope the server keeps.
pub fn seal_message(me: &Account, s: &OutSession, body: &Body) -> (Value, [u8; 16]) {
    let mid: [u8; 16] = prim::random();
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let sig = me
        .identity
        .sign(&to_sign(s.cid, &s.sid, &mid, me.uid, &bytes));
    let mut plain = Vec::with_capacity(padded(68 + bytes.len()));
    plain.extend_from_slice(&sig);
    plain.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    plain.extend_from_slice(&bytes);
    plain.resize(padded(plain.len()), 0);
    let key = message_key(&s.key, s.cid, &s.sid, &mid);
    prim::seal(&key, [0; 12], &header(s.cid, &s.sid, &mid), &mut plain);
    let env = json!({
        "v": 1,
        "s": prim::b64(&s.sid),
        "n": prim::b64(&mid),
        "c": prim::b64(&plain),
    });
    (env, mid)
}

/// An envelope's session and message ids, as the server keeps them.
pub fn envelope_ids(env: &Value) -> Option<([u8; 16], [u8; 16])> {
    if env.get("v")?.as_i64()? != 1 {
        return None;
    }
    Some((
        prim::unb64_n(env.get("s")?.as_str()?)?,
        prim::unb64_n(env.get("n")?.as_str()?)?,
    ))
}

/// Why a message does not open.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not an envelope this app makes.
    Malformed,
    /// The session is someone else's than the message's author, or another
    /// conversation's.
    WrongSession,
    /// The bytes were changed, or moved here from elsewhere.
    Tampered,
    /// The signature is not the author's.
    NotSigned,
    /// The same envelope already came in another message.
    Replayed,
}

/// The body of an envelope found in conversation `cid` under `author`'s
/// name, with the session key it says. `identity` is the author's identity
/// as this PC pinned it.
pub fn open_message(
    env: &Value,
    cid: i64,
    author: i64,
    session: &InSession,
    identity: &[u8; 32],
) -> Result<Body, Refused> {
    let (sid, mid) = envelope_ids(env).ok_or(Refused::Malformed)?;
    if session.cid != cid || session.sender != author {
        return Err(Refused::WrongSession);
    }
    let ct = env
        .get("c")
        .and_then(Value::as_str)
        .and_then(prim::unb64)
        .ok_or(Refused::Malformed)?;
    let key = message_key(&session.key, cid, &sid, &mid);
    let plain =
        prim::open(&key, [0; 12], &header(cid, &sid, &mid), &ct).ok_or(Refused::Tampered)?;
    if plain.len() < 68 {
        return Err(Refused::Malformed);
    }
    let len = u32::from_be_bytes(plain[64..68].try_into().unwrap_or_default()) as usize;
    let bytes = plain.get(68..68 + len).ok_or(Refused::Malformed)?;
    if !prim::verify(
        identity,
        &to_sign(cid, &sid, &mid, author, bytes),
        &plain[..64],
    ) {
        return Err(Refused::NotSigned);
    }
    serde_json::from_slice(bytes).map_err(|_| Refused::Malformed)
}

fn react_key(session_key: &[u8; 32], cid: i64, sid: &[u8; 16], mid: &[u8; 16]) -> [u8; 32] {
    prim::key32(mid, session_key, &[DS_REACT_KEY, &cid.to_be_bytes(), sid])
}

/// The mark the server groups reactions by: the same for everyone who
/// reacts with this emoji to this message, and nothing to anyone without
/// the message's key.
pub fn reaction_token(
    session_key: &[u8; 32],
    cid: i64,
    env: &Value,
    emoji: &str,
) -> Option<String> {
    let (sid, mid) = envelope_ids(env)?;
    let rk = react_key(session_key, cid, &sid, &mid);
    let mac = prim::hmac256(&rk, &[DS_REACT_TOKEN, emoji.as_bytes()]);
    Some(prim::b64(&mac[..16]))
}

fn react_sign(cid: i64, mid: &[u8; 16], uid: i64, emoji: &str) -> Vec<u8> {
    [
        DS_REACT,
        &cid.to_be_bytes(),
        mid,
        &uid.to_be_bytes(),
        emoji.as_bytes(),
    ]
    .concat()
}

/// My reaction, encrypted and signed: what goes with its mark.
pub fn seal_reaction(
    me: &Account,
    session_key: &[u8; 32],
    cid: i64,
    env: &Value,
    emoji: &str,
) -> Option<String> {
    let (sid, mid) = envelope_ids(env)?;
    let rk = react_key(session_key, cid, &sid, &mid);
    let sig = me.identity.sign(&react_sign(cid, &mid, me.uid, emoji));
    let nonce: [u8; 12] = prim::random();
    let mut data = [&sig[..], emoji.as_bytes()].concat();
    prim::seal(&rk, nonce, &react_sign(cid, &mid, me.uid, ""), &mut data);
    Some(prim::b64(&[&nonce[..], &data].concat()))
}

/// The emoji in someone's reaction, if it is theirs and matches its mark.
pub fn open_reaction(
    session_key: &[u8; 32],
    cid: i64,
    env: &Value,
    uid: i64,
    identity: &[u8; 32],
    token: &str,
    data: &str,
) -> Option<String> {
    let (sid, mid) = envelope_ids(env)?;
    let rk = react_key(session_key, cid, &sid, &mid);
    let raw = prim::unb64(data)?;
    if raw.len() < 12 + 64 + prim::TAG {
        return None;
    }
    let nonce: [u8; 12] = raw[..12].try_into().ok()?;
    let plain = prim::open(&rk, nonce, &react_sign(cid, &mid, uid, ""), &raw[12..])?;
    let (sig, emoji) = plain.split_at(64);
    let emoji = String::from_utf8(emoji.to_vec()).ok()?;
    let ok = prim::verify(identity, &react_sign(cid, &mid, uid, &emoji), sig)
        && reaction_token(session_key, cid, env, &emoji).as_deref() == Some(token);
    ok.then_some(emoji)
}

pub(super) mod b64_16 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::prim::b64(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        super::prim::unb64_n(&s).ok_or_else(|| serde::de::Error::custom("base64 de 16 bytes"))
    }
}

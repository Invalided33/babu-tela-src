//! Files sent directly ("Enviar direto"): a huge file goes from the
//! sender's app to each person who downloads it, peer to peer, while both
//! are online, as in Keet. The server never holds it, so it takes none of
//! the storage budget (files.rs) and none of R2.
//!
//! What the server does keep is the message: its attachment says what the
//! file is (name, size, how it is cut in pieces and the hash those pieces
//! add up to), so everyone who can read the message knows what to ask for
//! and can check what they get. And it carries the signalling that sets up
//! each connection, the same way it does for calls (gateway.rs), but only
//! between the message's author and someone who may read the message.

use serde_json::{Map, Value, json};

use crate::db::RowExt;
use crate::params;
use crate::state::State;
use crate::util::{ApiResult, fail, str};

/// The most the signalling of one step may carry (a description is a few
/// kilobytes, a candidate a few hundred bytes).
const MAX_DATA: usize = 16 * 1024;

/// Signalling messages per person per minute: setting up a connection
/// takes an offer, an answer and a few candidates each way.
pub const PER_MINUTE: u32 = 600;

/// 32 lowercase hex characters: the id the sender's app gives the file.
fn valid_id(s: &str) -> bool {
    s.len() == 32 && is_lower_hex(s)
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What the server keeps of a direct file on a message, checked: None when
/// `item` is not one (an upload, read by send_message), an error when it
/// says it is one and its id does not hold. Only the id (random, made on
/// the sender's PC): its name, size, piece size and hash are in the
/// encrypted envelope, for the people in the conversation alone.
pub fn attachment(item: &Value) -> ApiResult<Option<Value>> {
    let Some(at) = item.as_object() else {
        return Ok(None);
    };
    let Some(direct) = at.get("direct") else {
        return Ok(None);
    };
    let id = str(Some(direct));
    if !valid_id(&id) {
        return fail(400, "Anexo inválido");
    }
    Ok(Some(json!({ "kind": "direct", "direct": { "id": id } })))
}

/// Whether the attachments of a message (as stored) have the direct file
/// `id`.
fn has_file(attachments: &str, id: &str) -> bool {
    let Ok(Value::Array(list)) = serde_json::from_str::<Value>(attachments) else {
        return false;
    };
    list.iter()
        .any(|a| a["kind"] == "direct" && a["direct"]["id"].as_str() == Some(id))
}

/// Why a step of the signalling could not go on, as the app is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The other one has no connection open now.
    Offline,
    /// The message, or that file on it, is not there (any more).
    Gone,
    /// Neither one wrote the message, the one who did not may not read it,
    /// or one of them blocked the other.
    Denied,
}

impl Refusal {
    pub fn wire(self) -> &'static str {
        match self {
            Refusal::Offline => "offline",
            Refusal::Gone => "gone",
            Refusal::Denied => "denied",
        }
    }
}

/// Whether `uid` may signal `to` about file `file` of message `mid`: one of
/// them wrote it, the other may read it, and neither blocked the other.
pub fn check(st: &State, uid: i64, to: i64, mid: i64, file: &str) -> ApiResult<Option<Refusal>> {
    if to == uid || !valid_id(file) {
        return Ok(Some(Refusal::Denied));
    }
    let Some(m) = st.db.get(
        "SELECT author_id, channel_id, attachments FROM messages WHERE id = ?",
        params![mid],
    )?
    else {
        return Ok(Some(Refusal::Gone));
    };
    let author = m.int("author_id");
    let reader = if author == uid {
        to
    } else if author == to {
        uid
    } else {
        return Ok(Some(Refusal::Denied));
    };
    // The reader has to see the conversation (and the author still does).
    if st.access(reader, m.int("channel_id")).is_err()
        || st.access(author, m.int("channel_id")).is_err()
    {
        return Ok(Some(Refusal::Denied));
    }
    if st.blocked_between(uid, to)? {
        return Ok(Some(Refusal::Denied));
    }
    if !has_file(m.str("attachments"), file) {
        return Ok(Some(Refusal::Gone));
    }
    if !st.online(to) {
        return Ok(Some(Refusal::Offline));
    }
    Ok(None)
}

/// One step of the signalling, from `uid`: `{to, msg, file, data}`. It goes
/// to every connection of `to` (only the device that has the file, or that
/// asked for it, answers: `data` says which transfer it is). A step that
/// cannot go on is answered at once, so the app asking need not wait.
pub fn relay(st: &mut State, uid: i64, m: &Map<String, Value>) -> ApiResult<()> {
    if !st.hit(&format!("direct:{uid}"), PER_MINUTE, 60_000) {
        return Ok(());
    }
    let (Some(to), Some(mid)) = (int(m.get("to")), int(m.get("msg"))) else {
        return fail(400, "Pedido inválido");
    };
    let file = str(m.get("file"));
    let data = m.get("data").cloned().unwrap_or(Value::Null);
    if !data.is_object() || data.to_string().len() > MAX_DATA {
        return fail(400, "Pedido inválido");
    }
    match check(st, uid, to, mid, &file)? {
        None => st.send(
            to,
            "DIRECT",
            &json!({ "from": uid, "msg": mid, "file": file, "data": data }),
        ),
        // An answer to a refusal would only bounce back and forth.
        Some(_) if data.get("error").is_some() => {}
        Some(why) => st.send(
            uid,
            "DIRECT",
            &json!({
                "from": to, "msg": mid, "file": file,
                "data": { "sid": data.get("sid").cloned().unwrap_or(Value::Null), "error": why.wire() },
            }),
        ),
    }
    Ok(())
}

/// A whole number, from a JSON number or a string of digits.
fn int(v: Option<&Value>) -> Option<i64> {
    crate::util::to_int(v).ok()
}

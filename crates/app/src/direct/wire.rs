//! What goes over the connection of one transfer, and over the server to
//! set it up.
//!
//! On the data channel (reliable, in order), the person downloading asks
//! once, as the channel opens, in JSON, from which piece on and whether the
//! pieces' hashes are needed; the sender says so if they have to wait their
//! turn, then sends the hashes, then the file from there to the end, in
//! binary messages of 64 KiB, each saying what it is and where it goes, and
//! a last JSON message when it is all out (or one saying why it stopped).
//!
//! Through the server (the gateway's `direct` op, worker/src/direct.rs),
//! the signalling to make the connection: a description or a candidate,
//! or why not, all marked with the transfer's own number (`sid`), since
//! one person may download several files from another at once.

use bytes::{BufMut, BytesMut};
use serde_json::{Value, json};

/// The first byte of a binary message: what it carries.
pub const HASHES: u8 = 1;
pub const DATA: u8 = 2;

/// A binary message's header: the kind, then where its bytes go (in the
/// list of hashes, or in the file), little-endian.
pub const HEADER: usize = 9;

/// The bytes in one message: a piece is a whole number of them, so none
/// spans two. With its header it is just over the 64 KiB the WebRTC stack
/// takes by default, so link.rs raises that.
pub const CHUNK: usize = 64 * 1024;

pub fn chunk(kind: u8, offset: u64, data: &[u8]) -> BytesMut {
    let mut out = BytesMut::with_capacity(HEADER + data.len());
    out.put_u8(kind);
    out.put_u64_le(offset);
    out.put_slice(data);
    out
}

/// A binary message's kind, offset and bytes, if it holds together.
pub fn read_chunk(bytes: &[u8]) -> Option<(u8, u64, &[u8])> {
    if bytes.len() <= HEADER || !matches!(bytes[0], HASHES | DATA) {
        return None;
    }
    let offset = u64::from_le_bytes(bytes[1..HEADER].try_into().ok()?);
    Some((bytes[0], offset, &bytes[HEADER..]))
}

/// "Send me the file from piece `from` on, and the hashes if `hashes`."
pub fn get(from: u64, hashes: bool) -> Value {
    json!({ "t": "get", "from": from, "hashes": hashes })
}

/// Everything asked for went out.
pub fn done() -> Value {
    json!({ "t": "done" })
}

/// The sender is sending to others: this one waits its turn, connected.
pub fn queued() -> Value {
    json!({ "t": "queued" })
}

/// Why the sender stopped (see `Stop`).
pub fn stop(why: Stop) -> Value {
    json!({ "t": "stop", "why": why.wire() })
}

/// What a JSON message on the channel says.
#[derive(Debug, PartialEq, Eq)]
pub enum Said {
    Get { from: u64, hashes: bool },
    Queued,
    Done,
    Stop(Stop),
}

pub fn read(v: &Value) -> Option<Said> {
    match v["t"].as_str()? {
        "get" => Some(Said::Get {
            from: v["from"].as_u64()?,
            hashes: v["hashes"].as_bool().unwrap_or(true),
        }),
        "queued" => Some(Said::Queued),
        "done" => Some(Said::Done),
        "stop" => Some(Said::Stop(Stop::from_wire(v["why"].as_str()?))),
        _ => None,
    }
}

/// Why a transfer cannot go on, from either side or from the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The sender has no connection open (the server says).
    Offline,
    /// The message, or the file on the sender's PC, is not there any more.
    Gone,
    /// Not allowed (the server says).
    Denied,
    /// The file on the sender's PC is not the one sent any more.
    Changed,
    /// The sender is sending to too many people right now.
    Busy,
    /// The sender could not read the file.
    Unreadable,
    /// What arrived did not match its hash.
    Corrupt,
}

impl Stop {
    pub fn wire(self) -> &'static str {
        match self {
            Stop::Offline => "offline",
            Stop::Gone => "gone",
            Stop::Denied => "denied",
            Stop::Changed => "changed",
            Stop::Busy => "busy",
            Stop::Unreadable => "unreadable",
            Stop::Corrupt => "corrupt",
        }
    }

    pub fn from_wire(s: &str) -> Stop {
        match s {
            "offline" => Stop::Offline,
            "denied" => Stop::Denied,
            "changed" => Stop::Changed,
            "busy" => Stop::Busy,
            "unreadable" => Stop::Unreadable,
            "corrupt" => Stop::Corrupt,
            _ => Stop::Gone,
        }
    }
}

/// The signalling of transfer `sid`: `data` with the number in it.
pub fn signal(sid: &str, mut data: Value) -> Value {
    if let Value::Object(m) = &mut data {
        m.insert("sid".into(), json!(sid));
    }
    data
}

/// The signalling saying why transfer `sid` cannot go on.
pub fn refusal(sid: &str, why: Stop) -> Value {
    json!({ "sid": sid, "error": why.wire() })
}

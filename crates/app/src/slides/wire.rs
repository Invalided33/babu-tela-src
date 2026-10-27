//! A presentation on the call's data channel (voice/rtc.rs): a few JSON
//! messages for what is presented and on which slide, and the pictures in
//! binary pieces small enough for any copy of the WebRTC stack to take
//! (16 KiB a message). The channel is reliable and keeps the order, so the
//! pieces of a picture arrive one after another; pieces of different
//! slides may interleave, when the presenter moves on before a picture is
//! through.

use serde_json::{Value, json};

/// The most slides a presentation may have, here and from anyone else.
pub const MAX_SLIDES: usize = 500;

/// The largest picture of a slide, as it travels (a 1920 × 1080 JPEG of a
/// photo is about 0.3 MB; a PNG of a busy chart under 2 MB).
pub const MAX_PICTURE: usize = 8 * 1024 * 1024;

/// A picture's longest side, from anyone.
pub const MAX_SIDE: u32 = 4096;

/// A presentation's name is cut to this many characters.
const MAX_NAME: usize = 120;

/// The first byte of a piece of a picture, and the version of its layout.
/// (Every other message on the channel is JSON, which starts with `{`.)
const MAGIC: u8 = 0xB7;
const VERSION: u8 = 1;

/// A piece's header: magic, version, the presentation (u32), the slide
/// (u16), the picture's whole length and where this piece starts (u32
/// each), all little-endian.
const HEADER: usize = 16;

/// A whole message, and what of a picture fits in one.
pub const MESSAGE: usize = 16 * 1024;
pub const PIECE: usize = MESSAGE - HEADER;

/// "I show presentations", said as the channel opens. A copy of the app
/// that does not say it gets none: it would not show them.
pub fn hello() -> Value {
    json!({ "t": "slides", "v": 1 })
}

/// What I present, said to each person once (and again to anyone who
/// arrives): its id, how many slides, the pictures' size, its name and
/// the slide I am on.
pub fn announce(id: u32, count: usize, size: [u32; 2], name: &str, at: usize) -> Value {
    json!({
        "t": "deck",
        "id": id,
        "n": count,
        "w": size[0],
        "h": size[1],
        "name": name,
        "at": at,
    })
}

/// I went to slide `at`.
pub fn moved(id: u32, at: usize) -> Value {
    json!({ "t": "slide", "id": id, "at": at })
}

/// I stopped presenting.
pub fn ended(id: u32) -> Value {
    json!({ "t": "deckEnd", "id": id })
}

/// A presentation someone announced, checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Announced {
    pub id: u32,
    pub count: usize,
    pub size: [u32; 2],
    pub name: String,
    pub at: usize,
}

/// `announce`'s message, if it makes sense.
pub fn read_announce(v: &Value) -> Option<Announced> {
    let id = u32::try_from(v["id"].as_u64()?).ok()?;
    let count = usize::try_from(v["n"].as_u64()?).ok()?;
    let side = |key: &str| {
        v[key]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=MAX_SIDE).contains(n))
    };
    let size = [side("w")?, side("h")?];
    if count == 0 || count > MAX_SLIDES {
        return None;
    }
    let at = usize::try_from(v["at"].as_u64().unwrap_or(0))
        .unwrap_or(0)
        .min(count - 1);
    let name: String = v["name"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    Some(Announced {
        id,
        count,
        size,
        name,
        at,
    })
}

/// The presentation and slide of `moved`.
pub fn read_moved(v: &Value) -> Option<(u32, usize)> {
    let id = u32::try_from(v["id"].as_u64()?).ok()?;
    let at = usize::try_from(v["at"].as_u64()?).ok()?;
    Some((id, at))
}

/// The presentation of `ended`.
pub fn read_ended(v: &Value) -> Option<u32> {
    u32::try_from(v["id"].as_u64()?).ok()
}

/// A piece of a slide's picture.
#[derive(Debug, PartialEq, Eq)]
pub struct Piece<'a> {
    pub deck: u32,
    pub slide: usize,
    /// The whole picture's length, and where this piece goes in it.
    pub total: usize,
    pub offset: usize,
    pub data: &'a [u8],
}

/// The message carrying `data`, the part of slide `slide`'s picture (of
/// `total` bytes) that starts at `offset`.
pub fn piece(deck: u32, slide: usize, total: usize, offset: usize, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + data.len());
    out.push(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&deck.to_le_bytes());
    out.extend_from_slice(&(slide as u16).to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(offset as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// A piece, if `bytes` is one and holds together.
pub fn read_piece(bytes: &[u8]) -> Option<Piece<'_>> {
    if bytes.len() <= HEADER || bytes[0] != MAGIC || bytes[1] != VERSION {
        return None;
    }
    let u32_at =
        |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let deck = u32_at(2);
    let slide = usize::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let total = u32_at(8) as usize;
    let offset = u32_at(12) as usize;
    let data = &bytes[HEADER..];
    if slide >= MAX_SLIDES || total > MAX_PICTURE || offset.checked_add(data.len())? > total {
        return None;
    }
    Some(Piece {
        deck,
        slide,
        total,
        offset,
        data,
    })
}

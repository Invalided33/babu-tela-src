//! A GIPHY of our own, for `cargo xtask medir figurinhas`: the app's
//! sticker picker talks to it (BABUTELA_GIPHY_URL) the way it talks to
//! GIPHY, without a key of GIPHY's and without spending its calls. It
//! answers trending, searches and lookups by id with stickers drawn here,
//! animated WebP with a see-through background as GIPHY's are (and GIF
//! for the `url` field), and counts what it was asked.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};

use image::{Rgba, RgbaImage};
use serde_json::{Value, json};

use crate::Result;

/// Stickers in trending (GIPHY pages them; so does this).
const TRENDING: usize = 60;

/// Frames per sticker, and how long each shows: a loop of 1.2 s.
const FRAMES: usize = 20;
const FRAME_MS: u32 = 60;

/// The grid's rendition and the messages' one, as GIPHY sizes them.
const SMALL: u32 = 100;
const LARGE: u32 = 200;

/// The eyes and the smile.
const DARK: [u8; 3] = [20, 18, 40];

pub struct Fake {
    pub base: String,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// How many requests started with `prefix`.
    pub fn asked(&self, prefix: &str) -> usize {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|t| t.starts_with(prefix))
            .count()
    }
}

/// Starts it on a free port of this PC; it answers until the process ends.
pub fn start() -> Result<Fake> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
    let (b, log) = (base.clone(), asked.clone());
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let (b, log, files) = (b.clone(), log.clone(), files.clone());
            std::thread::spawn(move || {
                let _ = serve(sock, &b, &log, &files);
            });
        }
    });
    Ok(Fake { base, asked })
}

fn serve(
    mut sock: TcpStream,
    base: &str,
    log: &Mutex<Vec<String>>,
    files: &Mutex<HashMap<String, Vec<u8>>>,
) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&head);
    let target = text.split(' ').nth(1).unwrap_or("").to_owned();
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(target.clone());
    let (status, kind, body) = answer(base, &target, files);
    let reply = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Not Found" },
        body.len()
    );
    sock.write_all(reply.as_bytes())?;
    sock.write_all(&body)
}

fn answer(
    base: &str,
    target: &str,
    files: &Mutex<HashMap<String, Vec<u8>>>,
) -> (u16, &'static str, Vec<u8>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let param = |name: &str| -> String {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
            .map(decode)
            .unwrap_or_default()
    };
    let number = |name: &str, or: usize| param(name).parse().unwrap_or(or);
    let page = |ids: Vec<String>| {
        let (offset, limit) = (number("offset", 0), number("limit", 25).min(50));
        let total = ids.len();
        let data: Vec<Value> = ids
            .iter()
            .skip(offset)
            .take(limit)
            .map(|id| item(base, id))
            .collect();
        let count = data.len();
        let body = json!({
            "data": data,
            "pagination": { "total_count": total, "count": count, "offset": offset },
            "meta": { "status": 200, "msg": "OK", "response_id": "falso" },
        });
        (200, "application/json", body.to_string().into_bytes())
    };
    match path {
        "/v1/stickers/trending" => page((0..TRENDING).map(id_of).collect()),
        // "nada" finds nothing; anything else, 30 stickers in an order of
        // its own.
        "/v1/stickers/search" => {
            let q = param("q");
            if q.trim() == "nada" {
                return page(Vec::new());
            }
            let start = q.bytes().map(usize::from).sum::<usize>() % TRENDING;
            page((0..30).map(|i| id_of((start + i * 7) % TRENDING)).collect())
        }
        // Only stickers of this fake exist.
        "/v1/gifs" => {
            let data: Vec<Value> = param("ids")
                .split(',')
                .filter(|id| index_of(id).is_some())
                .map(|id| item(base, id))
                .collect();
            let body = json!({
                "data": data,
                "meta": { "status": 200, "msg": "OK", "response_id": "falso" },
            });
            (200, "application/json", body.to_string().into_bytes())
        }
        "/ping" => (200, "application/json", b"{}".to_vec()),
        p => match media(p, files) {
            Some((kind, bytes)) => (200, kind, bytes),
            None => (404, "text/plain", Vec::new()),
        },
    }
}

/// %2C and friends, and + for a space.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn id_of(i: usize) -> String {
    format!("babu{i:02}")
}

fn index_of(id: &str) -> Option<usize> {
    id.strip_prefix("babu")?
        .parse()
        .ok()
        .filter(|i| *i < TRENDING)
}

/// A sticker as GIPHY describes one (the fields the app reads).
fn item(base: &str, id: &str) -> Value {
    let rendition = |side: u32| {
        json!({
            "url": format!("{base}/m/{id}-{side}.gif?cid=teste&ct=s"),
            "webp": format!("{base}/m/{id}-{side}.webp?cid=teste&ct=s"),
            "width": side.to_string(), "height": side.to_string(),
        })
    };
    let ping = |kind: &str| json!({ "url": format!("{base}/ping?action_type={kind}&id={id}") });
    json!({
        "type": "sticker", "id": id, "title": format!("Figurinha de teste {id}"),
        "username": "babutela",
        "user": { "username": "babutela", "display_name": "Babu Tela (teste)" },
        "images": {
            "fixed_width_small": rendition(SMALL), "fixed_width": rendition(LARGE),
            "original": rendition(LARGE),
        },
        "analytics": { "onload": ping("SEEN"), "onclick": ping("CLICK"), "onsent": ping("SENT") },
    })
}

/// /m/<id>-<side>.webp (or .gif): drawn the first time, then kept.
fn media(path: &str, files: &Mutex<HashMap<String, Vec<u8>>>) -> Option<(&'static str, Vec<u8>)> {
    let name = path.strip_prefix("/m/")?;
    let (stem, ext) = name.rsplit_once('.')?;
    let (id, side) = stem.rsplit_once('-')?;
    let (i, side) = (index_of(id)?, side.parse::<u32>().ok()?);
    if side != SMALL && side != LARGE {
        return None;
    }
    let kind = match ext {
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };
    let mut cache = files.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(b) = cache.get(name) {
        return Some((kind, b.clone()));
    }
    let frames: Vec<RgbaImage> = (0..FRAMES).map(|f| draw(i, f, side)).collect();
    let bytes = if ext == "webp" {
        webp(&frames)
    } else {
        gif(&frames)
    }?;
    cache.insert(name.to_owned(), bytes.clone());
    Some((kind, bytes))
}

/// Frame `f` of sticker `i`: a round face in its own colour, breathing,
/// with a dot going round it, on nothing (see-through, soft edges).
fn draw(i: usize, f: usize, side: u32) -> RgbaImage {
    let s = side as f32;
    let t = f as f32 / FRAMES as f32 * std::f32::consts::TAU;
    let (cx, cy) = (s / 2.0, s / 2.0);
    let r = s * 0.30 * (1.0 + 0.08 * t.sin());
    let color = hue(i as f32 * 47.0);
    let (dx, dy) = (cx + s * 0.40 * t.cos(), cy + s * 0.40 * t.sin());
    let eyes = [
        (cx - r * 0.38, cy - r * 0.22),
        (cx + r * 0.38, cy - r * 0.22),
    ];
    // How much of a pixel a circle covers (1 px of soft edge).
    let cover = |x: f32, y: f32, ox: f32, oy: f32, rad: f32| {
        (rad - ((x - ox).powi(2) + (y - oy).powi(2)).sqrt() + 0.5).clamp(0.0, 1.0)
    };
    RgbaImage::from_fn(side, side, |px, py| {
        let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
        let mut out = [0.0f32; 4];
        let mut over = |c: [u8; 3], a: f32| {
            for k in 0..3 {
                out[k] = out[k] * (1.0 - a) + f32::from(c[k]) * a;
            }
            out[3] = out[3] * (1.0 - a) + a;
        };
        over(color, cover(x, y, cx, cy, r));
        for (ex, ey) in eyes {
            over(DARK, cover(x, y, ex, ey, r * 0.13));
        }
        // The smile: the lower half of a thin ring.
        if y > cy + r * 0.12 {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            over(
                DARK,
                (r * 0.07 - (d - r * 0.55).abs() + 0.5).clamp(0.0, 1.0),
            );
        }
        over([255, 255, 255], cover(x, y, dx, dy, s * 0.06));
        // Back from premultiplied: what a file keeps.
        let a = out[3];
        let un = |v: f32| {
            if a > 0.0 {
                (v / a).round().clamp(0.0, 255.0) as u8
            } else {
                0
            }
        };
        Rgba([
            un(out[0]),
            un(out[1]),
            un(out[2]),
            (a * 255.0).round() as u8,
        ])
    })
}

fn hue(deg: f32) -> [u8; 3] {
    let h = (deg % 360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    let v = |c: f32| (80.0 + c * 175.0) as u8;
    [v(r), v(g), v(b)]
}

/// An animated WebP: each frame lossless (VP8L), replacing the one before.
fn webp(frames: &[RgbaImage]) -> Option<Vec<u8>> {
    fn chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(name);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
    }
    fn u24(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    }
    let (w, h) = frames.first()?.dimensions();
    let mut body = Vec::new();
    // Animated, with alpha; the canvas.
    let mut vp8x = vec![0x02 | 0x10, 0, 0, 0];
    u24(&mut vp8x, w - 1);
    u24(&mut vp8x, h - 1);
    chunk(&mut body, b"VP8X", &vp8x);
    // No background colour, looping forever.
    chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 0, 0]);
    for img in frames {
        let mut file = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut file)
            .encode(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .ok()?;
        let at = file.windows(4).position(|x| x == b"VP8L")?;
        let len = u32::from_le_bytes(file.get(at + 4..at + 8)?.try_into().ok()?) as usize;
        let mut frame = Vec::new();
        u24(&mut frame, 0);
        u24(&mut frame, 0);
        u24(&mut frame, w - 1);
        u24(&mut frame, h - 1);
        u24(&mut frame, FRAME_MS);
        // No blending, no disposal: each frame is the whole picture.
        frame.push(0b10);
        chunk(&mut frame, b"VP8L", file.get(at + 8..at + 8 + len)?);
        chunk(&mut body, b"ANMF", &frame);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Some(out)
}

fn gif(frames: &[RgbaImage]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).ok()?;
        for img in frames {
            let delay = image::Delay::from_numer_denom_ms(FRAME_MS, 1);
            enc.encode_frame(image::Frame::from_parts(img.clone(), 0, 0, delay))
                .ok()?;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::AnimationDecoder;

    #[test]
    fn its_stickers_are_animated_and_see_through() {
        let frames: Vec<RgbaImage> = (0..FRAMES).map(|f| draw(3, f, SMALL)).collect();
        let bytes = webp(&frames).unwrap();
        let d = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
        assert!(d.has_animation());
        let back: Vec<_> = d
            .into_frames()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(back.len(), FRAMES);
        let first = back[0].buffer();
        assert_eq!(first.get_pixel(0, 0)[3], 0, "the corner is see-through");
        assert_eq!(first.get_pixel(50, 50)[3], 255, "the middle is not");
        assert_ne!(back[0].buffer(), back[5].buffer(), "it moves");
        assert_eq!(back[0].delay().numer_denom_ms(), (FRAME_MS, 1));
    }

    #[test]
    fn it_answers_like_giphy() {
        let files = Mutex::new(HashMap::new());
        let (status, _, body) = answer(
            "http://x",
            "/v1/stickers/trending?limit=24&offset=48",
            &files,
        );
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, 200);
        assert_eq!(v["data"].as_array().unwrap().len(), TRENDING - 48);
        assert_eq!(v["data"][0]["id"], "babu48");
        assert_eq!(v["pagination"]["total_count"], TRENDING);
        let (_, _, body) = answer("http://x", "/v1/gifs?ids=babu03%2Csumiu%2Cbabu59", &files);
        let v: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["id"].as_str())
            .collect();
        assert_eq!(ids, ["babu03", "babu59"]);
        let (_, _, body) = answer("http://x", "/v1/stickers/search?q=nada", &files);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["data"].as_array().unwrap().is_empty());
        assert_eq!(
            answer("http://x", "/m/babu03-100.webp?cid=teste", &files).0,
            200
        );
        assert_eq!(answer("http://x", "/m/babu03-37.webp", &files).0, 404);
        assert_eq!(decode("a%2Cb+c%"), "a,b c%");
    }
}

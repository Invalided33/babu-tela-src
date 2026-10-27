//! The annotations' messages, as they go over a call's data channel: JSON,
//! like everything else that goes there (voice/rtc.rs). A copy of the app
//! from before annotations ignores them.
//!
//! - `inkSurfaces`: everything of the sender's that can be annotated now
//!   (its screen share, its presentation), and whether anyone may annotate
//!   it. The whole list each time, so one that ended while a message was
//!   lost still goes.
//! - `ink`: a stroke begins, or comes whole (`e`), on one page of a
//!   surface. Only the surface's owner sends someone else's (`a`, its
//!   author), to whoever arrives after it was drawn.
//! - `inkMore`: more points of the sender's stroke being drawn (for a
//!   shape, where it ends now); `inkEnd`: it is finished.
//! - `inkDel`: strokes taken away (the eraser, undo).
//! - `inkClear`: the sender's strokes on a surface go, or everyone's.
//!
//! Positions go as whole numbers from 0 to 10 000 across and down the
//! picture, so they mean the same at any size it is shown.

use egui::{Pos2, pos2};
use serde_json::{Value, json};

use super::{Kind, MAX_POINTS, Page, Stroke, Surface, Tool};

const SCALE: f32 = 10_000.0;

/// Widths go in the same units, from this thin to this thick.
const WIDTH_RANGE: (u64, u64) = (1, 500);

/// Pages beyond this are not a slide anyone presents.
const MAX_PAGE: u64 = 10_000;

/// What one message says.
#[derive(Debug, PartialEq)]
pub enum Msg {
    /// The sender's surfaces: id, kind, and whether anyone may annotate.
    Surfaces(Vec<(u32, Kind, bool)>),
    Stroke {
        page: Page,
        id: u32,
        author: Option<i64>,
        tool: Tool,
        color: [u8; 4],
        width: f32,
        points: Vec<Pos2>,
        /// For `Tool::Text`, what was typed. Not sent for the other tools.
        text: Option<String>,
        done: bool,
    },
    More {
        id: u32,
        points: Vec<Pos2>,
    },
    End {
        id: u32,
    },
    Delete {
        surface: Surface,
        strokes: Vec<(i64, u32)>,
    },
    Clear {
        surface: Surface,
        all: bool,
    },
}

fn kind_wire(kind: Kind) -> &'static str {
    match kind {
        Kind::Screen => "screen",
        Kind::Slides => "slides",
    }
}

fn kind_from_wire(s: &str) -> Option<Kind> {
    match s {
        "screen" => Some(Kind::Screen),
        "slides" => Some(Kind::Slides),
        _ => None,
    }
}

pub fn surfaces(list: &[(u32, Kind, bool)]) -> Value {
    let list: Vec<Value> = list
        .iter()
        .map(|(id, kind, all)| json!({ "s": id, "k": kind_wire(*kind), "all": all }))
        .collect();
    json!({ "t": "inkSurfaces", "list": list })
}

/// A stroke as it is now. `with_author`: someone else's, handed on by the
/// surface's owner.
pub fn stroke(s: &Stroke, with_author: bool) -> Value {
    let mut v = json!({
        "t": "ink",
        "o": s.page.surface.owner,
        "s": s.page.surface.id,
        "g": s.page.n,
        "n": s.id,
        "f": s.tool.wire(),
        "c": s.color,
        "w": width_wire(s.width),
        "p": points(&s.points),
        "e": s.done.is_some(),
    });
    if with_author {
        v["a"] = json!(s.author);
    }
    if let Some(t) = s.text.as_ref() {
        v["x"] = json!(t);
    }
    v
}

pub fn more(id: u32, p: &[Pos2]) -> Value {
    json!({ "t": "inkMore", "n": id, "p": points(p) })
}

pub fn end(id: u32) -> Value {
    json!({ "t": "inkEnd", "n": id })
}

pub fn delete(surface: Surface, strokes: &[(i64, u32)]) -> Value {
    let d: Vec<Value> = strokes.iter().map(|(a, n)| json!([a, n])).collect();
    json!({ "t": "inkDel", "o": surface.owner, "s": surface.id, "d": d })
}

pub fn clear(surface: Surface, all: bool) -> Value {
    json!({ "t": "inkClear", "o": surface.owner, "s": surface.id, "all": all })
}

fn width_wire(width: f32) -> u64 {
    ((width * SCALE).round() as u64).clamp(WIDTH_RANGE.0, WIDTH_RANGE.1)
}

fn points(p: &[Pos2]) -> Value {
    let mut out = Vec::with_capacity(p.len() * 2);
    for q in p {
        out.push((q.x.clamp(0.0, 1.0) * SCALE).round() as u16);
        out.push((q.y.clamp(0.0, 1.0) * SCALE).round() as u16);
    }
    json!(out)
}

fn read_points(v: &Value) -> Option<Vec<Pos2>> {
    let list = v.as_array()?;
    if list.len() % 2 != 0 || list.len() > MAX_POINTS * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(list.len() / 2);
    for [x, y] in list.as_chunks::<2>().0 {
        let x = x.as_u64()?.min(SCALE as u64) as f32 / SCALE;
        let y = y.as_u64()?.min(SCALE as u64) as f32 / SCALE;
        out.push(pos2(x, y));
    }
    Some(out)
}

fn read_u32(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

fn read_surface(v: &Value) -> Option<Surface> {
    Some(Surface {
        owner: v["o"].as_i64()?,
        id: read_u32(&v["s"])?,
    })
}

/// What a message says, when it is one of these and well formed.
pub fn read(v: &Value) -> Option<Msg> {
    match v["t"].as_str()? {
        "inkSurfaces" => {
            let mut list = Vec::new();
            for item in v["list"].as_array()?.iter().take(8) {
                list.push((
                    read_u32(&item["s"])?,
                    kind_from_wire(item["k"].as_str()?)?,
                    item["all"].as_bool().unwrap_or(true),
                ));
            }
            Some(Msg::Surfaces(list))
        }
        "ink" => {
            let page = v["g"].as_u64().filter(|g| *g <= MAX_PAGE)? as u32;
            let tool = Tool::from_wire(v["f"].as_str()?)?;
            let c = v["c"].as_array().filter(|c| c.len() == 4)?;
            let mut color = [0u8; 4];
            for (slot, part) in color.iter_mut().zip(c) {
                *slot = u8::try_from(part.as_u64()?).ok()?;
            }
            let w = v["w"]
                .as_u64()
                .filter(|w| (WIDTH_RANGE.0..=WIDTH_RANGE.1).contains(w))?;
            let points = read_points(&v["p"]).filter(|p| !p.is_empty())?;
            Some(Msg::Stroke {
                page: Page {
                    surface: read_surface(v)?,
                    n: page,
                },
                id: read_u32(&v["n"])?,
                author: v["a"].as_i64(),
                tool,
                color,
                width: w as f32 / SCALE,
                points,
                text: v["x"].as_str().map(|t| t.to_owned()),
                done: v["e"].as_bool().unwrap_or(false),
            })
        }
        "inkMore" => Some(Msg::More {
            id: read_u32(&v["n"])?,
            points: read_points(&v["p"])?,
        }),
        "inkEnd" => Some(Msg::End {
            id: read_u32(&v["n"])?,
        }),
        "inkDel" => {
            let mut strokes = Vec::new();
            for pair in v["d"].as_array()?.iter().take(MAX_POINTS) {
                strokes.push((pair[0].as_i64()?, read_u32(&pair[1])?));
            }
            Some(Msg::Delete {
                surface: read_surface(v)?,
                strokes,
            })
        }
        "inkClear" => Some(Msg::Clear {
            surface: read_surface(v)?,
            all: v["all"].as_bool().unwrap_or(false),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, Page, Stroke, Surface, Tool};
    use super::*;
    use std::time::Instant;

    fn text_stroke(text: &str) -> Stroke {
        Stroke {
            author: 3,
            id: 7,
            page: Page {
                surface: Surface { owner: 3, id: 42 },
                n: 0,
            },
            tool: Tool::Text,
            color: [255, 0, 0, 255],
            width: 0.034,
            points: vec![Pos2::new(0.1, 0.2)],
            text: Some(text.to_owned()),
            done: Some(Instant::now()),
            touched: Instant::now(),
        }
    }

    #[test]
    fn text_strokes_round_trip() {
        let s = text_stroke("olá, mundo! 🙂");
        let encoded = stroke(&s, false);
        let msg = read(&encoded).expect("decodes");
        match msg {
            Msg::Stroke {
                tool,
                text,
                points,
                done,
                ..
            } => {
                assert_eq!(tool, Tool::Text);
                assert_eq!(text.as_deref(), Some("olá, mundo! 🙂"));
                assert_eq!(points.len(), 1);
                assert!(done);
            }
            other => panic!("not a stroke: {other:?}"),
        }
    }

    #[test]
    fn non_text_strokes_leave_the_text_field_out() {
        let mut s = text_stroke("x");
        s.tool = Tool::Pen;
        s.text = None;
        let encoded = stroke(&s, false);
        assert!(encoded.get("x").is_none(), "text leaked out: {encoded}");
    }

    #[test]
    fn surfaces_round_trip_as_before() {
        let list = vec![(1, Kind::Screen, true), (2, Kind::Slides, false)];
        let v = surfaces(&list);
        let msg = read(&v).expect("decodes");
        assert_eq!(msg, Msg::Surfaces(list));
    }
}

//! Anotações: drawing over what someone shares in a call (a screen, a
//! window, the slides of a presentation), seen by everyone as it is drawn.
//! Done the way Zoom, Teams and Meet do it:
//!
//! - It is not in the video. Every copy of the app draws the strokes over
//!   the picture itself, from the points each person sends over the call's
//!   data channels (voice/rtc.rs): sharp at any size, at once for whoever
//!   draws, and the video of the screen is left alone.
//! - Whoever shares their screen sees the strokes over the real screen too
//!   (overlay.rs), in a window Windows leaves out of every capture, so they
//!   never come back in the video.
//! - Anyone in the call may annotate, unless the person sharing chooses
//!   "Só eu" (Teams' "Only I can annotate", Zoom's "Disable annotation for
//!   others"). Each person takes away their own strokes; the person sharing
//!   takes away anyone's.
//! - The person sharing keeps every stroke drawn over what they share, and
//!   hands them to whoever arrives later (`hello`).
//! - Disappearing ink (Meet's default, Zoom's vanishing pen) fades a moment
//!   after the stroke ends; the pen stays until it is erased or cleared.
//! - Who is drawing shows next to the stroke for a moment (Zoom's "names
//!   of annotators", Teams' collaborative cursors).
//!
//! Positions are from 0 to 1 across and down the picture, and widths a
//! fraction of its height, so a stroke lands in the same place of the
//! picture at whatever size each person shows it.

pub mod overlay;
pub mod paint;
mod raster;
pub mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use egui::{Pos2, Vec2, pos2, vec2};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

/// Something that can be annotated: someone's screen share (a new one each
/// time they share) or their presentation (its deck's id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Surface {
    pub owner: i64,
    pub id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A screen or a window.
    Screen,
    Slides,
}

/// One page of a surface: a screen share has one (0), a presentation one
/// per slide, so each slide keeps its own strokes, as in PowerPoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Page {
    pub surface: Surface,
    pub n: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    /// Fades a moment after the stroke ends.
    Vanish,
    Pen,
    /// Wide and see-through.
    Highlighter,
    Arrow,
    Rect,
    Ellipse,
    /// A piece of typed text, placed where it was clicked.
    Text,
    /// Takes away the strokes it touches.
    Eraser,
}

impl Tool {
    pub fn wire(self) -> &'static str {
        match self {
            Tool::Vanish => "vanish",
            Tool::Pen => "pen",
            Tool::Highlighter => "marker",
            Tool::Arrow => "arrow",
            Tool::Rect => "rect",
            Tool::Ellipse => "ellipse",
            Tool::Text => "text",
            Tool::Eraser => "eraser",
        }
    }

    /// What can come over the wire: everything that draws.
    pub fn from_wire(s: &str) -> Option<Tool> {
        [
            Tool::Vanish,
            Tool::Pen,
            Tool::Highlighter,
            Tool::Arrow,
            Tool::Rect,
            Tool::Ellipse,
            Tool::Text,
        ]
        .into_iter()
        .find(|t| t.wire() == s)
    }

    /// Two points, from where the drag began to where it is.
    pub fn shape(self) -> bool {
        matches!(self, Tool::Arrow | Tool::Rect | Tool::Ellipse)
    }

    /// What the toolbars call it.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Vanish => "Tinta que some",
            Tool::Pen => "Caneta",
            Tool::Highlighter => "Marca-texto",
            Tool::Arrow => "Seta",
            Tool::Rect => "Retângulo",
            Tool::Ellipse => "Elipse",
            Tool::Text => "Texto",
            Tool::Eraser => "Borracha",
        }
    }
}

/// One stroke, mine or someone's.
#[derive(Clone, Debug)]
pub struct Stroke {
    pub author: i64,
    /// Its number among its author's.
    pub id: u32,
    pub page: Page,
    pub tool: Tool,
    /// RGBA, not premultiplied.
    pub color: [u8; 4],
    /// A fraction of the picture's height. For `Tool::Text` it is the
    /// font size (ascent), still a fraction of the picture's height.
    pub width: f32,
    /// From 0 to 1 across and down the picture. A shape has two: where it
    /// began and where it ends. A text has one, where its top left sits.
    pub points: Vec<Pos2>,
    /// For `Tool::Text`, what was typed. None for every other tool.
    pub text: Option<String>,
    /// When it ended; None while it is being drawn.
    pub done: Option<Instant>,
    /// Its last change: one whose end never comes (its author's connection
    /// fell) counts as ended after a while.
    pub touched: Instant,
}

impl Stroke {
    fn key(&self) -> (i64, u32) {
        (self.author, self.id)
    }

    /// When it ended, or counts as ended.
    fn ended(&self, now: Instant) -> Option<Instant> {
        self.done.or_else(|| {
            let stale = self.touched + STALE;
            (now >= stale).then_some(stale)
        })
    }

    /// How opaque it is now (disappearing ink fades), 0 once it is gone.
    fn opacity(&self, now: Instant) -> f32 {
        if self.tool != Tool::Vanish {
            return 1.0;
        }
        let Some(ended) = self.ended(now) else {
            return 1.0;
        };
        let since = now.saturating_duration_since(ended);
        if since <= VANISH_HOLD {
            1.0
        } else {
            1.0 - (since - VANISH_HOLD).as_secs_f32() / VANISH_FADE.as_secs_f32()
        }
        .clamp(0.0, 1.0)
    }

    fn gone(&self, now: Instant) -> bool {
        self.tool == Tool::Vanish
            && self
                .ended(now)
                .is_some_and(|e| now.saturating_duration_since(e) >= VANISH_HOLD + VANISH_FADE)
    }
}

/// The colours to draw in: red first, as Teams' pen.
pub const COLORS: [[u8; 3]; 8] = [
    [0xff, 0x3b, 0x30],
    [0xff, 0x95, 0x00],
    [0xff, 0xd6, 0x0a],
    [0x34, 0xc7, 0x59],
    [0x0a, 0x84, 0xff],
    [0xbf, 0x5a, 0xf2],
    [0xff, 0xff, 0xff],
    [0x1c, 0x1c, 0x1e],
];

/// Thin, medium and thick: of the picture's height (a full HD screen's 3,
/// 5 and 11 pixels).
pub const SIZES: [f32; 3] = [0.0025, 0.005, 0.01];

/// For `Tool::Text`, the font size (as a fraction of the picture's
/// height) that each width slot of the toolbar maps to. On a 1080p
/// picture the three come out to 24, 36 and 54 px.
pub const TEXT_SIZES: [f32; 3] = [0.022, 0.034, 0.050];

/// The longest a text annotation goes, in bytes. Each new character while
/// the user types is checked against this.
pub const MAX_TEXT_BYTES: usize = 500;

/// The nearest valid char boundary at or before `cap`, so a truncation of
/// a UTF-8 string does not split a code point.
fn nearest_char(s: &str, cap: usize) -> usize {
    (0..=cap.min(s.len()))
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0)
}

/// The highlighter: this many times wider, and this see-through.
const MARKER_WIDTH: f32 = 4.0;
const MARKER_ALPHA: f32 = 0.38;

/// Disappearing ink stays whole this long once the stroke ends, then
/// fades over this.
pub const VANISH_HOLD: Duration = Duration::from_millis(1500);
pub const VANISH_FADE: Duration = Duration::from_millis(1000);

/// How long after someone's stroke ends their name still shows next to it.
pub const NAME_FOR: Duration = Duration::from_millis(1500);

/// A stroke whose end never came counts as ended after this.
const STALE: Duration = Duration::from_secs(10);

/// The points of my stroke go out at most this often while it is drawn.
const SEND_EVERY: Duration = Duration::from_millis(40);

/// The most points a stroke has: mine goes on as a new stroke past this,
/// so every message stays small (wire.rs reads no more than this).
pub const MAX_POINTS: usize = 2000;

/// The most strokes kept in all: past this, new ones are not kept.
const MAX_STROKES: usize = 5000;

/// A shape smaller than this (of the picture) was a click, not a shape.
const MIN_SHAPE: f32 = 0.004;

/// What I draw with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pen {
    pub tool: Tool,
    /// In COLORS and SIZES.
    pub color: usize,
    pub size: usize,
}

impl Default for Pen {
    /// Disappearing ink, Meet's default: pointing at something leaves
    /// nothing behind to clean up.
    fn default() -> Pen {
        Pen {
            tool: Tool::Vanish,
            color: 0,
            size: 1,
        }
    }
}

/// What the owner says of a surface.
#[derive(Clone, Copy, Debug)]
struct Info {
    kind: Kind,
    /// Anyone may annotate it (else only its owner).
    everyone: bool,
}

/// My stroke being drawn.
struct Drawing {
    id: u32,
    /// How many of its points went out.
    sent: usize,
    /// A shape's end moved since it last went out.
    moved: bool,
    sent_at: Instant,
}

/// What undo takes back.
enum Action {
    /// I drew it.
    Drew(Stroke),
    /// I took these away (the eraser, clearing).
    Erased(Vec<Stroke>),
}

impl Action {
    fn surface(&self) -> Option<Surface> {
        match self {
            Action::Drew(s) => Some(s.page.surface),
            Action::Erased(list) => list.first().map(|s| s.page.surface),
        }
    }
}

/// A stroke as it shows now (`Ink::looks`).
#[derive(Clone, Debug)]
pub struct Look {
    pub tool: Tool,
    /// For `Tool::Text`, what was typed. None for every other tool.
    pub text: Option<String>,
    /// RGBA, not premultiplied, with the fading and the highlighter's
    /// transparency in it.
    pub color: [u8; 4],
    /// Of the picture's height: the tool's own width.
    pub width: f32,
    pub points: Vec<Pos2>,
    pub author: i64,
    /// Its author's name shows next to it now.
    pub tag: bool,
}

#[derive(Default)]
struct State {
    me: i64,
    /// Everything that can be annotated now: mine and everyone's.
    surfaces: HashMap<Surface, Info>,
    /// Every stroke, in the order drawn.
    strokes: Vec<Stroke>,
    next_id: u32,
    pen: Pen,
    /// What I annotate now (its toolbar is open), if anything.
    active: Option<Surface>,
    drawing: Option<Drawing>,
    /// What the eraser took in the drag under way: one undo for it all.
    erasing: Option<Vec<Stroke>>,
    undo: Vec<Action>,
    redo: Vec<Action>,
    /// Where messages for everyone in the call go, while there is a call.
    out: Option<UnboundedSender<Value>>,
    /// Told whenever the strokes over my own screen change (overlay.rs).
    on_mine: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The names of who is in the call, for the window over my screen,
    /// which draws them next to their strokes on its own thread.
    names: HashMap<i64, String>,
    /// Hide the toolbars for one frame: a picture of what is annotated is
    /// being taken (`Core::save_annotation`).
    hide_tools: bool,
}

pub struct Ink {
    ctx: egui::Context,
    state: Mutex<State>,
}

impl Ink {
    pub fn new(ctx: egui::Context) -> Arc<Ink> {
        Arc::new(Ink {
            ctx,
            state: Mutex::new(State {
                next_id: crate::voice::rtc::random_u32() >> 1,
                ..Default::default()
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(s: &State, v: Value) {
        if let Some(out) = &s.out {
            let _ = out.send(v);
        }
    }

    /// Something on `surface` changed: the interface draws again, and so
    /// does the window over my screen when it is my screen's.
    fn changed(&self, s: &State, surface: Surface) {
        self.ctx.request_repaint();
        let mine_screen = surface.owner == s.me
            && s.surfaces
                .get(&surface)
                .is_some_and(|i| i.kind == Kind::Screen);
        if mine_screen && let Some(poke) = &s.on_mine {
            poke();
        }
    }

    // ------------------------------------------------------------ the call

    /// A call begins, as `me`.
    pub fn set_me(&self, me: i64) {
        self.lock().me = me;
    }

    /// Messages for everyone in the call go to `out` from now on.
    pub fn attach(&self, out: UnboundedSender<Value>) {
        self.lock().out = Some(out);
    }

    pub fn detach(&self) {
        self.lock().out = None;
    }

    /// The call is over: nothing of anyone's is kept, nor of mine.
    pub fn clear(&self) {
        {
            let mut s = self.lock();
            s.surfaces.clear();
            s.strokes.clear();
            s.names.clear();
            s.active = None;
            s.drawing = None;
            s.erasing = None;
            s.undo.clear();
            s.redo.clear();
        }
        self.ctx.request_repaint();
    }

    /// The window over my screen wants to know when its strokes change.
    pub fn watch_mine(&self, poke: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.lock().on_mine = poke;
    }

    /// Who is in the call now, by name (each time that changes).
    pub fn set_names(&self, names: impl IntoIterator<Item = (i64, String)>) {
        self.lock().names = names.into_iter().collect();
    }

    /// Someone in the call changed their name.
    pub fn rename(&self, uid: i64, name: &str) {
        if let Some(n) = self.lock().names.get_mut(&uid) {
            name.clone_into(n);
        }
    }

    /// `uid`'s name, when they are in the call.
    pub fn name_of(&self, uid: i64) -> String {
        self.lock().names.get(&uid).cloned().unwrap_or_default()
    }

    /// For someone whose connection with me just opened: what I share that
    /// can be annotated, and every stroke on it so far (disappearing ink
    /// aside, which would be gone in a moment).
    pub fn hello(&self) -> Vec<Value> {
        let s = self.lock();
        let mut out = vec![Self::surfaces_message(&s)];
        for st in &s.strokes {
            let mine = st.page.surface.owner == s.me && s.surfaces.contains_key(&st.page.surface);
            if mine && st.tool != Tool::Vanish {
                out.push(wire::stroke(st, st.author != s.me));
            }
        }
        out
    }

    fn surfaces_message(s: &State) -> Value {
        let mut list: Vec<(u32, Kind, bool)> = s
            .surfaces
            .iter()
            .filter(|(k, _)| k.owner == s.me)
            .map(|(k, i)| (k.id, i.kind, i.everyone))
            .collect();
        list.sort_by_key(|(id, ..)| *id);
        wire::surfaces(&list)
    }

    /// Someone left the call: what they shared goes, with its strokes.
    pub fn forget_owner(&self, uid: i64) {
        let mut s = self.lock();
        let gone: Vec<Surface> = s
            .surfaces
            .keys()
            .filter(|k| k.owner == uid)
            .copied()
            .collect();
        if gone.is_empty() {
            return;
        }
        for surface in gone {
            Self::drop_surface(&mut s, surface);
        }
        drop(s);
        self.ctx.request_repaint();
    }

    fn drop_surface(s: &mut State, surface: Surface) {
        s.surfaces.remove(&surface);
        s.strokes.retain(|st| st.page.surface != surface);
        if s.active == Some(surface) {
            s.active = None;
        }
        if s.drawing.as_ref().is_some_and(|d| {
            !s.strokes
                .iter()
                .any(|st| st.author == s.me && st.id == d.id)
        }) {
            s.drawing = None;
        }
        s.undo.retain(|a| a.surface() != Some(surface));
        s.redo.retain(|a| a.surface() != Some(surface));
    }

    // ------------------------------------------------------------ mine

    /// What I share now that can be annotated (a new screen share, a
    /// presentation): everyone is told. Another one of the same kind ends.
    pub fn open_mine(&self, kind: Kind, id: u32) -> Surface {
        let mut s = self.lock();
        let surface = Surface { owner: s.me, id };
        let me = s.me;
        let old: Vec<Surface> = s
            .surfaces
            .iter()
            .filter(|(k, i)| k.owner == me && i.kind == kind && **k != surface)
            .map(|(k, _)| *k)
            .collect();
        for o in old {
            Self::drop_surface(&mut s, o);
        }
        s.surfaces.entry(surface).or_insert(Info {
            kind,
            everyone: true,
        });
        Self::send(&s, Self::surfaces_message(&s));
        self.changed(&s, surface);
        surface
    }

    /// What I shared of `kind` ended.
    pub fn close_mine(&self, kind: Kind) {
        let mut s = self.lock();
        let me = s.me;
        let gone: Vec<Surface> = s
            .surfaces
            .iter()
            .filter(|(k, i)| k.owner == me && i.kind == kind)
            .map(|(k, _)| *k)
            .collect();
        if gone.is_empty() {
            return;
        }
        for surface in gone {
            Self::drop_surface(&mut s, surface);
        }
        Self::send(&s, Self::surfaces_message(&s));
        drop(s);
        self.ctx.request_repaint();
    }

    /// My surface of `kind`, while I share one.
    pub fn mine(&self, kind: Kind) -> Option<Surface> {
        let s = self.lock();
        s.surfaces
            .iter()
            .find(|(k, i)| k.owner == s.me && i.kind == kind)
            .map(|(k, _)| *k)
    }

    /// Whether anyone may annotate `surface` (mine), or only I.
    pub fn set_everyone(&self, surface: Surface, everyone: bool) {
        let mut s = self.lock();
        if surface.owner != s.me {
            return;
        }
        let Some(info) = s.surfaces.get_mut(&surface) else {
            return;
        };
        if info.everyone == everyone {
            return;
        }
        info.everyone = everyone;
        Self::send(&s, Self::surfaces_message(&s));
        self.changed(&s, surface);
    }

    pub fn everyone(&self, surface: Surface) -> bool {
        self.lock()
            .surfaces
            .get(&surface)
            .is_some_and(|i| i.everyone)
    }

    // ------------------------------------------------------------ what can be annotated

    /// `uid`'s screen share, while it can be annotated.
    pub fn screen_of(&self, uid: i64) -> Option<Surface> {
        self.lock()
            .surfaces
            .iter()
            .find(|(k, i)| k.owner == uid && i.kind == Kind::Screen)
            .map(|(k, _)| *k)
    }

    /// `uid`'s presentation `deck`, while it can be annotated.
    pub fn slides_of(&self, uid: i64, deck: u32) -> Option<Surface> {
        let surface = Surface {
            owner: uid,
            id: deck,
        };
        self.lock()
            .surfaces
            .get(&surface)
            .is_some_and(|i| i.kind == Kind::Slides)
            .then_some(surface)
    }

    fn may(s: &State, surface: Surface) -> bool {
        s.surfaces
            .get(&surface)
            .is_some_and(|i| surface.owner == s.me || i.everyone)
    }

    /// Whether I may annotate `surface`.
    pub fn may_annotate(&self, surface: Surface) -> bool {
        Self::may(&self.lock(), surface)
    }

    /// Whether `surface` is mine.
    pub fn owns(&self, surface: Surface) -> bool {
        surface.owner == self.lock().me
    }

    /// What I annotate now (its toolbar is open), if anything.
    pub fn active(&self) -> Option<Surface> {
        self.lock().active
    }

    /// Opens the toolbar over `surface`, when I may annotate it.
    pub fn open(&self, surface: Surface) {
        let mut s = self.lock();
        if Self::may(&s, surface) {
            Self::finish_drawing(&mut s);
            s.active = Some(surface);
        }
        drop(s);
        self.ctx.request_repaint();
    }

    /// Closes the toolbar.
    pub fn close(&self) {
        let mut s = self.lock();
        Self::finish_drawing(&mut s);
        s.active = None;
        drop(s);
        self.ctx.request_repaint();
    }

    pub fn pen(&self) -> Pen {
        self.lock().pen
    }

    pub fn set_pen(&self, pen: Pen) {
        let mut s = self.lock();
        s.pen = Pen {
            tool: pen.tool,
            color: pen.color.min(COLORS.len() - 1),
            size: pen.size.min(SIZES.len() - 1),
        };
        // The toolbar over my screen shows the pen too.
        if let Some(poke) = &s.on_mine {
            poke();
        }
        drop(s);
        self.ctx.request_repaint();
    }

    /// The toolbars stay out of the next frame (a picture is taken).
    pub fn set_hide_tools(&self, hide: bool) {
        self.lock().hide_tools = hide;
    }

    pub fn tools_hidden(&self) -> bool {
        self.lock().hide_tools
    }

    // ------------------------------------------------------------ drawing

    /// Starts a stroke of my pen at `p` on `page`. False when I may not
    /// annotate there (or the pen is the eraser or the text tool, which
    /// has its own path through [`Ink::insert_text`]).
    pub fn begin(&self, page: Page, p: Pos2) -> bool {
        let mut s = self.lock();
        if !Self::may(&s, page.surface) || s.pen.tool == Tool::Eraser || s.pen.tool == Tool::Text {
            return false;
        }
        Self::finish_drawing(&mut s);
        if s.strokes.len() >= MAX_STROKES {
            return false;
        }
        let pen = s.pen;
        let id = Self::new_id(&mut s);
        let p = clamp(p);
        let stroke = Stroke {
            author: s.me,
            id,
            page,
            tool: pen.tool,
            color: with_alpha(COLORS[pen.color], 255),
            width: SIZES[pen.size],
            points: if pen.tool.shape() {
                vec![p, p]
            } else {
                vec![p]
            },
            text: None,
            done: None,
            touched: Instant::now(),
        };
        Self::send(&s, wire::stroke(&stroke, false));
        s.drawing = Some(Drawing {
            id,
            sent: stroke.points.len(),
            moved: false,
            sent_at: Instant::now(),
        });
        s.strokes.push(stroke);
        s.redo.clear();
        self.changed(&s, page.surface);
        true
    }

    fn new_id(s: &mut State) -> u32 {
        let id = s.next_id;
        s.next_id = s.next_id.wrapping_add(1);
        id
    }

    /// Puts the typed text at `p` (0 to 1 across and down the picture) on
    /// `page`, as a stroke of `Tool::Text`. Nothing comes out when the
    /// text is empty, the pen is not on text, or I cannot annotate here.
    /// The size of the font comes from the pen's size slot (`TEXT_SIZES`).
    pub fn insert_text(&self, page: Page, p: Pos2, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let mut s = self.lock();
        if !Self::may(&s, page.surface) || s.pen.tool != Tool::Text {
            return false;
        }
        Self::finish_drawing(&mut s);
        if s.strokes.len() >= MAX_STROKES {
            return false;
        }
        let pen = s.pen;
        let id = Self::new_id(&mut s);
        let p = clamp(p);
        let mut owned = text.to_owned();
        if owned.len() > MAX_TEXT_BYTES {
            owned.truncate(nearest_char(&owned, MAX_TEXT_BYTES));
        }
        let now = Instant::now();
        let stroke = Stroke {
            author: s.me,
            id,
            page,
            tool: Tool::Text,
            color: with_alpha(COLORS[pen.color], 255),
            width: TEXT_SIZES[pen.size],
            points: vec![p],
            text: Some(owned),
            // A text annotation is already finished when it is placed:
            // there is no further input to come.
            done: Some(now),
            touched: now,
        };
        Self::send(&s, wire::stroke(&stroke, true));
        s.strokes.push(stroke.clone());
        s.undo.push(Action::Drew(stroke));
        s.redo.clear();
        self.changed(&s, page.surface);
        true
    }

    /// My stroke goes on to `p` (a shape: now ends there). When its points
    /// are not all out yet, how long until they should be (`tick`).
    pub fn extend(&self, p: Pos2) -> Option<Duration> {
        let mut s = self.lock();
        let me = s.me;
        let id = s.drawing.as_ref()?.id;
        let p = clamp(p);
        let i = s
            .strokes
            .iter()
            .position(|st| st.author == me && st.id == id)?;
        let surface = s.strokes[i].page.surface;
        let shape = {
            let st = &mut s.strokes[i];
            st.touched = Instant::now();
            if st.tool.shape() {
                st.points[1] = p;
            } else {
                st.points.push(p);
            }
            st.tool.shape()
        };
        if shape && let Some(d) = s.drawing.as_mut() {
            d.moved = true;
        }
        // Too long for one message: it ends here and goes on as another,
        // from the same point.
        if s.strokes[i].points.len() >= MAX_POINTS {
            let page = s.strokes[i].page;
            Self::finish_drawing(&mut s);
            drop(s);
            self.begin(page, p);
            return None;
        }
        let wait = Self::flush(&mut s, false);
        self.changed(&s, surface);
        wait
    }

    /// Sends what of my stroke has not gone out, when it is time (or
    /// `now`). How long until it is, when something is still waiting.
    fn flush(s: &mut State, now: bool) -> Option<Duration> {
        let me = s.me;
        let d = s.drawing.as_ref()?;
        let (id, sent, moved, sent_at) = (d.id, d.sent, d.moved, d.sent_at);
        let st = s.strokes.iter().find(|st| st.author == me && st.id == id)?;
        let waiting = if st.tool.shape() {
            moved
        } else {
            sent < st.points.len()
        };
        if !waiting {
            return None;
        }
        let due = sent_at + SEND_EVERY;
        let at = Instant::now();
        if !now && at < due {
            return Some(due - at);
        }
        let msg = if st.tool.shape() {
            wire::more(id, &st.points[1..2])
        } else {
            wire::more(id, &st.points[sent..])
        };
        let total = st.points.len();
        Self::send(s, msg);
        if let Some(d) = s.drawing.as_mut() {
            d.sent = total;
            d.moved = false;
            d.sent_at = at;
        }
        None
    }

    /// Sends what of my stroke is due (the interface calls it every frame
    /// while I draw). How long until the next part is, if any is waiting.
    pub fn tick(&self) -> Option<Duration> {
        Self::flush(&mut self.lock(), false)
    }

    /// My stroke is finished.
    pub fn finish(&self) {
        let mut s = self.lock();
        let surface = Self::finish_drawing(&mut s);
        if let Some(surface) = surface {
            self.changed(&s, surface);
        }
    }

    fn finish_drawing(s: &mut State) -> Option<Surface> {
        let me = s.me;
        let id = s.drawing.as_ref()?.id;
        let Some(i) = s
            .strokes
            .iter()
            .position(|st| st.author == me && st.id == id)
        else {
            s.drawing = None;
            return None;
        };
        // What has not gone out goes now.
        Self::flush(s, true);
        s.drawing = None;
        let surface = s.strokes[i].page.surface;
        let small_shape = {
            let st = &s.strokes[i];
            st.tool.shape() && (st.points[1] - st.points[0]).length() < MIN_SHAPE
        };
        if small_shape {
            // A click with a shape: nothing to keep.
            let st = s.strokes.remove(i);
            Self::send(s, wire::delete(surface, &[st.key()]));
            return Some(surface);
        }
        s.strokes[i].done = Some(Instant::now());
        Self::send(s, wire::end(id));
        if s.strokes[i].tool != Tool::Vanish {
            let copy = s.strokes[i].clone();
            s.undo.push(Action::Drew(copy));
        }
        Some(surface)
    }

    /// The eraser goes down on `page`: what it takes away until it is
    /// lifted (`end_erase`) is one thing to undo.
    pub fn begin_erase(&self) {
        let mut s = self.lock();
        Self::finish_drawing(&mut s);
        s.erasing = Some(Vec::new());
    }

    /// The eraser at `p` on `page`: takes away the strokes within `tol`
    /// pixels of it that I may take away (mine; anyone's, on my own
    /// surface), for a picture shown `size` pixels big.
    pub fn erase(&self, page: Page, p: Pos2, size: Vec2, tol: f32) {
        let mut s = self.lock();
        let me = s.me;
        let owner = page.surface.owner == me;
        let hit: Vec<(i64, u32)> = s
            .strokes
            .iter()
            .filter(|st| st.page == page && (st.author == me || owner))
            .filter(|st| near(st, p, size, tol))
            .map(Stroke::key)
            .collect();
        if hit.is_empty() {
            return;
        }
        let removed = Self::take(&mut s, &hit);
        Self::send(&s, wire::delete(page.surface, &hit));
        let kept: Vec<Stroke> = removed
            .into_iter()
            .filter(|st| st.tool != Tool::Vanish)
            .collect();
        match s.erasing.as_mut() {
            Some(list) => list.extend(kept),
            None if !kept.is_empty() => s.undo.push(Action::Erased(kept)),
            None => {}
        }
        s.redo.clear();
        self.changed(&s, page.surface);
    }

    pub fn end_erase(&self) {
        let mut s = self.lock();
        if let Some(list) = s.erasing.take()
            && !list.is_empty()
        {
            s.undo.push(Action::Erased(list));
        }
    }

    /// Takes the strokes of `keys` out of the list, and returns them.
    fn take(s: &mut State, keys: &[(i64, u32)]) -> Vec<Stroke> {
        let mut out = Vec::new();
        s.strokes.retain(|st| {
            if keys.contains(&st.key()) {
                out.push(st.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// Puts strokes back (undo of the eraser, redo of a stroke) and tells
    /// everyone: someone else's only on my own surface, as its owner.
    fn put_back(s: &mut State, list: &[Stroke]) {
        for st in list {
            if !s.surfaces.contains_key(&st.page.surface) {
                continue;
            }
            let theirs = st.author != s.me;
            if theirs && st.page.surface.owner != s.me {
                continue;
            }
            if s.strokes.iter().any(|o| o.key() == st.key()) {
                continue;
            }
            Self::send(s, wire::stroke(st, theirs));
            s.strokes.push(st.clone());
        }
    }

    /// Takes back what I did last (on something still annotated).
    pub fn undo(&self) {
        let mut s = self.lock();
        Self::finish_drawing(&mut s);
        while let Some(action) = s.undo.pop() {
            let Some(surface) = action.surface().filter(|k| s.surfaces.contains_key(k)) else {
                continue;
            };
            match action {
                Action::Drew(st) => {
                    if Self::take(&mut s, &[st.key()]).is_empty() {
                        // Someone took it away already.
                        continue;
                    }
                    Self::send(&s, wire::delete(surface, &[st.key()]));
                    s.redo.push(Action::Drew(st));
                }
                Action::Erased(list) => {
                    Self::put_back(&mut s, &list);
                    s.redo.push(Action::Erased(list));
                }
            }
            self.changed(&s, surface);
            return;
        }
    }

    /// Does again what I took back last.
    pub fn redo(&self) {
        let mut s = self.lock();
        Self::finish_drawing(&mut s);
        while let Some(action) = s.redo.pop() {
            let Some(surface) = action.surface().filter(|k| s.surfaces.contains_key(k)) else {
                continue;
            };
            match action {
                Action::Drew(st) => {
                    Self::put_back(&mut s, std::slice::from_ref(&st));
                    s.undo.push(Action::Drew(st));
                }
                Action::Erased(list) => {
                    let keys: Vec<(i64, u32)> = list.iter().map(Stroke::key).collect();
                    Self::take(&mut s, &keys);
                    Self::send(&s, wire::delete(surface, &keys));
                    s.undo.push(Action::Erased(list));
                }
            }
            self.changed(&s, surface);
            return;
        }
    }

    pub fn can_undo(&self) -> bool {
        let s = self.lock();
        s.undo
            .iter()
            .any(|a| a.surface().is_some_and(|k| s.surfaces.contains_key(&k)))
    }

    pub fn can_redo(&self) -> bool {
        let s = self.lock();
        s.redo
            .iter()
            .any(|a| a.surface().is_some_and(|k| s.surfaces.contains_key(&k)))
    }

    /// My strokes on `surface` go (`all`: everyone's, when it is mine).
    pub fn clear_surface(&self, surface: Surface, all: bool) {
        let mut s = self.lock();
        Self::finish_drawing(&mut s);
        let me = s.me;
        if all && surface.owner != me {
            return;
        }
        let keys: Vec<(i64, u32)> = s
            .strokes
            .iter()
            .filter(|st| st.page.surface == surface && (all || st.author == me))
            .map(Stroke::key)
            .collect();
        if keys.is_empty() {
            return;
        }
        let removed: Vec<Stroke> = Self::take(&mut s, &keys)
            .into_iter()
            .filter(|st| st.tool != Tool::Vanish)
            .collect();
        Self::send(&s, wire::clear(surface, all));
        if !removed.is_empty() {
            s.undo.push(Action::Erased(removed));
        }
        s.redo.clear();
        self.changed(&s, surface);
    }

    /// Whether there is anything on `surface` to clear: mine, and anyone's.
    pub fn has_strokes(&self, surface: Surface) -> (bool, bool) {
        let s = self.lock();
        let mut on = s.strokes.iter().filter(|st| st.page.surface == surface);
        let any = on.clone().next().is_some();
        (on.any(|st| st.author == s.me), any)
    }

    // ------------------------------------------------------------ arriving

    /// A message from `from` over their data channel.
    pub fn arrived(&self, from: i64, v: &Value) {
        let Some(msg) = wire::read(v) else {
            return;
        };
        let mut s = self.lock();
        let touched = match msg {
            wire::Msg::Surfaces(list) => {
                let had: Vec<Surface> = s
                    .surfaces
                    .keys()
                    .filter(|k| k.owner == from)
                    .copied()
                    .collect();
                for k in had {
                    if !list.iter().any(|(id, ..)| *id == k.id) {
                        Self::drop_surface(&mut s, k);
                    }
                }
                for (id, kind, everyone) in list {
                    let surface = Surface { owner: from, id };
                    s.surfaces.insert(surface, Info { kind, everyone });
                    if !everyone && s.active == Some(surface) {
                        // Only its owner may annotate it now.
                        Self::finish_drawing(&mut s);
                        s.active = None;
                    }
                }
                None
            }
            wire::Msg::Stroke {
                page,
                id,
                author,
                tool,
                color,
                width,
                points,
                text,
                done,
            } => {
                let surface = page.surface;
                let author = match author {
                    // Someone else's, handed on by the surface's owner.
                    Some(a) if from == surface.owner => a,
                    Some(_) => return,
                    None => from,
                };
                let Some(info) = s.surfaces.get(&surface) else {
                    return;
                };
                if from != surface.owner && !info.everyone {
                    return;
                }
                let now = Instant::now();
                let mut points = points;
                if tool.shape() {
                    let last = *points.last().unwrap_or(&points[0]);
                    points.truncate(1);
                    points.push(last);
                }
                let text = text.filter(|_| tool == Tool::Text).map(|t| {
                    // The length cap keeps a mislaid message from using up
                    // all of someone's memory.
                    let mut t = t;
                    if t.len() > MAX_TEXT_BYTES {
                        t.truncate(nearest_char(&t, MAX_TEXT_BYTES));
                    }
                    t
                });
                let stroke = Stroke {
                    author,
                    id,
                    page,
                    tool,
                    color,
                    width,
                    points,
                    text,
                    done: done.then_some(now),
                    touched: now,
                };
                match s.strokes.iter().position(|st| st.key() == (author, id)) {
                    Some(i) => s.strokes[i] = stroke,
                    None if s.strokes.len() < MAX_STROKES => s.strokes.push(stroke),
                    None => return,
                }
                Some(surface)
            }
            wire::Msg::More { id, points } => {
                let Some(st) = s
                    .strokes
                    .iter_mut()
                    .find(|st| st.key() == (from, id) && st.done.is_none())
                else {
                    return;
                };
                if st.tool.shape() {
                    if let Some(p) = points.last() {
                        st.points[1] = *p;
                    }
                } else {
                    let room = MAX_POINTS.saturating_sub(st.points.len());
                    st.points.extend(points.into_iter().take(room));
                }
                st.touched = Instant::now();
                Some(st.page.surface)
            }
            wire::Msg::End { id } => {
                let Some(st) = s.strokes.iter_mut().find(|st| st.key() == (from, id)) else {
                    return;
                };
                st.done.get_or_insert_with(Instant::now);
                Some(st.page.surface)
            }
            wire::Msg::Delete { surface, strokes } => {
                let allowed: Vec<(i64, u32)> = strokes
                    .into_iter()
                    .filter(|(author, _)| *author == from || from == surface.owner)
                    .collect();
                let gone = Self::take(&mut s, &allowed);
                gone.first().map(|st| st.page.surface)
            }
            wire::Msg::Clear { surface, all } => {
                if all && from != surface.owner {
                    return;
                }
                let before = s.strokes.len();
                s.strokes
                    .retain(|st| !(st.page.surface == surface && (all || st.author == from)));
                (s.strokes.len() != before).then_some(surface)
            }
        };
        match touched {
            Some(surface) => self.changed(&s, surface),
            None => self.ctx.request_repaint(),
        }
    }

    // ------------------------------------------------------------ showing

    /// The strokes on `page` as they show at `now`, and when that changes
    /// by itself (ink fading, a name hiding), if it does. Disappearing ink
    /// that has faded goes.
    pub fn looks(&self, page: Page, now: Instant) -> (Vec<Look>, Option<Instant>) {
        let mut s = self.lock();
        let before = s.strokes.len();
        s.strokes.retain(|st| !st.gone(now));
        if s.strokes.len() != before {
            self.changed(&s, page.surface);
        }
        let me = s.me;
        let mut next: Option<Instant> = None;
        let mut soon = |t: Instant| {
            next = Some(next.map_or(t, |n| n.min(t)));
        };
        let looks = s
            .strokes
            .iter()
            .filter(|st| st.page == page)
            .map(|st| {
                let ended = st.ended(now);
                match ended {
                    None => soon(st.touched + STALE),
                    Some(e) if st.tool == Tool::Vanish => {
                        let hold = e + VANISH_HOLD;
                        // While it fades: every frame.
                        soon(if now < hold { hold } else { now });
                    }
                    Some(_) => {}
                }
                let tag = st.author != me && ended.is_none_or(|e| now < e + NAME_FOR);
                if st.author != me
                    && let Some(e) = ended.filter(|e| now < *e + NAME_FOR)
                {
                    soon(e + NAME_FOR);
                }
                look(st, st.opacity(now), tag)
            })
            .collect();
        (looks, next)
    }

    /// For the tests' report: what can be annotated and what is on it.
    pub fn report(&self) -> Value {
        let s = self.lock();
        let mut surfaces: Vec<Value> = s
            .surfaces
            .iter()
            .map(|(k, i)| {
                let mut by: HashMap<String, usize> = HashMap::new();
                let mut points = 0;
                for st in s.strokes.iter().filter(|st| st.page.surface == *k) {
                    *by.entry(st.author.to_string()).or_default() += 1;
                    points += st.points.len();
                }
                json!({
                    "owner": k.owner,
                    "id": k.id,
                    "kind": match i.kind { Kind::Screen => "screen", Kind::Slides => "slides" },
                    "everyone": i.everyone,
                    "strokesBy": by,
                    "points": points,
                })
            })
            .collect();
        surfaces.sort_by_key(|v| (v["owner"].as_i64(), v["id"].as_u64()));
        json!({
            "surfaces": surfaces,
            "active": s.active.map(|k| json!({ "owner": k.owner, "id": k.id })),
            "tool": s.pen.tool.wire(),
            "undo": s.undo.len(),
        })
    }
}

fn clamp(p: Pos2) -> Pos2 {
    pos2(p.x.clamp(0.0, 1.0), p.y.clamp(0.0, 1.0))
}

fn with_alpha([r, g, b]: [u8; 3], a: u8) -> [u8; 4] {
    [r, g, b, a]
}

/// How `st` shows at `opacity`.
fn look(st: &Stroke, opacity: f32, tag: bool) -> Look {
    let (width, alpha) = match st.tool {
        Tool::Highlighter => (st.width * MARKER_WIDTH, MARKER_ALPHA),
        _ => (st.width, 1.0),
    };
    let [r, g, b, a] = st.color;
    Look {
        tool: st.tool,
        text: st.text.clone(),
        color: [r, g, b, (f32::from(a) * alpha * opacity).round() as u8],
        width,
        points: st.points.clone(),
        author: st.author,
        tag,
    }
}

// ---------------------------------------------------------------- shapes

/// The lines a stroke is drawn as, in the units of `map` (which takes a
/// point of the picture, 0 to 1, to where it shows), for a picture
/// `height` of those units tall: freehand as it was drawn, the shapes from
/// their two points. Each line is drawn on its own.
pub fn lines(
    tool: Tool,
    points: &[Pos2],
    width: f32,
    map: impl Fn(Pos2) -> Pos2,
    height: f32,
) -> Vec<Vec<Pos2>> {
    if points.is_empty() || tool == Tool::Text {
        // Text has its own render path (an egui layout, or Windows' own
        // text through the overlay): nothing comes out as a line.
        return Vec::new();
    }
    if !tool.shape() {
        return vec![points.iter().map(|p| map(*p)).collect()];
    }
    let a = map(points[0]);
    let b = map(*points.last().unwrap_or(&points[0]));
    match tool {
        Tool::Rect => {
            let (min, max) = (a.min(b), a.max(b));
            vec![vec![min, pos2(max.x, min.y), max, pos2(min.x, max.y), min]]
        }
        Tool::Ellipse => {
            let center = a + (b - a) / 2.0;
            let radius = ((b - a) / 2.0).abs();
            let steps = 72;
            vec![
                (0..=steps)
                    .map(|i| {
                        let t = i as f32 / steps as f32 * std::f32::consts::TAU;
                        center + vec2(radius.x * t.cos(), radius.y * t.sin())
                    })
                    .collect(),
            ]
        }
        _ => {
            // An arrow: its shaft, and a head as long as a few widths
            // (at least a little of the picture).
            let along = b - a;
            let len = along.length();
            if len < 1e-3 {
                return vec![vec![a, b]];
            }
            let dir = along / len;
            let head = (width * height * 4.5).max(height * 0.02).min(len * 0.6);
            let back = b - dir * head;
            let side = vec2(-dir.y, dir.x) * head * 0.55;
            vec![vec![a, b], vec![back + side, b, back - side]]
        }
    }
}

/// Whether `p` (0 to 1) is within `tol` pixels of what `st` draws, on a
/// picture shown `size` pixels big.
fn near(st: &Stroke, p: Pos2, size: Vec2, tol: f32) -> bool {
    let map = |q: Pos2| pos2(q.x * size.x, q.y * size.y);
    let at = map(p);
    let reach = tol
        + st.width
            * size.y
            * if st.tool == Tool::Highlighter {
                MARKER_WIDTH
            } else {
                1.0
            }
            / 2.0;
    lines(st.tool, &st.points, st.width, map, size.y)
        .iter()
        .any(|line| match line.as_slice() {
            [only] => (*only - at).length() <= reach,
            many => many
                .windows(2)
                .any(|w| distance_to_segment(at, w[0], w[1]) <= reach),
        })
}

fn distance_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink_with_screen(id: u32) -> (Arc<Ink>, Page) {
        let ink = Ink::new(egui::Context::default());
        ink.set_me(1);
        let surface = ink.open_mine(Kind::Screen, id);
        (ink, Page { surface, n: 0 })
    }

    #[test]
    fn text_tool_makes_a_visible_look() {
        let (ink, page) = ink_with_screen(42);
        ink.set_pen(Pen {
            tool: Tool::Text,
            color: 0,
            size: 1,
        });
        assert!(ink.insert_text(page, pos2(0.3, 0.4), "Olá, mundo"));
        let (looks, _) = ink.looks(page, Instant::now());
        let t = looks
            .iter()
            .find(|l| l.tool == Tool::Text)
            .expect("a text look");
        assert_eq!(t.text.as_deref(), Some("Olá, mundo"));
        assert!(t.color[3] > 0, "text would be invisible");
        assert_eq!(t.points.len(), 1, "text has one anchor point");
    }

    #[test]
    fn insert_text_refuses_when_the_pen_is_not_text() {
        let (ink, page) = ink_with_screen(7);
        ink.set_pen(Pen {
            tool: Tool::Pen,
            color: 0,
            size: 1,
        });
        assert!(!ink.insert_text(page, pos2(0.1, 0.1), "x"));
    }

    #[test]
    fn insert_text_refuses_blank_text() {
        let (ink, page) = ink_with_screen(7);
        ink.set_pen(Pen {
            tool: Tool::Text,
            color: 0,
            size: 1,
        });
        assert!(!ink.insert_text(page, pos2(0.1, 0.1), "   "));
    }

    #[test]
    fn a_text_stroke_can_be_undone() {
        let (ink, page) = ink_with_screen(7);
        ink.set_pen(Pen {
            tool: Tool::Text,
            color: 0,
            size: 1,
        });
        ink.insert_text(page, pos2(0.1, 0.1), "hello");
        assert_eq!(ink.looks(page, Instant::now()).0.len(), 1);
        ink.undo();
        assert_eq!(ink.looks(page, Instant::now()).0.len(), 0);
    }
}

//! Presenting a file in a call, the way Teams' PowerPoint Live shows the
//! deck itself instead of a video of it. The presenter's PC draws the
//! slides of a .pptx (BetterOffice, in Rust, inside the .exe) or of a PDF
//! (Windows' own reader), where the presentation's fonts are, and the
//! pictures go to everyone in the call over its data channel, person to
//! person, the slide on screen first (voice/rtc.rs). Going to another slide
//! is a message of a few bytes, and everyone sees it at once and sharp, with
//! no video compression: nobody else needs PowerPoint or the fonts, and
//! nobody opens someone else's file, only pictures.
//!
//! This module keeps both sides: my presentation (what render.rs draws and
//! the call sends out) and everyone else's (what arrives), and turns the
//! pictures into textures at the size they are shown.

pub mod fonts;
mod pdf;
mod pptx;
pub mod render;
pub mod timing;
pub mod wire;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use egui::load::SizedTexture;
use egui::{ColorImage, TextureHandle, TextureOptions};
use serde_json::{Value, json};
use tokio::sync::watch;

/// The pictures' largest size: a 16:9 slide comes out 1920 × 1080, a 4:3
/// one 1440 × 1080 (what a full HD screen shows of either).
pub const MAX_W: u32 = 1920;
pub const MAX_H: u32 = 1080;

/// Everything held of someone else's presentation, whole pictures and
/// pieces on their way: past this, what more comes is dropped.
const MAX_HELD: usize = 200 * 1024 * 1024;

/// Pictures arriving at the same time from one presenter (who only goes
/// ahead with the slide on screen and the next ones).
const MAX_ARRIVING: usize = 8;

/// How many bytes of pictures may be on their way to one person (sent, not
/// yet acknowledged): the slide on screen and the next one get more room,
/// the rest less, and both are shared out between everyone receiving, so
/// what queues on a slow connection is a fraction of a second. The voice's
/// pings and the requests for full pictures go over the same channel and
/// wait behind at most this.
const URGENT_ROOM: usize = 128 * 1024;
const URGENT_ROOM_EACH: (usize, usize) = (32 * 1024, 64 * 1024);
const BACKGROUND_ROOM: usize = 64 * 1024;
const BACKGROUND_ROOM_EACH: (usize, usize) = (16 * 1024, 32 * 1024);

/// Whose presentation: mine, or someone's in the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    Me,
    Them(i64),
}

/// A presentation, mine or someone's.
struct Deck {
    id: u32,
    name: String,
    /// The pictures' size (a PDF's pages may differ), for the room a slide
    /// takes before its picture is here.
    size: [u32; 2],
    /// The presenter's slide.
    at: usize,
    /// Each slide's picture as it travels (PNG, or JPEG for photos).
    pictures: Vec<Option<Arc<[u8]>>>,
    since: Instant,
}

/// What I present.
struct Mine {
    deck: Deck,
    /// The file is open: the number of slides is known.
    opened: bool,
    /// Every slide is drawn.
    drawn: bool,
    /// Slides that could not be drawn (a grey picture goes in their place).
    failed: Vec<bool>,
    notes: Vec<String>,
    /// How many slides each person receiving has whole.
    sent: HashMap<i64, usize>,
}

/// Someone else's.
struct Theirs {
    deck: Deck,
    /// The connection it came over (see `Slides::forget`).
    link: u64,
    /// Pictures on their way: what arrived of each so far.
    arriving: HashMap<usize, Vec<u8>>,
    /// The bytes held, whole pictures and pieces.
    held: usize,
}

#[derive(Default)]
struct State {
    mine: Option<Mine>,
    theirs: HashMap<i64, Theirs>,
}

/// A presentation as the interface shows it.
#[derive(Clone, Debug)]
pub struct View {
    pub owner: Owner,
    pub id: u32,
    pub name: String,
    /// 0 while my file is still opening.
    pub count: usize,
    pub at: usize,
    pub size: [u32; 2],
    /// Which slides' pictures are here.
    pub ready: Vec<bool>,
    pub since: Instant,
    /// Mine: every slide drawn, which ones failed, and how many people
    /// receive it, how many of them have it all, and how much of it has
    /// gone out (0 to 1).
    pub drawn: bool,
    pub failed: Vec<bool>,
    pub people: usize,
    pub people_done: usize,
    pub sent: f32,
}

/// What one person has been sent of my presentation (each sender in
/// voice/rtc.rs keeps one).
#[derive(Default)]
pub struct Progress {
    /// The presentation and slide they were last told about.
    told: Option<(u32, usize)>,
    /// How much of each picture went to them.
    sent: Vec<usize>,
}

/// What a sender does next.
pub enum Step {
    /// A message for the channel.
    Say(Value),
    /// A piece of a picture; `urgent` for the slide on screen and the next.
    Piece { bytes: Vec<u8>, urgent: bool },
    /// Nothing until my presentation changes.
    Wait,
}

/// The order slides go in for someone on slide `at`: that one, then one
/// ahead and one back, two ahead and two back, and so on.
fn rank(slide: usize, at: usize) -> usize {
    if slide >= at {
        (slide - at) * 2
    } else {
        (at - slide) * 2 + 1
    }
}

pub struct Slides {
    ctx: egui::Context,
    state: Mutex<State>,
    /// Bumped whenever my presentation changes: each sender waits on it.
    changed: watch::Sender<u64>,
    textures: Arc<Textures>,
}

impl Slides {
    pub fn new(ctx: egui::Context) -> Arc<Slides> {
        Arc::new(Slides {
            textures: Arc::new(Textures::new(ctx.clone())),
            ctx,
            state: Mutex::default(),
            changed: watch::Sender::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// My presentation changed: the senders take another look, and so does
    /// the interface.
    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
        self.ctx.request_repaint();
    }

    // ------------------------------------------------------------ mine

    /// Starts presenting `name` as `id`, empty until the file opens.
    pub fn start_mine(&self, id: u32, name: String) {
        let old = self.lock().mine.replace(Mine {
            deck: Deck {
                id,
                name,
                size: [16, 9],
                at: 0,
                pictures: Vec::new(),
                since: Instant::now(),
            },
            opened: false,
            drawn: false,
            failed: Vec::new(),
            notes: Vec::new(),
            sent: HashMap::new(),
        });
        if let Some(old) = old {
            self.textures.forget(Owner::Me, Some(old.deck.id));
        }
        self.bump();
    }

    /// The presentation I present, while I do.
    pub fn mine_id(&self) -> Option<u32> {
        self.lock().mine.as_ref().map(|m| m.deck.id)
    }

    /// The file of `id` opened: so many slides, pictures of about this
    /// size, and their notes. False when `id` is no longer presented.
    pub fn opened_mine(&self, id: u32, count: usize, size: [u32; 2], notes: Vec<String>) -> bool {
        {
            let mut state = self.lock();
            let Some(m) = state.mine.as_mut().filter(|m| m.deck.id == id) else {
                return false;
            };
            m.deck.pictures = vec![None; count];
            m.deck.size = size;
            m.deck.at = m.deck.at.min(count.saturating_sub(1));
            m.failed = vec![false; count];
            m.notes = notes;
            m.opened = true;
        }
        self.bump();
        true
    }

    /// The slide of `id` to draw next: the nearest to the one on screen
    /// not drawn yet. None once all are, or when `id` is no longer
    /// presented.
    pub fn next_to_draw(&self, id: u32) -> Option<usize> {
        let state = self.lock();
        let deck = &state.mine.as_ref().filter(|m| m.deck.id == id)?.deck;
        (0..deck.pictures.len())
            .filter(|&i| deck.pictures[i].is_none())
            .min_by_key(|&i| rank(i, deck.at))
    }

    /// Slide `slide` of `id` is drawn: its picture, or a grey one in its
    /// place when it `failed`.
    pub fn drawn_mine(&self, id: u32, slide: usize, picture: Vec<u8>, failed: bool) {
        {
            let mut state = self.lock();
            let Some(m) = state.mine.as_mut().filter(|m| m.deck.id == id) else {
                return;
            };
            if slide >= m.deck.pictures.len() {
                return;
            }
            m.deck.pictures[slide] = Some(picture.into());
            m.failed[slide] = failed;
        }
        self.bump();
    }

    /// Every slide of `id` is drawn.
    pub fn finished_mine(&self, id: u32) {
        if let Some(m) = self.lock().mine.as_mut().filter(|m| m.deck.id == id) {
            m.drawn = true;
        }
        self.ctx.request_repaint();
    }

    /// Stops presenting (whatever was presented: its id).
    pub fn stop_mine(&self) -> Option<u32> {
        let old = self.lock().mine.take()?;
        self.textures.forget(Owner::Me, None);
        self.bump();
        Some(old.deck.id)
    }

    /// Stops presenting `id`, if that is still what I present (a file that
    /// did not open).
    pub fn stop_mine_if(&self, id: u32) {
        let mine = self.lock().mine.as_ref().map(|m| m.deck.id);
        if mine == Some(id) {
            self.stop_mine();
        }
    }

    /// The presenter goes to `slide`.
    pub fn go_to(&self, slide: usize) {
        {
            let mut state = self.lock();
            let Some(m) = state.mine.as_mut().filter(|m| m.opened) else {
                return;
            };
            let slide = slide.min(m.deck.pictures.len().saturating_sub(1));
            if slide == m.deck.at {
                return;
            }
            m.deck.at = slide;
        }
        self.bump();
    }

    /// My notes for `slide`.
    pub fn notes(&self, slide: usize) -> String {
        self.lock()
            .mine
            .as_ref()
            .and_then(|m| m.notes.get(slide).cloned())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ sending

    /// Wakes up whenever my presentation changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// What the sender to `uid` (who keeps `p`) does next: say what I
    /// present or that I stopped, say which slide I am on, or send them a
    /// piece of the picture they lack that is nearest my slide.
    pub fn next_step(&self, uid: i64, p: &mut Progress) -> Step {
        let mut state = self.lock();
        let Some(m) = state.mine.as_mut().filter(|m| m.opened) else {
            return match p.told.take() {
                Some((id, _)) => {
                    p.sent.clear();
                    Step::Say(wire::ended(id))
                }
                None => Step::Wait,
            };
        };
        m.sent.entry(uid).or_insert(0);
        let deck = &m.deck;
        match p.told {
            Some((id, _)) if id != deck.id => {
                p.told = None;
                p.sent.clear();
                return Step::Say(wire::ended(id));
            }
            None => {
                p.told = Some((deck.id, deck.at));
                p.sent = vec![0; deck.pictures.len()];
                return Step::Say(wire::announce(
                    deck.id,
                    deck.pictures.len(),
                    deck.size,
                    &deck.name,
                    deck.at,
                ));
            }
            Some((_, at)) if at != deck.at => {
                p.told = Some((deck.id, deck.at));
                return Step::Say(wire::moved(deck.id, deck.at));
            }
            Some(_) => {}
        }
        let next = (0..deck.pictures.len())
            .filter(|&i| {
                deck.pictures[i]
                    .as_ref()
                    .is_some_and(|pic| p.sent[i] < pic.len())
            })
            .min_by_key(|&i| rank(i, deck.at));
        let Some(slide) = next else {
            return Step::Wait;
        };
        let Some(picture) = &deck.pictures[slide] else {
            return Step::Wait;
        };
        let from = p.sent[slide];
        let to = (from + wire::PIECE).min(picture.len());
        let bytes = wire::piece(deck.id, slide, picture.len(), from, &picture[from..to]);
        p.sent[slide] = to;
        let urgent = slide == deck.at || slide == deck.at + 1;
        let finished = to == picture.len();
        if finished {
            let whole = deck
                .pictures
                .iter()
                .zip(&p.sent)
                .filter(|(pic, sent)| pic.as_ref().is_some_and(|pic| **sent >= pic.len()))
                .count();
            m.sent.insert(uid, whole);
        }
        drop(state);
        if finished {
            // The presenter's view says how far the sending is.
            self.ctx.request_repaint();
        }
        Step::Piece { bytes, urgent }
    }

    /// The room a sender has (see `URGENT_ROOM`).
    pub fn room(&self, urgent: bool) -> usize {
        let people = self.lock().mine.as_ref().map_or(1, |m| m.sent.len().max(1));
        if urgent {
            (URGENT_ROOM / people).clamp(URGENT_ROOM_EACH.0, URGENT_ROOM_EACH.1)
        } else {
            (BACKGROUND_ROOM / people).clamp(BACKGROUND_ROOM_EACH.0, BACKGROUND_ROOM_EACH.1)
        }
    }

    /// The sender to `uid` is gone (they left, or the connection closed).
    pub fn sender_gone(&self, uid: i64) {
        if let Some(m) = self.lock().mine.as_mut() {
            m.sent.remove(&uid);
        }
    }

    // ------------------------------------------------------------ arriving

    /// `uid` presents `a` (over connection `link`), or says so again.
    pub fn arrived_deck(&self, uid: i64, link: u64, a: wire::Announced) {
        {
            let mut state = self.lock();
            match state.theirs.get_mut(&uid) {
                Some(t) if t.deck.id == a.id => {
                    t.deck.at = a.at;
                    t.link = link;
                }
                _ => {
                    state.theirs.insert(
                        uid,
                        Theirs {
                            deck: Deck {
                                id: a.id,
                                name: a.name,
                                size: a.size,
                                at: a.at,
                                pictures: vec![None; a.count],
                                since: Instant::now(),
                            },
                            link,
                            arriving: HashMap::new(),
                            held: 0,
                        },
                    );
                }
            }
        }
        self.textures.forget(Owner::Them(uid), Some(a.id));
        self.ctx.request_repaint();
    }

    /// `uid` went to slide `at` of `id`.
    pub fn arrived_move(&self, uid: i64, id: u32, at: usize) {
        if let Some(t) = self.lock().theirs.get_mut(&uid).filter(|t| t.deck.id == id) {
            t.deck.at = at.min(t.deck.pictures.len().saturating_sub(1));
        }
        self.ctx.request_repaint();
    }

    /// `uid` stopped presenting `id`.
    pub fn arrived_end(&self, uid: i64, id: u32) {
        let gone = {
            let mut state = self.lock();
            let ends = state.theirs.get(&uid).is_some_and(|t| t.deck.id == id);
            ends && state.theirs.remove(&uid).is_some()
        };
        if gone {
            self.textures.forget(Owner::Them(uid), None);
            self.ctx.request_repaint();
        }
    }

    /// A piece of a picture from `uid`.
    pub fn arrived_piece(&self, uid: i64, bytes: &[u8]) {
        let Some(piece) = wire::read_piece(bytes) else {
            return;
        };
        let whole = {
            let mut state = self.lock();
            let Some(t) = state
                .theirs
                .get_mut(&uid)
                .filter(|t| t.deck.id == piece.deck)
            else {
                return;
            };
            if t.deck.pictures.get(piece.slide).is_none_or(Option::is_some) {
                return;
            }
            if !t.arriving.contains_key(&piece.slide) && t.arriving.len() >= MAX_ARRIVING {
                return;
            }
            let buf = t.arriving.entry(piece.slide).or_default();
            if piece.offset != buf.len() {
                // The picture started over (or a piece went missing, which
                // the channel never does): only its start is any good.
                t.held -= buf.len();
                buf.clear();
                if piece.offset != 0 {
                    t.arriving.remove(&piece.slide);
                    return;
                }
            }
            if t.held + piece.data.len() > MAX_HELD {
                return;
            }
            buf.extend_from_slice(piece.data);
            t.held += piece.data.len();
            if buf.len() < piece.total {
                return;
            }
            let picture = t.arriving.remove(&piece.slide).unwrap_or_default();
            t.deck.pictures[piece.slide] = Some(picture.into());
            if t.deck.pictures.iter().all(Option::is_some) {
                crate::logln!(
                    "apresentação de {uid}: {} slides chegaram em {} ms ({} KB)",
                    t.deck.pictures.len(),
                    t.deck.since.elapsed().as_millis(),
                    t.held / 1024
                );
            }
            true
        };
        if whole {
            self.ctx.request_repaint();
        }
    }

    /// `uid`'s connection `link` closed: what came over it goes. (A
    /// connection made again since keeps what came over it.)
    pub fn forget(&self, uid: i64, link: u64) {
        let gone = {
            let mut state = self.lock();
            let ends = state.theirs.get(&uid).is_some_and(|t| t.link == link);
            ends && state.theirs.remove(&uid).is_some()
        };
        if gone {
            self.textures.forget(Owner::Them(uid), None);
            self.ctx.request_repaint();
        }
    }

    /// The call is over: nothing of anyone's, nor mine, is kept.
    pub fn clear(&self) {
        {
            let mut state = self.lock();
            state.mine = None;
            state.theirs.clear();
        }
        self.textures.clear();
        self.bump();
    }

    // ------------------------------------------------------------ the interface

    /// My presentation, while I present.
    pub fn mine(&self) -> Option<View> {
        let state = self.lock();
        let m = state.mine.as_ref()?;
        let count = m.deck.pictures.len();
        let people = m.sent.len();
        let people_done = m.sent.values().filter(|n| **n >= count).count();
        let sent = if people == 0 || count == 0 {
            1.0
        } else {
            m.sent.values().sum::<usize>() as f32 / (people * count) as f32
        };
        Some(View {
            owner: Owner::Me,
            id: m.deck.id,
            name: m.deck.name.clone(),
            count,
            at: m.deck.at,
            size: m.deck.size,
            ready: m.deck.pictures.iter().map(Option::is_some).collect(),
            since: m.deck.since,
            drawn: m.drawn,
            failed: m.failed.clone(),
            people,
            people_done,
            sent,
        })
    }

    /// Everyone else's presentations, the newest first.
    pub fn theirs(&self) -> Vec<View> {
        let state = self.lock();
        let mut list: Vec<View> = state
            .theirs
            .iter()
            .map(|(uid, t)| View {
                owner: Owner::Them(*uid),
                id: t.deck.id,
                name: t.deck.name.clone(),
                count: t.deck.pictures.len(),
                at: t.deck.at,
                size: t.deck.size,
                ready: t.deck.pictures.iter().map(Option::is_some).collect(),
                since: t.deck.since,
                drawn: true,
                failed: Vec::new(),
                people: 0,
                people_done: 0,
                sent: 1.0,
            })
            .collect();
        list.sort_by_key(|v| std::cmp::Reverse(v.since));
        list
    }

    /// Who presents (besides me).
    pub fn presenters(&self) -> HashSet<i64> {
        self.lock().theirs.keys().copied().collect()
    }

    /// Whether `uid` presents.
    pub fn presenting(&self, uid: i64) -> bool {
        self.lock().theirs.contains_key(&uid)
    }

    /// Slide `slide` of presentation `id`, scaled down to `width` pixels
    /// (in steps), as a texture; None until it is ready. Asking is what
    /// makes it: on a thread of its own, `urgency` 0 first (what is on
    /// screen), and meanwhile the same slide at another size, if there is
    /// one, stands in.
    pub fn texture(
        &self,
        owner: Owner,
        id: u32,
        slide: usize,
        width: f32,
        urgency: u8,
    ) -> Option<SizedTexture> {
        let (picture, full) = {
            let state = self.lock();
            let deck = match owner {
                Owner::Me => state.mine.as_ref().map(|m| &m.deck),
                Owner::Them(uid) => state.theirs.get(&uid).map(|t| &t.deck),
            }
            .filter(|d| d.id == id)?;
            (deck.pictures.get(slide)?.clone(), deck.size[0])
        };
        let width = if width >= full as f32 {
            0
        } else {
            (width.max(64.0) as u32).next_multiple_of(64)
        };
        let key = TexKey {
            owner,
            id,
            slide,
            width,
        };
        self.textures.get(key, picture, urgency)
    }

    /// For the tests' report: what I present and what arrived of everyone
    /// else's.
    pub fn report(&self) -> Value {
        let state = self.lock();
        let mine = state.mine.as_ref().map(|m| {
            json!({
                "id": m.deck.id,
                "name": m.deck.name,
                "count": m.deck.pictures.len(),
                "at": m.deck.at,
                "opened": m.opened,
                "drawn": m.deck.pictures.iter().filter(|p| p.is_some()).count(),
                "failed": m.failed.iter().filter(|f| **f).count(),
                "bytes": m.deck.pictures.iter().flatten().map(|p| p.len()).sum::<usize>(),
                "sentTo": m.sent.iter().map(|(uid, n)| (uid.to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
            })
        });
        let theirs: Vec<Value> = state
            .theirs
            .iter()
            .map(|(uid, t)| {
                json!({
                    "from": uid,
                    "id": t.deck.id,
                    "name": t.deck.name,
                    "count": t.deck.pictures.len(),
                    "at": t.deck.at,
                    "received": t.deck.pictures.iter().filter(|p| p.is_some()).count(),
                    "bytes": t.held,
                })
            })
            .collect();
        json!({ "mine": mine, "theirs": theirs })
    }
}

// ---------------------------------------------------------------- textures

/// A picture as a texture of a given width (0: the picture as it is).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TexKey {
    owner: Owner,
    id: u32,
    slide: usize,
    width: u32,
}

/// A picture to decode.
struct Job {
    key: TexKey,
    picture: Arc<[u8]>,
    urgency: u8,
    /// When it was last asked for (a count), the latest first.
    asked: u64,
}

struct TexState {
    ready: HashMap<TexKey, (TextureHandle, Instant)>,
    queue: Vec<Job>,
    busy: Option<TexKey>,
    failed: HashSet<TexKey>,
    asked: u64,
    swept: Instant,
}

/// Pictures turned into textures at the size they are shown. egui's
/// textures have no mipmaps, so a picture drawn at half its size or less
/// would shimmer (text above all): each is scaled down on the processor
/// instead, once per size, in steps of 64 pixels so resizing the window
/// does not redo it every frame. One thread decodes, what is on screen
/// first; textures nobody drew for a while are let go.
struct Textures {
    ctx: egui::Context,
    state: Mutex<TexState>,
    work: Condvar,
    thread: OnceLock<bool>,
}

/// A big texture nobody drew for this long goes (the current slide and its
/// neighbours are asked for every frame); a small one (the strip of
/// slides) after longer.
const BIG_UNUSED: Duration = Duration::from_secs(3);
const SMALL_UNUSED: Duration = Duration::from_secs(60);
const SMALL: u32 = 640;

/// Pictures waiting to be decoded: past this, the ones asked for longest
/// ago go (slides scrolled past).
const MAX_QUEUE: usize = 64;

impl Textures {
    fn new(ctx: egui::Context) -> Textures {
        Textures {
            ctx,
            state: Mutex::new(TexState {
                ready: HashMap::new(),
                queue: Vec::new(),
                busy: None,
                failed: HashSet::new(),
                asked: 0,
                swept: Instant::now(),
            }),
            work: Condvar::new(),
            thread: OnceLock::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TexState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(
        self: &Arc<Self>,
        key: TexKey,
        picture: Option<Arc<[u8]>>,
        urgency: u8,
    ) -> Option<SizedTexture> {
        let now = Instant::now();
        let mut s = self.lock();
        s.sweep(now);
        if let Some((t, used)) = s.ready.get_mut(&key) {
            *used = now;
            return Some(SizedTexture::from_handle(t));
        }
        let picture = picture?;
        if s.busy != Some(key) && !s.failed.contains(&key) {
            s.asked += 1;
            let asked = s.asked;
            match s.queue.iter_mut().find(|j| j.key == key) {
                Some(job) => {
                    job.urgency = job.urgency.min(urgency);
                    job.asked = asked;
                }
                None => s.queue.push(Job {
                    key,
                    picture,
                    urgency,
                    asked,
                }),
            }
            if s.queue.len() > MAX_QUEUE
                && let Some(oldest) = (0..s.queue.len()).min_by_key(|&i| s.queue[i].asked)
            {
                s.queue.swap_remove(oldest);
            }
            self.work.notify_one();
            self.start();
        }
        // The same slide at another size, meanwhile (the biggest; 0 is the
        // picture as it is).
        s.ready
            .iter_mut()
            .filter(|(k, _)| k.owner == key.owner && k.id == key.id && k.slide == key.slide)
            .max_by_key(|(k, _)| if k.width == 0 { u32::MAX } else { k.width })
            .map(|(_, (t, used))| {
                *used = now;
                SizedTexture::from_handle(t)
            })
    }

    /// The decoding thread, the first time a picture is asked for.
    fn start(self: &Arc<Self>) {
        self.thread.get_or_init(|| {
            let me = self.clone();
            std::thread::Builder::new()
                .name("slides-imagens".into())
                .spawn(move || me.work())
                .is_ok()
        });
    }

    fn work(&self) {
        loop {
            let job = {
                let mut s = self.lock();
                loop {
                    let best = (0..s.queue.len())
                        .min_by_key(|&i| (s.queue[i].urgency, std::cmp::Reverse(s.queue[i].asked)));
                    if let Some(i) = best {
                        let job = s.queue.swap_remove(i);
                        s.busy = Some(job.key);
                        break job;
                    }
                    s = self.work.wait(s).unwrap_or_else(PoisonError::into_inner);
                }
            };
            let k = job.key;
            let made = decode(&job.picture, k.width).map(|img| {
                self.ctx.load_texture(
                    format!("slide:{:?}:{}:{}:{}", k.owner, k.id, k.slide, k.width),
                    img,
                    TextureOptions::LINEAR,
                )
            });
            let mut s = self.lock();
            // Forgotten while it was decoded (the presentation ended).
            let wanted = s.busy.take() == Some(k);
            match made {
                Ok(t) if wanted => {
                    s.ready.insert(k, (t, Instant::now()));
                }
                Ok(_) => {}
                Err(e) => {
                    crate::logln!("apresentação: imagem do slide {}: {e}", k.slide + 1);
                    s.failed.insert(k);
                }
            }
            drop(s);
            self.ctx.request_repaint();
        }
    }

    /// Lets the textures of `owner`'s presentations go, all of them or all
    /// but `keep`.
    fn forget(&self, owner: Owner, keep: Option<u32>) {
        let mut s = self.lock();
        let gone = |k: &TexKey| k.owner == owner && Some(k.id) != keep;
        s.ready.retain(|k, _| !gone(k));
        s.queue.retain(|j| !gone(&j.key));
        s.failed.retain(|k| !gone(k));
        if s.busy.is_some_and(|k| gone(&k)) {
            s.busy = None;
        }
    }

    fn clear(&self) {
        let mut s = self.lock();
        s.ready.clear();
        s.queue.clear();
        s.failed.clear();
        s.busy = None;
    }
}

impl TexState {
    fn sweep(&mut self, now: Instant) {
        if now.duration_since(self.swept) < Duration::from_secs(1) {
            return;
        }
        self.swept = now;
        self.ready.retain(|k, (_, used)| {
            let keep = if k.width != 0 && k.width <= SMALL {
                SMALL_UNUSED
            } else {
                BIG_UNUSED
            };
            now.duration_since(*used) < keep
        });
    }
}

/// A picture decoded and, when it is wider than `width` (0: never),
/// scaled down to it.
fn decode(picture: &[u8], width: u32) -> Result<ColorImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(picture))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(wire::MAX_SIDE);
    limits.max_image_height = Some(wire::MAX_SIDE);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let img = if width != 0 && img.width() > width {
        img.resize(width, u32::MAX, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgb = img.into_rgb8();
    Ok(ColorImage::from_rgb(
        [rgb.width() as usize, rgb.height() as usize],
        rgb.as_raw(),
    ))
}

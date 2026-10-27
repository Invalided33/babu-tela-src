//! GIPHY stickers: the search, the trending ones and the ones people keep,
//! asked of GIPHY by the app itself, and their animations.
//!
//! GIPHY's rules shape this module. Its API and its media are reached by
//! the app, never through our server. Media addresses are used as they
//! come and kept nowhere: the server stores only sticker ids (favourites,
//! and messages that are a sticker), and each session asks GIPHY again
//! what an id looks like (`sticker`, up to 100 ids in one call). Results
//! show in GIPHY's order, and the picker says "Powered by GIPHY". The key
//! comes from the server in READY (the Worker's GIPHY_API_KEY secret);
//! without one, the stickers stay hidden.
//!
//! A beta key allows 100 calls an hour for everyone together, so the calls
//! are few: a search waits for the typing to stop, pages come as the grid
//! scrolls, trending is asked again only after ten minutes, and the ids of
//! a whole conversation go in one lookup.
//!
//! The animations live in memory only (textures, one per frame), at the
//! size they are drawn, and go once nobody has drawn them for a while.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use egui::{ColorImage, Rect, TextureHandle, TextureOptions, Vec2};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const API: &str = "https://api.giphy.com";

/// Points the app at another GIPHY (a fake one, for the tests).
pub const URL_ENV: &str = "BABUTELA_GIPHY_URL";

/// A key of one's own, over the server's: to try one before the Worker
/// has it, or to test against a fake GIPHY.
pub const KEY_ENV: &str = "BABUTELA_GIPHY_KEY";

/// Stickers per page: six rows of the picker's four columns.
const PAGE: usize = 24;

/// The last offsets GIPHY accepts, for trending and for a search.
const LAST_TRENDING: usize = 499;
const LAST_SEARCH: usize = 4999;

/// Ids per lookup (GIPHY's limit), and so the most favourites one keeps.
pub const MAX_IDS: usize = 100;

/// The longest search GIPHY takes.
pub const MAX_QUERY: usize = 50;

/// Up to PG-13: what a chat among friends shows by default.
const RATING: &str = "pg-13";

/// Only the renditions a messaging app uses: smaller answers.
const BUNDLE: &str = "messaging_non_clips";

/// The language people search in here, for regional results.
const LANG: &str = "pt";

/// Trending seen less than this ago is not asked again.
const TRENDING_FOR: Duration = Duration::from_secs(10 * 60);

/// Ids wanted within this long of each other go in one lookup.
const LOOKUP_AFTER: Duration = Duration::from_millis(80);

/// After a lookup fails, the ids wait this long before the next try.
const LOOKUP_RETRY: Duration = Duration::from_secs(60);

/// What GIPHY told about stickers is forgotten past this many.
const MAX_KNOWN: usize = 2000;

/// A sticker file bigger than this is left alone.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Stickers downloaded and decoded at the same time.
const LOADS_AT_ONCE: usize = 4;

/// The largest picture decoded (GIPHY's biggest rendition here is 480 px).
const MAX_CANVAS: u64 = 2_000_000;

/// Frames read from one file at most: 15 seconds at 60 a second (one of
/// GIPHY's trending stickers has 299). Past it, the loop would end early.
const MAX_DECODED: usize = 900;

/// Frames shorter than this are merged into the one before (at most 20
/// frames a second per sticker: every frame shown is the whole window
/// drawn again).
const MIN_FRAME_MS: u32 = 50;

/// Frames kept of one animation at most (six seconds at 20 a second), and
/// their pixels at most (16 MB); longer ones are thinned out evenly. With
/// 40 frames, a 15-second sticker showed a frame every 375 ms.
const MAX_FRAMES: usize = 120;
const ANIM_PIXELS: usize = 4_000_000;

/// The beat all the stickers on screen change frames on (30 a second), and
/// how far past it the frame that shows a change is asked for. Simulated
/// with twenty of GIPHY's trending stickers: a 50 ms beat drew the window
/// 20 times a second but put each frame 17 ms off its time (median; 34 ms
/// for one in ten), the frames of a 15 a second sticker showing for 100,
/// 50 and 50 ms in turn; this one draws it 29 times, 6.7 ms off (20); a
/// 60 Hz one 47 times, 0.7 ms off (10). Measured with them moving in the
/// picker: 2.2 % of a core with the 50 ms beat, 3.0 % with this one.
const BEAT_MS: f64 = 1000.0 / 30.0;
const BEAT_SLACK_MS: f64 = 1.0;

/// An animation nobody drew for this long lets its textures go.
const UNUSED_FOR: Duration = Duration::from_secs(20);

/// When a failed one may be tried again.
const RETRY_FAILED: Duration = Duration::from_secs(30);

/// The frame that lets them go comes this much after they are due.
const SWEEP_SLACK: Duration = Duration::from_secs(2);

/// Pixels of all the animations together (64 MB of textures): past it, the
/// ones drawn longest ago go first.
const BUDGET: u64 = 16_000_000;

/// GIPHY's "Powered by GIPHY" mark with its logo, the version for dark
/// backgrounds, as it comes in GIPHY's attribution kit
/// (media.giphy.com/giphy-attribution-marks.zip). GIPHY's terms ask for
/// the words and the logo wherever its search is used.
const MARK: &[u8] = include_bytes!("../assets/giphy/powered-by-giphy.png");

/// The mark as a texture `height` points tall, and its size in points:
/// shrunk once (and kept) to the pixels it takes on this screen, so its
/// small letters stay sharp.
pub fn mark(ctx: &egui::Context, height: f32) -> Option<(TextureHandle, Vec2)> {
    let px = (height * ctx.pixels_per_point()).round().max(1.0) as u32;
    let id = egui::Id::unique(("giphy-mark", px));
    let texture = match ctx.data(|d| d.get_temp::<TextureHandle>(id)) {
        Some(t) => t,
        None => {
            let img = mark_picture(px)?;
            let t = ctx.load_texture("giphy-mark", img, TextureOptions::LINEAR);
            ctx.data_mut(|d| d.insert_temp(id, t.clone()));
            t
        }
    };
    let [w, h] = texture.size();
    Some((texture, Vec2::new(height * w as f32 / h as f32, height)))
}

/// The mark, `px` pixels tall, premultiplied (so its see-through edges
/// stay clean when shrunk).
fn mark_picture(px: u32) -> Option<ColorImage> {
    let mut img = image::load_from_memory(MARK).ok()?.into_rgba8();
    for p in img.pixels_mut() {
        let a = u16::from(p[3]);
        for c in &mut p.0[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    let (w, h) = img.dimensions();
    if px < h {
        let width = ((w as f32 * px as f32 / h as f32).round() as u32).max(1);
        img = image::imageops::resize(&img, width, px, image::imageops::FilterType::Triangle);
    }
    Some(ColorImage::from_rgba_premultiplied(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    ))
}

/// Whether `id` looks like a GIPHY id; the server takes the same ones.
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// One rendition of a sticker: where its file is, and its size in pixels
/// (0 when GIPHY did not say).
#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub url: String,
    pub size: Vec2,
}

/// A sticker as GIPHY described it.
#[derive(Debug)]
pub struct Sticker {
    pub id: String,
    /// GIPHY's title ("Happy Dance Sticker by …").
    pub title: String,
    /// GIPHY's description for screen readers, when it has one.
    pub alt: String,
    /// Who made it, when GIPHY says: its terms ask that the creator be
    /// credited wherever the content shows.
    pub creator: Option<Creator>,
    /// For the picker's grid (100 px wide) and for messages (200 px).
    pub small: Media,
    pub large: Media,
    /// GIPHY's addresses for "seen", "clicked" and "sent".
    pings: [String; 3],
}

/// The GIPHY account a sticker came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Creator {
    /// Its display name, or its user name when it has none.
    pub name: String,
    pub username: String,
}

impl Sticker {
    /// What shows under the pointer: the title, and who made it.
    pub fn about(&self) -> String {
        let title = [&self.title, &self.alt]
            .into_iter()
            .find(|t| !t.is_empty())
            .map_or("Figurinha do GIPHY", String::as_str);
        match &self.creator {
            Some(c) if c.name != c.username => {
                format!("{title}\nPor {} (@{}) no GIPHY", c.name, c.username)
            }
            Some(c) => format!("{title}\nPor @{} no GIPHY", c.username),
            None => format!("{title}\nGIPHY"),
        }
    }

    /// What a screen reader says of it.
    pub fn label(&self) -> &str {
        [&self.alt, &self.title]
            .into_iter()
            .find(|t| !t.is_empty())
            .map_or("Figurinha do GIPHY", String::as_str)
    }
}

/// What is known of an id.
pub enum Lookup {
    /// No GIPHY key: the stickers are off.
    Off,
    Loading,
    /// GIPHY has no such sticker (removed, or above the rating).
    Missing,
    /// GIPHY did not answer (or turned the key down) a moment ago: it is
    /// asked again in a while.
    Failed,
    Ready(Arc<Sticker>),
}

/// A rendition's animation.
pub enum Frames {
    Loading,
    Failed,
    Ready(Arc<Anim>),
}

#[derive(Clone, Copy)]
enum Ping {
    Seen = 0,
    Click = 1,
    Sent = 2,
}

/// An animation: its frames side by side in one texture (`sheet`), where
/// each one is in it, and how long each shows.
pub struct Anim {
    texture: TextureHandle,
    uvs: Vec<Rect>,
    delays: Vec<u32>,
    total: u32,
    pixels: u64,
}

impl Anim {
    pub fn frame_count(&self) -> usize {
        self.uvs.len()
    }

    /// Where the frame for `t` seconds into the app's clock is, and how
    /// long until it changes; the first frame, standing, when not
    /// `animate`. Every sticker changes on the same beat (`BEAT_MS`): each
    /// on a clock of its own, twenty of GIPHY's stickers out of step drew
    /// the window again 82 times a second (measured).
    fn frame(&self, t: f64, animate: bool) -> Option<(Rect, Option<Duration>)> {
        let first = *self.uvs.first()?;
        if !animate || self.uvs.len() < 2 || self.total == 0 {
            return Some((first, None));
        }
        let (i, wait) = on_beat(&self.delays, self.total, t.max(0.0) * 1000.0);
        Some((self.uvs[i], Some(Duration::from_secs_f64(wait / 1000.0))))
    }
}

/// Paints `anim` at `rect`, on the frame for now, and asks for the next
/// frame when `animate`.
pub fn paint(ui: &egui::Ui, anim: &Anim, rect: Rect, animate: bool) {
    let Some((uv, next)) = anim.frame(ui.input(|i| i.time), animate) else {
        return;
    };
    egui::Image::from_texture(egui::load::SizedTexture::from_handle(&anim.texture))
        .uv(uv)
        .paint_at(ui, rect);
    if let Some(next) = next {
        ui.ctx().request_repaint_after(next);
    }
}

/// The size to draw a sticker GIPHY says is `natural` at: its shape, the
/// longer side `side` (a square when the size is not known).
pub fn fit(natural: Vec2, side: f32) -> Vec2 {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return Vec2::splat(side);
    }
    natural * (side / natural.x.max(natural.y))
}

/// The frame (of `delays`, `total` in all) to show `ms` into the clock,
/// which is the one due at the last beat, and how long (ms) until the
/// first beat after it is over, plus a hair, so the frame drawn then is
/// on that beat.
fn on_beat(delays: &[u32], total: u32, ms: f64) -> (usize, f64) {
    let beat = (ms / BEAT_MS).floor() * BEAT_MS;
    // `at` counts whole milliseconds: a beat stands for the millisecond it
    // rounds to.
    let start = beat.round();
    let (i, left) = at(delays, total, start as u64);
    // The first beat that rounds to the change or after it.
    let change = start + f64::from(left);
    let over = ((change - 0.5) / BEAT_MS).ceil() * BEAT_MS;
    (i, (over - ms).max(0.0) + BEAT_SLACK_MS)
}

/// Which of the frames lasting `delays` (in all, `total`) shows `ms` into
/// the clock, and for how much longer.
fn at(delays: &[u32], total: u32, ms: u64) -> (usize, u32) {
    let mut into = (ms % u64::from(total.max(1))) as u32;
    for (i, &d) in delays.iter().enumerate() {
        if into < d {
            return (i, d - into);
        }
        into -= d;
    }
    (0, delays.first().copied().unwrap_or(1))
}

// ---------------------------------------------------------------- the state

#[derive(Default)]
struct Feed {
    /// Empty for trending.
    query: String,
    ids: Vec<String>,
    /// The offset of the next page.
    next: usize,
    loading: bool,
    error: Option<String>,
    /// No more pages.
    done: bool,
    /// 0 for a feed never asked for.
    generation: u64,
    fetched: Option<Instant>,
}

/// A feed as the picker shows it.
#[derive(Clone, Debug, Default)]
pub struct FeedView {
    pub ids: Vec<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub done: bool,
}

enum Slot {
    Loading,
    /// The animation, and when it was last drawn.
    Ready(Arc<Anim>, Instant),
    Failed(Instant),
}

/// A rendition at a size in pixels.
type AnimKey = (String, u32);

struct Inner {
    key: String,
    customer: String,
    trending: Feed,
    search: Feed,
    generation: u64,
    known: HashMap<String, Arc<Sticker>>,
    missing: HashSet<String>,
    /// Ids for the next lookup, and those in the one on its way.
    wanted: Vec<String>,
    asking: HashSet<String>,
    lookup_running: bool,
    lookup_retry: Option<Instant>,
    anims: HashMap<AnimKey, Slot>,
    /// When the frame being drawn began (`tick`).
    frame_at: Instant,
    /// Stickers whose "seen" went to GIPHY since the last feed came, and
    /// the ones seen in conversations this session.
    seen: HashSet<String>,
    seen_in_chat: HashSet<String>,
    /// For the test reports: calls to the API, and pings by kind.
    calls: u32,
    pings: [u32; 3],
}

impl Inner {
    fn feed(&mut self, query: &str) -> &mut Feed {
        if query.is_empty() {
            &mut self.trending
        } else {
            &mut self.search
        }
    }

    /// At the start of a frame: lets go of the animations the last frame
    /// did not draw and none drew for `UNUSED_FOR`, and of failures old
    /// enough to try again. One on screen in a window left alone stays,
    /// however long ago that frame was. Says whether any are left.
    fn sweep(&mut self, now: Instant) -> bool {
        let last_frame = std::mem::replace(&mut self.frame_at, now);
        self.anims.retain(|_, slot| match slot {
            Slot::Ready(_, used) => *used >= last_frame || now.duration_since(*used) < UNUSED_FOR,
            Slot::Failed(at) => now.duration_since(*at) < RETRY_FAILED,
            Slot::Loading => true,
        });
        !self.anims.is_empty()
    }

    /// Keeps the animations within `BUDGET`, dropping the ones drawn
    /// longest ago (never one drawn in the last two seconds).
    fn budget(&mut self, now: Instant) {
        let mut total: u64 = self
            .anims
            .values()
            .map(|s| match s {
                Slot::Ready(a, _) => a.pixels,
                _ => 0,
            })
            .sum();
        if total <= BUDGET {
            return;
        }
        let mut idle: Vec<(AnimKey, Instant, u64)> = self
            .anims
            .iter()
            .filter_map(|(k, s)| match s {
                Slot::Ready(a, used) if now.duration_since(*used) > Duration::from_secs(2) => {
                    Some((k.clone(), *used, a.pixels))
                }
                _ => None,
            })
            .collect();
        idle.sort_by_key(|(_, used, _)| *used);
        for (key, _, pixels) in idle {
            if total <= BUDGET {
                break;
            }
            self.anims.remove(&key);
            total -= pixels;
        }
    }

    /// Forgets the stickers past `MAX_KNOWN`, but not the ones on show.
    fn trim_known(&mut self) {
        if self.known.len() <= MAX_KNOWN {
            return;
        }
        let keep: HashSet<&String> = self.trending.ids.iter().chain(&self.search.ids).collect();
        self.known.retain(|id, _| keep.contains(id));
        self.missing.clear();
    }
}

pub struct Giphy {
    ctx: egui::Context,
    rt: tokio::runtime::Handle,
    http: reqwest::Client,
    base: String,
    /// Talking to a fake GIPHY: its own addresses are allowed.
    fake: bool,
    loads: Arc<tokio::sync::Semaphore>,
    inner: Mutex<Inner>,
}

impl Giphy {
    pub fn new(ctx: egui::Context, rt: tokio::runtime::Handle) -> Arc<Giphy> {
        let fake = std::env::var(URL_ENV).ok().filter(|u| !u.trim().is_empty());
        let base = fake.clone().unwrap_or_else(|| API.to_owned());
        Giphy::at(ctx, rt, &base, fake.is_some())
    }

    fn at(ctx: egui::Context, rt: tokio::runtime::Handle, base: &str, fake: bool) -> Arc<Giphy> {
        crate::net::install_crypto_provider();
        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("BabuTela/", env!("CARGO_PKG_VERSION")));
        let http = match crate::tor::proxy() {
            // GIPHY too only through Tor when it is on (tor.rs).
            Some(proxy) => builder
                .proxy(reqwest::Proxy::all(proxy).expect("o proxy do Tor"))
                .connect_timeout(Duration::from_secs(60))
                .build()
                .expect("o cliente do GIPHY pelo Tor"),
            None => builder.build().unwrap_or_else(|_| reqwest::Client::new()),
        };
        Arc::new(Giphy {
            ctx,
            rt,
            http,
            base: base.trim().trim_end_matches('/').to_owned(),
            fake,
            loads: Arc::new(tokio::sync::Semaphore::new(LOADS_AT_ONCE)),
            inner: Mutex::new(Inner {
                key: String::new(),
                customer: String::new(),
                trending: Feed::default(),
                search: Feed::default(),
                generation: 0,
                known: HashMap::new(),
                missing: HashSet::new(),
                wanted: Vec::new(),
                asking: HashSet::new(),
                lookup_running: false,
                lookup_retry: None,
                anims: HashMap::new(),
                frame_at: Instant::now(),
                seen: HashSet::new(),
                seen_in_chat: HashSet::new(),
                calls: 0,
                pings: [0; 3],
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The key from READY (or BABUTELA_GIPHY_KEY), and whose requests
    /// these are: GIPHY gets a hash, never the account. An empty key (and
    /// no BABUTELA_GIPHY_KEY) turns the stickers off.
    pub fn set_key(&self, key: &str, uid: i64, username: &str) {
        let key = std::env::var(KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| key.to_owned())
            .trim()
            .to_owned();
        let mut s = self.lock();
        if s.key != key {
            s.trending = Feed::default();
            s.search = Feed::default();
            s.missing.clear();
            s.seen.clear();
            s.seen_in_chat.clear();
            s.lookup_retry = None;
        }
        s.key = key;
        s.customer = customer_id(uid, username);
    }

    /// Whether there is a key (the stickers are on).
    pub fn enabled(&self) -> bool {
        !self.lock().key.is_empty()
    }

    /// Before every frame: lets go of the animations nobody drew lately.
    /// While some are held, a frame comes at least every `UNUSED_FOR` (and
    /// a little), so they go even when nothing else moves: an idle app
    /// draws no frames at all.
    pub fn tick(&self) {
        if self.lock().sweep(Instant::now()) {
            self.ctx.request_repaint_after(UNUSED_FOR + SWEEP_SLACK);
        }
    }

    // ------------------------------------------------------------ feeds

    /// The picker opened: trending older than ten minutes is asked again.
    pub fn opened(&self) {
        let mut s = self.lock();
        let stale = s
            .trending
            .fetched
            .is_some_and(|t| t.elapsed() > TRENDING_FOR);
        if stale && !s.trending.loading {
            s.trending = Feed::default();
        }
    }

    /// The feed for `query` (trending when empty) as it is now.
    pub fn feed(&self, query: &str) -> FeedView {
        let mut s = self.lock();
        let f = s.feed(query);
        if f.generation == 0 || f.query != query {
            return FeedView {
                loading: true,
                ..FeedView::default()
            };
        }
        FeedView {
            ids: f.ids.clone(),
            loading: f.loading,
            error: f.error.clone(),
            done: f.done,
        }
    }

    /// Asks GIPHY for the feed of `query`, unless it is the one there.
    pub fn show(self: &Arc<Self>, query: &str) {
        let mut s = self.lock();
        if s.key.is_empty() {
            return;
        }
        let f = s.feed(query);
        if f.generation != 0 && f.query == query {
            return;
        }
        s.generation += 1;
        let generation = s.generation;
        *s.feed(query) = Feed {
            query: query.to_owned(),
            loading: true,
            generation,
            ..Feed::default()
        };
        s.seen.clear();
        drop(s);
        self.page(generation, query.to_owned(), 0);
    }

    /// The next page of the feed of `query`, when there is one.
    pub fn more(self: &Arc<Self>, query: &str) {
        let mut s = self.lock();
        let f = s.feed(query);
        if f.generation == 0 || f.query != query || f.loading || f.done || f.error.is_some() {
            return;
        }
        f.loading = true;
        let (generation, next) = (f.generation, f.next);
        drop(s);
        self.page(generation, query.to_owned(), next);
    }

    /// "Tentar de novo" after an error: the page that failed.
    pub fn retry(self: &Arc<Self>, query: &str) {
        let mut s = self.lock();
        let f = s.feed(query);
        if f.generation == 0 || f.query != query || f.loading || f.error.is_none() {
            return;
        }
        f.error = None;
        f.loading = true;
        let (generation, next) = (f.generation, f.next);
        drop(s);
        self.page(generation, query.to_owned(), next);
    }

    fn page(self: &Arc<Self>, generation: u64, query: String, offset: usize) {
        let me = self.clone();
        self.rt.spawn(async move {
            let mut params = vec![
                ("limit", PAGE.to_string()),
                ("offset", offset.to_string()),
                ("rating", RATING.to_owned()),
                ("bundle", BUNDLE.to_owned()),
            ];
            let path = if query.is_empty() {
                "/v1/stickers/trending"
            } else {
                params.push(("q", query.clone()));
                params.push(("lang", LANG.to_owned()));
                "/v1/stickers/search"
            };
            let res = me.get(path, params).await;
            me.landed(generation, &query, offset, res);
            me.ctx.request_repaint();
        });
    }

    /// A page of a feed arrived, or failed to.
    fn landed(&self, generation: u64, query: &str, offset: usize, res: Result<Value, String>) {
        let mut guard = self.lock();
        let s = &mut *guard;
        let f = if query.is_empty() {
            &mut s.trending
        } else {
            &mut s.search
        };
        // Another search took its place meanwhile.
        if f.generation != generation {
            return;
        }
        f.loading = false;
        let body = match res {
            Ok(body) => body,
            Err(e) => {
                f.error = Some(e);
                return;
            }
        };
        let raw = body["data"].as_array().map_or(0, Vec::len);
        let total = count(&body["pagination"]["total_count"]);
        let items = parse_list(&body);
        for it in &items {
            // GIPHY's order, but the same sticker only once (trending can
            // move between two pages).
            if !f.ids.contains(&it.id) {
                f.ids.push(it.id.clone());
            }
        }
        f.next = offset + raw;
        if offset == 0 {
            f.fetched = Some(Instant::now());
        }
        let last = if query.is_empty() {
            LAST_TRENDING
        } else {
            LAST_SEARCH
        };
        f.done = raw == 0 || (total > 0 && f.next >= total) || f.next > last;
        for it in items {
            s.missing.remove(&it.id);
            s.known.insert(it.id.clone(), Arc::new(it));
        }
        s.trim_known();
    }

    // ------------------------------------------------------------ by id

    /// What `id` looks like. Unknown ids go to GIPHY together, a moment
    /// after the first is asked for.
    pub fn sticker(self: &Arc<Self>, id: &str) -> Lookup {
        let mut s = self.lock();
        if s.key.is_empty() {
            return Lookup::Off;
        }
        if let Some(it) = s.known.get(id) {
            return Lookup::Ready(it.clone());
        }
        if s.missing.contains(id) || !valid_id(id) {
            return Lookup::Missing;
        }
        if s.asking.contains(id) || s.wanted.iter().any(|w| w == id) {
            return Lookup::Loading;
        }
        if s.lookup_retry.is_some_and(|t| Instant::now() < t) {
            return Lookup::Failed;
        }
        s.lookup_retry = None;
        s.wanted.push(id.to_owned());
        if !s.lookup_running {
            s.lookup_running = true;
            let me = self.clone();
            self.rt.spawn(async move { me.lookup().await });
        }
        Lookup::Loading
    }

    async fn lookup(self: Arc<Self>) {
        tokio::time::sleep(LOOKUP_AFTER).await;
        loop {
            let batch: Vec<String> = {
                let mut s = self.lock();
                let n = s.wanted.len().min(MAX_IDS);
                if n == 0 {
                    s.lookup_running = false;
                    return;
                }
                let batch: Vec<String> = s.wanted.drain(..n).collect();
                s.asking.extend(batch.iter().cloned());
                batch
            };
            let res = self
                .get(
                    "/v1/gifs",
                    vec![("ids", batch.join(",")), ("rating", RATING.to_owned())],
                )
                .await;
            let ok = self.looked_up(&batch, res);
            if !ok {
                self.ctx.request_repaint_after(LOOKUP_RETRY);
                return;
            }
            self.ctx.request_repaint();
        }
    }

    /// The answer to a lookup: what GIPHY did not send does not exist (for
    /// this rating). A failure holds the lookups back for a while.
    fn looked_up(&self, batch: &[String], res: Result<Value, String>) -> bool {
        let mut s = self.lock();
        for id in batch {
            s.asking.remove(id);
        }
        match res {
            Ok(body) => {
                for it in parse_list(&body) {
                    s.known.insert(it.id.clone(), Arc::new(it));
                }
                for id in batch {
                    if !s.known.contains_key(id) {
                        s.missing.insert(id.clone());
                    }
                }
                s.trim_known();
                true
            }
            Err(e) => {
                crate::logln!("giphy: figurinhas por id: {e}");
                s.lookup_retry = Some(Instant::now() + LOOKUP_RETRY);
                s.wanted.clear();
                s.lookup_running = false;
                false
            }
        }
    }

    // ------------------------------------------------------------ animations

    /// The animation of `media`, made for `side` points; loads it the first
    /// time.
    pub fn anim(self: &Arc<Self>, media: &Media, side: f32) -> Frames {
        let px = (side * self.ctx.pixels_per_point())
            .round()
            .clamp(16.0, 512.0) as u32;
        let key = (media.url.clone(), px);
        let now = Instant::now();
        let mut s = self.lock();
        match s.anims.get_mut(&key) {
            Some(Slot::Ready(a, used)) => {
                *used = now;
                return Frames::Ready(a.clone());
            }
            Some(Slot::Loading) => return Frames::Loading,
            Some(Slot::Failed(_)) => return Frames::Failed,
            None => {}
        }
        if !self.allowed(&media.url) {
            s.anims.insert(key, Slot::Failed(now));
            return Frames::Failed;
        }
        s.anims.insert(key.clone(), Slot::Loading);
        drop(s);
        let me = self.clone();
        self.rt.spawn(async move { me.load(key).await });
        Frames::Loading
    }

    async fn load(self: Arc<Self>, key: AnimKey) {
        let px = key.1;
        let side = self
            .ctx
            .input(|i| i.max_texture_side)
            .clamp(MIN_SHEET, MAX_SHEET);
        let decoded = async {
            let _turn = self.loads.acquire().await.map_err(|e| e.to_string())?;
            let bytes = self.fetch(&key.0).await?;
            tokio::task::spawn_blocking(move || decode(&bytes, px).map(|f| sheet(f, side)))
                .await
                .map_err(|e| e.to_string())?
        }
        .await;
        let now = Instant::now();
        let slot = match decoded {
            Ok(Sheet { image, uvs, delays }) => {
                let pixels = (image.size[0] * image.size[1]) as u64;
                let texture =
                    self.ctx
                        .load_texture(format!("giphy:{px}"), image, TextureOptions::LINEAR);
                let anim = Anim {
                    texture,
                    total: delays.iter().sum(),
                    uvs,
                    delays,
                    pixels,
                };
                Slot::Ready(Arc::new(anim), now)
            }
            Err(e) => {
                crate::logln!("figurinha: {e}");
                Slot::Failed(now)
            }
        };
        let mut s = self.lock();
        s.anims.insert(key, slot);
        s.budget(now);
        drop(s);
        self.ctx.request_repaint();
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        use futures_util::StreamExt;
        let res = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "image/webp,image/gif,image/*")
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !res.status().is_success() {
            return Err(format!("resposta {}", res.status().as_u16()));
        }
        if res.content_length().is_some_and(|n| n > MAX_BYTES) {
            return Err("grande demais".into());
        }
        let mut out = Vec::new();
        let mut body = res.bytes_stream();
        while let Some(piece) = body.next().await {
            out.extend_from_slice(&piece.map_err(|e| e.without_url().to_string())?);
            if out.len() as u64 > MAX_BYTES {
                return Err("grande demais".into());
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ analytics

    /// The sticker is on show in the picker: GIPHY hears it once per feed.
    pub fn seen(self: &Arc<Self>, id: &str) {
        let first = self.lock().seen.insert(id.to_owned());
        if first {
            self.ping(id, Ping::Seen);
        }
    }

    /// A sticker in a conversation came on screen: GIPHY hears it once a
    /// session (it asks to hear of every view).
    pub fn seen_in_chat(self: &Arc<Self>, id: &str) {
        let first = self.lock().seen_in_chat.insert(id.to_owned());
        if first {
            self.ping(id, Ping::Seen);
        }
    }

    /// It was clicked and sent.
    pub fn chosen(self: &Arc<Self>, id: &str) {
        self.ping(id, Ping::Click);
        self.ping(id, Ping::Sent);
    }

    fn ping(self: &Arc<Self>, id: &str, kind: Ping) {
        let url = {
            let mut s = self.lock();
            let Some(it) = s.known.get(id) else { return };
            let url = it.pings[kind as usize].clone();
            if url.is_empty() || !self.allowed(&url) {
                return;
            }
            s.pings[kind as usize] += 1;
            pingback(&url, &s.customer, chrono::Utc::now().timestamp_millis())
        };
        let http = self.http.clone();
        self.rt.spawn(async move {
            if let Err(e) = http.get(&url).send().await {
                crate::logln!("giphy: registro: {}", e.without_url());
            }
        });
    }

    // ------------------------------------------------------------ plumbing

    async fn get(
        &self,
        path: &str,
        mut params: Vec<(&'static str, String)>,
    ) -> Result<Value, String> {
        let (key, customer) = {
            let mut s = self.lock();
            s.calls += 1;
            (s.key.clone(), s.customer.clone())
        };
        if key.is_empty() {
            return Err("As figurinhas estão desligadas.".into());
        }
        params.insert(0, ("api_key", key));
        if !customer.is_empty() {
            params.push(("customer_id", customer));
        }
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(&params)
            .send()
            .await
            .map_err(|e| {
                // Without the address: it carries the key.
                crate::logln!("giphy {path}: {}", e.without_url());
                "Não deu para falar com o GIPHY. Confira a internet.".to_owned()
            })?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(refusal(status));
        }
        let body = res
            .json::<Value>()
            .await
            .map_err(|_| "O GIPHY respondeu algo inesperado.".to_owned())?;
        if synthetic(&body) {
            return Err("O GIPHY está com problemas agora. Tente de novo daqui a pouco.".into());
        }
        Ok(body)
    }

    /// Only GIPHY's own addresses are fetched (or the fake's, in a test).
    fn allowed(&self, url: &str) -> bool {
        if self.fake {
            return url.starts_with(&format!("{}/", self.base));
        }
        giphys(url)
    }

    /// What the test reports show of the stickers.
    pub fn report(&self) -> Value {
        let s = self.lock();
        let feed = |f: &Feed| {
            json!({
                "query": f.query, "items": f.ids.len(), "loading": f.loading,
                "error": f.error, "done": f.done,
            })
        };
        let ready: Vec<&Arc<Anim>> = s
            .anims
            .values()
            .filter_map(|slot| match slot {
                Slot::Ready(a, _) => Some(a),
                _ => None,
            })
            .collect();
        let count_of = |want: fn(&Slot) -> bool| s.anims.values().filter(|x| want(x)).count();
        json!({
            "on": !s.key.is_empty(),
            "trending": feed(&s.trending),
            "search": feed(&s.search),
            "known": s.known.len(),
            "missing": s.missing.len(),
            "anims": ready.len(),
            "animated": ready.iter().filter(|a| a.frame_count() > 1).count(),
            "frames": ready.iter().map(|a| a.frame_count()).sum::<usize>(),
            "pixels": ready.iter().map(|a| a.pixels).sum::<u64>(),
            "loadingAnims": count_of(|x| matches!(x, Slot::Loading)),
            "failedAnims": count_of(|x| matches!(x, Slot::Failed(_))),
            "calls": s.calls,
            "pings": { "seen": s.pings[0], "click": s.pings[1], "sent": s.pings[2] },
            // The hash GIPHY knows this person by (not who they are).
            "customerId": s.customer,
        })
    }
}

fn refusal(status: u16) -> String {
    match status {
        401 | 403 => "O GIPHY recusou a chave do Babu Tela.".to_owned(),
        429 => "O limite de buscas no GIPHY acabou por agora. Tente de novo mais tarde.".to_owned(),
        n => format!("O GIPHY não respondeu (erro {n})."),
    }
}

/// Whether `url` is an https address of GIPHY's (giphy.com or one of its
/// subdomains: media0..4, i, giphy-analytics). One with a user part, which
/// could pass for another host, is not.
fn giphys(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = &rest[..rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len())];
    if authority.contains('@') {
        return false;
    }
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "giphy.com" || host.ends_with(".giphy.com")
}

/// GIPHY's address for an action, with who did it and when, as it asks.
fn pingback(url: &str, customer: &str, ts: i64) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!(
        "{url}{sep}customer_id={}&ts={ts}",
        crate::net::escape(customer)
    )
}

/// The same person, across devices, without telling GIPHY who it is: a
/// hash of the account (its id and name, which GIPHY does not have).
fn customer_id(uid: i64, username: &str) -> String {
    if uid == 0 {
        return String::new();
    }
    let h = Sha256::digest(format!("babutela-giphy:{uid}:{username}").as_bytes());
    hex::encode(&h[..16])
}

// ---------------------------------------------------------------- GIPHY's answers

/// The stickers in an answer, in GIPHY's order; ones without a usable
/// picture are left out.
fn parse_list(body: &Value) -> Vec<Sticker> {
    body["data"]
        .as_array()
        .map(|list| list.iter().filter_map(parse_sticker).collect())
        .unwrap_or_default()
}

fn parse_sticker(v: &Value) -> Option<Sticker> {
    let id = v["id"].as_str().filter(|id| valid_id(id))?.to_owned();
    let images = &v["images"];
    let small = rendition(
        images,
        &[
            "fixed_width_small",
            "fixed_height_small",
            "fixed_width",
            "fixed_height",
            "original",
        ],
    )?;
    let large = rendition(
        images,
        &[
            "fixed_width",
            "fixed_height",
            "original",
            "fixed_width_small",
        ],
    )?;
    let text = |k: &str| v[k].as_str().map_or("", str::trim).to_owned();
    let ping = |k: &str| v["analytics"][k]["url"].as_str().unwrap_or("").to_owned();
    Some(Sticker {
        id,
        title: text("title"),
        alt: text("alt_text"),
        creator: creator_of(v),
        small,
        large,
        pings: [ping("onload"), ping("onclick"), ping("onsent")],
    })
}

/// Who made a sticker: GIPHY's `user` (its display name, or else its user
/// name), or the bare `username` when that is all there is.
fn creator_of(v: &Value) -> Option<Creator> {
    let user = &v["user"];
    let username = [&user["username"], &v["username"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|u| !u.is_empty())?
        .to_owned();
    let name = user["display_name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&username)
        .to_owned();
    Some(Creator { name, username })
}

/// GIPHY's answer while its own systems are in trouble: 200, an empty
/// response id and nothing else. It asks that this be taken as the
/// failure it is (a lookup would otherwise call every sticker gone).
fn synthetic(body: &Value) -> bool {
    body["meta"]["response_id"].as_str() == Some("")
        && !body["data"].as_array().is_some_and(|d| !d.is_empty())
}

/// The first of `names` GIPHY sent, as WebP when it has it (stickers are
/// transparent, and GIPHY asks for WebP over GIF).
fn rendition(images: &Value, names: &[&str]) -> Option<Media> {
    names.iter().find_map(|n| {
        let r = &images[*n];
        let url = ["webp", "url"]
            .iter()
            .filter_map(|k| r[*k].as_str())
            .find(|u| !u.is_empty())?;
        Some(Media {
            url: url.to_owned(),
            size: Vec2::new(number(&r["width"]), number(&r["height"])),
        })
    })
}

/// GIPHY sends its numbers as strings ("200").
fn number(v: &Value) -> f32 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0.0) as f32
}

fn count(v: &Value) -> usize {
    number(v).max(0.0) as usize
}

// ---------------------------------------------------------------- decoding

/// The frames of an animated WebP or GIF (or of a still picture), at most
/// about `px` pixels on their longer side, each with how long it shows
/// (ms). Each frame is shrunk as it comes, so a long animation never sits
/// in memory at full size.
fn decode(bytes: &[u8], px: u32) -> Result<Vec<(ColorImage, u32)>, String> {
    Ok(decode_kept(bytes, px)?
        .into_iter()
        .map(|((_, img), ms)| (img, ms))
        .collect())
}

/// A frame kept: which of the file's it is, its picture, and how long it
/// shows (ms).
type Kept = ((usize, ColorImage), u32);

/// `decode`, with the number of the file's frame each one kept is.
fn decode_kept(bytes: &[u8], px: u32) -> Result<Vec<Kept>, String> {
    use image::codecs::{gif::GifDecoder, webp::WebPDecoder};
    use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat};
    let err = |e: image::ImageError| e.to_string();
    let cursor = || std::io::Cursor::new(bytes);
    let still = |img: DynamicImage| vec![((0, picture(img.into_rgba8(), px)), 0)];
    let (frames, size) = match image::guess_format(bytes).map_err(err)? {
        ImageFormat::WebP => {
            let mut d = WebPDecoder::new(cursor()).map_err(err)?;
            let size = d.dimensions();
            fits(size)?;
            if !d.has_animation() {
                return Ok(still(DynamicImage::from_decoder(d).map_err(err)?));
            }
            // A frame marked "dispose" leaves its place see-through for the
            // next, as in libwebp and the browsers. image-webp only does it
            // with a background colour set, and skips it otherwise: what the
            // frame drew stays under the ones after it. Compared with
            // libwebp, 42 of 96 GIPHY sticker files showed such leftovers,
            // in up to 28 % of a frame; with this, none.
            d.set_background_color(image::Rgba([0, 0, 0, 0]))
                .map_err(err)?;
            (d.into_frames(), size)
        }
        ImageFormat::Gif => {
            let d = GifDecoder::new(cursor()).map_err(err)?;
            let size = d.dimensions();
            fits(size)?;
            (d.into_frames(), size)
        }
        _ => {
            let img = image::load_from_memory(bytes).map_err(err)?;
            fits((img.width(), img.height()))?;
            return Ok(still(img));
        }
    };
    // A broken frame after good ones ends the animation there.
    let mut failure = None;
    let frames = frames
        .take(MAX_DECODED)
        .enumerate()
        .map_while(|(i, f)| match f {
            Ok(f) => {
                let ms = millis(f.delay());
                Some(((i, f.into_buffer()), ms))
            }
            Err(e) => {
                failure = Some(e.to_string());
                None
            }
        });
    let kept = pick(frames, frames_for(size, px), |(i, buffer)| {
        (i, picture(buffer, px))
    });
    if kept.is_empty() {
        return Err(failure.unwrap_or_else(|| "sem quadros".to_owned()));
    }
    Ok(kept)
}

/// How many frames of a `w`x`h` animation shown at `px` may be kept: up to
/// `MAX_FRAMES`, and `ANIM_PIXELS` in all, at the size `picture` makes.
fn frames_for((w, h): (u32, u32), px: u32) -> usize {
    let longest = w.max(h).max(1);
    let scale = if longest > px + px / 10 {
        px as f32 / longest as f32
    } else {
        1.0
    };
    let side = |v: u32| (v as f32 * scale).round().max(1.0) as usize;
    (ANIM_PIXELS / (side(w) * side(h))).clamp(2, MAX_FRAMES)
}

fn fits((w, h): (u32, u32)) -> Result<(), String> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_CANVAS {
        return Err(format!("tamanho estranho ({w}x{h})"));
    }
    Ok(())
}

fn millis(delay: image::Delay) -> u32 {
    let (n, d) = delay.numer_denom_ms();
    n.checked_div(d).unwrap_or(0)
}

/// Which frames to keep (at most `max`), and for how long each shows;
/// `make` turns the ones kept into pictures, as they come. Delays of 10 ms
/// or less mean 100 ms, as browsers read them. A frame that comes while
/// the one kept before has shown for less than `MIN_FRAME_MS` joins it
/// (its time added). Should more than twice `max` pile up, every two
/// become one, and the shortest frame doubles; at the end, at most `max`
/// stay, evenly spread. The loop lasts as long as it did.
fn pick<T, P>(
    frames: impl Iterator<Item = (T, u32)>,
    max: usize,
    mut make: impl FnMut(T) -> P,
) -> Vec<(P, u32)> {
    let max = max.max(2);
    let mut kept: Vec<(P, u32)> = Vec::new();
    let mut shortest = MIN_FRAME_MS;
    for (frame, ms) in frames {
        let ms = if ms <= 10 { 100 } else { ms };
        match kept.last_mut() {
            Some((_, shown)) if *shown < shortest => *shown += ms,
            _ => {
                if kept.len() == 2 * max {
                    kept = halve(kept);
                    shortest *= 2;
                }
                kept.push((make(frame), ms));
            }
        }
    }
    thin_to(kept, max)
}

/// Every two frames become one, showing for both.
fn halve<P>(frames: Vec<(P, u32)>) -> Vec<(P, u32)> {
    let mut out: Vec<(P, u32)> = Vec::with_capacity(frames.len() / 2 + 1);
    for (i, (frame, ms)) in frames.into_iter().enumerate() {
        match out.last_mut() {
            Some(last) if i % 2 == 1 => last.1 += ms,
            _ => out.push((frame, ms)),
        }
    }
    out
}

/// At most `max` frames: the ones on show at `max` evenly spaced moments
/// of the loop, each kept for the stretch it stands for (a long pause
/// keeps its length, short frames are sampled evenly). The loop lasts as
/// long as it did, and starts on the same frame.
fn thin_to<P>(frames: Vec<(P, u32)>, max: usize) -> Vec<(P, u32)> {
    if frames.len() <= max || max == 0 {
        return frames;
    }
    let total: u64 = frames.iter().map(|f| u64::from(f.1)).sum();
    // Where moment j falls, in whole milliseconds, so the stretches add up
    // to the loop exactly.
    let moment = |j: usize| j as u64 * total / max as u64;
    let mut out: Vec<(P, u32)> = Vec::with_capacity(max);
    let (mut next, mut start) = (0usize, 0u64);
    for (frame, ms) in frames {
        let end = start + u64::from(ms);
        let first = next;
        while next < max && moment(next) < end {
            next += 1;
        }
        if next > first {
            let until = if next < max { moment(next) } else { total };
            out.push((frame, (until - moment(first)) as u32));
        }
        start = end;
    }
    out
}

/// The sides a sheet of frames may have: what every graphics card takes,
/// and what the card says it takes, up to 4096.
const MIN_SHEET: usize = 2048;
const MAX_SHEET: usize = 4096;

/// A transparent pixel around every frame of a sheet, so a frame drawn
/// smaller or larger never shows a sliver of its neighbour.
const GUTTER: usize = 1;

/// An animation's frames in one picture, and where each one is in it.
struct Sheet {
    image: ColorImage,
    uvs: Vec<Rect>,
    delays: Vec<u32>,
}

/// The frames side by side in rows, as square as they go, in one picture
/// of at most `side` pixels a side (fewer frames, evenly spread, when they
/// do not all fit): one texture per animation, not one per frame.
fn sheet(frames: Vec<(ColorImage, u32)>, side: usize) -> Sheet {
    let [w, h] = frames.first().map_or([1, 1], |f| f.0.size);
    let (cw, ch) = (w + 2 * GUTTER, h + 2 * GUTTER);
    let (fit_cols, fit_rows) = ((side / cw).max(1), (side / ch).max(1));
    let frames = thin_to(frames, fit_cols * fit_rows);
    let n = frames.len().max(1);
    let cols = n.isqrt();
    let cols = if cols * cols < n { cols + 1 } else { cols }.min(fit_cols);
    let rows = n.div_ceil(cols);
    let (sw, sh) = (cols * cw, rows * ch);
    let mut pixels = vec![egui::Color32::TRANSPARENT; sw * sh];
    let mut uvs = Vec::with_capacity(n);
    let mut delays = Vec::with_capacity(n);
    for (k, (img, ms)) in frames.into_iter().enumerate() {
        let (x0, y0) = ((k % cols) * cw + GUTTER, (k / cols) * ch + GUTTER);
        if img.size == [w, h] {
            for (y, line) in img.pixels.chunks_exact(w).enumerate() {
                let at = (y0 + y) * sw + x0;
                pixels[at..at + w].copy_from_slice(line);
            }
        }
        // The frame's own pixels; smoothing at its edges reads at most one
        // pixel past them, which is the gutter.
        let (fx, fy) = (sw as f32, sh as f32);
        uvs.push(Rect::from_min_max(
            egui::pos2(x0 as f32 / fx, y0 as f32 / fy),
            egui::pos2((x0 + w) as f32 / fx, (y0 + h) as f32 / fy),
        ));
        delays.push(ms);
    }
    Sheet {
        image: ColorImage::new([sw, sh], pixels),
        uvs,
        delays,
    }
}

/// Pixels this close to see-through are made fully so: some stickers carry
/// a haze over the whole frame (two of GIPHY's trending cats: white at
/// 2/255, over 52 and 72 % of it), invisible on GIPHY's white pages and a
/// faint box on this app's dark background. At 3/255 a pixel shows on no
/// background.
const HAZE: u8 = 3;

/// A frame as a texture's picture, premultiplied first, so shrinking does
/// not darken the see-through edges; shrunk to `px` when it is over it by
/// more than a tenth (GIPHY's 200 px for 160 points keeps 36 % fewer
/// pixels, and looks better than the card shrinking it as it draws).
fn picture(mut img: image::RgbaImage, px: u32) -> ColorImage {
    for p in img.pixels_mut() {
        if p[3] <= HAZE {
            p.0 = [0; 4];
            continue;
        }
        let a = u16::from(p[3]);
        for c in &mut p.0[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest > px + px / 10 {
        let scale = px as f32 / longest as f32;
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    }
    ColorImage::from_rgba_premultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame, Rgba, RgbaImage};

    /// An animated GIF of solid 40x20 frames.
    fn gif(colors: &[[u8; 4]], delays: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            enc.set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            for (c, d) in colors.iter().zip(delays) {
                let img = RgbaImage::from_pixel(40, 20, Rgba(*c));
                let delay = Delay::from_numer_denom_ms(*d, 1);
                enc.encode_frame(Frame::from_parts(img, 0, 0, delay))
                    .unwrap();
            }
        }
        out
    }

    /// An animated WebP, as GIPHY sends stickers: lossless frames of
    /// `w`x`h`, one colour each, each replacing the one before.
    fn webp(colors: &[[u8; 4]], delays: &[u32], (w, h): (u32, u32)) -> Vec<u8> {
        let frames: Vec<WebpFrame> = colors
            .iter()
            .zip(delays)
            .map(|(&color, &ms)| WebpFrame {
                color,
                rect: (0, 0, w, h),
                ms,
                blend: false,
                dispose: false,
            })
            .collect();
        webp_frames((w, h), &frames)
    }

    /// One frame of `webp_frames`: a colour over `rect` (x, y, width,
    /// height; x and y even), blended over what is there or replacing it,
    /// and cleared ("disposed") before the next frame or not.
    struct WebpFrame {
        color: [u8; 4],
        rect: (u32, u32, u32, u32),
        ms: u32,
        blend: bool,
        dispose: bool,
    }

    fn webp_frames((w, h): (u32, u32), frames: &[WebpFrame]) -> Vec<u8> {
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
        let mut body = Vec::new();
        // Animated, with alpha; the canvas.
        let mut vp8x = vec![0x02 | 0x10, 0, 0, 0];
        u24(&mut vp8x, w - 1);
        u24(&mut vp8x, h - 1);
        chunk(&mut body, b"VP8X", &vp8x);
        // The background colour hint GIPHY's files carry: opaque white,
        // which the browsers (and libwebp) do not paint.
        chunk(&mut body, b"ANIM", &[255, 255, 255, 255, 0, 0]);
        for f in frames {
            let (x, y, fw, fh) = f.rect;
            let img = RgbaImage::from_pixel(fw, fh, Rgba(f.color));
            let mut file = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut file)
                .encode(img.as_raw(), fw, fh, image::ExtendedColorType::Rgba8)
                .unwrap();
            // The still picture's VP8L chunk, as it is.
            let at = file.windows(4).position(|x| x == b"VP8L").unwrap();
            let len = u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
            let mut frame = Vec::new();
            u24(&mut frame, x / 2);
            u24(&mut frame, y / 2);
            u24(&mut frame, fw - 1);
            u24(&mut frame, fh - 1);
            u24(&mut frame, f.ms);
            // Bit 1: do not blend; bit 0: dispose.
            frame.push(u8::from(!f.blend) << 1 | u8::from(f.dispose));
            chunk(&mut frame, b"VP8L", &file[at + 8..at + 8 + len]);
            chunk(&mut body, b"ANMF", &frame);
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn ids_are_the_ones_the_server_takes() {
        assert!(valid_id("xT4uQulxzV39haRFjG"));
        assert!(!valid_id(""));
        assert!(!valid_id("a-b"));
        assert!(!valid_id("../x"));
        assert!(!valid_id(&"a".repeat(65)));
    }

    #[test]
    fn answers_keep_giphys_order_and_prefer_webp() {
        let body: Value = serde_json::from_str(
            r#"{"data":[
                {"id":"abc123","title":"Oi Sticker","username":"artista",
                 "user":{"username":"artista","display_name":"Artista Legal"},"analytics":{
                    "onload":{"url":"https://giphy-analytics.giphy.com/v2/pingback_simple?p=e%3DZ&action_type=SEEN"},
                    "onclick":{"url":"https://giphy-analytics.giphy.com/v2/pingback_simple?action_type=CLICK"},
                    "onsent":{"url":"https://giphy-analytics.giphy.com/v2/pingback_simple?action_type=SENT"}},
                 "images":{
                    "fixed_width_small":{"url":"https://media2.giphy.com/media/abc123/100w.gif?cid=x&ct=s",
                        "webp":"https://media2.giphy.com/media/abc123/100w.webp?cid=x&ct=s","width":"100","height":"80"},
                    "fixed_width":{"url":"https://media2.giphy.com/media/abc123/200w.gif?cid=x&ct=s",
                        "webp":"https://media2.giphy.com/media/abc123/200w.webp?cid=x&ct=s","width":"200","height":"160"}}},
                {"id":"semwebp","title":" ","alt_text":"Um gato","images":{
                    "original":{"url":"https://media.giphy.com/media/semwebp/giphy.gif","width":480,"height":"480"}}},
                {"id":"sem/id","images":{"fixed_width":{"webp":"https://media.giphy.com/x.webp"}}},
                {"id":"semimagem","images":{}}
            ],"pagination":{"total_count":2,"count":4,"offset":0}}"#,
        )
        .unwrap();
        let list = parse_list(&body);
        let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["abc123", "semwebp"]);
        let a = &list[0];
        assert_eq!(a.title, "Oi Sticker");
        assert_eq!(
            a.small.url,
            "https://media2.giphy.com/media/abc123/100w.webp?cid=x&ct=s"
        );
        assert_eq!(a.small.size, Vec2::new(100.0, 80.0));
        assert_eq!(
            a.large.url,
            "https://media2.giphy.com/media/abc123/200w.webp?cid=x&ct=s"
        );
        assert!(a.pings[0].ends_with("action_type=SEEN"));
        assert!(a.pings[2].ends_with("action_type=SENT"));
        // Its creator is named, as GIPHY's terms ask.
        assert_eq!(
            a.about(),
            "Oi Sticker\nPor Artista Legal (@artista) no GIPHY"
        );
        assert_eq!(a.label(), "Oi Sticker");
        // No WebP, no small sizes: the original GIF, for both.
        let b = &list[1];
        assert_eq!((b.title.as_str(), b.alt.as_str()), ("", "Um gato"));
        assert_eq!(
            (b.label(), b.about().as_str()),
            ("Um gato", "Um gato\nGIPHY")
        );
        assert_eq!(b.small, b.large);
        assert_eq!(b.small.size, Vec2::new(480.0, 480.0));
        assert!(b.pings.iter().all(String::is_empty));
        assert_eq!(count(&body["pagination"]["total_count"]), 2);
        // A creator with only a user name.
        let only = creator_of(&json!({ "username": "so_nome", "user": { "display_name": " " } }));
        assert_eq!(
            only,
            Some(Creator {
                name: "so_nome".into(),
                username: "so_nome".into()
            })
        );
        assert_eq!(creator_of(&json!({ "username": "" })), None);
    }

    #[test]
    fn giphys_answer_in_trouble_is_an_error() {
        // 200, an empty response id and nothing else (GIPHY's "synthetic
        // response"): not an empty result, or a lookup would call every
        // sticker gone.
        assert!(synthetic(
            &json!({ "meta": { "status": 200, "response_id": "" } })
        ));
        assert!(synthetic(
            &json!({ "data": [], "meta": { "response_id": "" } })
        ));
        assert!(!synthetic(
            &json!({ "data": [{ "id": "a" }], "meta": { "response_id": "" } })
        ));
        assert!(!synthetic(
            &json!({ "data": [], "meta": { "response_id": "r1" } })
        ));
        assert!(!synthetic(&json!({ "data": [] })));
    }

    #[test]
    fn pingbacks_say_who_and_when_without_saying_who() {
        assert_eq!(
            pingback("https://x.giphy.com/p?a=1", "c0ffee", 1_527_703_430_507),
            "https://x.giphy.com/p?a=1&customer_id=c0ffee&ts=1527703430507"
        );
        assert_eq!(
            pingback("https://x.giphy.com/p", "c", 5),
            "https://x.giphy.com/p?customer_id=c&ts=5"
        );
        let ana = customer_id(7, "ana");
        assert_eq!(ana.len(), 32);
        assert!(ana.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(ana, customer_id(7, "ana"), "the same on every device");
        assert_ne!(ana, customer_id(8, "ana"));
        assert!(!ana.contains("ana"));
        assert_eq!(customer_id(0, ""), "");
    }

    #[test]
    fn only_giphys_addresses_are_fetched() {
        for url in [
            "https://media2.giphy.com/media/abc/200w.webp?cid=x&ct=s",
            "https://giphy-analytics.giphy.com/v2/pingback_simple?x=1",
            "https://GIPHY.com/stickers/x",
        ] {
            assert!(giphys(url), "{url}");
        }
        for url in [
            "http://media.giphy.com/x.webp",
            "https://giphy.com.evil.example/x",
            "https://evil.example/?giphy.com",
            "https://media.giphy.com@evil.example/x",
            "https://evilgiphy.com/x",
            "file:///C:/x.webp",
        ] {
            assert!(!giphys(url), "{url}");
        }
    }

    #[test]
    fn short_and_many_frames_are_merged_and_the_loop_keeps_its_length() {
        // 0 and 10 ms mean 100, as in browsers; three 20 ms frames make one.
        let short = [0, 10, 20, 20, 20, 100].into_iter().enumerate();
        let kept = pick(short, MAX_FRAMES, |i| i);
        assert_eq!(kept, [(0, 100), (1, 100), (2, 60), (5, 100)]);
        // 500 frames of 60 ms: at most MAX_FRAMES, spread over the whole
        // loop, which lasts as long as it did.
        let kept = pick((0..500).map(|i| (i, 60)), MAX_FRAMES, |i| i);
        assert!(kept.len() <= MAX_FRAMES, "{}", kept.len());
        assert!(kept.len() >= MAX_FRAMES / 2, "{}", kept.len());
        assert_eq!(kept.iter().map(|k| k.1).sum::<u32>(), 500 * 60);
        assert_eq!(kept[0].0, 0);
        assert!(kept.last().unwrap().0 >= 490, "{:?}", kept.last());
        assert!(
            kept.windows(2)
                .all(|w| w[0].0 < w[1].0 && w[1].0 - w[0].0 <= 8),
            "{kept:?}"
        );
        // A short animation stays as it is.
        let kept = pick((0..10).map(|i| (i, 70)), MAX_FRAMES, |i| i);
        assert_eq!(kept.len(), 10);
        // Thinned by time: a one second pause among 50 ms frames keeps its
        // second, and the rest are spread evenly.
        let frames: Vec<(usize, u32)> = (0..41)
            .map(|i| (i, if i == 20 { 1000 } else { 50 }))
            .collect();
        let kept = thin_to(frames, 15);
        assert!(kept.len() <= 15, "{kept:?}");
        assert_eq!(kept.iter().map(|k| k.1).sum::<u32>(), 40 * 50 + 1000);
        assert_eq!(kept[0].0, 0, "starts on the first frame");
        let pause = kept.iter().find(|k| k.0 == 20).expect("the pause stays");
        assert!(pause.1 >= 1000, "{kept:?}");
        let others: Vec<u32> = kept.iter().filter(|k| k.0 != 20).map(|k| k.1).collect();
        let (lo, hi) = (others.iter().min().unwrap(), others.iter().max().unwrap());
        assert!(hi - lo <= 50, "uneven: {kept:?}");
        // Fewer when big: the frames of 200 px (40 000 pixels) at 160
        // pixels (25 600) fit ANIM_PIXELS 156 times, and MAX_FRAMES caps
        // that; at 512, 100 of them.
        assert_eq!(
            frames_for((200, 200), 160),
            MAX_FRAMES.min(ANIM_PIXELS / (160 * 160))
        );
        assert_eq!(frames_for((200, 200), 512), ANIM_PIXELS / (200 * 200));
        assert_eq!(frames_for((4000, 1), 4000), 1000.min(MAX_FRAMES));
        assert_eq!(frames_for((2000, 2000), 2000), 2, "never fewer than two");
    }

    #[test]
    fn unused_animations_go_but_not_the_ones_on_screen() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = egui::Context::default();
        let g = Giphy::at(ctx.clone(), rt.handle().clone(), "http://127.0.0.1:9", true);
        let white = ColorImage::filled([2, 2], egui::Color32::WHITE);
        let texture = ctx.load_texture("t", white, TextureOptions::LINEAR);
        let anim = || {
            Arc::new(Anim {
                texture: texture.clone(),
                uvs: vec![Rect::ZERO],
                delays: vec![100],
                total: 100,
                pixels: 4,
            })
        };
        let t0 = Instant::now();
        let mut s = g.lock();
        s.frame_at = t0;
        let key = |name: &str| (name.to_owned(), 72);
        // Drawn by the last frame: on screen, in a window left alone since.
        let shown = Slot::Ready(anim(), t0 + Duration::from_millis(5));
        s.anims.insert(key("na tela"), shown);
        // Not drawn by it: one long ago, one a moment before.
        let old = Slot::Ready(anim(), t0 - UNUSED_FOR);
        s.anims.insert(key("antiga"), old);
        let recent = Slot::Ready(anim(), t0 - Duration::from_secs(1));
        s.anims.insert(key("recente"), recent);
        s.anims.insert(key("chegando"), Slot::Loading);
        assert!(s.sweep(t0 + Duration::from_secs(10)));
        let mut left: Vec<&str> = s.anims.keys().map(|k| k.0.as_str()).collect();
        left.sort_unstable();
        assert_eq!(left, ["chegando", "na tela", "recente"]);
        // A frame came (at +10 s) without them: after a while, they go.
        assert!(s.sweep(t0 + Duration::from_secs(40)));
        let left: Vec<&str> = s.anims.keys().map(|k| k.0.as_str()).collect();
        assert_eq!(left, ["chegando"]);
        s.anims.clear();
        assert!(
            !s.sweep(t0 + Duration::from_secs(41)),
            "nothing left to ask frames for"
        );
    }

    #[test]
    fn stickers_out_of_step_still_change_on_one_beat() {
        let ctx = egui::Context::default();
        let white = ColorImage::filled([2, 2], egui::Color32::WHITE);
        let texture = ctx.load_texture("t", white, TextureOptions::LINEAR);
        let anim = |delays: Vec<u32>| Anim {
            texture: texture.clone(),
            uvs: (0..delays.len())
                .map(|i| Rect::from_min_size(egui::pos2(i as f32, 0.0), Vec2::splat(1.0)))
                .collect(),
            total: delays.iter().sum(),
            delays,
            pixels: 4,
        };
        // Three stickers of their own timing, drawn whenever any of them
        // asks: the window is drawn only on the beat, the frames shown are
        // never late by a beat or more, and every frame of each still shows.
        let stickers = [
            anim(vec![70, 70]),
            anim(vec![130, 60, 90]),
            anim(vec![55; 7]),
        ];
        let mut t = 0.0f64;
        let mut draws = 0;
        let mut shown: Vec<HashSet<usize>> = vec![HashSet::new(); stickers.len()];
        while t < 10_000.0 {
            let mut next = f64::MAX;
            for (k, a) in stickers.iter().enumerate() {
                let (uv, wait) = a.frame(t / 1000.0, true).unwrap();
                let i = uv.min.x as usize;
                shown[k].insert(i);
                // The frame due at the last beat: never late by a beat.
                let beat = (t / BEAT_MS).floor() * BEAT_MS;
                let due = at(&a.delays, a.total, beat.round() as u64).0;
                assert_eq!(i, due, "at {t:.1} ms");
                next = next.min(t + wait.unwrap().as_secs_f64() * 1000.0);
            }
            draws += 1;
            let beats = (next - BEAT_SLACK_MS) / BEAT_MS;
            assert!(
                (beats - beats.round()).abs() < 1e-6,
                "a draw off the beat at {next}"
            );
            assert!(next > t);
            t = next;
        }
        assert!(
            draws <= (10_000.0 / BEAT_MS) as usize + 1,
            "{draws} draws in 10 s"
        );
        assert!(
            shown
                .iter()
                .zip(&stickers)
                .all(|(s, a)| s.len() == a.uvs.len()),
            "{shown:?}"
        );
        // Standing still: no frame asked for.
        assert_eq!(stickers[0].frame(1.234, false).unwrap().1, None);
    }

    #[test]
    fn frames_go_side_by_side_in_one_picture() {
        let frame = |k: u8| {
            let mut img = ColorImage::filled([10, 8], egui::Color32::from_gray(k));
            img.pixels[0] = egui::Color32::from_rgb(k, 0, 0);
            img
        };
        let s = sheet(
            (0..5)
                .map(|k| (frame(k * 40), 100 + u32::from(k)))
                .collect(),
            2048,
        );
        // Five frames: three columns, two rows, each frame with its gutter.
        assert_eq!(s.image.size, [3 * 12, 2 * 10]);
        assert_eq!(s.delays, [100, 101, 102, 103, 104]);
        let [sw, sh] = s.image.size.map(|v| v as f32);
        let uv = s.uvs[4];
        let (x0, y0) = (
            (uv.min.x * sw).round() as usize,
            (uv.min.y * sh).round() as usize,
        );
        assert_eq!((x0, y0), (12 + 1, 10 + 1));
        assert_eq!(
            ((uv.width() * sw).round(), (uv.height() * sh).round()),
            (10.0, 8.0)
        );
        assert_eq!(
            s.image.pixels[y0 * 36 + x0],
            egui::Color32::from_rgb(160, 0, 0)
        );
        // The gutter is see-through.
        assert_eq!(
            s.image.pixels[(y0 - 1) * 36 + x0],
            egui::Color32::TRANSPARENT
        );
        // When they do not all fit, fewer go, spread over the whole loop.
        let big = |_| (ColorImage::filled([100, 100], egui::Color32::WHITE), 60);
        let s = sheet((0..50).map(big).collect(), 512);
        assert_eq!(s.uvs.len(), 25);
        assert!(
            s.image.size[0] <= 512 && s.image.size[1] <= 512,
            "{:?}",
            s.image.size
        );
        assert_eq!(s.delays.iter().sum::<u32>(), 50 * 60);
    }

    #[test]
    fn the_clock_picks_the_frame() {
        let delays = [100, 50, 150];
        assert_eq!(at(&delays, 300, 0), (0, 100));
        assert_eq!(at(&delays, 300, 99), (0, 1));
        assert_eq!(at(&delays, 300, 100), (1, 50));
        assert_eq!(at(&delays, 300, 149), (1, 1));
        assert_eq!(at(&delays, 300, 150), (2, 150));
        assert_eq!(at(&delays, 300, 299), (2, 1));
        assert_eq!(at(&delays, 300, 300), (0, 100), "and round again");
        assert_eq!(at(&delays, 300, 300 * 1000 + 120), (1, 30));
        assert_eq!(fit(Vec2::new(200.0, 100.0), 160.0), Vec2::new(160.0, 80.0));
        assert_eq!(fit(Vec2::ZERO, 160.0), Vec2::splat(160.0));
    }

    #[test]
    fn animated_gifs_keep_their_timing_and_shrink_to_fit() {
        let bytes = gif(
            &[[255, 0, 0, 255], [0, 0, 255, 255], [0, 255, 0, 255]],
            &[100, 200, 300],
        );
        let frames = decode(&bytes, 64).unwrap();
        let delays: Vec<u32> = frames.iter().map(|f| f.1).collect();
        assert_eq!(delays, [100, 200, 300]);
        assert_eq!(frames[0].0.size, [40, 20]);
        let (red, blue) = (frames[0].0.pixels[0], frames[1].0.pixels[0]);
        assert!(red.r() > 200 && red.b() < 50, "{red:?}");
        assert!(blue.b() > 200 && blue.r() < 50, "{blue:?}");
        let small = decode(&bytes, 16).unwrap();
        assert_eq!(small[0].0.size, [16, 8]);
        // A still picture is one frame, standing.
        let mut png = Vec::new();
        RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(decode(&png, 64).unwrap().len(), 1);
        assert!(decode(b"isto nao e uma figurinha", 64).is_err());
        assert!(
            decode(&bytes[..40], 64).is_err(),
            "cut short before a frame"
        );
    }

    #[test]
    fn animated_webp_stickers_come_apart_into_frames() {
        let bytes = webp(&[[255, 0, 0, 255], [0, 255, 0, 128]], &[100, 250], (24, 16));
        let frames = decode(&bytes, 64).unwrap();
        let delays: Vec<u32> = frames.iter().map(|f| f.1).collect();
        assert_eq!(delays, [100, 250]);
        assert_eq!(frames[0].0.size, [24, 16]);
        assert_eq!(frames[0].0.pixels[0], egui::Color32::from_rgb(255, 0, 0));
        // Half see-through green, premultiplied: half the green.
        let half = frames[1].0.pixels[0];
        assert_eq!((half.r(), half.a()), (0, 128), "{half:?}");
        assert!((i32::from(half.g()) - 128).abs() <= 1, "{half:?}");
    }

    /// Runs sticker files (BABUTELA_FIGURINHAS/files/*.webp|gif, downloaded
    /// from GIPHY) through everything the app does with them (decoding,
    /// merging frames, shrinking, the sheet) and writes the sheets out, for
    /// a comparison with libwebp's frames outside (`cargo test -- --ignored
    /// figurinhas_reais`).
    #[test]
    #[ignore = "precisa de figurinhas baixadas do GIPHY em BABUTELA_FIGURINHAS"]
    fn figurinhas_reais() {
        let Ok(dir) = std::env::var("BABUTELA_FIGURINHAS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let out = dir.join("app");
        std::fs::create_dir_all(&out).unwrap();
        let mut summary = Vec::new();
        for entry in std::fs::read_dir(dir.join("files")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // The grid's 72 points and the chat's 160, at 100 %.
            let px = if name.contains(".large.") { 160 } else { 72 };
            let bytes = std::fs::read(&path).unwrap();
            let kept = match decode_kept(&bytes, px) {
                Ok(k) => k,
                Err(e) => {
                    summary.push(json!({ "file": name, "error": e }));
                    continue;
                }
            };
            let sources: Vec<usize> = kept.iter().map(|((i, _), _)| *i).collect();
            let frames = kept.into_iter().map(|((_, img), ms)| (img, ms)).collect();
            let s = sheet(frames, MIN_SHEET);
            let [w, h] = s.image.size;
            let raw: Vec<u8> = s.image.pixels.iter().flat_map(|c| c.to_array()).collect();
            std::fs::write(out.join(format!("{name}.sheet")), raw).unwrap();
            let uvs: Vec<[f32; 4]> = s
                .uvs
                .iter()
                .map(|r| {
                    [
                        r.min.x * w as f32,
                        r.min.y * h as f32,
                        r.max.x * w as f32,
                        r.max.y * h as f32,
                    ]
                })
                .collect();
            summary.push(json!({
                "file": name, "sheet": [w, h], "uvs": uvs, "sources": sources, "delays": s.delays,
            }));
        }
        std::fs::write(
            out.join("summary.json"),
            serde_json::to_vec(&summary).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_haze_over_the_frame_is_left_out() {
        // White at 2/255 (an encoder's haze) goes; a soft shadow at 4/255
        // and the sticker itself stay, premultiplied.
        let mut img = RgbaImage::from_pixel(4, 1, Rgba([255, 255, 255, 2]));
        img.put_pixel(1, 0, Rgba([0, 0, 0, 4]));
        img.put_pixel(2, 0, Rgba([200, 100, 50, 128]));
        img.put_pixel(3, 0, Rgba([10, 20, 30, 255]));
        let px = picture(img, 64).pixels;
        assert_eq!(px[0], egui::Color32::TRANSPARENT);
        assert_eq!(px[1].a(), 4);
        assert_eq!(
            px[2],
            egui::Color32::from_rgba_premultiplied(100, 50, 25, 128)
        );
        assert_eq!(px[3], egui::Color32::from_rgb(10, 20, 30));
    }

    #[test]
    fn a_disposed_frame_leaves_nothing_behind() {
        // A red frame over the whole canvas, then a small green one blended
        // in its corner. With the red one disposed, the rest of the canvas
        // is see-through (as libwebp and the browsers show it); without,
        // the red stays.
        let frames = |dispose: bool| {
            vec![
                WebpFrame {
                    color: [255, 0, 0, 255],
                    rect: (0, 0, 24, 16),
                    ms: 100,
                    blend: false,
                    dispose,
                },
                WebpFrame {
                    color: [0, 255, 0, 255],
                    rect: (0, 0, 8, 8),
                    ms: 100,
                    blend: true,
                    dispose: false,
                },
            ]
        };
        // Blending rounds: within one step of each channel.
        let at = |img: &ColorImage, x: usize, y: usize| img.pixels[y * img.size[0] + x].to_array();
        let near =
            |got: [u8; 4], want: [u8; 4]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1);
        let (green, red, clear) = ([0, 255, 0, 255], [255, 0, 0, 255], [0, 0, 0, 0]);
        let disposed = decode(&webp_frames((24, 16), &frames(true)), 64).unwrap();
        assert!(
            near(at(&disposed[1].0, 2, 2), green),
            "{:?}",
            at(&disposed[1].0, 2, 2)
        );
        assert_eq!(at(&disposed[1].0, 20, 12), clear, "left behind");
        let kept = decode(&webp_frames((24, 16), &frames(false)), 64).unwrap();
        assert!(near(at(&kept[1].0, 2, 2), green));
        assert!(near(at(&kept[1].0, 20, 12), red), "not disposed: it stays");
    }

    // ------------------------------------------------------------ a fake GIPHY

    struct Fake {
        base: String,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        fn asked(&self, prefix: &str) -> Vec<String> {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.starts_with(prefix))
                .cloned()
                .collect()
        }
    }

    fn item(base: &str, id: &str) -> Value {
        let media = json!({ "url": format!("{base}/m/{id}.gif"), "width": "40", "height": "20" });
        let ping = |kind: &str| json!({ "url": format!("{base}/ping?action_type={kind}&id={id}") });
        json!({
            "id": id, "title": format!("Figurinha {id}"),
            "images": { "fixed_width_small": media, "fixed_width": media },
            "analytics": { "onload": ping("SEEN"), "onclick": ping("CLICK"), "onsent": ping("SENT") },
        })
    }

    /// What the fake answers: trending has two stickers, a search is over
    /// the limit, lookups know the ids starting with a or b.
    fn answer(base: &str, target: &str, gif: &[u8]) -> (u16, &'static str, Vec<u8>) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                .unwrap_or("")
                .replace("%2C", ",")
        };
        let json_of = |v: Value| (200, "application/json", v.to_string().into_bytes());
        match path {
            "/v1/stickers/trending" => json_of(json!({
                "data": [item(base, "a1"), item(base, "b2")],
                "pagination": { "total_count": 2, "count": 2, "offset": 0 },
            })),
            "/v1/stickers/search" if param("q") == "sintetico" => json_of(json!({
                "data": [], "meta": { "status": 200, "msg": "OK", "response_id": "" },
            })),
            "/v1/stickers/search" => (429, "application/json", b"{}".to_vec()),
            "/v1/gifs" if param("ids").contains("erro") => {
                (503, "application/json", b"{}".to_vec())
            }
            "/v1/gifs" => json_of(json!({
                "data": param("ids")
                    .split(',')
                    .filter(|id| id.starts_with('a') || id.starts_with('b'))
                    .map(|id| item(base, id))
                    .collect::<Vec<_>>(),
            })),
            p if p.starts_with("/m/") => (200, "image/gif", gif.to_vec()),
            "/ping" => json_of(json!({})),
            _ => (404, "text/plain", Vec::new()),
        }
    }

    fn fake(rt: &tokio::runtime::Runtime) -> Fake {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let gif = gif(
            &[[255, 0, 0, 255], [0, 0, 255, 255], [0, 255, 0, 255]],
            &[100, 100, 100],
        );
        let (b, log) = (base.clone(), asked.clone());
        rt.spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (b, log, gif) = (b.clone(), log.clone(), gif.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head);
                    let target = text.split(' ').nth(1).unwrap_or("").to_owned();
                    log.lock().unwrap().push(target.clone());
                    let (status, kind, body) = answer(&b, &target, &gif);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        Fake { base, asked }
    }

    fn wait<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < until, "tempo esgotado esperando {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn talks_to_giphy_the_way_it_asks() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let fake = fake(&rt);
        let g = Giphy::at(
            egui::Context::default(),
            rt.handle().clone(),
            &fake.base,
            true,
        );

        // Without a key nothing goes out, and the stickers are off.
        assert!(!g.enabled());
        assert!(matches!(g.sticker("a1"), Lookup::Off));
        g.show("");
        g.set_key("chave", 7, "ana");
        assert!(g.enabled());

        // Trending, in GIPHY's order, with the key, the rating and whose
        // request it is.
        g.show("");
        let view = wait("em alta", || Some(g.feed("")).filter(|v| !v.loading));
        assert_eq!(view.ids, ["a1", "b2"]);
        assert!(view.done && view.error.is_none(), "{view:?}");
        let trending = fake.asked("/v1/stickers/trending");
        assert_eq!(trending.len(), 1, "{trending:?}");
        let call = &trending[0];
        assert!(
            call.starts_with("/v1/stickers/trending?api_key=chave&"),
            "{call}"
        );
        assert!(call.contains("rating=pg-13") && call.contains("bundle=messaging_non_clips"));
        assert!(call.contains(&format!("customer_id={}", customer_id(7, "ana"))));
        g.show("");
        g.more("");
        assert_eq!(fake.asked("/v1/stickers/trending").len(), 1, "asked once");

        // A search over the limit says so, and can be tried again.
        g.show("gato");
        let view = wait("a busca", || Some(g.feed("gato")).filter(|v| !v.loading));
        assert!(
            view.error.as_deref().is_some_and(|e| e.contains("limite")),
            "{view:?}"
        );
        assert!(fake.asked("/v1/stickers/search")[0].contains("q=gato&lang=pt"));
        // GIPHY in trouble ("synthetic" answer) is an error, not nothing.
        g.show("sintetico");
        let view = wait("a resposta sintética", || {
            Some(g.feed("sintetico")).filter(|v| !v.loading)
        });
        assert!(
            view.error
                .as_deref()
                .is_some_and(|e| e.contains("problemas")),
            "{view:?}"
        );

        // Ids from elsewhere (favourites, messages) go in one lookup; what
        // GIPHY does not send is gone.
        for id in ["a9", "b8", "zzz"] {
            assert!(matches!(g.sticker(id), Lookup::Loading), "{id}");
        }
        wait("a busca por id", || {
            matches!(g.sticker("zzz"), Lookup::Missing).then_some(())
        });
        assert!(matches!(g.sticker("a9"), Lookup::Ready(_)));
        let lookups = fake.asked("/v1/gifs?");
        assert_eq!(lookups.len(), 1, "{lookups:?}");
        assert!(lookups[0].contains("ids=a9%2Cb8%2Czzz"), "{}", lookups[0]);

        // The animation: three frames, in memory.
        let Lookup::Ready(a1) = g.sticker("a1") else {
            panic!("a1 veio no em alta");
        };
        assert!(matches!(g.anim(&a1.small, 40.0), Frames::Loading));
        let anim = wait("a animação", || match g.anim(&a1.small, 40.0) {
            Frames::Ready(a) => Some(a),
            _ => None,
        });
        assert_eq!(anim.frame_count(), 3);
        assert_eq!(fake.asked("/m/a1.gif").len(), 1, "fetched once");
        // Anything not from GIPHY (here, not from the fake) is not fetched.
        let elsewhere = Media {
            url: "http://example.com/x.gif".into(),
            size: Vec2::ZERO,
        };
        assert!(matches!(g.anim(&elsewhere, 40.0), Frames::Failed));

        // Seen once per feed, clicked and sent when chosen; each says who
        // and when.
        g.seen("a1");
        g.seen("a1");
        g.chosen("a1");
        let pings = wait("os registros", || {
            Some(fake.asked("/ping")).filter(|p| p.len() >= 3)
        });
        let me = format!("customer_id={}", customer_id(7, "ana"));
        assert!(
            pings.iter().all(|p| p.contains(&me) && p.contains("&ts=")),
            "{pings:?}"
        );
        for kind in ["SEEN", "CLICK", "SENT"] {
            assert_eq!(
                pings.iter().filter(|p| p.contains(kind)).count(),
                1,
                "{pings:?}"
            );
        }
        let report = g.report();
        assert_eq!(report["pings"], json!({ "seen": 1, "click": 1, "sent": 1 }));
        assert_eq!(
            (report["anims"].as_u64(), report["frames"].as_u64()),
            (Some(1), Some(3))
        );

        // GIPHY failing a lookup: the ids say so (not "gone"), and the
        // next ones wait a while instead of asking again at once.
        assert!(matches!(g.sticker("erro1"), Lookup::Loading));
        wait("a falha", || {
            matches!(g.sticker("erro1"), Lookup::Failed).then_some(())
        });
        assert!(
            matches!(g.sticker("a77"), Lookup::Failed),
            "held back for a while"
        );
        assert_eq!(fake.asked("/v1/gifs?").len(), 2, "no second try at once");

        // Signing out turns them off again.
        g.set_key("", 0, "");
        assert!(matches!(g.sticker("a1"), Lookup::Off));
        rt.shutdown_timeout(Duration::from_secs(1));
    }
}

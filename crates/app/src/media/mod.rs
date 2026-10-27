//! Audio and video attachments, played inside the app.
//!
//! Audio files (MP3, M4A/AAC, OGG with Vorbis or Opus, WAV, FLAC) are
//! decoded in Rust (symphonia, and the Opus library the calls already use).
//! Videos go through Windows' own media stack (Media Foundation), which
//! brings its decoders for H.264, HEVC and VP9, in MP4, MOV and WebM, on the
//! graphics card when it can.
//!
//! Each player reads its file from the server as it plays (in pieces, so a
//! file of any size starts at once and never has to fit in memory), decodes
//! on a thread of its own a little ahead of time, and plays its sound
//! through the same mixer as the app's cues. Pictures are shown when the
//! sound reaches them.

mod audio;
pub mod mf;
pub mod output;
mod remote;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use self::output::{Clock, Feed};
use crate::model::Kind;
use crate::net::Api;
use crate::sounds::Sounds;

/// Pictures are kept at most this big: enough for the chat and for the
/// enlarged view, and a 4K video does not cost 4K textures.
pub const MAX_W: usize = 1280;
pub const MAX_H: usize = 720;

/// Pictures decoded ahead of time: 130 ms of a 30 fps video. Eight kept
/// 15 MB more of a 720p video in memory and showed the same pictures.
pub const MAX_FRAMES: usize = 4;

/// Players kept open (each holds its file, a thread and a few pictures);
/// past this, the one left alone the longest closes.
const MAX_PLAYERS: usize = 6;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Loading,
    Playing,
    Paused,
    Ended,
    Failed(String),
}

/// A decoded picture and when it shows. The decoder writes the next
/// pictures over it once the interface has uploaded it (pictures.rs).
pub struct Frame {
    pub pts: f64,
    pub image: Arc<egui::ColorImage>,
}

pub enum Cmd {
    Seek(f64),
    Quit,
}

/// How a decoder that is far enough ahead waits for something to do: a
/// picture went to the screen, the sound card took sound, a command came.
/// A ring between two waits is kept, so none is missed.
#[derive(Default)]
pub struct Wake {
    rung: Mutex<bool>,
    bell: Condvar,
}

impl Wake {
    pub fn ring(&self) {
        *Shared::lock(&self.rung) = true;
        self.bell.notify_one();
    }

    /// Until the next ring, or not at all if there was one since the last
    /// wait.
    pub fn wait(&self) {
        let rung = Shared::lock(&self.rung);
        let mut rung = self
            .bell
            .wait_while(rung, |rung| !*rung)
            .unwrap_or_else(PoisonError::into_inner);
        *rung = false;
    }
}

/// What the decoding thread and the interface share about one player.
pub struct Shared {
    /// The decoder's: rung by everything below that it may wait for.
    wake: Arc<Wake>,
    status: Mutex<Status>,
    duration: Mutex<Option<f64>>,
    size: Mutex<Option<[usize; 2]>>,
    frames: Mutex<VecDeque<Frame>>,
    feed: Mutex<Option<Arc<Feed>>>,
    clock: Clock,
    /// Set when the person pressed play; the player starts as soon as it is
    /// ready.
    wants_play: AtomicBool,
    /// The decoder reached the end of the file.
    decoded_all: AtomicBool,
    ctx: egui::Context,
}

impl Shared {
    fn new(ctx: egui::Context) -> Arc<Shared> {
        Arc::new(Shared {
            wake: Arc::default(),
            status: Mutex::new(Status::Loading),
            duration: Mutex::new(None),
            size: Mutex::new(None),
            frames: Mutex::new(VecDeque::new()),
            feed: Mutex::new(None),
            clock: Clock::default(),
            wants_play: AtomicBool::new(true),
            decoded_all: AtomicBool::new(false),
            ctx,
        })
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn feed(&self) -> Option<Arc<Feed>> {
        Self::lock(&self.feed).clone()
    }

    /// Called by the decoder once it knows what it is playing.
    fn ready(&self, duration: Option<f64>, size: Option<[usize; 2]>, feed: Option<Arc<Feed>>) {
        *Self::lock(&self.duration) = duration.filter(|d| d.is_finite() && *d > 0.0);
        *Self::lock(&self.size) = size;
        *Self::lock(&self.feed) = feed;
        let play = self.wants_play.load(Ordering::SeqCst);
        self.set_running(play);
        *Self::lock(&self.status) = if play {
            Status::Playing
        } else {
            Status::Paused
        };
        self.ctx.request_repaint();
    }

    fn fail(&self, why: String) {
        crate::logln!("mídia: {why}");
        *Self::lock(&self.status) = Status::Failed(why);
        self.ctx.request_repaint();
    }

    fn set_running(&self, running: bool) {
        let feed = self.feed();
        if let Some(f) = &feed {
            f.set_paused(!running);
        }
        self.clock.run(running, feed.as_deref());
    }

    /// The decoder moved to `t`: what was queued is from before.
    fn sought(&self, t: f64) {
        Self::lock(&self.frames).clear();
        if let Some(f) = self.feed() {
            f.clear();
        }
        let running = *Self::lock(&self.status) == Status::Playing;
        self.clock.set(t, running && self.feed().is_none());
        self.decoded_all.store(false, Ordering::SeqCst);
    }

    fn now(&self) -> f64 {
        self.clock.now(self.feed().as_deref())
    }

    /// Whether the decoder should wait before adding another picture.
    fn frames_full(&self) -> bool {
        Self::lock(&self.frames).len() >= MAX_FRAMES
    }

    fn push_frame(&self, frame: Frame) {
        Self::lock(&self.frames).push_back(frame);
    }
}

/// One attachment's player, as the interface sees it.
pub struct Player {
    pub kind: Kind,
    shared: Arc<Shared>,
    cmds: Sender<Cmd>,
    texture: Option<egui::TextureHandle>,
    muted: bool,
    /// When it was last played (a count, not a time): the oldest closes
    /// first.
    used: u64,
}

impl Player {
    pub fn status(&self) -> Status {
        Shared::lock(&self.shared.status).clone()
    }

    pub fn duration(&self) -> Option<f64> {
        *Shared::lock(&self.shared.duration)
    }

    /// The picture's size, once known (video only).
    pub fn size(&self) -> Option<[usize; 2]> {
        *Shared::lock(&self.shared.size)
    }

    pub fn position(&self) -> f64 {
        let t = self.shared.now();
        self.duration().map_or(t, |d| t.min(d))
    }

    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn playing(&self) -> bool {
        self.status() == Status::Playing
    }

    pub fn play(&mut self) {
        self.shared.wants_play.store(true, Ordering::SeqCst);
        match self.status() {
            Status::Ended => {
                self.seek(0.0);
                self.resume();
            }
            Status::Paused => self.resume(),
            _ => {}
        }
    }

    fn resume(&mut self) {
        *Shared::lock(&self.shared.status) = Status::Playing;
        self.shared.set_running(true);
        self.shared.ctx.request_repaint();
    }

    pub fn pause(&mut self) {
        self.shared.wants_play.store(false, Ordering::SeqCst);
        if self.status() == Status::Playing {
            *Shared::lock(&self.shared.status) = Status::Paused;
            self.shared.set_running(false);
        }
    }

    pub fn seek(&mut self, t: f64) {
        let t = self
            .duration()
            .map_or(t, |d| t.clamp(0.0, (d - 0.05).max(0.0)));
        if self.status() == Status::Ended {
            *Shared::lock(&self.shared.status) = Status::Paused;
        }
        self.shared.sought(t);
        self.command(Cmd::Seek(t));
    }

    /// Tells the decoder, waking it to look.
    fn command(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
        self.shared.wake.ring();
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Some(f) = self.shared.feed() {
            f.set_volume(if muted { 0.0 } else { 1.0 });
        }
    }

    /// Every frame: shows the picture whose time has come and notices the
    /// end.
    fn tick(&mut self, ctx: &egui::Context) {
        if !self.playing() {
            return;
        }
        let now = self.shared.now();
        let mut newest = None;
        {
            let mut frames = Shared::lock(&self.shared.frames);
            while frames.front().is_some_and(|f| f.pts <= now) {
                newest = frames.pop_front();
            }
        }
        if let Some(f) = newest {
            // A place for the next picture: the decoder makes it.
            self.shared.wake.ring();
            let uploading = std::time::Instant::now();
            crate::probe::count("midia.quadros", 1);
            let options = egui::TextureOptions::LINEAR;
            match &mut self.texture {
                Some(t) if t.size() == f.image.size => t.set_partial([0, 0], f.image, options),
                Some(t) => t.set(f.image, options),
                None => self.texture = Some(ctx.load_texture("midia", f.image, options)),
            }
            crate::probe::since("midia.textura", uploading);
        }
        // The next picture's time, for the repaint below.
        let next = Shared::lock(&self.shared.frames).front().map(|f| f.pts);
        let drained = self.shared.feed().is_none_or(|f| f.queued() == 0)
            && Shared::lock(&self.shared.frames).is_empty();
        let past_end = self.duration().is_none_or(|d| now >= d - 0.1);
        if self.shared.decoded_all.load(Ordering::SeqCst) && drained && past_end {
            *Shared::lock(&self.shared.status) = Status::Ended;
            self.shared.set_running(false);
            self.shared.wants_play.store(false, Ordering::SeqCst);
        }
        // A video repaints when its next picture is due (every 15 ms before,
        // twice per picture of a 30 fps video), or soon when the decoder
        // has none ready yet; the time display is enough for sound.
        let wait = match (self.kind, next) {
            (Kind::Video, Some(at)) => Duration::from_secs_f64((at - now).clamp(0.002, 0.1)),
            (Kind::Video, None) => Duration::from_millis(15),
            _ => Duration::from_millis(250),
        };
        ctx.request_repaint_after(wait);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.command(Cmd::Quit);
        if let Some(f) = self.shared.feed() {
            f.finish();
        }
    }
}

/// Every player of the session, by attachment address.
pub struct Media {
    rt: tokio::runtime::Handle,
    api: Arc<Api>,
    sounds: Arc<Sounds>,
    ctx: egui::Context,
    players: HashMap<String, Player>,
    /// Counts the times play was pressed, to know which player is oldest.
    uses: u64,
}

impl Media {
    pub fn new(
        rt: tokio::runtime::Handle,
        api: Arc<Api>,
        sounds: Arc<Sounds>,
        ctx: egui::Context,
    ) -> Media {
        Media {
            rt,
            api,
            sounds,
            ctx,
            players: HashMap::new(),
            uses: 0,
        }
    }

    pub fn get(&mut self, url: &str) -> Option<&mut Player> {
        self.players.get_mut(url)
    }

    /// For drawing: what a player shows, without changing it.
    pub fn peek(&self, url: &str) -> Option<&Player> {
        self.players.get(url)
    }

    /// What each player is doing, for the test report.
    pub fn report(&self) -> Vec<serde_json::Value> {
        self.players
            .iter()
            .map(|(url, p)| {
                serde_json::json!({
                    "url": url,
                    "status": format!("{:?}", p.status()),
                    "position": (p.position() * 10.0).round() / 10.0,
                    "duration": p.duration().map(|d| (d * 10.0).round() / 10.0),
                    "size": p.size(),
                    "picture": p.texture().is_some(),
                })
            })
            .collect()
    }

    /// Starts playing `url` (opening it first, if needed), and pauses any
    /// other player: one thing plays at a time.
    pub fn play(&mut self, url: &str, name: &str, kind: Kind) {
        self.uses += 1;
        for (u, p) in self.players.iter_mut() {
            if u != url {
                p.pause();
            }
        }
        if let Some(p) = self.players.get_mut(url) {
            if matches!(p.status(), Status::Failed(_)) {
                self.players.remove(url);
            } else {
                p.used = self.uses;
                p.play();
                return;
            }
        }
        // Room for one more: the player left alone the longest closes (they
        // are all paused by now).
        while self.players.len() >= MAX_PLAYERS {
            let oldest = self
                .players
                .iter()
                .min_by_key(|(_, p)| p.used)
                .map(|(u, _)| u.clone());
            match oldest {
                Some(u) => self.players.remove(&u),
                None => break,
            };
        }
        let mut player = self.open(url, name, kind);
        player.used = self.uses;
        self.players.insert(url.to_owned(), player);
    }

    /// Pauses everything (leaving a call, signing out).
    pub fn pause_all(&mut self) {
        for p in self.players.values_mut() {
            p.pause();
        }
    }

    pub fn tick(&mut self) {
        let ctx = self.ctx.clone();
        for p in self.players.values_mut() {
            p.tick(&ctx);
        }
    }

    fn open(&self, url: &str, name: &str, kind: Kind) -> Player {
        let shared = Shared::new(self.ctx.clone());
        let (cmds, rx) = channel();
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .filter(|e| e.len() <= 5 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
            .unwrap_or_default();
        // In the app's test mode an address may also be a file of this PC
        // (`file:///C:/...`), which is how a test plays media without
        // uploading it; anywhere else only the server's own addresses are
        // read.
        let source = match url.strip_prefix("file:///") {
            Some(path) if crate::smoke::active() => Source::Local(PathBuf::from(path)),
            Some(_) => Source::Refused,
            None => Source::Remote(self.api.absolute(url)),
        };
        let job = Job {
            kind,
            ext: ext.clone(),
            api: self.api.clone(),
            rt: self.rt.clone(),
            sounds: self.sounds.clone(),
            shared: shared.clone(),
            cmds: rx,
        };
        let spawned = std::thread::Builder::new()
            .name(format!("midia-{ext}"))
            .spawn(move || decode(source, job));
        if let Err(e) = spawned {
            shared.fail(format!("thread: {e}"));
        }
        Player {
            kind,
            shared,
            cmds,
            texture: None,
            muted: false,
            used: 0,
        }
    }
}

/// Where a player reads its file from.
enum Source {
    /// A file of this PC (the test mode's `file:///` addresses).
    Local(PathBuf),
    /// The server's copy, read in pieces as it plays.
    Remote(String),
    /// A `file:///` address outside the test mode.
    Refused,
}

/// What the player's thread needs.
struct Job {
    kind: Kind,
    ext: String,
    api: Arc<Api>,
    rt: tokio::runtime::Handle,
    sounds: Arc<Sounds>,
    shared: Arc<Shared>,
    cmds: Receiver<Cmd>,
}

/// The player's thread: opens the file and plays it until told to stop.
/// Nothing is downloaded first: sound comes in pieces (remote.rs) and video
/// through Media Foundation's own reading of the address.
fn decode(source: Source, job: Job) {
    let Job {
        kind,
        ext,
        api,
        rt,
        sounds,
        shared,
        cmds,
    } = job;
    match (kind, source) {
        (_, Source::Refused) => shared.fail("endereço local recusado".into()),
        (Kind::Audio, Source::Local(path)) => match std::fs::File::open(&path) {
            Ok(file) => audio::run(Box::new(file), &ext, &shared, &cmds, &sounds),
            Err(e) => shared.fail(format!("abrindo: {e}")),
        },
        (Kind::Audio, Source::Remote(url)) => match remote::Remote::open(api, rt, url) {
            Ok(file) => audio::run(Box::new(file), &ext, &shared, &cmds, &sounds),
            Err(e) => shared.fail(format!("baixando: {e}")),
        },
        (_, Source::Local(path)) => {
            let path = path.to_string_lossy().replace('/', "\\");
            if !mf::run(&path, &shared, &cmds, &sounds) {
                shared.fail("o Windows não abriu este vídeo".into());
            }
        }
        (_, Source::Remote(url)) => {
            // A message's video is encrypted (e2e/file.rs): Media
            // Foundation cannot read it from the server, so it comes down
            // first, decrypted on the way.
            if crate::e2e::key_for(&url).is_none() && mf::run(&url, &shared, &cmds, &sounds) {
                return;
            }
            // Media Foundation could not read the address itself (a proxy
            // in the way, say): the file comes down whole first, to disk.
            let path = temp_path(&ext);
            if let Err(e) = rt.block_on(api.save_to(&url, &path)) {
                let _ = std::fs::remove_file(&path);
                return shared.fail(format!("baixando: {e}"));
            }
            if !mf::run(&path.to_string_lossy(), &shared, &cmds, &sounds) {
                shared.fail("o Windows não abriu este vídeo".into());
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// A file in %TEMP% for a video that has to be downloaded to play; it goes
/// when the player does (and `sweep` takes what a crash left).
fn temp_path(ext: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("babutela-midia");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!(
        "{}-{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
        if ext.is_empty() { "mp4" } else { ext }
    ))
}

/// Formats seconds as the players show them: 0:07, 3:41, 1:02:03.
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Removes what earlier sessions left in %TEMP% (a crash, a forced exit).
pub fn sweep() {
    let dir = std::env::temp_dir().join("babutela-midia");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mine = format!("{}-", std::process::id());
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().starts_with(&mine) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_a_player() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(7.9), "0:07");
        assert_eq!(clock(221.0), "3:41");
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(clock(-5.0), "0:00");
    }
}

//! Music in the call: anyone pastes a YouTube or SoundCloud link, the
//! person who added it DJs (their app pulls the audio off the web with
//! yt-dlp and sends it to everyone through the same track screen share
//! sound rides on), the queue is kept in sync over the "controle" data
//! channel. The DJ hands off automatically when they leave the call.
//!
//! No server is involved: the song itself goes peer to peer, encrypted end
//! to end along with the voice.

pub mod extractor;
pub mod queue;
pub mod stream;
pub mod ytdlp;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

pub use self::queue::{DjAction, Item, Queue, State, View, handles};
pub use self::stream::Pcm;

/// A control-channel message about music, with its destination: `to` is
/// `None` for everyone in the call and `Some(uid)` for one person (an
/// `m-state` to a late joiner).
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub to: Option<i64>,
    pub msg: Value,
}

/// The music feature's shared state. Owned by `Core`; `Voice` wires it to
/// the call when one joins and clears it on leave. Methods are safe to call
/// outside a call: they do nothing until a call activates it.
pub struct Music {
    queue: Arc<Queue>,
    rt: tokio::runtime::Handle,
    toast: Arc<dyn Fn(String) + Send + Sync>,

    /// Who this PC is in the call, so the Queue can tell whether this one
    /// should play.
    me: AtomicI64,

    /// How the Control channel broadcasts an `Outgoing` to everyone (or to
    /// one person). Set when a call starts; cleared when it ends.
    outgoing: Mutex<Option<UnboundedSender<Outgoing>>>,

    /// The screen audio pipeline the music encoder writes Opus packets
    /// into (same track screen share sound uses: on the receive side the
    /// listeners cannot tell music from a game's sound). Set when a call
    /// starts; cleared when it ends.
    screen_packets: Mutex<Option<UnboundedSender<crate::voice::audio::Packet>>>,

    /// How to tell the call to add the screen audio track to each peer
    /// (`Cmd::ScreenAudio(true)` when music starts, `(false)` on stop).
    cmds: Mutex<Option<UnboundedSender<crate::voice::rtc::Cmd>>>,

    /// The call's shared audio state. The music player pushes the decoded
    /// PCM into `screen_peer(me)` so the DJ hears the song too (there is
    /// no Windows playback in the middle, and the remote peers get it
    /// through `screen_packets`, not through this).
    audio: Mutex<Option<Arc<crate::voice::audio::AudioShared>>>,

    /// A background task that watches the queue and starts or stops the
    /// local player when the DJ changes.
    dj: Mutex<Option<DjState>>,

    /// What the first `extract()` found for each song added by this app,
    /// kept so the DJ task does not run yt-dlp a second time to get the
    /// same thing. Cleared on deactivate and whenever the song it refers
    /// to is skipped or ends.
    cached: Mutex<HashMap<String, extractor::Extracted>>,

    /// Bumped whenever anything the UI shows may have changed.
    generation: AtomicU64,

    /// A toast is sent after each failed extraction, with the actual error.
    extracting: AtomicBool,
    ctx: egui::Context,
}

/// What the local player holds while the DJ is this PC.
struct DjState {
    item_id: String,
    stop: Arc<AtomicBool>,
    /// Set to true while the song is paused: the player task sleeps on it
    /// (no `yt-dlp` is run again, nothing is lost), and clearing it picks
    /// playback up where it stopped.
    paused: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Music {
    pub fn new(
        rt: tokio::runtime::Handle,
        toast: Arc<dyn Fn(String) + Send + Sync>,
        ctx: egui::Context,
    ) -> Arc<Music> {
        Arc::new(Music {
            queue: Queue::new(),
            rt,
            toast,
            me: AtomicI64::new(0),
            outgoing: Mutex::new(None),
            screen_packets: Mutex::new(None),
            cmds: Mutex::new(None),
            audio: Mutex::new(None),
            dj: Mutex::new(None),
            cached: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            extracting: AtomicBool::new(false),
            ctx,
        })
    }

    /// true while a yt-dlp extraction is running, so the UI can show "aguarde".
    pub fn extracting(&self) -> bool {
        self.extracting.load(Ordering::Relaxed)
    }

    pub fn now_playing(&self) -> Option<Item> {
        self.queue.now_playing()
    }

    pub fn view(&self) -> View {
        self.queue.view()
    }

    /// Called by Voice when the local user joins a call. The music module
    /// uses `outgoing` to post control messages to peers, encodes the
    /// decoded music to Opus and writes it into `screen_packets`, asks
    /// the call to turn on the screen audio track with `cmds`, and
    /// pushes the same PCM into `audio.screen_peer(me)` so the DJ hears
    /// what everyone else hears.
    pub fn activate(
        self: &Arc<Music>,
        me: i64,
        outgoing: UnboundedSender<Outgoing>,
        screen_packets: UnboundedSender<crate::voice::audio::Packet>,
        cmds: UnboundedSender<crate::voice::rtc::Cmd>,
        audio: Arc<crate::voice::audio::AudioShared>,
    ) {
        self.me.store(me, Ordering::Relaxed);
        *self.outgoing.lock().unwrap_or_else(PoisonError::into_inner) = Some(outgoing);
        *self
            .screen_packets
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(screen_packets);
        *self.cmds.lock().unwrap_or_else(PoisonError::into_inner) = Some(cmds);
        *self.audio.lock().unwrap_or_else(PoisonError::into_inner) = Some(audio);
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    /// Called by Voice when the user leaves the call: the queue is cleared,
    /// the DJ task (if any) is cancelled, and nothing new is posted.
    pub fn deactivate(self: &Arc<Music>) {
        self.me.store(0, Ordering::Relaxed);
        *self.outgoing.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .screen_packets
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self.cmds.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.audio.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.stop_dj();
        self.cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.queue.clear();
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    /// The members list from a call sync changed: hand the DJ over if the
    /// current one left, and send a snapshot to anyone new (handled by the
    /// caller through `snapshot_for`).
    pub fn set_members(self: &Arc<Music>, members: &[i64]) {
        let changed = self.queue.set_members(members);
        if changed {
            self.sync_dj();
        }
    }

    /// `m-state` for a late joiner (sent to one person).
    pub fn snapshot(&self) -> Value {
        self.queue.snapshot_message()
    }

    /// A music message arrived from someone else: apply it, and update the
    /// DJ player if the queue head changed.
    pub fn arrived(self: &Arc<Music>, _uid: i64, msg: &Value) {
        if self.queue.apply(msg) {
            self.sync_dj();
            self.ctx.request_repaint();
        }
    }

    /// Pastes a link in: validates it, runs yt-dlp, and posts the song to
    /// everyone (and adds it locally).
    pub fn add_url(self: &Arc<Music>, url: String, by_name: String) {
        let me = self.me.load(Ordering::Relaxed);
        if me == 0 {
            (self.toast)("Entre numa chamada para tocar música.".into());
            return;
        }
        if extractor::classify(&url).is_none() {
            (self.toast)("Só links do YouTube e do SoundCloud.".into());
            return;
        }
        if self.extracting.swap(true, Ordering::Relaxed) {
            (self.toast)("Já estou buscando outra música. Espera um pouquinho.".into());
            return;
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let res = this.extract_and_post(url, by_name, me).await;
            this.extracting.store(false, Ordering::Relaxed);
            if let Err(e) = res {
                (this.toast)(format!("Música não pôde ser lida: {e}"));
            }
            this.ctx.request_repaint();
        });
    }

    async fn extract_and_post(
        self: &Arc<Music>,
        url: String,
        by_name: String,
        me: i64,
    ) -> Result<(), String> {
        let tools = ytdlp::ensure().await?;
        let extracted = extractor::extract(&url, &tools).await?;
        let id = format!("{}-{:016x}", me, rand_id());
        let item = Item {
            id: id.clone(),
            url,
            title: extracted.title.clone(),
            uploader: extracted.uploader.clone(),
            duration: extracted.duration,
            service: extracted.service.pretty().to_owned(),
            by: me,
            by_name,
        };
        // Keep what yt-dlp said around for `start_dj`, so it does not
        // have to run yt-dlp a second time for the same song.
        self.cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, extracted);
        // Add locally first (so a slow send to a peer does not block it),
        // then post to all.
        if self.queue.add(item.clone()) {
            self.sync_dj();
        }
        self.broadcast(queue::add_message(&item));
        self.ctx.request_repaint();
        Ok(())
    }

    /// Removes the song whose id matches, locally and over the network.
    pub fn skip(self: &Arc<Music>, id: &str) {
        if self.queue.skip(id) {
            self.sync_dj();
            self.broadcast(queue::skip_message(id));
            self.ctx.request_repaint();
        }
    }

    pub fn pause(self: &Arc<Music>) {
        if self.queue.set_paused(true) {
            self.sync_dj();
            self.broadcast(queue::pause_message());
            self.ctx.request_repaint();
        }
    }

    pub fn resume(self: &Arc<Music>) {
        if self.queue.set_paused(false) {
            self.sync_dj();
            self.broadcast(queue::play_message());
            self.ctx.request_repaint();
        }
    }

    /// A snapshot to a specific uid (used when a new person joins).
    fn broadcast(&self, msg: Value) {
        let Some(out) = self
            .outgoing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return;
        };
        let _ = out.send(Outgoing { to: None, msg });
    }

    /// Looks at the queue and either starts, pauses or stops the local
    /// player. A pause only flips a flag on the player task: the yt-dlp
    /// extraction and the download stay in memory, so resuming picks up
    /// where it stopped.
    fn sync_dj(self: &Arc<Music>) {
        let me = self.me.load(Ordering::Relaxed);
        if me == 0 {
            self.stop_dj();
            return;
        }
        match self.queue.action_for(me) {
            DjAction::Idle | DjAction::Stop => self.stop_dj(),
            DjAction::Pause => self.set_dj_paused(true),
            DjAction::Play(item) => {
                let current = {
                    let dj = self.dj.lock().unwrap_or_else(PoisonError::into_inner);
                    dj.as_ref().map(|s| s.item_id.clone())
                };
                // Already playing the song whose head came up: an
                // unpause does the resume. For anything else, start
                // fresh.
                if current.as_deref() == Some(&item.id) {
                    self.set_dj_paused(false);
                } else {
                    self.start_dj(item);
                }
            }
        }
    }

    fn set_dj_paused(&self, paused: bool) {
        let dj = self.dj.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(state) = &*dj {
            state.paused.store(paused, Ordering::Relaxed);
        }
    }

    fn start_dj(self: &Arc<Music>, item: Item) {
        {
            let dj = self.dj.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(state) = &*dj
                && state.item_id == item.id
            {
                return;
            }
        }
        self.stop_dj();
        let Some(screen_packets) = self
            .screen_packets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return;
        };
        let Some(cmds) = self
            .cmds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return;
        };
        let Some(audio) = self
            .audio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return;
        };
        let me = self.me.load(Ordering::Relaxed);
        // The buffer the local mixer pulls my own screen-audio from. The
        // music player pushes into it so the DJ hears the song too.
        let local = audio.screen_peer(me);

        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let stop_for_task = stop.clone();
        let paused_for_task = paused.clone();
        let this = self.clone();
        let item_id = item.id.clone();
        // Turn the screen audio track on (if it was off); the song's
        // Opus packets ride on it.
        let _ = cmds.send(crate::voice::rtc::Cmd::ScreenAudio(true));
        let handle = self.rt.spawn(async move {
            let tools = match ytdlp::ensure().await {
                Ok(t) => t,
                Err(e) => {
                    (this.toast)(format!("Música parou: {e}"));
                    this.ctx.request_repaint();
                    return;
                }
            };
            // Reuse the extraction `add_url` already paid for: no second
            // yt-dlp run for the same song.
            let cached = this
                .cached
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&item.id);
            let extracted = match cached {
                Some(e) => e,
                None => match extractor::extract(&item.url, &tools).await {
                    Ok(e) => e,
                    Err(e) => {
                        (this.toast)(format!("Música parou: {e}"));
                        this.skip(&item.id);
                        return;
                    }
                },
            };
            // The decoder fills a bounded channel (5 s of PCM); the
            // encoder paces at real time. A pause blocks the encoder
            // without losing anything: the decoder stops a few seconds
            // ahead and resumes right where it stopped.
            let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Pcm>(500);
            let encoder_stop = stop_for_task.clone();
            let encoder_paused = paused_for_task.clone();
            let this_for_encoder = this.clone();
            let id_for_encoder = item.id.clone();
            let encoder_task = tokio::task::spawn_blocking(move || {
                paced_encoder(
                    screen_packets,
                    &mut pcm_rx,
                    &encoder_stop,
                    &encoder_paused,
                    &local,
                    this_for_encoder,
                    id_for_encoder,
                );
            });
            let r = stream::play(extracted, pcm_tx, stop_for_task.clone()).await;
            if let Err(e) = r {
                crate::logln!("música: {e}");
            }
            // Wait for the encoder to drain so a quick-ending song is
            // heard whole before the queue moves on.
            let _ = encoder_task.await;
            if !stop_for_task.load(Ordering::Relaxed) {
                // Song ended of its own: advance the queue.
                this.finished(&item.id);
            }
        });
        *self.dj.lock().unwrap_or_else(PoisonError::into_inner) = Some(DjState {
            item_id,
            stop,
            paused,
            handle,
        });
    }

    fn stop_dj(&self) {
        let Some(state) = self
            .dj
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        state.stop.store(true, Ordering::Relaxed);
        state.handle.abort();
    }

    /// The current song's player ran out of audio: skip it (locally and
    /// over the network).
    fn finished(self: &Arc<Music>, id: &str) {
        self.cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        if self.queue.finished(id) {
            self.sync_dj();
            self.broadcast(queue::skip_message(id));
            self.ctx.request_repaint();
        }
    }

    /// The DJ's player just sent its first PCM chunk out: the elapsed
    /// clock starts now, locally and for everyone else.
    fn start_playing_locally(self: &Arc<Music>, id: &str) {
        if self.queue.start_playing(id) {
            self.broadcast(queue::start_message(id));
            self.ctx.request_repaint();
        }
    }
}

/// The encoder's own loop: paces the output at real time (with the lead
/// burst), gates on pause (keeps the song in memory while paused), and
/// starts the elapsed clock on the first chunk that goes out.
fn paced_encoder(
    screen_packets: UnboundedSender<crate::voice::audio::Packet>,
    pcm_rx: &mut tokio::sync::mpsc::Receiver<Pcm>,
    stop: &AtomicBool,
    paused: &AtomicBool,
    local: &Arc<crate::voice::audio::PeerAudio>,
    music: Arc<Music>,
    item_id: String,
) {
    use std::time::{Duration, Instant};
    const LEAD: Duration = Duration::from_millis(stream::LEAD_CHUNKS * stream::CHUNK_MS);
    const CHUNK: Duration = Duration::from_millis(stream::CHUNK_MS);
    let mut encoder = match crate::voice::audio::ScreenEncoder::new(screen_packets) {
        Ok(e) => e,
        Err(e) => {
            crate::logln!("música: encoder: {e}");
            return;
        }
    };
    let mut started = false;
    // `send_at` is when the next chunk should leave. The first `LEAD`
    // chunks go immediately (negative deadline), priming the mixer.
    let mut send_at = Instant::now()
        .checked_sub(LEAD)
        .unwrap_or_else(Instant::now);
    let mut was_paused = false;
    while let Some(chunk) = pcm_rx.blocking_recv() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        while paused.load(Ordering::Relaxed) {
            was_paused = true;
            std::thread::sleep(Duration::from_millis(50));
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        if was_paused {
            // Reset the pacing clock so the song does not burst out all
            // the chunks that piled up during the pause.
            send_at = Instant::now()
                .checked_sub(LEAD)
                .unwrap_or_else(Instant::now);
            was_paused = false;
        }
        let now = Instant::now();
        if send_at > now {
            std::thread::sleep(send_at - now);
        }
        send_at += CHUNK;
        if !started {
            started = true;
            music.start_playing_locally(&item_id);
        }
        local.push(&chunk.samples);
        encoder.feed(&chunk.samples, Instant::now());
    }
}

/// For the queue item ids: a 64-bit random, hex-encoded.
fn rand_id() -> u64 {
    use std::time::SystemTime;
    // Mix the system clock with the thread id, enough entropy for a per-
    // call id and nothing cryptographic.
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos() as u64;
    let tid = thread_id_hash();
    now.rotate_left(13) ^ tid
}

fn thread_id_hash() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one(std::thread::current().id())
}

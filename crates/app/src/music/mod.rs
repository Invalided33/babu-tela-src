//! Music in the call: anyone pastes a YouTube or SoundCloud link, the
//! person who added it DJs (their app pulls the audio off the web with
//! yt-dlp and sends it to everyone through the same track screen share
//! sound rides on), the queue is kept in sync over the "controle" data
//! channel. The DJ hands off automatically when they leave the call.
//!
//! No server is involved: the song itself goes peer to peer, encrypted end
//! to end along with the voice.
//!
//! Two volumes: the queue's, for everyone (the DJ turns the song down
//! before it goes out, `m-volume`), and each one's own, on this PC only
//! (the speaker turns down the DJ's sound, voice/audio.rs `music_volume`).

pub mod extractor;
pub mod queue;
pub mod stream;
pub mod ytdlp;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use std::collections::HashMap;

use serde_json::{Value, json};
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
    /// (`Cmd::ScreenAudio(true)` when music starts).
    cmds: Mutex<Option<UnboundedSender<crate::voice::rtc::Cmd>>>,

    /// The call's shared audio state. The music player pushes the decoded
    /// PCM into `screen_peer(me)` so the DJ hears the song too (there is
    /// no Windows playback in the middle, and the remote peers get it
    /// through `screen_packets`, not through this). The speaker learns
    /// from it whose sound is the music (`music_from`).
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

    /// Bumped as each call starts and ends: a song still being read when
    /// its call ended goes nowhere.
    call: AtomicU64,

    /// Links being read now (several can be, each its own yt-dlp).
    extracting: AtomicUsize,

    /// Links pasted go into the queue in the order they were pasted, even
    /// when a later one is read first: each takes a number, and waits for
    /// `order` to reach it.
    tickets: AtomicU64,
    order: tokio::sync::watch::Sender<u64>,

    /// How long the last song added here took (for the tests' report).
    timing: Mutex<Timing>,
    ctx: egui::Context,
}

/// How long the last song added on this PC took, in milliseconds from the
/// link pasted: until everyone had it in their queue, and until it played.
#[derive(Clone, Copy, Debug, Default)]
struct Timing {
    pasted: Option<Instant>,
    queued_ms: Option<f64>,
    playing_ms: Option<f64>,
    fast: Option<bool>,
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
            call: AtomicU64::new(0),
            extracting: AtomicUsize::new(0),
            tickets: AtomicU64::new(0),
            order: tokio::sync::watch::Sender::new(0),
            timing: Mutex::new(Timing::default()),
            ctx,
        })
    }

    /// How many links are being read now, so the UI can say "Buscando…".
    pub fn extracting(&self) -> usize {
        self.extracting.load(Ordering::Relaxed)
    }

    pub fn now_playing(&self) -> Option<Item> {
        self.queue.now_playing()
    }

    pub fn view(&self) -> View {
        self.queue.view()
    }

    /// The music window opened: the programs that read the links get ready
    /// meanwhile (downloaded the first time), so the first link pasted does
    /// not wait for them.
    pub fn prepare(&self) {
        ytdlp::prewarm(&self.rt);
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
        self.call.fetch_add(1, Ordering::SeqCst);
        self.me.store(me, Ordering::Relaxed);
        *self.outgoing.lock().unwrap_or_else(PoisonError::into_inner) = Some(outgoing);
        *self
            .screen_packets
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(screen_packets);
        *self.cmds.lock().unwrap_or_else(PoisonError::into_inner) = Some(cmds);
        audio.music_from.store(0, Ordering::Relaxed);
        *self.audio.lock().unwrap_or_else(PoisonError::into_inner) = Some(audio);
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    /// Called by Voice when the user leaves the call: the queue is cleared,
    /// the DJ task (if any) is cancelled, and nothing new is posted.
    pub fn deactivate(self: &Arc<Music>) {
        self.call.fetch_add(1, Ordering::SeqCst);
        self.me.store(0, Ordering::Relaxed);
        *self.outgoing.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .screen_packets
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self.cmds.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if let Some(audio) = self
            .audio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            audio.music_from.store(0, Ordering::Relaxed);
        }
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

    /// `m-state` for someone who just joined (sent to one person), when
    /// there is anything to tell: songs, or a volume turned down.
    pub fn snapshot(&self) -> Option<Value> {
        self.queue
            .worth_telling()
            .then(|| self.queue.snapshot_message())
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
    /// everyone (and adds it locally). Several links can be read at once;
    /// they go into the queue in the order they were pasted.
    pub fn add_url(self: &Arc<Music>, url: String, by_name: String) {
        let me = self.me.load(Ordering::Relaxed);
        if me == 0 {
            (self.toast)(crate::t!("music.join_to_play").into());
            return;
        }
        if extractor::classify(&url).is_none() {
            (self.toast)(crate::t!("music.only_links").into());
            return;
        }
        let call = self.call.load(Ordering::SeqCst);
        let ticket = self.tickets.fetch_add(1, Ordering::SeqCst);
        let pasted = Instant::now();
        self.extracting.fetch_add(1, Ordering::Relaxed);
        self.ctx.request_repaint();
        let this = self.clone();
        self.rt.spawn(async move {
            let read = this.read_song(url, by_name, me).await;
            // The ones pasted before go in first.
            let mut turn = this.order.subscribe();
            let _ = turn.wait_for(|next| *next >= ticket).await;
            this.extracting.fetch_sub(1, Ordering::Relaxed);
            match read {
                // The call this was pasted in is over: it goes nowhere.
                Ok(_) if this.call.load(Ordering::SeqCst) != call => {}
                Ok((item, extracted)) => {
                    {
                        let mut t = this.timing.lock().unwrap_or_else(PoisonError::into_inner);
                        *t = Timing {
                            pasted: Some(pasted),
                            queued_ms: Some(pasted.elapsed().as_secs_f64() * 1000.0),
                            playing_ms: None,
                            fast: Some(extracted.fast),
                        };
                    }
                    this.post(item, extracted);
                }
                Err(e) => (this.toast)(crate::tf!("music.read_failed", e = e)),
            }
            this.order
                .send_modify(|next| *next = (*next).max(ticket + 1));
            this.ctx.request_repaint();
        });
    }

    /// Runs yt-dlp on `url`: the song as the queue shows it, and what to
    /// play it from.
    async fn read_song(
        self: &Arc<Music>,
        url: String,
        by_name: String,
        me: i64,
    ) -> Result<(Item, extractor::Extracted), String> {
        let tools = ytdlp::ensure().await?;
        let extracted = extractor::extract(&url, &tools).await?;
        let item = Item {
            id: format!("{}-{:016x}", me, rand_id()),
            url,
            title: extracted.title.clone(),
            uploader: extracted.uploader.clone(),
            duration: extracted.duration,
            service: extracted.service.pretty().to_owned(),
            by: me,
            by_name,
        };
        Ok((item, extracted))
    }

    /// Puts a song read here in the queue, here and for everyone.
    fn post(self: &Arc<Music>, item: Item, extracted: extractor::Extracted) {
        // Keep what yt-dlp said around for `start_dj`, so it does not
        // have to run yt-dlp a second time for the same song.
        self.cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(item.id.clone(), extracted);
        // Add locally first (so a slow send to a peer does not block it),
        // then post to all.
        if self.queue.add(item.clone()) {
            self.sync_dj();
        }
        self.broadcast(queue::add_message(&item));
        self.ctx.request_repaint();
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

    /// How loud the music goes out to everyone, 0 to 1 ("para todos"). The
    /// DJ's player follows within 10 ms.
    pub fn set_volume(self: &Arc<Music>, volume: f32) {
        if self.queue.set_volume(volume) {
            self.broadcast(queue::volume_message(self.queue.volume()));
            self.ctx.request_repaint();
        }
    }

    /// The music as the tests' report shows it.
    pub fn report(&self) -> Value {
        let view = self.queue.view();
        let t = *self.timing.lock().unwrap_or_else(PoisonError::into_inner);
        json!({
            "queue": view.items.iter().map(|i| i.title.clone()).collect::<Vec<_>>(),
            "dj": view.dj,
            "playing": view.elapsed > 0.0,
            "elapsed": view.elapsed,
            "volume": view.volume,
            "extracting": self.extracting(),
            "lastQueuedMs": t.queued_ms,
            "lastPlayingMs": t.playing_ms,
            "lastFast": t.fast,
        })
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

    /// Tells the speaker whose sound is the music now: the DJ's, that of
    /// whoever added the song at the head of the queue.
    fn publish_dj(&self) {
        if let Some(audio) = self
            .audio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let dj = self.queue.now_playing().map_or(0, |item| item.by);
            audio.music_from.store(dj, Ordering::Relaxed);
        }
    }

    /// Looks at the queue and either starts, pauses or stops the local
    /// player. A pause only flips a flag on the player task: the yt-dlp
    /// extraction and the download stay in memory, so resuming picks up
    /// where it stopped.
    fn sync_dj(self: &Arc<Music>) {
        self.publish_dj();
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
                    (this.toast)(crate::tf!("music.stopped", e = e));
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
                        (this.toast)(crate::tf!("music.stopped", e = e));
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
            let mut played = stream::play(extracted, pcm_tx.clone(), stop_for_task.clone()).await;
            // Not a sound came: the fast way's audio was refused, or the
            // link waited in the queue long enough for its address to run
            // out. yt-dlp's own way, once more, from the start.
            if let Err(e) = &played
                && !stop_for_task.load(Ordering::Relaxed)
            {
                crate::logln!("música: {e}; lendo o link de novo");
                played = match extractor::extract_full(&item.url, &tools).await {
                    Ok(again) => stream::play(again, pcm_tx.clone(), stop_for_task.clone()).await,
                    Err(e) => Err(e),
                };
            }
            drop(pcm_tx);
            if let Err(e) = &played
                && !stop_for_task.load(Ordering::Relaxed)
            {
                crate::logln!("música: {e}");
                (this.toast)(crate::tf!("music.stopped", e = e));
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
        {
            let mut t = self.timing.lock().unwrap_or_else(PoisonError::into_inner);
            if t.playing_ms.is_none()
                && let Some(pasted) = t.pasted
            {
                t.playing_ms = Some(pasted.elapsed().as_secs_f64() * 1000.0);
            }
        }
        if self.queue.start_playing(id) {
            self.broadcast(queue::start_message(id));
            self.ctx.request_repaint();
        }
    }
}

/// Turns 10 ms of interleaved stereo from volume `from` to `to`, gliding
/// over it so a change makes no click.
fn turn(samples: &mut [f32], from: f32, to: f32) {
    if from == 1.0 && to == 1.0 {
        return;
    }
    let frames = (samples.len() / 2).max(1) as f32;
    for (i, frame) in samples.chunks_mut(2).enumerate() {
        let g = from + (to - from) * (i as f32 + 1.0) / frames;
        for s in frame {
            *s *= g;
        }
    }
}

/// The encoder's own loop: paces the output at real time (with the lead
/// burst), gates on pause (keeps the song in memory while paused), starts
/// the elapsed clock on the first chunk that goes out, and turns the song
/// to the volume everyone set (the queue's).
fn paced_encoder(
    screen_packets: UnboundedSender<crate::voice::audio::Packet>,
    pcm_rx: &mut tokio::sync::mpsc::Receiver<Pcm>,
    stop: &AtomicBool,
    paused: &AtomicBool,
    local: &Arc<crate::voice::audio::PeerAudio>,
    music: Arc<Music>,
    item_id: String,
) {
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
    let mut volume = music.queue.volume();
    // `send_at` is when the next chunk should leave. The first `LEAD`
    // chunks go immediately (negative deadline), priming the mixer.
    let mut send_at = Instant::now()
        .checked_sub(LEAD)
        .unwrap_or_else(Instant::now);
    let mut was_paused = false;
    while let Some(mut chunk) = pcm_rx.blocking_recv() {
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
        let wanted = music.queue.volume();
        turn(&mut chunk.samples, volume, wanted);
        volume = wanted;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::audio::{AudioShared, Packet, PeerAudio};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc::unbounded_channel;

    /// A non-silent 10 ms stereo chunk (alternating, so the Opus encoder
    /// never treats it as DTX silence and drops it).
    fn loud_chunk() -> Pcm {
        let samples = (0..stream::CHUNK * 2)
            .map(|i| if i % 2 == 0 { 0.3 } else { -0.3 })
            .collect();
        Pcm { samples }
    }

    fn a_music() -> (Arc<Music>, Arc<AudioShared>, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let music = Music::new(
            rt.handle().clone(),
            Arc::new(|_: String| {}),
            egui::Context::default(),
        );
        let audio = Arc::new(AudioShared::default());
        let (out_tx, _out_rx) = unbounded_channel();
        let (act_sp, _act_sp_rx) = unbounded_channel();
        let (cmd_tx, _cmd_rx) = unbounded_channel();
        // Leak the receivers so the senders stay open for the test.
        std::mem::forget(_out_rx);
        std::mem::forget(_act_sp_rx);
        std::mem::forget(_cmd_rx);
        music.activate(5, out_tx, act_sp, cmd_tx, audio.clone());
        (music, audio, rt)
    }

    #[test]
    fn the_dj_hears_its_own_music_and_peers_get_opus() {
        let (music, audio, _rt) = a_music();
        let local: Arc<PeerAudio> = audio.screen_peer(5);
        let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Pcm>(500);
        let (sp_tx, mut sp_rx) = unbounded_channel::<Packet>();
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));

        // 30 chunks (300 ms) then close the channel.
        let feeder = std::thread::spawn(move || {
            for _ in 0..30 {
                if pcm_tx.blocking_send(loud_chunk()).is_err() {
                    break;
                }
            }
        });
        paced_encoder(
            sp_tx,
            &mut pcm_rx,
            &stop,
            &paused,
            &local,
            music,
            "song".into(),
        );
        feeder.join().unwrap();

        // The DJ's own mixer buffer got the music (this is what was
        // silent before: the PCM only went to the encoder, never here).
        assert!(local.queued() > 0, "DJ heard nothing");
        // The peers' track got Opus packets.
        assert!(sp_rx.try_recv().is_ok(), "no Opus for the peers");
    }

    #[test]
    fn a_pause_holds_the_music_without_losing_it() {
        let (music, audio, _rt) = a_music();
        let local = audio.screen_peer(9);
        let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Pcm>(500);
        let (sp_tx, mut sp_rx) = unbounded_channel::<Packet>();
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(true)); // start paused
        let stop_t = stop.clone();
        let paused_t = paused.clone();

        let feeder = std::thread::spawn(move || {
            for _ in 0..30 {
                if pcm_tx.blocking_send(loud_chunk()).is_err() {
                    break;
                }
            }
            // Keep the channel open so the encoder parks on the pause,
            // not on an empty closed channel.
            std::thread::sleep(std::time::Duration::from_millis(400));
        });
        let local_t = local.clone();
        let player = std::thread::spawn(move || {
            paced_encoder(
                sp_tx,
                &mut pcm_rx,
                &stop_t,
                &paused_t,
                &local_t,
                music,
                "s".into(),
            );
        });

        // While paused, nothing should have been emitted.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(sp_rx.try_recv().is_err(), "played while paused");
        assert_eq!(local.queued(), 0, "buffered while paused");

        // Resume: now it plays.
        paused.store(false, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(sp_rx.try_recv().is_ok(), "did not resume");

        stop.store(true, Ordering::Relaxed);
        let _ = player.join();
        let _ = feeder.join();
    }

    #[test]
    fn a_volume_change_glides_over_the_chunk() {
        let mut same = vec![0.5f32; 8];
        turn(&mut same, 1.0, 1.0);
        assert!(same.iter().all(|&s| s == 0.5), "left as it comes");
        let mut half = vec![0.5f32; 8];
        turn(&mut half, 0.5, 0.5);
        assert!(half.iter().all(|&s| (s - 0.25).abs() < 1e-6));
        // Down from 1 to 0 over 4 stereo frames: both sides of a frame
        // alike, each frame quieter, the last at 0.
        let mut down = vec![1.0f32; 8];
        turn(&mut down, 1.0, 0.0);
        assert_eq!(down, [0.75, 0.75, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0]);
    }

    #[test]
    fn the_dj_sends_the_music_as_loud_as_everyone_set_it() {
        let (music, audio, _rt) = a_music();
        music.set_volume(0.5);
        let local = audio.screen_peer(5);
        let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Pcm>(500);
        let (sp_tx, _sp_rx) = unbounded_channel::<Packet>();
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let feeder = std::thread::spawn(move || {
            for _ in 0..12 {
                if pcm_tx.blocking_send(loud_chunk()).is_err() {
                    break;
                }
            }
        });
        paced_encoder(
            sp_tx,
            &mut pcm_rx,
            &stop,
            &paused,
            &local,
            music.clone(),
            "song".into(),
        );
        feeder.join().unwrap();
        // What the DJ hears is what went out: 0.3 at half.
        let heard = local.queued_samples();
        assert!(!heard.is_empty());
        assert!(
            heard.iter().all(|s| (s.abs() - 0.15).abs() < 1e-6),
            "{:?}",
            &heard[..4]
        );
        // Everyone learns it, and the speaker knows whose sound is the
        // song.
        assert_eq!(music.view().volume, 0.5);
        music.post(
            Item {
                id: "5-a".into(),
                url: "https://youtu.be/a".into(),
                title: "a".into(),
                uploader: None,
                duration: None,
                service: "YouTube".into(),
                by: 7,
                by_name: "x".into(),
            },
            extractor::Extracted {
                service: extractor::Service::YouTube,
                title: "a".into(),
                uploader: None,
                duration: None,
                stream_url: String::new(),
                ext: String::new(),
                hls: false,
                available_at: None,
                fast: true,
            },
        );
        assert_eq!(audio.music_from.load(Ordering::Relaxed), 7);
        music.deactivate();
        assert_eq!(audio.music_from.load(Ordering::Relaxed), 0);
        assert_eq!(music.view().volume, 1.0, "the next call starts as it comes");
    }

    #[test]
    fn links_go_in_in_the_order_they_were_pasted() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let order = tokio::sync::watch::Sender::new(0u64);
        let done = Arc::new(Mutex::new(Vec::new()));
        // The second one is read first; the first one still goes first.
        let mut tasks = Vec::new();
        for (ticket, ms) in [(0u64, 120u64), (1, 10), (2, 60)] {
            let (order, done) = (order.clone(), done.clone());
            tasks.push(rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                let mut turn = order.subscribe();
                let _ = turn.wait_for(|next| *next >= ticket).await;
                done.lock().unwrap().push(ticket);
                order.send_modify(|next| *next = (*next).max(ticket + 1));
            }));
        }
        rt.block_on(async {
            for t in tasks {
                t.await.unwrap();
            }
        });
        assert_eq!(*done.lock().unwrap(), [0, 1, 2]);
    }
}

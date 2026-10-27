//! Voice, camera and screen sharing over WebRTC, natively and peer to peer:
//! each person in a call has a connection of their own to each of the
//! others (a mesh, with the gateway relaying the signalling), and no server
//! ever touches the media. This is voice.js in Rust: the same rules for
//! joining, leaving, muting and the cues, on top of audio.rs (devices, echo
//! cancellation, Opus), denoise.rs (the noise suppression's neural
//! network), rtc.rs (the connections), video.rs (H.264 and AV1, with
//! hw.rs for a screen share on the graphics card), loopback.rs (the PC's
//! sound of a screen share) and plan.rs (what keeps a big call from
//! overloading anyone).

pub mod assemble;
pub mod audio;
pub mod av1;
pub mod denoise;
pub mod dxgi;
pub mod hw;
pub mod interop;
pub mod loopback;
pub mod plan;
pub mod relay;
pub mod rtc;
pub mod sources;
pub(crate) mod udp;
pub mod video;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use self::audio::{AudioShared, Gains};
pub use self::denoise::Noise;
use self::plan::Layer;
use self::sources::Source;
use self::video::{LayerControl, VideoShared};
use crate::gateway::{self, op};
use crate::ink::{self, Ink};
use crate::model::{IceConfig, Video, VoiceUser};
use crate::net::{Api, Req};
use crate::slides::Slides;
use crate::sounds::{Cue, Sounds};

/// The chosen quality: the big copy's size and rate, and its best bitrate
/// (kb/s) for a screen and for a camera. Each person who watches it
/// enlarged gets a copy of their own, so the call's budget (plan.rs) may
/// hold it lower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    pub key: &'static str,
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub screen: u32,
    pub camera: u32,
}

pub const QUALITIES: [Quality; 4] = [
    Quality {
        key: "720p30",
        label: "720p · 30 fps",
        width: 1280,
        height: 720,
        fps: 30,
        screen: 2500,
        camera: 1500,
    },
    Quality {
        key: "1080p30",
        label: "1080p · 30 fps",
        width: 1920,
        height: 1080,
        fps: 30,
        screen: 4500,
        camera: 2500,
    },
    Quality {
        key: "1080p60",
        label: "1080p · 60 fps",
        width: 1920,
        height: 1080,
        fps: 60,
        screen: 8000,
        camera: 4000,
    },
    Quality {
        key: "source",
        label: "Original · 60 fps",
        width: 3840,
        height: 2160,
        fps: 60,
        screen: 12000,
        camera: 4000,
    },
];

impl Quality {
    pub fn by_key(key: &str) -> Quality {
        QUALITIES
            .iter()
            .copied()
            .find(|q| q.key == key)
            .unwrap_or(QUALITIES[1])
    }
}

use audio::SPEAKING;

/// The speaking ring stays on this long after the last loud moment.
const SPEAKING_HOLD: Duration = Duration::from_millis(300);

/// The call view says every frame which videos it shows. When it has not
/// said for this long (another channel is open, the window is hidden),
/// nothing is shown, and nobody needs to send me video.
const WANTS_STALE: Duration = Duration::from_secs(1);

/// A video shown smaller (or not at all) only stops coming in after this
/// long, so a tile scrolled past or a window being resized does not keep
/// switching copies.
const WANTS_SETTLE: Duration = Duration::from_secs(1);

/// A call in progress.
struct Call {
    cmds: UnboundedSender<rtc::Cmd>,
    pictures: UnboundedSender<video::EncodedFrame>,
    sending: Option<video::Sending>,
    /// Where the microphone's packets go, kept to reopen the devices.
    sound: UnboundedSender<audio::Packet>,
    /// Where the packets of the PC's sound go while a screen is shared.
    screen_sound: UnboundedSender<audio::Packet>,
    /// Taking that sound, while it goes with the share.
    loopback: Option<loopback::Loopback>,
    /// The open devices; dropping them stops both.
    io: Arc<std::sync::Mutex<Option<audio::AudioIo>>>,
    /// The call's task: from opening the devices to every connection, and
    /// the relay, closed.
    task: tokio::task::JoinHandle<()>,
}

pub struct Voice {
    rt: tokio::runtime::Handle,
    api: Arc<Api>,
    gw: gateway::Sender,
    /// The signalling, sealed end to end (e2e/signals.rs).
    signals: Arc<crate::e2e::signals::Signals>,
    sounds: Arc<Sounds>,
    toast: Arc<dyn Fn(String) + Send + Sync>,
    shared: Arc<AudioShared>,
    video_shared: Arc<VideoShared>,
    layers: Arc<LayerControl>,
    /// Bumped by every join and leave, so a join still under way when the
    /// person hangs up does nothing when it finishes.
    generation: Arc<AtomicU64>,
    call: Option<Call>,
    /// The task of the last call left, which ends once it has hung up.
    hanging_up: Option<tokio::task::JoinHandle<()>>,
    channel: Option<i64>,
    me: i64,
    mute: bool,
    deaf: bool,
    /// What I am showing: the camera, a screen or a window.
    source: Option<Source>,
    /// The PC's sound goes with a screen or window I share.
    share_audio: bool,
    /// Whose screen's sound I silenced here.
    screen_muted: HashSet<i64>,
    /// Whose screen share I stopped watching ("Parar de assistir"): neither
    /// its picture nor its sound comes, until I watch it again or the share
    /// ends.
    unwatched: HashSet<i64>,
    /// Whose screen the call view shows enlarged: its sound is the only
    /// one that comes while it is.
    screen_focus: Option<i64>,
    present: Option<HashSet<i64>>,
    streaming: HashSet<i64>,
    quality: Quality,
    speaking: HashMap<i64, Instant>,
    /// When each person last spoke, for choosing whose video to show.
    spoke: HashMap<i64, Instant>,
    /// Which copy of each person's video I receive, as last told to them.
    wants: HashMap<i64, Layer>,
    /// When the call view last said what it shows.
    wants_at: Option<Instant>,
    /// Since when a smaller copy has been asked for and not yet applied.
    settling: Option<Instant>,
    /// How the call is going, kept by rtc.rs.
    report: Arc<std::sync::Mutex<rtc::Report>>,
    /// The presentations: mine, and everyone else's in the call.
    slides: Arc<Slides>,
    /// Who presented at the last frame, for the cues.
    presenters: HashSet<i64>,
    /// The annotations over what is shared, mine and everyone's (ink/).
    ink: Arc<Ink>,
    /// The share starts again in another quality: it is the same thing
    /// shared, and its annotations stay.
    restarting: bool,
    /// Everyone's strokes over my own shared screen (ink/overlay.rs). It
    /// only opens when `draw_wanted` is true: the bar jumping onto the
    /// screen the moment someone starts sharing was in the way.
    overlay: Option<ink::overlay::Overlay>,
    /// Whether the user asked for the drawing bar over their own shared
    /// screen (the "Desenhar" button next to "Compartilhar tela"). Off by
    /// default: viewers can still annotate what I share, through the
    /// "Anotar" button on their copy.
    draw_wanted: bool,
    /// Music in the call: shared with Core and the UI.
    music: Arc<crate::music::Music>,
    ctx: egui::Context,
}

impl Voice {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rt: tokio::runtime::Handle,
        api: Arc<Api>,
        gw: gateway::Sender,
        signals: Arc<crate::e2e::signals::Signals>,
        sounds: Arc<Sounds>,
        music: Arc<crate::music::Music>,
        ctx: egui::Context,
        toast: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Voice {
        Voice {
            rt,
            api,
            gw,
            signals,
            sounds,
            toast,
            shared: {
                let shared = Arc::new(AudioShared::default());
                shared.set_wake(ctx.clone());
                shared
            },
            video_shared: VideoShared::new(ctx.clone()),
            layers: Arc::new(LayerControl::default()),
            generation: Arc::new(AtomicU64::new(0)),
            call: None,
            hanging_up: None,
            channel: None,
            me: 0,
            mute: false,
            deaf: false,
            source: None,
            share_audio: true,
            screen_muted: HashSet::new(),
            unwatched: HashSet::new(),
            screen_focus: None,
            present: None,
            streaming: HashSet::new(),
            quality: QUALITIES[1],
            speaking: HashMap::new(),
            spoke: HashMap::new(),
            wants: HashMap::new(),
            wants_at: None,
            settling: None,
            report: Arc::default(),
            slides: Slides::new(ctx.clone()),
            presenters: HashSet::new(),
            ink: Ink::new(ctx.clone()),
            restarting: false,
            overlay: None,
            draw_wanted: false,
            music,
            ctx,
        }
    }

    /// The annotations over what is shared in the call.
    pub fn ink(&self) -> &Arc<Ink> {
        &self.ink
    }

    /// For the tests: the annotations, and the window over my screen.
    pub fn ink_report(&self) -> Value {
        let mut v = self.ink.report();
        v["overlay"] = self
            .overlay
            .as_ref()
            .map_or(Value::Null, ink::overlay::Overlay::report);
        v
    }

    /// How the call is going: each connection, and the video budget.
    pub fn report(&self) -> rtc::Report {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn channel_id(&self) -> Option<i64> {
        self.channel
    }

    /// The last call left, still hanging up: its task ends once every
    /// connection and the relay have closed. None once it was taken.
    pub fn hanging_up(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.hanging_up.take().filter(|t| !t.is_finished())
    }

    /// "Cannot hear me": muted, or deafened (which mutes too).
    pub fn mute(&self) -> bool {
        self.mute || self.deaf
    }

    pub fn deaf(&self) -> bool {
        self.deaf
    }

    pub fn video(&self) -> Option<Video> {
        self.source.map(Source::kind)
    }

    /// The screen or window being shared, or the camera.
    pub fn source(&self) -> Option<Source> {
        self.source
    }

    /// Audio packets sent and received in calls so far.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.shared.sent.load(Ordering::Relaxed),
            self.shared.received.load(Ordering::Relaxed),
        )
    }

    pub fn speaking(&self, uid: i64) -> bool {
        self.speaking
            .get(&uid)
            .is_some_and(|until| *until > Instant::now())
    }

    /// When `uid` last spoke in this call.
    pub fn last_spoke(&self, uid: i64) -> Option<Instant> {
        self.spoke.get(&uid).copied()
    }

    /// The newest picture from `uid` (or mine, for my own preview).
    pub fn frame(&self, uid: i64) -> Option<egui::load::SizedTexture> {
        // My tile drawn again after a while: the share on the graphics
        // card, which made no tile while nobody drew it, is woken to.
        let back = uid == self.me && !self.video_shared.wanted_recently(uid, video::TILE_GRACE);
        let frame = self.video_shared.frame(uid);
        if back {
            self.layers.ring();
        }
        frame
    }

    /// The call view drew `uid`'s newest picture (for the measurements).
    pub fn frame_shown(&self, uid: i64) {
        self.video_shared.shown(uid);
    }

    /// Which copy of `uid`'s video I receive.
    pub fn showing(&self, uid: i64) -> Layer {
        self.wants.get(&uid).copied().unwrap_or_default()
    }

    /// The call view, every frame: which copy of whose video it shows.
    /// Bigger copies are asked for at once; smaller ones (or none) once the
    /// view has settled on them.
    pub fn want_videos(&mut self, wanted: HashMap<i64, Layer>) {
        let now = Instant::now();
        self.wants_at = Some(now);
        if wanted == self.wants {
            self.settling = None;
            return;
        }
        let shrinks = self
            .wants
            .iter()
            .any(|(uid, had)| wanted.get(uid).copied().unwrap_or_default() < *had);
        let next = if shrinks {
            let since = *self.settling.get_or_insert(now);
            if now.saturating_duration_since(since) >= WANTS_SETTLE {
                self.settling = None;
                wanted
            } else {
                // Grow what grows now; keep what shrinks for a moment.
                let mut merged = self.wants.clone();
                for (uid, layer) in wanted {
                    let had = merged.entry(uid).or_default();
                    *had = (*had).max(layer);
                }
                merged
            }
        } else {
            self.settling = None;
            wanted
        };
        if next != self.wants {
            self.wants = next;
            self.send_wants();
        }
    }

    fn send_wants(&self) {
        if let Some(call) = &self.call {
            let wants = self
                .wants
                .iter()
                .filter(|(_, l)| **l != Layer::Off)
                .map(|(u, l)| (*u, *l))
                .collect();
            let _ = call.cmds.send(rtc::Cmd::Wants(wants));
        }
    }

    pub fn set_quality(&mut self, q: Quality) {
        self.quality = q;
        // A new encoder at the new size and bitrate (the same screen: its
        // annotations stay).
        if let Some(source) = self.source {
            self.restarting = true;
            self.share(None);
            self.share(Some(source));
            self.restarting = false;
        }
    }

    fn apply_audio(&self) {
        self.shared
            .mute
            .store(self.mute || self.deaf, Ordering::Relaxed);
        self.shared.deaf.store(self.deaf, Ordering::Relaxed);
    }

    fn push(&self) {
        self.gw
            .send(&op::voice_state(self.mute, self.deaf, self.video()));
    }

    /// After the gateway reconnected (Core calls this on READY): if we were
    /// in a call, re-register the voice on the fresh connection. The server
    /// keeps my place for a few seconds after the old one closes
    /// (VOICE_HOLD on the Worker), along with what the others signalled to
    /// me meanwhile: back within that, the voice moves over silently and
    /// nobody sees us bounce. Back later, it had told everyone I left: they
    /// hung up on me, and make new connections now that I am back, which
    /// mine follow (rtc.rs, `Cmd::Signal`).
    pub fn rejoin(&self) {
        if let Some(channel) = self.channel {
            self.gw.send(&op::voice_join(
                channel,
                self.mute(),
                self.deaf,
                self.video(),
            ));
            // After the voice_join: the offers this may send go out behind
            // it, once the server has me in the call again.
            if let Some(call) = &self.call {
                let _ = call.cmds.send(rtc::Cmd::Reconnected);
            }
        }
    }

    pub fn join(&mut self, channel: i64, me: i64) {
        if self.channel == Some(channel) {
            return;
        }
        if self.channel.is_some() {
            self.leave(false, Cue::Leave);
        }
        self.me = me;
        self.mute = false;
        self.deaf = false;
        self.source = None;
        self.apply_audio();
        // The noise suppression's network reads in the background while
        // the devices open (the first call only).
        if self.shared.noise() == Noise::Ia {
            denoise::preload();
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.channel = Some(channel);

        let (cmds, cmd_rx) = unbounded_channel();
        let (pictures, picture_rx) = unbounded_channel();
        let (sound_tx, sound_rx) = unbounded_channel();
        let (screen_sound, screen_sound_rx) = unbounded_channel();
        // Music in the call: the Opus encoder it runs writes to the same
        // screen audio sender; the control-channel messages it posts flow
        // out over music_out_rx. Giving the music module a clone of `cmds`
        // lets it ask the connections to add the screen audio track as the
        // song starts.
        let (music_out_tx, music_out_rx) = unbounded_channel();
        self.music
            .activate(me, music_out_tx, screen_sound.clone(), cmds.clone());
        let music = self.music.clone();
        // Whose screens' sound this side does not take, from the start.
        self.unwatched.clear();
        let _ = cmds.send(rtc::Cmd::ScreenMuted(self.unheard_screens()));
        let io = Arc::new(std::sync::Mutex::new(None));
        // The call keeps these too, to reopen the devices.
        let (sound, devices) = (sound_tx.clone(), io.clone());
        self.shared.broken.store(false, Ordering::SeqCst);
        let shared = self.shared.clone();
        let api = self.api.clone();
        let gw = self.gw.clone();
        let signals = self.signals.clone();
        let current = self.generation.clone();
        let toast = self.toast.clone();
        let video = self.video_shared.clone();
        let layers = self.layers.clone();
        let report = self.report.clone();
        let slides = self.slides.clone();
        self.ink.set_me(me);
        let ink = self.ink.clone();
        let ctx = self.ctx.clone();
        let task = self.rt.spawn(async move {
            let opened = {
                let shared = shared.clone();
                tokio::task::spawn_blocking(move || audio::start(shared, sound_tx)).await
            };
            let Ok(opened) = opened else { return };
            if current.load(Ordering::SeqCst) != generation {
                return;
            }
            *io.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(opened);
            let has_mic = shared.has_mic.load(Ordering::Relaxed);
            if !has_mic {
                shared.mute.store(true, Ordering::Relaxed);
                toast("Sem acesso ao microfone: você entrou só para ouvir.".into());
            }
            // The server hands out the relay (TURN) with short-lived
            // credentials, which is what lets a pair of people with no
            // direct path between them still reach each other.
            let ice: Option<IceConfig> = api.call_as(&Req::get("/ice")).await.ok();
            if current.load(Ordering::SeqCst) != generation {
                return;
            }
            gw.send(&op::voice_join(channel, !has_mic, false, None));
            ctx.request_repaint();
            // The relay over TCP and TLS, for networks without UDP; it
            // closes with the call.
            let relay = relay::Relay::start(ice.as_ref()).await;
            let ctx = rtc::Ctx {
                me,
                ice,
                relay: relay.servers(),
                gw,
                signals,
                channel,
                audio: shared,
                video,
                layers,
                report,
                slides,
                ink,
                music,
            };
            rtc::run(
                ctx,
                cmd_rx,
                sound_rx,
                screen_sound_rx,
                picture_rx,
                music_out_rx,
            )
            .await;
            // What the connections sent as they closed (the release of their
            // relay allocations) still reaches the server.
            relay.close().await;
            // The devices close when the call ends.
            io.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        });
        self.call = Some(Call {
            cmds,
            pictures,
            sending: None,
            sound,
            screen_sound,
            loopback: None,
            io: devices,
            task,
        });
    }

    /// `cue` is `Disconnect` when the call ended without the user asking,
    /// `None` when it should end quietly.
    pub fn leave(&mut self, silent: bool, cue: Cue) {
        if self.channel.is_none() {
            return;
        }
        if !silent {
            self.gw.send(&op::voice_leave());
        }
        self.sounds.play(cue);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.music.deactivate();
        if let Some(call) = self.call.take() {
            let _ = call.cmds.send(rtc::Cmd::Close);
            drop(call.sending);
            // The microphone goes off at once, not when the connections
            // have finished closing.
            let io = call.io;
            self.rt.spawn_blocking(move || {
                io.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
            });
            self.hanging_up = Some(call.task);
        }
        self.present = None;
        self.streaming.clear();
        self.unwatched.clear();
        self.screen_focus = None;
        self.speaking.clear();
        self.spoke.clear();
        self.wants.clear();
        self.wants_at = None;
        self.settling = None;
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rtc::Report::default();
        self.video_shared.clear();
        self.slides.clear();
        self.overlay = None;
        self.ink.clear();
        self.presenters.clear();
        self.shared
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.channel = None;
        self.source = None;
        self.mute = false;
        self.deaf = false;
    }

    /// Called with the authoritative participant list whenever it changes.
    pub fn sync(&mut self, users: &[VoiceUser], me: i64) {
        let ids: HashSet<i64> = users.iter().map(|u| u.user_id).collect();
        if !ids.contains(&me) {
            return;
        }
        // The server can mute someone who joined without a microphone; the
        // interface follows what it says about me.
        if let Some(mine) = users.iter().find(|u| u.user_id == me)
            && !self.shared.has_mic.load(Ordering::Relaxed)
            && mine.mute
        {
            self.mute = true;
        }
        let streaming: HashSet<i64> = users
            .iter()
            .filter(|u| u.video == Some(Video::Screen))
            .map(|u| u.user_id)
            .collect();
        // One cue per update: arrivals and departures win over streams.
        let cue = match &self.present {
            None => Cue::Join,
            Some(present) if ids.iter().any(|id| !present.contains(id)) => Cue::Join,
            Some(present) if present.iter().any(|id| !ids.contains(id)) => Cue::Leave,
            Some(_) if streaming.iter().any(|id| !self.streaming.contains(id)) => Cue::StreamStart,
            Some(_)
                if self
                    .streaming
                    .iter()
                    .any(|id| ids.contains(id) && !streaming.contains(id)) =>
            {
                Cue::StreamStop
            }
            Some(_) => Cue::None,
        };
        self.sounds.play(cue);
        // Whoever left takes what they shared with them, annotations and
        // all.
        if let Some(before) = &self.present {
            for gone in before.iter().filter(|uid| !ids.contains(uid)) {
                self.ink.forget_owner(*gone);
            }
        }
        // The music queue follows the call: the DJ of a song that is no
        // longer there steps down.
        let member_list: Vec<i64> = ids.iter().copied().collect();
        self.music.set_members(&member_list);
        self.present = Some(ids.clone());
        // The screen watched enlarged is gone: every other one is heard
        // again (the call view says the same once it is drawn).
        if self
            .screen_focus
            .is_some_and(|uid| !streaming.contains(&uid))
        {
            self.set_screen_focus(None);
        }
        // A share I stopped watching ended: the next one is watched again.
        let unwatched = self.unwatched.len();
        self.unwatched.retain(|uid| streaming.contains(uid));
        if self.unwatched.len() != unwatched
            && let Some(call) = &self.call
        {
            let _ = call
                .cmds
                .send(rtc::Cmd::ScreenMuted(self.unheard_screens()));
        }
        self.streaming = streaming;
        for gone in users.iter().filter(|u| u.video.is_none()) {
            if gone.user_id != me {
                self.video_shared.forget(gone.user_id);
            }
        }
        self.spoke.retain(|uid, _| ids.contains(uid));
        if let Some(call) = &self.call {
            let members = users.iter().map(|u| u.user_id).collect();
            let _ = call.cmds.send(rtc::Cmd::Sync(members));
        }
    }

    /// A step of someone's signalling: opened first (it must be sealed for
    /// me by them, for this call), then to their connection, in order.
    pub fn on_signal(&mut self, from: i64, data: Value, _me: i64) {
        let Some(channel) = self.channel else {
            return;
        };
        if let Some(call) = &self.call {
            let cmds = call.cmds.clone();
            self.signals.open(
                from,
                crate::e2e::signals::call_context(channel),
                data,
                Box::new(move |opened| {
                    let _ = cmds.send(rtc::Cmd::Signal(from, opened));
                }),
            );
        }
    }

    pub fn toggle_mute(&mut self) -> Result<(), String> {
        if self.channel.is_none() {
            return Ok(());
        }
        if self.deaf {
            self.toggle_deaf();
            return Ok(());
        }
        if !self.shared.has_mic.load(Ordering::Relaxed) {
            return Err("Nenhum microfone disponível.".into());
        }
        self.mute = !self.mute;
        self.sounds
            .play(if self.mute { Cue::Mute } else { Cue::Unmute });
        self.apply_audio();
        self.push();
        Ok(())
    }

    pub fn toggle_deaf(&mut self) {
        if self.channel.is_none() {
            return;
        }
        self.deaf = !self.deaf;
        self.sounds.play(if self.deaf {
            Cue::Deafen
        } else {
            Cue::Undeafen
        });
        self.apply_audio();
        self.push();
    }

    /// The camera, or the main screen. The picker uses `share` to choose
    /// another screen or a window.
    pub fn set_video(&mut self, kind: Option<Video>) {
        let source = match kind {
            None => None,
            Some(Video::Camera) => Some(Source::Camera),
            Some(Video::Screen) => match Source::main_screen() {
                Some(s) => Some(s),
                None => {
                    (self.toast)("Não achei nenhuma tela para compartilhar.".into());
                    return;
                }
            },
        };
        self.share(source);
    }

    /// Starts showing `source` (stopping whatever was shown before), or
    /// stops with `None`.
    pub fn share(&mut self, source: Option<Source>) {
        if self.channel.is_none() {
            return;
        }
        // A screen takes the place of a presentation (and the other way
        // round, see `present`).
        if matches!(source, Some(Source::Monitor(_) | Source::Window(_))) {
            self.slides.stop_mine();
        }
        let Some(call) = &mut self.call else { return };
        if let Some(sending) = call.sending.take() {
            drop(sending);
            let _ = call.cmds.send(rtc::Cmd::Video(None));
            self.video_shared.forget(self.me);
        }
        if call.loopback.take().is_some() {
            let _ = call.cmds.send(rtc::Cmd::ScreenAudio(false));
        }
        let mut source = source;
        if let Some(s) = source {
            let copies = plan::copies(s.kind(), self.quality);
            match video::start(
                s,
                copies,
                call.pictures.clone(),
                self.layers.clone(),
                self.video_shared.clone(),
                self.me,
            ) {
                Ok(sending) => {
                    call.sending = Some(sending);
                    let _ = call.cmds.send(rtc::Cmd::Video(Some(copies)));
                }
                Err(e) => {
                    crate::logln!("vídeo: {e}");
                    source = None;
                    (self.toast)(
                        if matches!(s, Source::Window(_)) {
                            "Não foi possível compartilhar essa janela."
                        } else {
                            "Não foi possível iniciar o vídeo."
                        }
                        .into(),
                    );
                }
            }
        }
        self.source = source;
        if let Some(s) = source {
            self.start_sound(s);
        }
        // A screen or a window can be annotated, a new one each time (and
        // I see everyone's strokes over it); the camera cannot. The bar
        // over the shared screen only opens when the user asks for it with
        // the "Desenhar" button; otherwise the strokes still get drawn by
        // whoever watches, over their copy.
        if !self.restarting {
            // The last one's window goes first: it stops watching the
            // strokes as it goes.
            self.overlay = None;
            match source {
                Some(s @ (Source::Monitor(_) | Source::Window(_))) => {
                    let surface = self.ink.open_mine(ink::Kind::Screen, rtc::random_u32());
                    if self.draw_wanted {
                        let page = ink::Page { surface, n: 0 };
                        self.overlay = ink::overlay::Overlay::start(self.ink.clone(), s, page);
                    }
                }
                _ => self.ink.close_mine(ink::Kind::Screen),
            }
        }
        self.push();
    }

    /// Whether the "Desenhar" button was turned on (so the bar is on, or
    /// would be on if a screen were being shared).
    pub fn drawing(&self) -> bool {
        self.draw_wanted
    }

    /// Toggles the drawing bar on and off: the viewers can always
    /// annotate what I share from their own side; this is only about the
    /// native bar that goes over my own screen.
    pub fn toggle_draw(&mut self) {
        self.draw_wanted = !self.draw_wanted;
        let Some(source) = self.source else {
            // Nothing to do until something is shared; next `share` will
            // honour the new state.
            return;
        };
        if !matches!(source, Source::Monitor(_) | Source::Window(_)) {
            return;
        }
        if self.draw_wanted {
            if self.overlay.is_some() {
                return;
            }
            // Reuses the surface opened when the share started (`share`
            // called `open_mine` for it): a new id here would wipe out
            // what has been drawn so far.
            let Some(surface) = self.ink.mine(ink::Kind::Screen) else {
                return;
            };
            let page = ink::Page { surface, n: 0 };
            self.overlay = ink::overlay::Overlay::start(self.ink.clone(), source, page);
        } else {
            self.overlay = None;
        }
        self.push();
    }

    /// Presents the file at `path` (a .pptx or a PDF) in the call: its
    /// slides are drawn here, on a thread of their own, and go to everyone
    /// as pictures (slides/). A screen being shared stops: the presentation
    /// takes its place.
    pub fn present(&mut self, path: std::path::PathBuf) {
        if self.call.is_none() {
            return;
        }
        if self.video() == Some(Video::Screen) {
            self.share(None);
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let id = loop {
            let id = rtc::random_u32();
            if id != 0 && self.slides.mine_id() != Some(id) {
                break id;
            }
        };
        self.slides.start_mine(id, name);
        crate::slides::render::start(path, id, self.slides.clone(), self.toast.clone());
    }

    pub fn stop_presenting(&mut self) {
        self.slides.stop_mine();
    }

    pub fn presenting(&self) -> bool {
        self.slides.mine_id().is_some()
    }

    /// The presenter goes to slide `slide` (from 0).
    pub fn go_to_slide(&mut self, slide: usize) {
        self.slides.go_to(slide);
    }

    /// The presentations: mine and everyone else's.
    pub fn slides(&self) -> &Arc<Slides> {
        &self.slides
    }

    /// The PC's sound along with sharing `source`, when that is on and
    /// there is a sound for it (a screen or another program's window).
    fn start_sound(&mut self, source: Source) {
        let Some(call) = &mut self.call else { return };
        if call.loopback.is_some() || !self.share_audio {
            return;
        }
        let Some(target) = loopback::Target::of(source) else {
            return;
        };
        let mut encoder = match audio::ScreenEncoder::new(call.screen_sound.clone()) {
            Ok(e) => e,
            Err(e) => {
                crate::logln!("som da tela: {e}");
                return;
            }
        };
        match loopback::start(target, move |pcm| encoder.feed(pcm, Instant::now())) {
            Ok(capture) => {
                call.loopback = Some(capture);
                let _ = call.cmds.send(rtc::Cmd::ScreenAudio(true));
            }
            Err(e) => crate::logln!("som da tela: {e}"),
        }
    }

    /// Whether the PC's sound goes with a screen or window shared (from the
    /// share picker; it stays chosen).
    pub fn share_audio(&self) -> bool {
        self.share_audio
    }

    /// Another microphone or output was chosen: a call under way opens
    /// them again (with the same connections, as when a headset is
    /// plugged in).
    pub fn devices_changed(&self) {
        if self.call.is_some() {
            self.shared.broken.store(true, Ordering::SeqCst);
            self.ctx.request_repaint();
        }
    }

    /// The noise suppression the microphone goes through, in a call under
    /// way too (within 10 ms).
    pub fn set_noise(&mut self, noise: Noise) {
        self.shared.set_noise(noise);
        if noise == Noise::Ia && self.call.is_some() {
            denoise::preload();
        }
    }

    pub fn noise(&self) -> Noise {
        self.shared.noise()
    }

    /// For the tests: the suppression chosen, the one at work now (null
    /// with no microphone open), and the network's time per 10 ms.
    pub fn noise_report(&self) -> Value {
        serde_json::json!({
            "chosen": self.shared.noise().key(),
            "now": self.shared.noise_now().map(Noise::key),
            "msPer10ms": self.shared.noise_ms(),
            "modelReady": denoise::ready().is_some(),
        })
    }

    /// The microphone and output the call opened last, by name.
    pub fn opened_devices(&self) -> [String; 2] {
        self.shared
            .opened
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether it is going right now.
    pub fn sharing_audio(&self) -> bool {
        self.call.as_ref().is_some_and(|c| c.loopback.is_some())
    }

    pub fn set_share_audio(&mut self, on: bool) {
        self.share_audio = on;
        let Some(source) = self.source else { return };
        if on {
            self.start_sound(source);
        } else if let Some(call) = &mut self.call
            && call.loopback.take().is_some()
        {
            let _ = call.cmds.send(rtc::Cmd::ScreenAudio(false));
        }
    }

    /// How loud `uid`'s voice and screen's sound play here (in this and
    /// every later call). A screen silenced here stops coming at all.
    pub fn set_gains(&mut self, uid: i64, gains: Gains) {
        self.shared.set_gains(uid, gains);
        let muted = gains.screen == 0.0;
        let changed = if muted {
            self.screen_muted.insert(uid)
        } else {
            self.screen_muted.remove(&uid)
        };
        if changed && let Some(call) = &self.call {
            let _ = call
                .cmds
                .send(rtc::Cmd::ScreenMuted(self.unheard_screens()));
        }
    }

    pub fn gains(&self, uid: i64) -> Gains {
        self.shared.gains(uid)
    }

    /// Whose screens' sound does not come here: those silenced, and those
    /// not watched.
    fn unheard_screens(&self) -> HashSet<i64> {
        self.screen_muted.union(&self.unwatched).copied().collect()
    }

    /// Whether I watch `uid`'s screen share (the default, for every share).
    pub fn watching(&self, uid: i64) -> bool {
        !self.unwatched.contains(&uid)
    }

    /// "Parar de assistir" (`on` false) or "Assistir transmissão" (true)
    /// `uid`'s screen share, here only. Not watched, its sound stops coming
    /// at once, and its picture once the call view stops asking for it
    /// (stage.rs leaves it out); a share that starts later is watched.
    pub fn set_watching(&mut self, uid: i64, on: bool) {
        let changed = if on {
            self.unwatched.remove(&uid)
        } else {
            self.streaming.contains(&uid) && self.unwatched.insert(uid)
        };
        if !changed {
            return;
        }
        if !on && self.screen_focus == Some(uid) {
            self.set_screen_focus(None);
        }
        if let Some(call) = &self.call {
            let _ = call
                .cmds
                .send(rtc::Cmd::ScreenMuted(self.unheard_screens()));
        }
    }

    /// Whose screen shares I stopped watching, for the tests' report.
    pub fn unwatched(&self) -> Vec<i64> {
        let mut out: Vec<i64> = self.unwatched.iter().copied().collect();
        out.sort_unstable();
        out
    }

    /// The call view, every frame: whose screen it shows enlarged (None:
    /// none, or not a screen). Of several screens shared, the sound of
    /// the one watched enlarged is the one heard; otherwise all of them
    /// are (but those silenced here).
    pub fn set_screen_focus(&mut self, uid: Option<i64>) {
        if uid == self.screen_focus {
            return;
        }
        self.screen_focus = uid;
        if let Some(call) = &self.call {
            let _ = call.cmds.send(rtc::Cmd::ScreenFocus(uid));
        }
    }

    /// Called every frame: updates who is speaking, stops asking for video
    /// the call view no longer shows, and keeps the call view refreshing
    /// while there is a call.
    pub fn tick(&mut self) {
        if self.channel.is_none() {
            return;
        }
        // Someone began or stopped presenting: the cues of a screen share.
        // (A presenter who left the call had the cue of leaving.)
        let presenters = self.slides.presenters();
        if presenters != self.presenters {
            let began = presenters.iter().any(|uid| !self.presenters.contains(uid));
            let stopped = self.presenters.iter().any(|uid| {
                !presenters.contains(uid) && self.present.as_ref().is_some_and(|p| p.contains(uid))
            });
            if began {
                self.sounds.play(Cue::StreamStart);
            } else if stopped {
                self.sounds.play(Cue::StreamStop);
            }
            self.presenters = presenters;
        }
        // My presentation can be annotated for as long as I present it
        // (which ends in more ways than one: a file that did not open, a
        // screen shared in its place).
        let deck = self.slides.mine_id();
        if self.ink.mine(ink::Kind::Slides).map(|s| s.id) != deck {
            match deck {
                Some(id) => {
                    self.ink.open_mine(ink::Kind::Slides, id);
                }
                None => self.ink.close_mine(ink::Kind::Slides),
            }
        }
        // The shared window was closed: stop showing it, and say why.
        let ended = self
            .call
            .as_ref()
            .and_then(|c| c.sending.as_ref())
            .is_some_and(video::Sending::ended);
        if ended {
            crate::logln!("vídeo: a janela compartilhada foi fechada");
            self.share(None);
            (self.toast)("A janela que você compartilhava foi fechada.".into());
        }
        // The graphics card stopped encoding in the middle of the share:
        // it carries on through the processor, with the same tracks.
        let healed = self
            .call
            .as_mut()
            .and_then(|c| c.sending.as_mut())
            .and_then(video::Sending::heal);
        match healed {
            Some(Ok(())) => crate::logln!("vídeo: o compartilhamento seguiu pelo processador"),
            Some(Err(e)) => {
                crate::logln!("vídeo: nem pelo processador ({e})");
                self.share(None);
                (self.toast)("O compartilhamento parou: a placa de vídeo falhou.".into());
            }
            None => {}
        }
        // Windows refused the PC's sound (or took the device away): the
        // picture goes on without it, and the person is told once.
        if let Some(call) = &mut self.call
            && call
                .loopback
                .as_ref()
                .is_some_and(|l| l.failure().is_some())
        {
            call.loopback = None;
            let _ = call.cmds.send(rtc::Cmd::ScreenAudio(false));
            (self.toast)("Não deu para compartilhar o som do PC: a tela continua sem ele.".into());
        }
        if self.shared.noise_overloaded.swap(false, Ordering::Relaxed) {
            (self.toast)(
                "Este PC não deu conta da supressão de ruído por IA a tempo: a chamada seguiu com a padrão."
                    .into(),
            );
        }
        // A headset plugged in or out mid-call: open whatever Windows uses
        // now, and carry on with the same connections.
        if self.shared.broken.swap(false, Ordering::SeqCst)
            && let Some(call) = &self.call
        {
            let (io, sound, shared) = (call.io.clone(), call.sound.clone(), self.shared.clone());
            self.rt.spawn_blocking(move || {
                let mut slot = io.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if slot.take().is_some() {
                    *slot = Some(audio::start(shared, sound));
                }
            });
        }
        // The call view is not being drawn: no video is needed.
        let stale = self.wants_at.is_none_or(|at| at.elapsed() > WANTS_STALE);
        if stale && !self.wants.is_empty() {
            self.wants.clear();
            self.settling = None;
            self.send_wants();
            for uid in self.present.iter().flatten() {
                if *uid != self.me {
                    self.video_shared.forget(*uid);
                }
            }
        }
        let now = Instant::now();
        if !(self.mute || self.deaf) && self.shared.my_level.get() > SPEAKING {
            self.speaking.insert(self.me, now + SPEAKING_HOLD);
            self.spoke.insert(self.me, now);
        }
        let levels: Vec<(i64, f32)> = self
            .shared
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(uid, p)| (*uid, p.level.get()))
            .collect();
        for (uid, level) in levels {
            if level > SPEAKING {
                self.speaking.insert(uid, now + SPEAKING_HOLD);
                self.spoke.insert(uid, now);
            }
        }
        self.speaking.retain(|_, until| *until > now);
        // The audio threads wake the interface when someone starts or stops
        // being loud; what is left is putting out the rings whose time is
        // up (and keeping them on while the person keeps talking).
        if let Some(next) = self.speaking.values().min() {
            self.ctx
                .request_repaint_after(next.saturating_duration_since(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualities_by_key_with_a_safe_default() {
        assert_eq!(Quality::by_key("720p30").width, 1280);
        assert_eq!(Quality::by_key("source").fps, 60);
        assert_eq!(Quality::by_key("???").key, "1080p30");
    }
}

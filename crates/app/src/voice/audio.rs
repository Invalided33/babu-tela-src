//! The sound side of a call: the microphone goes through echo cancellation,
//! noise suppression and automatic gain (what the browser's getUserMedia did
//! with echoCancellation/noiseSuppression/autoGainControl), is encoded as
//! Opus and handed to the network side. What arrives from each person is
//! decoded into a small buffer of their own, and the speaker mixes them all.
//!
//! The noise suppression is the one chosen in the settings (`Noise`): a
//! neural network (denoise.rs) between the echo canceller and the gain, or
//! WebRTC's own, or none. It can change in the middle of a call.
//!
//! The encoder uses DTX: while someone is quiet it produces packets of a
//! byte or two, which are not sent at all, so a call of many people costs
//! little more than its speakers. The bitrate and the packet length follow
//! the size of the call (plan.rs).
//!
//! Everything runs at 48 kHz, the rate Opus and the echo canceller work at;
//! the devices are resampled to and from whatever they use. Voices are mono.
//! The PC's sound that comes with someone's screen share is stereo, in a
//! buffer of its own, so it can be turned down apart from their voice; the
//! speaker plays the mix in stereo.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, MaxProcessingRate,
    NoiseSuppression, NoiseSuppressionLevel, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};
use tokio::sync::mpsc::UnboundedSender;

use super::denoise::{self, Denoiser, Noise};
use super::plan::VoicePlan;

/// One Opus packet on its way out, stamped on the call's 48 kHz clock (the
/// RTP clock of the voice). The clock keeps running through silence, so
/// after a quiet stretch the next packet's time says how long it was.
pub struct Packet {
    pub data: Vec<u8>,
    pub timestamp: u32,
    /// The first packet after silence (DTX or muted): RTP's marker bit.
    pub start: bool,
}

/// The rate everything inside a call runs at.
pub const RATE: u32 = 48_000;

/// The shortest Opus packet the call sends: 20 ms.
pub const FRAME: usize = 960;

/// What the echo canceller takes at a time: 10 ms.
const CHUNK: usize = 480;

/// Packets this small are DTX's "still quiet": not worth sending.
const DTX_PACKET: usize = 2;

/// For tests: "fala" replaces the microphone with a voice-like signal (two
/// seconds of talking, one of silence, over and over), "silencio" with
/// silence, so a call on one PC sends the same whatever the room sounds
/// like.
const TEST_VOICE_ENV: &str = "BABUTELA_TESTE_VOZ";

/// A person starts playing once this much of them is buffered (60 ms),
/// which rides over the jitter of an ordinary connection.
const PRIME: usize = 3 * FRAME;

/// More than this buffered (200 ms) means the network caught up in a burst;
/// the oldest part is dropped so the delay does not keep growing.
const MAX_BUFFER: usize = 10 * FRAME;

/// What the echo canceller remembers of the speaker (200 ms).
const MAX_RENDER: usize = 20 * CHUNK;

/// How far behind the speaker the microphone hears it, roughly: the output
/// and input buffers of an ordinary Windows device.
const STREAM_DELAY_MS: i32 = 50;

/// A level stored in an atomic, as the bits of an f32.
#[derive(Default)]
pub struct Level(AtomicU32);

impl Level {
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// The arrivals the jitter of a person's packets is judged over.
const JITTER_WINDOW: Duration = Duration::from_secs(2);

/// A queue this much above its target (latency that crept up: a burst,
/// the two sound cards' clocks drifting apart) is cut back to the target
/// in the next quiet moment, where nobody hears the cut.
const TRIM_ABOVE: usize = 3 * CHUNK;

/// Below this level a chunk is quiet enough to cut.
const QUIET: f32 = 0.004;

/// Milliseconds on a clock of this process, for arrival times.
fn mono_ms(t: Instant) -> f64 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    t.saturating_duration_since(*START).as_secs_f64() * 1000.0
}

/// What arrives from one person, waiting for the speaker: their voice, or
/// (in stereo) the sound of their screen share.
#[derive(Default)]
pub struct PeerAudio {
    /// Samples, interleaved when there are two channels.
    buf: Mutex<VecDeque<f32>>,
    /// 2 for a screen's sound; 0 (the default) means 1, a voice.
    channels: AtomicUsize,
    playing: AtomicBool,
    pub level: Level,
    /// Packets received from them, for the tests' report.
    pub packets: AtomicU64,
    /// Measuring: until when (ms since 1970, as f64 bits) the last beep of
    /// their clock voice still sounds.
    beep_until: AtomicU64,
    /// How much to hold before playing, in samples (0 until a packet came):
    /// one packet, the jitter of their packets' arrival, and one mixing
    /// step. Far less than a fixed safe amount on a steady connection, and
    /// more when it is not.
    target: AtomicUsize,
    arrivals: Mutex<Arrivals>,
    /// Loud in the last mixing step (for waking the interface when that
    /// changes).
    loud: AtomicBool,
    /// Samples in their last packet.
    frame: AtomicUsize,
    /// The lowest the queue went since the last check, and the mixing
    /// steps since then.
    low: AtomicUsize,
    steps: AtomicUsize,
}

/// Every this many mixing steps (half a second), a queue that never came
/// near what the jitter needs gives up 10 ms.
const ACCELERATE_EVERY: usize = 50;

/// Over this many samples (2.5 ms) the sound fades across a cut.
const FADE: usize = 120;

/// Removes the first `n` samples, fading the `fade` before the cut into
/// the ones after it so there is no click. Both are whole frames (as many
/// samples as channels), so left still meets left.
fn cut(buf: &mut VecDeque<f32>, n: usize, fade: usize) {
    if buf.len() < n + fade {
        return;
    }
    for i in 0..fade {
        let w = (i as f32 + 0.5) / fade as f32;
        buf[n + i] = buf[i] * (1.0 - w) + buf[n + i] * w;
    }
    buf.drain(..n);
}

/// When a person's recent packets arrived, against when they were sent.
#[derive(Default)]
struct Arrivals {
    /// The first packet's RTP time, which the others are counted from.
    base: Option<u32>,
    /// (arrival, arrival minus send time, in ms), oldest first.
    recent: VecDeque<(Instant, f64)>,
}

impl PeerAudio {
    /// For the sound of a screen share.
    pub fn stereo() -> PeerAudio {
        let p = PeerAudio::default();
        p.channels.store(2, Ordering::Relaxed);
        p
    }

    fn ch(&self) -> usize {
        self.channels.load(Ordering::Relaxed).max(1)
    }

    /// A packet stamped `timestamp` (RTP, 48 kHz) with `samples` of sound
    /// (per channel) arrived at `now`: the jitter, and so the target,
    /// follow.
    pub fn arrived(&self, timestamp: u32, samples: usize, now: Instant) {
        let mut a = self.arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        let base = *a.base.get_or_insert(timestamp);
        let sent = f64::from(timestamp.wrapping_sub(base)) * 1000.0 / f64::from(RATE);
        a.recent.push_back((now, mono_ms(now) - sent));
        while a
            .recent
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > JITTER_WINDOW)
        {
            a.recent.pop_front();
        }
        let (lo, hi) = a
            .recent
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), (_, d)| {
                (lo.min(*d), hi.max(*d))
            });
        let jitter_ms = (hi - lo).clamp(0.0, 150.0);
        let jitter = (jitter_ms * f64::from(RATE) / 1000.0) as usize;
        let mut target = samples + jitter + CHUNK;
        // Until half a second has been seen, a packet more of margin.
        if a.recent.len() < 25 {
            target = target.max(2 * samples + CHUNK);
        }
        self.target
            .store(target.min(MAX_BUFFER / 2), Ordering::Relaxed);
        self.frame.store(samples, Ordering::Relaxed);
        if crate::probe::on() && self.ch() == 1 {
            crate::probe::record("voz.variacao", jitter_ms);
            crate::probe::record("voz.alvo", target as f64 * 1000.0 / f64::from(RATE));
        }
    }

    /// How much to hold before playing, per channel.
    fn target(&self) -> usize {
        match self.target.load(Ordering::Relaxed) {
            0 => PRIME,
            t => t.max(CHUNK),
        }
    }

    /// Adds decoded samples (from the network side), interleaved when
    /// there are two channels.
    pub fn push(&self, pcm: &[f32]) {
        let ch = self.ch();
        let mut buf = self.buf.lock().unwrap_or_else(PoisonError::into_inner);
        buf.extend(pcm);
        if buf.len() > MAX_BUFFER * ch {
            let extra = buf.len() - self.target() * ch;
            buf.drain(..extra);
            crate::probe::count("voz.cortado", 1);
        }
    }

    /// Mixes the next `out.len()` samples into `out` (from the speaker),
    /// turned up or down by `gain`, which start playing at `at_ms` (ms
    /// since 1970). Never waits: if the network side holds the buffer right
    /// now, this person is silent for 10 ms.
    fn pull(&self, out: &mut [f32], at_ms: f64, gain: f32) {
        let Ok(mut buf) = self.buf.try_lock() else {
            return;
        };
        // In samples from here on: frames times channels.
        let ch = self.ch();
        let target = self.target() * ch;
        let step = CHUNK * ch;
        if !self.playing.load(Ordering::Relaxed) {
            if buf.len() < target {
                self.level.set(0.0);
                return;
            }
            self.playing.store(true, Ordering::Relaxed);
        }
        if buf.len() > target + TRIM_ABOVE * ch {
            // Only the quiet stretch at the front goes, never speech.
            let excess = buf.len() - target;
            let mut quiet = 0;
            while quiet + step <= excess {
                let energy: f32 = buf.range(quiet..quiet + step).map(|s| s * s).sum();
                if (energy / step as f32).sqrt() >= QUIET {
                    break;
                }
                quiet += step;
            }
            if quiet > 0 {
                buf.drain(..quiet);
                crate::probe::count("voz.aparado", 1);
            }
        }
        // Speech that never pauses (or a start with margin to spare) keeps
        // whatever delay it began with. When the queue's lowest point stayed
        // a step above what the jitter needs for half a second, 10 ms of it
        // go, faded over.
        let low = self.low.load(Ordering::Relaxed).min(buf.len());
        self.low.store(low, Ordering::Relaxed);
        if self.steps.fetch_add(1, Ordering::Relaxed) + 1 >= ACCELERATE_EVERY {
            self.steps.store(0, Ordering::Relaxed);
            self.low.store(usize::MAX, Ordering::Relaxed);
            let frame = self.frame.load(Ordering::Relaxed) * ch;
            let needed = target.saturating_sub(frame);
            if frame > 0 && low > needed + step && buf.len() >= out.len() + step + FADE * ch {
                cut(&mut buf, step, FADE * ch);
                crate::probe::count("voz.acelerado", 1);
            }
        }
        if crate::probe::on() && ch == 1 {
            crate::probe::record("voz.fila", buf.len() as f64 * 1000.0 / f64::from(RATE));
            self.listen(&buf, out.len(), at_ms);
        }
        let n = out.len().min(buf.len());
        let mut energy = 0.0;
        for (slot, s) in out.iter_mut().zip(buf.drain(..n)) {
            *slot += s * gain;
            energy += s * s;
        }
        if n < out.len() {
            // Ran dry: wait to build up again instead of stuttering.
            self.playing.store(false, Ordering::Relaxed);
            crate::probe::count("voz.secou", 1);
        }
        self.level.set(if n == 0 {
            0.0
        } else {
            (energy / n as f32).sqrt()
        });
    }

    /// Measuring: whether a beep of their clock voice starts in the next
    /// `n` samples, which play from `at_ms`. Each beep left the microphone
    /// at the top of a second, so when it plays says how late it is.
    fn listen(&self, buf: &VecDeque<f32>, n: usize, at_ms: f64) {
        let until = f64::from_bits(self.beep_until.load(Ordering::Relaxed));
        for (k, s) in buf.iter().take(n).enumerate() {
            let t = at_ms + k as f64 * 1000.0 / f64::from(RATE);
            if t < until || s.abs() < BEEP_HEARD {
                continue;
            }
            crate::probe::record("voz.boca_ouvido", t.rem_euclid(1000.0));
            self.beep_until
                .store((t + 500.0).to_bits(), Ordering::Relaxed);
            return;
        }
    }
}

/// Shared between the call's controls and the audio threads.
#[derive(Default)]
pub struct AudioShared {
    /// Send nothing.
    pub mute: AtomicBool,
    /// Play nothing.
    pub deaf: AtomicBool,
    /// Whether a microphone opened at all.
    pub has_mic: AtomicBool,
    /// Set when Windows took a device away (unplugged, switched, its format
    /// changed); the call opens the current ones again.
    pub broken: AtomicBool,
    pub my_level: Level,
    /// Each person's voice.
    pub peers: Mutex<HashMap<i64, Arc<PeerAudio>>>,
    /// The PC's sound of each person's screen share (stereo).
    pub screens: Mutex<HashMap<i64, Arc<PeerAudio>>>,
    /// How loud I play each person who is not at the defaults (`Gains`),
    /// and a count bumped on every change, so the speaker copies them
    /// only then.
    gains: Mutex<HashMap<i64, Gains>>,
    gains_changed: AtomicU64,
    /// Packets sent to and received from the others, for the smoke test
    /// and for telling a silent call from a broken one.
    pub sent: AtomicU64,
    pub received: AtomicU64,
    /// The same for the PC's sound of my screen share and theirs.
    pub screen_sent: AtomicU64,
    pub screen_received: AtomicU64,
    /// How the voice goes out for the size of the call (plan.rs); 0 until
    /// the call says, which means the defaults.
    bitrate: AtomicI32,
    frame_ms: AtomicU32,
    /// The voice's clock (48 kHz samples), kept here so it survives the
    /// devices being opened again.
    clock: AtomicU32,
    /// What the speaker played, for the echo canceller.
    render: Mutex<VecDeque<f32>>,
    /// The names of the microphone and the output opened last ("" for
    /// none), for the log and the tests.
    pub opened: Mutex<[String; 2]>,
    /// The interface, woken when something it shows changes here (someone
    /// starts speaking, a device goes away), so it need not keep checking.
    wake: OnceLock<egui::Context>,
    /// I was loud in my last frame.
    me_loud: AtomicBool,
    /// The noise suppression chosen (`Noise as u8`).
    noise: AtomicU8,
    /// The one the microphone goes through right now, as 1 + `Noise as
    /// u8` (0: no microphone open): the standard one while the network
    /// loads, or after this PC could not run it in time.
    noise_now: AtomicU8,
    /// The network ran too slow here and the call went on with the
    /// standard suppressor; the interface takes this and says so.
    pub noise_overloaded: AtomicBool,
    /// The network's average time per 10 ms, in µs (0 while it is not
    /// the one at work).
    noise_us: AtomicU32,
}

// The network takes the 10 ms the echo canceller hands over.
const _: () = assert!(CHUNK == denoise::HOP);

/// How loud counts as speaking (RMS of a 10–20 ms block).
pub const SPEAKING: f32 = 0.012;

/// How loud one person plays here, for this PC only: their voice and the
/// sound of their screen share, 1 as they come, 0 silenced, up to 2.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gains {
    pub voice: f32,
    pub screen: f32,
}

impl Default for Gains {
    fn default() -> Gains {
        Gains {
            voice: 1.0,
            screen: 1.0,
        }
    }
}

impl AudioShared {
    pub fn set_wake(&self, ctx: egui::Context) {
        let _ = self.wake.set(ctx);
    }

    fn wake(&self) {
        if let Some(ctx) = self.wake.get() {
            ctx.request_repaint();
        }
    }

    /// Wakes the interface when someone's being loud changes.
    fn loudness(&self, flag: &AtomicBool, level: f32) {
        let loud = level > SPEAKING;
        if flag.swap(loud, Ordering::Relaxed) != loud {
            self.wake();
        }
    }
    /// The noise suppression from now on (the microphone follows within
    /// 10 ms).
    pub fn set_noise(&self, noise: Noise) {
        self.noise.store(noise as u8, Ordering::Relaxed);
    }

    pub fn noise(&self) -> Noise {
        Noise::from_u8(self.noise.load(Ordering::Relaxed))
    }

    /// The suppression the microphone goes through now (None: no
    /// microphone open).
    pub fn noise_now(&self) -> Option<Noise> {
        match self.noise_now.load(Ordering::Relaxed) {
            0 => None,
            v => Some(Noise::from_u8(v - 1)),
        }
    }

    /// The network's average time per 10 ms of sound, in ms (0 while it
    /// is not the one at work).
    pub fn noise_ms(&self) -> f64 {
        f64::from(self.noise_us.load(Ordering::Relaxed)) / 1000.0
    }

    /// Opus bitrate and packet length for the call as it is now.
    pub fn set_plan(&self, plan: VoicePlan) {
        self.bitrate.store(plan.bitrate, Ordering::Relaxed);
        self.frame_ms.store(plan.frame_ms, Ordering::Relaxed);
    }

    pub fn plan(&self) -> VoicePlan {
        let default = VoicePlan::default();
        let bitrate = self.bitrate.load(Ordering::Relaxed);
        let frame_ms = self.frame_ms.load(Ordering::Relaxed);
        VoicePlan {
            bitrate: if bitrate > 0 {
                bitrate
            } else {
                default.bitrate
            },
            frame_ms: if matches!(frame_ms, 10 | 20 | 40 | 60) {
                frame_ms
            } else {
                default.frame_ms
            },
        }
    }

    pub fn peer(&self, uid: i64) -> Arc<PeerAudio> {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(uid)
            .or_default()
            .clone()
    }

    /// The buffer for the sound of `uid`'s screen share.
    pub fn screen_peer(&self, uid: i64) -> Arc<PeerAudio> {
        self.screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(uid)
            .or_insert_with(|| Arc::new(PeerAudio::stereo()))
            .clone()
    }

    /// How loud `uid` plays from now on.
    pub fn set_gains(&self, uid: i64, gains: Gains) {
        let mut all = self.gains.lock().unwrap_or_else(PoisonError::into_inner);
        if gains == Gains::default() {
            all.remove(&uid);
        } else {
            all.insert(uid, gains);
        }
        self.gains_changed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn gains(&self, uid: i64) -> Gains {
        self.gains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&uid)
            .copied()
            .unwrap_or_default()
    }

    /// What a device's error callback reports. An overrun (the first
    /// callback often has one) needs nothing; a device that went away does.
    fn device_error(&self, err: &cpal::Error) {
        use cpal::ErrorKind;
        if matches!(
            err.kind(),
            ErrorKind::StreamInvalidated | ErrorKind::DeviceNotAvailable | ErrorKind::DeviceChanged
        ) {
            self.broken.store(true, Ordering::SeqCst);
            // The call reopens the devices from the interface's next frame.
            self.wake();
        }
    }

    pub fn forget(&self, uid: i64) {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        self.screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
    }
}

/// Linear resampling, enough for speech, done in place on a stream.
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        Resampler {
            step: f64::from(from) / f64::from(to),
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        for &x in input {
            while self.pos < 1.0 {
                out.push(self.prev + (x - self.prev) * self.pos as f32);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = x;
        }
    }
}

/// Keeps peaks from clipping harshly when several people talk at once.
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.8 {
        x
    } else {
        x.signum() * (0.8 + 0.2 * ((x.abs() - 0.8) / 0.2).tanh())
    }
}

/// The open devices; dropping this stops both.
pub struct AudioIo {
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
}

impl Drop for AudioIo {
    /// The devices close on a thread of their own, as they open on one
    /// (`start`): closing a stream sets the thread it closes on up for COM
    /// in the single-threaded model (cpal), for as long as that thread
    /// lives. On one of the runtime's threads, which every task shares (the
    /// call's end, a headset plugged in), that stuck, and the next thing
    /// there that wanted COM the other way failed with RPC_E_CHANGED_MODE.
    fn drop(&mut self) {
        let streams = (self._input.take(), self._output.take());
        if streams.0.is_none() && streams.1.is_none() {
            return;
        }
        // Should the thread not start, the streams close here after all.
        if let Ok(closing) = std::thread::Builder::new()
            .name("audio-close".into())
            .spawn(move || drop(streams))
        {
            let _ = closing.join();
        }
    }
}

/// Opens the default microphone and speaker. `packets` receives one Opus
/// packet every 20 to 60 ms while someone speaks into the microphone. A
/// missing microphone is not an error: the call goes on, listen-only
/// (`has_mic` says which).
///
/// Runs on a thread of its own, since the audio devices set their thread up
/// for COM in a way that must not clash with the window's.
pub fn start(shared: Arc<AudioShared>, packets: UnboundedSender<Packet>) -> AudioIo {
    std::thread::Builder::new()
        .name("audio-open".into())
        .spawn(move || open(shared, packets))
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or(AudioIo {
            _input: None,
            _output: None,
        })
}

/// The first of `devices` that opens, or why none did.
fn first_open<T>(
    devices: Vec<cpal::Device>,
    none: &str,
    mut open: impl FnMut(&cpal::Device) -> Result<T, String>,
) -> Result<T, String> {
    let mut why = none.to_owned();
    for d in &devices {
        match open(d) {
            Ok(stream) => return Ok(stream),
            Err(e) => why = e,
        }
    }
    Err(why)
}

fn open(shared: Arc<AudioShared>, packets: UnboundedSender<Packet>) -> AudioIo {
    let host = cpal::default_host();
    // The ones chosen in the settings, or else Windows' default.
    let name = |d: &cpal::Device| {
        d.description()
            .map(|x| x.name().to_owned())
            .unwrap_or_default()
    };
    let output = first_open(
        crate::devices::outputs(&host),
        "nenhuma saída de áudio",
        |d| open_output(d, shared.clone()).map(|s| (s, name(d))),
    );
    let (output, output_name) = match output {
        Ok((s, n)) => (Some(s), n),
        Err(e) => {
            crate::logln!("voz: alto-falante: {e}");
            (None, String::new())
        }
    };
    let input = first_open(crate::devices::inputs(&host), "nenhum microfone", |d| {
        open_input(d, shared.clone(), packets.clone()).map(|s| (s, name(d)))
    });
    let (input, input_name) = match input {
        Ok((s, n)) => (Some(s), n),
        Err(e) => {
            crate::logln!("voz: microfone: {e}");
            (None, String::new())
        }
    };
    crate::logln!("voz: microfone \"{input_name}\", saída \"{output_name}\"");
    *shared.opened.lock().unwrap_or_else(PoisonError::into_inner) = [input_name, output_name];
    shared.has_mic.store(input.is_some(), Ordering::Relaxed);
    AudioIo {
        _input: input,
        _output: output,
    }
}

// ---------------------------------------------------------------- the microphone

/// Everything the microphone callback keeps between calls.
struct Capture {
    channels: usize,
    resampler: Resampler,
    mono: Vec<f32>,
    pending: Vec<f32>,
    processed: Vec<f32>,
    apm: AudioProcessing,
    /// The noise suppression `apm` is set up for now, and the network
    /// while that is it.
    noise: Noise,
    net: Option<Net>,
    /// The network failed or ran too slow: the standard suppressor until
    /// another one is chosen.
    net_gave_up: bool,
    /// How long the network may take per 10 ms on average (µs).
    net_budget_us: f32,
    encoder: opus::Encoder,
    /// The bitrate the encoder has now.
    bitrate: i32,
    packet: Vec<u8>,
    shared: Arc<AudioShared>,
    packets: UnboundedSender<Packet>,
    /// The call's clock, in samples: where the next packet starts.
    clock: u32,
    /// Nothing went out for the last frame (quiet or muted).
    quiet: bool,
    test: Option<TestVoice>,
    /// When the microphone caught the last sample it handed over (ms since
    /// 1970), which dates every frame for the clock voice.
    input_end_ms: f64,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // No microphone, no suppression (until another one opens and says).
        self.shared.noise_now.store(0, Ordering::Relaxed);
        self.shared.noise_us.store(0, Ordering::Relaxed);
    }
}

/// What the tests feed the encoder instead of the microphone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestVoice {
    Talk,
    Silent,
    /// "relogio": talking all the time, with a loud beep at every second of
    /// the PC's clock, as the microphone would have caught it. The other
    /// side times when it hears each beep (`BEEP`).
    Clock,
}

/// The clock voice's beep: how long, how loud, and how loud the other
/// side must hear it to count it (the voice under it stays far below).
const BEEP_MS: f64 = 40.0;
const BEEP_LEVEL: f64 = 0.6;
const BEEP_HEARD: f32 = 0.4;

impl TestVoice {
    fn from_env() -> Option<TestVoice> {
        match std::env::var(TEST_VOICE_ENV).ok()?.as_str() {
            "fala" => Some(TestVoice::Talk),
            "silencio" => Some(TestVoice::Silent),
            "relogio" => Some(TestVoice::Clock),
            _ => None,
        }
    }

    /// The signal at `clock` (in samples): a vowel-like buzz (a 140 Hz
    /// voice with its harmonics) in syllables, for two seconds out of every
    /// three. Not a steady tone: the encoder's own voice detection, which
    /// drives DTX, should take it for speech. `mouth_ms` is when the first
    /// sample would have reached the microphone, for the clock's beeps.
    fn fill(self, clock: u32, out: &mut [f32], mouth_ms: f64) {
        for (i, slot) in out.iter_mut().enumerate() {
            let n = clock.wrapping_add(i as u32);
            let t = f64::from(n) / f64::from(RATE);
            let talking = match self {
                TestVoice::Talk => t % 3.0 < 2.0,
                TestVoice::Silent => false,
                TestVoice::Clock => true,
            };
            let mut s = if talking {
                let syllable = (std::f64::consts::PI * 4.0 * t).sin().abs();
                let voice: f64 = (1..=6)
                    .map(|h| {
                        (std::f64::consts::TAU * 140.0 * f64::from(h) * t).sin() / f64::from(h)
                    })
                    .sum();
                0.12 * syllable * voice
            } else {
                0.0
            };
            if self == TestVoice::Clock {
                let at = mouth_ms + i as f64 * 1000.0 / f64::from(RATE);
                let into = at.rem_euclid(1000.0);
                if into < BEEP_MS {
                    s = BEEP_LEVEL * (std::f64::consts::TAU * 1000.0 * into / 1000.0).sin();
                }
            }
            *slot = s as f32;
        }
    }
}

fn gain_controller() -> GainController2 {
    GainController2 {
        adaptive_digital: Some(AdaptiveDigital::default()),
        ..Default::default()
    }
}

fn pipeline() -> Pipeline {
    Pipeline {
        maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
        ..Default::default()
    }
}

/// The processing for each noise suppression: the high-pass filter and the
/// echo canceller always; WebRTC's suppressor for the standard one; and
/// the gain, except with the network, which has a gain of its own after it
/// (so that what gets lifted is the voice, not the noise).
fn config(noise: Noise) -> Config {
    Config {
        pipeline: pipeline(),
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: Some(EchoCanceller::default()),
        noise_suppression: (noise == Noise::Padrao).then(|| NoiseSuppression {
            level: NoiseSuppressionLevel::High,
            ..Default::default()
        }),
        gain_controller2: (noise != Noise::Ia).then(gain_controller),
        ..Default::default()
    }
}

fn processor(noise: Noise) -> AudioProcessing {
    let stream = StreamConfig::new(RATE, 1);
    let mut apm = AudioProcessing::builder()
        .config(config(noise))
        .capture_config(stream)
        .render_config(stream)
        .build();
    let _ = apm.set_stream_delay_ms(STREAM_DELAY_MS);
    apm
}

/// After this many runs (2 s) the network's pace is judged.
const NET_WARMUP: u32 = 200;

/// The network taking more than this per 10 ms on average (µs) leaves too
/// little of the microphone's time for the rest: this PC cannot run it.
const NET_TOO_SLOW_US: f32 = 6_000.0;

/// The network at work on the microphone, and what goes with it.
struct Net {
    denoiser: Denoiser,
    /// The gain on its own, after the network.
    gain: AudioProcessing,
    /// Its average time per 10 ms (µs), over the last two seconds or so,
    /// and how many times it ran.
    avg_us: f32,
    runs: u32,
    /// Muted: it rests, and starts over when the microphone opens again.
    resting: bool,
}

impl Net {
    fn new(denoiser: Denoiser) -> Net {
        let stream = StreamConfig::new(RATE, 1);
        let gain = AudioProcessing::builder()
            .config(Config {
                pipeline: pipeline(),
                gain_controller2: Some(gain_controller()),
                ..Default::default()
            })
            .capture_config(stream)
            .render_config(stream)
            .build();
        Net {
            denoiser,
            gain,
            avg_us: 0.0,
            runs: 0,
            resting: false,
        }
    }
}

fn encoder(bitrate: i32) -> Result<opus::Encoder, String> {
    let mut enc = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .map_err(|e| e.to_string())?;
    let _ = enc.set_bitrate(opus::Bitrate::Bits(bitrate));
    // Each packet carries a rough copy of the one before, so a single lost
    // packet costs almost nothing.
    let _ = enc.set_inband_fec(true);
    let _ = enc.set_packet_loss_perc(10);
    // Discontinuous transmission: about 200 ms into a silence the encoder
    // only emits a byte or two per frame (and a little comfort noise now
    // and then), which the capture does not send. It needs to know the
    // signal is speech to do it.
    let _ = enc.set_signal(opus::Signal::Voice);
    let _ = enc.set_dtx(true);
    // Complexity stays at libopus' 9, as WebRTC keeps it on desktops:
    // measured, 5 takes a 10 ms frame from 0.66 to 0.39 ms (under 3 % of
    // one core), which is not worth the poorer voice.
    Ok(enc)
}

impl Capture {
    /// For a microphone of `channels` at `rate`.
    fn new(
        channels: usize,
        rate: u32,
        shared: Arc<AudioShared>,
        packets: UnboundedSender<Packet>,
    ) -> Result<Capture, String> {
        let bitrate = shared.plan().bitrate;
        // A microphone opened again mid-call (a headset plugged in)
        // carries on the same clock.
        let clock = shared.clock.load(Ordering::Relaxed);
        // The network joins in the first 10 ms if it is ready
        // (`follow_noise`).
        let noise = match shared.noise() {
            Noise::Ia => {
                denoise::preload();
                Noise::Padrao
            }
            other => other,
        };
        Ok(Capture {
            channels,
            resampler: Resampler::new(rate, RATE),
            mono: Vec::new(),
            pending: Vec::new(),
            processed: Vec::new(),
            apm: processor(noise),
            noise,
            net: None,
            net_gave_up: false,
            net_budget_us: NET_TOO_SLOW_US,
            encoder: encoder(bitrate)?,
            bitrate,
            packet: Vec::new(),
            shared,
            packets,
            clock,
            quiet: true,
            test: TestVoice::from_env(),
            input_end_ms: 0.0,
        })
    }

    fn feed(&mut self, data: &[f32]) {
        self.mono.clear();
        let ch = self.channels.max(1);
        for frame in data.chunks(ch) {
            self.mono
                .push(frame.iter().sum::<f32>() / frame.len() as f32);
        }
        let mono = std::mem::take(&mut self.mono);
        self.resampler.process(&mono, &mut self.pending);
        self.mono = mono;
        while self.pending.len() >= CHUNK {
            let mic: Vec<f32> = self.pending.drain(..CHUNK).collect();
            self.chunk(&mic);
        }
    }

    /// Sets the chain up for the noise suppression chosen, when that
    /// changed or the network just got ready.
    fn follow_noise(&mut self) {
        let chosen = self.shared.noise();
        if chosen != Noise::Ia {
            self.net_gave_up = false;
        }
        let want = match chosen {
            Noise::Ia if self.net_gave_up || denoise::broken() => Noise::Padrao,
            Noise::Ia if self.net.is_some() => Noise::Ia,
            // Until the model is read, the standard one.
            Noise::Ia => match denoise::ready().map(Denoiser::new) {
                Some(Ok(d)) => {
                    self.net = Some(Net::new(d));
                    Noise::Ia
                }
                Some(Err(e)) => {
                    crate::logln!("voz: a rede de supressão de ruído não abriu: {e}");
                    self.net_gave_up = true;
                    Noise::Padrao
                }
                None => {
                    denoise::preload();
                    Noise::Padrao
                }
            },
            other => other,
        };
        if want != Noise::Ia && self.net.take().is_some() {
            self.shared.noise_us.store(0, Ordering::Relaxed);
        }
        if want != self.noise {
            self.apm.apply_config(config(want));
            self.noise = want;
            crate::logln!("voz: supressão de ruído: {}", want.key());
        }
        self.shared
            .noise_now
            .store(1 + want as u8, Ordering::Relaxed);
    }

    /// The network's turn with 10 ms that left the echo canceller: they
    /// come back cleaned, with their gain, `denoise::DELAY` later.
    fn denoise(&mut self, sound: &mut [f32; CHUNK]) {
        let Some(net) = &mut self.net else { return };
        let muted =
            self.shared.mute.load(Ordering::Relaxed) || self.shared.deaf.load(Ordering::Relaxed);
        if muted {
            // Nothing goes out meanwhile (`frame`), so the network rests,
            // and what it held of before is not sent after.
            if !net.resting {
                net.denoiser.reset();
                net.resting = true;
            }
            sound.fill(0.0);
            return;
        }
        net.resting = false;
        let started = Instant::now();
        let mut cleaned = [0f32; CHUNK];
        if let Err(e) = net.denoiser.process(sound, &mut cleaned) {
            // This 10 ms go as they are; the next ones, through the
            // standard suppressor.
            crate::logln!("voz: a rede de supressão de ruído falhou: {e}");
            self.net_gave_up = true;
            return;
        }
        if net
            .gain
            .process_capture_f32(&[&cleaned], &mut [&mut sound[..]])
            .is_err()
        {
            sound.copy_from_slice(&cleaned);
        }
        crate::probe::since("voz.ruido", started);
        let us = started.elapsed().as_secs_f32() * 1e6;
        // The plain average at first, then one over the last ~2 s.
        net.runs = net.runs.saturating_add(1);
        net.avg_us += (us - net.avg_us) / net.runs.min(NET_WARMUP) as f32;
        self.shared
            .noise_us
            .store(net.avg_us as u32, Ordering::Relaxed);
        if net.runs > NET_WARMUP && net.avg_us > self.net_budget_us {
            crate::logln!(
                "voz: a rede de supressão de ruído levou {:.1} ms por 10 ms; segue a padrão",
                net.avg_us / 1000.0
            );
            self.net_gave_up = true;
            self.shared.noise_overloaded.store(true, Ordering::Relaxed);
            self.shared.wake();
        }
    }

    /// 10 ms of microphone, cleaned: echo cancellation against what the
    /// speaker played in the same 10 ms, then the noise suppression.
    fn process(&mut self, mic: &[f32]) -> [f32; CHUNK] {
        let processing = Instant::now();
        self.follow_noise();
        let mut far = [0f32; CHUNK];
        if let Ok(mut render) = self.shared.render.try_lock() {
            let n = render.len().min(CHUNK);
            for (slot, s) in far.iter_mut().zip(render.drain(..n)) {
                *slot = s;
            }
        }
        let mut far_out = [0f32; CHUNK];
        let _ = self.apm.process_render_f32(&[&far], &mut [&mut far_out]);
        let mut clean = [0f32; CHUNK];
        if self
            .apm
            .process_capture_f32(&[mic], &mut [&mut clean])
            .is_err()
        {
            clean.copy_from_slice(mic);
        }
        crate::probe::since("voz.apm", processing);
        self.denoise(&mut clean);
        clean
    }

    /// 10 ms of microphone through `process`, then on to the encoder.
    fn chunk(&mut self, mic: &[f32]) {
        let clean = self.process(mic);
        self.processed.extend_from_slice(&clean);
        loop {
            // 20, 40 or 60 ms, as the size of the call asks.
            let len = RATE as usize / 1000 * self.shared.plan().frame_ms as usize;
            if self.processed.len() < len {
                break;
            }
            let mut frame: Vec<f32> = self.processed.drain(..len).collect();
            self.frame(&mut frame);
        }
    }

    fn frame(&mut self, pcm: &mut [f32]) {
        let timestamp = self.clock;
        self.clock = self.clock.wrapping_add(pcm.len() as u32);
        self.shared.clock.store(self.clock, Ordering::Relaxed);
        let muted =
            self.shared.mute.load(Ordering::Relaxed) || self.shared.deaf.load(Ordering::Relaxed);
        if muted {
            self.shared.my_level.set(0.0);
            self.quiet = true;
            return;
        }
        if let Some(test) = self.test {
            // Everything still waiting here was caught after this frame,
            // and so was what the network still holds.
            let held = self.net.as_ref().map_or(0, |_| denoise::DELAY);
            let newer = self.pending.len() + self.processed.len() + pcm.len() + held;
            let mouth = self.input_end_ms - newer as f64 * 1000.0 / f64::from(RATE);
            test.fill(timestamp, pcm, mouth);
        }
        let energy: f32 = pcm.iter().map(|s| s * s).sum();
        let level = (energy / pcm.len() as f32).sqrt();
        self.shared.my_level.set(level);
        self.shared.loudness(&self.shared.me_loud, level);
        let bitrate = self.shared.plan().bitrate;
        if bitrate != self.bitrate
            && self
                .encoder
                .set_bitrate(opus::Bitrate::Bits(bitrate))
                .is_ok()
        {
            self.bitrate = bitrate;
        }
        self.packet.resize(4000, 0);
        let encoding = Instant::now();
        let Ok(n) = self.encoder.encode_float(pcm, &mut self.packet) else {
            return;
        };
        crate::probe::since("voz.codificacao", encoding);
        if n <= DTX_PACKET {
            // Quiet: the other side plays nothing until the next packet,
            // whose timestamp tells it how long the pause was.
            self.quiet = true;
            return;
        }
        let start = std::mem::replace(&mut self.quiet, false);
        let _ = self.packets.send(Packet {
            data: self.packet[..n].to_vec(),
            timestamp,
            start,
        });
    }
}

fn open_input(
    device: &cpal::Device,
    shared: Arc<AudioShared>,
    packets: UnboundedSender<Packet>,
) -> Result<cpal::Stream, String> {
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config = supported.config();
    let capture = Capture::new(
        usize::from(config.channels),
        config.sample_rate,
        shared,
        packets,
    )?;
    let stream = match format {
        SampleFormat::F32 => input_stream::<f32>(device, config, capture),
        SampleFormat::I16 => input_stream::<i16>(device, config, capture),
        SampleFormat::I32 => input_stream::<i32>(device, config, capture),
        SampleFormat::U16 => input_stream::<u16>(device, config, capture),
        other => return Err(format!("formato de microfone sem suporte: {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut capture: Capture,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let mut scratch: Vec<f32> = Vec::new();
    let shared = capture.shared.clone();
    let (channels, rate) = (usize::from(config.channels.max(1)), config.sample_rate);
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                // How long these samples waited for the callback: the
                // microphone's own delay.
                let ts = info.timestamp();
                let waited = ts.callback.duration_since(ts.capture).as_secs_f64() * 1000.0;
                let frames = (data.len() / channels) as f64;
                capture.input_end_ms =
                    crate::probe::wall_ms() - waited + frames * 1000.0 / f64::from(rate);
                crate::probe::record("voz.entrada", waited);
                scratch.clear();
                scratch.extend(
                    data.iter()
                        .map(|&s| <f32 as FromSample<T>>::from_sample_(s)),
                );
                capture.feed(&scratch);
            },
            move |err: cpal::Error| shared.device_error(&err),
            None,
        )
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- the speaker

struct Playback {
    channels: usize,
    /// The device's sample rate.
    rate: u32,
    /// Left and right.
    resampler: [Resampler; 2],
    fifo: VecDeque<[f32; 2]>,
    staged: [Vec<f32>; 2],
    shared: Arc<AudioShared>,
    /// A copy of `AudioShared::gains`, and the change it is of.
    gains: HashMap<i64, Gains>,
    gains_seen: u64,
}

impl Playback {
    fn new(channels: usize, rate: u32, shared: Arc<AudioShared>) -> Playback {
        Playback {
            channels,
            rate,
            resampler: [Resampler::new(RATE, rate), Resampler::new(RATE, rate)],
            fifo: VecDeque::new(),
            staged: [Vec::new(), Vec::new()],
            shared,
            gains: HashMap::new(),
            gains_seen: u64::MAX,
        }
    }

    /// Produces 10 ms of everyone mixed, at 48 kHz in stereo, which starts
    /// playing at `at_ms` (ms since 1970).
    fn mix(&mut self, at_ms: f64) {
        let started = Instant::now();
        let changed = self.shared.gains_changed.load(Ordering::Relaxed);
        if changed != self.gains_seen
            && let Ok(gains) = self.shared.gains.try_lock()
        {
            self.gains.clone_from(&gains);
            self.gains_seen = changed;
        }
        let (left, right) = self.mix_chunk(at_ms);
        // The echo canceller hears what the speaker plays.
        if let Ok(mut render) = self.shared.render.try_lock() {
            render.extend(left.iter().zip(&right).map(|(l, r)| (l + r) * 0.5));
            if render.len() > MAX_RENDER {
                let extra = render.len() - MAX_RENDER;
                render.drain(..extra);
            }
        }
        for (side, (resampler, staged)) in [left, right]
            .iter()
            .zip(self.resampler.iter_mut().zip(self.staged.iter_mut()))
        {
            staged.clear();
            resampler.process(side, staged);
        }
        // Both sides resample alike, so they come out the same length.
        let [l, r] = &self.staged;
        self.fifo.extend(l.iter().zip(r).map(|(l, r)| [*l, *r]));
        crate::probe::since("voz.mistura", started);
    }

    /// Everyone's voice in the middle, each screen's sound in stereo, each
    /// as loud as set for them here.
    fn mix_chunk(&self, at_ms: f64) -> ([f32; CHUNK], [f32; CHUNK]) {
        let gain = |uid: &i64| self.gains.get(uid).copied().unwrap_or_default();
        let mut voice = [0f32; CHUNK];
        if let Ok(peers) = self.shared.peers.try_lock() {
            for (uid, p) in peers.iter() {
                p.pull(&mut voice, at_ms, gain(uid).voice);
                self.shared.loudness(&p.loud, p.level.get());
            }
        }
        let mut screen = [0f32; 2 * CHUNK];
        if let Ok(screens) = self.shared.screens.try_lock() {
            for (uid, p) in screens.iter() {
                p.pull(&mut screen, at_ms, gain(uid).screen);
            }
        }
        let (mut left, mut right) = ([0f32; CHUNK], [0f32; CHUNK]);
        if !self.shared.deaf.load(Ordering::Relaxed) {
            for i in 0..CHUNK {
                left[i] = soft_clip(voice[i] + screen[2 * i]);
                right[i] = soft_clip(voice[i] + screen[2 * i + 1]);
            }
        }
        (left, right)
    }

    /// `ahead_ms`: how long until the first of these samples plays.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], ahead_ms: f64) {
        let ch = self.channels.max(1);
        let frames = out.len() / ch;
        let starts = crate::probe::wall_ms() + ahead_ms;
        while self.fifo.len() < frames {
            // What is already in the fifo plays first.
            let at = starts + self.fifo.len() as f64 * 1000.0 / f64::from(self.rate);
            self.mix(at);
        }
        for frame in out.chunks_mut(ch) {
            let [l, r] = self.fifo.pop_front().unwrap_or([0.0; 2]);
            let mid = T::from_sample((l + r) * 0.5);
            match frame {
                [only] => *only = mid,
                [first, second, rest @ ..] => {
                    *first = T::from_sample(l);
                    *second = T::from_sample(r);
                    // Any further speakers (5.1 and so on) get the middle,
                    // as they got everything before there was stereo.
                    rest.fill(mid);
                }
                [] => {}
            }
        }
    }
}

fn open_output(device: &cpal::Device, shared: Arc<AudioShared>) -> Result<cpal::Stream, String> {
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    // The default buffer: measured, asking for 10 ms (BufferSize::Fixed)
    // changes nothing in shared mode, where the engine keeps its own.
    let config = supported.config();
    let playback = Playback::new(usize::from(config.channels), config.sample_rate, shared);
    let stream = match format {
        SampleFormat::F32 => output_stream::<f32>(device, config, playback),
        SampleFormat::I16 => output_stream::<i16>(device, config, playback),
        SampleFormat::I32 => output_stream::<i32>(device, config, playback),
        SampleFormat::U16 => output_stream::<u16>(device, config, playback),
        other => return Err(format!("formato de saída sem suporte: {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut playback: Playback,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let shared = playback.shared.clone();
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                // How long until these samples reach the speaker: the
                // device's own delay.
                let ts = info.timestamp();
                let ahead = ts.playback.duration_since(ts.callback).as_secs_f64() * 1000.0;
                crate::probe::record("voz.saida", ahead);
                playback.fill(data, ahead);
            },
            move |err: cpal::Error| shared.device_error(&err),
            None,
        )
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- decoding

/// Turns one person's packets back into sound, covering for lost ones.
pub struct Decoder {
    decoder: opus::Decoder,
    channels: usize,
    last_seq: Option<u16>,
    /// How much sound their packets carry (20 to 60 ms, per channel), which
    /// is how much a lost one is covered for.
    frame: usize,
    pcm: Vec<f32>,
}

impl Decoder {
    /// For a voice.
    pub fn new() -> Result<Decoder, String> {
        Decoder::with(opus::Channels::Mono, 1)
    }

    /// For the sound of a screen share.
    pub fn stereo() -> Result<Decoder, String> {
        Decoder::with(opus::Channels::Stereo, 2)
    }

    fn with(layout: opus::Channels, channels: usize) -> Result<Decoder, String> {
        Ok(Decoder {
            decoder: opus::Decoder::new(RATE, layout).map_err(|e| e.to_string())?,
            channels,
            last_seq: None,
            frame: FRAME,
            // Opus' longest packet, 120 ms.
            pcm: vec![0.0; 5760 * channels],
        })
    }

    /// Decodes one RTP payload with its sequence number and timestamp,
    /// which arrived at `now`, into `out`.
    pub fn packet(
        &mut self,
        seq: u16,
        timestamp: u32,
        payload: &[u8],
        out: &PeerAudio,
        now: Instant,
    ) {
        let started = Instant::now();
        self.decode(seq, payload, out);
        out.arrived(timestamp, self.frame, now);
        crate::probe::since("voz.decodificacao", started);
    }

    fn decode(&mut self, seq: u16, payload: &[u8], out: &PeerAudio) {
        // Lengths below are per channel, the samples interleaved.
        let ch = self.channels;
        let longest = self.pcm.len() / ch;
        if let Some(last) = self.last_seq {
            let gap = seq.wrapping_sub(last);
            if gap == 0 || gap >= 0x8000 {
                // A copy, or older than what already played.
                return;
            }
            if gap > 1 {
                // The packet right after a loss carries a copy of the lost
                // one (in-band FEC); before it, plain concealment for any
                // further losses.
                let lost = usize::from(gap - 1);
                let frame = self.frame.min(longest);
                for _ in 1..lost.min(3) {
                    if let Ok(n) =
                        self.decoder
                            .decode_float(&[], &mut self.pcm[..frame * ch], false)
                    {
                        out.push(&self.pcm[..n * ch]);
                    }
                }
                if let Ok(n) = self
                    .decoder
                    .decode_float(payload, &mut self.pcm[..frame * ch], true)
                {
                    out.push(&self.pcm[..n * ch]);
                }
            }
        }
        self.last_seq = Some(seq);
        if let Ok(n) = self.decoder.decode_float(payload, &mut self.pcm, false) {
            out.push(&self.pcm[..n * ch]);
            if (FRAME / 8..=longest).contains(&n) {
                self.frame = n;
            }
        }
    }
}

// ---------------------------------------------------------------- a screen's sound

/// The bitrate of the PC's sound with a screen share: stereo, for music
/// and games as much as for speech.
const SCREEN_BITRATE: i32 = 96_000;

/// A pause this long in what Windows hands over means nothing played in
/// it: the stream's clock jumps over it.
const SCREEN_GAP: Duration = Duration::from_millis(60);

/// The PC's sound of my screen share on its way out: 20 ms stereo Opus
/// packets, stamped on a clock of its own (the stream's RTP clock), which
/// also runs through the stretches where nothing played. Silence sends
/// nothing.
pub struct ScreenEncoder {
    encoder: opus::Encoder,
    /// Interleaved samples not yet a whole packet.
    pending: Vec<f32>,
    packet: Vec<u8>,
    clock: u32,
    /// Nothing went out for the last packet.
    quiet: bool,
    last: Option<Instant>,
    packets: UnboundedSender<Packet>,
}

impl ScreenEncoder {
    pub fn new(packets: UnboundedSender<Packet>) -> Result<Self, String> {
        let mut encoder =
            opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)
                .map_err(|e| e.to_string())?;
        let _ = encoder.set_bitrate(opus::Bitrate::Bits(SCREEN_BITRATE));
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(5);
        Ok(ScreenEncoder {
            encoder,
            pending: Vec::new(),
            packet: vec![0; 4000],
            clock: 0,
            quiet: true,
            last: None,
            packets,
        })
    }

    /// 48 kHz stereo, interleaved, as Windows played it, handed over at
    /// `now`.
    pub fn feed(&mut self, pcm: &[f32], now: Instant) {
        if let Some(last) = self.last {
            let gap = now.saturating_duration_since(last);
            if gap > SCREEN_GAP {
                // Nothing at all came for a while: the clock runs on over
                // it, in whole packets, and half a packet from before goes.
                let frames = (gap.as_secs_f64() * f64::from(RATE)) as u32;
                self.clock = self
                    .clock
                    .wrapping_add(frames / FRAME as u32 * FRAME as u32);
                self.pending.clear();
                self.quiet = true;
            }
        }
        self.last = Some(now);
        self.pending.extend_from_slice(pcm);
        while self.pending.len() >= 2 * FRAME {
            let frame: Vec<f32> = self.pending.drain(..2 * FRAME).collect();
            self.frame(&frame);
        }
    }

    fn frame(&mut self, pcm: &[f32]) {
        let timestamp = self.clock;
        self.clock = self.clock.wrapping_add(FRAME as u32);
        // Digital silence (a paused video, Windows' own "nothing" packets)
        // is not sent: the other side plays nothing until the next packet.
        if pcm.iter().all(|s| s.abs() < 1e-5) {
            self.quiet = true;
            return;
        }
        let Ok(n) = self.encoder.encode_float(pcm, &mut self.packet) else {
            return;
        };
        if n <= DTX_PACKET {
            self.quiet = true;
            return;
        }
        let start = std::mem::replace(&mut self.quiet, false);
        let _ = self.packets.send(Packet {
            data: self.packet[..n].to_vec(),
            timestamp,
            start,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_rate() {
        let mut up = Resampler::new(44_100, RATE);
        let mut out = Vec::new();
        up.process(&vec![0.5; 44_100], &mut out);
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        // Same rate passes straight through.
        let mut same = Resampler::new(RATE, RATE);
        let mut out = Vec::new();
        same.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_peer_plays_only_after_priming_and_stops_when_dry() {
        let p = PeerAudio::default();
        let mut out = [0f32; CHUNK];
        p.push(&vec![0.5; PRIME - 1]);
        p.pull(&mut out, 0.0, 1.0);
        assert!(out.iter().all(|&s| s == 0.0), "tocou antes de encher");
        p.push(&[0.5; 1]);
        p.pull(&mut out, 0.0, 1.0);
        assert!(out.iter().all(|&s| s == 0.5));
        assert!(p.level.get() > 0.4);
        // Drain the rest, then it waits again.
        for _ in 0..10 {
            let mut o = [0f32; CHUNK];
            p.pull(&mut o, 0.0, 1.0);
        }
        p.push(&[0.5; CHUNK]);
        let mut o = [0f32; CHUNK];
        p.pull(&mut o, 0.0, 1.0);
        assert!(o.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_burst_does_not_pile_up_delay() {
        let p = PeerAudio::default();
        p.push(&vec![0.1; MAX_BUFFER * 2]);
        assert!(p.buf.lock().unwrap().len() <= MAX_BUFFER);
    }

    #[test]
    fn silence_sends_almost_nothing() {
        let mut enc = encoder(32_000).unwrap();
        let mut packet = vec![0u8; 4000];
        let quiet = [0f32; FRAME];
        let sizes: Vec<usize> = (0..50)
            .map(|_| enc.encode_float(&quiet, &mut packet).unwrap())
            .collect();
        // A second of silence: after the first fifth of it, DTX leaves only
        // packets too small to send, but for the odd comfort-noise update.
        let sent = sizes[10..].iter().filter(|&&n| n > DTX_PACKET).count();
        assert!(sent <= 4, "{sizes:?}");
        // And the test voice talks for two seconds out of three.
        let mut buf = vec![0f32; RATE as usize];
        TestVoice::Talk.fill(0, &mut buf, 0.0);
        assert!(buf.iter().any(|s| s.abs() > 0.05));
        TestVoice::Talk.fill(2 * RATE + RATE / 2, &mut buf[..FRAME], 0.0);
        assert!(buf[..FRAME].iter().all(|&s| s == 0.0));
        let mut talk = vec![0f32; FRAME];
        TestVoice::Talk.fill(RATE / 3, &mut talk, 0.0);
        let n = enc.encode_float(&talk, &mut packet).unwrap();
        assert!(n > DTX_PACKET, "{n}");
    }

    #[test]
    fn the_clock_voice_beeps_at_the_top_of_each_second() {
        // 20 ms that start 10 ms before a second of the clock.
        let mut buf = vec![0f32; FRAME];
        TestVoice::Clock.fill(0, &mut buf, 1_793_000_000_990.0);
        let loud = buf.iter().position(|s| s.abs() > BEEP_HEARD);
        // 10 ms in (480 samples), within the beep's first cycle.
        assert!(
            loud.is_some_and(|i| (480..480 + 48).contains(&i)),
            "{loud:?}"
        );
        // The voice under it stays well below what counts as the beep.
        let mut voice = vec![0f32; RATE as usize / 2];
        TestVoice::Clock.fill(0, &mut voice, 1_793_000_000_100.0);
        assert!(voice.iter().all(|s| s.abs() < BEEP_HEARD));
        assert!(voice.iter().any(|s| s.abs() > 0.05));
    }

    #[test]
    fn longer_packets_are_covered_for_by_their_own_length() {
        let mut enc = encoder(24_000).unwrap();
        let mut talk = vec![0f32; 2 * FRAME];
        let mut packet = vec![0u8; 4000];
        let mut packets = Vec::new();
        for i in 0..4u32 {
            TestVoice::Talk.fill(i * 2 * FRAME as u32, &mut talk, 0.0);
            let n = enc.encode_float(&talk, &mut packet).unwrap();
            packets.push(packet[..n].to_vec());
        }
        let mut dec = Decoder::new().unwrap();
        let out = PeerAudio::default();
        dec.decode(1, &packets[0], &out);
        assert_eq!(dec.frame, 2 * FRAME);
        // 2 is lost: the gap is covered with 40 ms, not 20.
        dec.decode(3, &packets[2], &out);
        assert_eq!(out.buf.lock().unwrap().len(), 3 * 2 * FRAME);
    }

    #[test]
    fn the_plan_has_safe_defaults() {
        let shared = AudioShared::default();
        assert_eq!(shared.plan(), VoicePlan::default());
        shared.set_plan(VoicePlan {
            bitrate: 24_000,
            frame_ms: 40,
        });
        assert_eq!(shared.plan().frame_ms, 40);
        shared.frame_ms.store(33, Ordering::Relaxed);
        assert_eq!(
            shared.plan().frame_ms,
            VoicePlan::default().frame_ms,
            "Opus has no 33 ms frames"
        );
    }

    #[test]
    fn opus_round_trip_and_loss_concealment() {
        let mut enc = encoder(48_000).unwrap();
        let tone: Vec<f32> = (0..FRAME)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.3)
            .collect();
        let mut packet = vec![0u8; 4000];
        let mut packets = Vec::new();
        for _ in 0..5 {
            let n = enc.encode_float(&tone, &mut packet).unwrap();
            packets.push(packet[..n].to_vec());
        }
        let mut dec = Decoder::new().unwrap();
        let out = PeerAudio::default();
        let start = Instant::now();
        let at = |n: u32| start + Duration::from_millis(u64::from(n) * 20);
        let ts = |n: u32| n * FRAME as u32;
        dec.packet(10, ts(0), &packets[0], &out, at(0));
        dec.packet(11, ts(1), &packets[1], &out, at(1));
        // 12 and 13 are lost; 14 arrives.
        dec.packet(14, ts(4), &packets[4], &out, at(4));
        // A late copy of 12 is ignored.
        dec.packet(12, ts(2), &packets[2], &out, at(5));
        let buffered = out.buf.lock().unwrap().len();
        // Two plus the recovered one, plus one concealed, plus the last.
        assert_eq!(buffered, 5 * FRAME);
    }

    #[test]
    fn the_target_follows_the_jitter() {
        let p = PeerAudio::default();
        let start = Instant::now();
        // Packets on time, every 20 ms, for a second: one packet and a
        // mixing step.
        for n in 0..50u32 {
            let at = start + Duration::from_millis(u64::from(n) * 20);
            p.arrived(n * FRAME as u32, FRAME, at);
        }
        assert_eq!(p.target(), FRAME + CHUNK);
        // One packet 30 ms late: that much more is held from then on.
        p.arrived(
            50 * FRAME as u32,
            FRAME,
            start + Duration::from_millis(50 * 20 + 30),
        );
        let late = p.target();
        assert!(
            (FRAME + CHUNK + 1400..=FRAME + CHUNK + 1460).contains(&late),
            "{late}"
        );
        // Two seconds later, it is forgotten.
        for n in 51..200u32 {
            let at = start + Duration::from_millis(u64::from(n) * 20);
            p.arrived(n * FRAME as u32, FRAME, at);
        }
        assert_eq!(p.target(), FRAME + CHUNK);
    }

    #[test]
    fn creeping_latency_is_cut_where_it_is_quiet() {
        let p = PeerAudio::default();
        p.target.store(FRAME + CHUNK, Ordering::Relaxed);
        // 80 ms of quiet queued (well above the 30 ms target): the next
        // pull cuts it back to the target, then takes its 10 ms.
        p.push(&vec![0.0; 8 * CHUNK]);
        let mut out = [0f32; CHUNK];
        p.pull(&mut out, 0.0, 1.0);
        assert_eq!(p.buf.lock().unwrap().len(), FRAME + CHUNK - CHUNK);
        // Speech is never cut: only the 20 ms of quiet in front of it go.
        p.push(&vec![0.3; 8 * CHUNK]);
        p.pull(&mut out, 0.0, 1.0);
        assert_eq!(p.buf.lock().unwrap().len(), 8 * CHUNK - CHUNK);
        assert!(out.iter().all(|&s| s == 0.3));
    }

    #[test]
    fn clipping_is_soft_and_bounded() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(3.0) <= 1.0);
        assert!(soft_clip(-3.0) >= -1.0);
        assert!(soft_clip(0.9) > 0.8);
    }

    #[test]
    fn a_screens_sound_is_held_and_played_in_stereo() {
        let p = PeerAudio::stereo();
        // Left loud, right quiet: interleaved, primed per frame (60 ms of
        // both channels).
        let frames: Vec<f32> = (0..PRIME).flat_map(|_| [0.5, 0.1]).collect();
        p.push(&frames[..frames.len() - 2]);
        let mut out = [0f32; 2 * CHUNK];
        p.pull(&mut out, 0.0, 1.0);
        assert!(out.iter().all(|&s| s == 0.0), "tocou antes de encher");
        p.push(&[0.5, 0.1]);
        p.pull(&mut out, 0.0, 0.5);
        assert!(out.chunks(2).all(|f| f == [0.25, 0.05]), "{:?}", &out[..4]);
        // A burst is cut to whole frames: left stays left.
        p.push(&vec![0.5; 4 * MAX_BUFFER]);
        assert!(p.buf.lock().unwrap().len() <= 2 * MAX_BUFFER);
        assert_eq!(p.buf.lock().unwrap().len() % 2, 0);
    }

    fn playback_with(shared: &Arc<AudioShared>, channels: usize) -> Playback {
        let mut p = Playback::new(channels, RATE, shared.clone());
        p.gains_seen = shared.gains_changed.load(Ordering::Relaxed);
        p
    }

    #[test]
    fn each_person_plays_as_loud_as_set_here() {
        let shared = Arc::new(AudioShared::default());
        shared.peer(1).push(&vec![0.2; PRIME]);
        shared.peer(2).push(&vec![0.1; PRIME]);
        let screen: Vec<f32> = (0..PRIME).flat_map(|_| [0.3, -0.3]).collect();
        shared.screen_peer(1).push(&screen);
        // Person 1's voice silenced and their screen at half; person 2 as
        // they come.
        shared.set_gains(
            1,
            Gains {
                voice: 0.0,
                screen: 0.5,
            },
        );
        let mut playback = playback_with(&shared, 2);
        playback.gains = HashMap::from([(1, shared.gains(1))]);
        let (left, right) = playback.mix_chunk(0.0);
        assert!((left[0] - (0.1 + 0.15)).abs() < 1e-6, "{}", left[0]);
        assert!((right[0] - (0.1 - 0.15)).abs() < 1e-6, "{}", right[0]);
        // Back to the defaults: nothing kept for them.
        shared.set_gains(1, Gains::default());
        assert!(shared.gains.lock().unwrap().is_empty());
        // Deafened, nothing at all.
        shared.deaf.store(true, Ordering::Relaxed);
        let (left, right) = playback.mix_chunk(0.0);
        assert!(left.iter().chain(&right).all(|&s| s == 0.0));
    }

    #[test]
    fn the_speaker_takes_left_and_right_and_the_middle_for_the_rest() {
        let shared = Arc::new(AudioShared::default());
        let screen: Vec<f32> = (0..4 * PRIME).flat_map(|_| [0.4, 0.0]).collect();
        shared.screen_peer(7).push(&screen);
        let mut three = playback_with(&shared, 3);
        let mut out = vec![0f32; 3 * 64];
        three.fill(&mut out, 0.0);
        assert!(
            out.chunks(3).all(|f| f == [0.4, 0.0, 0.2]),
            "{:?}",
            &out[..3]
        );
        let mut one = playback_with(&shared, 1);
        let mut mono = vec![0f32; 64];
        one.fill(&mut mono, 0.0);
        assert!(
            mono.iter().all(|&s| (s - 0.2).abs() < 1e-6),
            "{:?}",
            &mono[..2]
        );
    }

    #[test]
    fn a_screens_sound_goes_out_in_stereo_and_skips_silence() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut enc = ScreenEncoder::new(tx).unwrap();
        let start = Instant::now();
        // 100 ms of silence: nothing goes.
        enc.feed(&vec![0.0; 2 * 4800], start);
        assert!(rx.try_recv().is_err());
        // 100 ms of a tone, louder on the left: five packets, the first
        // marked, on the stream's own clock.
        let tone: Vec<f32> = (0..4800)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin();
                [0.4 * s, 0.1 * s]
            })
            .collect();
        enc.feed(&tone, start + Duration::from_millis(10));
        let packets: Vec<Packet> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(packets.len(), 5);
        assert!(packets[0].start && !packets[1].start);
        assert_eq!(packets[1].timestamp - packets[0].timestamp, FRAME as u32);
        // After a second with nothing at all, the clock has moved on by it.
        enc.feed(&tone, start + Duration::from_millis(1010));
        let later: Vec<Packet> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(later[0].start);
        let moved = later[0].timestamp - packets[4].timestamp;
        assert!((RATE * 9 / 10..=RATE * 11 / 10).contains(&moved), "{moved}");
        // Decoded on the other side, still louder on the left.
        let mut dec = Decoder::stereo().unwrap();
        let out = PeerAudio::stereo();
        for (i, p) in packets.iter().enumerate() {
            dec.decode(i as u16, &p.data, &out);
        }
        let buf = out.buf.lock().unwrap();
        assert_eq!(buf.len(), 5 * 2 * FRAME);
        let energy = |side: usize| -> f32 { buf.iter().skip(side).step_by(2).map(|s| s * s).sum() };
        assert!(energy(0) > 4.0 * energy(1), "{} {}", energy(0), energy(1));
    }

    #[test]
    fn the_echo_canceller_takes_our_frames() {
        for noise in Noise::ALL {
            let mut apm = processor(noise);
            let mic = [0.01f32; CHUNK];
            let mut out = [0f32; CHUNK];
            apm.process_render_f32(&[&[0f32; CHUNK]], &mut [&mut [0f32; CHUNK]])
                .unwrap();
            apm.process_capture_f32(&[&mic], &mut [&mut out]).unwrap();
        }
    }

    /// A microphone at 48 kHz mono, with the network read beforehand (as
    /// a call's start does in the background).
    fn test_capture(noise: Noise) -> (Capture, Arc<AudioShared>) {
        denoise::load().unwrap();
        let shared = Arc::new(AudioShared::default());
        shared.set_noise(noise);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let capture = Capture::new(1, RATE, shared.clone(), tx).unwrap();
        (capture, shared)
    }

    /// White noise from a fixed seed, at an RMS of about `level`.
    fn hiss(n: usize, level: f32) -> Vec<f32> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let u = (x >> 11) as f32 / (1u64 << 53) as f32 - 0.5;
                // A uniform value in [-0.5, 0.5) has an RMS of 1/√12.
                u * level * 12f32.sqrt()
            })
            .collect()
    }

    fn level(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// How loud `noise` comes out of the chain set up for `choice`, after
    /// a second to settle (in dB against what went in).
    fn left_of(choice: Noise, noise: &[f32]) -> f32 {
        let (mut capture, _) = test_capture(choice);
        // However busy the other tests keep this PC.
        capture.net_budget_us = f32::INFINITY;
        let out: Vec<f32> = noise
            .as_chunks::<CHUNK>()
            .0
            .iter()
            .flat_map(|c| capture.process(c))
            .collect();
        let settled = RATE as usize;
        20.0 * (level(&out[settled..]) / level(&noise[settled..])).log10()
    }

    /// Where `out` lines up with `clean` (out[lag + i] against clean[i]),
    /// within 4000 samples, judged over the first 40 seconds (a stretch
    /// with too little speech would leave it to chance).
    fn best_lag(out: &[f32], clean: &[f32]) -> usize {
        let span = 0..(40 * RATE as usize).min(clean.len().min(out.len()) - 4000);
        let dot = |lag: usize| -> f64 {
            clean[span.clone()]
                .iter()
                .zip(&out[span.start + lag..])
                .map(|(c, o)| f64::from(c * o))
                .sum()
        };
        let best = |lags: &mut dyn Iterator<Item = usize>| {
            lags.map(|l| (l, dot(l)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map_or(0, |(l, _)| l)
        };
        let rough = best(&mut (0..4000).step_by(16));
        best(&mut (rough.saturating_sub(16)..rough + 16))
    }

    /// In `out` (`lag` behind `clean`): how far below the voice its pauses
    /// are, and how loud its voice is against the clean one (dB).
    fn pauses_and_voice(out: &[f32], clean: &[f32], lag: usize) -> (f32, f32) {
        let frames = clean.len().min(out.len() - lag) / CHUNK;
        let c: Vec<f32> = (0..frames)
            .map(|k| level(&clean[k * CHUNK..(k + 1) * CHUNK]))
            .collect();
        let o: Vec<f32> = (0..frames)
            .map(|k| level(&out[lag + k * CHUNK..lag + (k + 1) * CHUNK]))
            .collect();
        let loudest = c.iter().copied().fold(0.0, f32::max);
        let mean = |pick: &dyn Fn(f32) -> bool, of: &[f32]| {
            let (sum, n) = c
                .iter()
                .zip(of)
                .filter(|(c, _)| pick(**c))
                .fold((0.0, 0), |(s, n), (_, v)| (s + v, n + 1));
            sum / n.max(1) as f32
        };
        let speech = |c: f32| c > 0.001 * loudest;
        let pause = |c: f32| c < 1e-4;
        (
            20.0 * (mean(&speech, &o) / mean(&pause, &o).max(1e-9)).log10(),
            20.0 * (mean(&speech, &o) / mean(&speech, &c)).log10(),
        )
    }

    /// Real speech in real noise through each suppression: clips of
    /// DPDFNet's evaluation set (huggingface.co/datasets/Ceva-IP/
    /// DPDFNet_EvalSet), at 48 kHz in raw f32, NAME_noisy.f32 next to
    /// NAME_clean.f32 in the folder BABUTELA_RUIDO_AVAL names.
    #[test]
    #[ignore = "precisa de gravações (BABUTELA_RUIDO_AVAL)"]
    fn real_noisy_speech() {
        let dir = std::path::PathBuf::from(std::env::var("BABUTELA_RUIDO_AVAL").unwrap());
        let read = |name: String| -> Vec<f32> {
            std::fs::read(dir.join(name))
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        };
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| {
                let file = e.ok()?.file_name();
                Some(file.to_str()?.strip_suffix("_noisy.f32")?.to_owned())
            })
            .collect();
        names.sort();
        assert!(!names.is_empty());
        for name in names {
            let noisy = read(format!("{name}_noisy.f32"));
            let clean = read(format!("{name}_clean.f32"));
            let (pauses, _) = pauses_and_voice(&noisy, &clean, 0);
            println!("{name}: entrada, pausas {pauses:.1} dB abaixo da voz");
            for choice in Noise::ALL {
                let (mut capture, shared) = test_capture(choice);
                capture.net_budget_us = f32::INFINITY;
                let started = Instant::now();
                let out: Vec<f32> = noisy
                    .as_chunks::<CHUNK>()
                    .0
                    .iter()
                    .flat_map(|c| capture.process(c))
                    .collect();
                let per = started.elapsed().as_secs_f64() * 1000.0 / (noisy.len() / CHUNK) as f64;
                assert_eq!(shared.noise_now(), Some(choice));
                let lag = best_lag(&out, &clean);
                let (pauses, voice) = pauses_and_voice(&out, &clean, lag);
                println!(
                    "  {:9}: pausas {pauses:5.1} dB abaixo da voz, voz {voice:+5.1} dB, \
                     {:.1} ms de atraso, {per:.2} ms por 10 ms",
                    choice.key(),
                    lag as f64 * 1000.0 / f64::from(RATE)
                );
            }
        }
    }

    #[test]
    fn the_network_takes_more_noise_away_than_webrtc() {
        let noise = hiss(3 * RATE as usize, 0.02);
        let ia = left_of(Noise::Ia, &noise);
        let padrao = left_of(Noise::Padrao, &noise);
        let off = left_of(Noise::Desligada, &noise);
        println!("ruído que sobra: IA {ia:.1} dB, padrão {padrao:.1} dB, desligada {off:.1} dB");
        assert!(ia < -30.0, "{ia}");
        assert!(ia < padrao - 10.0, "IA {ia}, padrão {padrao}");
        assert!(padrao < off, "padrão {padrao}, desligada {off}");
    }

    #[test]
    fn the_microphone_follows_the_choice_mid_call() {
        let (mut capture, shared) = test_capture(Noise::Ia);
        // Opened with the standard one, the network takes over at once
        // when it is ready.
        assert_eq!(capture.noise, Noise::Padrao);
        assert_eq!(shared.noise_now(), None);
        let quiet = [0f32; CHUNK];
        capture.process(&quiet);
        assert_eq!(shared.noise_now(), Some(Noise::Ia));
        assert!(capture.net.is_some());
        assert!(shared.noise_ms() > 0.0);
        for (choice, net) in [
            (Noise::Padrao, false),
            (Noise::Desligada, false),
            (Noise::Ia, true),
        ] {
            shared.set_noise(choice);
            capture.process(&quiet);
            assert_eq!(shared.noise_now(), Some(choice));
            assert_eq!(capture.net.is_some(), net, "{choice:?}");
            // Its time per 10 ms is only there while it runs.
            assert_eq!(shared.noise_ms() > 0.0, net, "{choice:?}");
        }
        // Muted, the network rests (and sends silence); unmuted, it runs
        // again from the start.
        shared.mute.store(true, Ordering::Relaxed);
        let loud = [0.3f32; CHUNK];
        let runs = capture.net.as_ref().unwrap().runs;
        assert_eq!(capture.process(&loud), [0f32; CHUNK]);
        assert!(capture.net.as_ref().unwrap().resting);
        assert_eq!(capture.net.as_ref().unwrap().runs, runs);
        shared.mute.store(false, Ordering::Relaxed);
        capture.process(&loud);
        let net = capture.net.as_ref().unwrap();
        assert!(!net.resting && net.runs == runs + 1);
        assert!(shared.noise_ms() > 0.0);
        drop(capture);
        assert_eq!(shared.noise_now(), None);
        assert_eq!(shared.noise_ms(), 0.0);
    }

    #[test]
    fn a_pc_too_slow_for_the_network_falls_back_to_webrtc() {
        let (mut capture, shared) = test_capture(Noise::Ia);
        let quiet = [0f32; CHUNK];
        capture.process(&quiet);
        // As if each 10 ms had taken a second, for two seconds.
        let net = capture.net.as_mut().unwrap();
        net.runs = NET_WARMUP;
        net.avg_us = 1e6;
        capture.process(&quiet);
        assert!(shared.noise_overloaded.load(Ordering::Relaxed));
        capture.process(&quiet);
        assert_eq!(shared.noise_now(), Some(Noise::Padrao));
        assert!(capture.net.is_none());
        // Chosen again, it gets another go.
        shared.set_noise(Noise::Padrao);
        capture.process(&quiet);
        shared.set_noise(Noise::Ia);
        capture.process(&quiet);
        assert_eq!(shared.noise_now(), Some(Noise::Ia));
    }
}

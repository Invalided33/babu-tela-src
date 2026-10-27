//! Short synthesized cues, one per event, so the app ships no audio files.
//!
//! Each cue is a few notes (frequency, start offset); rising shapes mean
//! "on / arrived", falling shapes mean "off / gone". They are the same
//! notes, envelopes and levels the old page made with the Web Audio API,
//! computed here into a buffer once and played through the default output.

use std::collections::{HashMap, HashSet};
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink};

const RATE: u32 = 48_000;

/// How long a sound to be played waits for the output while it opens (it
/// is opening right after the app starts, and after the device changed).
const OPEN_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cue {
    Join,
    Leave,
    Disconnect,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    StreamStart,
    StreamStop,
    Message,
    /// No sound at all, for calls that end without the user noticing.
    None,
}

#[derive(Clone, Copy)]
enum Wave {
    Sine,
    Triangle,
}

struct Shape {
    notes: &'static [(f32, f32)],
    decay: f32,
    gain: f32,
    wave: Wave,
}

// Note frequencies, as the old page named them.
const G3: f32 = 196.0;
const D4: f32 = 293.66;
const E4: f32 = 329.63;
const G4: f32 = 392.0;
const B4: f32 = 493.88;
const C5: f32 = 523.25;
const D5: f32 = 587.33;
const E5: f32 = 659.25;
const G5: f32 = 783.99;
const A5: f32 = 880.0;
const C6: f32 = 1046.5;
const E6: f32 = 1318.5;

impl Cue {
    /// Every cue that has a sound, in the order the settings list them.
    pub const ALL: [Cue; 10] = [
        Cue::Join,
        Cue::Leave,
        Cue::Disconnect,
        Cue::Mute,
        Cue::Unmute,
        Cue::Deafen,
        Cue::Undeafen,
        Cue::StreamStart,
        Cue::StreamStop,
        Cue::Message,
    ];

    /// The name saved in prefs.json (the keys the old page used).
    pub fn key(self) -> &'static str {
        match self {
            Cue::Join => "join",
            Cue::Leave => "leave",
            Cue::Disconnect => "disconnect",
            Cue::Mute => "mute",
            Cue::Unmute => "unmute",
            Cue::Deafen => "deafen",
            Cue::Undeafen => "undeafen",
            Cue::StreamStart => "stream_start",
            Cue::StreamStop => "stream_stop",
            Cue::Message => "message",
            Cue::None => "",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Cue::Join => "Alguém entrou na chamada",
            Cue::Leave => "Alguém saiu da chamada",
            Cue::Disconnect => "Chamada desconectada",
            Cue::Mute => "Microfone desligado",
            Cue::Unmute => "Microfone ligado",
            Cue::Deafen => "Áudio desligado",
            Cue::Undeafen => "Áudio ligado",
            Cue::StreamStart => "Alguém começou a transmitir a tela",
            Cue::StreamStop => "Alguém parou de transmitir a tela",
            Cue::Message => "Mensagem nova (direta ou menção)",
            Cue::None => "",
        }
    }

    fn shape(self) -> Option<Shape> {
        let s = |notes, decay, gain, wave| {
            Some(Shape {
                notes,
                decay,
                gain,
                wave,
            })
        };
        match self {
            Cue::Join => s(&[(D5, 0.0), (A5, 0.085)], 0.32, 1.0, Wave::Sine),
            Cue::Leave => s(&[(A5, 0.0), (D5, 0.085)], 0.32, 1.0, Wave::Sine),
            Cue::Disconnect => s(&[(E5, 0.0), (B4, 0.11), (E4, 0.22)], 0.45, 1.0, Wave::Sine),
            Cue::Mute => s(&[(E5, 0.0), (B4, 0.06)], 0.16, 0.8, Wave::Sine),
            Cue::Unmute => s(&[(B4, 0.0), (E5, 0.06)], 0.16, 0.8, Wave::Sine),
            Cue::Deafen => s(
                &[(G4, 0.0), (D4, 0.07), (G3, 0.14)],
                0.22,
                1.0,
                Wave::Triangle,
            ),
            Cue::Undeafen => s(
                &[(G3, 0.0), (D4, 0.07), (G4, 0.14)],
                0.22,
                1.0,
                Wave::Triangle,
            ),
            Cue::StreamStart => s(
                &[(C5, 0.0), (E5, 0.07), (G5, 0.14), (C6, 0.21)],
                0.3,
                1.0,
                Wave::Sine,
            ),
            Cue::StreamStop => s(
                &[(C6, 0.0), (G5, 0.07), (E5, 0.14), (C5, 0.21)],
                0.3,
                1.0,
                Wave::Sine,
            ),
            Cue::Message => s(&[(A5, 0.0), (E6, 0.045)], 0.4, 0.7, Wave::Sine),
            Cue::None => None,
        }
    }
}

fn wave(kind: Wave, phase: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    match kind {
        Wave::Sine => (tau * phase).sin(),
        // Same shape as the Web Audio triangle: peaks at a quarter period.
        Wave::Triangle => {
            let p = phase.fract();
            if p < 0.25 {
                4.0 * p
            } else if p < 0.75 {
                2.0 - 4.0 * p
            } else {
                4.0 * p - 4.0
            }
        }
    }
}

/// Web Audio's exponentialRampToValueAtTime, between two points.
fn ramp(from: f32, to: f32, t: f32, t0: f32, t1: f32) -> f32 {
    from * (to / from).powf(((t - t0) / (t1 - t0)).clamp(0.0, 1.0))
}

/// Renders one cue into mono samples at 48 kHz.
fn render(shape: &Shape) -> Vec<f32> {
    const START: f32 = 0.02;
    const ATTACK: f32 = 0.012;
    const FLOOR: f32 = 0.0001;
    let out_gain = 0.35 * shape.gain;
    let last = shape.notes.iter().map(|n| n.1).fold(0.0, f32::max);
    let total = START + last + shape.decay + 0.03;
    let len = (total * RATE as f32).ceil() as usize;
    let mut buf = vec![0f32; len];
    for &(freq, delay) in shape.notes {
        let t_on = START + delay;
        let t_off = t_on + shape.decay + 0.03;
        let first = (t_on * RATE as f32) as usize;
        let end = ((t_off * RATE as f32).ceil() as usize).min(len);
        for (i, slot) in buf.iter_mut().enumerate().take(end).skip(first) {
            let t = i as f32 / RATE as f32;
            let env = if t < t_on + ATTACK {
                ramp(FLOOR, 1.0, t, t_on, t_on + ATTACK)
            } else if t < t_on + shape.decay {
                ramp(1.0, FLOOR, t, t_on + ATTACK, t_on + shape.decay)
            } else {
                FLOOR
            };
            let local = t - t_on;
            // A base tone with a quiet octave on top gives a soft, bell-like
            // sound.
            let tone =
                wave(shape.wave, freq * local) + 0.12 * wave(Wave::Triangle, 2.0 * freq * local);
            *slot += out_gain * env * tone;
        }
    }
    buf
}

pub struct Sounds {
    sink: Mutex<Option<MixerDeviceSink>>,
    /// Told whenever an opening of the device ends, opened or not.
    opened: Condvar,
    /// Set when the device stopped working (unplugged, switched, its format
    /// changed); the next cue opens the current default one again.
    broken: Arc<AtomicBool>,
    /// The device is being opened. Turned off under `sink`'s lock, so
    /// whoever waits on `opened` sees the device and this change together.
    opening: AtomicBool,
    off: Mutex<HashSet<String>>,
    rendered: Mutex<HashMap<Cue, Arc<Vec<f32>>>>,
}

impl Sounds {
    pub fn new(off: HashSet<String>) -> Arc<Sounds> {
        let sounds = Arc::new(Sounds {
            sink: Mutex::new(None),
            opened: Condvar::new(),
            broken: Arc::new(AtomicBool::new(false)),
            opening: AtomicBool::new(false),
            off: Mutex::new(off),
            rendered: Mutex::new(HashMap::new()),
        });
        // Open from the start: kept open with nothing to play, it measured
        // under 0.1 % of a core, and opening on demand would delay the
        // first sound by the device's start-up.
        sounds.open();
        sounds
    }

    /// Opens the output (the one chosen, or the default) on a thread of its
    /// own: the audio device sets its thread up for COM in a way that must
    /// not clash with the window's.
    fn open(self: &Arc<Self>) {
        if self.opening.swap(true, Ordering::SeqCst) {
            return;
        }
        let s = self.clone();
        let spawned = std::thread::Builder::new()
            .name("sounds".into())
            .spawn(move || {
                // The output chosen in the settings, or else the default.
                let mut sink = Err("nenhuma saída de áudio".to_owned());
                for device in crate::devices::outputs(&cpal::default_host()) {
                    use cpal::traits::DeviceTrait;
                    let name = device
                        .description()
                        .map(|d| d.name().to_owned())
                        .unwrap_or_default();
                    let broken = s.broken.clone();
                    sink = DeviceSinkBuilder::from_device(device)
                        .and_then(|b| {
                            b.with_error_callback(move |_e| broken.store(true, Ordering::SeqCst))
                                .open_stream()
                        })
                        .map_err(|e| e.to_string());
                    if sink.is_ok() {
                        crate::logln!("sons: saída \"{name}\"");
                        break;
                    }
                }
                let mut slot = s.sink.lock().unwrap_or_else(PoisonError::into_inner);
                match sink {
                    Ok(mut sink) => {
                        sink.log_on_drop(false);
                        s.broken.store(false, Ordering::SeqCst);
                        *slot = Some(sink);
                    }
                    Err(e) => crate::logln!("sons: {e}"),
                }
                s.opening.store(false, Ordering::SeqCst);
                drop(slot);
                s.opened.notify_all();
            });
        if spawned.is_err() {
            self.opening.store(false, Ordering::SeqCst);
        }
    }

    /// Another output was chosen: opens it now, so the next sound goes
    /// through it.
    pub fn output_changed(self: &Arc<Self>) {
        self.sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.open();
    }

    pub fn enabled(&self, cue: Cue) -> bool {
        !self
            .off
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(cue.key())
    }

    pub fn set_enabled(&self, cue: Cue, on: bool) {
        let mut off = self.off.lock().unwrap_or_else(PoisonError::into_inner);
        if on {
            off.remove(cue.key());
        } else {
            off.insert(cue.key().to_owned());
        }
    }

    /// The cues turned off, for prefs.json.
    pub fn off_list(&self) -> Vec<String> {
        let mut list: Vec<String> = self
            .off
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        list.sort();
        list
    }

    pub fn enabled_count(&self) -> usize {
        Cue::ALL.iter().filter(|c| self.enabled(**c)).count()
    }

    /// Plays any sound through the same output as the cues (an attachment
    /// being played). False when there is no output right now.
    pub fn add_source(self: &Arc<Self>, source: impl rodio::Source + Send + 'static) -> bool {
        if self.broken.load(Ordering::SeqCst) {
            self.sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            self.open();
        }
        // The device may still be opening (it is, right after start): until
        // that ends, one way or the other.
        let sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        let (sink, _) = self
            .opened
            .wait_timeout_while(sink, OPEN_WAIT, |sink| {
                sink.is_none() && self.opening.load(Ordering::SeqCst)
            })
            .unwrap_or_else(PoisonError::into_inner);
        match sink.as_ref() {
            Some(sink) => {
                sink.mixer().add(source);
                true
            }
            None => false,
        }
    }

    /// Plays a cue unless it is turned off.
    pub fn play(self: &Arc<Self>, cue: Cue) {
        if self.enabled(cue) {
            self.play_always(cue);
        }
    }

    /// Plays a cue even if it is turned off (the preview button).
    pub fn play_always(self: &Arc<Self>, cue: Cue) {
        let Some(shape) = cue.shape() else { return };
        if self.broken.load(Ordering::SeqCst) {
            // This cue is lost; the next one plays on the reopened device.
            self.sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            self.open();
            return;
        }
        let samples = self
            .rendered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(cue)
            .or_insert_with(|| Arc::new(render(&shape)))
            .clone();
        let sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(sink) = sink.as_ref() else { return };
        let (Some(channels), Some(rate)) = (NonZero::new(1u16), NonZero::new(RATE)) else {
            return;
        };
        sink.mixer().add(SamplesBuffer::new(
            channels,
            rate,
            samples.as_slice().to_vec(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cue_renders_something_audible_and_bounded() {
        for cue in Cue::ALL {
            let shape = cue.shape().expect("todo aviso tem forma");
            let buf = render(&shape);
            let peak = buf.iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.05, "{cue:?} está mudo ({peak})");
            assert!(peak < 1.0, "{cue:?} estoura ({peak})");
            // Starts and ends in silence, so there is no click.
            assert!(buf[0].abs() < 1e-3, "{cue:?} começa com estalo");
            assert!(
                buf[buf.len() - 1].abs() < 1e-2,
                "{cue:?} termina com estalo"
            );
        }
        assert!(Cue::None.shape().is_none());
    }

    #[test]
    fn lengths_follow_the_notes() {
        let join = render(&Cue::Join.shape().unwrap());
        // 0.02 start + 0.085 second note + 0.32 decay + 0.03 tail.
        let expected = ((0.02 + 0.085 + 0.32 + 0.03) * 48_000f32).ceil() as usize;
        assert_eq!(join.len(), expected);
    }

    #[test]
    fn the_triangle_matches_web_audio() {
        assert!((wave(Wave::Triangle, 0.0)).abs() < 1e-6);
        assert!((wave(Wave::Triangle, 0.25) - 1.0).abs() < 1e-6);
        assert!((wave(Wave::Triangle, 0.5)).abs() < 1e-6);
        assert!((wave(Wave::Triangle, 0.75) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn turning_cues_off_is_remembered_by_key() {
        let s = Arc::new(Sounds {
            sink: Mutex::new(None),
            opened: Condvar::new(),
            broken: Arc::new(AtomicBool::new(false)),
            // As if already opening, so the test never touches a device.
            opening: AtomicBool::new(true),
            off: Mutex::new(HashSet::new()),
            rendered: Mutex::new(HashMap::new()),
        });
        assert_eq!(s.enabled_count(), 10);
        s.set_enabled(Cue::Join, false);
        s.set_enabled(Cue::Message, false);
        assert!(!s.enabled(Cue::Join));
        assert_eq!(s.off_list(), ["join", "message"]);
        s.set_enabled(Cue::Join, true);
        assert_eq!(s.off_list(), ["message"]);
        assert_eq!(s.enabled_count(), 9);
        // No device here: playing is a no-op, not a crash.
        s.play_always(Cue::Join);
    }
}

//! Video in a call: the camera or the screen, encoded as H.264 by openh264
//! on the processor (screens and windows go through the graphics card when
//! it can, hw.rs), and the pictures that arrive from the others, in H.264
//! or AV1 (av1.rs), decoded into textures for the call view.
//!
//! What I show goes out in up to two copies (plan.rs): a small one for the
//! tiles and one at the chosen quality, each made only while someone wants
//! it and at the bitrate the call allots it.
//!
//! Capture, encoding and decoding each run on threads of their own, so a
//! slow picture never holds up the sound or the interface. The encoder only
//! ever takes the newest picture, which drops frames instead of falling
//! behind when the machine is busy.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use egui::load::SizedTexture;
use egui::{ColorImage, TextureHandle, TextureOptions};
use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::YUVBuffer;
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

use super::plan::{Codec, Copies, Layer, Spec};
use super::sources::Source;
use crate::model::Video;
use crate::pictures::Pictures;

/// One encoded picture of one copy, and how long it stays on screen:
/// an H.264 access unit (Annex B), or an AV1 temporal unit (its OBUs, each
/// with its size).
pub struct EncodedFrame {
    pub layer: Layer,
    pub codec: Codec,
    /// A full picture (IDR, key frame): someone can start watching this
    /// copy here.
    pub key: bool,
    pub data: Vec<u8>,
    pub duration: Duration,
    /// When the picture was captured.
    pub captured: Instant,
}

/// What the call wants of the encoder, per copy: whether to make it, at
/// what bitrate, and that a full picture is due; and in which codec. Set
/// by rtc.rs, read by the encoder's thread.
///
/// The card's encoder thread (hw.rs) sleeps until something it acts on
/// changes, and this is where it is woken: by a change here, a new
/// picture of the screen, my own tile drawn again, or the end of the
/// share (`ring`).
#[derive(Default)]
pub struct LayerControl {
    on: [AtomicBool; 2],
    key: [AtomicBool; 2],
    kbps: [AtomicU32; 2],
    /// The codec the call wants (`Codec as u8`).
    codec: AtomicU8,
    /// The encoder running now makes AV1 too (the card's, hw.rs).
    av1: AtomicBool,
    /// How many times it rang: a ring is never missed between two waits.
    rang: Mutex<u64>,
    bell: Condvar,
}

impl LayerControl {
    /// Wakes the encoder's thread: something it acts on changed.
    pub fn ring(&self) {
        *self.rang.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.bell.notify_all();
    }

    /// How many times it rang so far, for the first `wait`.
    pub(super) fn rings(&self) -> u64 {
        *self.rang.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sleeps until it rings again after `heard` rings (at once if it did
    /// already), or until `until`, if anything comes due then. Returns the
    /// rings heard.
    pub(super) fn wait(&self, heard: u64, until: Option<Instant>) -> u64 {
        let rang = self.rang.lock().unwrap_or_else(PoisonError::into_inner);
        let rang = match until {
            Some(until) => {
                let left = until.saturating_duration_since(Instant::now());
                self.bell
                    .wait_timeout_while(rang, left, |n| *n == heard)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => self
                .bell
                .wait_while(rang, |n| *n == heard)
                .unwrap_or_else(PoisonError::into_inner),
        };
        *rang
    }

    pub fn set_codec(&self, codec: Codec) {
        if self.codec.swap(codec as u8, Ordering::Relaxed) != codec as u8 {
            self.ring();
        }
    }

    pub fn codec(&self) -> Codec {
        if self.codec.load(Ordering::Relaxed) == Codec::Av1 as u8 {
            Codec::Av1
        } else {
            Codec::H264
        }
    }

    /// Set by whichever encoder runs: whether it can make AV1.
    pub fn set_encodes_av1(&self, yes: bool) {
        self.av1.store(yes, Ordering::Relaxed);
    }

    pub fn encodes_av1(&self) -> bool {
        self.av1.load(Ordering::Relaxed)
    }

    /// A new bitrate is taken with the next picture; a copy turned on or
    /// off wakes the encoder.
    pub fn set(&self, layer: Layer, on: bool, kbps: u32) {
        if let Some(i) = layer.slot() {
            self.kbps[i].store(kbps, Ordering::Relaxed);
            if self.on[i].swap(on, Ordering::Relaxed) != on {
                self.ring();
            }
        }
    }

    pub fn is_on(&self, layer: Layer) -> bool {
        layer
            .slot()
            .is_some_and(|i| self.on[i].load(Ordering::Relaxed))
    }

    pub fn kbps(&self, layer: Layer) -> u32 {
        layer
            .slot()
            .map_or(0, |i| self.kbps[i].load(Ordering::Relaxed))
    }

    /// Someone needs a full picture of this copy (they just started
    /// watching it, or lost part of one).
    pub fn want_key(&self, layer: Layer) {
        if let Some(i) = layer.slot() {
            self.key[i].store(true, Ordering::Relaxed);
            self.ring();
        }
    }

    /// Nothing is wanted (the call ended, or the video stopped).
    pub fn reset(&self) {
        for i in 0..2 {
            self.on[i].store(false, Ordering::Relaxed);
            self.key[i].store(false, Ordering::Relaxed);
        }
        self.ring();
    }

    /// Takes the request for a full picture of copy `i`, if there is one.
    pub(super) fn key_due(&self, i: usize) -> bool {
        self.key[i].swap(false, Ordering::Relaxed)
    }

    /// Whether a full picture of copy `i` is asked for (left for
    /// `key_due` to take).
    pub(super) fn key_asked(&self, i: usize) -> bool {
        self.key[i].load(Ordering::Relaxed)
    }
}

/// The newest picture of everyone in the call, as textures.
pub struct VideoShared {
    ctx: egui::Context,
    frames: Mutex<HashMap<i64, TextureHandle>>,
    /// Pictures decoded from each person, and the size of the last one.
    seen: Mutex<HashMap<i64, (u64, [usize; 2])>>,
    /// Measuring: when the newest picture of each person was stamped,
    /// until the call view shows it.
    stamps: Mutex<HashMap<i64, f64>>,
    /// The newest picture of each person, until the call view draws it.
    /// Shared with the thread that made it, which writes the next picture
    /// over it once egui has uploaded it (see `Pictures`).
    pending: Mutex<HashMap<i64, Arc<ColorImage>>>,
    /// When [`Self::frame`] was last asked for each uid, so the sender
    /// knows whether the preview is still being drawn (another channel
    /// open, the local tile hidden by an enlarged peer: the preview path
    /// stops downscaling for nobody).
    wanted_at: Mutex<HashMap<i64, Instant>>,
    /// Pictures that stay on the graphics card (my own tile, shared with
    /// egui's device: interop.rs).
    gpu: Mutex<HashMap<i64, super::interop::Tile>>,
}

impl VideoShared {
    pub fn new(ctx: egui::Context) -> Arc<VideoShared> {
        Arc::new(VideoShared {
            ctx,
            frames: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            stamps: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            wanted_at: Mutex::new(HashMap::new()),
            gpu: Mutex::new(HashMap::new()),
        })
    }

    fn count_seen(&self, uid: i64, size: [usize; 2]) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = seen.entry(uid).or_insert((0, [0, 0]));
        entry.0 += 1;
        entry.1 = size;
    }

    /// Lets `uid`'s pictures on the card go, if there were any.
    fn drop_gpu(&self, uid: i64) {
        let tile = self
            .gpu
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        if let Some(tile) = tile {
            tile.free();
        }
    }

    /// Whether the UI asked for `uid`'s picture within the last `within`.
    /// Used by the sender to skip the preview when nobody is looking.
    pub fn wanted_recently(&self, uid: i64, within: Duration) -> bool {
        self.wanted_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&uid)
            .is_some_and(|at| at.elapsed() < within)
    }

    /// Asks the interface for a frame.
    pub fn wake(&self) {
        self.ctx.request_repaint();
    }

    /// Measuring: the call view drew `uid`'s newest picture; the first time
    /// for a picture, how long since it was stamped.
    pub fn shown(&self, uid: i64) {
        if !crate::probe::on() {
            return;
        }
        let stamp = self
            .stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        if let Some(at) = stamp {
            crate::probe::record("tela.na_tela", crate::probe::wall_ms() - at);
        }
    }

    /// How many pictures arrived from `uid`, and the size of the last.
    pub fn seen(&self, uid: i64) -> (u64, Option<[usize; 2]>) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&uid)
            .map_or((0, None), |(n, size)| (*n, Some(*size)))
    }

    /// `uid`'s texture, for the frame being drawn. The newest picture that
    /// arrived goes into it now, and only now: pictures nobody draws are
    /// never uploaded, and at most one per frame is. Same size, it is
    /// written over the texture there is (a new size makes a new one).
    /// Pictures that stay on the card are drawn from there.
    pub fn frame(&self, uid: i64) -> Option<SizedTexture> {
        self.wanted_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(uid, Instant::now());
        if let Some(tile) = self
            .gpu
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&uid)
        {
            return tile.frame(self.ctx.cumulative_frame_nr());
        }
        let newest = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        let mut frames = self.frames.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(img) = newest {
            match frames.get_mut(&uid) {
                Some(t) if t.size() == img.size => {
                    t.set_partial([0, 0], img, TextureOptions::LINEAR)
                }
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => {
                    let t =
                        self.ctx
                            .load_texture(format!("video:{uid}"), img, TextureOptions::LINEAR);
                    frames.insert(uid, t);
                }
            }
        }
        frames.get(&uid).map(SizedTexture::from_handle)
    }

    /// A picture that stays on the card (interop.rs).
    pub fn put_gpu(&self, uid: i64, frame: super::interop::Frame) {
        self.count_seen(uid, frame.ring.size);
        // What came the other way before is stale now.
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        self.gpu
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(uid)
            .or_default()
            .put(frame);
        self.ctx.request_repaint();
    }

    /// `stamp`: measuring, when the picture was stamped by its sender.
    pub fn put(&self, uid: i64, img: Arc<ColorImage>, stamp: Option<f64>) {
        self.count_seen(uid, img.size);
        // Pictures of it on the card are stale now (my preview went back
        // to memory).
        self.drop_gpu(uid);
        if let Some(at) = stamp {
            self.stamps
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(uid, at);
        }
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(uid, img);
        self.ctx.request_repaint();
    }

    pub fn forget(&self, uid: i64) {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        self.wanted_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uid);
        self.drop_gpu(uid);
    }

    pub fn clear(&self) {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.wanted_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        let tiles: Vec<_> = self
            .gpu
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, t)| t)
            .collect();
        for tile in tiles {
            tile.free();
        }
    }
}

// ---------------------------------------------------------------- sending

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Layout {
    Bgra,
    Rgb,
}

/// A picture as it came from the camera or the screen.
pub(super) struct Raw {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) data: Vec<u8>,
    pub(super) layout: Layout,
    /// When it arrived.
    pub(super) at: Instant,
}

/// Holds the newest picture until the encoder takes it.
#[derive(Default)]
pub(super) struct Slot {
    frame: Mutex<Option<Raw>>,
    ready: Condvar,
    stop: AtomicBool,
    /// What was being shared is gone (the window closed).
    ended: AtomicBool,
    /// Buffers of pictures the encoder is done with, for the next ones. A
    /// screen picture is 3 to 8 MB, and Windows hands a block that big out
    /// as fresh pages each time: allocating one per picture made every
    /// picture pay for thousands of page faults.
    spare: Mutex<Vec<Vec<u8>>>,
}

impl Slot {
    pub(super) fn put(&self, raw: Raw) {
        let replaced = self
            .frame
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(raw);
        self.ready.notify_one();
        if let Some(old) = replaced {
            self.give_back(old.data);
        }
    }

    /// A buffer for the next picture: one given back, or a new one.
    pub(super) fn buffer(&self) -> Vec<u8> {
        self.spare
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_default()
    }

    fn give_back(&self, data: Vec<u8>) {
        let mut spare = self.spare.lock().unwrap_or_else(PoisonError::into_inner);
        // The capture, the slot and the encoder hold one each at most.
        if spare.len() < 2 {
            spare.push(data);
        }
    }

    /// The next picture, or `None` after `wait` without one.
    fn take(&self, wait: Duration) -> Option<Raw> {
        let guard = self.frame.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut guard, _) = self
            .ready
            .wait_timeout_while(guard, wait, |f| {
                f.is_none() && !self.stop.load(Ordering::Relaxed)
            })
            .unwrap_or_else(PoisonError::into_inner);
        guard.take()
    }

    pub(super) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Video going out; dropping it stops the capture and the encoder.
pub struct Sending {
    slot: Arc<Slot>,
    screen: Option<windows_capture::capture::CaptureControl<Screen, BoxError>>,
    /// Measuring: the screen through Desktop Duplication instead.
    _dxgi: Option<super::dxgi::Duplication>,
    /// The screen captured and encoded on the graphics card (hw.rs).
    zero: Option<super::hw::Zero>,
    /// What it was started with, to carry on through the processor.
    again: Again,
}

/// What a share was started with.
#[derive(Clone)]
struct Again {
    source: Source,
    copies: Copies,
    out: UnboundedSender<EncodedFrame>,
    control: Arc<LayerControl>,
    preview: Arc<VideoShared>,
    me: i64,
}

impl Sending {
    /// Whether what was being shared went away on its own (a window that
    /// was closed): time to stop and say so.
    pub fn ended(&self) -> bool {
        self.slot.ended.load(Ordering::Relaxed)
            || self.zero.as_ref().is_some_and(super::hw::Zero::ended)
    }

    /// The card stopped encoding in the middle of the share (its driver
    /// reset, say): the share carries on through the processor, with the
    /// same source, copies and tracks. The new encoders start with a full
    /// picture, so those watching see a moment's pause. None when there
    /// was nothing to do.
    pub fn heal(&mut self) -> Option<Result<(), String>> {
        if !self.zero.as_ref().is_some_and(super::hw::Zero::failed) {
            return None;
        }
        Some(start_cpu(self.again.clone()).map(|next| *self = next))
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        self.slot.stop.store(true, Ordering::Relaxed);
        self.slot.ready.notify_all();
        if let Some(control) = self.screen.take() {
            let _ = control.stop();
        }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Starts capturing `source` and making the copies `control` asks for.
/// Pictures go to `out`; a small view of what is being sent shows as `me`
/// in the call view. A screen or a window is captured and encoded on the
/// graphics card (hw.rs) when it can be, through the processor otherwise.
pub fn start(
    source: Source,
    copies: Copies,
    out: UnboundedSender<EncodedFrame>,
    control: Arc<LayerControl>,
    preview: Arc<VideoShared>,
    me: i64,
) -> Result<Sending, String> {
    let again = Again {
        source,
        copies,
        out,
        control,
        preview,
        me,
    };
    let card = match source {
        Source::Camera => false,
        // Measuring, the smoke test picks the path (hw.rs `WANTED`).
        Source::Monitor(_) => !crate::probe::on() || super::hw::WANTED.load(Ordering::Relaxed),
        Source::Window(_) => true,
    };
    if card {
        if matches!(source, Source::Window(_)) && !source.exists() {
            return Err("a janela foi fechada".into());
        }
        // Falls through to the processor when the card has no encoder,
        // something in its setup fails, or BABUTELA_SEM_PLACA is set.
        match super::hw::start(
            source,
            copies,
            again.out.clone(),
            again.control.clone(),
            again.preview.clone(),
            me,
        ) {
            Ok(zero) => {
                return Ok(Sending {
                    slot: Arc::new(Slot::default()),
                    screen: None,
                    _dxgi: None,
                    zero: Some(zero),
                    again,
                });
            }
            Err(e) => crate::logln!("vídeo: na placa não deu ({e}), vai pelo processador"),
        }
    }
    start_cpu(again)
}

/// The capture on this side and openh264 on a thread of its own.
fn start_cpu(again: Again) -> Result<Sending, String> {
    let Again {
        source,
        copies,
        ref out,
        ref control,
        ref preview,
        me,
    } = again;
    let slot = Arc::new(Slot::default());
    let kind = source.kind();
    // openh264 makes H.264 only: said before the call hears of this video
    // (the card's share before it may have said AV1).
    control.set_encodes_av1(false);
    // The capture runs at the big copy's size and rate; the small one is
    // made from the same pictures.
    let spec = copies.high;
    let mut dxgi = None;
    let screen = match source {
        Source::Camera => {
            start_camera(slot.clone(), spec)?;
            None
        }
        Source::Monitor(m) if crate::probe::on() && super::dxgi::WANTED.load(Ordering::Relaxed) => {
            dxgi = Some(super::dxgi::start(m, slot.clone(), spec.fps)?);
            None
        }
        Source::Monitor(m) => Some(start_screen(
            windows_capture::monitor::Monitor::from_raw_hmonitor(m as _),
            slot.clone(),
            spec,
        )?),
        Source::Window(w) => {
            if !source.exists() {
                return Err("a janela foi fechada".into());
            }
            Some(start_screen(
                windows_capture::window::Window::from_raw_hwnd(w as _),
                slot.clone(),
                spec,
            )?)
        }
    };
    let encoder_slot = slot.clone();
    let (out, control, preview) = (out.clone(), control.clone(), preview.clone());
    std::thread::Builder::new()
        .name("video-encode".into())
        .spawn(move || encode_loop(&encoder_slot, kind, copies, &out, &control, &preview, me))
        .map_err(|e| e.to_string())?;
    Ok(Sending {
        slot,
        screen,
        _dxgi: dxgi,
        zero: None,
        again,
    })
}

// ------------------------------------------------ the screen

struct Screen {
    slot: Arc<Slot>,
    /// The CPU-readable copy of the picture and its size, kept from one
    /// picture to the next: windows-capture's `Frame::buffer` makes a new
    /// one for every picture (a driver allocation, mapped afresh).
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl windows_capture::capture::GraphicsCaptureApiHandler for Screen {
    type Flags = Arc<Slot>;
    type Error = BoxError;

    fn new(ctx: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Screen {
            slot: ctx.flags,
            staging: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        control: windows_capture::graphics_capture_api::InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.slot.stopped() {
            control.stop();
            return Ok(());
        }
        let reading = Instant::now();
        let mut data = self.slot.buffer();
        read_frame(frame, &mut self.staging, &mut data)?;
        crate::probe::since("video.captura", reading);
        self.slot.put(Raw {
            width: frame.width() as usize,
            height: frame.height() as usize,
            data,
            layout: Layout::Bgra,
            at: Instant::now(),
        });
        Ok(())
    }

    /// The shared window was closed (screens do not close).
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.slot.ended.store(true, Ordering::Relaxed);
        self.slot.ready.notify_all();
        Ok(())
    }
}

/// Copies a captured picture (BGRA, the capture's format) into `data`, row
/// by row without the padding, through the staging texture in `staging`
/// (made again only when the size changes).
fn read_frame(
    frame: &windows_capture::frame::Frame,
    staging: &mut Option<(ID3D11Texture2D, u32, u32)>,
    data: &mut Vec<u8>,
) -> windows::core::Result<()> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    let (width, height) = (frame.width(), frame.height());
    if !staging
        .as_ref()
        .is_some_and(|(_, w, h)| (*w, *h) == (width, height))
    {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: frame.desc().Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a plain description, and an out pointer to a local.
        unsafe {
            frame
                .device()
                .CreateTexture2D(&desc, None, Some(&mut texture))?
        };
        *staging = texture.map(|t| (t, width, height));
    }
    let Some((texture, _, _)) = staging.as_ref() else {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_FAIL,
        ));
    };
    let context = frame.device_context();
    let row = width as usize * 4;
    // SAFETY: the staging texture has the picture's size and format, and
    // runs on the capture's thread, the one its context belongs to. While
    // mapped, Windows guarantees `height` rows of `RowPitch` bytes at pData,
    // each holding at least `row` bytes of pixels.
    unsafe {
        context.CopyResource(texture, frame.as_raw_texture());
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        data.clear();
        data.reserve(row * height as usize);
        let base = mapped.pData.cast::<u8>().cast_const();
        for y in 0..height as usize {
            let line = std::slice::from_raw_parts(base.add(y * mapped.RowPitch as usize), row);
            data.extend_from_slice(line);
        }
        context.Unmap(texture, 0);
    }
    Ok(())
}

/// Captures a screen or a window (`item`) with Windows Graphics Capture.
fn start_screen<T>(
    item: T,
    slot: Arc<Slot>,
    spec: Spec,
) -> Result<windows_capture::capture::CaptureControl<Screen, BoxError>, String>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType> + Copy + Send + 'static,
{
    use windows_capture::capture::GraphicsCaptureApiHandler;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };
    // A little under the frame interval: the screen updates on its own
    // refresh, so asking for exactly 33 ms on a 60 Hz screen gets 33 or
    // 50 ms (25 fps); the encoder keeps its own pace from the extra ones.
    let interval = Duration::from_millis((750 / u64::from(spec.fps.max(1))).max(1));
    // Newer Windows lets the capture drop the yellow border and keep to a
    // frame rate; older Windows 10 refuses both, so try plain settings next.
    let attempts = [
        (
            DrawBorderSettings::WithoutBorder,
            MinimumUpdateIntervalSettings::Custom(interval),
        ),
        (
            DrawBorderSettings::Default,
            MinimumUpdateIntervalSettings::Default,
        ),
    ];
    let mut last = String::new();
    for (border, rate) in attempts {
        let settings = Settings::new(
            item,
            CursorCaptureSettings::WithCursor,
            border,
            SecondaryWindowSettings::Default,
            rate,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            slot.clone(),
        );
        match Screen::start_free_threaded(settings) {
            Ok(control) => return Ok(control),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

// ------------------------------------------------ the camera

fn start_camera(slot: Arc<Slot>, spec: Spec) -> Result<(), String> {
    use nokhwa::Camera;
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
        Resolution,
    };
    let (opened_tx, opened_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
            // What the chosen quality asks for; the camera picks the
            // nearest it can do (a 4K or 60 fps one gets to use it).
            let wanted = CameraFormat::new(
                Resolution::new(spec.width, spec.height),
                FrameFormat::MJPEG,
                spec.fps,
            );
            let request = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(wanted));
            let camera =
                Camera::with_backend(CameraIndex::Index(0), request, ApiBackend::MediaFoundation)
                    .and_then(|mut c| c.open_stream().map(|()| c));
            let mut camera = match camera {
                Ok(c) => {
                    let _ = opened_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = opened_tx.send(Err(e.to_string()));
                    return;
                }
            };
            while !slot.stopped() {
                let Ok(buffer) = camera.frame() else {
                    std::thread::sleep(Duration::from_millis(30));
                    continue;
                };
                let Ok(rgb) = buffer.decode_image::<RgbFormat>() else {
                    continue;
                };
                let (width, height) = (rgb.width() as usize, rgb.height() as usize);
                slot.put(Raw {
                    width,
                    height,
                    data: rgb.into_raw(),
                    layout: Layout::Rgb,
                    at: Instant::now(),
                });
            }
            let _ = camera.stop_stream();
        })
        .map_err(|e| e.to_string())?;
    opened_rx
        .recv_timeout(Duration::from_secs(8))
        .map_err(|_| "a câmera não respondeu".to_owned())?
}

// ------------------------------------------------ encoding

/// The largest size that fits the picture inside `max_w` x `max_h`, with
/// the width a multiple of 8 (what the fast colour conversions on both
/// ends need) and an even height (what I420 needs).
pub fn fit(width: usize, height: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    let scale = (max_w as f64 / width as f64)
        .min(max_h as f64 / height as f64)
        .min(1.0);
    let w = ((width as f64 * scale) as usize) & !7;
    let h = ((height as f64 * scale) as usize) & !1;
    (w.max(8), h.max(2))
}

/// The first `width` x `height` pixels of a picture whose rows are
/// `stride` pixels long: a picture a few columns too wide for `fit` is
/// cropped this way, for nothing, instead of being scaled.
struct View<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
    layout: Layout,
}

impl View<'_> {
    fn bpp(&self) -> usize {
        match self.layout {
            Layout::Bgra => 4,
            Layout::Rgb => 3,
        }
    }
}

impl openh264::formats::RGBSource for View<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        use openh264::formats::RGB8Source;
        let at = (y * self.stride + x) * self.bpp();
        let (r, g, b) = self.rgb_channel_offsets();
        (
            self.data[at + r].into(),
            self.data[at + g].into(),
            self.data[at + b].into(),
        )
    }
}

impl openh264::formats::RGB8Source for View<'_> {
    fn dimensions_padded(&self) -> (usize, usize) {
        (self.stride, self.data.len() / (self.stride * self.bpp()))
    }

    fn rgb8_data(&self) -> &[u8] {
        self.data
    }

    fn pixel_stride(&self) -> usize {
        self.bpp()
    }

    fn rgb_channel_offsets(&self) -> (usize, usize, usize) {
        match self.layout {
            Layout::Bgra => (2, 1, 0),
            Layout::Rgb => (0, 1, 2),
        }
    }
}

/// Resizes a Vec to exactly `len` bytes without zeroing. The caller must
/// write every byte before reading, since the memory may hold values from
/// a previous use. Saves the `memset` of large buffers (a 4K BGRA halving
/// pass is 8 MB; the scalar `resize(len, 0)` cost ~1 ms per picture).
fn reset_buffer(buf: &mut Vec<u8>, len: usize) {
    buf.clear();
    buf.reserve_exact(len);
    // SAFETY: capacity() >= len (just reserved), and the halving and
    // nearest-neighbor passes below write every byte before anything reads
    // from the buffer.
    unsafe {
        buf.set_len(len);
    }
}

/// Halves a BGRA (or RGBA) picture into `out` by averaging 2x2 blocks.
/// Processes 8 output pixels per iteration (32 bytes); `w` must be at
/// least 16 (so one iteration fits), and `w` divisible by 16 avoids a
/// scalar tail.
///
/// # Safety
///
/// AVX2 must be available on the CPU, `src` must have `w * h * 4` bytes
/// and `out` at least `(w / 2) * (h / 2) * 4` bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn halve_bgra_avx2(src: &[u8], w: usize, h: usize, out: &mut [u8]) {
    use std::arch::x86_64::{
        __m256i, _mm256_avg_epu8, _mm256_loadu_si256, _mm256_permute2x128_si256,
        _mm256_permute4x64_epi64, _mm256_setr_epi8, _mm256_shuffle_epi8, _mm256_storeu_si256,
    };
    let (dw, dh) = (w / 2, h / 2);
    let src_row = w * 4;
    let dst_row = dw * 4;
    // Within each 128-bit lane (four BGRA pixels), gather the two even
    // pixels (bytes 0-3 and 8-11) into the low 8 bytes; the odd ones
    // (bytes 4-7 and 12-15) likewise. The high 8 bytes of each lane are
    // zeroed by the -1 entries.
    let mask_even = _mm256_setr_epi8(
        0, 1, 2, 3, 8, 9, 10, 11, -1, -1, -1, -1, -1, -1, -1, -1, 0, 1, 2, 3, 8, 9, 10, 11, -1, -1,
        -1, -1, -1, -1, -1, -1,
    );
    let mask_odd = _mm256_setr_epi8(
        4, 5, 6, 7, 12, 13, 14, 15, -1, -1, -1, -1, -1, -1, -1, -1, 4, 5, 6, 7, 12, 13, 14, 15, -1,
        -1, -1, -1, -1, -1, -1, -1,
    );
    for y in 0..dh {
        // SAFETY: 2*y+1 < h since y < dh = h/2; multiplications fit in
        // usize for any reasonable picture.
        let row0 = unsafe { src.as_ptr().add(2 * y * src_row) };
        let row1 = unsafe { src.as_ptr().add((2 * y + 1) * src_row) };
        let dst = unsafe { out.as_mut_ptr().add(y * dst_row) };
        let mut x = 0;
        while x + 8 <= dw {
            // SAFETY: each load reads 32 bytes starting at `2*x*4` and
            // `2*x*4 + 32` within the row, which is `src_row = w * 4` long;
            // `2*x + 16 <= 2*dw = w`, so the loads are in bounds. The
            // stores write 32 bytes at `x * 4`, with `x + 8 <= dw`, in bounds.
            unsafe {
                let a0 = _mm256_loadu_si256(row0.add(x * 8).cast::<__m256i>());
                let a1 = _mm256_loadu_si256(row0.add(x * 8 + 32).cast::<__m256i>());
                let b0 = _mm256_loadu_si256(row1.add(x * 8).cast::<__m256i>());
                let b1 = _mm256_loadu_si256(row1.add(x * 8 + 32).cast::<__m256i>());
                // Vertical pair average: each result holds 8 BGRA pixels
                // averaged with the pixel below.
                let v0 = _mm256_avg_epu8(a0, b0);
                let v1 = _mm256_avg_epu8(a1, b1);
                // Horizontal pair average: shuffle even and odd pixels
                // apart, then average, giving two output pixels per 128-bit
                // lane (in the low 8 bytes of each).
                let h0 = _mm256_avg_epu8(
                    _mm256_shuffle_epi8(v0, mask_even),
                    _mm256_shuffle_epi8(v0, mask_odd),
                );
                let h1 = _mm256_avg_epu8(
                    _mm256_shuffle_epi8(v1, mask_even),
                    _mm256_shuffle_epi8(v1, mask_odd),
                );
                // Pack the two valid pixels of each 128-bit lane into one
                // contiguous 128-bit half: the 64-bit chunks of `h0` are
                // (lane0_low, lane0_high, lane1_low, lane1_high); picking
                // 0 and 2 gives (lane0_low, lane1_low) in the low 128 bits.
                let p0 = _mm256_permute4x64_epi64::<0b_00_00_10_00>(h0);
                let p1 = _mm256_permute4x64_epi64::<0b_00_00_10_00>(h1);
                // Final 256-bit vector: p0's low lane (4 pixels) then p1's
                // low lane (4 more). 0x20 picks low lane of each operand.
                let result = _mm256_permute2x128_si256::<0x20>(p0, p1);
                _mm256_storeu_si256(dst.add(x * 4).cast::<__m256i>(), result);
            }
            x += 8;
        }
        // Scalar tail for widths not a multiple of 16 (16 input bytes per
        // output pixel: 2 input pixels × 2 rows × 4 bpp).
        while x < dw {
            // SAFETY: 2*x+1 < w and 2*y+1 < h by the loop bounds.
            unsafe {
                for c in 0..4 {
                    let p = |dx: usize, dy: usize| -> u32 {
                        u32::from(*src.get_unchecked((2 * y + dy) * src_row + (2 * x + dx) * 4 + c))
                    };
                    *out.get_unchecked_mut(y * dst_row + x * 4 + c) =
                        ((p(0, 0) + p(1, 0) + p(0, 1) + p(1, 1) + 2) / 4) as u8;
                }
            }
            x += 1;
        }
    }
}

/// The scalar halving, kept as a fallback for RGB (3 bpp, from the camera)
/// and for CPUs without AVX2.
fn halve_scalar(src: &[u8], w: usize, h: usize, bpp: usize, out: &mut [u8]) {
    let (nw, nh) = (w / 2, h / 2);
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..bpp {
                let p = |dx: usize, dy: usize| {
                    u32::from(src[((2 * y + dy) * w + 2 * x + dx) * bpp + c])
                };
                out[(y * nw + x) * bpp + c] =
                    ((p(0, 0) + p(1, 0) + p(0, 1) + p(1, 1) + 2) / 4) as u8;
            }
        }
    }
}

/// Dispatches a 2:1 halve to the best path for this CPU and pixel format.
fn halve_into(src: &[u8], w: usize, h: usize, bpp: usize, out: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    if bpp == 4 && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is confirmed by the runtime check; the slice sizes
        // are guaranteed by `scale_into` (reset_buffer sized `out` to
        // `(w/2) * (h/2) * 4`, and the caller passed a `src` of `w*h*4`).
        unsafe {
            halve_bgra_avx2(src, w, h, out);
        }
        return;
    }
    halve_scalar(src, w, h, bpp, out);
}

/// Resizes a picture with `bpp` bytes per pixel into `out`. Halving first
/// keeps text readable; what is left is done by picking the nearest pixel.
/// `out` and `tmp` keep their memory from one picture to the next.
#[allow(clippy::too_many_arguments)]
fn scale_into(
    data: &[u8],
    width: usize,
    height: usize,
    bpp: usize,
    tw: usize,
    th: usize,
    out: &mut Vec<u8>,
    tmp: &mut Vec<u8>,
) {
    let (mut w, mut h) = (width, height);
    // Where the picture is now: still in `data`, or in `tmp`.
    let mut halved = false;
    while w >= tw * 2 && h >= th * 2 {
        let (nw, nh) = (w / 2, h / 2);
        reset_buffer(out, nw * nh * bpp);
        let src: &[u8] = if halved { &tmp[..] } else { data };
        halve_into(src, w, h, bpp, out);
        std::mem::swap(out, tmp);
        halved = true;
        (w, h) = (nw, nh);
    }
    if w == tw && h == th {
        if halved {
            std::mem::swap(out, tmp);
        } else {
            out.clear();
            out.extend_from_slice(&data[..w * h * bpp]);
        }
        return;
    }
    let src: &[u8] = if halved { &tmp[..] } else { data };
    reset_buffer(out, tw * th * bpp);
    // Each row picks the same source columns, so compute the byte offsets
    // once instead of a division per output pixel (tw divisions a row,
    // th rows: the inner loop was ~1 ms of its own at 640x360).
    let src_cols: Vec<usize> = (0..tw).map(|x| ((x * w / tw).min(w - 1)) * bpp).collect();
    let src_stride = w * bpp;
    let dst_stride = tw * bpp;
    for y in 0..th {
        let sy = (y * h / th).min(h - 1);
        let src_row = &src[sy * src_stride..sy * src_stride + src_stride];
        let dst_row = &mut out[y * dst_stride..y * dst_stride + dst_stride];
        for (x, &sxb) in src_cols.iter().enumerate() {
            dst_row[x * bpp..x * bpp + bpp].copy_from_slice(&src_row[sxb..sxb + bpp]);
        }
    }
}

/// Buffers the preview keeps between pictures.
struct Preview {
    pictures: Pictures,
    small: Vec<u8>,
    tmp: Vec<u8>,
}

impl Preview {
    fn new() -> Preview {
        Preview {
            // The one in the tile, and the one being written.
            pictures: Pictures::new(3),
            small: Vec::new(),
            tmp: Vec::new(),
        }
    }
}

/// Reorders a BGRA buffer into egui's RGBA Color32 pixels. Processes 8
/// pixels (32 bytes) per iteration with one shuffle; the input and output
/// have the same number of pixels.
///
/// # Safety
///
/// AVX2 must be available on the CPU, `src` must have at least
/// `dst.len() * 4` bytes, and `dst` must have `src.len() / 4` entries.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bgra_to_color32_avx2(src: &[u8], dst: &mut [egui::Color32]) {
    use std::arch::x86_64::{
        __m256i, _mm256_loadu_si256, _mm256_setr_epi8, _mm256_shuffle_epi8, _mm256_storeu_si256,
    };
    // Swap B and R inside each four-byte pixel: (B,G,R,A) → (R,G,B,A).
    let mask = _mm256_setr_epi8(
        2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11,
        14, 13, 12, 15,
    );
    let count = dst.len();
    let full = count / 8 * 8;
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr().cast::<u8>();
    let mut i = 0;
    while i < full {
        // SAFETY: i + 8 <= full <= count, so i*4 + 32 <= src.len() and
        // i*4 + 32 <= dst.len() * 4.
        unsafe {
            let px = _mm256_loadu_si256(src_ptr.add(i * 4).cast::<__m256i>());
            let swapped = _mm256_shuffle_epi8(px, mask);
            _mm256_storeu_si256(dst_ptr.add(i * 4).cast::<__m256i>(), swapped);
        }
        i += 8;
    }
    while i < count {
        // SAFETY: scalar tail, bounds checked by the loop condition.
        unsafe {
            let px = src.get_unchecked(i * 4..i * 4 + 4);
            *dst.get_unchecked_mut(i) = egui::Color32::from_rgb(px[2], px[1], px[0]);
        }
        i += 1;
    }
}

pub(super) fn bgra_to_color32(src: &[u8], dst: &mut [egui::Color32]) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 confirmed by the runtime check; the caller passes
        // slices sized by `preview_image` (the Pictures ring buffer holds
        // one Color32 per BGRA pixel of `scaled`).
        unsafe {
            bgra_to_color32_avx2(src, dst);
        }
        return;
    }
    let (chunks, _) = src.as_chunks::<4>();
    for (px, out) in chunks.iter().zip(dst.iter_mut()) {
        *out = egui::Color32::from_rgb(px[2], px[1], px[0]);
    }
}

/// A small copy for my own tile in the call view. [`Shared`] holds the
/// raw already downscaled from a step() of the small copy on the same
/// picture; the preview reuses it instead of downscaling again.
fn preview_image(raw: &Raw, p: &mut Preview, shared: &mut Shared) -> Arc<ColorImage> {
    let bpp = match raw.layout {
        Layout::Bgra => 4,
        Layout::Rgb => 3,
    };
    let (tw, th) = fit(raw.width, raw.height, 640, 360);
    let scaled: &[u8] = if let Some(bgra) = shared.bgra(raw, tw, th) {
        bgra
    } else {
        scale_into(
            &raw.data,
            raw.width,
            raw.height,
            bpp,
            tw,
            th,
            &mut p.small,
            &mut p.tmp,
        );
        &p.small
    };
    let mut img = p.pictures.take([tw, th]);
    if let Some(dst) = Arc::get_mut(&mut img) {
        match raw.layout {
            Layout::Bgra => bgra_to_color32(scaled, &mut dst.pixels),
            Layout::Rgb => {
                let (chunks, _) = scaled.as_chunks::<3>();
                for (px, out) in chunks.iter().zip(dst.pixels.iter_mut()) {
                    *out = egui::Color32::from_rgb(px[0], px[1], px[2]);
                }
            }
        }
    }
    p.pictures.keep(&img);
    img
}

/// An encoder for one copy. It starts at the copy's best bitrate, which is
/// also its ceiling, and is turned down from there as the call allots.
fn new_encoder(kind: Video, spec: Spec) -> Result<Encoder, String> {
    let usage = match kind {
        Video::Screen => UsageType::ScreenContentRealTime,
        Video::Camera => UsageType::CameraVideoRealTime,
    };
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(spec.max_kbps * 1000))
        .max_frame_rate(FrameRate::from_hz(spec.fps as f32))
        .usage_type(usage)
        .rate_control_mode(RateControlMode::Bitrate)
        .complexity(Complexity::Low)
        // Full pictures come when asked for (and every KEY_EVERY); the
        // encoder's own period is only a backstop.
        .intra_frame_period(IntraFramePeriod::from_num_frames(spec.fps * 10));
    // One slice, encoded on one thread: measured, four threads with slices
    // of one packet each only took a 1080p picture from 9.9 to 8.3 ms, and
    // made full pictures 7 % bigger.
    Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| e.to_string())
}

/// Changes a running encoder's bitrate.
fn set_bitrate(encoder: &mut Encoder, kbps: u32) {
    use openh264_sys2::{ENCODER_OPTION_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};
    let mut info = SBitrateInfo {
        iLayer: SPATIAL_LAYER_ALL,
        iBitrate: i32::try_from(kbps.saturating_mul(1000)).unwrap_or(i32::MAX),
    };
    // SAFETY: ENCODER_OPTION_BITRATE takes a pointer to an SBitrateInfo,
    // which lives through the call; the encoder is initialized (it has
    // encoded a picture before this is ever called).
    unsafe {
        let _ = encoder
            .raw_api()
            .set_option(ENCODER_OPTION_BITRATE, std::ptr::from_mut(&mut info).cast());
    }
}

/// A static screen sends nothing; repeating the last picture this often
/// lets someone who just arrived (or lost a packet) see it anyway.
pub(super) const REPEAT_AFTER: Duration = Duration::from_millis(500);

/// A full picture at least this often, so a loss nobody reported heals.
/// Losses are reported (and resent, NACK), so this is only a backstop:
/// each full picture is several times an ordinary one, and goes to
/// everyone receiving the copy at once.
pub(super) const KEY_EVERY: Duration = Duration::from_secs(30);

/// Full pictures asked for come at most this often per copy: each one goes
/// to everyone receiving the copy, and is several times a normal picture.
pub(super) const KEY_GAP: Duration = Duration::from_millis(800);

/// An encoder unused this long is let go.
pub(super) const IDLE: Duration = Duration::from_secs(15);

/// My own tile counts as looked at this long after the call view last drew
/// it (`VideoShared::frame`); after that, it is not made.
pub(super) const TILE_GRACE: Duration = Duration::from_millis(300);

/// A picture downscaled from the raw once per frame, shared between the
/// small copy and the preview of my own tile: both want the same 640x360
/// of the raw and would do one scale each. Shaped by the first caller
/// that wants it, cleared with `prepare`.
#[derive(Default)]
struct Shared {
    bgra: Vec<u8>,
    tmp: Vec<u8>,
    size: Option<(usize, usize)>,
}

impl Shared {
    /// Clears the cached picture (a new raw is in).
    fn prepare(&mut self) {
        self.size = None;
    }

    /// The raw scaled to `(tw, th)`, computed once. The layout stays what
    /// the raw was (BGRA or RGB); callers that want RGBA convert the
    /// pixels themselves.
    fn bgra(&mut self, raw: &Raw, tw: usize, th: usize) -> Option<&[u8]> {
        if raw.width < tw * 2 || raw.height < th * 2 {
            return None;
        }
        if self.size == Some((tw, th)) {
            return Some(&self.bgra);
        }
        let bpp = if raw.layout == Layout::Bgra { 4 } else { 3 };
        scale_into(
            &raw.data,
            raw.width,
            raw.height,
            bpp,
            tw,
            th,
            &mut self.bgra,
            &mut self.tmp,
        );
        self.size = Some((tw, th));
        Some(&self.bgra)
    }
}

/// One copy being made.
struct Encoding {
    layer: Layer,
    spec: Spec,
    encoder: Option<Encoder>,
    /// Picture size and bitrate the encoder was last set to.
    size: Option<(usize, usize)>,
    kbps: u32,
    /// It was being made at the last picture (a copy that starts again
    /// starts with a full picture).
    active: bool,
    idle_since: Instant,
    last_sent: Instant,
    last_key: Instant,
    /// When the next picture is due.
    due: Instant,
    /// The last picture's I420 buffer, reused while the size stays.
    yuv: Option<YUVBuffer>,
    /// The picture scaled down to this copy's size, and the halfway steps,
    /// kept from one picture to the next. Only used when the small copy's
    /// target does not match [`Shared`]'s one.
    scaled: Vec<u8>,
    tmp: Vec<u8>,
}

impl Encoding {
    fn new(layer: Layer, spec: Spec) -> Encoding {
        let long_ago = Instant::now() - Duration::from_secs(60);
        Encoding {
            layer,
            spec,
            encoder: None,
            size: None,
            kbps: 0,
            active: false,
            idle_since: Instant::now(),
            last_sent: long_ago,
            last_key: long_ago,
            due: long_ago,
            yuv: None,
            scaled: Vec::new(),
            tmp: Vec::new(),
        }
    }

    /// Encodes `raw` for this copy if the copy is wanted and due. `fresh`
    /// says the picture is new (a static screen repeats its last one every
    /// REPEAT_AFTER). `shared` holds the raw already downscaled to the
    /// small copy's size, when that is what this encoder wants too.
    /// Returns false once nobody listens any more.
    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        raw: &Raw,
        fresh: bool,
        kind: Video,
        control: &LayerControl,
        out: &UnboundedSender<EncodedFrame>,
        started: Instant,
        shared: &mut Shared,
    ) -> bool {
        let Some(slot) = self.layer.slot() else {
            return true;
        };
        if !control.is_on(self.layer) {
            if self.active {
                self.active = false;
                self.idle_since = Instant::now();
            }
            if self.encoder.is_some() && self.idle_since.elapsed() > IDLE {
                self.encoder = None;
                self.size = None;
            }
            return true;
        }
        let now = Instant::now();
        let since = now.saturating_duration_since(self.last_sent);
        let gap = Duration::from_secs_f64(1.0 / f64::from(self.spec.fps.max(1)));
        // Pictures are made at a steady pace. The capture runs at the same
        // rate with some jitter, so one that comes a little early still
        // counts (skipping it would halve the rate).
        let slack = gap / 4;
        if now + slack < self.due || (!fresh && since < REPEAT_AFTER) {
            return true;
        }
        self.due = (self.due + gap).max(now + gap / 2);
        let began = Instant::now();
        if fresh {
            crate::probe::record(
                "video.espera",
                began.saturating_duration_since(raw.at).as_secs_f64() * 1000.0,
            );
        }
        let (w, h) = fit(
            raw.width,
            raw.height,
            self.spec.width as usize,
            self.spec.height as usize,
        );
        let bpp = if raw.layout == Layout::Bgra { 4 } else { 3 };
        // A few columns (or a row) too many are cut off; more than that,
        // the picture is scaled down. The small copy and the preview want
        // the same 640x360 of the raw, so the first one to run leaves it
        // in `shared` for the next (shaped the same way at 60 fps, two
        // halvings of a 4K picture through the processor every picture).
        let crop = raw.width - w < 8 && raw.height - h < 2;
        let view = if crop {
            View {
                data: &raw.data,
                width: w,
                height: h,
                stride: raw.width,
                layout: raw.layout,
            }
        } else if let Some(bgra) = shared.bgra(raw, w, h) {
            View {
                data: bgra,
                width: w,
                height: h,
                stride: w,
                layout: raw.layout,
            }
        } else {
            scale_into(
                &raw.data,
                raw.width,
                raw.height,
                bpp,
                w,
                h,
                &mut self.scaled,
                &mut self.tmp,
            );
            View {
                data: &self.scaled,
                width: w,
                height: h,
                stride: w,
                layout: raw.layout,
            }
        };
        let yuv = match self.yuv.take() {
            Some(mut buf) if openh264::formats::YUVSource::dimensions(&buf) == (w, h) => {
                buf.read_rgb8(view);
                buf
            }
            _ => YUVBuffer::from_rgb8_source(view),
        };
        if self.encoder.is_none() {
            match new_encoder(kind, self.spec) {
                Ok(e) => self.encoder = Some(e),
                Err(e) => {
                    crate::logln!("vídeo: codificador: {e}");
                    return true;
                }
            }
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return true;
        };
        let kbps = control
            .kbps(self.layer)
            .clamp(self.spec.min_kbps, self.spec.max_kbps);
        // A new size starts the encoder over at its first bitrate, so the
        // allotted one is set again after that picture.
        let resized = self.size != Some((w, h));
        if !resized && kbps != self.kbps {
            set_bitrate(encoder, kbps);
            self.kbps = kbps;
        }
        let asked = self.last_key.elapsed() >= KEY_GAP && control.key_due(slot);
        if !self.active || asked || self.last_key.elapsed() >= KEY_EVERY {
            encoder.force_intra_frame();
        }
        crate::probe::since("video.conversao", began);
        let encoding = Instant::now();
        let stamp = openh264::Timestamp::from_millis(started.elapsed().as_millis() as u64);
        let encoded = encoder
            .encode_at(&yuv, stamp)
            .map(|bits| (bits.frame_type(), bits.to_vec()));
        // Kept for the next picture of the same size.
        self.yuv = Some(yuv);
        let (data, key) = match encoded {
            Ok((kind, data)) if kind != FrameType::Skip => {
                (data, matches!(kind, FrameType::IDR | FrameType::I))
            }
            Ok(_) => return true,
            Err(e) => {
                crate::logln!("vídeo: codificação: {e}");
                return true;
            }
        };
        if resized {
            self.size = Some((w, h));
            set_bitrate(encoder, kbps);
            self.kbps = kbps;
        }
        if data.is_empty() {
            return true;
        }
        crate::probe::since("video.codificacao", encoding);
        crate::probe::record(
            if key {
                "video.chave_kb"
            } else {
                "video.quadro_kb"
            },
            data.len() as f64 / 1024.0,
        );
        self.active = true;
        if key {
            self.last_key = Instant::now();
        }
        let duration = since.min(Duration::from_millis(500));
        self.last_sent = Instant::now();
        out.send(EncodedFrame {
            layer: self.layer,
            codec: Codec::H264,
            key,
            data,
            duration,
            captured: raw.at,
        })
        .is_ok()
    }
}

fn encode_loop(
    slot: &Slot,
    kind: Video,
    copies: Copies,
    out: &UnboundedSender<EncodedFrame>,
    control: &LayerControl,
    preview: &VideoShared,
    me: i64,
) {
    let started = Instant::now();
    // openh264 makes H.264 only, whatever the call would like.
    control.set_encodes_av1(false);
    let mut made = [
        Encoding::new(Layer::Low, copies.low),
        Encoding::new(Layer::High, copies.high),
    ];
    let mut last_raw: Option<Raw> = None;
    let mut last_preview = Instant::now() - Duration::from_secs(1);
    let mut own = Preview::new();
    // The small copy and the preview both ask the raw downscaled to
    // 640x360; the first one to see the raw leaves it here for the
    // second.
    let mut shared = Shared::default();
    // Wakes at least this often, so a static screen still repeats.
    let tick = Duration::from_millis(100);
    'pictures: loop {
        let fresh = slot.take(tick);
        if slot.stopped() {
            break;
        }
        if slot.ended.load(Ordering::Relaxed) {
            // The window is gone: the interface says so and stops the share.
            preview.wake();
            break;
        }
        let is_fresh = fresh.is_some();
        if let Some(raw) = fresh
            && let Some(old) = last_raw.replace(raw)
        {
            slot.give_back(old.data);
        }
        let Some(raw) = last_raw.as_ref() else {
            continue;
        };
        let bpp = if raw.layout == Layout::Bgra { 4 } else { 3 };
        if raw.data.len() < raw.width * raw.height * bpp {
            continue;
        }
        if is_fresh {
            shared.prepare();
        }
        for copy in &mut made {
            if !copy.step(raw, is_fresh, kind, control, out, started, &mut shared) {
                break 'pictures;
            }
        }
        // My own tile shows what I share even while nobody receives it,
        // and only while someone is looking (the local tile is on screen,
        // in the call view, with the window up): otherwise the downscale
        // of the full 4K picture every 66 ms is for nobody. The call view
        // asks for the picture every repaint; after a short grace, the
        // preview stops. The first ask after a pause brings it back in a
        // frame or two.
        if is_fresh
            && last_preview.elapsed() > Duration::from_millis(66)
            && preview.wanted_recently(me, TILE_GRACE)
        {
            last_preview = Instant::now();
            preview.put(me, preview_image(raw, &mut own, &mut shared), None);
        }
    }
    preview.forget(me);
}

// ---------------------------------------------------------------- receiving

/// One person's decoder, for the codec their video comes in.
enum Decoding {
    H264(Decoder),
    Av1(super::av1::Decoder),
}

/// Starts a thread that decodes one person's pictures into their texture.
/// Send it access units (H.264) or temporal units (AV1); the flag turns on
/// when a picture could not be decoded and a full one should be asked for.
pub fn decoder_thread(
    uid: i64,
    video: Arc<VideoShared>,
    codec: Codec,
) -> (std::sync::mpsc::Sender<Vec<u8>>, Arc<AtomicBool>) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let wants_key = Arc::new(AtomicBool::new(false));
    let flag = wants_key.clone();
    std::thread::Builder::new()
        .name(format!("video-decode-{uid}"))
        .spawn(move || {
            let made = match codec {
                Codec::H264 => Decoder::new()
                    .map(Decoding::H264)
                    .map_err(|e| e.to_string()),
                Codec::Av1 => super::av1::Decoder::new().map(Decoding::Av1),
            };
            let mut decoder = match made {
                Ok(d) => d,
                Err(e) => {
                    crate::logln!("vídeo: decodificador de {uid}: {e}");
                    return;
                }
            };
            let mut last_shown: Option<Instant> = None;
            let mut batch = Vec::new();
            let mut pictures = Pictures::new(3);
            while let Ok(first) = rx.recv() {
                // Every picture is decoded (each one is the reference of
                // the next), but under load only the newest is shown.
                batch.push(first);
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }
                let last = batch.len() - 1;
                for (i, au) in batch.drain(..).enumerate() {
                    let started = Instant::now();
                    let shown = match &mut decoder {
                        Decoding::H264(d) => match d.decode(&au) {
                            Ok(Some(img)) if i == last => {
                                crate::probe::since("video.decodificacao", started);
                                show(&img, uid, &video, &mut pictures);
                                Ok(true)
                            }
                            Ok(_) => Ok(false),
                            Err(_) => Err(()),
                        },
                        Decoding::Av1(d) => match d.decode(&au) {
                            Ok(Some(img)) if i == last => {
                                crate::probe::since("video.decodificacao", started);
                                show_av1(&img, uid, &video, &mut pictures);
                                Ok(true)
                            }
                            Ok(_) => Ok(false),
                            Err(_) => Err(()),
                        },
                    };
                    match shown {
                        Ok(true) => {
                            if let Some(at) = last_shown.replace(Instant::now()) {
                                crate::probe::since("video.intervalo", at);
                            }
                        }
                        Ok(false) => crate::probe::count("video.pulados", 1),
                        Err(()) => flag.store(true, Ordering::Relaxed),
                    }
                }
            }
        })
        .ok();
    (tx, wants_key)
}

/// As `show`, for a picture rav1d decoded.
fn show_av1(img: &super::av1::Picture, uid: i64, video: &VideoShared, pictures: &mut Pictures) {
    let Some((planes, strides)) = img.planes() else {
        return;
    };
    let (w, h) = img.dimensions();
    let stamp = if crate::probe::on() {
        read_stamp(&openh264::formats::YUVSlices::new(
            (planes[0], planes[1], planes[2]),
            (w, h),
            (strides.0, strides.1, strides.1),
        ))
    } else {
        None
    };
    if let Some(at) = stamp {
        crate::probe::record("tela.decodificada", crate::probe::wall_ms() - at);
    }
    let converting = Instant::now();
    crate::probe::record("video.largura", w as f64);
    let mut picture = pictures.take([w, h]);
    if let Some(dst) = Arc::get_mut(&mut picture) {
        super::av1::i420_to_rgba(
            planes,
            strides,
            (w, h),
            bytemuck::cast_slice_mut(&mut dst.pixels),
        );
    }
    pictures.keep(&picture);
    video.put(uid, picture, stamp);
    crate::probe::since("video.cor", converting);
}

/// Turns a decoded picture into `uid`'s texture. The colours are written
/// straight into the texture's pixels: opaque RGBA is already what egui
/// keeps (premultiplied), so there is nothing to convert pixel by pixel.
fn show(
    img: &openh264::decoder::DecodedYUV<'_>,
    uid: i64,
    video: &VideoShared,
    pictures: &mut Pictures,
) {
    use openh264::formats::YUVSource;
    let stamp = if crate::probe::on() {
        read_stamp(img)
    } else {
        None
    };
    if let Some(at) = stamp {
        crate::probe::record("tela.decodificada", crate::probe::wall_ms() - at);
    }
    let converting = Instant::now();
    let (w, h) = img.dimensions();
    crate::probe::record("video.largura", w as f64);
    let mut picture = pictures.take([w, h]);
    if let Some(dst) = Arc::get_mut(&mut picture) {
        img.write_rgba8(bytemuck::cast_slice_mut(&mut dst.pixels));
    }
    pictures.keep(&picture);
    video.put(uid, picture, stamp);
    crate::probe::since("video.cor", converting);
}

/// Measuring: the time a sharing app stamped across the top of its window
/// (see `probe::stamp`), read from the brightness of the picture.
fn read_stamp(img: &impl openh264::formats::YUVSource) -> Option<f64> {
    use crate::probe::stamp;
    let (w, h) = img.dimensions();
    let (stride, _, _) = img.strides();
    let y = img.y();
    if w < stamp::CELLS * 2 || h < 8 {
        return None;
    }
    let row = ((h as f32 * stamp::BAND * 0.5) as usize).min(h - 1) * stride;
    let white = |i: usize| {
        let x = ((i as f32 + 0.5) / stamp::CELLS as f32 * w as f32) as usize;
        let (from, to) = (x.saturating_sub(1), (x + 1).min(w - 1));
        let sum: u32 = (from..=to)
            .map(|x| u32::from(y.get(row + x).copied().unwrap_or(0)))
            .sum();
        sum / (to - from + 1) as u32 > 128
    };
    stamp::read(white, crate::probe::wall_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_keeps_the_shape_and_even_sizes() {
        assert_eq!(fit(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit(2560, 1440, 1920, 1080), (1920, 1080));
        assert_eq!(fit(1280, 720, 1920, 1080), (1280, 720));
        let (w, h) = fit(1366, 769, 3840, 2160);
        assert_eq!((w % 2, h % 2), (0, 0));
        assert_eq!(fit(1001, 1001, 1920, 1080), (1000, 1000));
    }

    #[test]
    fn scaling_halves_and_averages() {
        // 4x2 BGRA: a white column pair and a black one.
        let mut px = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 255 } else { 0 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        // The buffers come with leftovers, as they do from the last picture.
        let (mut out, mut tmp) = (vec![7; 100], vec![9; 3]);
        scale_into(&px, 4, 2, 4, 2, 1, &mut out, &mut tmp);
        assert_eq!(out, [255, 255, 255, 255, 0, 0, 0, 255]);
        // Same size comes back unchanged.
        scale_into(&px, 4, 2, 4, 4, 2, &mut out, &mut tmp);
        assert_eq!(out, px);
        // Halved, then the nearest pixel: 4x2 -> 2x1 -> 1x1.
        scale_into(&px, 4, 2, 4, 1, 1, &mut out, &mut tmp);
        assert_eq!(out, [255, 255, 255, 255]);
    }

    /// Builds a reproducible BGRA picture whose values stress the SIMD
    /// shuffles (each byte of each pixel is distinct, so a mix-up shows).
    fn stripes(w: usize, h: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                data.push(((x * 13 + y * 7) & 0xff) as u8); // B
                data.push(((x * 29 + y * 11) & 0xff) as u8); // G
                data.push(((x * 53 + y * 17) & 0xff) as u8); // R
                data.push(0xff); // A
            }
        }
        data
    }

    /// The AVX2 halve agrees with the scalar one within a one-unit rounding
    /// difference (both are 2x2 averages, but AVX2 pairs rounded averages).
    #[test]
    fn halve_simd_matches_scalar() {
        #[cfg(target_arch = "x86_64")]
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        for &(w, h) in &[
            (16, 2),      // one SIMD iteration, one row of output
            (32, 4),      // two SIMD iterations per row
            (24, 2),      // 24 input pixels: one SIMD + 4-pixel scalar tail
            (160, 60),    // small size
            (1920, 1080), // 1080p -> 540p
        ] {
            let src = stripes(w, h);
            let mut simd = vec![0u8; (w / 2) * (h / 2) * 4];
            let mut scalar = vec![0u8; (w / 2) * (h / 2) * 4];
            // SAFETY: AVX2 was checked above; buffers are correctly sized.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                halve_bgra_avx2(&src, w, h, &mut simd);
            }
            halve_scalar(&src, w, h, 4, &mut scalar);
            assert_eq!(simd.len(), scalar.len());
            // Allow a one-unit rounding difference between
            // ((a+b+1)/2 + (c+d+1)/2 + 1)/2 and (a+b+c+d+2)/4.
            let bad: Vec<_> = simd
                .iter()
                .zip(&scalar)
                .enumerate()
                .filter(|(_, (a, b))| i32::from(**a).abs_diff(i32::from(**b)) > 1)
                .collect();
            assert!(bad.is_empty(), "{w}x{h}: {} bytes off > 1", bad.len());
        }
    }

    /// The SIMD BGRA→Color32 produces the same pixels as the scalar one.
    #[test]
    fn bgra_to_color32_simd_matches_scalar() {
        #[cfg(target_arch = "x86_64")]
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        for &pixels in &[8usize, 16, 17, 100, 640 * 360] {
            let src = stripes(pixels, 1);
            let mut simd = vec![egui::Color32::BLACK; pixels];
            let mut scalar = vec![egui::Color32::BLACK; pixels];
            // SAFETY: AVX2 was checked above; src has 4*pixels bytes and
            // simd has `pixels` entries, as required.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                bgra_to_color32_avx2(&src, &mut simd);
            }
            let (chunks, _) = src.as_chunks::<4>();
            for (px, out) in chunks.iter().zip(scalar.iter_mut()) {
                *out = egui::Color32::from_rgb(px[2], px[1], px[0]);
            }
            assert_eq!(simd, scalar, "{pixels} pixels");
        }
    }

    /// `reset_buffer` keeps the capacity across calls and does not zero
    /// the memory (the scalar `resize(n, 0)` was ~1 ms on an 8 MB buffer).
    #[test]
    fn reset_buffer_keeps_capacity() {
        let mut buf = Vec::<u8>::with_capacity(16);
        buf.extend_from_slice(&[1, 2, 3, 4, 5]);
        reset_buffer(&mut buf, 8);
        assert_eq!(buf.len(), 8);
        assert!(buf.capacity() >= 8);
        // Reusing the Vec for a bigger size reallocates; a smaller one does not.
        reset_buffer(&mut buf, 4);
        assert_eq!(buf.len(), 4);
        assert!(buf.capacity() >= 8, "smaller size does not shrink");
    }

    fn screen_copies() -> Copies {
        super::super::plan::copies(Video::Screen, super::super::Quality::by_key("720p30"))
    }

    #[test]
    fn the_copies_are_made_on_demand() {
        let copies = screen_copies();
        let control = LayerControl::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let started = Instant::now();
        let mut low = Encoding::new(Layer::Low, copies.low);
        let mut high = Encoding::new(Layer::High, copies.high);
        let (w, h) = (1280, 720);
        // A smooth picture, as a screen mostly is (noise would make the
        // first full picture so big that the encoder skips the next ones).
        let picture = |shift: usize| Raw {
            width: w,
            height: h,
            data: (0..w * h)
                .flat_map(|i| {
                    let (x, y) = (i % w, i / w);
                    [
                        ((x + shift) * 255 / (w + shift)) as u8,
                        (y * 255 / h) as u8,
                        80,
                        255,
                    ]
                })
                .collect(),
            layout: Layout::Bgra,
            at: Instant::now(),
        };
        let raw = picture(0);
        let mut shared = Shared::default();
        // Nobody wants either: nothing is made.
        assert!(low.step(
            &raw,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
        assert!(rx.try_recv().is_err());
        // The small copy, wanted: it starts with a full picture, at its size.
        control.set(Layer::Low, true, 200);
        assert!(low.step(
            &raw,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
        assert!(high.step(
            &raw,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
        let first = rx.try_recv().unwrap();
        assert_eq!((first.layer, first.key), (Layer::Low, true));
        assert!(rx.try_recv().is_err(), "the big copy was not asked for");
        let mut dec = Decoder::new().unwrap();
        let img = dec
            .decode(&first.data)
            .unwrap()
            .expect("uma imagem completa");
        use openh264::formats::YUVSource;
        assert_eq!(img.dimensions(), (640, 360));
        // Asked again right after a full picture: an ordinary one goes, and
        // the request waits its turn. (A picture that changed: an identical
        // one may be skipped by the encoder.)
        std::thread::sleep(Duration::from_millis(130));
        control.want_key(Layer::Low);
        let moved = picture(64);
        shared.prepare();
        assert!(low.step(
            &moved,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
        assert!(!rx.try_recv().unwrap().key);
        let raw = moved;
        assert!(control.key[0].load(Ordering::Relaxed), "still pending");
        // Turned off and on again: a full picture first.
        control.set(Layer::Low, false, 200);
        shared.prepare();
        assert!(low.step(
            &raw,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
        control.set(Layer::Low, true, 300);
        std::thread::sleep(Duration::from_millis(130));
        shared.prepare();
        assert!(low.step(
            &raw,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
        assert!(rx.try_recv().unwrap().key);
        // Nobody listening any more stops the loop.
        drop(rx);
        std::thread::sleep(Duration::from_millis(130));
        assert!(!low.step(
            &raw,
            true,
            Video::Screen,
            &control,
            &tx,
            started,
            &mut shared
        ));
    }

    /// Where the time of a screen share goes, stage by stage: the small
    /// copy, the big copy and the preview on each captured picture.
    /// Timed at 1080p60 and at 4K60 (where the pipeline runs against the
    /// 60 fps budget of 16.7 ms per picture). For hand-run profiling, so
    /// skipped unless asked for:
    ///
    ///   cargo test -p babutela --release profile_screen_share -- --ignored --nocapture
    #[test]
    #[ignore]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn profile_screen_share() {
        use std::time::Instant;

        fn gen_bgra(w: usize, h: usize) -> Vec<u8> {
            let mut data = vec![0u8; w * h * 4];
            // A busy screen keeps the encoder from skipping every frame.
            for (i, px) in data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i % w, i / w);
                px[0] = ((x * 31 + y * 17) % 256) as u8;
                px[1] = ((x * 7 + y * 97) % 256) as u8;
                px[2] = ((x * 11 + y * 3 + i / 7) % 256) as u8;
                px[3] = 255;
            }
            data
        }

        fn profile(w: usize, h: usize, fps: u32, max_kbps: u32, iters: usize) {
            println!("\n=== {w}x{h}@{fps} ===");
            let q = super::super::Quality {
                key: "profile",
                label: "profile",
                width: w as u32,
                height: h as u32,
                fps,
                screen: max_kbps,
                camera: max_kbps,
            };
            let copies = super::super::plan::copies(Video::Screen, q);

            let (control, (tx, mut rx)) = (
                LayerControl::default(),
                tokio::sync::mpsc::unbounded_channel(),
            );
            control.set(Layer::Low, true, copies.low.max_kbps);
            control.set(Layer::High, true, copies.high.max_kbps);

            let mut low = Encoding::new(Layer::Low, copies.low);
            let mut high = Encoding::new(Layer::High, copies.high);
            let started = Instant::now();
            let mut own = Preview::new();
            let mut shared = Shared::default();

            let data = gen_bgra(w, h);
            // Warm the encoders up (first frame is a full picture).
            let raw = Raw {
                width: w,
                height: h,
                data: data.clone(),
                layout: Layout::Bgra,
                at: Instant::now(),
            };
            let _ = low.step(
                &raw,
                true,
                Video::Screen,
                &control,
                &tx,
                started,
                &mut shared,
            );
            let _ = high.step(
                &raw,
                true,
                Video::Screen,
                &control,
                &tx,
                started,
                &mut shared,
            );
            while rx.try_recv().is_ok() {}

            let (mut t_low, mut t_high, mut t_preview, mut bytes_low, mut bytes_high) =
                (0u128, 0u128, 0u128, 0u64, 0u64);
            let gap = Duration::from_secs_f64(1.0 / f64::from(fps));
            let mut shift = 0u8;
            for i in 0..iters {
                shift = shift.wrapping_add(1);
                // Change the picture so the encoder does not skip it.
                let mut raw_data = data.clone();
                for px in raw_data.chunks_exact_mut(4).step_by(13) {
                    px[0] = px[0].wrapping_add(shift);
                }
                let raw = Raw {
                    width: w,
                    height: h,
                    data: raw_data,
                    layout: Layout::Bgra,
                    at: Instant::now(),
                };
                // The due time is set from the previous call; wait so the
                // encoders don't skip.
                let elapsed = gap * (i as u32 + 2);
                let synthetic_now = started + elapsed;
                while Instant::now() < synthetic_now {
                    std::hint::spin_loop();
                }
                shared.prepare();
                let t = Instant::now();
                let _ = low.step(
                    &raw,
                    true,
                    Video::Screen,
                    &control,
                    &tx,
                    started,
                    &mut shared,
                );
                t_low += t.elapsed().as_nanos();
                let t = Instant::now();
                let _ = high.step(
                    &raw,
                    true,
                    Video::Screen,
                    &control,
                    &tx,
                    started,
                    &mut shared,
                );
                t_high += t.elapsed().as_nanos();
                let t = Instant::now();
                let _ = preview_image(&raw, &mut own, &mut shared);
                t_preview += t.elapsed().as_nanos();
                while let Ok(frame) = rx.try_recv() {
                    match frame.layer {
                        Layer::Low => bytes_low += frame.data.len() as u64,
                        Layer::High => bytes_high += frame.data.len() as u64,
                        Layer::Off => {}
                    }
                }
            }
            let avg_ms = |n: u128| n as f64 / iters as f64 / 1_000_000.0;
            println!(
                "low         {:5.2} ms/pic ({} fps, output {} kb/s)",
                avg_ms(t_low),
                copies.low.fps,
                bytes_low / iters as u64 * 8 * fps as u64 / 1000
            );
            println!(
                "high        {:5.2} ms/pic ({} fps, output {} kb/s)",
                avg_ms(t_high),
                copies.high.fps,
                bytes_high / iters as u64 * 8 * fps as u64 / 1000
            );
            println!("preview     {:5.2} ms/pic", avg_ms(t_preview));
            println!(
                "all three   {:5.2} ms/pic (60 fps budget: 16.7)",
                avg_ms(t_low + t_high + t_preview)
            );
            let budget_ms = 1000.0 / f64::from(fps);
            let utilization = avg_ms(t_low + t_high + t_preview) / budget_ms * 100.0;
            println!("one core at {utilization:.1} % while sharing");
        }

        // 60 iterations is one second of video.
        profile(1920, 1080, 60, 5000, 60);
        profile(3840, 2160, 60, 12000, 20);
    }

    /// The whole encode loop one second at a time, with the preview gated
    /// by whether the UI is looking (the local tile on screen) or not (an
    /// enlarged peer, another channel, a hidden window). Hand-run:
    ///
    ///   cargo test -p babutela --release profile_screen_share_loop -- --ignored --nocapture
    #[test]
    #[ignore]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn profile_screen_share_loop() {
        use std::time::Instant;

        fn gen_bgra(w: usize, h: usize) -> Vec<u8> {
            let mut data = vec![0u8; w * h * 4];
            for (i, px) in data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i % w, i / w);
                px[0] = ((x * 31 + y * 17) % 256) as u8;
                px[1] = ((x * 7 + y * 97) % 256) as u8;
                px[2] = ((x * 11 + y * 3 + i / 7) % 256) as u8;
                px[3] = 255;
            }
            data
        }

        fn profile(w: usize, h: usize, visible: bool) {
            let q = super::super::Quality {
                key: "profile",
                label: "profile",
                width: w as u32,
                height: h as u32,
                fps: 60,
                screen: 5000,
                camera: 5000,
            };
            let copies = super::super::plan::copies(Video::Screen, q);
            let (control, (tx, mut rx)) = (
                LayerControl::default(),
                tokio::sync::mpsc::unbounded_channel(),
            );
            control.set(Layer::Low, true, copies.low.max_kbps);
            control.set(Layer::High, true, copies.high.max_kbps);
            let video = VideoShared::new(egui::Context::default());
            let (me, iters) = (42i64, 60);
            let data = gen_bgra(w, h);
            let make_raw = |shift: u8| {
                let mut d = data.clone();
                for px in d.chunks_exact_mut(4).step_by(13) {
                    px[0] = px[0].wrapping_add(shift);
                }
                Raw {
                    width: w,
                    height: h,
                    data: d,
                    layout: Layout::Bgra,
                    at: Instant::now(),
                }
            };

            let mut low = Encoding::new(Layer::Low, copies.low);
            let mut high = Encoding::new(Layer::High, copies.high);
            let mut own = Preview::new();
            let mut shared = Shared::default();
            let started = Instant::now();
            // Warm up the encoders.
            let raw = make_raw(0);
            let _ = low.step(
                &raw,
                true,
                Video::Screen,
                &control,
                &tx,
                started,
                &mut shared,
            );
            let _ = high.step(
                &raw,
                true,
                Video::Screen,
                &control,
                &tx,
                started,
                &mut shared,
            );
            while rx.try_recv().is_ok() {}
            if visible {
                let _ = video.frame(me);
            }
            let mut last_preview = Instant::now() - Duration::from_secs(1);

            let t0 = Instant::now();
            for i in 0..iters {
                let shift = (i + 1) as u8;
                let raw = make_raw(shift);
                shared.prepare();
                let _ = low.step(
                    &raw,
                    true,
                    Video::Screen,
                    &control,
                    &tx,
                    started,
                    &mut shared,
                );
                let _ = high.step(
                    &raw,
                    true,
                    Video::Screen,
                    &control,
                    &tx,
                    started,
                    &mut shared,
                );
                // The UI tells the preview it is drawing: in a real call,
                // the stage calls voice.frame(me) every repaint.
                if visible {
                    let _ = video.frame(me);
                }
                if last_preview.elapsed() > Duration::from_millis(66)
                    && video.wanted_recently(me, Duration::from_millis(300))
                {
                    last_preview = Instant::now();
                    let _ = preview_image(&raw, &mut own, &mut shared);
                }
                while rx.try_recv().is_ok() {}
            }
            let total = t0.elapsed();
            let budget = Duration::from_secs_f64(iters as f64 / 60.0);
            println!(
                "{}x{} {} {:5.1} ms total ({:.0} % of {} ms budget)",
                w,
                h,
                if visible {
                    "tile  visible  "
                } else {
                    "tile  hidden   "
                },
                total.as_secs_f64() * 1000.0,
                total.as_secs_f64() / budget.as_secs_f64() * 100.0,
                budget.as_millis()
            );
        }

        profile(1920, 1080, true);
        profile(1920, 1080, false);
        profile(3840, 2160, true);
        profile(3840, 2160, false);
    }

    /// Where the time of one actual low-stream frame goes: scale, YUV
    /// conversion and encode, timed separately. Lets a candidate like
    /// `halve_bgra_to_i420` be sized against the YUV cost before it is
    /// written. Hand-run:
    ///
    ///   cargo test -p babutela --release profile_screen_share_stages -- --ignored --nocapture
    #[test]
    #[ignore]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn profile_screen_share_stages() {
        use openh264::formats::YUVSource;
        use std::time::Instant;

        fn gen_bgra(w: usize, h: usize) -> Vec<u8> {
            let mut data = vec![0u8; w * h * 4];
            for (i, px) in data.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i % w, i / w);
                px[0] = ((x * 31 + y * 17) % 256) as u8;
                px[1] = ((x * 7 + y * 97) % 256) as u8;
                px[2] = ((x * 11 + y * 3 + i / 7) % 256) as u8;
                px[3] = 255;
            }
            data
        }

        /// Each stage separately, averaged over `iters` iterations.
        fn breakdown(src_w: usize, src_h: usize, dst: Spec, label: &str, iters: usize) {
            let raw_data = gen_bgra(src_w, src_h);
            let (tw, th) = (dst.width as usize, dst.height as usize);
            let bpp = 4;

            let mut encoder = new_encoder(Video::Screen, dst).unwrap();
            let mut scaled: Vec<u8> = Vec::new();
            let mut tmp: Vec<u8> = Vec::new();
            let started = Instant::now();

            // Warm up: one encode (first frame is a full picture, bigger).
            scale_into(&raw_data, src_w, src_h, bpp, tw, th, &mut scaled, &mut tmp);
            let v = View {
                data: &scaled,
                width: tw,
                height: th,
                stride: tw,
                layout: Layout::Bgra,
            };
            let yb = YUVBuffer::from_rgb8_source(v);
            encoder.force_intra_frame();
            let _ = encoder.encode(&yb).unwrap().to_vec();
            let mut yuv = Some(yb);

            let (mut t_scale, mut t_yuv, mut t_enc, mut bytes) = (0u128, 0u128, 0u128, 0u64);
            for i in 0..iters {
                // Fresh data each iter so the encoder does not skip.
                let shift = (i + 1) as u8;
                let mut data = raw_data.clone();
                for px in data.chunks_exact_mut(4).step_by(13) {
                    px[0] = px[0].wrapping_add(shift);
                }

                // 1. Scale (or crop) the raw to the target size.
                let t = Instant::now();
                if src_w == tw && src_h == th {
                    scaled.clear();
                    scaled.extend_from_slice(&data);
                } else {
                    scale_into(&data, src_w, src_h, bpp, tw, th, &mut scaled, &mut tmp);
                }
                t_scale += t.elapsed().as_nanos();

                let v = View {
                    data: &scaled,
                    width: tw,
                    height: th,
                    stride: tw,
                    layout: Layout::Bgra,
                };

                // 2. BGRA → I420 (openh264's AVX2 path).
                let t = Instant::now();
                let yb = match yuv.take() {
                    Some(mut b) if YUVSource::dimensions(&b) == (tw, th) => {
                        b.read_rgb8(v);
                        b
                    }
                    _ => YUVBuffer::from_rgb8_source(v),
                };
                t_yuv += t.elapsed().as_nanos();

                // 3. Encode the I420 picture.
                let stamp = openh264::Timestamp::from_millis(started.elapsed().as_millis() as u64);
                let t = Instant::now();
                let bits = encoder.encode_at(&yb, stamp).unwrap();
                let kind = bits.frame_type();
                let data_out = bits.to_vec();
                t_enc += t.elapsed().as_nanos();
                if kind != openh264::encoder::FrameType::Skip {
                    bytes += data_out.len() as u64;
                }
                yuv = Some(yb);
            }

            let avg_ms = |n: u128| n as f64 / iters as f64 / 1_000_000.0;
            let total_ms = avg_ms(t_scale + t_yuv + t_enc);
            println!("\n{label} ({src_w}x{src_h} -> {tw}x{th}, {} iters):", iters);
            println!(
                "  scale  {:5.3} ms/pic  ({:4.1} %)",
                avg_ms(t_scale),
                avg_ms(t_scale) / total_ms * 100.0
            );
            println!(
                "  yuv    {:5.3} ms/pic  ({:4.1} %)   <- fusable with scale",
                avg_ms(t_yuv),
                avg_ms(t_yuv) / total_ms * 100.0
            );
            println!(
                "  encode {:5.3} ms/pic  ({:4.1} %)",
                avg_ms(t_enc),
                avg_ms(t_enc) / total_ms * 100.0
            );
            println!(
                "  total  {:5.3} ms/pic   ({} B/pic avg)",
                total_ms,
                bytes / iters as u64
            );
        }

        let q_full_hd = super::super::Quality::by_key("1080p60");
        let q_source = super::super::Quality::by_key("source");
        let copies_full_hd = super::super::plan::copies(Video::Screen, q_full_hd);
        let copies_source = super::super::plan::copies(Video::Screen, q_source);

        println!("\n=== 1080p source ===");
        breakdown(1920, 1080, copies_full_hd.low, "LOW ", 60);
        breakdown(1920, 1080, copies_full_hd.high, "HIGH", 60);

        println!("\n=== 4K source ===");
        breakdown(3840, 2160, copies_source.low, "LOW ", 30);
        breakdown(3840, 2160, copies_source.high, "HIGH", 15);
    }

    #[test]
    fn encoded_pictures_decode_again() {
        let mut enc = new_encoder(Video::Screen, screen_copies().high).unwrap();
        let (w, h) = (320, 240);
        let bgra: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, (i % 13) as u8 * 19, 80, 255])
            .collect();
        let yuv = YUVBuffer::from_bgra8_source(openh264::formats::BgraSliceU8::new(&bgra, (w, h)));
        enc.force_intra_frame();
        let bits = enc.encode(&yuv).unwrap().to_vec();
        assert!(!bits.is_empty());
        let mut dec = Decoder::new().unwrap();
        let img = dec
            .decode(&bits)
            .unwrap()
            .expect("a primeira imagem é completa");
        use openh264::formats::YUVSource;
        assert_eq!(img.dimensions(), (w, h));
    }
}

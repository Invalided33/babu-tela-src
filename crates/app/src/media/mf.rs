//! Videos, through Media Foundation: Windows reads the container (MP4, MOV,
//! WebM, MKV) and decodes with what it has (H.264 and HEVC, VP9 with the
//! extension that ships with Windows, AAC, MP3, Opus...), on the graphics
//! card when it can. Pictures come out as 32-bit RGB, sound as floats.
//! A video on the server is read straight from its address, in pieces.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};

use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFMediaType, IMFSourceReader, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_GEOMETRIC_APERTURE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
    MF_PD_DURATION, MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ANY_STREAM,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION,
    MFAudioFormat_Float, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL,
    MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoArea,
    MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING};

use super::output::Feed;
use super::{Cmd, Frame, MAX_FRAMES, MAX_H, MAX_W, Shared};
use crate::pictures::Pictures;
use crate::sounds::Sounds;

/// Media Foundation counts time in 100 ns units.
const TICKS: f64 = 10_000_000.0;

/// Plays `source`: a file's path, or an http(s) address, which Media
/// Foundation reads itself, asking the server for the pieces it needs as it
/// plays and seeks (so a long video starts at once). False when it could not
/// even be opened, so the caller can try another way; anything that goes
/// wrong later is reported on `shared`.
pub fn run(source: &str, shared: &Arc<Shared>, cmds: &Receiver<Cmd>, sounds: &Arc<Sounds>) -> bool {
    // COM in whatever model the thread already has (winsys.rs), undone
    // when this returns, after everything below.
    let _com = match crate::winsys::Com::multithreaded() {
        Ok(com) => com,
        Err(e) => {
            shared.fail(format!("COM: {e}"));
            return true;
        }
    };
    // SAFETY: Media Foundation is started and stopped on this thread,
    // around everything that uses it; the reader goes before it does.
    unsafe {
        // The full start: addresses need Media Foundation's network side.
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            shared.fail(format!("Media Foundation: {e}"));
            return true;
        }
        // On the graphics card when it can: measured with a 720p video, a
        // third of the processor time (`cargo xtask medir midia`, against
        // BABUTELA_TESTE_MIDIA=processador). The way that always works
        // when the card cannot open the video, or fails before any of it
        // played.
        let manager = if card_wanted() { card() } else { None };
        let mut opened = false;
        for way in [manager.as_ref(), None] {
            let reader = match open(source, way) {
                Ok(reader) => reader,
                Err(e) if way.is_some() => {
                    crate::logln!("mídia: na placa de vídeo não abriu ({e})");
                    continue;
                }
                Err(e) => {
                    crate::logln!("mídia: {e}");
                    opened = false;
                    break;
                }
            };
            opened = true;
            let mut started = false;
            match play(&reader, way.is_some(), shared, cmds, sounds, &mut started) {
                Err(e) if way.is_some() && !started => {
                    crate::logln!("mídia: na placa de vídeo não deu ({e})");
                }
                Err(e) => {
                    shared.fail(e);
                    break;
                }
                Ok(()) => break,
            }
        }
        let _ = MFShutdown();
        opened
    }
}

/// A reader of `source` that turns the pictures into RGB: on the graphics
/// card of `manager`, or on the processor.
unsafe fn open(
    source: &str,
    manager: Option<&IMFDXGIDeviceManager>,
) -> Result<IMFSourceReader, String> {
    // SAFETY: as in `run`.
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2).map_err(|e| e.to_string())?;
        let attrs = attrs.ok_or("atributos")?;
        match manager {
            Some(manager) => {
                attrs
                    .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager)
                    .map_err(|e| e.to_string())?;
                attrs
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                    .map_err(|e| e.to_string())?;
            }
            None => attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                .map_err(|e| e.to_string())?,
        }
        MFCreateSourceReaderFromURL(&HSTRING::from(source), &attrs)
            .map_err(|e| format!("abrindo o vídeo: {e}"))
    }
}

/// Measuring: BABUTELA_TESTE_MIDIA=processador decodes on the processor,
/// as before, to compare.
fn card_wanted() -> bool {
    !(crate::probe::on()
        && std::env::var_os("BABUTELA_TESTE_MIDIA").is_some_and(|v| v == "processador"))
}

/// A Direct3D 11 device for the reader, so the video is decoded (DXVA)
/// and turned into RGB on the graphics card. The pictures still come to
/// memory as before (locking a buffer copies it out of the card): copying
/// them costs little, decoding them is what weighs.
fn card() -> Option<IMFDXGIDeviceManager> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        D3D11CreateDevice, ID3D11Multithread,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
    use windows::Win32::Media::MediaFoundation::MFCreateDXGIDeviceManager;
    use windows::core::Interface;
    // SAFETY: plain COM calls; every out pointer is a local.
    let made = unsafe {
        (|| -> windows::core::Result<_> {
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let fail = || windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
            let (device, context) = (device.ok_or_else(fail)?, context.ok_or_else(fail)?);
            // Media Foundation uses the device from threads of its own.
            let _ = context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            let mut token = 0;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(fail)?;
            manager.ResetDevice(&device, token)?;
            Ok(manager)
        })()
    };
    match made {
        Ok(m) => Some(m),
        Err(e) => {
            crate::logln!("mídia: sem a placa de vídeo ({e})");
            None
        }
    }
}

/// The picture's layout as the reader hands it out.
#[derive(Clone, Copy, Debug)]
struct Picture {
    width: usize,
    height: usize,
    stride: isize,
    /// The part meant to be seen (x, y, width, height): decoders work in
    /// blocks of 16, so a 540-line video comes out with 544 lines whose
    /// last 4 are filler.
    crop: (usize, usize, usize, usize),
}

fn picture(kind: &IMFMediaType) -> Option<Picture> {
    // SAFETY: plain attribute reads on a media type the reader owns.
    unsafe {
        let size = kind.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let (width, height) = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
        if width == 0 || height == 0 {
            return None;
        }
        let stride = kind
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map_or(width as isize * 4, |s| s as i32 as isize);
        let mut crop = (0, 0, width, height);
        for key in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
            let mut area = [0u8; size_of::<MFVideoArea>()];
            if kind.GetBlob(key, &mut area, None).is_ok() {
                // MFVideoArea: two MFOffsets (u16 fraction, i16 value), then
                // the size as two i32.
                let x = i16::from_le_bytes([area[2], area[3]]).max(0) as usize;
                let y = i16::from_le_bytes([area[6], area[7]]).max(0) as usize;
                let w = i32::from_le_bytes([area[8], area[9], area[10], area[11]]).max(0) as usize;
                let h =
                    i32::from_le_bytes([area[12], area[13], area[14], area[15]]).max(0) as usize;
                if w > 0 && h > 0 && x + w <= width && y + h <= height {
                    crop = (x, y, w, h);
                    break;
                }
            }
        }
        Some(Picture {
            width,
            height,
            stride,
            crop,
        })
    }
}

/// With BABUTELA_MIDIA_LOG set, tells how the pictures come out.
fn log_picture(pic: Option<Picture>) {
    if std::env::var_os("BABUTELA_MIDIA_LOG").is_some()
        && let Some(p) = pic
    {
        crate::logln!(
            "mídia: {}x{} stride {} recorte {:?}",
            p.width,
            p.height,
            p.stride,
            p.crop
        );
    }
}

unsafe fn select(reader: &IMFSourceReader, stream: i32, major: &GUID, sub: &GUID) -> bool {
    // SAFETY: as in `play`; a stream that is not there just fails.
    unsafe {
        if reader.SetStreamSelection(stream as u32, true).is_err() {
            return false;
        }
        let Ok(kind) = MFCreateMediaType() else {
            return false;
        };
        let ok = kind.SetGUID(&MF_MT_MAJOR_TYPE, major).is_ok()
            && kind.SetGUID(&MF_MT_SUBTYPE, sub).is_ok()
            && reader
                .SetCurrentMediaType(stream as u32, None, &kind)
                .is_ok();
        if !ok {
            // No decoder for it here: carry on without that stream.
            let _ = reader.SetStreamSelection(stream as u32, false);
        }
        ok
    }
}

/// Plays what `reader` reads. `on_card`: the reader decodes on the graphics
/// card; `started` turns on once a picture or sound was handed over (until
/// then, a failure leaves nothing behind, and the processor can try).
unsafe fn play(
    reader: &IMFSourceReader,
    on_card: bool,
    shared: &Arc<Shared>,
    cmds: &Receiver<Cmd>,
    sounds: &Arc<Sounds>,
    started: &mut bool,
) -> Result<(), String> {
    // SAFETY: the calls below follow Media Foundation's rules: the reader
    // and every object it hands out live on this thread, and buffers are
    // unlocked before they go.
    unsafe {
        let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        let has_video = select(
            reader,
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0,
            &MFMediaType_Video,
            &MFVideoFormat_RGB32,
        );
        if on_card
            && !has_video
            && reader
                .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)
                .is_ok()
        {
            // There is a picture the card cannot turn into RGB.
            return Err("a placa não converte esta imagem".into());
        }
        let has_audio = select(
            reader,
            MF_SOURCE_READER_FIRST_AUDIO_STREAM.0,
            &MFMediaType_Audio,
            &MFAudioFormat_Float,
        );
        if !has_video && !has_audio {
            return Err("o Windows não tem como tocar este vídeo".into());
        }
        let mut pic = if has_video {
            reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                .ok()
                .as_ref()
                .and_then(picture)
        } else {
            None
        };
        log_picture(pic);
        // The sound's rate and channel count, when there is sound.
        let mut audio = None;
        let feed = if has_audio {
            let kind = reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
                .map_err(|e| e.to_string())?;
            let rate = kind
                .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                .unwrap_or(48_000);
            let channels = kind.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2) as u16;
            audio = Some((rate, channels));
            let feed = Feed::new(rate, channels, shared.wake.clone());
            sounds.add_source(feed.source()).then_some(feed)
        } else {
            None
        };
        let duration = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()
            .and_then(|v: PROPVARIANT| u64::try_from(&v).ok())
            .map(|ticks| ticks as f64 / TICKS);
        let shown = pic.map(|p| {
            let (w, h) = fit(p.crop.2, p.crop.3);
            [w, h]
        });
        shared.ready(duration, shown, feed.clone());

        let mut video_done = !has_video;
        let mut audio_done = feed.is_none();
        // The queued pictures, the one on screen and the one being written.
        let mut pool = Pictures::new(MAX_FRAMES + 2);
        // Where the last seek asked to go. Media Foundation lands on the key
        // frame before it; what comes before is decoded but neither shown
        // nor heard, so picture and sound start together where asked.
        let mut start = 0.0;
        loop {
            let all_read = video_done && audio_done;
            let cmd = if all_read {
                // Everything is out: nothing to do until a seek (playing
                // again) or the end.
                shared.decoded_all.store(true, Ordering::SeqCst);
                match cmds.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                }
            } else {
                match cmds.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => break,
                }
            };
            match cmd {
                Some(Cmd::Seek(t)) => {
                    seek(reader, t);
                    shared.sought(t);
                    start = t;
                    video_done = !has_video;
                    audio_done = feed.is_none();
                    continue;
                }
                Some(Cmd::Quit) => break,
                None => {}
            }
            // Far enough ahead: until a picture goes to the screen, the
            // sound runs low or a command comes.
            let sound_full = feed.as_ref().is_none_or(|f| !f.wants_more());
            if (video_done || shared.frames_full()) && (audio_done || sound_full) {
                shared.wake.wait();
                continue;
            }
            let mut index = 0u32;
            let mut flags = 0u32;
            let mut stamp = 0i64;
            let mut sample = None;
            let read = reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&mut index),
                Some(&mut flags),
                Some(&mut stamp),
                Some(&mut sample),
            );
            if let Err(e) = read {
                // The sound ends with it (the mixer lets a finished feed go).
                if let Some(f) = &feed {
                    f.finish();
                }
                return Err(format!("lendo o vídeo: {e}"));
            }
            let kind = reader
                .GetCurrentMediaType(index)
                .ok()
                .and_then(|t| t.GetGUID(&MF_MT_MAJOR_TYPE).ok());
            let is_video = kind == Some(MFMediaType_Video);
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 && is_video {
                pic = reader
                    .GetCurrentMediaType(index)
                    .ok()
                    .as_ref()
                    .and_then(picture);
                log_picture(pic);
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                if is_video {
                    video_done = true;
                } else {
                    audio_done = true;
                }
            }
            let Some(sample) = sample else { continue };
            let Ok(buffer) = sample.ConvertToContiguousBuffer() else {
                continue;
            };
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            if buffer.Lock(&mut data, None, Some(&mut len)).is_err() {
                continue;
            }
            let bytes = std::slice::from_raw_parts(data, len as usize);
            let pts = stamp as f64 / TICKS;
            if is_video {
                // A picture that is over before the start is not shown.
                let ends = pts + sample.GetSampleDuration().map_or(0.0, |d| d as f64 / TICKS);
                if ends > start
                    && let Some(p) = pic
                {
                    let converting = std::time::Instant::now();
                    if let Some(frame) = to_frame(bytes, packed(p, bytes.len()), pts, &mut pool) {
                        crate::probe::since("midia.quadro", converting);
                        shared.push_frame(frame);
                        *started = true;
                    }
                }
            } else if let Some(f) = &feed {
                let floats = bytes.as_chunks::<4>().0;
                let skip = audio.map_or(0, |(rate, channels)| {
                    early_samples(start - pts, rate, channels).min(floats.len())
                });
                f.push(floats[skip..].iter().map(|b| f32::from_le_bytes(*b)));
                *started |= skip < floats.len();
            }
            let _ = buffer.Unlock();
        }
        if let Some(f) = feed {
            f.finish();
        }
        Ok(())
    }
}

unsafe fn seek(reader: &IMFSourceReader, t: f64) {
    let at = PROPVARIANT::from((t.max(0.0) * TICKS) as i64);
    // SAFETY: GUID_NULL means 100 ns units; the reader is this thread's.
    unsafe {
        let _ = reader.SetCurrentPosition(&GUID::zeroed(), &at);
    }
}

/// The layout of `len` bytes of picture `p`. Locked whole, a picture's
/// buffer comes out in its contiguous form, rows packed (for RGB, a row is
/// the width in bytes), whatever stride the media type gives for the
/// texture or surface behind it.
fn packed(p: Picture, len: usize) -> Picture {
    let row = p.width * 4;
    if len == row * p.height && p.stride.unsigned_abs() != row {
        // Packed rows are top-down.
        Picture {
            stride: row as isize,
            ..p
        }
    } else {
        p
    }
}

/// How many samples (every channel counted) of a sound buffer that begins
/// `early` seconds before the start are to be cut off: whole frames only.
fn early_samples(early: f64, rate: u32, channels: u16) -> usize {
    if early <= 0.0 {
        return 0;
    }
    (early * f64::from(rate)).round() as usize * usize::from(channels)
}

/// The largest size inside MAX_W x MAX_H with the picture's shape.
fn fit(width: usize, height: usize) -> (usize, usize) {
    let scale = (MAX_W as f64 / width as f64)
        .min(MAX_H as f64 / height as f64)
        .min(1.0);
    (
        ((width as f64 * scale) as usize).max(1),
        ((height as f64 * scale) as usize).max(1),
    )
}

/// BGRX rows (top-down, or bottom-up when the stride is negative) to a
/// picture of the part meant to be seen, no bigger than MAX_W x MAX_H,
/// written over one from `pool` that the interface is done with.
fn to_frame(bytes: &[u8], p: Picture, pts: f64, pool: &mut Pictures) -> Option<Frame> {
    let row = p.stride.unsigned_abs();
    let (x0, y0, cw, ch) = p.crop;
    if row < p.width * 4
        || bytes.len() < row * p.height
        || cw == 0
        || ch == 0
        || x0 + cw > p.width
        || y0 + ch > p.height
    {
        return None;
    }
    let (w, h) = fit(cw, ch);
    let mut image = pool.take([w, h]);
    let pixels = &mut Arc::get_mut(&mut image)?.pixels;
    let bgrx = |px: &[u8; 4]| egui::Color32::from_rgb(px[2], px[1], px[0]);
    for (y, out) in pixels.chunks_exact_mut(w).enumerate() {
        let sy = y0 + y * ch / h;
        let sy = if p.stride < 0 { p.height - 1 - sy } else { sy };
        let line = bytes[sy * row..sy * row + p.width * 4].as_chunks::<4>().0;
        if w == cw {
            // Same size (the usual case): a straight pass the compiler
            // turns into vector code.
            for (o, px) in out.iter_mut().zip(&line[x0..x0 + cw]) {
                *o = bgrx(px);
            }
        } else {
            for (x, o) in out.iter_mut().enumerate() {
                *o = bgrx(&line[x0 + x * cw / w]);
            }
        }
    }
    pool.keep(&image);
    Some(Frame { pts, image })
}

/// For `cargo xtask medir`: writes an H.264 MP4 of `secs` seconds at 720p
/// and 30 frames a second, a pattern that moves (so every picture costs
/// what a real one does), with Windows' own encoder.
pub fn write_test_video(path: &std::path::Path, secs: u32) -> Result<(), String> {
    write_video(path, secs, 1280, 720)
}

fn write_video(path: &std::path::Path, secs: u32, w: u32, h: u32) -> Result<(), String> {
    use windows::Win32::Media::MediaFoundation::{
        MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_INTERLACE_MODE, MF_MT_PIXEL_ASPECT_RATIO,
        MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFVideoFormat_H264,
        MFVideoInterlace_Progressive,
    };
    #[allow(non_snake_case)]
    let (W, H) = (w, h);
    const FPS: u32 = 30;
    let e = |e: windows::core::Error| e.to_string();
    // COM in whatever model the thread already has (winsys.rs).
    let _com = crate::winsys::Com::multithreaded()?;
    // SAFETY: Media Foundation is started and stopped around the writer on
    // this thread; the buffer is written only while locked, and within the
    // length it was made with.
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(e)?;
        let written = (|| -> Result<(), String> {
            let size = (u64::from(W) << 32) | u64::from(H);
            let rate = (u64::from(FPS) << 32) | 1;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None)
                .map_err(e)?;
            let out = MFCreateMediaType().map_err(e)?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(e)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(e)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, 4_000_000).map_err(e)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(e)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(e)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(e)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
                .map_err(e)?;
            let stream = writer.AddStream(&out).map_err(e)?;
            let input = MFCreateMediaType().map_err(e)?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(e)?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(e)?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(e)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(e)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(e)?;
            input
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
                .map_err(e)?;
            writer.SetInputMediaType(stream, &input, None).map_err(e)?;
            writer.BeginWriting().map_err(e)?;
            let bytes = W * H * 4;
            let tick = TICKS as i64 / i64::from(FPS);
            for n in 0..secs * FPS {
                let buffer = MFCreateMemoryBuffer(bytes).map_err(e)?;
                let mut data = std::ptr::null_mut();
                buffer.Lock(&mut data, None, None).map_err(e)?;
                let px = std::slice::from_raw_parts_mut(data, bytes as usize);
                // A gradient that slides, and a bar that crosses it.
                let bar = (n * 12) % W;
                for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (i as u32 % W, i as u32 / W);
                    let on_bar = x.abs_diff(bar) < 24;
                    p[0] = if on_bar {
                        255
                    } else {
                        ((x + n * 4) % 256) as u8
                    };
                    p[1] = ((y + n * 2) % 256) as u8;
                    p[2] = ((x ^ y) % 256) as u8;
                    p[3] = 255;
                }
                buffer.Unlock().map_err(e)?;
                buffer.SetCurrentLength(bytes).map_err(e)?;
                let sample = MFCreateSample().map_err(e)?;
                sample.AddBuffer(&buffer).map_err(e)?;
                sample.SetSampleTime(i64::from(n) * tick).map_err(e)?;
                sample.SetSampleDuration(tick).map_err(e)?;
                writer.WriteSample(stream, &sample).map_err(e)?;
            }
            writer.Finalize().map_err(e)
        })();
        let _ = MFShutdown();
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pixels as RGBA bytes.
    fn rgba(f: &Frame) -> Vec<u8> {
        f.image.pixels.iter().flat_map(|c| c.to_array()).collect()
    }

    #[test]
    fn pictures_fit_and_keep_their_colours() {
        let mut pool = Pictures::new(4);
        assert_eq!(fit(1920, 1080), (1280, 720));
        assert_eq!(fit(640, 360), (640, 360));
        assert_eq!(fit(1080, 1920), (405, 720));
        // 2x1 BGRX: blue, then red.
        let bytes = [255, 0, 0, 0, 0, 0, 255, 0];
        let p = Picture {
            width: 2,
            height: 1,
            stride: 8,
            crop: (0, 0, 2, 1),
        };
        let f = to_frame(&bytes, p, 1.5, &mut pool).unwrap();
        assert_eq!(f.image.size, [2, 1]);
        assert_eq!(rgba(&f), [0, 0, 255, 255, 255, 0, 0, 255]);
        assert_eq!(f.pts, 1.5);
        // Bottom-up rows come out the right way up.
        let bytes = [1, 1, 1, 0, 2, 2, 2, 0];
        let p = Picture {
            width: 1,
            height: 2,
            stride: -4,
            crop: (0, 0, 1, 2),
        };
        let f = to_frame(&bytes, p, 0.0, &mut pool).unwrap();
        assert_eq!(&rgba(&f)[..3], &[2, 2, 2]);
        assert!(to_frame(&[0; 4], p, 0.0, &mut pool).is_none());
    }

    #[test]
    fn bigger_pictures_are_scaled_down() {
        // 2560x1 (twice MAX_W): every other pixel is kept.
        let mut bytes = Vec::new();
        for x in 0..2560u32 {
            bytes.extend_from_slice(&[(x % 251) as u8, 0, 0, 0]);
        }
        let p = Picture {
            width: 2560,
            height: 1,
            stride: 2560 * 4,
            crop: (0, 0, 2560, 1),
        };
        let f = to_frame(&bytes, p, 0.0, &mut Pictures::new(1)).unwrap();
        assert_eq!(f.image.size, [1280, 1]);
        assert_eq!(f.image.pixels[3].b(), 6);
    }

    #[test]
    fn decoder_filler_is_cut_off() {
        // 2x3 with the last line as filler (green, as decoders leave it),
        // and a crop that keeps the right column of the first two lines.
        let bytes = [
            1, 1, 1, 0, 2, 2, 2, 0, //
            3, 3, 3, 0, 4, 4, 4, 0, //
            0, 255, 0, 0, 0, 255, 0, 0,
        ];
        let p = Picture {
            width: 2,
            height: 3,
            stride: 8,
            crop: (1, 0, 1, 2),
        };
        let mut pool = Pictures::new(4);
        let f = to_frame(&bytes, p, 0.0, &mut pool).unwrap();
        assert_eq!(f.image.size, [1, 2]);
        assert_eq!(rgba(&f), [2, 2, 2, 255, 4, 4, 4, 255]);
        // A crop that does not fit the picture is refused.
        let bad = Picture {
            crop: (1, 0, 2, 2),
            ..p
        };
        assert!(to_frame(&bytes, bad, 0.0, &mut pool).is_none());
    }

    /// The first `n` pictures a reader hands out, as `play` makes them.
    unsafe fn first_pictures(reader: &IMFSourceReader, n: usize) -> Vec<Frame> {
        let mut pool = Pictures::new(n + 1);
        let mut frames = Vec::new();
        // SAFETY: as in `play`, on the test's thread.
        unsafe {
            let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0;
            assert!(select(
                reader,
                stream,
                &MFMediaType_Video,
                &MFVideoFormat_RGB32
            ));
            let mut pic = reader
                .GetCurrentMediaType(stream as u32)
                .ok()
                .as_ref()
                .and_then(picture);
            while frames.len() < n {
                let (mut flags, mut stamp, mut sample) = (0u32, 0i64, None);
                reader
                    .ReadSample(
                        stream as u32,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut stamp),
                        Some(&mut sample),
                    )
                    .unwrap();
                if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                    pic = reader
                        .GetCurrentMediaType(stream as u32)
                        .ok()
                        .as_ref()
                        .and_then(picture);
                }
                assert!(flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 == 0);
                let Some(sample) = sample else { continue };
                let buffer = sample.ConvertToContiguousBuffer().unwrap();
                let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
                buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                let bytes = std::slice::from_raw_parts(data, len as usize);
                let p = pic.unwrap();
                frames.push(to_frame(bytes, packed(p, bytes.len()), 0.0, &mut pool).unwrap());
                let _ = buffer.Unlock();
            }
        }
        frames
    }

    /// A video whose width is not a multiple of 16 (the card's texture is
    /// wider than the picture) plays on the graphics card the same as on
    /// the processor. `cargo test -p babutela mf::tests::on_the_card -- --ignored`
    #[test]
    #[ignore = "precisa de uma placa de vídeo"]
    fn on_the_card_pictures_are_the_same() {
        let path = std::env::temp_dir().join("babutela-teste-854x480.mp4");
        write_video(&path, 1, 854, 480).unwrap();
        let _com = crate::winsys::Com::multithreaded().unwrap();
        // SAFETY: Media Foundation around everything that uses it.
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
            let source = path.to_string_lossy().into_owned();
            let manager = card().expect("sem placa de vídeo");
            let on_card = first_pictures(&open(&source, Some(&manager)).unwrap(), 10);
            let on_cpu = first_pictures(&open(&source, None).unwrap(), 10);
            for (i, (a, b)) in on_card.iter().zip(&on_cpu).enumerate() {
                assert_eq!(a.image.size, [854, 480], "imagem {i}");
                assert_eq!(a.image.size, b.image.size, "imagem {i}");
                // Both conversions round their own way: on average, within
                // a step or two of each other, never a skewed row.
                let diff: u64 = a
                    .image
                    .pixels
                    .iter()
                    .zip(&b.image.pixels)
                    .map(|(x, y)| {
                        let (x, y) = (x.to_array(), y.to_array());
                        (0..3).map(|c| u64::from(x[c].abs_diff(y[c]))).sum::<u64>()
                    })
                    .sum();
                let mean = diff as f64 / (854.0 * 480.0 * 3.0);
                assert!(mean < 3.0, "imagem {i}: diferença média {mean:.2}");
            }
            let _ = MFShutdown();
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn packed_rows_from_the_card_are_read_as_packed() {
        // The card's texture is 864 wide for an 854-wide video; locked, the
        // rows come packed.
        let p = Picture {
            width: 854,
            height: 2,
            stride: 864 * 4,
            crop: (0, 0, 854, 2),
        };
        assert_eq!(packed(p, 854 * 4 * 2).stride, 854 * 4);
        // A buffer that does hold the padding keeps it, and so does one
        // laid out bottom-up.
        assert_eq!(packed(p, 864 * 4 * 2).stride, 864 * 4);
        let up = Picture {
            stride: -(854 * 4),
            ..p
        };
        assert_eq!(packed(up, 854 * 4 * 2).stride, -(854 * 4));
    }

    #[test]
    fn sound_from_before_a_seek_is_cut_whole_frames() {
        // A buffer 10 ms early at 48 kHz stereo: 480 frames, 960 samples.
        assert_eq!(early_samples(0.010, 48_000, 2), 960);
        assert_eq!(early_samples(0.0, 48_000, 2), 0);
        assert_eq!(early_samples(-0.5, 44_100, 1), 0);
        assert_eq!(early_samples(1.0 / 3.0, 3, 6) % 6, 0);
    }
}

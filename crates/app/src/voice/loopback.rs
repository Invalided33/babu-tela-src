//! The PC's sound for a screen share, taken as Windows plays it: WASAPI's
//! process loopback, in Windows 10 version 2004 and later (what OBS's
//! application audio capture uses too). A whole screen takes everything but
//! this app, whose sound is the call itself and would come back to everyone
//! as an echo; a window takes only its own program, and the processes that
//! program started (a browser plays its sound from one of those).
//!
//! What comes out is 48 kHz stereo, handed over as Windows delivers it
//! (every 10 ms or so). While nothing plays, nothing comes at all.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_BLOB;
use windows_core::{HRESULT, IUnknown, Interface, Ref, implement};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use super::audio::RATE;
use super::sources::Source;

/// For tests: the process id of a sound player of their own, whose sound
/// is taken instead (several copies of the app on one PC would otherwise
/// take each other's calls). Only in the app's test mode.
pub const TEST_ENV: &str = "BABUTELA_TESTE_SOM_DE";

/// How much Windows keeps for us between reads (100 ns units: 100 ms).
const BUFFER: i64 = 1_000_000;

/// What to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Everything but this process and the ones it started.
    AllBut(u32),
    /// Only this process and the ones it started.
    Only(u32),
}

impl Target {
    /// What goes with showing `source`: for a screen, the whole PC's sound
    /// but this app's; for a window, its program's. Nothing for the camera,
    /// or for a window of this very app.
    pub fn of(source: Source) -> Option<Target> {
        if crate::smoke::active()
            && let Some(pid) = std::env::var(TEST_ENV).ok().and_then(|p| p.parse().ok())
        {
            return Some(Target::Only(pid));
        }
        let me = std::process::id();
        match source {
            Source::Camera => None,
            Source::Monitor(_) => Some(Target::AllBut(me)),
            Source::Window(hwnd) => window_process(hwnd)
                .filter(|pid| *pid != me)
                .map(Target::Only),
        }
    }
}

/// The process a window belongs to.
fn window_process(hwnd: isize) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: a read-only question about a window handle; a stale one
    // answers 0.
    unsafe { GetWindowThreadProcessId(hwnd as HWND, &mut pid) };
    (pid != 0).then_some(pid)
}

/// A capture running on a thread of its own, which stops when this is
/// dropped (within a tenth of a second, without waiting for it here).
pub struct Loopback {
    stop: Arc<AtomicBool>,
    failure: Arc<OnceLock<String>>,
}

impl Loopback {
    /// Why the capture stopped by itself (Windows refused it, or the
    /// device went away), once it has.
    pub fn failure(&self) -> Option<&str> {
        self.failure.get().map(String::as_str)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts taking `target`'s sound, handing each piece of it to `sound`:
/// 48 kHz stereo samples, interleaved.
pub fn start(
    target: Target,
    mut sound: impl FnMut(&[f32]) + Send + 'static,
) -> Result<Loopback, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(OnceLock::new());
    let (thread_stop, thread_failure) = (stop.clone(), failure.clone());
    std::thread::Builder::new()
        .name("som-da-tela".into())
        .spawn(move || {
            if let Err(e) = run(target, &thread_stop, &mut sound) {
                crate::logln!("som da tela: {e}");
                let _ = thread_failure.set(e);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Loopback { stop, failure })
}

fn text(e: windows_core::Error) -> String {
    e.message()
}

fn run(target: Target, stop: &AtomicBool, sound: &mut dyn FnMut(&[f32])) -> Result<(), String> {
    // COM for this thread, in whatever model it already has (winsys.rs).
    let _com = crate::winsys::Com::multithreaded()?;
    capture(target, stop, sound)
}

/// Told by Windows when the activation is done.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<Option<mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        if let Some(done) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = done.send(());
        }
        Ok(())
    }
}

/// An audio client on the process loopback of `target`.
fn activate(target: Target) -> Result<IAudioClient, String> {
    let (pid, mode) = match target {
        Target::AllBut(pid) => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
        Target::Only(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
    };
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    // The parameters go as a blob that points at `params`. Never cleared:
    // PropVariantClear would try to free memory that is not COM's.
    let mut blob = ManuallyDrop::new(PROPVARIANT::default());
    // SAFETY: plain writes into the variant's fields; `params` outlives
    // every use of the blob (the activation copies it before returning).
    unsafe {
        let v = &mut *blob.Anonymous.Anonymous;
        v.vt = VT_BLOB;
        v.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&raw const params).cast_mut().cast(),
        };
    }
    let (done, finished) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler =
        Activated(Mutex::new(Some(done))).into();
    // SAFETY: every pointer is valid for the call; the handler is agile, as
    // the activation requires (`implement` makes it so).
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&raw const *blob),
            &handler,
        )
    }
    .map_err(text)?;
    finished
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "o Windows não respondeu".to_owned())?;
    let mut result = HRESULT(0);
    let mut client: Option<IUnknown> = None;
    // SAFETY: the operation is complete (its handler said so).
    unsafe { operation.GetActivateResult(&mut result, &mut client) }.map_err(text)?;
    result.ok().map_err(text)?;
    client
        .ok_or_else(|| "o Windows não entregou o áudio".to_owned())?
        .cast::<IAudioClient>()
        .map_err(text)
}

/// 16-bit stereo at the call's rate: what the process loopback converts
/// to (it has no format of its own to ask for).
fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 2,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

fn capture(target: Target, stop: &AtomicBool, sound: &mut dyn FnMut(&[f32])) -> Result<(), String> {
    let client = activate(target)?;
    let format = format();
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    // SAFETY: a format that lives across the call; asking for conversion
    // first, and without it if this Windows refuses the flag.
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER,
                0,
                &format,
                None,
            )
            .or_else(|_| {
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, &format, None)
            })
    }
    .map_err(text)?;
    // SAFETY: an unnamed auto-reset event, closed below.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        return Err("não foi possível criar o evento do áudio".into());
    }
    let out = read(&client, event, stop, sound);
    // SAFETY: the event made above, no longer waited on.
    unsafe { CloseHandle(event) };
    out
}

/// Reads what Windows hands over until `stop`.
fn read(
    client: &IAudioClient,
    event: windows_sys::Win32::Foundation::HANDLE,
    stop: &AtomicBool,
    sound: &mut dyn FnMut(&[f32]),
) -> Result<(), String> {
    // SAFETY: the client is initialized; the event lives longer than it
    // is used.
    let reader: IAudioCaptureClient = unsafe {
        client.SetEventHandle(HANDLE(event)).map_err(text)?;
        client.GetService().map_err(text)?
    };
    // SAFETY: started here, stopped at the end.
    unsafe { client.Start() }.map_err(text)?;
    let mut pcm: Vec<f32> = Vec::new();
    let out = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        // SAFETY: waiting on our own event; a tenth of a second at most,
        // so a stop is seen soon even while nothing plays.
        if unsafe { WaitForSingleObject(event, 100) } != WAIT_OBJECT_0 {
            continue;
        }
        if let Err(e) = drain(&reader, &mut pcm, stop, sound) {
            break Err(e);
        }
    };
    // SAFETY: the client started above.
    let _ = unsafe { client.Stop() };
    out
}

/// Every packet that is waiting, handed to `sound`.
fn drain(
    reader: &IAudioCaptureClient,
    pcm: &mut Vec<f32>,
    stop: &AtomicBool,
    sound: &mut dyn FnMut(&[f32]),
) -> Result<(), String> {
    loop {
        // SAFETY: plain calls on the capture client; the buffer it hands
        // over is read before it is released.
        let frames = unsafe { reader.GetNextPacketSize() }.map_err(text)?;
        if frames == 0 {
            return Ok(());
        }
        let mut data = std::ptr::null_mut();
        let mut count = 0u32;
        let mut flags = 0u32;
        unsafe { reader.GetBuffer(&mut data, &mut count, &mut flags, None, None) }.map_err(text)?;
        pcm.clear();
        if data.is_null() || flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            pcm.resize(count as usize * 2, 0.0);
        } else {
            // SAFETY: `count` frames of two 16-bit samples, readable until
            // ReleaseBuffer.
            let bytes = unsafe { std::slice::from_raw_parts(data, count as usize * 4) };
            pcm.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0),
            );
        }
        unsafe { reader.ReleaseBuffer(count) }.map_err(text)?;
        if !stop.load(Ordering::Relaxed) {
            sound(pcm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_leaves_this_app_out_and_a_window_takes_its_program() {
        let me = std::process::id();
        assert_eq!(Target::of(Source::Camera), None);
        assert_eq!(Target::of(Source::Monitor(1)), Some(Target::AllBut(me)));
        // A handle that is no window: nothing to take.
        assert_eq!(Target::of(Source::Window(0)), None);
    }

    #[test]
    fn the_format_is_the_calls_rate_in_stereo() {
        let f = format();
        let (rate, channels, align) = (f.nSamplesPerSec, f.nChannels, f.nBlockAlign);
        assert_eq!((rate, channels, align), (RATE, 2, 4));
    }

    /// Takes a second of what this PC plays (minus this test). Needs
    /// Windows 10 2004 or later and a sound device; it only checks that
    /// Windows accepts the capture and hands over well-formed audio.
    #[test]
    #[ignore = "precisa de um dispositivo de som"]
    fn windows_accepts_the_capture() {
        let got = Arc::new(Mutex::new(0usize));
        let seen = got.clone();
        let capture = start(Target::AllBut(std::process::id()), move |pcm| {
            assert_eq!(pcm.len() % 2, 0);
            *seen.lock().unwrap() += pcm.len();
        })
        .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(capture.failure(), None);
        drop(capture);
        println!("amostras: {}", got.lock().unwrap());
    }
}

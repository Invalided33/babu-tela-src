//! A screen or a window shared without its pictures ever coming to the
//! processor, which is how the app shares them whenever the graphics card
//! can. Windows Graphics Capture (Windows 10 1803 and later) hands each
//! picture over as a Direct3D 11 texture, cursor already drawn in and
//! upright even on turned screens, from a monitor or a single window
//! alike. The card's video processor scales it and turns it into NV12 for
//! each copy, and the card's own encoder (Media Foundation's hardware
//! encoder: NVENC, Quick Sync or AMF) encodes it from there. All that
//! comes back are the encoded pictures, a few KB each.
//!
//! The encoder makes baseline H.264 (what every copy of the app decodes,
//! with openh264) in the colours openh264 expects (BT.601, limited range),
//! and every full picture starts with the stream's parameters, as the
//! receiving side expects (assemble.rs). When the card encodes AV1 too and
//! everyone in the call decodes it (plan.rs `pick_codec`), the copies go
//! out in AV1 instead, at `AV1_SHARE` percent of the bitrate (av1.rs
//! decodes them).
//!
//! My own tile is drawn on the card as well, into textures egui's
//! Direct3D 12 device opens and draws from (interop.rs) when egui draws on
//! the same card; on another card it is read back into memory.
//!
//! When the card stops encoding in the middle of a share (its driver
//! reset, or the encoder kept refusing pictures), the share carries on
//! through the processor (video.rs `Sending::heal`), as it starts there
//! when the card has no H.264 encoder, Windows is too old, or
//! `BABUTELA_SEM_PLACA=1` is set. `cargo xtask medir captura` compares the
//! two paths.
//!
//! The frame pool is made from the same Direct3D 11 device the encoder
//! uses, so there is no cross-device copy: the texture Windows hands over
//! goes straight into the processor. HDR screens are read as the capture
//! API's SDR tonemap of them (close enough; a full HDR path would use the
//! `R16G16B16A16Float` pool format and Direct2D's tonemap effect ahead of
//! the processor).

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use egui::ColorImage;
use tokio::sync::mpsc::UnboundedSender;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_FAIL, GENERIC_ALL, HMODULE, HWND, LUID, RECT, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_SHARED,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice, ID3D11Device, ID3D11Device5,
    ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory1, IDXGIResource1,
};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxQP, CODECAPI_AVEncVideoMinQP,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager,
    IMFMediaEventGenerator, IMFSample, IMFShutdown, IMFTransform,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformHaveOutput, METransformNeedInput,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFNominalRange_16_235,
    MFSTARTUP_NOSOCKET, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT601, eAVEncAV1VProfile_Main_420_8,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{GUID, Interface, PCWSTR, PWSTR};

use super::interop;
use super::plan::{Codec, Copies, Layer, Spec};
use super::video::{
    EncodedFrame, IDLE, KEY_EVERY, KEY_GAP, LayerControl, REPEAT_AFTER, TILE_GRACE, VideoShared,
    fit,
};
use crate::pictures::Pictures;

/// Measuring (`BABUTELA_MEDIR=1`), a whole screen goes through the card
/// only when the smoke test's `share:screen-zero` sets this, and through
/// the processor (video.rs) otherwise, so `cargo xtask medir captura` can
/// compare the two. Without measuring, screens and windows always try the
/// card first.
pub static WANTED: AtomicBool = AtomicBool::new(false);

/// NV12 pictures each copy draws into in turn: the encoder may still be
/// reading the one before.
const RING: usize = 3;

/// The encoder's quantizers: the range openh264 keeps to for screens (its
/// MIN_SCREEN_QP and MAX_SCREEN_QP, which video.rs's encoder gets), so
/// both paths make pictures of the same quality. It matters below too: at
/// the lowest ones NVENC writes coefficients openh264 cannot read.
const MIN_QP: u32 = 26;
const MAX_QP: u32 = 35;

/// How often my own tile is drawn again (as video.rs does).
const PREVIEW_EVERY: Duration = Duration::from_millis(66);

/// The share of H.264's bitrate an AV1 copy gets, in percent: AV1 needs
/// about 30 % fewer bits for the same picture.
const AV1_SHARE: u32 = 70;

/// What the share's thread and its owner tell each other.
#[derive(Default)]
struct Flags {
    /// The owner is done with the share.
    stop: AtomicBool,
    /// The card stopped encoding (its device was lost, or the encoder
    /// refused `FAIL_LIMIT` pictures within `FAIL_WINDOW`): the owner
    /// carries the share on through the processor.
    failed: AtomicBool,
    /// The shared window was closed: the owner stops the share.
    ended: AtomicBool,
}

/// The share; dropping it stops it.
pub struct Zero {
    flags: Arc<Flags>,
    /// Wakes the share's thread to see it was stopped.
    bell: Arc<LayerControl>,
}

impl Zero {
    /// The card stopped encoding: time for the processor path
    /// (video.rs `Sending::heal`).
    pub fn failed(&self) -> bool {
        self.flags.failed.load(Ordering::Relaxed)
    }

    /// The shared window was closed.
    pub fn ended(&self) -> bool {
        self.flags.ended.load(Ordering::Relaxed)
    }
}

impl Drop for Zero {
    fn drop(&mut self) {
        self.flags.stop.store(true, Ordering::Relaxed);
        self.bell.ring();
    }
}

/// The card's encoder may refuse a picture now and then (it is made again
/// and tried after `RETRY_AFTER`); this many refusals within `FAIL_WINDOW`
/// and the share leaves the card. With the encoder's own waits, that is
/// about two seconds without pictures.
const FAIL_LIMIT: usize = 3;
const FAIL_WINDOW: Duration = Duration::from_secs(10);
const RETRY_AFTER: Duration = Duration::from_millis(500);

/// What one copy's step came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// A picture went out, or none was due.
    Ok,
    /// Nobody listens any more: the share is over.
    Dead,
    /// The card's encoder refused the picture (it is made again later).
    Failed,
}

/// Starts sharing `source` (a monitor or a window): the copies `control`
/// asks for go to `out`, and a small view of it shows as `me` in the call
/// view. Fails when the card has no H.264 encoder, Windows is too old for
/// Graphics Capture, or `BABUTELA_SEM_PLACA=1` is set.
pub(super) fn start(
    source: super::sources::Source,
    copies: Copies,
    out: UnboundedSender<EncodedFrame>,
    control: Arc<LayerControl>,
    preview: Arc<VideoShared>,
    me: i64,
) -> Result<Zero, String> {
    if std::env::var_os("BABUTELA_SEM_PLACA").is_some() {
        return Err("desligado por BABUTELA_SEM_PLACA".into());
    }
    // Windows 10 1803 and later; a plain `false` the share path can read.
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err("essa versão do Windows não captura em placa".into());
    }
    let flags = Arc::new(Flags::default());
    let running = flags.clone();
    let bell = control.clone();
    let (opened, ready) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("video-placa".into())
        .spawn(move || {
            let _media = match Media::start() {
                Ok(m) => m,
                Err(e) => {
                    let _ = opened.send(Err(e));
                    return;
                }
            };
            match open(&source) {
                Ok((gpu, item, name)) => {
                    // One picture end-to-end before the share runs: the
                    // card has an H.264 encoder (found by `open`), but it
                    // may still fail with the settings this path wants. A
                    // failure here lets the caller fall through to the
                    // processor path without a stuck share. Takes ~50 ms
                    // (one encode on the card).
                    if let Err(e) = smoke_test(&gpu, Codec::H264) {
                        let _ = opened.send(Err(format!("teste falhou: {e}")));
                        return;
                    }
                    // AV1 too, when the card has the encoder and it takes
                    // these settings: the call may then go out in AV1.
                    let av1 = find_encoder(gpu.vendor, Codec::Av1)
                        .and_then(|_| smoke_test(&gpu, Codec::Av1));
                    if let Err(e) = &av1 {
                        crate::logln!("vídeo: sem AV1 na placa ({e})");
                    }
                    let screen = match Screen::start(&gpu, &item, control.clone()) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = opened.send(Err(e));
                            return;
                        }
                    };
                    control.set_encodes_av1(av1.is_ok());
                    let _ = opened.send(Ok(name));
                    run(
                        &gpu,
                        &screen,
                        copies,
                        &out,
                        &control,
                        &preview,
                        me,
                        &running,
                        av1.is_ok(),
                    );
                    control.set_encodes_av1(false);
                }
                Err(e) => {
                    let _ = opened.send(Err(e));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let name = ready.recv().map_err(|e| e.to_string())??;
    crate::logln!("vídeo: a tela pela placa de vídeo, com {name}");
    Ok(Zero { flags, bell })
}

/// One picture through the whole pipeline: a blank BGRA texture turned
/// into NV12 by the processor and encoded by the card in `codec`. Checks
/// the parts of hw.rs the capture path (`open` + `find_encoder`) does not
/// touch (encoder settings, output type, async events).
fn smoke_test(gpu: &Gpu, codec: Codec) -> Result<(), String> {
    let (w, h) = (640u32, 360u32);
    let texture = gpu
        .texture(
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            D3D11_USAGE_DEFAULT,
            0,
        )
        .map_err(text)?;
    let picture = Picture {
        texture,
        width: w,
        height: h,
        id: 1,
    };
    let mut scaler = Scaler::new(gpu, &picture, (w, h), DXGI_FORMAT_NV12, 2).map_err(text)?;
    let mut encoder = Encoder::new(gpu, (w, h), 30, 500, codec)?;
    let nv12 = scaler.draw(gpu).map_err(text)?;
    let made = encoder.encode(
        &nv12,
        Duration::ZERO,
        Duration::from_millis(33),
        true,
        Instant::now(),
    )?;
    if made.is_empty() || made[0].data.is_empty() {
        return Err("o codificador não entregou um quadro".into());
    }
    Ok(())
}

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

fn failed() -> windows::core::Error {
    windows::core::Error::from(E_FAIL)
}

/// COM and Media Foundation, while the thread runs.
struct Media {
    /// Last to go (fields drop in order, after `drop` below).
    _com: crate::winsys::Com,
}

impl Media {
    fn start() -> Result<Media, String> {
        // COM in whatever model the thread already has (winsys.rs).
        let com = crate::winsys::Com::multithreaded()?;
        // SAFETY: a plain call, undone in Drop on the same thread.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(text)?;
        Ok(Media { _com: com })
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        // SAFETY: undoes `start`, on the same thread, once everything made
        // with them is gone (this is the first thing the thread makes).
        unsafe {
            let _ = MFShutdown();
        }
    }
}

// ------------------------------------------------ the card

/// The graphics card the screen is on, shared with its encoder.
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    manager: IMFDXGIDeviceManager,
    /// Its maker (the PCI vendor id), to pick its own encoder.
    vendor: u32,
    /// Which card it is, to tell whether egui draws on the same one.
    luid: LUID,
}

impl Gpu {
    fn new(adapter: &IDXGIAdapter1) -> Result<Gpu, String> {
        // SAFETY: plain COM calls; every out pointer is a local.
        unsafe {
            let desc = adapter.GetDesc1().map_err(text)?;
            let (vendor, luid) = (desc.VendorId, desc.AdapterLuid);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(text)?;
            let device: ID3D11Device = device.ok_or("sem dispositivo Direct3D 11")?;
            let context: ID3D11DeviceContext = context.ok_or("sem contexto Direct3D 11")?;
            // The encoder uses the device from threads of its own.
            let threads: ID3D11Multithread = context.cast().map_err(text)?;
            let _ = threads.SetMultithreadProtected(true);
            let video: ID3D11VideoDevice = device.cast().map_err(text)?;
            let video_context: ID3D11VideoContext1 = context.cast().map_err(text)?;
            let mut token = 0;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(text)?;
            let manager = manager.ok_or("sem gerenciador DXGI")?;
            manager.ResetDevice(&device, token).map_err(text)?;
            Ok(Gpu {
                device,
                context,
                video,
                video_context,
                manager,
                vendor,
                luid,
            })
        }
    }

    /// A BGRA texture another device on this card can open, through the NT
    /// handle that comes with it.
    fn shared_texture(
        &self,
        width: u32,
        height: u32,
    ) -> windows::core::Result<(ID3D11Texture2D, interop::Handle)> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            // Kept apart from egui's device by fences (interop.rs), not by
            // a keyed mutex.
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
        };
        let mut made = None;
        // SAFETY: a plain description and an out pointer to a local; the
        // handle is made for the texture just made.
        unsafe {
            self.device.CreateTexture2D(&desc, None, Some(&mut made))?;
            let texture = made.ok_or_else(failed)?;
            let handle = texture.cast::<IDXGIResource1>()?.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )?;
            Ok((texture, interop::Handle(handle)))
        }
    }

    /// A fence another device on this card can open, and its NT handle.
    fn shared_fence(&self) -> windows::core::Result<(ID3D11Fence, interop::Handle)> {
        let device: ID3D11Device5 = self.device.cast()?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: an out pointer to a local; the handle is made for the
        // fence just made.
        unsafe {
            device.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
            let fence = fence.ok_or_else(failed)?;
            let handle = fence.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())?;
            Ok((fence, interop::Handle(handle)))
        }
    }

    /// Whether the card's device is gone (a driver reset, the card taken
    /// out): nothing made on it works again.
    fn lost(&self) -> bool {
        // SAFETY: a plain query.
        unsafe { self.device.GetDeviceRemovedReason() }.is_err()
    }

    fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: u32,
        usage: D3D11_USAGE,
        cpu: u32,
    ) -> windows::core::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: usage,
            BindFlags: bind,
            CPUAccessFlags: cpu,
            MiscFlags: 0,
        };
        let mut made = None;
        // SAFETY: a plain description, and an out pointer to a local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut made))? };
        made.ok_or_else(failed)
    }
}

/// The screen's newest picture, in a texture of our own on the card.
#[derive(Clone)]
struct Picture {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    /// Which texture this is: views of an older one are stale.
    id: u64,
}

/// Where the capture callback hands fresh pictures over. The callback
/// runs on Windows Graphics Capture's own thread and rings the share's
/// thread awake (`LayerControl::ring`). Only the newest picture is kept
/// (the one before it was never processed goes back to the pool).
#[derive(Default)]
struct Slot {
    picture: Mutex<Option<Picture>>,
    /// Our cached picture texture (reused while the size stays).
    cached: Mutex<Option<Picture>>,
    /// The window being shared was closed (monitors do not close).
    ended: AtomicBool,
    stop: AtomicBool,
    /// Textures made so far: a change in size gives the next one a new id.
    made: AtomicU64,
}

impl Slot {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Everything held so the capture keeps running: the capture session, the
/// frame pool it reads from, and the handlers that write new pictures
/// into [`Slot`]. Dropping it stops the share.
struct Screen {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    closed_token: i64,
    slot: Arc<Slot>,
}

impl Drop for Screen {
    fn drop(&mut self) {
        // SAFETY: plain WinRT calls on self-owned handles.
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self._item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// The adapter the capture item lives on (its monitor, or the one its
/// window is on now), the gpu for it, and the item itself.
fn open(source: &super::sources::Source) -> Result<(Gpu, GraphicsCaptureItem, String), String> {
    let item_factory = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(text)?;
    let (adapter, item) = match source {
        super::sources::Source::Monitor(m) => {
            let hmon = HMONITOR(*m as *mut _);
            let adapter = adapter_for_monitor(hmon)?;
            // SAFETY: `hmon` is the HMONITOR from the Source variant; the
            // factory produces a GraphicsCaptureItem for it, in the only
            // out pointer the call writes to.
            let item: GraphicsCaptureItem =
                unsafe { item_factory.CreateForMonitor(hmon) }.map_err(text)?;
            (adapter, item)
        }
        super::sources::Source::Window(w) => {
            let hwnd = HWND(*w as *mut _);
            // SAFETY: a plain Win32 call; HWND is read.
            let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
            let adapter = adapter_for_monitor(hmon)?;
            // SAFETY: `hwnd` from the Source variant; out pointer written by factory.
            let item: GraphicsCaptureItem =
                unsafe { item_factory.CreateForWindow(hwnd) }.map_err(text)?;
            (adapter, item)
        }
        super::sources::Source::Camera => {
            return Err("placa de vídeo não compartilha câmera".into());
        }
    };
    let gpu = Gpu::new(&adapter)?;
    let (_, name) = find_encoder(gpu.vendor, Codec::H264)?;
    Ok((gpu, item, name))
}

/// Enumerates DXGI adapters and finds the one that owns `monitor`. Falls
/// back to the first adapter when the HMONITOR is not among the outputs
/// of any adapter (an unusual setup).
fn adapter_for_monitor(monitor: HMONITOR) -> Result<IDXGIAdapter1, String> {
    // SAFETY: plain COM calls.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(text)?;
        let mut a = 0;
        let mut fallback: Option<IDXGIAdapter1> = None;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            if fallback.is_none() {
                fallback = Some(adapter.clone());
            }
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                if let Ok(desc) = output.GetDesc()
                    && desc.Monitor == monitor
                {
                    return Ok(adapter);
                }
            }
        }
        fallback.ok_or_else(|| "o DXGI não achou nenhuma placa".into())
    }
}

impl Screen {
    /// Starts the capture: a frame pool on the same card as the encoder,
    /// a session that draws the pointer in, and the handler that moves
    /// each picture into [`Slot`] and rings `bell` (the share's thread
    /// sleeps on it).
    fn start(
        gpu: &Gpu,
        item: &GraphicsCaptureItem,
        bell: Arc<LayerControl>,
    ) -> Result<Screen, String> {
        // The frame pool draws into textures on our own device: there is
        // no cross-device copy on the way to the encoder.
        // SAFETY: both calls take COM pointers of this device and return
        // the WinRT device wrapper the frame pool asks for.
        let direct_device: IDirect3DDevice = unsafe {
            let dxgi_device: IDXGIDevice = gpu.device.cast().map_err(text)?;
            CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                .map_err(text)?
                .cast()
                .map_err(text)?
        };
        let size = item.Size().map_err(text)?;
        // Two buffers: one being filled, one being handed over. More just
        // lets stale pictures sit around (`TryGetNextFrame` returns one
        // and leaves the other, so we read the newest).
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(text)?;
        let session = pool.CreateCaptureSession(item).map_err(text)?;
        // The pointer, drawn in by Windows: our scaler reads it from the
        // picture as if it were any other pixels.
        session.SetIsCursorCaptureEnabled(true).map_err(text)?;
        // The yellow border Windows draws around a captured surface: not
        // every build takes `false`, so a failure here is swallowed.
        let _ = session.SetIsBorderRequired(false);

        let slot = Arc::new(Slot::default());
        let frame_slot = slot.clone();
        let gpu_arc = Arc::new(GpuHandle {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
        });
        let frame_gpu = gpu_arc.clone();
        let frame_pool = pool.clone();
        let frame_bell = bell.clone();
        let frame_token = pool
            .FrameArrived(&TypedEventHandler::new(move |_, _| {
                frame_arrived(&frame_pool, &frame_slot, &frame_gpu)?;
                frame_bell.ring();
                Ok(())
            }))
            .map_err(text)?;
        let closed_slot = slot.clone();
        let closed_token = item
            .Closed(&TypedEventHandler::new(move |_, _| {
                closed_slot.ended.store(true, Ordering::Relaxed);
                bell.ring();
                Ok(())
            }))
            .map_err(text)?;
        session.StartCapture().map_err(text)?;
        Ok(Screen {
            _item: item.clone(),
            pool,
            session,
            frame_token,
            closed_token,
            slot,
        })
    }

    /// The picture that came since the last call, if one did.
    fn take(&self) -> Option<Picture> {
        self.slot
            .picture
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn ended(&self) -> bool {
        self.slot.ended.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.slot.stop.store(true, Ordering::Relaxed);
    }
}

/// The device bits the capture callback needs. The callback is called by
/// Windows from its own thread, so these are shared by Arc.
struct GpuHandle {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl GpuHandle {
    fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: u32,
        usage: D3D11_USAGE,
        cpu: u32,
    ) -> windows::core::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: usage,
            BindFlags: bind,
            CPUAccessFlags: cpu,
            MiscFlags: 0,
        };
        let mut made = None;
        // SAFETY: plain description and out pointer to a local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut made))? };
        made.ok_or_else(failed)
    }
}

/// Called by Windows on its own thread for each new picture: copies it
/// into the slot, and gives the frame back to the pool.
fn frame_arrived(
    pool: &Direct3D11CaptureFramePool,
    slot: &Slot,
    gpu: &GpuHandle,
) -> windows::core::Result<()> {
    if slot.stopped() {
        return Ok(());
    }
    let reading = Instant::now();
    let frame = pool.TryGetNextFrame()?;
    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    // SAFETY: the surface is a Direct3D11 one, and `GetInterface` casts
    // its underlying DXGI object (an ID3D11Texture2D here).
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: out pointer to a local.
    unsafe { texture.GetDesc(&mut desc) };
    // The frame pool may hold a bigger buffer than the picture: look at
    // the picture's actual size (`ContentSize`), not the texture's. A
    // size change triggers `pool.Recreate`, not a smaller frame.
    let mut cached = slot.cached.lock().unwrap_or_else(PoisonError::into_inner);
    let fresh_size = cached
        .as_ref()
        .is_none_or(|p| (p.width, p.height) != (desc.Width, desc.Height));
    if fresh_size {
        let new_texture = gpu.texture(
            desc.Width,
            desc.Height,
            desc.Format,
            (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            D3D11_USAGE_DEFAULT,
            0,
        )?;
        let id = slot.made.fetch_add(1, Ordering::Relaxed) + 1;
        *cached = Some(Picture {
            texture: new_texture,
            width: desc.Width,
            height: desc.Height,
            id,
        });
    }
    let picture = cached.as_ref().ok_or_else(failed)?;
    // SAFETY: both textures are on this device and of the same size.
    unsafe { gpu.context.CopyResource(&picture.texture, &texture) };
    let fresh = Picture {
        texture: picture.texture.clone(),
        width: picture.width,
        height: picture.height,
        id: picture.id,
    };
    drop(cached);
    crate::probe::since("video.captura", reading);
    let _ = slot
        .picture
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(fresh);
    Ok(())
}

/// The card's video processor drawing the screen's picture at one size,
/// in NV12 for an encoder or BGRA for my own tile, into a few textures in
/// turn. A pointer, when one is on screen, is drawn on top as a second
/// stream with alpha blending.
struct Scaler {
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    size: (u32, u32),
    /// The picture texture it reads (`Picture::id`).
    source: u64,
}

impl Scaler {
    fn new(
        gpu: &Gpu,
        from: &Picture,
        size: (u32, u32),
        format: DXGI_FORMAT,
        count: usize,
    ) -> windows::core::Result<Scaler> {
        let (w, h) = size;
        let (in_w, in_h) = (from.width, from.height);
        let rate = DXGI_RATIONAL {
            Numerator: 30,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: w,
            OutputHeight: h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let whole = |w: u32, h: u32| RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        let (src, dst) = (whole(in_w, in_h), whole(w, h));
        // SAFETY: plain descriptions, and out pointers to locals; the views
        // are of textures of this same device.
        unsafe {
            let enumerator = gpu.video.CreateVideoProcessorEnumerator(&content)?;
            let processor = gpu.video.CreateVideoProcessor(&enumerator, 0)?;
            let mut input = None;
            gpu.video.CreateVideoProcessorInputView(
                &from.texture,
                &enumerator,
                &input_desc,
                Some(&mut input),
            )?;
            let input = input.ok_or_else(failed)?;
            let mut outputs = Vec::with_capacity(count);
            for _ in 0..count {
                let texture = gpu.texture(
                    w,
                    h,
                    format,
                    D3D11_BIND_RENDER_TARGET.0 as u32,
                    D3D11_USAGE_DEFAULT,
                    0,
                )?;
                let mut view = None;
                gpu.video.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &output_desc,
                    Some(&mut view),
                )?;
                outputs.push((texture, view.ok_or_else(failed)?));
            }
            let vc = &gpu.video_context;
            vc.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // No enhancements of the driver's own: the picture as it is.
            vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            vc.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            // What openh264 turns back into colours on the other end.
            let out_space = if format == DXGI_FORMAT_NV12 {
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601
            } else {
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
            };
            vc.VideoProcessorSetOutputColorSpace1(&processor, out_space);
            Ok(Scaler {
                processor,
                input,
                outputs,
                next: 0,
                size,
                source: from.id,
            })
        }
    }

    /// Draws the picture into the next texture, and returns the texture.
    fn draw(&mut self, gpu: &Gpu) -> windows::core::Result<ID3D11Texture2D> {
        let (texture, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        }];
        // SAFETY: the processor and both views belong to this device; the
        // stream holds a reference of its own to the input view, released
        // right after.
        let drawn = unsafe {
            gpu.video_context
                .VideoProcessorBlt(&self.processor, view, 0, &streams)
        };
        // SAFETY: taken once, from the stream made above.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        drawn.map(|()| texture.clone())
    }
}

// ------------------------------------------------ the encoder

/// The hardware encoder for `codec` of the card made by `vendor` (or the
/// first there is), and its name.
fn find_encoder(vendor: u32, codec: Codec) -> Result<(IMFActivate, String), String> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Av1 => MFVideoFormat_AV1,
        },
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out pointers to locals. Media Foundation allocates the list;
    // each entry is taken out of it before it is freed.
    let all: Vec<IMFActivate> = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output),
            &mut list,
            &mut count,
        )
        .map_err(text)?;
        if list.is_null() {
            Vec::new()
        } else {
            let all = std::slice::from_raw_parts_mut(list, count as usize)
                .iter_mut()
                .filter_map(Option::take)
                .collect();
            CoTaskMemFree(Some(list.cast_const().cast()));
            all
        }
    };
    let wanted = format!("VEN_{vendor:04X}");
    let pick = all
        .iter()
        .position(|a| {
            string(a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
                .is_some_and(|v| v.eq_ignore_ascii_case(&wanted))
        })
        .unwrap_or(0);
    let activate = all.into_iter().nth(pick).ok_or(match codec {
        Codec::H264 => "a placa de vídeo não tem codificador H.264",
        Codec::Av1 => "a placa de vídeo não tem codificador AV1",
    })?;
    let name = string(&activate, &MFT_FRIENDLY_NAME_Attribute)
        .unwrap_or_else(|| "o codificador da placa".into());
    Ok((activate, name))
}

/// A text attribute.
fn string(attributes: &IMFActivate, key: &GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut len = 0;
    // SAFETY: out pointers to locals; the string is Media Foundation's,
    // freed once copied.
    unsafe {
        attributes
            .GetAllocatedString(key, &mut value, &mut len)
            .ok()?;
        let s = value.to_string().ok();
        CoTaskMemFree(Some(value.0.cast_const().cast()));
        s
    }
}

/// A VT_UI4 VARIANT, what the encoder's numeric settings take.
fn number(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// A VT_BOOL VARIANT saying yes.
fn yes() -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_TRUE,
                },
            }),
        },
    }
}

/// What the encoder's events say.
enum Event {
    /// It takes another picture.
    Need,
    /// An encoded picture is ready.
    Have,
    Failed(windows::core::HRESULT),
    Other,
}

/// An interface used from another thread.
struct Agile<T>(T);

// SAFETY: asynchronous MFTs have to be free-threaded (Media Foundation's
// rule for them), so their event generator may be called from any thread.
unsafe impl<T> Send for Agile<T> {}

impl<T> Agile<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

/// Hands the encoder's events over as they come, from a thread of their
/// own (waiting for one blocks). It ends when the encoder is shut down.
fn listen(generator: IMFMediaEventGenerator) -> Result<Receiver<Event>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let generator = Agile(generator);
    std::thread::Builder::new()
        .name("video-placa-eventos".into())
        .spawn(move || {
            loop {
                // SAFETY: plain COM calls.
                let next = unsafe {
                    generator
                        .get()
                        .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
                };
                let Ok(event) = next else { break };
                // SAFETY: as above.
                let (kind, status) = unsafe { (event.GetType(), event.GetStatus()) };
                let kind = kind.unwrap_or(0);
                let event = match status {
                    Ok(s) if s.is_err() => Event::Failed(s),
                    _ if kind == METransformNeedInput.0 as u32 => Event::Need,
                    _ if kind == METransformHaveOutput.0 as u32 => Event::Have,
                    _ => Event::Other,
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

/// One encoded picture.
struct Made {
    /// H.264: Annex B, the parameters first if it is a full picture. AV1:
    /// the temporal unit's OBUs, the sequence header first if it is one.
    data: Vec<u8>,
    codec: Codec,
    key: bool,
    captured: Instant,
}

/// The card's encoder (H.264 or AV1), at one size.
struct Encoder {
    activate: IMFActivate,
    transform: IMFTransform,
    codec: ICodecAPI,
    events: Receiver<Event>,
    /// Pictures it asked for and has not been given.
    asked: u32,
    /// It hands out the buffers of its pictures itself (hardware ones do).
    provides: bool,
    /// The size of its buffers otherwise.
    out_size: u32,
    /// When each picture it was given and has not encoded yet was captured,
    /// and given to it.
    waiting: VecDeque<(Instant, Instant)>,
    ready: VecDeque<Made>,
    /// The stream's parameters, for the full pictures that come without
    /// them: SPS and PPS in Annex B (H.264), or the sequence header OBU
    /// (AV1).
    params: Vec<u8>,
    size: (u32, u32),
    makes: Codec,
}

impl Encoder {
    /// Set up as video.rs sets openh264: a constant bitrate, no pictures
    /// held back (low latency, no B pictures), full pictures when asked
    /// for and every 10 s at least.
    fn new(
        gpu: &Gpu,
        size: (u32, u32),
        fps: u32,
        kbps: u32,
        makes: Codec,
    ) -> Result<Encoder, String> {
        let (activate, _) = find_encoder(gpu.vendor, makes)?;
        let (w, h) = size;
        let frame_size = (u64::from(w) << 32) | u64::from(h);
        let rate = (u64::from(fps) << 32) | 1;
        let square = (1u64 << 32) | 1;
        let progressive = MFVideoInterlace_Progressive.0 as u32;
        // SAFETY: plain COM calls on the encoder just made, before it runs;
        // every out pointer is a local.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(text)?;
            let attributes = transform.GetAttributes().map_err(text)?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err("o codificador da placa não é assíncrono".into());
            }
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(text)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                .map_err(text)?;
            let codec: ICodecAPI = transform.cast().map_err(text)?;
            let cbr = eAVEncCommonRateControlMode_CBR.0 as u32;
            let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &number(cbr));
            let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &number(kbps * 1000));
            let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &yes());
            let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &number(0));
            let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &number(fps * 10));
            let (subtype, profile) = match makes {
                Codec::H264 => {
                    // With bits to spare (a still or simple screen) NVENC
                    // goes down to QP 0, where its coefficients need the
                    // long escape codes (level_prefix > 15) that openh264
                    // rejects: tried, the test below fails that way. AV1
                    // goes to dav1d, which takes everything AV1 allows.
                    codec
                        .SetValue(&CODECAPI_AVEncVideoMinQP, &number(MIN_QP))
                        .map_err(|e| format!("QP mínimo: {e}"))?;
                    let _ = codec.SetValue(&CODECAPI_AVEncVideoMaxQP, &number(MAX_QP));
                    (MFVideoFormat_H264, eAVEncH264VProfile_Base.0 as u32)
                }
                Codec::Av1 => (MFVideoFormat_AV1, eAVEncAV1VProfile_Main_420_8.0 as u32),
            };

            let output = MFCreateMediaType().map_err(text)?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(text)?;
            output.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(text)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)
                .map_err(text)?;
            output
                .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(text)?;
            output.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(text)?;
            output
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square)
                .map_err(text)?;
            output
                .SetUINT32(&MF_MT_INTERLACE_MODE, progressive)
                .map_err(text)?;
            // The same attribute (MF_MT_VIDEO_PROFILE) for either codec.
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
                .map_err(text)?;
            transform.SetOutputType(0, &output, 0).map_err(text)?;

            let mut input = None;
            let mut i = 0;
            while let Ok(t) = transform.GetInputAvailableType(0, i) {
                i += 1;
                if t.GetGUID(&MF_MT_SUBTYPE)
                    .is_ok_and(|g| g == MFVideoFormat_NV12)
                {
                    input = Some(t);
                    break;
                }
            }
            let input = input.ok_or("o codificador da placa não aceita NV12")?;
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(text)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(text)?;
            input
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, progressive)
                .map_err(text)?;
            let _ = input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32);
            let _ = input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            transform.SetInputType(0, &input, 0).map_err(text)?;

            let info = transform.GetOutputStreamInfo(0).map_err(text)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(text)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(text)?;
            let events = listen(transform.cast().map_err(text)?)?;
            Ok(Encoder {
                activate,
                transform,
                codec,
                events,
                asked: 0,
                provides,
                out_size: info.cbSize.max(w * h),
                waiting: VecDeque::new(),
                ready: VecDeque::new(),
                params: Vec::new(),
                size,
                makes,
            })
        }
    }

    fn set_bitrate(&self, kbps: u32) {
        let bps = number(kbps.saturating_mul(1000));
        // SAFETY: a plain call, with the type the setting takes.
        let _ = unsafe { self.codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &bps) };
    }

    /// Encodes `texture` (NV12 at this encoder's size), a full picture if
    /// `key`. Returns what came out by the time it was done: in low latency
    /// mode, that very picture.
    fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        time: Duration,
        length: Duration,
        key: bool,
        captured: Instant,
    ) -> Result<Vec<Made>, String> {
        let until = Instant::now() + Duration::from_millis(500);
        while self.asked == 0 {
            if !self.pump(until)? {
                return Err("o codificador da placa não pediu imagem".into());
            }
        }
        let sample = surface(texture, time, length).map_err(text)?;
        if key {
            // SAFETY: a plain call, with the type the setting takes.
            let _ = unsafe {
                self.codec
                    .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &number(1))
            };
        }
        // SAFETY: the sample holds the texture, of the encoder's device and
        // the size it was set to.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(text)?;
        self.asked -= 1;
        self.waiting.push_back((captured, Instant::now()));
        let until = Instant::now() + Duration::from_millis(250);
        while self.ready.is_empty() && !self.waiting.is_empty() {
            if !self.pump(until)? {
                break;
            }
        }
        Ok(self.ready.drain(..).collect())
    }

    /// Handles the encoder's next event, waiting until `until` at most.
    /// False if none came.
    fn pump(&mut self, until: Instant) -> Result<bool, String> {
        let wait = until.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(wait) {
            Ok(Event::Need) => self.asked += 1,
            Ok(Event::Have) => self.collect()?,
            Ok(Event::Failed(hr)) => {
                return Err(format!(
                    "o codificador da placa falhou: {}",
                    windows::core::Error::from(hr)
                ));
            }
            Ok(Event::Other) => {}
            Err(RecvTimeoutError::Timeout) => return Ok(false),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("o codificador da placa parou".into());
            }
        }
        Ok(true)
    }

    /// Takes the picture the encoder says is ready.
    fn collect(&mut self) -> Result<(), String> {
        // SAFETY: plain COM calls. The sample (the encoder's, or the one
        // given to it) is owned here, and read only while locked.
        unsafe {
            let given = if self.provides {
                None
            } else {
                let sample = MFCreateSample().map_err(text)?;
                let buffer = MFCreateMemoryBuffer(self.out_size).map_err(text)?;
                sample.AddBuffer(&buffer).map_err(text)?;
                Some(sample)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(given),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let done = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match done {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder settled its output: take it as it is.
                    let t = self.transform.GetOutputAvailableType(0, 0).map_err(text)?;
                    self.transform.SetOutputType(0, &t, 0).map_err(text)?;
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(text(e)),
            }
            let Some(sample) = sample else {
                return Ok(());
            };
            let bits = read(&sample).map_err(text)?;
            let (captured, given) = self
                .waiting
                .pop_front()
                .unwrap_or_else(|| (Instant::now(), Instant::now()));
            crate::probe::since("video.codificacao", given);
            let (data, key) = match self.makes {
                Codec::H264 => {
                    if self.params.is_empty() && !units(&bits).iter().any(|n| n[0] & 0x1f == 7) {
                        self.params = self.header();
                    }
                    tidy(&bits, &mut self.params)
                }
                Codec::Av1 => {
                    let has_header = obus(&bits)
                        .is_some_and(|all| all.iter().any(|o| o.kind == OBU_SEQUENCE_HEADER));
                    if self.params.is_empty() && !has_header {
                        self.params = self.header();
                    }
                    tidy_av1(&bits, &mut self.params)
                }
            };
            if !data.is_empty() {
                self.ready.push_back(Made {
                    data,
                    codec: self.makes,
                    key,
                    captured,
                });
            }
        }
        Ok(())
    }

    /// The stream's parameters as the output type holds them: SPS and PPS
    /// in Annex B (H.264), or the sequence header OBU (AV1, whose blob is
    /// either an AV1CodecConfigurationRecord or the OBUs themselves).
    fn header(&self) -> Vec<u8> {
        let mut params = Vec::new();
        // SAFETY: plain COM calls; the blob is copied into a buffer of the
        // size asked for.
        let blob = unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(0) else {
                return params;
            };
            let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
                return params;
            };
            let mut blob = vec![0; n as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
                .is_err()
            {
                return params;
            }
            blob
        };
        match self.makes {
            Codec::H264 => {
                for nal in units(&blob) {
                    if matches!(nal[0] & 0x1f, 7 | 8) {
                        params.extend_from_slice(&START);
                        params.extend_from_slice(nal);
                    }
                }
            }
            Codec::Av1 => {
                // An av1C record starts with its marker and version (0x81)
                // and has the configuration OBUs after four bytes.
                let configs = match blob.first() {
                    Some(0x81) => blob.get(4..).unwrap_or_default(),
                    _ => &blob[..],
                };
                if let Some(seq) = obus(configs)
                    .and_then(|all| all.into_iter().find(|o| o.kind == OBU_SEQUENCE_HEADER))
                {
                    push_obu(&mut params, &seq);
                }
            }
        }
        params
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: plain COM calls. Shutting it down ends its event thread.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// A sample of `texture` as the encoder takes it.
fn surface(
    texture: &ID3D11Texture2D,
    time: Duration,
    length: Duration,
) -> windows::core::Result<IMFSample> {
    // SAFETY: plain COM calls; the buffer holds a reference of its own to
    // the texture.
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
        if let Ok(planes) = buffer.cast::<IMF2DBuffer>() {
            buffer.SetCurrentLength(planes.GetContiguousLength()?)?;
        }
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(hundreds(time))?;
        sample.SetSampleDuration(hundreds(length))?;
        Ok(sample)
    }
}

/// The bytes of an encoded sample.
fn read(sample: &IMFSample) -> windows::core::Result<Vec<u8>> {
    // SAFETY: while locked, the buffer holds `len` bytes at `at`.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut at = std::ptr::null_mut();
        let mut len = 0;
        buffer.Lock(&mut at, None, Some(&mut len))?;
        let bits = if at.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(at, len as usize).to_vec()
        };
        buffer.Unlock()?;
        Ok(bits)
    }
}

/// In Media Foundation's unit, 100 ns.
fn hundreds(d: Duration) -> i64 {
    i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX)
}

const START: [u8; 4] = [0, 0, 0, 1];

/// The NAL units of an Annex B stream, without their start codes.
fn units(data: &[u8]) -> Vec<&[u8]> {
    // Without the zeros a four-byte start code leaves before it.
    let trim = |nal: &'_ [u8]| -> usize { nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1) };
    let mut found = Vec::new();
    let mut begin: Option<usize> = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(b) = begin {
                let nal = &data[b..i];
                found.push(&nal[..trim(nal)]);
            }
            i += 3;
            begin = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(b) = begin {
        let nal = &data[b..];
        found.push(&nal[..trim(nal)]);
    }
    found.retain(|n| !n.is_empty());
    found
}

/// What the encoder made, as the call sends it: only the slices, and the
/// parameters (the newest that came) ahead of every IDR picture. Access
/// unit delimiters, SEI and filler go. Whether it is a full picture.
fn tidy(data: &[u8], params: &mut Vec<u8>) -> (Vec<u8>, bool) {
    let nals = units(data);
    let kind = |n: &[u8]| n[0] & 0x1f;
    let key = nals.iter().any(|n| kind(n) == 5);
    if nals.iter().any(|n| kind(n) == 7) {
        params.clear();
        for n in nals.iter().filter(|n| matches!(kind(n), 7 | 8)) {
            params.extend_from_slice(&START);
            params.extend_from_slice(n);
        }
    }
    let mut out = Vec::with_capacity(data.len() + params.len());
    if key {
        out.extend_from_slice(params);
    }
    for n in nals.iter().filter(|n| matches!(kind(n), 1..=5)) {
        out.extend_from_slice(&START);
        out.extend_from_slice(n);
    }
    (out, key)
}

/// The AV1 OBU types this side looks at.
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;
const OBU_PADDING: u8 = 15;

/// One AV1 OBU: its type, its header (one byte, two with the extension)
/// and its payload.
struct Obu<'a> {
    kind: u8,
    header: &'a [u8],
    payload: &'a [u8],
}

/// A leb128 number and how many bytes it took.
fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (i, byte) in data.iter().take(8).enumerate() {
        value |= usize::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

fn push_leb128(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// The OBUs of a temporal unit, with or without their size fields (one
/// without runs to the end). None if they do not add up.
fn obus(data: &[u8]) -> Option<Vec<Obu<'_>>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let first = data[at];
        let header_len = 1 + usize::from(first & 0x04 != 0);
        let header = data.get(at..at + header_len)?;
        let mut from = at + header_len;
        let len = if first & 0x02 != 0 {
            let (len, used) = leb128(data.get(from..)?)?;
            from += used;
            len
        } else {
            data.len() - from
        };
        let payload = data.get(from..from.checked_add(len)?)?;
        found.push(Obu {
            kind: (first >> 3) & 0x0f,
            header,
            payload,
        });
        at = from + len;
    }
    Some(found)
}

/// An OBU written with its size field.
fn push_obu(out: &mut Vec<u8>, obu: &Obu<'_>) {
    out.push(obu.header[0] | 0x02);
    out.extend_from_slice(&obu.header[1..]);
    push_leb128(out, obu.payload.len());
    out.extend_from_slice(obu.payload);
}

/// What the card's AV1 encoder made, as the call sends it: each OBU with
/// its size, without temporal delimiters or padding, and the sequence
/// header (the newest that came, kept in `seq`) at the head of every key
/// frame and nowhere else, since the receiving side takes a picture that
/// starts with one for a key frame (assemble.rs). Whether it is one: a
/// frame (or frame header) that shows no earlier frame and is a KEY_FRAME,
/// the first three bits of its header.
fn tidy_av1(data: &[u8], seq: &mut Vec<u8>) -> (Vec<u8>, bool) {
    let Some(all) = obus(data) else {
        return (Vec::new(), false);
    };
    let key = all.iter().any(|o| {
        matches!(o.kind, OBU_FRAME | OBU_FRAME_HEADER)
            && o.payload.first().is_some_and(|b| b & 0xe0 == 0)
    });
    if let Some(header) = all.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER) {
        seq.clear();
        push_obu(seq, header);
    }
    let mut out = Vec::with_capacity(data.len() + seq.len());
    if key {
        out.extend_from_slice(seq);
    }
    for o in all.iter().filter(|o| {
        !matches!(
            o.kind,
            OBU_SEQUENCE_HEADER | OBU_TEMPORAL_DELIMITER | OBU_PADDING
        )
    }) {
        push_obu(&mut out, o);
    }
    (out, key)
}

// ------------------------------------------------ sharing

/// One copy being made (as video.rs `Encoding` does, on the card).
struct Encoding {
    layer: Layer,
    spec: Spec,
    scaler: Option<Scaler>,
    encoder: Option<Encoder>,
    kbps: u32,
    /// It was being made at the last picture (a copy that starts again
    /// starts with a full picture).
    active: bool,
    idle_since: Instant,
    last_sent: Instant,
    last_key: Instant,
    /// When the next picture is due.
    due: Instant,
    /// The number of the screen's last picture this copy took (`run`
    /// counts them): a newer one goes out once it is due.
    took: u64,
    /// After a failure, nothing is tried before this.
    resting: Option<Instant>,
    /// The card encodes AV1 as well (checked when the share started).
    av1: bool,
}

impl Encoding {
    fn new(layer: Layer, spec: Spec, av1: bool) -> Encoding {
        let long_ago = Instant::now() - Duration::from_secs(60);
        Encoding {
            layer,
            spec,
            scaler: None,
            encoder: None,
            kbps: 0,
            active: false,
            idle_since: Instant::now(),
            last_sent: long_ago,
            last_key: long_ago,
            due: long_ago,
            took: 0,
            resting: None,
            av1,
        }
    }

    fn gap(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.spec.fps.max(1)))
    }

    /// When this copy takes a picture unless a new one comes first, if
    /// anyone wants it: the screen's picture `newest`, once it is due (a
    /// little early counts, see `step`) and has not gone out yet; a full
    /// picture someone asked for, as soon as KEY_GAP allows; or else a
    /// still screen's repeat. Never earlier than `step` would take one, or
    /// the share's thread would wake for nothing again and again.
    fn wants(&self, control: &LayerControl, newest: u64) -> Option<Instant> {
        let slot = self.layer.slot()?;
        if !control.is_on(self.layer) {
            return None;
        }
        let due = self.due.checked_sub(self.gap() / 4).unwrap_or(self.due);
        let mut at = if newest == self.took {
            due.max(self.last_sent + REPEAT_AFTER)
        } else {
            due
        };
        if control.key_asked(slot) {
            at = at.min(due.max(self.last_key + KEY_GAP));
        }
        Some(self.resting.map_or(at, |until| at.max(until)))
    }

    /// When the encoder of a copy nobody wants any more is let go (`step`
    /// does it, IDLE after it saw the copy stop).
    fn lets_go(&self, control: &LayerControl) -> Option<Instant> {
        if control.is_on(self.layer) || self.encoder.is_none() {
            return None;
        }
        Some(if self.active {
            // Not seen stopped yet: `step` notes when, now.
            Instant::now()
        } else {
            self.idle_since + IDLE
        })
    }

    /// Encodes the screen's picture for this copy if the copy is wanted
    /// and due. `newest` numbers the picture: one this copy has taken
    /// already only goes again as a still screen's repeat, every
    /// REPEAT_AFTER.
    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        gpu: &Gpu,
        picture: &Picture,
        newest: u64,
        captured: Instant,
        control: &LayerControl,
        out: &UnboundedSender<EncodedFrame>,
        started: Instant,
    ) -> Step {
        let Some(slot) = self.layer.slot() else {
            return Step::Ok;
        };
        if !control.is_on(self.layer) {
            if self.active {
                self.active = false;
                self.idle_since = Instant::now();
            }
            if self.encoder.is_some() && self.idle_since.elapsed() >= IDLE {
                self.encoder = None;
                self.scaler = None;
            }
            return Step::Ok;
        }
        let now = Instant::now();
        if self.resting.is_some_and(|until| now < until) {
            return Step::Ok;
        }
        let since = now.saturating_duration_since(self.last_sent);
        let gap = self.gap();
        let fresh = newest != self.took;
        // A full picture someone asked for goes out from a still screen too.
        let key = control.key_asked(slot) && self.last_key.elapsed() >= KEY_GAP;
        if now + gap / 4 < self.due || (!fresh && !key && since < REPEAT_AFTER) {
            return Step::Ok;
        }
        self.due = (self.due + gap).max(now + gap / 2);
        self.took = newest;
        let made = match self.encode(gpu, picture, captured, control, slot, started) {
            Ok(made) => made,
            Err(e) => {
                crate::logln!("vídeo: na placa: {e}");
                // Made again from scratch next time, starting with a full
                // picture.
                self.encoder = None;
                self.scaler = None;
                self.active = false;
                self.resting = Some(now + RETRY_AFTER);
                return Step::Failed;
            }
        };
        let duration = since.min(Duration::from_millis(500));
        for m in made {
            crate::probe::record(
                if m.key {
                    "video.chave_kb"
                } else {
                    "video.quadro_kb"
                },
                m.data.len() as f64 / 1024.0,
            );
            self.active = true;
            if m.key {
                self.last_key = Instant::now();
            }
            self.last_sent = Instant::now();
            let frame = EncodedFrame {
                layer: self.layer,
                codec: m.codec,
                key: m.key,
                data: m.data,
                duration,
                captured: m.captured,
            };
            if out.send(frame).is_err() {
                return Step::Dead;
            }
        }
        Step::Ok
    }

    fn encode(
        &mut self,
        gpu: &Gpu,
        picture: &Picture,
        captured: Instant,
        control: &LayerControl,
        slot: usize,
        started: Instant,
    ) -> Result<Vec<Made>, String> {
        let (w, h) = fit(
            picture.width as usize,
            picture.height as usize,
            self.spec.width as usize,
            self.spec.height as usize,
        );
        let size = (w as u32, h as u32);
        if self
            .scaler
            .as_ref()
            .is_none_or(|s| s.source != picture.id || s.size != size)
        {
            self.scaler =
                Some(Scaler::new(gpu, picture, size, DXGI_FORMAT_NV12, RING).map_err(text)?);
        }
        // The codec the call asks for, when this card makes it.
        let makes = match control.codec() {
            Codec::Av1 if self.av1 => Codec::Av1,
            _ => Codec::H264,
        };
        // The call allots bitrates as H.264 needs them; AV1 makes the same
        // picture with fewer bits, and the difference stays off the line.
        let kbps = control
            .kbps(self.layer)
            .clamp(self.spec.min_kbps, self.spec.max_kbps);
        let kbps = match makes {
            Codec::H264 => kbps,
            Codec::Av1 => kbps * AV1_SHARE / 100,
        };
        if self
            .encoder
            .as_ref()
            .is_none_or(|e| e.size != size || e.makes != makes)
        {
            if self.encoder.as_ref().is_some_and(|e| e.makes != makes) {
                // Another codec starts with a full picture.
                self.active = false;
            }
            self.encoder = None;
            self.encoder = Some(Encoder::new(gpu, size, self.spec.fps, kbps, makes)?);
            self.kbps = kbps;
        }
        let gap = self.gap();
        let (Some(scaler), Some(encoder)) = (self.scaler.as_mut(), self.encoder.as_mut()) else {
            return Ok(Vec::new());
        };
        if kbps != self.kbps {
            encoder.set_bitrate(kbps);
            self.kbps = kbps;
        }
        let converting = Instant::now();
        let nv12 = scaler.draw(gpu).map_err(text)?;
        crate::probe::since("video.conversao", converting);
        let asked = self.last_key.elapsed() >= KEY_GAP && control.key_due(slot);
        let key = !self.active || asked || self.last_key.elapsed() >= KEY_EVERY;
        encoder.encode(&nv12, started.elapsed(), gap, key, captured)
    }
}

/// The ring my tile is drawn into for egui's device to draw from
/// (interop.rs), and the fences between them.
struct SharedRing {
    ring: Arc<interop::Ring>,
    textures: Vec<ID3D11Texture2D>,
    ready: ID3D11Fence,
    done: ID3D11Fence,
    context: ID3D11DeviceContext4,
    /// The last `ready` value signalled, and the texture written then.
    value: u64,
    last: usize,
}

impl SharedRing {
    fn new(gpu: &Gpu, size: (u32, u32)) -> windows::core::Result<SharedRing> {
        static RINGS: AtomicU64 = AtomicU64::new(0);
        let context: ID3D11DeviceContext4 = gpu.context.cast()?;
        let mut textures = Vec::with_capacity(interop::RING);
        let mut handles = Vec::with_capacity(interop::RING);
        for _ in 0..interop::RING {
            let (texture, handle) = gpu.shared_texture(size.0, size.1)?;
            textures.push(texture);
            handles.push(handle);
        }
        let (ready, ready_handle) = gpu.shared_fence()?;
        let (done, done_handle) = gpu.shared_fence()?;
        let ring = interop::Ring {
            id: RINGS.fetch_add(1, Ordering::Relaxed) + 1,
            size: [size.0 as usize, size.1 as usize],
            textures: handles.try_into().map_err(|_| failed())?,
            ready: ready_handle,
            done: done_handle,
            shown_in: Default::default(),
            refused: AtomicBool::new(false),
        };
        Ok(SharedRing {
            ring: Arc::new(ring),
            textures,
            ready,
            done,
            context,
            value: 0,
            last: interop::RING - 1,
        })
    }

    /// Copies `small` into a texture no frame of egui's is drawing any
    /// more, and says when it is written. None when every texture is still
    /// in use: this picture is skipped.
    fn publish(
        &mut self,
        gpu: &Gpu,
        small: &ID3D11Texture2D,
    ) -> windows::core::Result<Option<interop::Frame>> {
        // SAFETY: a plain query.
        let done = unsafe { self.done.GetCompletedValue() };
        let free = (1..=interop::RING)
            .map(|i| (self.last + i) % interop::RING)
            .find(|&slot| self.ring.shown_in[slot].load(Ordering::Relaxed) <= done);
        let Some(slot) = free else {
            return Ok(None);
        };
        self.value += 1;
        // SAFETY: both textures are this device's, of the same size and
        // format. The signal follows the copy on the same context, and the
        // flush sends both to the card before egui hears of the picture.
        unsafe {
            gpu.context.CopyResource(&self.textures[slot], small);
            self.context.Signal(&self.ready, self.value)?;
            gpu.context.Flush();
        }
        self.last = slot;
        Ok(Some(interop::Frame {
            ring: self.ring.clone(),
            slot,
            ready: self.value,
        }))
    }
}

/// My own tile: the screen small, drawn on the card. egui draws it from
/// there when it draws on the same card (interop.rs); otherwise it is read
/// back into a ColorImage.
struct Own {
    scaler: Option<Scaler>,
    staging: Option<ID3D11Texture2D>,
    pictures: Pictures,
    last: Instant,
    /// The number of the screen's last picture shown (`run` counts them).
    took: u64,
    shared: Option<SharedRing>,
    /// egui draws elsewhere, or could not open the ring: read back.
    read_back: bool,
}

impl Own {
    fn new(gpu: &Gpu) -> Own {
        let same_card = interop::card() == Some(gpu.luid);
        Own {
            scaler: None,
            staging: None,
            // The one in the tile, and the one being written.
            pictures: Pictures::new(3),
            last: Instant::now() - Duration::from_secs(1),
            took: 0,
            shared: None,
            read_back: !same_card,
        }
    }

    /// Puts the screen's picture, small, in my tile.
    fn show(
        &mut self,
        gpu: &Gpu,
        picture: &Picture,
        preview: &VideoShared,
        me: i64,
    ) -> windows::core::Result<()> {
        let (w, h) = fit(picture.width as usize, picture.height as usize, 640, 360);
        let size = (w as u32, h as u32);
        if self
            .scaler
            .as_ref()
            .is_none_or(|s| s.source != picture.id || s.size != size)
        {
            self.scaler = Some(Scaler::new(
                gpu,
                picture,
                size,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                1,
            )?);
            self.staging = None;
        }
        if self
            .shared
            .as_ref()
            .is_some_and(|s| s.ring.refused.load(Ordering::Relaxed))
        {
            self.shared = None;
            self.read_back = true;
        }
        if !self.read_back && self.shared.as_ref().is_none_or(|s| s.ring.size != [w, h]) {
            match SharedRing::new(gpu, size) {
                Ok(ring) => self.shared = Some(ring),
                Err(e) => {
                    crate::logln!("vídeo: a prévia na placa não deu ({e}), vai pela memória");
                    self.shared = None;
                    self.read_back = true;
                }
            }
        }
        let Some(scaler) = self.scaler.as_mut() else {
            return Err(failed());
        };
        let small = scaler.draw(gpu)?;
        if let Some(shared) = self.shared.as_mut() {
            if let Some(frame) = shared.publish(gpu, &small)? {
                preview.put_gpu(me, frame);
            }
            return Ok(());
        }
        preview.put(me, self.read(gpu, &small, w, h)?, None);
        Ok(())
    }

    /// `small` (`w` x `h`) read back into a ColorImage.
    fn read(
        &mut self,
        gpu: &Gpu,
        small: &ID3D11Texture2D,
        w: usize,
        h: usize,
    ) -> windows::core::Result<Arc<ColorImage>> {
        if self.staging.is_none() {
            self.staging = Some(gpu.texture(
                w as u32,
                h as u32,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                0,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ.0 as u32,
            )?);
        }
        let Some(staging) = self.staging.as_ref() else {
            return Err(failed());
        };
        let mut img = self.pictures.take([w, h]);
        // SAFETY: the staging texture has the small picture's size and
        // format, and belongs to this device. While mapped, Windows
        // guarantees `h` rows of `RowPitch` bytes at pData, each holding at
        // least `w` pixels.
        unsafe {
            gpu.context.CopyResource(staging, small);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            if let Some(dst) = Arc::get_mut(&mut img) {
                let base = mapped.pData.cast::<u8>().cast_const();
                for (y, line) in dst.pixels.chunks_exact_mut(w).enumerate() {
                    let row =
                        std::slice::from_raw_parts(base.add(y * mapped.RowPitch as usize), w * 4);
                    super::video::bgra_to_color32(row, line);
                }
            }
            gpu.context.Unmap(staging, 0);
        }
        self.pictures.keep(&img);
        Ok(img)
    }
}

/// The card's encoder's recent refusals, to tell a hiccup (a picture the
/// driver turned down once) from a card that stopped encoding.
#[derive(Default)]
struct Failures(VecDeque<Instant>);

impl Failures {
    /// One refusal at `now`. Whether the card is done for: `FAIL_LIMIT`
    /// of them within `FAIL_WINDOW`.
    fn record(&mut self, now: Instant) -> bool {
        self.0.push_back(now);
        while self
            .0
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > FAIL_WINDOW)
        {
            self.0.pop_front();
        }
        self.0.len() >= FAIL_LIMIT
    }
}

/// Takes the screen whenever a copy wants a picture, until stopped. The
/// capture callback (`frame_arrived`) replaces the slot with the newest
/// picture as it comes; this loop reads from there. When the card stops
/// encoding, it raises `flags.failed` and ends: the owner carries the
/// share on through the processor.
#[allow(clippy::too_many_arguments)]
fn run(
    gpu: &Gpu,
    screen: &Screen,
    copies: Copies,
    out: &UnboundedSender<EncodedFrame>,
    control: &LayerControl,
    preview: &VideoShared,
    me: i64,
    flags: &Flags,
    av1: bool,
) {
    let started = Instant::now();
    let mut made = [
        Encoding::new(Layer::Low, copies.low, av1),
        Encoding::new(Layer::High, copies.high, av1),
    ];
    let mut own = Own::new(gpu);
    let mut last_picture: Option<Picture> = None;
    // The screen's pictures, counted: each copy, and my tile, know the
    // last one they took.
    let mut newest = 0u64;
    let mut captured = Instant::now();
    let mut complained = false;
    let mut failures = Failures::default();
    let mut gave_up = false;
    // The thread sleeps until something it acts on happens (a new picture,
    // a copy turned on or off, a full picture asked for, my tile drawn
    // again, the end of the share: `LayerControl::ring`) or comes due (a
    // copy's next picture, a still screen's repeat, an idle encoder let
    // go, my tile's next picture). It never wakes for what nobody wants:
    // a wait that ended before anything could be done spun a whole core.
    let mut heard = control.rings();
    'screen: while !flags.stop.load(Ordering::Relaxed) {
        if screen.ended() {
            // The window being shared was closed; the share stops.
            flags.ended.store(true, Ordering::Relaxed);
            preview.wake();
            break;
        }
        if let Some(p) = screen.take() {
            captured = Instant::now();
            newest += 1;
            last_picture = Some(p);
        }
        let Some(picture) = last_picture.as_ref() else {
            // Nothing to do before the first picture (it rings).
            heard = control.wait(heard, None);
            continue;
        };
        for m in &mut made {
            match m.step(gpu, picture, newest, captured, control, out, started) {
                Step::Ok => {}
                Step::Dead => break 'screen,
                // A lost device never comes back; otherwise the encoder
                // gets a few more tries.
                Step::Failed if gpu.lost() || failures.record(Instant::now()) => {
                    gave_up = true;
                    break 'screen;
                }
                Step::Failed => {}
            }
        }
        // My own tile shows what I share even while nobody receives it,
        // while the call view draws it (as video.rs does).
        let tile_wanted = own.took != newest && preview.wanted_recently(me, TILE_GRACE);
        if tile_wanted && own.last.elapsed() >= PREVIEW_EVERY {
            own.last = Instant::now();
            own.took = newest;
            match own.show(gpu, picture, preview, me) {
                Ok(()) => {}
                Err(e) if !complained => {
                    complained = true;
                    crate::logln!("vídeo: na placa, a minha imagem: {e}");
                }
                Err(_) => {}
            }
        }
        let tile = (own.took != newest && tile_wanted).then_some(own.last + PREVIEW_EVERY);
        let until = made
            .iter()
            .filter_map(|m| m.wants(control, newest).or_else(|| m.lets_go(control)))
            .chain(tile)
            .min();
        heard = control.wait(heard, until);
    }
    screen.stop();
    preview.forget(me);
    if gave_up {
        // Said only now, with my tile cleared, so the processor path that
        // takes over is the only one putting pictures there.
        crate::logln!("vídeo: a placa parou de codificar");
        flags.failed.store(true, Ordering::Relaxed);
        preview.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_encoder_makes_goes_as_the_call_sends_it() {
        // An access unit delimiter, the parameters, SEI and an IDR slice,
        // with both kinds of start code.
        let first = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x06, 5,
            0x80, 0, 0, 0, 1, 0x65, 7, 8,
        ];
        let mut params = Vec::new();
        let (data, key) = tidy(&first, &mut params);
        assert!(key);
        let sps_pps = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3];
        assert_eq!(data, [&sps_pps[..], &[0, 0, 0, 1, 0x65, 7, 8]].concat());
        // A later IDR picture without them gets the ones kept.
        let (data, key) = tidy(&[0, 0, 1, 0x65, 9], &mut params);
        assert!(key);
        assert_eq!(data, [&sps_pps[..], &[0, 0, 0, 1, 0x65, 9]].concat());
        // Other pictures go as they are.
        let (data, key) = tidy(&[0, 0, 0, 1, 0x41, 9, 9], &mut params);
        assert!(!key);
        assert_eq!(data, [0, 0, 0, 1, 0x41, 9, 9]);
    }

    /// Measuring what zero copy could save elsewhere: plain memcpy at the
    /// sizes the app moves, and what a received picture's trip to the card
    /// costs the processor. egui-wgpu hands `Queue::write_texture` the whole
    /// RGBA picture, which wgpu copies into a staging buffer; the I420
    /// planes a colour conversion on the card would upload instead are 1.5
    /// bytes a pixel, not 4.
    /// `cargo test -p babutela --profile medir hw::tests::copies -- --ignored --nocapture`
    #[test]
    #[ignore = "medição"]
    fn copies() {
        use eframe::wgpu;
        use futures_util::FutureExt;
        // Seconds a call: the median of 15 runs of about 40 MB each.
        fn per_call(bytes: usize, mut f: impl FnMut()) -> f64 {
            for _ in 0..3 {
                f();
            }
            let reps = (40_000_000 / bytes.max(1)).clamp(3, 100_000);
            let mut runs: Vec<f64> = (0..15)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..reps {
                        f();
                    }
                    t.elapsed().as_secs_f64() / reps as f64
                })
                .collect();
            runs.sort_by(f64::total_cmp);
            runs[runs.len() / 2]
        }
        let sizes = [
            ("um pacote RTP", 1_200),
            ("10 ms de voz", 1_920),
            ("um pedaço de arquivo", 256 * 1024),
            ("720p em NV12", 1280 * 720 * 3 / 2),
            ("1080p em I420", 1920 * 1080 * 3 / 2),
            ("720p em RGBA", 1280 * 720 * 4),
            ("1080p em RGBA", 1920 * 1080 * 4),
        ];
        for (label, n) in sizes {
            let src: Vec<u8> = (0..n).map(|i| i as u8).collect();
            // 64 bytes in: two buffers of the same size can sit a multiple
            // of 4 KB apart, which slows copying between them down a lot
            // ("4K aliasing"), and is not what the app's buffers do.
            let mut dst = vec![0u8; n + 64];
            let reused = per_call(n, || {
                dst[64..].copy_from_slice(std::hint::black_box(&src));
                std::hint::black_box(&mut dst);
            });
            let fresh = per_call(n, || {
                std::hint::black_box(std::hint::black_box(&src).to_vec());
            });
            println!(
                "memcpy de {label} ({n} B): {:.2} µs ({:.1} GB/s); para um buffer novo, {:.2} µs",
                reused * 1e6,
                n as f64 / reused / 1e9,
                fresh * 1e6
            );
        }

        // As the app runs: Direct3D 12 with its memory hints, egui writing
        // its vertices every frame (a small staging buffer of its own), and
        // a picture going up every 30 ms, each plane in one write_texture.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .now_or_never()
            .unwrap()
            .unwrap();
        let upload = |planes: &[(u32, u32, wgpu::TextureFormat, u32)]| -> f64 {
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                    ..Default::default()
                })
                .now_or_never()
                .unwrap()
                .unwrap();
            let vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 256 * 1024,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let small = vec![0u8; 200 * 1024];
            let made: Vec<_> = planes
                .iter()
                .map(|&(w, h, format, bpp)| {
                    let size = wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    };
                    let texture = device.create_texture(&wgpu::TextureDescriptor {
                        label: None,
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    (texture, size, bpp * w, vec![0x80u8; (w * h * bpp) as usize])
                })
                .collect();
            let mut times = Vec::new();
            for i in 0..130 {
                queue.write_buffer(&vertices, 0, &small);
                let t = Instant::now();
                for (texture, size, row, data) in &made {
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        data,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(*row),
                            rows_per_image: Some(size.height),
                        },
                        *size,
                    );
                }
                if i >= 30 {
                    times.push(t.elapsed().as_secs_f64());
                }
                queue.submit(None);
                let _ = device.poll(wgpu::PollType::Poll);
                std::thread::sleep(Duration::from_millis(30));
            }
            times.sort_by(f64::total_cmp);
            times[times.len() / 2]
        };
        let rgba = wgpu::TextureFormat::Rgba8UnormSrgb;
        let r8 = wgpu::TextureFormat::R8Unorm;
        let full = upload(&[(1920, 1080, rgba, 4)]);
        let i420 = upload(&[(1920, 1080, r8, 1), (960, 540, r8, 1), (960, 540, r8, 1)]);
        let hd = upload(&[(1280, 720, rgba, 4)]);
        println!(
            "write_texture: 1080p em RGBA {:.3} ms; em I420 (Y, U e V) {:.3} ms; 720p em RGBA {:.3} ms",
            full * 1e3,
            i420 * 1e3,
            hd * 1e3
        );
    }

    /// The card's encoder makes what openh264 decodes, in the colours
    /// openh264 expects, one picture out for each in, and full ones when
    /// asked. Needs a graphics card with an H.264 encoder:
    /// `cargo test -p babutela hw -- --ignored --nocapture`.
    #[test]
    #[ignore = "precisa de uma placa de vídeo com codificador H.264"]
    fn the_cards_pictures_decode() {
        use openh264::formats::YUVSource;
        let _media = Media::start().unwrap();
        // SAFETY: plain COM calls.
        let adapter = unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
            factory.EnumAdapters1(0).unwrap()
        };
        let gpu = Gpu::new(&adapter).unwrap();
        let (_, name) = find_encoder(gpu.vendor, Codec::H264).unwrap();
        println!("codificador: {name}");
        let (w, h) = (1920u32, 1080u32);
        let texture = gpu
            .texture(
                w,
                h,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                D3D11_USAGE_DEFAULT,
                0,
            )
            .unwrap();
        let picture = Picture {
            texture,
            width: w,
            height: h,
            id: 1,
        };
        let mut scaler = Scaler::new(&gpu, &picture, (w, h), DXGI_FORMAT_NV12, RING).unwrap();
        let mut encoder = Encoder::new(&gpu, (w, h), 30, 4000, Codec::H264).unwrap();
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        // The small copy, scaled on the card.
        let mut small = Scaler::new(&gpu, &picture, (640, 360), DXGI_FORMAT_NV12, RING).unwrap();
        let mut small_encoder = Encoder::new(&gpu, (640, 360), 8, 300, Codec::H264).unwrap();
        let mut small_decoder = openh264::decoder::Decoder::new().unwrap();
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let (mut times, mut keys, mut sizes) = (Vec::new(), Vec::new(), Vec::new());
        let mut decoded = 0;
        for i in 0..60u32 {
            // White on the left, black on the right; moving across them, a
            // grey square, a one-pixel checkerboard and a block flipping
            // between pure red and pure blue: the hardest residuals there
            // are, which once made the encoder use codes openh264 rejects.
            let (cx, bx) = ((i * 40) % (w - 256), (i * 56) % (w - 128));
            for (n, px) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (n as u32 % w, n as u32 / w);
                let grey = u8::from(x < w / 2) * 255;
                let bgra = if x / 64 == i % 30 && y / 64 == 8 {
                    [128, 128, 128, 255]
                } else if (cx..cx + 256).contains(&x) && (256..512).contains(&y) {
                    let v = u8::from((x + y + i) % 2 == 0) * 255;
                    [v, v, v, 255]
                } else if (bx..bx + 128).contains(&x) && (700..828).contains(&y) {
                    if i % 2 == 0 {
                        [0, 0, 255, 255]
                    } else {
                        [255, 0, 0, 255]
                    }
                } else {
                    [grey, grey, grey, 255]
                };
                *px = bgra;
            }
            // SAFETY: the texture is `w` x `h` BGRA, as `pixels` is.
            unsafe {
                gpu.context.UpdateSubresource(
                    &picture.texture,
                    0,
                    None,
                    pixels.as_ptr().cast(),
                    w * 4,
                    0,
                );
            }
            let nv12 = scaler.draw(&gpu).unwrap();
            let began = Instant::now();
            let made = encoder
                .encode(
                    &nv12,
                    Duration::from_millis(33 * u64::from(i)),
                    Duration::from_millis(33),
                    i == 30,
                    Instant::now(),
                )
                .unwrap();
            times.push(began.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(made.len(), 1, "uma imagem por imagem, na hora ({i})");
            let nv12_small = small.draw(&gpu).unwrap();
            let length = Duration::from_millis(125);
            let at = length * i;
            for m in small_encoder
                .encode(&nv12_small, at, length, false, Instant::now())
                .unwrap()
            {
                let img = small_decoder.decode(&m.data).unwrap().unwrap();
                assert_eq!(img.dimensions(), (640, 360));
                let (stride, _, _) = img.strides();
                let row = 30 * stride;
                let (white, black) = (img.y()[row + 30], img.y()[row + 640 - 30]);
                assert!(white.abs_diff(235) <= 6, "branco pequeno {white}");
                assert!(black.abs_diff(16) <= 6, "preto pequeno {black}");
            }
            for m in made {
                keys.push(m.key);
                sizes.push(m.data.len());
                if m.key {
                    assert_eq!(m.data[4] & 0x1f, 7, "os parâmetros primeiro");
                }
                let picture = decoder.decode(&m.data);
                let picture = picture.unwrap_or_else(|e| panic!("imagem {i}: {e:?}"));
                if let Some(img) = picture {
                    assert_eq!(img.dimensions(), (w as usize, h as usize));
                    let (stride, _, _) = img.strides();
                    let y = img.y();
                    let row = 100 * stride;
                    let (white, black) = (y[row + 100], y[row + w as usize - 100]);
                    assert!(white.abs_diff(235) <= 4, "branco {white}");
                    assert!(black.abs_diff(16) <= 4, "preto {black}");
                    decoded += 1;
                }
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "codificação: mediana {:.2} ms, pior {:.2} ms; chave {} B, quadro {} B",
            times[times.len() / 2],
            times[times.len() - 1],
            sizes[30],
            sizes[31]
        );
        assert!(keys[0] && keys[30], "chaves: {keys:?}");
        assert!(keys.iter().filter(|k| **k).count() <= 3, "chaves: {keys:?}");
        assert_eq!(decoded, 60);
    }
}

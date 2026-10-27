//! A whole screen through DXGI Desktop Duplication, the API remote-desktop
//! tools use, next to Windows Graphics Capture (video.rs `start_screen`),
//! so `cargo xtask medir captura` can compare the two on the same screen.
//!
//! Desktop Duplication hands over the desktop image as a Direct3D 11
//! texture each time it changes. The picture is copied to a texture the
//! processor can read (once, reused), into a buffer of the slot's pool,
//! exactly as the other capture does, so only the API differs.
//!
//! Not for everyday use as it stands: the mouse pointer is not in the
//! picture (Desktop Duplication hands it over apart), and rotated and HDR
//! screens would need more work.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::core::Interface;

use super::video::{Layout, Raw, Slot};

/// Measuring: screens are captured this way instead (set by the smoke
/// test's `share:screen-dxgi`).
pub static WANTED: AtomicBool = AtomicBool::new(false);

/// The capture; dropping it stops it.
pub struct Duplication {
    stop: Arc<AtomicBool>,
}

impl Drop for Duplication {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts capturing the screen `monitor` (an HMONITOR) at `fps` pictures a
/// second at most, into `slot`.
pub(super) fn start(monitor: isize, slot: Arc<Slot>, fps: u32) -> Result<Duplication, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (opened, ready) = std::sync::mpsc::channel();
    let running = stop.clone();
    // As with the other capture, a little under the frame interval.
    let interval = Duration::from_millis((750 / u64::from(fps.max(1))).max(1));
    std::thread::Builder::new()
        .name("video-dxgi".into())
        .spawn(move || match Screen::open(monitor) {
            Ok(mut screen) => {
                let _ = opened.send(Ok(()));
                screen.run(&slot, &running, interval);
            }
            Err(e) => {
                let _ = opened.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    ready.recv().map_err(|e| e.to_string())??;
    Ok(Duplication { stop })
}

struct Screen {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    /// The texture the processor reads, and its size.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

impl Screen {
    /// The device of the graphics card the screen is on, and the screen's
    /// duplication.
    fn open(monitor: isize) -> Result<Screen, String> {
        // SAFETY: plain COM calls; every out pointer is a local.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(text)?;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    o += 1;
                    let desc = output.GetDesc().map_err(text)?;
                    if desc.Monitor.0 as isize != monitor {
                        continue;
                    }
                    let (mut device, mut context) = (None, None);
                    D3D11CreateDevice(
                        &adapter,
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
                    let device = device.ok_or("sem dispositivo Direct3D 11")?;
                    let context = context.ok_or("sem contexto Direct3D 11")?;
                    let output: IDXGIOutput1 = output.cast().map_err(text)?;
                    let duplication = output.DuplicateOutput(&device).map_err(text)?;
                    return Ok(Screen {
                        device,
                        context,
                        output,
                        duplication,
                        staging: None,
                    });
                }
            }
        }
        Err("o DXGI não achou essa tela".into())
    }

    fn run(&mut self, slot: &Slot, stop: &AtomicBool, interval: Duration) {
        let mut last: Option<Instant> = None;
        while !stop.load(Ordering::Relaxed) && !slot.stopped() {
            // Nothing is read before the interval is up: the next picture
            // asked for then is the screen as it is at that moment.
            if let Some(at) = last {
                let wait = interval.saturating_sub(at.elapsed());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            // SAFETY: both out pointers are locals.
            let acquired = unsafe {
                self.duplication
                    .AcquireNextFrame(100, &mut info, &mut resource)
            };
            match acquired {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // A mode change, the secure desktop: duplicate again.
                    // SAFETY: as in `open`.
                    match unsafe { self.output.DuplicateOutput(&self.device) } {
                        Ok(d) => self.duplication = d,
                        Err(_) => std::thread::sleep(Duration::from_millis(200)),
                    }
                    continue;
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
            // Only the pointer moved: there is no new picture.
            if info.LastPresentTime == 0 {
                // SAFETY: a frame was acquired above.
                let _ = unsafe { self.duplication.ReleaseFrame() };
                continue;
            }
            let reading = Instant::now();
            last = Some(reading);
            let mut data = slot.buffer();
            let read = resource
                .and_then(|r| r.cast::<ID3D11Texture2D>().ok())
                .map(|t| self.read(&t, &mut data));
            // Released once the picture is in `data` (the copy has to have
            // happened before the desktop moves on).
            // SAFETY: a frame was acquired above.
            let _ = unsafe { self.duplication.ReleaseFrame() };
            if let Some(Ok((width, height))) = read {
                crate::probe::since("video.captura", reading);
                slot.put(Raw {
                    width,
                    height,
                    data,
                    layout: Layout::Bgra,
                    at: Instant::now(),
                });
            }
        }
    }

    /// Copies `texture` (BGRA) into `data`, rows without their padding.
    fn read(
        &mut self,
        texture: &ID3D11Texture2D,
        data: &mut Vec<u8>,
    ) -> windows::core::Result<(usize, usize)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: an out pointer to a local.
        unsafe { texture.GetDesc(&mut desc) };
        let (width, height) = (desc.Width, desc.Height);
        if !self
            .staging
            .as_ref()
            .is_some_and(|(_, w, h)| (*w, *h) == (width, height))
        {
            let staging = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut made = None;
            // SAFETY: a plain description, and an out pointer to a local.
            unsafe {
                self.device
                    .CreateTexture2D(&staging, None, Some(&mut made))?
            };
            self.staging = made.map(|t| (t, width, height));
        }
        let Some((staging, _, _)) = self.staging.as_ref() else {
            return Err(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            ));
        };
        let row = width as usize * 4;
        // SAFETY: the staging texture has the picture's size and format and
        // belongs to this thread's device. While mapped, Windows guarantees
        // `height` rows of `RowPitch` bytes at pData, each at least `row`.
        unsafe {
            self.context.CopyResource(staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            data.clear();
            data.reserve(row * height as usize);
            let base = mapped.pData.cast::<u8>().cast_const();
            for y in 0..height as usize {
                let line = std::slice::from_raw_parts(base.add(y * mapped.RowPitch as usize), row);
                data.extend_from_slice(line);
            }
            self.context.Unmap(staging, 0);
        }
        Ok((width as usize, height as usize))
    }
}

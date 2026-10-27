//! What a call can show: the camera, a whole screen or one window. For the
//! picker, the screens and windows come with a small picture of each, taken
//! once when it opens.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, HDC, MONITORINFO, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, IsIconic, IsWindow};

use crate::model::Video;

/// PrintWindow's "everything, including what DirectX draws" (Windows 8.1+).
const PW_RENDERFULLCONTENT: u32 = 2;

/// See `list`: tests only offer windows with this in the title.
pub const TEST_FILTER_ENV: &str = "BABUTELA_TESTE_COMPARTILHAR";

/// The largest the pictures in the picker get.
pub const THUMB_W: u32 = 320;
pub const THUMB_H: u32 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Camera,
    /// A whole screen (its HMONITOR).
    Monitor(isize),
    /// One window (its HWND).
    Window(isize),
}

impl Source {
    /// What the others are told: a camera or a screen (a window is a
    /// screen to them).
    pub fn kind(self) -> Video {
        match self {
            Source::Camera => Video::Camera,
            Source::Monitor(_) | Source::Window(_) => Video::Screen,
        }
    }

    /// The main screen, what sharing meant before there was a choice.
    pub fn main_screen() -> Option<Source> {
        windows_capture::monitor::Monitor::primary()
            .ok()
            .map(|m| Source::Monitor(m.as_raw_hmonitor() as isize))
    }

    /// Whether it can still be shared: a window may have closed, a screen
    /// been unplugged.
    pub fn exists(self) -> bool {
        match self {
            Source::Camera => true,
            // SAFETY: IsWindow only reads; a stale handle just answers no.
            Source::Window(h) => unsafe { IsWindow(h as HWND) != 0 },
            Source::Monitor(m) => windows_capture::monitor::Monitor::enumerate()
                .is_ok_and(|all| all.iter().any(|x| x.as_raw_hmonitor() as isize == m)),
        }
    }
}

/// One thing the picker offers.
pub struct Choice {
    pub source: Source,
    pub title: String,
    /// The program, or the screen's size.
    pub detail: String,
    pub thumb: Option<egui::ColorImage>,
}

/// The screens, then the windows, calling `found` with each as soon as its
/// picture is ready. Takes a moment (a picture of every window), so it runs
/// away from the interface. `own` is the app's window, left out.
pub fn list(own: Option<isize>, mut found: impl FnMut(Choice)) {
    // For automated tests on someone's own PC: only windows with this in
    // the title, and no pictures of the screens, so a test never captures
    // what that person has open.
    let only = std::env::var(TEST_FILTER_ENV)
        .ok()
        .filter(|f| !f.is_empty());
    if let Ok(monitors) = windows_capture::monitor::Monitor::enumerate() {
        let count = monitors.len();
        for (i, m) in monitors.iter().enumerate() {
            let handle = m.as_raw_hmonitor() as isize;
            let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
            let name = m.name().ok().filter(|n| !n.trim().is_empty());
            let title = match (count, name) {
                (1, _) => "Tela inteira".to_owned(),
                (_, Some(n)) => format!("Tela {}: {n}", i + 1),
                (_, None) => format!("Tela {}", i + 1),
            };
            found(Choice {
                source: Source::Monitor(handle),
                title,
                detail: format!("{w} × {h}"),
                thumb: if only.is_some() {
                    None
                } else {
                    monitor_picture(handle)
                },
            });
        }
    }
    let Ok(windows) = windows_capture::window::Window::enumerate() else {
        return;
    };
    for w in windows {
        let hwnd = w.as_raw_hwnd() as isize;
        if Some(hwnd) == own || !worth_offering(hwnd) {
            continue;
        }
        let Some(title) = w.title().ok().filter(|t| !t.trim().is_empty()) else {
            continue;
        };
        if only.as_deref().is_some_and(|f| !title.contains(f)) {
            continue;
        }
        let program = w
            .process_name()
            .ok()
            .map(|p| p.trim_end_matches(".exe").to_owned())
            .unwrap_or_default();
        // SAFETY: a read-only question about a window handle.
        let minimized = unsafe { IsIconic(hwnd as HWND) != 0 };
        let detail = if minimized {
            format!("{program} (minimizada)")
        } else {
            program
        };
        found(Choice {
            source: Source::Window(hwnd),
            title,
            detail,
            thumb: if minimized {
                None
            } else {
                window_picture(hwnd)
            },
        });
    }
}

/// Leaves out what is not a window anyone would share: the desktop itself,
/// and windows Windows keeps hidden ("cloaked": on another desktop, or
/// suspended Store apps).
fn worth_offering(hwnd: isize) -> bool {
    let hwnd = hwnd as HWND;
    let mut class = [0u16; 64];
    // SAFETY: the buffer is as long as said; the handle is only read.
    let n = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
        return false;
    }
    let mut cloaked = 0u32;
    // SAFETY: the attribute is a u32 and the size says so.
    let asked = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    asked != 0 || cloaked == 0
}

fn monitor_picture(hmonitor: isize) -> Option<egui::ColorImage> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is sized as the call expects.
    if unsafe { GetMonitorInfoW(hmonitor as _, &mut info) } == 0 {
        return None;
    }
    let r = info.rcMonitor;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    // SAFETY: copies from the whole-screen device context into our bitmap.
    grab(w, h, |dc| unsafe {
        let screen = GetDC(null_mut());
        let ok = BitBlt(dc, 0, 0, w, h, screen, r.left, r.top, SRCCOPY | CAPTUREBLT) != 0;
        ReleaseDC(null_mut(), screen);
        ok
    })
    .map(|rgba| thumbnail(rgba, w as u32, h as u32))
}

fn window_picture(hwnd: isize) -> Option<egui::ColorImage> {
    let hwnd = hwnd as HWND;
    let mut r = RECT::default();
    // The frame as drawn, without the invisible resize borders.
    // SAFETY: the attribute is a RECT and the size says so.
    let asked = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut r as *mut RECT).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
    };
    if asked != 0 {
        return None;
    }
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    // SAFETY: the window paints itself into our bitmap.
    grab(w, h, |dc| unsafe {
        PrintWindow(hwnd, dc, PW_RENDERFULLCONTENT) != 0
    })
    .map(|rgba| thumbnail(rgba, w as u32, h as u32))
}

/// A `w` x `h` bitmap that `draw` paints into, as RGBA.
fn grab(w: i32, h: i32, draw: impl FnOnce(HDC) -> bool) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 || w > 16384 || h > 16384 {
        return None;
    }
    // SAFETY: every handle made here is released here; `bits` points into
    // the DIB section, which lives until DeleteObject, after the copy.
    unsafe {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        if dc.is_null() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: rows top to bottom.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let mut out = None;
        if !dib.is_null() && !bits.is_null() {
            let old = SelectObject(dc, dib);
            if draw(dc) {
                let len = w as usize * h as usize * 4;
                let bgra = std::slice::from_raw_parts(bits as *const u8, len);
                let mut rgba = Vec::with_capacity(len);
                for [b, g, r, _] in bgra.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[*r, *g, *b, 255]);
                }
                out = Some(rgba);
            }
            SelectObject(dc, old);
            DeleteObject(dib);
        }
        DeleteDC(dc);
        out
    }
}

/// Fits the picture inside THUMB_W x THUMB_H, keeping its shape.
fn thumbnail(rgba: Vec<u8>, w: u32, h: u32) -> egui::ColorImage {
    let (tw, th) = thumb_size(w, h);
    let image = image::RgbaImage::from_raw(w, h, rgba)
        .map(|img| image::imageops::thumbnail(&img, tw, th))
        .unwrap_or_else(|| image::RgbaImage::new(tw, th));
    egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], image.as_raw())
}

fn thumb_size(w: u32, h: u32) -> (u32, u32) {
    let scale = (THUMB_W as f32 / w.max(1) as f32).min(THUMB_H as f32 / h.max(1) as f32);
    let scale = scale.min(1.0);
    (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_keep_the_shape() {
        assert_eq!(thumb_size(1920, 1080), (320, 180));
        assert_eq!(thumb_size(1080, 1920), (101, 180));
        assert_eq!(thumb_size(100, 50), (100, 50));
        assert_eq!(thumb_size(0, 0), (1, 1));
    }

    /// On this machine for real: at least one screen, with a picture, and
    /// the main screen among them.
    #[test]
    fn lists_the_screens_with_pictures() {
        let mut found = Vec::new();
        list(None, |c| found.push(c));
        let screens: Vec<&Choice> = found
            .iter()
            .filter(|c| matches!(c.source, Source::Monitor(_)))
            .collect();
        assert!(!screens.is_empty());
        assert!(
            screens.iter().all(|c| c.thumb.is_some()),
            "telas sem imagem"
        );
        let main = Source::main_screen().expect("tela principal");
        assert!(screens.iter().any(|c| c.source == main));
        assert!(main.exists());
        assert!(!Source::Window(0x7fff_fff0).exists());
        assert_eq!(main.kind(), Video::Screen);
        assert_eq!(Source::Camera.kind(), Video::Camera);
    }
}

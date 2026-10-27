//! The annotations over my own screen while I share it, the way Zoom and
//! Teams show the presenter what everyone draws, and let them draw right on
//! the screen:
//!
//! - A window over the shared screen (or window) shows everyone's strokes:
//!   see-through, letting every click through, and left out of every
//!   capture by Windows (display affinity WDA_EXCLUDEFROMCAPTURE, Windows
//!   10 2004 on, which the PC's sound of a share needs already), so the
//!   strokes never come back in the video, where everyone draws them
//!   already.
//! - A small bar at the top of the shared screen, left out of the capture
//!   too: "Anotar" opens the tools, and with a tool chosen I draw on the
//!   screen itself: a third window, which shows nothing at all, takes the
//!   mouse over the whole screen meanwhile. The mouse button (or a right
//!   click, as in Zoom) gives the mouse back to the screen. The bar can be
//!   dragged out of the way.
//!
//! They are this thread's, with its own message loop, and are drawn only
//! when something changes (and while disappearing ink fades), on the
//! processor, into bitmaps Windows composes (layered windows): the strokes
//! with tiny-skia, the icons from the interface's own SVGs, the words with
//! Windows' own text. Of the strokes, only the part that changed is drawn
//! again and handed over; with nothing to show, their window hides and
//! lets its bitmap go. Over a shared window, they follow it around, and
//! show while it is in front.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui::{Pos2, Vec2, pos2, vec2};
use serde_json::{Value, json};
use tiny_skia::{FillRule, Paint, PathBuilder, PixmapMut, Rect as SkRect, Transform};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC, DeleteObject, FW_NORMAL, FW_SEMIBOLD,
    GetMonitorInfoW, GetTextExtentPoint32W, HBITMAP, HDC, HGDIOBJ, MONITORINFO, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    GetForegroundWindow, GetMessageW, GetWindowDisplayAffinity, GetWindowRect,
    GetWindowThreadProcessId, HTCAPTION, HTCLIENT, HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW,
    IDC_CROSS, IsIconic, KillTimer, LoadCursorW, MA_NOACTIVATE, MSG, OBJID_WINDOW, PostMessageW,
    PostQuitMessage, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SetCursor, SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow,
    TranslateMessage, ULW_ALPHA, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindowIndirect,
    WDA_EXCLUDEFROMCAPTURE, WINEVENT_OUTOFCONTEXT, WM_APP, WM_CAPTURECHANGED, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
    WM_MOVE, WM_NCHITTEST, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use super::{COLORS, Ink, Look, Page, Pen, SIZES, Tool, raster};
use crate::voice::sources::Source;

/// The strokes (or the pen) changed: draw again (soon).
const REDRAW: u32 = WM_APP + 1;
/// The share ended: the windows go.
const QUIT: u32 = WM_APP + 2;
/// Drawing again (fading ink, or soon after the last time).
const TIMER: usize = 1;
/// What of my stroke being drawn waits to go out.
const FLUSH: usize = 2;

/// Drawn at most this often (about a screen's own pace).
const MIN_GAP: Duration = Duration::from_millis(16);

/// How near a stroke (in pixels, at 100 %) the eraser takes it.
const ERASER_REACH: f32 = 8.0;

/// The bar's window is at least this wide (at 100 %), for the tips.
const MIN_BAR_WIDTH: f32 = 560.0;

/// What is under the windows.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// A whole screen (its HMONITOR).
    Monitor(isize),
    /// One window (its HWND).
    Window(isize),
}

/// What the tests' report says of it.
#[derive(Default)]
struct Stats {
    renders: AtomicU64,
    /// Strokes drawn the last time.
    strokes: AtomicU64,
    /// Windows leaves it out of every capture.
    excluded: AtomicBool,
    shown: AtomicBool,
    /// The bar at the top of the screen shows.
    bar: AtomicBool,
}

/// The windows over my shared screen, while it lasts.
pub struct Overlay {
    hwnd: Arc<AtomicIsize>,
    stats: Arc<Stats>,
    ink: Arc<Ink>,
}

impl Overlay {
    /// Starts showing the strokes on `page` (my screen share's) over
    /// `source`, the screen or window shared, with the bar to draw on it.
    /// None for the camera, or when Windows cannot keep the windows out of
    /// captures: they would come back in the video, where everyone draws
    /// the strokes already.
    pub fn start(ink: Arc<Ink>, source: Source, page: Page) -> Option<Overlay> {
        let target = match source {
            Source::Monitor(m) => Target::Monitor(m),
            Source::Window(w) => Target::Window(w),
            Source::Camera => return None,
        };
        let hwnd = Arc::new(AtomicIsize::new(0));
        let stats = Arc::new(Stats::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (t_ink, t_hwnd, t_stats) = (ink.clone(), hwnd.clone(), stats.clone());
        std::thread::Builder::new()
            .name("anotacao-tela".into())
            .spawn(move || run(t_ink, target, page, &t_hwnd, t_stats, &ready_tx))
            .ok()?;
        // The windows are made in a moment: waited for, so the first
        // strokes are not missed.
        if ready_rx.recv_timeout(Duration::from_secs(2)) != Ok(true) {
            crate::logln!("anotação: a janela sobre a tela não abriu");
            return None;
        }
        let poke = hwnd.clone();
        ink.watch_mine(Some(Arc::new(move || post(&poke, REDRAW))));
        post(&hwnd, REDRAW);
        Some(Overlay { hwnd, stats, ink })
    }

    /// For the tests: how it went, and whether Windows leaves it out of
    /// captures (asked again of Windows).
    pub fn report(&self) -> Value {
        let h = self.hwnd.load(Ordering::Acquire);
        let mut affinity = 0u32;
        // SAFETY: a read-only question about our own window.
        let asked = h != 0 && unsafe { GetWindowDisplayAffinity(h as HWND, &mut affinity) } != 0;
        json!({
            "renders": self.stats.renders.load(Ordering::Relaxed),
            "strokes": self.stats.strokes.load(Ordering::Relaxed),
            "shown": self.stats.shown.load(Ordering::Relaxed),
            "bar": self.stats.bar.load(Ordering::Relaxed),
            "excluded": asked && affinity == WDA_EXCLUDEFROMCAPTURE,
        })
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.ink.watch_mine(None);
        post(&self.hwnd, QUIT);
    }
}

fn post(hwnd: &AtomicIsize, msg: u32) {
    let h = hwnd.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: posting to our own window; once it is gone this fails.
        unsafe {
            PostMessageW(h as HWND, msg, 0, 0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The windows' thread: from making them to its message loop's end.
fn run(
    ink: Arc<Ink>,
    target: Target,
    page: Page,
    slot: &AtomicIsize,
    stats: Arc<Stats>,
    ready: &mpsc::Sender<bool>,
) {
    let rect = target_rect(target);
    let corner = RECT {
        left: rect.left,
        top: rect.top,
        right: rect.left + 1,
        bottom: rect.top + 1,
    };
    let windows = [
        // The strokes: letting every click through.
        create(
            "BabuTelaAnotacao",
            "Anotações do Babu Tela",
            WS_EX_LAYERED | WS_EX_TRANSPARENT,
            wndproc,
            rect,
        ),
        // While drawing, what takes the mouse: a window with nothing to
        // show at all (not even a surface of its own for Windows to lay
        // over the screen), so nothing of it can be seen, nor captured.
        create(
            "BabuTelaAnotacaoMouse",
            "Desenho do Babu Tela",
            WS_EX_NOREDIRECTIONBITMAP,
            input_proc,
            rect,
        ),
        create(
            "BabuTelaAnotacaoBarra",
            "Barra de anotação do Babu Tela",
            WS_EX_LAYERED,
            bar_proc,
            corner,
        ),
    ];
    let [Some(hwnd), Some(input), Some(bar)] = windows else {
        for h in windows.into_iter().flatten() {
            // SAFETY: our own windows, just made on this thread.
            unsafe { DestroyWindow(h) };
        }
        let _ = ready.send(false);
        return;
    };
    // SAFETY: our own windows, just made on this thread.
    let excluded = unsafe {
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) != 0
            && SetWindowDisplayAffinity(bar, WDA_EXCLUDEFROMCAPTURE) != 0
    };
    if !excluded {
        crate::logln!("anotação: o Windows não deixa a janela fora da captura; sem ela");
        for h in [bar, input, hwnd] {
            // SAFETY: as above.
            unsafe { DestroyWindow(h) };
        }
        let _ = ready.send(false);
        return;
    }
    stats.excluded.store(true, Ordering::Relaxed);
    let hooks = hooks(target);
    // SAFETY: a read-only question about our own window.
    let scale = (unsafe { GetDpiForWindow(bar) }.max(96) as f32) / 96.0;
    STATE.with(|s| {
        *s.borrow_mut() = Some(Shown {
            hwnd,
            input,
            ink,
            page,
            target,
            rect,
            visible: visible(target),
            shown: false,
            dib: None,
            drawn: None,
            placed: None,
            last: None,
            due: None,
            stats,
            mode: Mode::Closed,
            down: None,
            bar: Bar {
                hwnd: bar,
                scale,
                offset: None,
                palette: false,
                hover: None,
                pressed: None,
                items: Vec::new(),
                tip: None,
                dib: None,
                icons: HashMap::new(),
                words: HashMap::new(),
                shown: false,
                tracking: false,
                drawn: None,
            },
        });
    });
    slot.store(hwnd as isize, Ordering::Release);
    let _ = ready.send(true);
    with_state(Shown::render_bar);
    let mut msg = MSG::default();
    // SAFETY: the usual message loop of this thread's own windows.
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    slot.store(0, Ordering::Release);
    for h in hooks {
        // SAFETY: hooks this thread set.
        unsafe { UnhookWinEvent(h) };
    }
    STATE.with(|s| s.borrow_mut().take());
}

type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// One of the windows: above the others, not on the taskbar, never taking
/// the focus, and `ex` (how it shows, and whether clicks go through).
fn create(class: &str, title: &str, ex: u32, proc: WndProc, rect: RECT) -> Option<HWND> {
    let class = wide(class);
    let title = wide(title);
    let ex = ex | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    // SAFETY: the classes and the windows are this module's; the strings
    // live through the calls. Registering a second time fails, harmlessly:
    // the class is there already.
    unsafe {
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            ex,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            rect.left,
            rect.top,
            (rect.right - rect.left).max(1),
            (rect.bottom - rect.top).max(1),
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        (!hwnd.is_null()).then_some(hwnd)
    }
}

/// Where the target is on the desktop, in pixels: a screen whole, a
/// window as drawn (without the invisible resizing borders), which is what
/// its capture shows.
fn target_rect(t: Target) -> RECT {
    let mut r = RECT::default();
    match t {
        Target::Monitor(m) => {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            // SAFETY: `info` is sized as the call expects.
            if unsafe { GetMonitorInfoW(m as _, &mut info) } != 0 {
                r = info.rcMonitor;
            }
        }
        Target::Window(w) => {
            // SAFETY: the attribute is a RECT and the size says so.
            let asked = unsafe {
                DwmGetWindowAttribute(
                    w as HWND,
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut r as *mut RECT).cast::<c_void>(),
                    size_of::<RECT>() as u32,
                )
            };
            if asked != 0 {
                // SAFETY: a read-only question about a window handle.
                unsafe { GetWindowRect(w as HWND, &mut r) };
            }
        }
    }
    r
}

/// Whether the strokes should show now: over a screen, always; over a
/// window, while it is in front and not minimized.
fn visible(t: Target) -> bool {
    match t {
        Target::Monitor(_) => true,
        // SAFETY: read-only questions about window handles.
        Target::Window(w) => unsafe {
            IsIconic(w as HWND) == 0 && GetForegroundWindow() as isize == w
        },
    }
}

/// Over a window: told when it moves or is minimized, and when another
/// window comes to the front (on this thread, from its message loop).
fn hooks(t: Target) -> Vec<HWINEVENTHOOK> {
    let Target::Window(w) = t else {
        return Vec::new();
    };
    let mut pid = 0u32;
    // SAFETY: read-only; the hooks call back on this thread while it reads
    // its messages (WINEVENT_OUTOFCONTEXT).
    unsafe {
        let tid = GetWindowThreadProcessId(w as HWND, &mut pid);
        [
            (
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                pid,
                tid,
            ),
            (
                EVENT_SYSTEM_MINIMIZESTART,
                EVENT_SYSTEM_MINIMIZEEND,
                pid,
                tid,
            ),
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, 0, 0),
        ]
        .into_iter()
        .map(|(from, to, pid, tid)| {
            SetWinEventHook(
                from,
                to,
                null_mut(),
                Some(on_event),
                pid,
                tid,
                WINEVENT_OUTOFCONTEXT,
            )
        })
        .filter(|h| !h.is_null())
        .collect()
    }
}

/// A part of a bitmap, in whole pixels, from x0, y0 up to (not including)
/// x1, y1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Area {
    /// Everything within `pad` pixels of the points (`min` to `max`).
    fn around(min: Pos2, max: Pos2, pad: f32) -> Area {
        Area {
            x0: (min.x - pad).floor() as i32,
            y0: (min.y - pad).floor() as i32,
            x1: (max.x + pad).ceil() as i32 + 1,
            y1: (max.y + pad).ceil() as i32 + 1,
        }
    }

    fn join(self, o: Area) -> Area {
        Area {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    /// What of it is within a `w` × `h` bitmap, if anything.
    fn within(self, w: i32, h: i32) -> Option<Area> {
        let a = Area {
            x0: self.x0.clamp(0, w),
            y0: self.y0.clamp(0, h),
            x1: self.x1.clamp(0, w),
            y1: self.y1.clamp(0, h),
        };
        (a.x0 < a.x1 && a.y0 < a.y1).then_some(a)
    }
}

fn join(a: Option<Area>, b: Option<Area>) -> Option<Area> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.join(b)),
        (a, b) => a.or(b),
    }
}

/// A bitmap a window is drawn from: 32 bits, blue first, premultiplied.
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    fn new(w: i32, h: i32) -> Option<Dib> {
        // SAFETY: every handle made here is released in Drop (or here, on
        // failure); `bits` points into the DIB section, which lives as long.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
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
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap);
            Some(Dib {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                w,
                h,
            })
        }
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the DIB section is w × h × 4 bytes, and only this borrows it.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.w as usize * self.h as usize * 4) }
    }

    /// Clears `a` (within the bitmap) to nothing.
    fn clear(&mut self, a: Area) {
        let w = self.w as usize;
        let bytes = self.bytes();
        for y in a.y0 as usize..a.y1 as usize {
            bytes[(y * w + a.x0 as usize) * 4..(y * w + a.x1 as usize) * 4].fill(0);
        }
    }

    /// Shows the bitmap as `hwnd`, at `at` on the desktop: all of it, or
    /// only what changed in `dirty` (the rest of the window keeps what it
    /// showed).
    fn show(&self, hwnd: HWND, at: POINT, dirty: Option<Area>) {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE {
            cx: self.w,
            cy: self.h,
        };
        let from = POINT { x: 0, y: 0 };
        let dirty = dirty.map(|a| RECT {
            left: a.x0,
            top: a.y0,
            right: a.x1,
            bottom: a.y1,
        });
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: null_mut(),
            pptDst: &at,
            psize: &size,
            hdcSrc: self.dc,
            pptSrc: &from,
            crKey: 0,
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: dirty.as_ref().map_or(null(), |r| r as *const RECT),
        };
        // SAFETY: our window, and the bitmap selected into `self.dc`, of
        // the size given; everything `info` points to lives through the
        // call.
        unsafe {
            UpdateLayeredWindowIndirect(hwnd, &info);
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: the handles made in `new`.
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

/// Puts `hwnd` above the others and shows it, without taking the focus.
fn raise(hwnd: HWND) {
    // SAFETY: our own window.
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// What the mouse does over the shared screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The bar is only "Anotar"; the screen has the mouse.
    Closed,
    /// The tools are open; the screen has the mouse.
    Mouse,
    /// The tools are open, and the mouse draws.
    Drawing,
}

/// The windows' state, on their own thread.
struct Shown {
    /// The strokes'.
    hwnd: HWND,
    /// The one that takes the mouse while drawing.
    input: HWND,
    ink: Arc<Ink>,
    page: Page,
    target: Target,
    rect: RECT,
    visible: bool,
    shown: bool,
    dib: Option<Dib>,
    /// Where something is drawn in the bitmap now (the rest is empty).
    drawn: Option<Area>,
    /// Where the window was last shown, on the desktop.
    placed: Option<(i32, i32)>,
    /// When the strokes were last drawn, and when the timer will draw them
    /// again.
    last: Option<Instant>,
    due: Option<Instant>,
    stats: Arc<Stats>,
    mode: Mode,
    /// The mouse button is down drawing (or erasing), last at this point.
    down: Option<POINT>,
    bar: Bar,
}

thread_local! {
    static STATE: RefCell<Option<Shown>> = const { RefCell::new(None) };
}

/// Runs `f` on the windows' state, unless it is in use already (a call
/// made while drawing that comes back here: it is drawn again anyway).
fn with_state(f: impl FnOnce(&mut Shown)) {
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut()
            && let Some(s) = s.as_mut()
        {
            f(s);
        }
    });
}

fn point_of(lp: LPARAM) -> POINT {
    POINT {
        x: i32::from((lp & 0xffff) as u16 as i16),
        y: i32::from(((lp >> 16) & 0xffff) as u16 as i16),
    }
}

impl Shown {
    fn size(&self) -> (i32, i32) {
        (
            self.rect.right - self.rect.left,
            self.rect.bottom - self.rect.top,
        )
    }

    /// The strokes changed: drawn now, or as soon as a frame's time has
    /// passed since the last time.
    fn redraw_soon(&mut self) {
        let now = Instant::now();
        let soonest = self.last.map_or(now, |t| t + MIN_GAP);
        if soonest <= now {
            self.render();
        } else {
            self.schedule(soonest);
        }
        self.render_bar();
    }

    /// Draws again at `at`, unless it is due sooner already.
    fn schedule(&mut self, at: Instant) {
        if self.due.is_some_and(|d| d <= at) {
            return;
        }
        let ms = at
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, u128::from(u32::MAX)) as u32;
        // SAFETY: a timer of our own window (set again, it moves).
        unsafe { SetTimer(self.hwnd, TIMER, ms, None) };
        self.due = Some(at);
    }

    /// The target moved, or changed size, or the screens changed.
    fn moved(&mut self) {
        let rect = target_rect(self.target);
        let same = (rect.left, rect.top, rect.right, rect.bottom)
            == (
                self.rect.left,
                self.rect.top,
                self.rect.right,
                self.rect.bottom,
            );
        if !same {
            self.rect = rect;
            self.render();
            self.place_input();
            self.render_bar();
        }
    }

    fn refresh_visibility(&mut self) {
        let visible = visible(self.target);
        if visible != self.visible {
            self.visible = visible;
            self.moved();
            self.render();
            self.place_input();
            self.render_bar();
        }
    }

    /// What the mouse does now: drawing, the window that takes it is over
    /// the target; otherwise it goes, and the mouse is the screen's.
    fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        if mode != Mode::Drawing {
            self.finish_stroke();
        }
        self.mode = mode;
        if mode == Mode::Closed {
            self.bar.palette = false;
        }
        self.place_input();
        self.render_bar();
    }

    /// The window that takes the mouse while drawing: over the whole
    /// target, under the bar (whose buttons still work).
    fn place_input(&mut self) {
        let (w, h) = self.size();
        // SAFETY: our own windows.
        unsafe {
            if self.mode == Mode::Drawing && self.visible && w > 0 && h > 0 {
                SetWindowPos(
                    self.input,
                    HWND_TOPMOST,
                    self.rect.left,
                    self.rect.top,
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                if self.bar.shown {
                    raise(self.bar.hwnd);
                }
            } else {
                ShowWindow(self.input, SW_HIDE);
            }
        }
    }

    // ------------------------------------------------------------ drawing on the screen

    /// A point of the strokes' window, on the picture (0 to 1).
    fn to_page(&self, p: POINT) -> Pos2 {
        let (w, h) = self.size();
        pos2(p.x as f32 / w.max(1) as f32, p.y as f32 / h.max(1) as f32)
    }

    fn press(&mut self, p: POINT) {
        if self.mode != Mode::Drawing {
            return;
        }
        // SAFETY: our own window takes the mouse until the button is up.
        unsafe { SetCapture(self.input) };
        self.down = Some(p);
        if self.ink.pen().tool == Tool::Eraser {
            self.ink.begin_erase();
            self.erase_at(p);
        } else {
            self.ink.begin(self.page, self.to_page(p));
        }
    }

    fn drag(&mut self, p: POINT) {
        let Some(from) = self.down else {
            return;
        };
        if (p.x, p.y) == (from.x, from.y) {
            return;
        }
        self.down = Some(p);
        if self.ink.pen().tool == Tool::Eraser {
            // All along the way, so a quick stroke takes what it passed.
            let steps = ((p.x - from.x).abs().max((p.y - from.y).abs()) / 4).max(1);
            for k in 1..=steps {
                let q = POINT {
                    x: from.x + (p.x - from.x) * k / steps,
                    y: from.y + (p.y - from.y) * k / steps,
                };
                self.erase_at(q);
            }
        } else if let Some(wait) = self.ink.extend(self.to_page(p)) {
            let ms = wait.as_millis().clamp(1, 1000) as u32;
            // SAFETY: a timer of our own window.
            unsafe { SetTimer(self.hwnd, FLUSH, ms, None) };
        }
    }

    fn erase_at(&self, p: POINT) {
        let (w, h) = self.size();
        self.ink.erase(
            self.page,
            self.to_page(p),
            vec2(w as f32, h as f32),
            ERASER_REACH * self.bar.scale,
        );
    }

    fn finish_stroke(&mut self) {
        if self.down.take().is_none() {
            return;
        }
        if self.ink.pen().tool == Tool::Eraser {
            self.ink.end_erase();
        } else {
            self.ink.finish();
        }
        // SAFETY: our own window's capture, and timer.
        unsafe {
            ReleaseCapture();
            KillTimer(self.hwnd, FLUSH);
        }
    }

    // ------------------------------------------------------------ the strokes

    fn render(&mut self) {
        let now = Instant::now();
        self.last = Some(now);
        let (looks, next) = self.ink.looks(self.page, now);
        self.stats.renders.fetch_add(1, Ordering::Relaxed);
        self.stats
            .strokes
            .store(looks.len() as u64, Ordering::Relaxed);
        let (w, h) = self.size();
        if looks.is_empty() || !self.visible || w <= 0 || h <= 0 {
            if self.shown {
                // SAFETY: our own window.
                unsafe { ShowWindow(self.hwnd, SW_HIDE) };
                self.shown = false;
            }
            // Nothing to show: its memory goes until there is.
            self.dib = None;
            self.drawn = None;
        } else {
            let fresh = self.dib.as_ref().is_none_or(|d| (d.w, d.h) != (w, h));
            if fresh {
                self.dib = Dib::new(w, h);
                self.drawn = None;
            }
            let Some(dib) = self.dib.as_mut() else {
                return;
            };
            // Whose names show, next to which strokes.
            let names: Vec<(usize, String)> = looks
                .iter()
                .enumerate()
                .filter(|(_, l)| l.tag)
                .map(|(i, l)| (i, self.ink.name_of(l.author)))
                .collect();
            let (area, dirty) = paint(
                dib,
                &mut self.bar.words,
                &looks,
                &names,
                self.bar.scale,
                self.drawn,
            );
            self.drawn = area;
            let at = POINT {
                x: self.rect.left,
                y: self.rect.top,
            };
            // A window shown again, made again, or moved takes all of it.
            let whole = fresh || !self.shown || self.placed != Some((at.x, at.y));
            if whole || dirty.is_some() {
                dib.show(self.hwnd, at, if whole { None } else { dirty });
                self.placed = Some((at.x, at.y));
            }
            if !self.shown {
                raise(self.hwnd);
                self.shown = true;
                // The bar stays over the strokes.
                if self.bar.shown {
                    raise(self.bar.hwnd);
                }
            }
        }
        self.stats.shown.store(self.shown, Ordering::Relaxed);
        // Ink fading: again in a frame's time, or when it begins to.
        if let Some(at) = next {
            self.schedule(at.max(now + MIN_GAP));
        }
    }

    // ------------------------------------------------------------ the bar

    fn render_bar(&mut self) {
        let pen = self.ink.pen();
        let can = (self.ink.can_undo(), self.ink.can_redo());
        let everyone = self.ink.everyone(self.page.surface);
        let any = self.ink.has_strokes(self.page.surface).1;
        let (rect, visible) = (self.rect, self.visible);
        let mode = self.mode;
        self.bar.render(
            rect,
            visible,
            &BarState {
                mode,
                pen,
                undo: can.0,
                redo: can.1,
                everyone,
                any,
            },
        );
        self.stats.bar.store(self.bar.shown, Ordering::Relaxed);
    }

    /// A click on the bar's `item`.
    fn act(&mut self, item: Item) {
        let pen = self.ink.pen();
        let surface = self.page.surface;
        match item {
            Item::Open => self.set_mode(Mode::Drawing),
            Item::Pointer => self.set_mode(Mode::Mouse),
            Item::Tool(tool) => {
                self.ink.set_pen(Pen { tool, ..pen });
                self.set_mode(Mode::Drawing);
            }
            Item::Color => self.bar.palette = !self.bar.palette,
            Item::Swatch(color) => {
                let tool = if pen.tool == Tool::Eraser {
                    Tool::Pen
                } else {
                    pen.tool
                };
                self.ink.set_pen(Pen { tool, color, ..pen });
                self.bar.palette = false;
                self.set_mode(Mode::Drawing);
            }
            Item::Size(size) => {
                let tool = if pen.tool == Tool::Eraser {
                    Tool::Pen
                } else {
                    pen.tool
                };
                self.ink.set_pen(Pen { tool, size, ..pen });
                self.bar.palette = false;
                self.set_mode(Mode::Drawing);
            }
            Item::Undo => self.ink.undo(),
            Item::Redo => self.ink.redo(),
            Item::Clear => self.ink.clear_surface(surface, true),
            Item::Who => {
                let everyone = self.ink.everyone(surface);
                self.ink.set_everyone(surface, !everyone);
            }
            Item::Close => self.set_mode(Mode::Closed),
        }
        self.render_bar();
    }
}

/// Draws `looks` into `dib`, with the names of `names` (which look, whose
/// name) next to theirs, when what is in it now is all within `drawn`.
/// Only what changes is cleared and drawn again: where things were, and
/// where they are now (the rest of the bitmap is empty already). Returns
/// where things are now, and what changed (all that Windows needs again).
fn paint(
    dib: &mut Dib,
    words: &mut HashMap<(String, u32), Words>,
    looks: &[Look],
    names: &[(usize, String)],
    scale: f32,
    drawn: Option<Area>,
) -> (Option<Area>, Option<Area>) {
    let (w, h) = (dib.w, dib.h);
    let (fw, fh) = (w as f32, h as f32);
    let mut area = None;
    for l in looks {
        area = join(area, stroke_area(l, fw, fh));
    }
    for (i, name) in names {
        if let Some((at, size)) = tag_place(words, &looks[*i], name, scale, fw, fh) {
            area = join(area, Some(Area::around(at, at + size, 2.0)));
        }
    }
    for l in looks.iter().filter(|l| l.tool == Tool::Text) {
        if let Some((at, size)) = text_place(words, l, fw, fh) {
            area = join(area, Some(Area::around(at, at + size, 2.0)));
        }
    }
    let area = area.and_then(|a| a.within(w, h));
    let dirty = join(drawn, area);
    if let Some(d) = dirty {
        dib.clear(d);
        if let Some(mut pixmap) = PixmapMut::from_bytes(dib.bytes(), w as u32, h as u32) {
            raster::draw(&mut pixmap, looks, true);
        }
        for l in looks.iter().filter(|l| l.tool == Tool::Text) {
            text_blit(words, dib, l);
        }
        for (i, name) in names {
            tag(words, dib, &looks[*i], name, scale);
        }
    }
    (area, dirty)
}

/// Where someone's text annotation goes in a `w` × `h` bitmap and how big
/// Windows drew it at the chosen font size.
fn text_place(
    words: &mut HashMap<(String, u32), Words>,
    l: &Look,
    w: f32,
    h: f32,
) -> Option<(Pos2, Vec2)> {
    let text = l.text.as_deref()?;
    let anchor = l.points.first()?;
    if text.is_empty() || l.color[3] == 0 {
        return None;
    }
    let px = (l.width * h).max(10.0).round() as u32;
    let t = words_for(words, text, px, false)?;
    let at = pos2(anchor.x * w, anchor.y * h);
    Some((at, vec2(t.w as f32, t.h as f32)))
}

/// Draws someone's text annotation into `dib`, in the author's colour.
fn text_blit(words: &mut HashMap<(String, u32), Words>, dib: &mut Dib, l: &Look) {
    let (w, h) = (dib.w, dib.h);
    let Some((at, _)) = text_place(words, l, w as f32, h as f32) else {
        return;
    };
    let text = match l.text.as_deref() {
        Some(t) => t,
        None => return,
    };
    let px = (l.width * h as f32).max(10.0).round() as u32;
    let Some(t) = words_for(words, text, px, false) else {
        return;
    };
    blit_words(dib.bytes(), w, h, t, at, l.color);
}

/// Everything a stroke can touch in a `w` × `h` bitmap: its lines (as
/// raster.rs draws them), half its width around them, and the edge's
/// smoothing.
fn stroke_area(l: &Look, w: f32, h: f32) -> Option<Area> {
    if l.color[3] == 0 {
        return None;
    }
    let map = |p: Pos2| pos2(p.x * w, p.y * h);
    let mut points = super::lines(l.tool, &l.points, l.width, map, h)
        .into_iter()
        .flatten();
    let first = points.next()?;
    let (min, max) = points.fold((first, first), |(lo, hi), p| (lo.min(p), hi.max(p)));
    Some(Area::around(min, max, (l.width * h).max(1.0) / 2.0 + 2.0))
}

/// The words in someone's name tag, at `px` pixels, and its padding.
fn tag_words(scale: f32) -> (u32, Vec2) {
    ((13.0 * scale).round() as u32, vec2(6.0, 3.0) * scale)
}

/// Where someone's name goes next to their stroke in a `w` × `h` bitmap
/// (its corner and size), if it shows.
fn tag_place(
    words: &mut HashMap<(String, u32), Words>,
    l: &Look,
    name: &str,
    scale: f32,
    w: f32,
    h: f32,
) -> Option<(Pos2, Vec2)> {
    let last = l.points.last()?;
    if name.is_empty() || l.color[3] == 0 {
        return None;
    }
    let (px, pad) = tag_words(scale);
    let text = words_for(words, name, px, true)?;
    let at = pos2(last.x * w + 12.0 * scale, last.y * h + 10.0 * scale);
    Some((at, vec2(text.w as f32, text.h as f32) + pad * 2.0))
}

/// Someone's name next to their stroke, on their colour.
fn tag(words: &mut HashMap<(String, u32), Words>, dib: &mut Dib, l: &Look, name: &str, scale: f32) {
    let (w, h) = (dib.w, dib.h);
    let Some((at, size)) = tag_place(words, l, name, scale, w as f32, h as f32) else {
        return;
    };
    let [r, g, b, a] = l.color;
    let (px, pad) = tag_words(scale);
    let Some(text) = words_for(words, name, px, true) else {
        return;
    };
    let light = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b) > 160.0;
    let ink = if light {
        [0x1c, 0x1c, 0x1e]
    } else {
        [0xff, 0xff, 0xff]
    };
    let bytes = dib.bytes();
    if let Some(mut pm) = PixmapMut::from_bytes(bytes, w as u32, h as u32) {
        rounded(&mut pm, at, size, 6.0 * scale, [r, g, b, a.min(235)]);
    }
    blit_words(
        dib.bytes(),
        w,
        h,
        text,
        at + pad,
        [ink[0], ink[1], ink[2], a],
    );
}

// ---------------------------------------------------------------- the bar

/// What the bar can be clicked for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Item {
    /// "Anotar": the tools open.
    Open,
    /// Back to the mouse.
    Pointer,
    Tool(Tool),
    /// The colours and widths, under the bar.
    Color,
    Swatch(usize),
    Size(usize),
    Undo,
    Redo,
    /// Everyone's strokes go.
    Clear,
    /// Everyone may annotate, or only I.
    Who,
    Close,
}

impl Item {
    /// What the bar says of it while the mouse is over it.
    fn tip(self, s: &BarState) -> &'static str {
        match self {
            Item::Open => "Desenhar sobre a tela que você compartilha",
            Item::Pointer => "Voltar ao mouse (ou clique com o botão direito)",
            Item::Tool(t) => t.name(),
            Item::Color => "Cor e espessura",
            Item::Swatch(_) => "Cor",
            Item::Size(0) => "Fina",
            Item::Size(1) => "Média",
            Item::Size(_) => "Grossa",
            Item::Undo => "Desfazer",
            Item::Redo => "Refazer",
            Item::Clear => "Limpar tudo",
            Item::Who if s.everyone => "Todos podem anotar: clique para só você",
            Item::Who => "Só você pode anotar: clique para todos",
            Item::Close => "Parar de anotar",
        }
    }
}

/// What the bar shows.
#[derive(Clone, Copy, PartialEq)]
struct BarState {
    mode: Mode,
    pen: Pen,
    undo: bool,
    redo: bool,
    everyone: bool,
    /// There are strokes to clear.
    any: bool,
}

/// Words drawn by Windows, as how much each pixel is covered (0 to 255).
struct Words {
    w: i32,
    h: i32,
    cover: Vec<u8>,
}

/// The bar at the top of the shared screen.
struct Bar {
    hwnd: HWND,
    /// The screen's scale (1 at 96 dpi).
    scale: f32,
    /// Where it was dragged to, from its place at the top in the middle.
    offset: Option<(i32, i32)>,
    palette: bool,
    hover: Option<Item>,
    /// Pressed with the mouse: clicked if the button comes up on it.
    pressed: Option<Item>,
    /// Where each thing is, in the window.
    items: Vec<(Item, SkRect)>,
    /// Where the tip goes, under the bar.
    tip: Option<(f32, f32)>,
    dib: Option<Dib>,
    /// The icons, drawn once for each colour.
    icons: HashMap<(&'static str, [u8; 3]), resvg::usvg::Tree>,
    words: HashMap<(String, u32), Words>,
    shown: bool,
    /// Windows tells it when the mouse leaves.
    tracking: bool,
    /// What it showed the last time it was drawn: drawn again only when
    /// that changes (every stroke changing does not change the bar).
    drawn: Option<BarKey>,
}

/// Everything the bar's picture and place come from.
#[derive(Clone, Copy, PartialEq)]
struct BarKey {
    rect: (i32, i32, i32, i32),
    state: BarState,
    hover: Option<Item>,
    palette: bool,
    offset: Option<(i32, i32)>,
}

/// The interface's colours (theme.rs).
const TEXT: [u8; 3] = [0xec, 0xeb, 0xff];
const MUTED: [u8; 3] = [0xa3, 0xa2, 0xc9];
const ACCENT: [u8; 3] = [0x85, 0x90, 0xff];
const ACCENT_INK: [u8; 3] = [0x0d, 0x0c, 0x26];

/// The icons' drawings (assets/icons, which `cargo xtask icones` writes).
fn icon_svg(name: &str) -> &'static str {
    match name {
        "edit" => include_str!("../../assets/icons/edit.svg"),
        "pointer" => include_str!("../../assets/icons/pointer.svg"),
        "vanish" => include_str!("../../assets/icons/vanish.svg"),
        "marker" => include_str!("../../assets/icons/marker.svg"),
        "arrow" => include_str!("../../assets/icons/arrow.svg"),
        "square" => include_str!("../../assets/icons/square.svg"),
        "circle" => include_str!("../../assets/icons/circle.svg"),
        "eraser" => include_str!("../../assets/icons/eraser.svg"),
        "text" => include_str!("../../assets/icons/text.svg"),
        "undo" => include_str!("../../assets/icons/undo.svg"),
        "redo" => include_str!("../../assets/icons/redo.svg"),
        "trash" => include_str!("../../assets/icons/trash.svg"),
        "users" => include_str!("../../assets/icons/users.svg"),
        "users-off" => include_str!("../../assets/icons/users-off.svg"),
        _ => include_str!("../../assets/icons/x.svg"),
    }
}

fn tool_icon(t: Tool) -> &'static str {
    match t {
        Tool::Vanish => "vanish",
        Tool::Pen => "edit",
        Tool::Highlighter => "marker",
        Tool::Arrow => "arrow",
        Tool::Rect => "square",
        Tool::Ellipse => "circle",
        Tool::Text => "text",
        Tool::Eraser => "eraser",
    }
}

const TOOLS: [Tool; 8] = [
    Tool::Vanish,
    Tool::Pen,
    Tool::Highlighter,
    Tool::Arrow,
    Tool::Rect,
    Tool::Ellipse,
    Tool::Text,
    Tool::Eraser,
];

/// One row of the bar, laid out from the left.
struct Row {
    items: Vec<(Item, SkRect)>,
    x: f32,
    top: f32,
    side: f32,
    gap: f32,
}

impl Row {
    /// `item`, `w` wide, at the next whole pixel (the icons stay sharp).
    fn put(&mut self, item: Item, w: f32) {
        if let Some(r) = SkRect::from_xywh(self.x.round(), self.top.round(), w, self.side) {
            self.items.push((item, r));
        }
        self.x += w + self.gap;
    }

    /// A wider gap between groups.
    fn space(&mut self, w: f32) {
        self.x += w;
    }

    /// How wide it came out, with `pad` at each end.
    fn width(&self, pad: f32) -> f32 {
        self.x - self.gap + pad
    }

    /// Its things, moved `dx` to the right.
    fn shifted(self, dx: f32) -> impl Iterator<Item = (Item, SkRect)> {
        self.items.into_iter().filter_map(move |(item, r)| {
            SkRect::from_xywh(r.x() + dx, r.y(), r.width(), r.height()).map(|r| (item, r))
        })
    }
}

impl Bar {
    /// Lays the bar out (in the window's pixels): where each thing on it
    /// is, and how big the window is. The bar, and the colours under it,
    /// are each in the middle of the window, which is wider than them and
    /// taller, for the tips under them.
    fn layout(&mut self, s: &BarState) -> (f32, f32) {
        let k = self.scale;
        let side = 32.0 * k;
        let pad = 6.0 * k;
        let gap = 2.0 * k;
        let sep = 7.0 * k;
        let row = |top: f32| Row {
            items: Vec::new(),
            x: pad,
            top,
            side,
            gap,
        };
        let mut bar = row(pad);
        if s.mode == Mode::Closed {
            let words = words_for(&mut self.words, "Anotar", (14.0 * k).round() as u32, true)
                .map_or(0.0, |t| t.w as f32);
            bar.put(Item::Open, 18.0 * k + 8.0 * k + words + 20.0 * k);
        } else {
            bar.put(Item::Pointer, side);
            bar.space(sep);
            for t in TOOLS {
                bar.put(Item::Tool(t), side);
            }
            bar.space(sep);
            bar.put(Item::Color, side);
            bar.space(sep);
            bar.put(Item::Undo, side);
            bar.put(Item::Redo, side);
            bar.space(sep);
            bar.put(Item::Clear, side);
            bar.put(Item::Who, side);
            bar.space(sep);
            bar.put(Item::Close, side);
        }
        let mut height = side + pad * 2.0;
        let mut colors = None;
        if self.palette && s.mode != Mode::Closed {
            // The colours, then the widths, under the bar.
            let mut r = row(height + 6.0 * k + pad);
            for i in 0..COLORS.len() {
                r.put(Item::Swatch(i), side);
            }
            r.space(sep);
            for i in 0..SIZES.len() {
                r.put(Item::Size(i), side);
            }
            height += 6.0 * k + side + pad * 2.0;
            colors = Some(r);
        }
        let widths = (
            bar.width(pad),
            colors.as_ref().map_or(0.0, |r| r.width(pad)),
        );
        let width = widths.0.max(widths.1).max(MIN_BAR_WIDTH * k).ceil();
        self.items = bar.shifted(((width - widths.0) / 2.0).round()).collect();
        if let Some(r) = colors {
            self.items
                .extend(r.shifted(((width - widths.1) / 2.0).round()));
        }
        self.tip = Some((pad, height + 6.0 * k));
        // Room under it all for the tip.
        (width, height + 6.0 * k + 28.0 * k)
    }

    /// Where the bar goes over `rect`: at the top in the middle, or where
    /// it was dragged to.
    fn place(&self, rect: RECT, w: i32) -> POINT {
        let (dx, dy) = self
            .offset
            .unwrap_or((0, (10.0 * self.scale).round() as i32));
        let middle = rect.left + (rect.right - rect.left) / 2;
        POINT {
            x: middle - w / 2 + dx,
            y: rect.top + dy,
        }
    }

    fn render(&mut self, rect: RECT, visible: bool, s: &BarState) {
        if !visible {
            if self.shown {
                // SAFETY: our own window.
                unsafe { ShowWindow(self.hwnd, SW_HIDE) };
                self.shown = false;
            }
            self.drawn = None;
            return;
        }
        let key = BarKey {
            rect: (rect.left, rect.top, rect.right, rect.bottom),
            state: *s,
            hover: self.hover,
            palette: self.palette,
            offset: self.offset,
        };
        if self.shown && self.drawn == Some(key) {
            return;
        }
        self.drawn = Some(key);
        let (w, h) = self.layout(s);
        let (w, h) = (w.ceil() as i32, h.ceil() as i32);
        if self.dib.as_ref().is_none_or(|d| (d.w, d.h) != (w, h)) {
            self.dib = Dib::new(w, h);
        }
        let k = self.scale;
        let items = self.items.clone();
        let hover = self.hover;
        let palette = self.palette && s.mode != Mode::Closed;
        let tip = self.tip;
        let Some(mut dib) = self.dib.take() else {
            return;
        };
        dib.bytes().fill(0);
        {
            let (dw, dh) = (dib.w as u32, dib.h as u32);
            let Some(mut pm) = PixmapMut::from_bytes(dib.bytes(), dw, dh) else {
                self.dib = Some(dib);
                return;
            };
            // The bar, the size of what is on it.
            let first = items.first().map_or(0.0, |(_, r)| r.left());
            let last = items
                .iter()
                .take_while(|(i, _)| !matches!(i, Item::Swatch(_) | Item::Size(_)))
                .last()
                .map_or(0.0, |(_, r)| r.right());
            let pad = 6.0 * k;
            let side = 32.0 * k;
            rounded(
                &mut pm,
                pos2(first - pad, 0.0),
                vec2(last - first + pad * 2.0, side + pad * 2.0),
                (side + pad * 2.0) / 2.0,
                [0, 0, 0, 0xdc],
            );
            if palette {
                let first = items
                    .iter()
                    .find(|(i, _)| matches!(i, Item::Swatch(_)))
                    .map_or(0.0, |(_, r)| r.left());
                let last = items.last().map_or(0.0, |(_, r)| r.right());
                let top = items.last().map_or(0.0, |(_, r)| r.top());
                rounded(
                    &mut pm,
                    pos2(first - pad, top - pad),
                    vec2(last - first + pad * 2.0, side + pad * 2.0),
                    10.0 * k,
                    [0, 0, 0, 0xdc],
                );
            }
            for (item, r) in &items {
                self.draw_item(&mut pm, *item, *r, hover == Some(*item), s);
            }
        }
        // The words, by Windows (into the same bitmap).
        let (dw, dh) = (dib.w, dib.h);
        if let Some((_, r)) = items.iter().find(|(i, _)| *i == Item::Open)
            && let Some(t) = words_for(&mut self.words, "Anotar", (14.0 * k).round() as u32, true)
        {
            let at = pos2(
                r.left() + 34.0 * k,
                r.top() + (r.height() - t.h as f32) / 2.0,
            );
            blit_words(dib.bytes(), dw, dh, t, at, [TEXT[0], TEXT[1], TEXT[2], 255]);
        }
        if let (Some(item), Some((x, y))) = (hover, tip)
            && let Some(t) = words_for(
                &mut self.words,
                item.tip(s),
                (12.0 * k).round() as u32,
                false,
            )
        {
            let pad = vec2(8.0, 4.0) * k;
            let size = vec2(t.w as f32, t.h as f32) + pad * 2.0;
            let x = items
                .iter()
                .find(|(i, _)| *i == item)
                .map_or(x, |(_, r)| (r.left() - 4.0 * k).max(0.0))
                .min(dw as f32 - size.x)
                .max(0.0);
            if let Some(mut pm) = PixmapMut::from_bytes(dib.bytes(), dw as u32, dh as u32) {
                rounded(&mut pm, pos2(x, y), size, 6.0 * k, [0x12, 0x11, 0x24, 0xf0]);
            }
            blit_words(
                dib.bytes(),
                dw,
                dh,
                t,
                pos2(x, y) + pad,
                [TEXT[0], TEXT[1], TEXT[2], 255],
            );
        }
        dib.show(self.hwnd, self.place(rect, w), None);
        if !self.shown {
            raise(self.hwnd);
            self.shown = true;
        }
        self.dib = Some(dib);
    }

    fn draw_item(&mut self, pm: &mut PixmapMut, item: Item, r: SkRect, hover: bool, s: &BarState) {
        let k = self.scale;
        let on = match item {
            Item::Pointer => s.mode == Mode::Mouse,
            Item::Tool(t) => s.mode == Mode::Drawing && s.pen.tool == t,
            Item::Swatch(i) => i == s.pen.color,
            Item::Size(i) => i == s.pen.size,
            _ => false,
        };
        let enabled = match item {
            Item::Undo => s.undo,
            Item::Redo => s.redo,
            Item::Clear => s.any,
            _ => true,
        };
        let center = pos2(r.left() + r.width() / 2.0, r.top() + r.height() / 2.0);
        let radius = 8.0 * k;
        match item {
            Item::Swatch(i) => {
                let [cr, cg, cb] = COLORS[i];
                // A faint ring, so the black one shows on the bar.
                circle(
                    pm,
                    center,
                    r.height() / 2.0 - 3.0 * k,
                    [0xff, 0xff, 0xff, 0x40],
                );
                if on || hover {
                    circle(
                        pm,
                        center,
                        r.height() / 2.0 - 1.0 * k,
                        [ACCENT[0], ACCENT[1], ACCENT[2], if on { 255 } else { 0x90 }],
                    );
                }
                circle(pm, center, r.height() / 2.0 - 4.0 * k, [cr, cg, cb, 255]);
                return;
            }
            Item::Size(i) => {
                if on || hover {
                    rounded(
                        pm,
                        pos2(r.left(), r.top()),
                        vec2(r.width(), r.height()),
                        radius,
                        [0xff, 0xff, 0xff, if on { 0x30 } else { 0x18 }],
                    );
                }
                let [cr, cg, cb] = COLORS[s.pen.color];
                circle(pm, center, (2.5 + i as f32 * 2.5) * k, [cr, cg, cb, 255]);
                return;
            }
            _ => {}
        }
        if on {
            rounded(
                pm,
                pos2(r.left(), r.top()),
                vec2(r.width(), r.height()),
                radius,
                [ACCENT[0], ACCENT[1], ACCENT[2], 255],
            );
        } else if hover && enabled {
            rounded(
                pm,
                pos2(r.left(), r.top()),
                vec2(r.width(), r.height()),
                radius,
                [0xff, 0xff, 0xff, 0x1c],
            );
        }
        let color = if on {
            ACCENT_INK
        } else if hover && enabled {
            TEXT
        } else {
            MUTED
        };
        let alpha = if enabled { 1.0 } else { 0.35 };
        let size = 18.0 * k;
        let icon = match item {
            Item::Open => {
                let at = pos2(r.left() + 12.0 * k + size / 2.0, center.y);
                self.icon(pm, "edit", TEXT, at, size, 1.0);
                return;
            }
            Item::Pointer => "pointer",
            Item::Tool(t) => tool_icon(t),
            Item::Color => {
                let [cr, cg, cb] = COLORS[s.pen.color];
                circle(
                    pm,
                    center,
                    (7.0 + s.pen.size as f32 * 1.5) * k + 1.5 * k,
                    [0xff, 0xff, 0xff, 0x90],
                );
                circle(
                    pm,
                    center,
                    (7.0 + s.pen.size as f32 * 1.5) * k,
                    [cr, cg, cb, 255],
                );
                return;
            }
            Item::Undo => "undo",
            Item::Redo => "redo",
            Item::Clear => "trash",
            Item::Who if s.everyone => "users",
            Item::Who => "users-off",
            Item::Close => "x",
            Item::Swatch(_) | Item::Size(_) => return,
        };
        self.icon(pm, icon, color, center, size, alpha);
    }

    /// Icon `name` in `color`, `size` pixels square, centred on `at`.
    fn icon(
        &mut self,
        pm: &mut PixmapMut,
        name: &'static str,
        color: [u8; 3],
        at: Pos2,
        size: f32,
        alpha: f32,
    ) {
        // Drawn blue first, as the bitmap is (see raster.rs).
        let [r, g, b] = color;
        let tree = match self.icons.entry((name, [b, g, r])) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let hex = format!("#{b:02x}{g:02x}{r:02x}");
                let svg = icon_svg(name).replace("#ffffff", &hex);
                let options = resvg::usvg::Options::default();
                let Ok(tree) = resvg::usvg::Tree::from_data(svg.as_bytes(), &options) else {
                    return;
                };
                e.insert(tree)
            }
        };
        let s = size / tree.size().width().max(1.0);
        let corner = at - Vec2::splat(size / 2.0);
        if alpha >= 1.0 {
            resvg::render(
                tree,
                Transform::from_row(s, 0.0, 0.0, s, corner.x, corner.y),
                pm,
            );
        } else {
            // Drawn apart, and laid on at that opacity: from the whole
            // pixel before its corner, so it lands where it would whole.
            let (x0, y0) = (corner.x.floor() - 1.0, corner.y.floor() - 1.0);
            let side = size.ceil() as u32 + 3;
            if let Some(mut apart) = tiny_skia::Pixmap::new(side, side) {
                let t = Transform::from_row(s, 0.0, 0.0, s, corner.x - x0, corner.y - y0);
                resvg::render(tree, t, &mut apart.as_mut());
                let paint = tiny_skia::PixmapPaint {
                    opacity: alpha,
                    ..Default::default()
                };
                pm.draw_pixmap(
                    x0 as i32,
                    y0 as i32,
                    apart.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
        }
    }

    fn hit(&self, p: POINT) -> Option<Item> {
        let (x, y) = (p.x as f32, p.y as f32);
        self.items
            .iter()
            .find(|(_, r)| x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom())
            .map(|(i, _)| *i)
    }

    /// Whether `p` is on the bar (or the colours) at all: there, it can be
    /// dragged; elsewhere in its window, the mouse goes through.
    fn on_bar(&self, p: POINT) -> bool {
        let k = self.scale;
        let (x, y) = (p.x as f32, p.y as f32);
        let bar_bottom = 44.0 * k;
        let row = self
            .items
            .iter()
            .filter(|(i, _)| matches!(i, Item::Swatch(_) | Item::Size(_)))
            .map(|(_, r)| *r)
            .reduce(|a, b| a.join(&b).unwrap_or(a));
        let left = self.items.first().map_or(0.0, |(_, r)| r.left() - 6.0 * k);
        let right = self
            .items
            .iter()
            .take_while(|(i, _)| !matches!(i, Item::Swatch(_) | Item::Size(_)))
            .last()
            .map_or(0.0, |(_, r)| r.right() + 6.0 * k);
        (y < bar_bottom && x >= left && x < right)
            || row.is_some_and(|r| {
                x >= r.left() - 6.0 * k
                    && x < r.right() + 6.0 * k
                    && y >= r.top() - 6.0 * k
                    && y < r.bottom() + 6.0 * k
            })
    }
}

/// The words of `text` at `px` pixels, drawn by Windows once.
fn words_for<'a>(
    cache: &'a mut HashMap<(String, u32), Words>,
    text: &str,
    px: u32,
    bold: bool,
) -> Option<&'a Words> {
    let key = (format!("{}{text}", if bold { "b:" } else { "" }), px);
    if !cache.contains_key(&key) {
        let words = draw_words(text, px as i32, bold)?;
        cache.insert(key.clone(), words);
    }
    cache.get(&key)
}

/// `text` in Segoe UI, `px` pixels tall, white on black into a bitmap of
/// its own: how much each pixel is covered is how light it came out.
fn draw_words(text: &str, px: i32, bold: bool) -> Option<Words> {
    let text: Vec<u16> = text.encode_utf16().collect();
    let face = wide("Segoe UI");
    // SAFETY: every handle made here is released here; the bitmap's bits
    // are read while it lives.
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return None;
        }
        let font = CreateFontW(
            -px,
            0,
            0,
            0,
            if bold { FW_SEMIBOLD } else { FW_NORMAL } as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            DEFAULT_PITCH as u32,
            face.as_ptr(),
        );
        let old_font = SelectObject(dc, font);
        let mut size = SIZE::default();
        GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size);
        let (w, h) = (size.cx.max(1), size.cy.max(1));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let mut out = None;
        if !bitmap.is_null() && !bits.is_null() {
            let old = SelectObject(dc, bitmap);
            let len = w as usize * h as usize * 4;
            std::slice::from_raw_parts_mut(bits.cast::<u8>(), len).fill(0);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00ff_ffff);
            TextOutW(dc, 0, 0, text.as_ptr(), text.len() as i32);
            let px = std::slice::from_raw_parts(bits.cast::<u8>(), len);
            out = Some(Words {
                w,
                h,
                cover: px.as_chunks::<4>().0.iter().map(|p| p[1]).collect(),
            });
            SelectObject(dc, old);
            DeleteObject(bitmap);
        }
        SelectObject(dc, old_font);
        DeleteObject(font);
        DeleteDC(dc);
        out
    }
}

/// Lays `words` on a bitmap (blue first, premultiplied) at `at`, in
/// `color` (RGBA).
fn blit_words(bytes: &mut [u8], w: i32, h: i32, words: &Words, at: Pos2, color: [u8; 4]) {
    let (x0, y0) = (at.x.round() as i32, at.y.round() as i32);
    let [r, g, b, a] = color;
    for y in 0..words.h {
        let dy = y0 + y;
        if dy < 0 || dy >= h {
            continue;
        }
        for x in 0..words.w {
            let dx = x0 + x;
            if dx < 0 || dx >= w {
                continue;
            }
            let cover = words.cover[(y * words.w + x) as usize];
            if cover == 0 {
                continue;
            }
            let alpha = f32::from(cover) / 255.0 * f32::from(a) / 255.0;
            let i = ((dy * w + dx) * 4) as usize;
            let src = [f32::from(b), f32::from(g), f32::from(r), 255.0];
            for c in 0..4 {
                let d = f32::from(bytes[i + c]);
                bytes[i + c] = (src[c] * alpha + d * (1.0 - alpha))
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// A rounded rectangle in `color` (RGBA; drawn blue first).
fn rounded(pm: &mut PixmapMut, at: Pos2, size: Vec2, radius: f32, color: [u8; 4]) {
    let Some(rect) = SkRect::from_xywh(at.x, at.y, size.x, size.y) else {
        return;
    };
    let r = radius.min(size.x / 2.0).min(size.y / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    let (l, t, rt, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    pb.move_to(l + r, t);
    pb.line_to(rt - r, t);
    pb.quad_to(rt, t, rt, t + r);
    pb.line_to(rt, b - r);
    pb.quad_to(rt, b, rt - r, b);
    pb.line_to(l + r, b);
    pb.quad_to(l, b, l, b - r);
    pb.line_to(l, t + r);
    pb.quad_to(l, t, l + r, t);
    pb.close();
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color[2], color[1], color[0], color[3]);
        paint.anti_alias = true;
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn circle(pm: &mut PixmapMut, at: Pos2, radius: f32, color: [u8; 4]) {
    if let Some(path) = PathBuilder::from_circle(at.x, at.y, radius.max(0.5)) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color[2], color[1], color[0], color[3]);
        paint.anti_alias = true;
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

// ---------------------------------------------------------------- messages

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        REDRAW => {
            with_state(Shown::redraw_soon);
            0
        }
        WM_TIMER if wp == TIMER => {
            // SAFETY: our own window's timer.
            unsafe { KillTimer(hwnd, TIMER) };
            with_state(|s| {
                s.due = None;
                s.render();
            });
            0
        }
        WM_TIMER if wp == FLUSH => {
            // SAFETY: as above.
            unsafe { KillTimer(hwnd, FLUSH) };
            with_state(|s| {
                if let Some(wait) = s.ink.tick() {
                    let ms = wait.as_millis().clamp(1, 1000) as u32;
                    // SAFETY: as above.
                    unsafe { SetTimer(s.hwnd, FLUSH, ms, None) };
                }
            });
            0
        }
        QUIT => {
            with_state(|s| {
                // SAFETY: our own windows, on their own thread.
                unsafe {
                    DestroyWindow(s.bar.hwnd);
                    DestroyWindow(s.input);
                }
            });
            // SAFETY: as above.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_DISPLAYCHANGE => {
            with_state(Shown::moved);
            // SAFETY: the default handling of a message of our window.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        // Never taking the focus.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        // SAFETY: the default handling of a message of our window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// The window that takes the mouse while drawing on the screen.
unsafe extern "system" fn input_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            with_state(|s| s.press(point_of(lp)));
            0
        }
        WM_MOUSEMOVE => {
            with_state(|s| s.drag(point_of(lp)));
            0
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            with_state(Shown::finish_stroke);
            0
        }
        // A right click gives the mouse back, as in Zoom.
        WM_RBUTTONUP => {
            with_state(|s| s.set_mode(Mode::Mouse));
            0
        }
        WM_SETCURSOR => {
            // SAFETY: a system cursor.
            unsafe { SetCursor(LoadCursorW(null_mut(), IDC_CROSS)) };
            1
        }
        // Nothing to paint: it shows nothing.
        WM_ERASEBKGND => 1,
        // Never taking the focus.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        // SAFETY: the default handling of a message of our window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // On a button: a click; on the rest of the bar: dragging it;
            // anywhere else: whatever is under it.
            let mut answer = HTTRANSPARENT as LRESULT;
            with_state(|s| {
                let mut r = RECT::default();
                // SAFETY: a read-only question about our own window.
                unsafe { GetWindowRect(hwnd, &mut r) };
                let screen = point_of(lp);
                let p = POINT {
                    x: screen.x - r.left,
                    y: screen.y - r.top,
                };
                answer = if s.bar.hit(p).is_some() {
                    HTCLIENT as LRESULT
                } else if s.bar.on_bar(p) {
                    HTCAPTION as LRESULT
                } else {
                    HTTRANSPARENT as LRESULT
                };
            });
            answer
        }
        WM_MOUSEMOVE => {
            with_state(|s| {
                let hover = s.bar.hit(point_of(lp));
                if !s.bar.tracking {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: our own window.
                    s.bar.tracking = unsafe { TrackMouseEvent(&mut track) } != 0;
                }
                if hover != s.bar.hover {
                    s.bar.hover = hover;
                    s.render_bar();
                }
            });
            0
        }
        WM_MOUSELEAVE => {
            with_state(|s| {
                s.bar.tracking = false;
                if s.bar.hover.take().is_some() {
                    s.render_bar();
                }
            });
            0
        }
        WM_LBUTTONDOWN => {
            with_state(|s| s.bar.pressed = s.bar.hit(point_of(lp)));
            0
        }
        WM_LBUTTONUP => {
            with_state(|s| {
                let pressed = s.bar.pressed.take();
                if let Some(item) = s.bar.hit(point_of(lp))
                    && pressed == Some(item)
                {
                    s.act(item);
                }
            });
            0
        }
        WM_MOVE => {
            // Dragged somewhere: it stays there, from the middle of the top.
            with_state(|s| {
                let mut r = RECT::default();
                // SAFETY: a read-only question about our own window.
                unsafe { GetWindowRect(hwnd, &mut r) };
                let w = r.right - r.left;
                let home = s.bar.place(s.rect, w);
                let (dx, dy) = s
                    .bar
                    .offset
                    .unwrap_or((0, (10.0 * s.bar.scale).round() as i32));
                let moved = (r.left - home.x, r.top - home.y);
                if moved != (0, 0) {
                    s.bar.offset = Some((dx + moved.0, dy + moved.1));
                }
            });
            // SAFETY: the default handling of a message of our window.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_SETCURSOR => {
            // SAFETY: a system cursor.
            unsafe { SetCursor(LoadCursorW(null_mut(), IDC_ARROW)) };
            1
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        // SAFETY: the default handling of a message of our window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    with_state(|s| {
        let Target::Window(target) = s.target else {
            return;
        };
        let ours = hwnd as isize == target && object == OBJID_WINDOW;
        match event {
            EVENT_SYSTEM_FOREGROUND => s.refresh_visibility(),
            EVENT_OBJECT_LOCATIONCHANGE if ours => s.moved(),
            EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND if ours => {
                s.refresh_visibility();
            }
            _ => {}
        }
    });
}

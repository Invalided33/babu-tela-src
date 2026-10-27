//! Babu Tela desktop app: a native window drawn with egui, talking to the
//! chat server on Cloudflare (crates/worker). No embedded browser: the
//! interface, the calls and everything around them are Rust.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("O app do Babu Tela é só para Windows.");

/// The argument a copy opened by the app itself gets (Core::restart): the
/// process id of the copy before it, which it waits for.
pub const RESTART_ARG: &str = "--depois-de=";

/// Prints one line to stderr the way Go's log package did.
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {
        eprintln!("{} {}", ::chrono::Local::now().format("%Y/%m/%d %H:%M:%S"), format_args!($($arg)*))
    };
}

mod core;
mod devices;
mod direct;
mod e2e;
mod emoji;
mod emoji_font;
mod gateway;
mod giphy;
mod google;
mod icons;
mod images;
mod ink;
mod markdown;
mod media;
mod model;
mod music;
mod net;
mod notify;
mod pictures;
mod prefs;
mod probe;
mod shell;
mod slides;
mod smoke;
mod sounds;
mod theme;
mod tor;
mod tray;
mod ui;
mod update;
mod voice;
mod winsys;

use std::sync::Arc;

use babutela_buildinfo::NAME;
use eframe::egui;

use crate::core::{Core, Msg, TrayAction, View};
use crate::shell::Shell;

struct App {
    core: Core,
    smoke: Option<smoke::Smoke>,
    /// Starting in the tray. eframe shows the window after its first frame
    /// no matter what, so the app starts minimized and hides itself as soon
    /// as that has happened.
    hide_pending: bool,
    maximize: Maximize,
    /// Measuring: when the last frame was drawn, and its number.
    last_frame: Option<std::time::Instant>,
    last_frame_nr: Option<u64>,
    cap: FrameCap,
}

/// When the window still has to maximize: it opens maximized, except in a
/// test run.
#[derive(Clone, Copy, PartialEq)]
enum Maximize {
    /// In the first frame, which eframe carries out right after it first
    /// shows the window. Maximizing as the window is made
    /// (`with_maximized`) is that first show itself, and Windows replaces
    /// it with what the shortcut asked for: started from a shortcut, the
    /// window came up minimized.
    FirstFrame,
    /// Started in the tray: the first time the window comes out of it.
    FirstShow,
    Done,
}

/// Keeps frames at most as frequent as the screen can show them. The
/// window's vsync does not hold them back here (frames came every 0.7 ms
/// on a 180 Hz screen whenever something repainted continuously), so
/// anything that animates would otherwise spin a whole core.
struct FrameCap {
    last: Option<std::time::Instant>,
    interval: std::time::Duration,
    /// When the screen's rate was last asked (it changes with the screen
    /// the window is on).
    asked: Option<std::time::Instant>,
}

impl FrameCap {
    fn new() -> FrameCap {
        FrameCap {
            last: None,
            interval: std::time::Duration::from_secs_f64(1.0 / 60.0),
            asked: None,
        }
    }

    fn wait(&mut self, hwnd: Option<isize>) {
        use std::time::{Duration, Instant};
        if self
            .asked
            .is_none_or(|at| at.elapsed() > Duration::from_secs(5))
        {
            self.asked = Some(Instant::now());
            let hz = winsys::refresh_rate(hwnd).unwrap_or(60).clamp(30, 500);
            self.interval = Duration::from_secs_f64(1.0 / f64::from(hz));
        }
        if let Some(last) = self.last {
            let since = last.elapsed();
            if since < self.interval {
                std::thread::sleep(self.interval - since);
            }
        }
        self.last = Some(Instant::now());
    }
}

impl eframe::App for App {
    /// Runs before every frame, and also while the window is hidden in the
    /// tray, so messages, notifications and calls carry on.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.cap.wait(self.core.shell.hwnd());
        self.core.pump();
        // Something asked for the window (a click on a notification, the
        // tray) while it was still on its way to the tray: it stays.
        if self.core.raise {
            self.hide_pending = false;
        }
        if self.maximize == Maximize::FirstFrame {
            self.maximize = Maximize::Done;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }
        if self.hide_pending {
            // eframe shows the window after its first frame whatever was
            // asked; minimized first, that is only a moment on the taskbar,
            // and then it goes to the tray.
            let shown = self.core.shell.hwnd().is_some_and(winsys::is_visible);
            if shown {
                self.hide_pending = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        // Here rather than in `ui`, so a test can run with the window hidden.
        if let Some(smoke) = &mut self.smoke {
            smoke.frame(ctx, &mut self.core);
        }
        if std::mem::take(&mut self.core.raise) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            if self.maximize == Maximize::FirstShow {
                self.maximize = Maximize::Done;
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            if let Some(hwnd) = self.core.shell.hwnd() {
                winsys::bring_to_front(hwnd);
            }
        }
        // Closing the window keeps the app in the tray unless that is off or
        // the person chose "Sair", and even then the window only closes
        // once Core has nothing left to wait for (a call hangs up first).
        if ctx.input(|i| i.viewport().close_requested()) && !self.core.can_close() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.core.quitting {
                if self.core.shell.settings().minimize_to_tray && crate::tray::present() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    self.core.quit();
                }
            }
        }
        // Asked on every pass until it happens: a hidden window only gets a
        // pass when one is asked for, and this asks for the next.
        if self.core.can_close() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = std::time::Instant::now();
        ui::draw(ui, &mut self.core);
        self.core.frame_done();
        if probe::on() {
            // How long drawing took, and how far apart the frames are (a
            // frame may take more than one pass).
            probe::since("ui.desenho", started);
            probe::count("ui.passadas", 1);
            if self.last_frame.is_none()
                && let Some(at) = probe::STARTED.get()
            {
                probe::since("app.primeiro_quadro", *at);
            }
            let frame = ui.ctx().cumulative_frame_nr();
            if self.last_frame_nr != Some(frame) {
                self.last_frame_nr = Some(frame);
                if let Some(at) = self.last_frame.replace(started) {
                    probe::since("ui.intervalo", at);
                }
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG0.to_normalized_gamma_f32()
    }

    /// Measuring, the test windows ignore the mouse: someone using the PC
    /// meanwhile must not click through what is being measured. A test's
    /// own pointing and typing (`click:` and the like) goes in here.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let Some(smoke) = &mut self.smoke else { return };
        if probe::on() {
            raw.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::MouseMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::PointerGone
                        | egui::Event::MouseWheel { .. }
                )
            });
        }
        smoke.feed(raw);
    }
}

fn window_icon() -> Option<egui::IconData> {
    let (rgba, w, h) = tray::APP_RGBA.as_ref()?;
    let side = 64;
    Some(egui::IconData {
        rgba: tray::scaled_icon(rgba, *w, *h, side, false),
        width: side as u32,
        height: side as u32,
    })
}

/// The GPU device, measured with `cargo xtask medir`:
///
/// - Direct3D 12 when the PC has a real GPU for it, alone: with every
///   backend on, wgpu loads the Vulkan and OpenGL drivers too, and Vulkan
///   alone kept 40 to 50 MB more of the app in RAM than D3D12 for the same
///   processor time. `WGPU_BACKEND` still decides when set.
/// - wgpu's default memory hint ("Performance") takes memory from the
///   driver in blocks of 64 to 256 MB, which suits a game; egui needs a
///   few MB, and the rest of a block sat committed in the process (180 MB
///   of private memory). `BABUTELA_TESTE_GPU=desempenho` keeps it, to
///   compare.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
    use eframe::wgpu::{Backends, DeviceDescriptor, MemoryHints};
    let asked =
        |name: &str, value: &str| probe::on() && std::env::var_os(name).is_some_and(|v| v == value);
    let hints = if asked("BABUTELA_TESTE_GPU", "desempenho") {
        MemoryHints::Performance
    } else {
        MemoryHints::MemoryUsage
    };
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    if Backends::from_env().is_none() && has_d3d12_gpu() {
        setup.instance_descriptor.backends = Backends::DX12;
    }
    let base = setup.device_descriptor.clone();
    setup.device_descriptor = Arc::new(move |adapter| DeviceDescriptor {
        memory_hints: hints.clone(),
        ..base(adapter)
    });
    // The surface stays at egui's two frames in flight: one (its "low
    // latency" setting) measured the same glass-to-glass (11 to 12 ms) and
    // processor time, since FrameCap already keeps frames from queueing.
    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// Whether Direct3D 12 sees a GPU that is not Windows' software one (old
/// GPUs and drivers have none; wgpu then picks between Vulkan and OpenGL).
fn has_d3d12_gpu() -> bool {
    use eframe::wgpu::{Backends, DeviceType, Instance, InstanceDescriptor};
    use futures_util::FutureExt;
    let started = std::time::Instant::now();
    let instance = Instance::new(InstanceDescriptor {
        backends: Backends::DX12,
        ..InstanceDescriptor::new_without_display_handle()
    });
    // Native enumeration is not really asynchronous: it is ready at once.
    let adapters = instance
        .enumerate_adapters(Backends::DX12)
        .now_or_never()
        .unwrap_or_default();
    let found = adapters
        .iter()
        .any(|a| a.get_info().device_type != DeviceType::Cpu);
    probe::since("gpu.sondagem", started);
    // Kept alive: each adapter holds a D3D12 device, and D3D12 hands the
    // same one to the next request for that adapter (eframe's, in a
    // moment) instead of starting the driver up again.
    std::mem::forget((instance, adapters));
    found
}

fn main() {
    let _ = probe::STARTED.set(std::time::Instant::now());
    net::install_crypto_provider();
    let shell = Arc::new(Shell::new());
    // The browser data the Tauri builds kept in the profile; nothing reads
    // it any more.
    let old_webview = shell.path("webview");
    if old_webview.is_dir() {
        std::thread::spawn(move || {
            let _ = std::fs::remove_dir_all(old_webview);
        });
    }
    // The installer is replacing the app right now (an update, which opens
    // it again when it is done): leave it be.
    if winsys::installer_running() {
        return;
    }
    // Opened again by the app itself (a setting that takes a restart,
    // Core::restart): the copy before it goes first.
    if let Some(pid) =
        std::env::args().find_map(|a| a.strip_prefix(RESTART_ARG).and_then(|p| p.parse().ok()))
    {
        winsys::wait_for_exit(pid, std::time::Duration::from_secs(20));
    }
    let instance = match winsys::single_instance(&shell.instance_id()) {
        winsys::Start::First(instance) => instance,
        // The copy already running was asked to show itself.
        winsys::Start::Second => return,
    };
    // A smoke test shows the window unless it is testing the hidden app.
    // Started by Windows for a click on a notification, the app is there to
    // show that conversation, so it never starts in the tray then.
    let hidden = !notify::launched_by_toast()
        && match smoke::Smoke::from_args() {
            Some(_) => std::env::var_os("PROSA_SMOKE_HIDDEN").is_some(),
            None => shell.start_hidden(),
        };
    // An update downloaded before is installed before anything opens, the
    // way Discord's splash screen does it (update.rs): the installer waits
    // for this process to end, shows its progress (unless the app was
    // starting in the tray) and opens the app again.
    let updates = update::dir(&shell);
    update::cleanup(&updates);
    if update::automatic()
        && let Some(p) = update::pending(&updates)
    {
        match update::hold_back(&p, &shell.server_url()) {
            Some(why) => logln!("atualização: a {} fica para depois: {why}", p.version),
            None if update::apply(&updates, p, !hidden) => return,
            None => {}
        }
    }
    // One worker per core, tokio's default: two workers measured the same
    // processor time in a call.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("babutela")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            winsys::alert(&format!("O Babu Tela não conseguiu iniciar.\n\n{e}"));
            std::process::exit(1);
        }
    };
    // Through Tor (Configurações → Privacidade): on before anything talks
    // to the server, since the HTTP clients are made with it.
    if prefs::Prefs::load(&shell.path("prefs.json")).tor {
        let _inside = rt.enter();
        if let Err(e) = tor::start(&shell.path("tor")) {
            logln!("tor: {e}");
            // Never without it unless the person says so: they turned it
            // on to keep their IP from the server.
            let without = winsys::ask(&format!(
                "O Babu Tela não conseguiu ligar o Tor.\n\n{e}\n\n\
                 Abrir sem o Tor? O servidor vai ver o seu IP. (\"Não\" fecha o app.)"
            ));
            if !without {
                std::process::exit(1);
            }
            let path = shell.path("prefs.json");
            let mut p = prefs::Prefs::load(&path);
            p.tor = false;
            p.save(&path);
        }
    }
    // The window opens maximized (App::maximize); the size below is
    // what it goes back to when un-maximized. A test run keeps that fixed
    // size, so its pictures and measurements stay comparable from one PC to
    // another.
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(NAME)
        .with_app_id("com.prosa.app")
        .with_inner_size([1240.0, 780.0])
        .with_min_inner_size([760.0, 480.0])
        .with_decorations(false)
        .with_drag_and_drop(true)
        .with_visible(!hidden);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    // Every call to the server runs on `rt`, which lives for as long as the
    // window and a little after it (see the end).
    let handle = rt.handle().clone();
    let result = eframe::run_native(
        NAME,
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = cc.window_handle()
                && let RawWindowHandle::Win32(w) = h.as_raw()
            {
                shell.set_hwnd(w.hwnd.get());
            }
            // My screen share's preview is drawn straight from the card
            // when egui draws on the same one (voice/interop.rs).
            voice::interop::install(cc.wgpu_render_state.as_ref());
            let (tx, rx) = std::sync::mpsc::channel::<Msg>();
            let ctx = cc.egui_ctx.clone();
            if std::env::var_os("PROSA_NO_TRAY").is_none() {
                let (tx, ctx) = (tx.clone(), ctx.clone());
                if let Err(e) = tray::start(move |action| {
                    let _ = tx.send(Msg::Tray(action));
                    ctx.request_repaint();
                }) {
                    logln!("bandeja: {e}");
                }
            }
            {
                // Another copy started: come forward. The installer is about
                // to replace the app: close, the way "Sair" does.
                let send = |action: TrayAction| {
                    let (tx, ctx) = (tx.clone(), ctx.clone());
                    move || {
                        let _ = tx.send(Msg::Tray(action));
                        ctx.request_repaint();
                    }
                };
                instance.listen(send(TrayAction::Open), send(TrayAction::Quit));
            }
            {
                // A click on one of the app's notifications, handed over by
                // Windows (also when it started the app for it): open the
                // conversation it is about, or at least the window.
                let (tx, ctx) = (tx.clone(), ctx.clone());
                notify::start(&shell, move |launch| {
                    let msg = match View::from_launch(&launch) {
                        Some(view) => Msg::Navigate(view),
                        None => Msg::Tray(TrayAction::Open),
                    };
                    let _ = tx.send(msg);
                    ctx.request_repaint();
                });
            }
            // Tor's way into the network shows while the app waits for it.
            if let Some(mut changes) = tor::changes() {
                let ctx = ctx.clone();
                handle.spawn(async move {
                    while changes.changed().await.is_ok() {
                        ctx.request_repaint();
                    }
                });
            }
            let mut core = Core::new(ctx, handle, shell, tx, rx);
            core.started_hidden = hidden;
            Ok(Box::new(App {
                core,
                smoke: smoke::Smoke::from_args(),
                hide_pending: hidden,
                maximize: match (smoke::active(), hidden) {
                    (true, _) => Maximize::Done,
                    (false, true) => Maximize::FirstShow,
                    (false, false) => Maximize::FirstFrame,
                },
                last_frame: None,
                last_frame_nr: None,
                cap: FrameCap::new(),
            }))
        }),
    );
    // The window is gone (Core::quit let a call hang up first). What the
    // runtime still runs stops at its next pause; work on its blocking
    // threads (a sound device closing, say) gets a moment, and then the
    // app ends regardless.
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    if let Err(e) = result {
        // With windows_subsystem = "windows" there is no console, so
        // without this the app would just not open.
        logln!("{e}");
        winsys::alert(&format!(
            "O Babu Tela não conseguiu abrir a janela.\n\n{e}\n\n\
             Se o problema continuar, atualize o driver de vídeo e tente de novo."
        ));
        std::process::exit(1);
    }
}

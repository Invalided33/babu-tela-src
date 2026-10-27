//! The interface, drawn with egui every frame from what `Core` knows. Each
//! region of the old page has its own file; this one lays them out and
//! draws what floats above them (the title bar, the banner, the toasts).
//!
//! Drawing only reads the core. A click records what it should do in `Acts`,
//! and those run after the frame is drawn, so nothing changes half-way
//! through a frame.

pub mod auth;
pub mod chat;
mod direct;
pub mod friends;
mod ink;
pub mod members;
pub mod modals;
pub mod player;
pub mod security;
pub mod sidebar;
mod slides;
pub mod stage;
pub mod widgets;

use egui::{
    Align, Align2, Area, CentralPanel, Color32, CornerRadius, CursorIcon, Frame, Id, Key, Layout,
    Margin, Order, Panel, Rect, ResizeDirection, Sense, Stroke, TextureHandle, TextureOptions, Ui,
    Vec2, ViewportCommand, pos2, vec2,
};

use crate::core::{Core, Gate, Phase};
use crate::icons::{self, Icon};
use crate::theme::{self, *};

type Act = Box<dyn FnOnce(&mut Core)>;

/// What the clicks of this frame asked for, run once it is drawn.
#[derive(Default)]
pub struct Acts(Vec<Act>);

impl Acts {
    pub fn later(&mut self, f: impl FnOnce(&mut Core) + 'static) {
        self.0.push(Box::new(f));
    }

    fn run(self, core: &mut Core) {
        for act in self.0 {
            act(core);
        }
    }
}

pub fn draw(ui: &mut Ui, core: &mut Core) {
    let ctx = ui.ctx().clone();
    let mut acts = Acts::default();
    let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    if !fullscreen {
        titlebar(ui, core, &mut acts);
    }
    banner(ui, core, &mut acts);
    match core.phase {
        // A newer version coming down as the app starts.
        _ if matches!(core.gate, Gate::Updating { .. }) => auth::updating(ui, core, &mut acts),
        Phase::Waiting { .. } | Phase::Connecting { .. } => {
            core.waiting_tick();
            auth::waiting(ui, core, &mut acts);
        }
        Phase::Auth => auth::sign_in(ui, core, &mut acts),
        Phase::Ready => app(ui, core, &mut acts),
    }
    modals::draw(&ctx, core, &mut acts);
    modals::pop(&ctx, core, &mut acts);
    toasts(&ctx, core);
    resize_edges(&ctx);
    keys(&ctx, core, &mut acts);
    let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|f| f.path().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .collect()
    });
    if !dropped.is_empty() && core.phase == Phase::Ready {
        acts.later(move |c| c.add_files(dropped));
    }
    let own_window = core.shell.hwnd().map(crate::voice::sources::Source::Window);
    let screen = matches!(
        core.voice.source(),
        Some(crate::voice::sources::Source::Monitor(_))
    ) && crate::probe::STAMP_SCREEN.load(std::sync::atomic::Ordering::Relaxed);
    if crate::probe::on() && ((own_window.is_some() && core.voice.source() == own_window) || screen)
    {
        time_stamp(&ctx);
    }
    acts.run(core);
}

/// Measuring while sharing: the time across the top of the window, drawn
/// again every frame, so whoever receives the picture can tell how old it
/// is (see `probe::stamp`).
fn time_stamp(ctx: &egui::Context) {
    use crate::probe::stamp;
    let rect = ctx.content_rect();
    let band = (rect.height() * stamp::BAND).max(20.0);
    let w = rect.width() / stamp::CELLS as f32;
    let painter = ctx.layer_painter(egui::LayerId::new(
        Order::Debug,
        Id::unique("medir-carimbo"),
    ));
    for (i, white) in stamp::cells(crate::probe::wall_ms()).iter().enumerate() {
        let cell = Rect::from_min_size(
            pos2(rect.left() + i as f32 * w, rect.top()),
            vec2(w + 1.0, band),
        );
        let color = if *white {
            Color32::WHITE
        } else {
            Color32::BLACK
        };
        painter.rect_filled(cell, CornerRadius::ZERO, color);
    }
    ctx.request_repaint();
}

/// The app itself: servers, the list, the conversation and the members.
fn app(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
    // In fullscreen, only the call view is shown: no rail, no sidebar, no
    // members panel, no titlebar. The call's buttons float over it
    // (stage.rs `fullscreen_bar`).
    if !fullscreen {
        let width = ui.ctx().content_rect().width();
        Panel::left("rail")
            .exact_size(RAIL_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(BG0)
                    .inner_margin(Margin::symmetric(0, 10)),
            )
            .show(ui, |ui| sidebar::rail(ui, core, acts));
        Panel::left("sidebar")
            .exact_size(SIDEBAR_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(BG1).corner_radius(CornerRadius {
                nw: 12,
                ne: 0,
                sw: 0,
                se: 0,
            }))
            .show(ui, |ui| sidebar::sidebar(ui, core, acts));
        let members = core.current_server().is_some() && core.show_members && width > NO_MEMBERS;
        if members {
            Panel::right("members")
                .exact_size(MEMBERS_W)
                .resizable(false)
                .show_separator_line(false)
                .frame(Frame::new().fill(BG1).inner_margin(Margin::same(8)))
                .show(ui, |ui| members::draw(ui, core, acts));
        }
    }
    CentralPanel::default()
        .frame(Frame::new().fill(BG2))
        .show(ui, |ui| {
            if core.view.is_home() && core.current_dm().is_none() {
                friends::draw(ui, core, acts);
            } else if let Some((_, c)) = core.current_channel() {
                if c.is_voice() {
                    stage::draw(ui, core, acts);
                } else {
                    chat::draw(ui, core, acts);
                }
            } else if core.current_dm().is_some() {
                chat::draw(ui, core, acts);
            } else {
                empty(
                    ui,
                    "Nada por aqui",
                    "Este servidor não tem canais que você possa ver.",
                );
            }
        });
}

/// A centred message for an area with nothing to show (`.empty`).
pub fn empty(ui: &mut Ui, title: &str, text: &str) {
    ui.with_layout(
        Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 30.0).max(0.0));
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::bold(17.0))
                        .color(TEXT),
                );
                ui.add_space(6.0);
                ui.label(egui::RichText::new(text).color(MUTED));
            });
        },
    );
}

/// The bar across the top of every page (`.head`): 50 px with a line under.
pub fn header(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui)) {
    Panel::top(Id::unique(id))
        .exact_size(HEAD_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().inner_margin(Margin::symmetric(16, 0)))
        .show(ui, |ui| {
            let line_y = ui.max_rect().bottom();
            ui.painter().hline(
                ui.max_rect().x_range().expand(16.0),
                line_y - 0.5,
                Stroke::new(1.0, BG0),
            );
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                add(ui);
            });
        });
}

// ---------------------------------------------------------------- the window

/// The app icon as a texture, for the title bar and the sign-in card.
pub fn app_icon(ui: &Ui) -> Option<TextureHandle> {
    let id = Id::unique("babutela-app-icon");
    if let Some(t) = ui.ctx().data(|d| d.get_temp::<TextureHandle>(id)) {
        return Some(t);
    }
    let (rgba, w, h) = crate::tray::APP_RGBA.as_ref()?;
    let img = egui::ColorImage::from_rgba_unmultiplied([*w, *h], rgba);
    let t = ui
        .ctx()
        .load_texture("app-icon", img, TextureOptions::LINEAR);
    ui.ctx().data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

fn titlebar(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    Panel::top("titlebar")
        .exact_size(TITLEBAR_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().fill(BG0))
        .show(ui, |ui| {
            let full = ui.max_rect();
            let buttons = 3.0 * 46.0;
            let drag = Rect::from_min_max(full.min, pos2(full.right() - buttons, full.bottom()));
            let resp = ui.interact(drag, Id::unique("titlebar-drag"), Sense::click_and_drag());
            let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            if resp.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            } else if resp.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            // The brand at the left.
            let mut x = full.left() + 12.0;
            if let Some(icon) = app_icon(ui) {
                let r = Rect::from_min_size(pos2(x, full.center().y - 8.0), Vec2::splat(16.0));
                egui::Image::from_texture(egui::load::SizedTexture::from_handle(&icon))
                    .paint_at(ui, r);
                x += 24.0;
            }
            ui.painter().text(
                pos2(x, full.center().y),
                Align2::LEFT_CENTER,
                babutela_buildinfo::NAME,
                theme::bold(12.0),
                MUTED,
            );
            // The three window buttons at the right.
            let mut left = full.right() - buttons;
            let items = [
                (Icon::WinMin, false),
                (Icon::WinMax, false),
                (Icon::WinClose, true),
            ];
            for (i, (icon, close)) in items.into_iter().enumerate() {
                let r = Rect::from_min_size(pos2(left, full.top()), vec2(46.0, TITLEBAR_H));
                left += 46.0;
                let resp = ui.interact(r, Id::unique(("win", i)), Sense::click());
                let (bg, fg) = match (resp.hovered(), close) {
                    (true, true) => (DANGER, Color32::WHITE),
                    (true, false) => (BG3, TEXT),
                    _ => (BG0, MUTED),
                };
                ui.painter().rect_filled(r, CornerRadius::ZERO, bg);
                let ir = Rect::from_center_size(r.center(), Vec2::splat(icons::WINDOW));
                icon.image(icons::WINDOW, fg).paint_at(ui, ir);
                if resp.clicked() {
                    match i {
                        0 => ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true)),
                        1 => ui
                            .ctx()
                            .send_viewport_cmd(ViewportCommand::Maximized(!maximized)),
                        _ => {
                            let ctx = ui.ctx().clone();
                            acts.later(move |c| close_window(&ctx, c));
                        }
                    }
                }
            }
            let _ = core;
        });
}

/// The close button: back to the tray when that is on, otherwise quit.
pub fn close_window(ctx: &egui::Context, core: &mut Core) {
    if core.shell.settings().minimize_to_tray && crate::tray::present() {
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    } else {
        core.quit();
    }
}

/// Lets the borderless window be resized from its edges, which Windows
/// does by itself only for windows with a frame.
fn resize_edges(ctx: &egui::Context) {
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if maximized {
        return;
    }
    let r = ctx.content_rect();
    let t = 5.0;
    let c = 10.0;
    let zones = [
        (
            Rect::from_min_max(r.min, pos2(r.left() + c, r.top() + c)),
            ResizeDirection::NorthWest,
            CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_max(pos2(r.right() - c, r.top()), pos2(r.right(), r.top() + c)),
            ResizeDirection::NorthEast,
            CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_max(
                pos2(r.left(), r.bottom() - c),
                pos2(r.left() + c, r.bottom()),
            ),
            ResizeDirection::SouthWest,
            CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_max(pos2(r.right() - c, r.bottom() - c), r.max),
            ResizeDirection::SouthEast,
            CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_max(
                pos2(r.left() + c, r.top()),
                pos2(r.right() - c, r.top() + t),
            ),
            ResizeDirection::North,
            CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(
                pos2(r.left() + c, r.bottom() - t),
                pos2(r.right() - c, r.bottom()),
            ),
            ResizeDirection::South,
            CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(
                pos2(r.left(), r.top() + c),
                pos2(r.left() + t, r.bottom() - c),
            ),
            ResizeDirection::West,
            CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_max(
                pos2(r.right() - t, r.top() + c),
                pos2(r.right(), r.bottom() - c),
            ),
            ResizeDirection::East,
            CursorIcon::ResizeHorizontal,
        ),
    ];
    for (i, (zone, dir, cursor)) in zones.into_iter().enumerate() {
        Area::new(Id::unique(("resize", i)))
            .order(Order::Foreground)
            .fixed_pos(zone.min)
            .show(ctx, |ui| {
                let (_, resp) = ui.allocate_exact_size(zone.size(), Sense::drag());
                let resp = resp.on_hover_cursor(cursor);
                if resp.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
                }
            });
    }
}

// ---------------------------------------------------------------- notices

/// The strip under the title bar: the connection dropped, or an update is
/// ready to install.
fn banner(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let text = if core.reconnecting {
        Some(("Conexão perdida. Reconectando…".to_owned(), false))
    } else {
        // Downloaded while the app ran (update.rs): the next start
        // installs it, as Discord's green arrow says.
        core.update_ready.as_ref().map(|v| {
            (
                format!("Atualização {v} pronta: ela entra quando o Babu Tela abrir de novo."),
                true,
            )
        })
    };
    let Some((text, update)) = text else { return };
    const RESTART: &str = "Reiniciar agora";
    Panel::top("banner")
        .resizable(false)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(WARN)
                .inner_margin(Margin::symmetric(12, 6)),
        )
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    // Centre the row by hand: egui centres single widgets only.
                    let measure = |s: &str| {
                        ui.painter()
                            .layout_no_wrap(s.to_owned(), theme::bold(13.0), WARN_INK)
                            .size()
                            .x
                    };
                    let mut width = measure(&text);
                    if update {
                        width += ui.spacing().item_spacing.x + measure(RESTART);
                    }
                    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                    ui.label(
                        egui::RichText::new(text)
                            .font(theme::bold(13.0))
                            .color(WARN_INK),
                    );
                    if update {
                        let r = ui.add(
                            egui::Label::new(
                                egui::RichText::new(RESTART)
                                    .font(theme::bold(13.0))
                                    .color(WARN_INK)
                                    .underline(),
                            )
                            .sense(Sense::click()),
                        );
                        if r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            acts.later(Core::install_update);
                        }
                    }
                });
            });
        });
}

fn toasts(ctx: &egui::Context, core: &Core) {
    if core.toasts.is_empty() {
        return;
    }
    let r = ctx.content_rect();
    Area::new(Id::unique("toasts"))
        .order(Order::Tooltip)
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -24.0))
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_max_width((r.width() * 0.8).min(560.0));
            for t in &core.toasts {
                Frame::new()
                    .fill(BG0)
                    .stroke(Stroke::new(1.0, BG4))
                    .corner_radius(CornerRadius::same(RADIUS_MD))
                    .inner_margin(Margin::symmetric(16, 10))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 30,
                        spread: 0,
                        color: Color32::from_black_alpha(0xa0),
                    })
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&t.text).color(TEXT));
                    });
                ui.add_space(8.0);
            }
        });
}

/// Escape closes what is open, innermost first, as on the old page.
/// In fullscreen it exits fullscreen first.
fn keys(ctx: &egui::Context, core: &mut Core, acts: &mut Acts) {
    if !ctx.input(|i| i.key_pressed(Key::Escape)) {
        return;
    }
    let fullscreen = stage::is_fullscreen(ctx);
    if core.voice.ink().active().is_some() {
        // Annotating: Escape stops it first (ui/ink.rs).
        core.voice.ink().close();
    } else if fullscreen {
        stage::set_fullscreen(ctx, false);
    } else if core.pop.is_some() {
        acts.later(|c| c.pop = None);
    } else if core.editing.is_some() {
        acts.later(|c| c.editing = None);
    } else if core.modal.is_some() {
        acts.later(Core::close_modal);
    } else if core.reply.is_some() {
        acts.later(|c| c.reply = None);
    }
}

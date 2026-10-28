//! The left side: the rail of servers, the list of conversations or
//! channels, the call in progress and the signed-in person.

use egui::{
    Align, Align2, CentralPanel, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin,
    Panel, Rect, Response, ScrollArea, Sense, Stroke, Ui, Vec2, pos2, vec2,
};

use super::Acts;
use super::widgets::{self, Ib};
use crate::core::{Core, Modal, Pop, PopKind, View};
use crate::icons::{self, Icon};
use crate::model::*;
use crate::sounds::Cue;
use crate::t;
use crate::theme::{self, *};

// ---------------------------------------------------------------- the rail

/// One round-cornered square in the rail (`.srv`).
fn rail_button(ui: &mut Ui, id: Id, on: bool, unread: bool, tip: &str) -> (Rect, Response) {
    let full = ui.available_width();
    let (row, _) = ui.allocate_exact_size(vec2(full, 46.0), Sense::hover());
    let rect = Rect::from_center_size(row.center(), Vec2::splat(46.0));
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let resp = if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    };
    // The white bar at the left edge: tall for the open one, a dot for
    // unread ones.
    if on || unread || resp.hovered() {
        let h = if on {
            28.0
        } else if resp.hovered() {
            18.0
        } else {
            8.0
        };
        let bar = Rect::from_min_size(pos2(row.left(), rect.center().y - h / 2.0), vec2(4.0, h));
        ui.painter().rect_filled(
            bar,
            CornerRadius {
                nw: 0,
                sw: 0,
                ne: 4,
                se: 4,
            },
            TEXT,
        );
    }
    (rect, resp)
}

pub fn rail(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    ScrollArea::vertical()
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            // Direct messages.
            let dm_hot: u32 = core
                .dms
                .iter()
                .map(|d| core.unread.get(&d.id).map_or(0, |u| u.hot))
                .sum();
            let pending = core.pending_friends() as u32;
            let home = core.view.is_home();
            let (rect, resp) =
                rail_button(ui, Id::unique("rail-home"), home, false, t!("rail.dms"));
            let lit = home || resp.hovered();
            paint_square(ui, rect, lit, if lit { ACCENT } else { BG3 });
            Icon::Chat
                .image(icons::SIZE, if lit { ACCENT_INK } else { TEXT })
                .paint_at(
                    ui,
                    Rect::from_center_size(rect.center(), Vec2::splat(icons::SIZE)),
                );
            if dm_hot + pending > 0 {
                widgets::badge_at(
                    ui,
                    rect.right_bottom() - vec2(5.0, 5.0),
                    dm_hot + pending,
                    Some(BG0),
                );
            }
            if resp.clicked() {
                acts.later(|c| c.go_to(View::HOME));
            }
            // The line between.
            let (sep, _) = ui.allocate_exact_size(vec2(ui.available_width(), 2.0), Sense::hover());
            ui.painter().rect_filled(
                Rect::from_center_size(sep.center(), vec2(30.0, 2.0)),
                CornerRadius::same(1),
                BG3,
            );
            // The servers.
            for s in &core.servers {
                let (hot, any) = core.server_unread(s);
                let on = core.view.server() == Some(s.id);
                let (rect, resp) =
                    rail_button(ui, Id::unique(("rail-srv", s.id)), on, any && !on, &s.name);
                crate::smoke::target(|| format!("servidor:{}", s.name), rect);
                let lit = on || resp.hovered();
                let color = theme::color_or(&s.color, ACCENT);
                widgets::paint_server(
                    ui,
                    core,
                    s.icon.as_deref(),
                    &s.initials(),
                    rect,
                    square_radius(lit),
                    if lit { color } else { BG3 },
                    if lit { ACCENT_INK } else { TEXT },
                );
                if hot > 0 {
                    widgets::badge_at(ui, rect.right_bottom() - vec2(5.0, 5.0), hot, Some(BG0));
                }
                if resp.clicked() {
                    let id = s.id;
                    acts.later(move |c| {
                        c.go_to(View::Server {
                            server: id,
                            channel: None,
                        })
                    });
                }
            }
            // Create or join.
            let (rect, resp) = rail_button(
                ui,
                Id::unique("rail-add"),
                false,
                false,
                t!("rail.add_server"),
            );
            if resp.hovered() {
                widgets::paint_gradient(ui, rect, CornerRadius::same(12), 1.0);
            } else {
                ui.painter().rect_filled(rect, CornerRadius::same(16), BG2);
            }
            Icon::Plus
                .image(
                    icons::SIZE,
                    if resp.hovered() { ACCENT_INK } else { ACCENT },
                )
                .paint_at(
                    ui,
                    Rect::from_center_size(rect.center(), Vec2::splat(icons::SIZE)),
                );
            if resp.clicked() {
                acts.later(|c| {
                    c.open_modal(Modal::AddServer {
                        name: String::new(),
                        code: String::new(),
                    })
                });
            }
        });
}

/// The squares are rounder at rest, less round when active.
fn square_radius(lit: bool) -> u8 {
    if lit { 12 } else { 16 }
}

/// The square itself.
fn paint_square(ui: &Ui, rect: Rect, lit: bool, fill: Color32) {
    ui.painter()
        .rect_filled(rect, CornerRadius::same(square_radius(lit)), fill);
}

// ---------------------------------------------------------------- the list

/// A row of the list (`.row`): returns its rect and response; the caller
/// paints what goes inside.
fn row(ui: &mut Ui, id: Id, on: bool, height: f32) -> (Rect, Response) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let radius = CornerRadius::same(RADIUS_SM);
    if on {
        ui.painter().rect_filled(rect, radius, BG3);
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, vec2(2.0, rect.height())),
            CornerRadius {
                nw: RADIUS_SM,
                sw: RADIUS_SM,
                ne: 0,
                se: 0,
            },
            ACCENT,
        );
    } else if ui.rect_contains_pointer(rect) {
        // Lit under a button of the row too (a channel's gear), which takes
        // the hover from it.
        ui.painter().rect_filled(rect, radius, BG2);
    }
    (rect, resp)
}

/// Text inside a row, cut to fit.
fn row_text(ui: &Ui, rect: Rect, x: f32, right: f32, text: &str, color: Color32, bold: bool) {
    let font = if bold {
        theme::bold(14.0)
    } else {
        theme::body()
    };
    let width = (right - x).max(10.0);
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    let galley = ui.painter().layout_job(job);
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(pos2(x, y), galley, color);
}

pub fn sidebar(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    // Bottom first: panels claim their space before the list fills the rest.
    Panel::bottom("user-panel")
        .resizable(false)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(BG0)
                .inner_margin(Margin::symmetric(12, 10)),
        )
        .show(ui, |ui| user_panel(ui, core, acts));
    if core.voice_location().is_some() {
        Panel::bottom("voice-panel")
            .resizable(false)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(BG2)
                    .inner_margin(Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| voice_panel(ui, core, acts));
    }
    Panel::top("side-head")
        .exact_size(HEAD_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new())
        .show(ui, |ui| side_head(ui, core, acts));
    CentralPanel::default()
        .frame(Frame::new().inner_margin(Margin::same(8)))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if core.current_server().is_some() {
                        channels(ui, core, acts);
                    } else {
                        conversations(ui, core, acts);
                    }
                });
        });
}

fn side_head(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let full = ui.max_rect();
    ui.painter()
        .hline(full.x_range(), full.bottom() - 0.5, Stroke::new(1.0, BG0));
    let Some(s) = core.current_server() else {
        ui.painter().text(
            pos2(full.left() + 14.0, full.center().y),
            Align2::LEFT_CENTER,
            t!("rail.dms"),
            theme::bold(15.0),
            TEXT,
        );
        return;
    };
    let resp = ui
        .interact(full, Id::unique("server-menu"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    crate::smoke::target(|| "cabecalho".into(), full);
    if resp.hovered() {
        ui.painter().rect_filled(full, CornerRadius::ZERO, BG2);
    }
    // What waits on the moderation: reports, and people asking to come in.
    let reports = if s.can(P_MANAGE_MESSAGES) {
        s.reports.len() as u32
    } else {
        0
    } + s.requests.len() as u32;
    let mut right = full.right() - 14.0;
    Icon::Down.image(icons::SIZE, MUTED).paint_at(
        ui,
        Rect::from_center_size(pos2(right - 9.0, full.center().y), Vec2::splat(icons::SIZE)),
    );
    right -= 26.0;
    if reports > 0 {
        widgets::badge_at(ui, pos2(right - 10.0, full.center().y), reports, None);
        right -= 28.0;
    }
    row_text(ui, full, full.left() + 14.0, right, &s.name, TEXT, true);
    if resp.clicked() {
        let anchor = full;
        acts.later(move |c| {
            c.pop = match c.pop {
                Some(Pop {
                    kind: PopKind::ServerMenu,
                    ..
                }) => None,
                _ => Some(Pop {
                    kind: PopKind::ServerMenu,
                    anchor,
                }),
            };
        });
    }
}

/// The home list: friends and direct conversations.
fn conversations(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let pending = core.pending_friends() as u32;
    let on = core.view == View::HOME;
    let (rect, resp) = row(ui, Id::unique("row-friends"), on, 34.0);
    let color = if on || resp.hovered() { TEXT } else { MUTED };
    Icon::Users.image(icons::SIZE, color).paint_at(
        ui,
        Rect::from_center_size(
            pos2(rect.left() + 17.0, rect.center().y),
            Vec2::splat(icons::SIZE),
        ),
    );
    let mut right = rect.right() - 8.0;
    if pending > 0 {
        widgets::badge_at(ui, pos2(right - 10.0, rect.center().y), pending, None);
        right -= 26.0;
    }
    row_text(
        ui,
        rect,
        rect.left() + 34.0,
        right,
        t!("nav.friends"),
        color,
        false,
    );
    if resp.clicked() {
        acts.later(|c| c.go_to(View::HOME));
    }
    ui.add_space(12.0);
    widgets::caption(ui, t!("nav.conversations"));
    ui.add_space(4.0);
    if core.dms.is_empty() {
        ui.label(
            egui::RichText::new(t!("nav.no_conversations"))
                .color(FAINT)
                .size(12.0),
        );
    }
    for d in &core.dms {
        let on = core.view
            == (View::Home {
                channel: Some(d.id),
            });
        let unread = core.unread.get(&d.id).copied();
        let (rect, resp) = row(ui, Id::unique(("row-dm", d.id)), on, 40.0);
        let lit = on || resp.hovered() || unread.is_some();
        let av =
            Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(28.0));
        widgets::paint_avatar(ui, core, &d.user, av, true, if on { BG3 } else { BG1 });
        let mut right = rect.right() - 8.0;
        if let Some(u) = unread {
            widgets::badge_at(ui, pos2(right - 10.0, rect.center().y), u.n, None);
            right -= 26.0;
        }
        row_text(
            ui,
            rect,
            rect.left() + 44.0,
            right,
            d.user.name(),
            if lit { TEXT } else { MUTED },
            unread.is_some(),
        );
        if resp.clicked() {
            let id = d.id;
            acts.later(move |c| c.go_to(View::Home { channel: Some(id) }));
        }
    }
}

/// A server's channels, grouped by category in the order they come.
fn channels(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Some(s) = core.current_server() else {
        return;
    };
    let mut cats: Vec<(&str, Vec<&Channel>)> = Vec::new();
    for c in &s.channels {
        let k = c.category_name();
        match cats.iter_mut().find(|(name, _)| *name == k) {
            Some((_, list)) => list.push(c),
            None => cats.push((k, vec![c])),
        }
    }
    if cats.is_empty() {
        widgets::caption(ui, t!("nav.no_channels"));
    }
    let manage = s.can(P_MANAGE_CHANNELS);
    let sid = s.id;
    for (cat, list) in cats {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            widgets::caption(ui, cat);
            if manage {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // A channel of this category's type, right here (the
                    // server menu's "Criar canal" asks for everything).
                    let kind = list[0].kind;
                    let mixed = super::modals::category_mixes_kinds(s, cat);
                    let r = widgets::icon_button_sized(
                        ui,
                        Icon::Plus,
                        18.0,
                        14.0,
                        Ib::Bare,
                        super::modals::quick_title(mixed, kind),
                    );
                    crate::smoke::target(|| format!("mais:{cat}"), r.rect);
                    if r.clicked() {
                        let category = cat.to_owned();
                        acts.later(move |c| {
                            c.open_modal(Modal::NewChannel {
                                sid,
                                kind,
                                name: String::new(),
                                category,
                                quick: true,
                            })
                        });
                    }
                });
            }
        });
        ui.add_space(2.0);
        for c in list {
            channel_row(ui, core, s, c, manage, acts);
        }
    }
}

fn channel_row(ui: &mut Ui, core: &Core, s: &Server, c: &Channel, manage: bool, acts: &mut Acts) {
    let on = core.view.channel() == Some(c.id);
    let unread = core.unread.get(&c.id).copied();
    let (rect, resp) = row(ui, Id::unique(("row-ch", c.id)), on, 32.0);
    // Not `resp.hovered()`: over the gear the hover is the gear's, and a
    // gear shown only while the row is hovered went away under the pointer
    // (every other frame), so its click was lost.
    let inside = ui.rect_contains_pointer(rect);
    crate::smoke::target(|| format!("canal:{}", c.label()), rect);
    let lit = on || inside || unread.is_some();
    let color = if lit { TEXT } else { MUTED };
    Icon::for_channel(c.kind)
        .image(icons::SIZE, color)
        .paint_at(
            ui,
            Rect::from_center_size(
                pos2(rect.left() + 17.0, rect.center().y),
                Vec2::splat(icons::SIZE),
            ),
        );
    let mut right = rect.right() - 8.0;
    let mut gear_clicked = false;
    if manage && inside {
        let gear = Rect::from_center_size(pos2(right - 9.0, rect.center().y), Vec2::splat(18.0));
        let g = ui
            .interact(gear, Id::unique(("gear", c.id)), Sense::click())
            .on_hover_text(t!("channel.edit"));
        crate::smoke::target(|| format!("engrenagem:{}", c.label()), gear);
        Icon::Gear
            .image(16.0, if g.hovered() { TEXT } else { MUTED })
            .paint_at(ui, gear.shrink(1.0));
        if g.clicked() {
            gear_clicked = true;
            let cid = c.id;
            acts.later(move |core| core.open_edit_channel(cid));
        }
        right -= 24.0;
    }
    if let Some(u) = unread
        && u.hot > 0
    {
        widgets::badge_at(ui, pos2(right - 10.0, rect.center().y), u.hot, None);
        right -= 26.0;
    }
    row_text(
        ui,
        rect,
        rect.left() + 34.0,
        right,
        c.label(),
        color,
        unread.is_some(),
    );
    // Clicking the gear must not also open the channel.
    if resp.clicked() && !gear_clicked {
        let (sid, cid) = (s.id, c.id);
        acts.later(move |core| core.open_channel(sid, cid));
    }
    // Who is in a voice channel, under it. A right click on someone: how
    // loud they play here.
    if c.is_voice() {
        for v in s.in_voice(c.id) {
            let u = core.user(v.user_id);
            let (r, row) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
            if v.user_id != core.me_id() && row.secondary_clicked() {
                let at = row.interact_pointer_pos().unwrap_or(r.center());
                super::stage::open_person(acts, v.user_id, at);
            }
            let av = Rect::from_center_size(
                pos2(r.left() + 30.0 + 11.0, r.center().y),
                Vec2::splat(22.0),
            );
            widgets::paint_avatar(ui, core, &u, av, false, BG1);
            let mut right = r.right() - 6.0;
            if core.voice.gains(v.user_id).voice == 0.0 {
                Icon::VolumeOff.image(icons::SMALL, DANGER).paint_at(
                    ui,
                    Rect::from_center_size(
                        pos2(right - 7.0, r.center().y),
                        Vec2::splat(icons::SMALL),
                    ),
                );
                right -= 20.0;
            }
            let off = if v.deaf {
                Some(Icon::HeadOff)
            } else if v.mute {
                Some(Icon::MicOff)
            } else {
                None
            };
            if let Some(icon) = off {
                icon.image(icons::SMALL, DANGER).paint_at(
                    ui,
                    Rect::from_center_size(
                        pos2(right - 7.0, r.center().y),
                        Vec2::splat(icons::SMALL),
                    ),
                );
                right -= 20.0;
            }
            if let Some(kind) = v.video {
                let icon = if kind == Video::Screen {
                    Icon::Screen
                } else {
                    Icon::Video
                };
                icon.image(icons::SMALL, MUTED).paint_at(
                    ui,
                    Rect::from_center_size(
                        pos2(right - 7.0, r.center().y),
                        Vec2::splat(icons::SMALL),
                    ),
                );
                right -= 20.0;
            }
            let mut job = egui::text::LayoutJob::simple_singleline(
                u.name().to_owned(),
                theme::small(),
                MUTED,
            );
            job.wrap =
                egui::text::TextWrapping::truncate_at_width((right - av.right() - 8.0).max(10.0));
            let galley = ui.painter().layout_job(job);
            ui.painter().galley(
                pos2(av.right() + 8.0, r.center().y - galley.size().y / 2.0),
                galley,
                MUTED,
            );
        }
    }
}

// ---------------------------------------------------------------- the call and me

/// One of a call's buttons.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CallButton {
    Mic,
    Sound,
    Camera,
    Screen,
    /// Shows or hides the native bar for annotating my own screen share.
    /// Only drawn when a screen or window is being shared.
    Draw,
    Music,
    Hang,
}

/// The microphone, the sound and the music, by the gear under "Voz
/// conectada", as Discord has them.
pub const SOUND_BUTTONS: &[CallButton] = &[CallButton::Mic, CallButton::Sound, CallButton::Music];

/// What stays under the call view: the camera, the screen, drawing and
/// hanging up (the microphone and the sound are in the side panel).
pub const STAGE_BUTTONS: &[CallButton] = &[
    CallButton::Camera,
    CallButton::Screen,
    CallButton::Draw,
    CallButton::Hang,
];

/// Everything, for fullscreen, where the side panel is not on screen.
pub const ALL_BUTTONS: &[CallButton] = &[
    CallButton::Mic,
    CallButton::Sound,
    CallButton::Camera,
    CallButton::Screen,
    CallButton::Draw,
    CallButton::Music,
    CallButton::Hang,
];

/// A call's buttons, `side` px each: round grey ones of 48 under the call
/// view, bare ones of 32 in the side panel, by the gear.
pub fn voice_buttons(ui: &mut Ui, core: &Core, acts: &mut Acts, side: f32, which: &[CallButton]) {
    let mute = core.voice.mute();
    let deaf = core.voice.deaf();
    let video = core.voice.video();
    let presenting = core.voice.presenting();
    let drawing = core.voice.drawing();
    let big = side >= 48.0;
    let icon_size = if big { icons::LARGE } else { icons::SIZE };
    let plain = if big { Ib::Normal } else { Ib::Bare };
    ui.spacing_mut().item_spacing.x = if big { 10.0 } else { 2.0 };
    for &button in which {
        // Drawing is only relevant when a screen or window is being
        // shared; the button hides when the camera is on or nothing is
        // shared, so a stale toggle does not sit on screen.
        if button == CallButton::Draw && video != Some(Video::Screen) {
            continue;
        }
        let (icon, style, tip) = match button {
            CallButton::Mic => (
                Icon::mic(mute),
                if mute { Ib::Off } else { plain },
                t!("call.mic"),
            ),
            CallButton::Sound => (
                Icon::head(deaf),
                if deaf { Ib::Off } else { plain },
                t!("call.audio"),
            ),
            CallButton::Camera => (
                Icon::Video,
                if video == Some(Video::Camera) {
                    Ib::On
                } else {
                    plain
                },
                t!("call.camera"),
            ),
            CallButton::Screen if presenting => {
                (Icon::Presentation, Ib::On, t!("call.stop_presenting"))
            }
            CallButton::Screen => (
                Icon::Screen,
                if video == Some(Video::Screen) {
                    Ib::On
                } else {
                    plain
                },
                t!("call.share_screen"),
            ),
            CallButton::Draw => (
                Icon::Edit,
                if drawing { Ib::On } else { plain },
                if drawing {
                    t!("call.hide_draw")
                } else {
                    t!("call.draw")
                },
            ),
            CallButton::Music => (
                Icon::Play,
                if core.music.now_playing().is_some() {
                    Ib::On
                } else {
                    plain
                },
                t!("call.music"),
            ),
            CallButton::Hang => (Icon::Hang, Ib::Hang, t!("call.hang")),
        };
        if !widgets::icon_button_sized(ui, icon, side, icon_size, style, tip).clicked() {
            continue;
        }
        match button {
            CallButton::Mic => acts.later(Core::toggle_mute),
            CallButton::Sound => acts.later(|c| c.voice.toggle_deaf()),
            CallButton::Camera => acts.later(move |c| {
                let next = (video != Some(Video::Camera)).then_some(Video::Camera);
                c.voice.set_video(next);
            }),
            // Sharing or presenting already: stop. Otherwise pick a screen,
            // a window or a presentation.
            CallButton::Screen => acts.later(move |c| {
                if presenting {
                    c.voice.stop_presenting();
                } else if video == Some(Video::Screen) {
                    c.voice.share(None);
                } else {
                    c.open_share();
                }
            }),
            CallButton::Draw => acts.later(|c| c.voice.toggle_draw()),
            CallButton::Music => acts.later(Core::open_music),
            CallButton::Hang => acts.later(|c| c.voice.leave(false, Cue::Leave)),
        }
    }
}

fn voice_panel(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Some((s, c)) = core.voice_location() else {
        return;
    };
    let (sid, cid) = (s.id, c.id);
    let place = format!("{} · {}", c.label(), s.name);
    let top = ui.max_rect().top() - 10.0;
    ui.painter().hline(
        ui.max_rect().x_range().expand(12.0),
        top,
        Stroke::new(1.0, BG0),
    );
    let resp = ui
        .vertical(|ui| {
            ui.label(
                egui::RichText::new(t!("call.connected"))
                    .font(theme::bold(13.0))
                    .color(ACCENT),
            );
            widgets::ellipsis(ui, &place, theme::small(), MUTED);
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        acts.later(move |c| {
            c.go_to(View::Server {
                server: sid,
                channel: Some(cid),
            })
        });
    }
}

/// Me at the bottom of the side panel, with the gear; in a call, the
/// microphone and the sound go by it (the other buttons are under the call
/// view).
fn user_panel(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Some(me) = core.me.clone() else { return };
    let in_call = core.voice_location().is_some();
    const SIDE: f32 = 32.0;
    let buttons = if in_call { 1 + SOUND_BUTTONS.len() } else { 1 };
    let room = buttons as f32 * SIDE + (buttons - 1) as f32 * 2.0 + 10.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let mut shown = me.clone();
        shown.status = me.status.public();
        widgets::avatar(ui, core, &shown, 34.0, true, BG0);
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - room).max(24.0));
            ui.spacing_mut().item_spacing.y = 0.0;
            widgets::ellipsis(ui, me.name(), theme::bold(14.0), TEXT);
            widgets::ellipsis(ui, me.status.label(), theme::tiny(), MUTED);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if widgets::icon_button(ui, Icon::Gear, SIDE, Ib::Bare, t!("settings.title")).clicked()
            {
                acts.later(Core::open_settings);
            }
            if in_call {
                // Right to left: the sound, then the microphone.
                let order: Vec<CallButton> = SOUND_BUTTONS.iter().rev().copied().collect();
                voice_buttons(ui, core, acts, SIDE, &order);
            }
        });
    });
}

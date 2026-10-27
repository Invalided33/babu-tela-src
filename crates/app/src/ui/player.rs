//! The players of audio and video attachments: a bar for sound, a picture
//! with the same bar under it for video, and the enlarged view.

use egui::{
    Align, Color32, CornerRadius, CursorIcon, Layout, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2,
};

use super::Acts;
use super::widgets::{self, Ib};
use crate::core::{Core, Modal};
use crate::icons::Icon;
use crate::media::{self, Status};
use crate::model::{Attachment, Kind};
use crate::theme::{self, *};

/// The widest an attachment gets in the chat.
pub const INLINE_W: f32 = 420.0;

/// A sound attachment: play, the bar, the time and the volume.
pub fn audio(ui: &mut Ui, core: &Core, a: &Attachment, acts: &mut Acts) {
    let width = ui.available_width().min(INLINE_W);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 64.0), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(RADIUS_MD),
        BG1,
        Stroke::new(1.0, BG4),
        egui::StrokeKind::Inside,
    );
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(12.0, 8.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let ui = &mut inner;
    ui.spacing_mut().item_spacing.x = 10.0;
    let player = core.media.peek(&a.url);
    play_button(ui, player, a, acts, 36.0);
    let rest = ui.available_width() - 40.0;
    ui.allocate_ui_with_layout(
        vec2(rest.max(60.0), 48.0),
        Layout::top_down(Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let name = widgets::ellipsis(ui, &a.name, theme::small(), TEXT)
                .interact(Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text("Abrir");
            if name.clicked() {
                super::chat::open_attachment(acts, &core.api, a);
            }
            // The line and the time side by side, under the name.
            ui.horizontal(|ui| bar(ui, player, &a.url, acts));
        },
    );
    mute_button(ui, player, &a.url, acts);
}

/// A video attachment: its picture (or a dark box with a play sign before it
/// starts) and the bar under it.
pub fn video(ui: &mut Ui, core: &Core, a: &Attachment, acts: &mut Acts, max: Vec2, enlarged: bool) {
    let player = core.media.peek(&a.url);
    let natural = player
        .and_then(|p| p.size())
        .map_or(vec2(16.0, 9.0) * 40.0, |[w, h]| vec2(w as f32, h as f32));
    let scale = (max.x / natural.x)
        .min(max.y / natural.y)
        .min(if enlarged { 4.0 } else { 1.0 });
    let size = (natural * scale).max(vec2(160.0, 90.0));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter_at(rect);
    let radius = CornerRadius::same(RADIUS_MD);
    painter.rect_filled(rect, radius, Color32::BLACK);
    if let Some(t) = player.and_then(|p| p.texture()) {
        // Rounded like the box behind it.
        egui::Image::from_texture(egui::load::SizedTexture::from_handle(t))
            .corner_radius(radius)
            .paint_at(ui, rect);
    }
    let status = player.map(|p| p.status());
    match &status {
        Some(Status::Loading) => {
            let r = Rect::from_center_size(rect.center(), vec2(28.0, 28.0));
            ui.put(r, egui::Spinner::new().size(28.0).color(TEXT));
        }
        Some(Status::Failed(_)) => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Não deu para tocar este vídeo aqui.\nClique para abrir no navegador.",
                theme::small(),
                MUTED,
            );
        }
        Some(Status::Playing) => {}
        _ => {
            // Not started, paused or over: the big play sign.
            let c = rect.center();
            painter.circle_filled(c, 28.0, Color32::from_black_alpha(150));
            let icon = Rect::from_center_size(c + vec2(2.0, 0.0), vec2(26.0, 26.0));
            Icon::Play.image(26.0, Color32::WHITE).paint_at(ui, icon);
        }
    }
    if resp.clicked() {
        if matches!(status, Some(Status::Failed(_))) {
            super::chat::open_attachment(acts, &core.api, a);
        } else {
            toggle(acts, a);
        }
    }
    // The bar under the picture, as wide as it.
    let (bar_rect, _) = ui.allocate_exact_size(vec2(size.x, 36.0), Sense::hover());
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(bar_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let ui = &mut row;
    ui.spacing_mut().item_spacing.x = 8.0;
    play_button(ui, player, a, acts, 28.0);
    let room = ui.available_width() - if enlarged { 40.0 } else { 76.0 };
    ui.allocate_ui_with_layout(
        vec2(room.max(60.0), 28.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            bar(ui, player, &a.url, acts);
        },
    );
    mute_button(ui, player, &a.url, acts);
    if !enlarged {
        let expand = widgets::icon_button(ui, Icon::Expand, 28.0, Ib::Bare, "Ampliar");
        if expand.clicked() {
            let a = a.clone();
            acts.later(move |c| c.open_modal(Modal::Player { attachment: a }));
        }
    }
}

fn toggle(acts: &mut Acts, a: &Attachment) {
    let (url, name, kind) = (a.url.clone(), a.name.clone(), a.kind);
    acts.later(move |c| {
        let playing = c.media.peek(&url).is_some_and(media::Player::playing);
        if playing {
            if let Some(p) = c.media.get(&url) {
                p.pause();
            }
        } else {
            c.media.play(&url, &name, kind);
        }
    });
}

fn play_button(
    ui: &mut Ui,
    player: Option<&media::Player>,
    a: &Attachment,
    acts: &mut Acts,
    side: f32,
) {
    let playing = player.is_some_and(media::Player::playing);
    let (icon, tip) = if playing {
        (Icon::Pause, "Pausar")
    } else {
        (Icon::Play, "Tocar")
    };
    let style = if playing { Ib::On } else { Ib::Normal };
    if widgets::icon_button(ui, icon, side, style, tip).clicked() {
        toggle(acts, a);
    }
}

fn mute_button(ui: &mut Ui, player: Option<&media::Player>, url: &str, acts: &mut Acts) {
    let muted = player.is_some_and(media::Player::muted);
    let (icon, tip) = if muted {
        (Icon::VolumeOff, "Ativar o som")
    } else {
        (Icon::Volume, "Silenciar")
    };
    if widgets::icon_button(ui, icon, 28.0, Ib::Bare, tip).clicked() && player.is_some() {
        let url = url.to_owned();
        acts.later(move |c| {
            if let Some(p) = c.media.get(&url) {
                p.set_muted(!muted);
            }
        });
    }
}

/// The position: a line to click or drag, and "0:12 / 1:03".
fn bar(ui: &mut Ui, player: Option<&media::Player>, url: &str, acts: &mut Acts) {
    let (at, total) = match player {
        Some(p) => (p.position(), p.duration()),
        None => (0.0, None),
    };
    let label = match total {
        Some(d) => format!("{} / {}", media::clock(at), media::clock(d)),
        None if player.is_some() => media::clock(at),
        None => "—".to_owned(),
    };
    let label_w = 86.0;
    let width = (ui.available_width() - label_w).max(40.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 16.0), Sense::click_and_drag());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 4.0));
    let painter = ui.painter();
    painter.rect_filled(track, CornerRadius::same(2), BG4);
    let fraction = total.map_or(0.0, |d| (at / d.max(0.001)).clamp(0.0, 1.0)) as f32;
    let filled = Rect::from_min_max(
        track.min,
        pos2(track.left() + track.width() * fraction, track.max.y),
    );
    painter.rect_filled(filled, CornerRadius::same(2), ACCENT);
    if resp.hovered() || resp.dragged() {
        painter.circle_filled(pos2(filled.right(), track.center().y), 6.0, TEXT);
    }
    if (resp.clicked() || resp.dragged())
        && let (Some(pos), Some(d)) = (resp.interact_pointer_pos(), total)
    {
        let t = f64::from(((pos.x - track.left()) / track.width()).clamp(0.0, 1.0)) * d;
        let url = url.to_owned();
        acts.later(move |c| {
            if let Some(p) = c.media.get(&url) {
                p.seek(t);
            }
        });
    }
    ui.add_sized(
        vec2(label_w, 16.0),
        egui::Label::new(egui::RichText::new(label).font(theme::tiny()).color(MUTED)),
    );
}

/// The enlarged view (Modal::Player).
pub fn enlarged(ui: &mut Ui, core: &Core, a: &Attachment, acts: &mut Acts) {
    ui.horizontal(|ui| {
        widgets::ellipsis(ui, &a.name, theme::bold(15.0), TEXT);
    });
    ui.add_space(8.0);
    let screen = ui.ctx().content_rect();
    let max = vec2(ui.available_width(), (screen.height() * 0.7).max(240.0));
    video(ui, core, a, acts, max, true);
}

/// Whether this attachment plays inside the app.
pub fn playable(a: &Attachment) -> bool {
    matches!(a.kind, Kind::Audio | Kind::Video)
}

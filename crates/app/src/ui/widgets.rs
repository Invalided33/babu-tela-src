//! The building blocks the stylesheet had: buttons (`.btn`, `.ghost`,
//! `.danger`, `.sm`, `.wide`), icon buttons (`.ib`), avatars with their
//! status dot, badges, pills, labelled fields and the checkbox rows.

use egui::epaint::{CornerRadius, RectShape, StrokeKind};
use egui::{
    Align2, Color32, CursorIcon, FontId, Id, Image, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextEdit, TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2,
};

use crate::core::Core;
use crate::icons::Icon;
use crate::images::Fit;
use crate::model::User;
use crate::theme::{self, *};

// ---------------------------------------------------------------- the gradient

/// The accent gradient as a tiny texture, so a rounded shape can be filled
/// with it the way `background: var(--grad)` filled the buttons.
fn gradient(ui: &Ui) -> Option<TextureHandle> {
    let id = Id::unique("babutela-gradient");
    if let Some(t) = ui.ctx().data(|d| d.get_temp::<TextureHandle>(id)) {
        return Some(t);
    }
    let w = 64;
    let pixels: Vec<Color32> = (0..w)
        .map(|x| theme::mix(GRAD_A, GRAD_B, x as f32 / (w - 1) as f32))
        .collect();
    let img = egui::ColorImage::new([w, 1], pixels);
    let t = ui
        .ctx()
        .load_texture("gradient", img, TextureOptions::LINEAR);
    ui.ctx().data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

/// Fills `rect` with the accent gradient.
pub fn paint_gradient(ui: &Ui, rect: Rect, radius: impl Into<CornerRadius>, dim: f32) {
    let radius = radius.into();
    let tint = Color32::WHITE.gamma_multiply(dim);
    match gradient(ui) {
        Some(t) => {
            let shape = RectShape::filled(rect, radius, tint)
                .with_texture(t.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
            ui.painter().add(Shape::Rect(shape));
        }
        None => {
            ui.painter()
                .rect_filled(rect, radius, ACCENT.gamma_multiply(dim));
        }
    }
}

// ---------------------------------------------------------------- buttons

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Ghost,
    Danger,
}

pub struct Button<'a> {
    text: &'a str,
    kind: Kind,
    small: bool,
    wide: bool,
    enabled: bool,
}

impl<'a> Button<'a> {
    pub fn primary(text: &'a str) -> Button<'a> {
        Button {
            text,
            kind: Kind::Primary,
            small: false,
            wide: false,
            enabled: true,
        }
    }

    pub fn ghost(text: &'a str) -> Button<'a> {
        Button {
            kind: Kind::Ghost,
            ..Button::primary(text)
        }
    }

    pub fn danger(text: &'a str) -> Button<'a> {
        Button {
            kind: Kind::Danger,
            ..Button::primary(text)
        }
    }

    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }

    pub fn wide(mut self) -> Self {
        self.wide = true;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let font = theme::bold(if self.small { 13.0 } else { 14.0 });
        let pad = if self.small {
            vec2(10.0, 5.0)
        } else if self.wide {
            vec2(16.0, 11.0)
        } else {
            vec2(16.0, 9.0)
        };
        let ink = match self.kind {
            Kind::Primary => ACCENT_INK,
            Kind::Ghost => TEXT,
            Kind::Danger => Color32::WHITE,
        };
        let galley = ui.painter().layout_no_wrap(self.text.to_owned(), font, ink);
        let mut size = galley.size() + pad * 2.0;
        if self.wide {
            size.x = ui.available_width().max(size.x);
        }
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        crate::smoke::target(|| format!("botao:{}", self.text), rect);
        if ui.is_rect_visible(rect) {
            let dim = if self.enabled { 1.0 } else { 0.5 };
            let radius = CornerRadius::same(RADIUS_MD);
            match self.kind {
                Kind::Primary => paint_gradient(ui, rect, radius, dim),
                Kind::Ghost => {
                    ui.painter()
                        .rect_filled(rect, radius, BG3.gamma_multiply(dim));
                }
                Kind::Danger => {
                    ui.painter()
                        .rect_filled(rect, radius, DANGER.gamma_multiply(dim));
                }
            }
            if self.enabled && resp.hovered() {
                ui.painter()
                    .rect_filled(rect, radius, Color32::from_white_alpha(18));
            }
            let pos = rect.center() - galley.size() / 2.0;
            ui.painter().galley(pos, galley, ink.gamma_multiply(dim));
        }
        if self.enabled {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }
}

/// "Continuar com o Google" the way Google's branding guidelines draw it
/// on a dark page: its G in its own colours on #131314, a grey edge, as
/// wide as the space given.
pub fn google_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    let fill = Color32::from_rgb(0x13, 0x13, 0x14);
    let edge = Color32::from_rgb(0x8e, 0x91, 0x8f);
    let ink = Color32::from_rgb(0xe3, 0xe3, 0xe3);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::bold(14.0), ink);
    let (logo, gap) = (20.0, 10.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), sense);
    if ui.is_rect_visible(rect) {
        let dim = if enabled { 1.0 } else { 0.55 };
        let radius = CornerRadius::same(21);
        ui.painter().rect_filled(rect, radius, fill);
        ui.painter().rect_stroke(
            rect,
            radius,
            Stroke::new(1.0, edge.gamma_multiply(dim)),
            StrokeKind::Inside,
        );
        if enabled && resp.hovered() {
            ui.painter()
                .rect_filled(rect, radius, Color32::from_white_alpha(12));
        }
        let left = rect.center().x - (logo + gap + galley.size().x) / 2.0;
        let mark = Rect::from_min_size(pos2(left, rect.center().y - logo / 2.0), Vec2::splat(logo));
        Image::new(egui::include_image!("../../assets/google-g.svg"))
            .tint(Color32::WHITE.gamma_multiply(dim))
            .paint_at(ui, mark);
        let text = pos2(left + logo + gap, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(text, galley, ink.gamma_multiply(dim));
    }
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// A text that acts as a link (`.link`).
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let font = theme::body();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, ACCENT);
    let (rect, resp) = ui.allocate_exact_size(galley.size(), Sense::click());
    let hovered = resp.hovered();
    ui.painter().galley(rect.min, galley, ACCENT);
    if hovered {
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 1.0,
            Stroke::new(1.0, ACCENT),
        );
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

// ---------------------------------------------------------------- icon buttons

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ib {
    /// The plain grey square.
    Normal,
    /// No background until hovered (headers, the composer, the user panel).
    Bare,
    /// Switched on: the accent gradient.
    On,
    /// Something off that should be noticed: microphone, sound.
    Off,
    /// Hang up.
    Hang,
}

/// A square button with an icon. `side` is the square, the icon is drawn at
/// the usual size inside it.
pub fn icon_button(ui: &mut Ui, icon: Icon, side: f32, style: Ib, tip: &str) -> Response {
    icon_button_sized(ui, icon, side, crate::icons::SIZE, style, tip)
}

pub fn icon_button_sized(
    ui: &mut Ui,
    icon: Icon,
    side: f32,
    icon_size: f32,
    style: Ib,
    tip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    let hovered = resp.hovered();
    let radius = if side >= 48.0 {
        CornerRadius::same((side / 2.0) as u8)
    } else {
        CornerRadius::same(RADIUS_MD)
    };
    let danger_soft = Color32::from_rgba_unmultiplied(0xef, 0x5f, 0x67, 0x22);
    let (bg, fg) = match (style, hovered) {
        (Ib::Normal, false) => (Some(BG3), MUTED),
        (Ib::Normal, true) => (Some(BG4), TEXT),
        (Ib::Bare, false) => (None, MUTED),
        (Ib::Bare, true) => (Some(BG3), TEXT),
        (Ib::On, _) => (None, ACCENT_INK),
        (Ib::Off, _) => (Some(danger_soft), DANGER),
        (Ib::Hang, false) => (Some(danger_soft), DANGER),
        (Ib::Hang, true) => (Some(DANGER), Color32::WHITE),
    };
    if style == Ib::On {
        paint_gradient(ui, rect, radius, 1.0);
    } else if let Some(bg) = bg {
        ui.painter().rect_filled(rect, radius, bg);
    }
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(icon_size));
    icon.image(icon_size, fg).paint_at(ui, icon_rect);
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    }
}

// ---------------------------------------------------------------- avatars

/// The round picture (or the initial on the person's colour) with an
/// optional status dot, ringed in `ring` so it reads on any background.
/// Lights up while the person is speaking in a call.
pub fn avatar(
    ui: &mut Ui,
    core: &Core,
    user: &User,
    size: f32,
    dot: bool,
    ring: Color32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    paint_avatar(ui, core, user, rect, dot, ring);
    resp
}

/// A server's rounded square: its picture when it has one (once loaded),
/// otherwise `fill` with its initials in `ink`.
#[allow(clippy::too_many_arguments)]
pub fn paint_server(
    ui: &Ui,
    core: &Core,
    icon: Option<&str>,
    initials: &str,
    rect: Rect,
    radius: u8,
    fill: Color32,
    ink: Color32,
) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let picture = icon.filter(|i| !i.is_empty()).and_then(|url| {
        core.images
            .get(url, Fit::Cover((rect.width() * 2.0).ceil() as u32))
    });
    match picture {
        Some(t) => {
            Image::from_texture(egui::load::SizedTexture::from_handle(&t))
                .corner_radius(CornerRadius::same(radius))
                .paint_at(ui, rect);
        }
        None => {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(radius), fill);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                initials,
                theme::bold((rect.height() * 0.33).round()),
                ink,
            );
        }
    }
}

pub fn paint_avatar(ui: &Ui, core: &Core, user: &User, rect: Rect, dot: bool, ring: Color32) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let size = rect.width();
    let center = rect.center();
    let painter = ui.painter();
    let picture = user
        .avatar
        .as_deref()
        .filter(|a| !a.is_empty())
        .and_then(|url| core.images.get(url, Fit::Cover((size * 2.0).ceil() as u32)));
    match picture {
        Some(t) => {
            Image::from_texture(egui::load::SizedTexture::from_handle(&t))
                .corner_radius(CornerRadius::same((size / 2.0) as u8))
                .paint_at(ui, rect);
        }
        None => {
            painter.circle_filled(center, size / 2.0, theme::color_or(&user.color, BG4));
            painter.text(
                center,
                Align2::CENTER_CENTER,
                user.initial(),
                theme::bold(size * 0.42),
                Color32::from_rgb(0x10, 0x13, 0x1a),
            );
        }
    }
    if core.voice.speaking(user.id) {
        painter.circle_stroke(center, size / 2.0 + 1.0, Stroke::new(2.0, ACCENT));
    }
    if dot {
        let d = (size * 0.3).max(10.0);
        let c = pos2(rect.right() - d / 2.0 + 2.0, rect.bottom() - d / 2.0 + 2.0);
        painter.circle_filled(c, d / 2.0 + 2.0, ring);
        painter.circle_filled(c, d / 2.0, theme::status_color(user.status.public()));
    }
}

// ---------------------------------------------------------------- small things

/// The red counter (`.badge`). `border` is the colour around it, `None`
/// for none.
pub fn badge_at(ui: &Ui, center: Pos2, n: u32, border: Option<Color32>) {
    let text = n.to_string();
    let galley = ui
        .painter()
        .layout_no_wrap(text, theme::bold(11.0), Color32::WHITE);
    let w = (galley.size().x + 10.0).max(18.0);
    let rect = Rect::from_center_size(center, vec2(w, 18.0));
    if let Some(b) = border {
        ui.painter()
            .rect_filled(rect.expand(2.0), CornerRadius::same(11), b);
    }
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), DANGER);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// A role's name with its colour dot (`.pill`).
pub fn pill(ui: &mut Ui, name: &str, color: Option<&str>) -> Response {
    let font = theme::small();
    let galley = ui.painter().layout_no_wrap(name.to_owned(), font, TEXT);
    let size = vec2(galley.size().x + 9.0 + 8.0 + 6.0 + 9.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    ui.painter().rect_filled(rect, CornerRadius::same(10), BG3);
    let dot = color.and_then(theme::parse_hex).unwrap_or(FAINT);
    ui.painter()
        .circle_filled(pos2(rect.left() + 13.0, rect.center().y), 4.0, dot);
    ui.painter().galley(
        pos2(rect.left() + 23.0, rect.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
    resp
}

/// A small uppercase heading (`.cat`, `h4`).
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .font(theme::bold(11.0))
            .color(FAINT)
            .extra_letter_spacing(0.5),
    );
}

/// The muted explanation under a heading (`p.sub`).
pub fn sub(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(MUTED));
}

/// A heading of a dialog (`h3`).
pub fn h3(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(theme::h3()).color(TEXT));
}

/// The error line under a form (`.err`).
pub fn error_line(ui: &mut Ui, text: Option<&String>) {
    let text = text.map_or("", String::as_str);
    ui.add_space(2.0);
    ui.label(egui::RichText::new(text).color(DANGER).size(13.0));
    ui.add_space(4.0);
}

/// A labelled input (`label.f input`). Returns the field's response.
pub fn field(ui: &mut Ui, label: &str, value: &mut String, max: usize, password: bool) -> Response {
    ui.label(
        egui::RichText::new(label.to_uppercase())
            .font(theme::bold(12.0))
            .color(MUTED)
            .extra_letter_spacing(0.5),
    );
    ui.add_space(2.0);
    let resp = input(ui, value, max, password, "");
    ui.add_space(10.0);
    resp
}

/// The dark input box (`.input`).
pub fn input(ui: &mut Ui, value: &mut String, max: usize, password: bool, hint: &str) -> Response {
    let edit = TextEdit::singleline(value)
        .password(password)
        .char_limit(max)
        .hint_text(egui::RichText::new(hint).color(FAINT))
        .desired_width(f32::INFINITY)
        .font(theme::body())
        .margin(egui::Margin::symmetric(12, 9))
        .background_color(BG0);
    let resp = ui.add(edit);
    if resp.has_focus() {
        ui.painter().rect_stroke(
            resp.rect,
            CornerRadius::same(RADIUS_MD),
            Stroke::new(1.0, ACCENT),
            StrokeKind::Inside,
        );
    }
    resp
}

/// A row with a title, an explanation and a switch at the right (`.check`).
/// Returns `true` when it was flipped.
pub fn check_row(ui: &mut Ui, label: &str, hint: &str, value: &mut bool, enabled: bool) -> bool {
    let mut flipped = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(ui.available_width() - 30.0);
            ui.label(egui::RichText::new(label).color(if enabled { TEXT } else { FAINT }));
            if !hint.is_empty() {
                ui.label(egui::RichText::new(hint).color(MUTED).size(13.0));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_enabled_ui(enabled, |ui| {
                if ui.checkbox(value, "").changed() {
                    flipped = true;
                }
            });
        });
    });
    ui.add_space(4.0);
    let y = ui.cursor().top();
    ui.painter()
        .hline(ui.max_rect().x_range(), y, Stroke::new(1.0, BG3));
    ui.add_space(4.0);
    flipped
}

/// A thin divider (`hr`).
pub fn divider(ui: &mut Ui) {
    ui.add_space(14.0);
    let y = ui.cursor().top();
    ui.painter()
        .hline(ui.max_rect().x_range(), y, Stroke::new(1.0, BG4));
    ui.add_space(16.0);
}

/// Text that is cut with "…" when it does not fit.
pub fn ellipsis(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> Response {
    ui.add(
        egui::Label::new(egui::RichText::new(text).font(font).color(color))
            .truncate()
            .selectable(false),
    )
}

/// The "@user" style used under names.
pub fn handle(user: &User) -> String {
    format!("@{}", user.username)
}

/// Formats a byte count the way the old page did (and in GB past that,
/// now that files have no size limit).
pub fn fmt_size(n: i64) -> String {
    if n > 1 << 30 {
        format!("{:.1} GB", n as f64 / f64::from(1u32 << 30))
    } else if n > 1_048_576 {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    } else {
        format!("{} KB", ((n as f64 / 1024.0).round() as i64).max(1))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_read_like_before() {
        assert_eq!(super::fmt_size(10), "1 KB");
        assert_eq!(super::fmt_size(2048), "2 KB");
        assert_eq!(super::fmt_size(3 * 1_048_576), "3.0 MB");
        assert_eq!(super::fmt_size(1_048_576), "1024 KB");
        assert_eq!(super::fmt_size(5 << 29), "2.5 GB");
    }
}

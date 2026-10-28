//! The call's view of a presentation (slides/). The presenter sees the
//! slide on screen, the next one, the notes and a strip of every slide,
//! with a clock; everyone else sees the presenter's slide, can go through
//! the slides on their own (and back to the presenter's with a button) and
//! zoom in, with the people of the call in a strip below.
//!
//! The keys are a presenter's remote's: → ↓ PageDown Space Enter N go
//! forward, ← ↑ PageUp Backspace P go back, Home and End to either end.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::load::SizedTexture;
use egui::{
    Align, Color32, CornerRadius, CursorIcon, FontId, Id, Key, Layout, Modifiers, Rect, RichText,
    ScrollArea, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2,
};

use super::Acts;
use super::widgets::{self, Button, Ib};
use crate::core::Core;
use crate::icons::Icon;
use crate::model::{Video, VoiceUser};
use crate::slides::{Owner, View};
use crate::theme::{self, ACCENT, BG2, BG3, BG4, FAINT, MUTED, RADIUS_SM, TEXT, WARN};
use crate::voice::plan::{self, Layer};
use crate::{t, tf};

/// The bar across the top of the view, and the space between its parts.
const BAR_H: f32 = 34.0;
const GAP: f32 = 12.0;

/// Viewers zoom in up to this many times the fitted slide.
const MAX_ZOOM: f32 = 4.0;

/// Which presentation the call view shows: mine while I present, else the
/// newest of someone in the call — but only if the viewer opted in. Others'
/// presentations start hidden (like Discord's "Watch Stream"); the viewer
/// must click "Assistir" to see it.
pub fn shown(core: &Core, users: &[VoiceUser]) -> Option<View> {
    let slides = core.voice.slides();
    // My own presentation: I always see it (I'm the presenter).
    if let Some(mine) = slides.mine() {
        return Some(mine);
    }
    // Someone else's: only if the viewer opted in to watching it.
    let theirs = slides.theirs();
    theirs.into_iter().find(|v| {
        matches!(v.owner, Owner::Them(uid) if users.iter().any(|u| u.user_id == uid))
            && is_watching(core, v.id)
    })
}

/// Whether the viewer chose to watch presentation `id`.
fn is_watching(core: &Core, id: u32) -> bool {
    core.ctx
        .data(|d| d.get_temp::<u32>(Id::unique("slides-watching")))
        == Some(id)
}

/// Makes the viewer watch presentation `id`, or stop watching (None).
fn set_watching(ctx: &egui::Context, id: Option<u32>) {
    ctx.data_mut(|d| {
        if let Some(id) = id {
            d.insert_temp(Id::unique("slides-watching"), id);
        } else {
            d.remove::<u32>(Id::unique("slides-watching"));
        }
    });
}

/// A banner at the top of the stage when someone is presenting but the
/// viewer hasn't opted in to watch yet. Shows "Fulano está apresentando
/// [file]" with an "Assistir" button.
pub fn presenting_banner(ui: &mut Ui, core: &Core, users: &[VoiceUser]) {
    let slides = core.voice.slides();
    // Don't show the banner if I'm the presenter (I always see my own).
    if slides.mine().is_some() {
        return;
    }
    let theirs = slides.theirs();
    let active = theirs
        .iter()
        .find(|v| matches!(v.owner, Owner::Them(uid) if users.iter().any(|u| u.user_id == uid)));
    let Some(v) = active else { return };
    // Don't show the banner if we're already watching this one.
    if is_watching(core, v.id) {
        return;
    }
    let uid = match v.owner {
        Owner::Them(uid) => uid,
        _ => return,
    };
    let user = core.user(uid);
    let id = v.id;
    let name = v.name.clone();
    egui::Frame::new()
        .fill(crate::theme::ACCENT_SOFT)
        .inner_margin(egui::Margin::symmetric(16, 8))
        .corner_radius(egui::CornerRadius::same(crate::theme::RADIUS_SM))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add(Icon::Presentation.tinted(crate::theme::ACCENT));
                ui.label(
                    egui::RichText::new(tf!("slides.presenting_by", name = user.name()))
                        .font(crate::theme::bold(14.0))
                        .color(crate::theme::TEXT),
                );
                if !name.is_empty() {
                    crate::ui::widgets::ellipsis(
                        ui,
                        &name,
                        crate::theme::small(),
                        crate::theme::MUTED,
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::ui::widgets::Button::primary(t!("slides.watch"))
                        .small()
                        .show(ui)
                        .clicked()
                    {
                        set_watching(ui.ctx(), Some(id));
                    }
                });
            });
        });
    ui.add_space(8.0);
}

/// Where the call view keeps the presentation it was told to set aside
/// for the people's tiles ("Ver pessoas"), by its id.
pub fn set_aside(cid: i64) -> Id {
    Id::unique(("stage-people", cid))
}

/// The presentation in the call view. What videos its strip of people
/// shows (nobody's, in the presenter's view).
pub fn draw(
    ui: &mut Ui,
    core: &Core,
    v: &View,
    users: &[VoiceUser],
    cid: i64,
    acts: &mut Acts,
) -> HashMap<i64, Layer> {
    match v.owner {
        Owner::Me => {
            presenter(ui, core, v, cid, acts);
            HashMap::new()
        }
        Owner::Them(uid) => viewer(ui, core, v, uid, users, cid, acts),
    }
}

// ---------------------------------------------------------------- moving

enum Nav {
    Next,
    Back,
    First,
    Last,
}

/// A key of a presenter's remote (or of the keyboard), taken before any
/// button of the view sees it. Not while typing, nor with a dialog or a
/// menu open.
fn keys(ui: &Ui, core: &Core) -> Option<Nav> {
    if core.modal.is_some() || core.pop.is_some() || ui.ctx().egui_wants_keyboard_input() {
        return None;
    }
    ui.ctx().input_mut(|i| {
        let mut take = |keys: &[Key]| keys.iter().any(|k| i.consume_key(Modifiers::NONE, *k));
        if take(&[
            Key::ArrowRight,
            Key::ArrowDown,
            Key::PageDown,
            Key::Space,
            Key::Enter,
            Key::N,
        ]) {
            Some(Nav::Next)
        } else if take(&[
            Key::ArrowLeft,
            Key::ArrowUp,
            Key::PageUp,
            Key::Backspace,
            Key::P,
        ]) {
            Some(Nav::Back)
        } else if take(&[Key::Home]) {
            Some(Nav::First)
        } else if take(&[Key::End]) {
            Some(Nav::Last)
        } else {
            None
        }
    })
}

fn apply(nav: Nav, at: usize, count: usize) -> usize {
    let last = count.saturating_sub(1);
    match nav {
        Nav::Next => (at + 1).min(last),
        Nav::Back => at.saturating_sub(1),
        Nav::First => 0,
        Nav::Last => last,
    }
}

/// ◀ "5 de 19" ▶, centred in `rect`: the slide to go to, if one was
/// clicked.
fn steps(ui: &mut Ui, rect: Rect, at: usize, count: usize, side: f32, style: Ib) -> Option<usize> {
    let label = tf!("common.x_of_y", sent = at + 1, total = count);
    let font = theme::bold(14.0);
    let text_w = ui
        .painter()
        .layout_no_wrap(label.clone(), font.clone(), TEXT)
        .size()
        .x;
    let width = side * 2.0 + text_w + 24.0;
    let row = Rect::from_center_size(rect.center(), vec2(width, side));
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(row)
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 12.0;
    let mut to = None;
    let back = widgets::icon_button(&mut row, Icon::Left, side, style, t!("slides.prev_slide"));
    if back.clicked() {
        back.surrender_focus();
        to = at.checked_sub(1);
    }
    row.label(RichText::new(label).font(font).color(TEXT));
    let next = widgets::icon_button(&mut row, Icon::Right, side, style, t!("slides.next_slide"));
    if next.clicked() {
        next.surrender_focus();
        to = (at + 1 < count).then_some(at + 1);
    }
    to
}

// ---------------------------------------------------------------- painting

/// A rect worked out by subtracting, never inside out (egui refuses a
/// child of negative size): in a small window the parts just get empty.
fn sane(r: Rect) -> Rect {
    Rect::from_min_max(r.min, r.max.max(r.min))
}

/// Below this the view has no room for its parts, only for the slide.
const ROOMY: Vec2 = vec2(520.0, 380.0);

/// A picture of `natural` size fitted in `area`, in its middle.
fn fit(natural: Vec2, area: Rect) -> Rect {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return area;
    }
    let scale = (area.width() / natural.x)
        .min(area.height() / natural.y)
        .max(0.0);
    Rect::from_center_size(area.center(), natural * scale)
}

fn natural(v: &View) -> Vec2 {
    vec2(v.size[0] as f32, v.size[1] as f32)
}

/// The texture of slide `slide` of `v` for showing it `width` points wide.
fn texture(
    ui: &Ui,
    core: &Core,
    v: &View,
    slide: usize,
    width: f32,
    urgency: u8,
) -> Option<SizedTexture> {
    let ppp = ui.ctx().pixels_per_point();
    core.voice
        .slides()
        .texture(v.owner, v.id, slide, width * ppp, urgency)
}

/// The room of a slide whose picture is not here yet.
fn waiting(ui: &Ui, rect: Rect) {
    ui.painter()
        .rect_filled(rect, CornerRadius::same(RADIUS_SM), BG3);
    let side = (rect.height() * 0.2).clamp(10.0, 32.0);
    egui::Spinner::new()
        .size(side)
        .color(MUTED)
        .paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(side)));
}

/// Slide `slide` of `v` fitted in `area`: its picture, or its room with a
/// spinner while the picture is on its way. The rect it took.
fn slide(ui: &Ui, core: &Core, v: &View, slide: usize, area: Rect, urgency: u8) -> Rect {
    let guess = fit(natural(v), area);
    match texture(ui, core, v, slide, guess.width(), urgency) {
        Some(t) => {
            let rect = fit(t.size, area);
            egui::Image::from_texture(t)
                .corner_radius(CornerRadius::same(RADIUS_SM))
                .paint_at(ui, rect);
            rect
        }
        None => {
            waiting(ui, guess);
            guess
        }
    }
}

/// A small picture of slide `i` with its number, for the strip; framed
/// when it is the one on screen.
fn thumb(
    ui: &mut Ui,
    core: &Core,
    v: &View,
    i: usize,
    size: Vec2,
    current: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let radius = CornerRadius::same(RADIUS_SM);
    ui.painter().rect_filled(rect, radius, BG2);
    let pic = slide(ui, core, v, i, rect.shrink(3.0), 2);
    let stroke = if current {
        Stroke::new(2.0, ACCENT)
    } else if resp.hovered() {
        Stroke::new(1.0, BG4)
    } else {
        Stroke::NONE
    };
    ui.painter()
        .rect_stroke(rect, radius, stroke, egui::StrokeKind::Inside);
    let number = ui
        .painter()
        .layout_no_wrap((i + 1).to_string(), theme::bold(11.0), TEXT);
    let tag = Rect::from_min_size(
        pos2(pic.left() + 4.0, pic.bottom() - number.size().y - 6.0),
        number.size() + vec2(8.0, 2.0),
    );
    ui.painter()
        .rect_filled(tag, CornerRadius::same(4), Color32::from_black_alpha(0xb0));
    ui.painter().galley(tag.min + vec2(4.0, 1.0), number, TEXT);
    resp
}

/// The strip of every slide, the one on screen kept in sight: a click
/// takes the presentation there.
fn strip(ui: &mut Ui, core: &Core, v: &View, rect: Rect, acts: &mut Acts) {
    let h = (rect.height() - 12.0).max(24.0);
    let aspect = natural(v).x / natural(v).y.max(1.0);
    let size = vec2(h * aspect, h);
    let seen = Id::unique(("slides-strip-at", v.id));
    let moved = ui.data(|d| d.get_temp::<usize>(seen)) != Some(v.at);
    ui.data_mut(|d| d.insert_temp(seen, v.at));
    let mut area = ui.new_child(UiBuilder::new().max_rect(rect));
    ScrollArea::horizontal()
        .id_salt(("slides-strip", v.id))
        .auto_shrink([false, false])
        .show(&mut area, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for i in 0..v.count {
                    let resp = thumb(ui, core, v, i, size, i == v.at);
                    if moved && i == v.at {
                        resp.scroll_to_me(Some(Align::Center));
                    }
                    if resp.clicked() {
                        resp.surrender_focus();
                        acts.later(move |c| c.voice.go_to_slide(i));
                    }
                }
            });
        });
}

// ---------------------------------------------------------------- the presenter

fn presenter(ui: &mut Ui, core: &Core, v: &View, cid: i64, acts: &mut Acts) {
    if v.count > 0
        && let Some(nav) = keys(ui, core)
    {
        let to = apply(nav, v.at, v.count);
        acts.later(move |c| c.voice.go_to_slide(to));
    }
    let full = sane(ui.available_rect_before_wrap());
    ui.allocate_rect(full, Sense::hover());
    if full.width() < ROOMY.x || full.height() < ROOMY.y {
        if v.count > 0 {
            slide(ui, core, v, v.at, full, 0);
        }
        return;
    }
    let bar = Rect::from_min_size(full.min, vec2(full.width(), BAR_H));
    presenter_bar(ui, v, bar, cid, acts);
    let rest = sane(Rect::from_min_max(
        pos2(full.left(), bar.bottom() + GAP),
        full.max,
    ));
    if v.count == 0 {
        let mut ui = ui.new_child(
            UiBuilder::new()
                .max_rect(rest)
                .layout(Layout::top_down(Align::Center)),
        );
        ui.add_space((rest.height() / 2.0 - 30.0).max(0.0));
        ui.add(egui::Spinner::new().size(24.0).color(MUTED));
        ui.label(RichText::new(tf!("slides.opening", name = v.name)).color(MUTED));
        return;
    }
    let strip_h = (full.height() * 0.15).clamp(64.0, 104.0);
    let strip_rect = Rect::from_min_max(pos2(full.left(), full.bottom() - strip_h), full.max);
    let nav_h = 40.0;
    let body = sane(Rect::from_min_max(
        rest.min,
        pos2(full.right(), strip_rect.top() - GAP - nav_h),
    ));
    let side_w = (body.width() * 0.3).clamp(200.0, 380.0);
    let main = sane(Rect::from_min_max(
        body.min,
        pos2(body.right() - side_w - GAP * 2.0, body.bottom()),
    ));
    let side = sane(Rect::from_min_max(
        pos2(main.right() + GAP * 2.0, body.top()),
        body.max,
    ));

    let shown = slide(ui, core, v, v.at, main, 0);
    // Each slide keeps its own annotations (ui/ink.rs).
    if let Some(surface) = core.voice.ink().slides_of(core.me_id(), v.id) {
        let page = crate::ink::Page {
            surface,
            n: v.at as u32,
        };
        super::ink::over(ui, core, page, shown, shown, true, acts);
    }
    // The next one decoded at this size too, so going ahead is instant.
    if v.at + 1 < v.count {
        let _ = texture(ui, core, v, v.at + 1, shown.width(), 1);
    }
    if v.failed.get(v.at).copied().unwrap_or(false) {
        ui.painter().text(
            shown.center(),
            egui::Align2::CENTER_CENTER,
            t!("slides.slide_failed"),
            theme::bold(15.0),
            Color32::from_gray(0x50),
        );
    }
    let nav = Rect::from_min_size(
        pos2(main.left(), main.bottom() + 4.0),
        vec2(main.width(), nav_h),
    );
    if let Some(to) = steps(ui, nav, v.at, v.count, 34.0, Ib::Normal) {
        acts.later(move |c| c.voice.go_to_slide(to));
    }
    next_and_notes(ui, core, v, side);
    strip(ui, core, v, strip_rect, acts);
}

/// "Você está apresentando", the file, how the drawing and sending go, the
/// clock and the buttons.
fn presenter_bar(ui: &mut Ui, v: &View, rect: Rect, cid: i64, acts: &mut Acts) {
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    ui.spacing_mut().item_spacing.x = 8.0;
    ui.add(Icon::Presentation.tinted(ACCENT));
    ui.label(
        RichText::new(t!("slides.you_presenting"))
            .font(theme::bold(14.0))
            .color(TEXT),
    );
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if Button::danger(t!("slides.stop_presenting"))
            .small()
            .show(ui)
            .clicked()
        {
            acts.later(|c| c.voice.stop_presenting());
        }
        if Button::ghost(t!("slides.view_people"))
            .small()
            .show(ui)
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp(set_aside(cid), v.id));
        }
        if v.count > 0 {
            clock(ui, v);
        }
        if let Some(status) = status(v) {
            ui.label(RichText::new(status).font(theme::small()).color(MUTED));
        }
        widgets::ellipsis(ui, &v.name, theme::small(), MUTED);
    });
}

/// How far my presentation is: being drawn, or on its way to people.
fn status(v: &View) -> Option<String> {
    let ready = v.ready.iter().filter(|r| **r).count();
    if v.count > 0 && !v.drawn {
        return Some(tf!("slides.preparing", ready = ready, total = v.count));
    }
    let waiting = v.people - v.people_done.min(v.people);
    if waiting > 0 {
        let pct = (v.sent * 100.0).floor();
        return Some(if waiting == 1 {
            tf!("slides.sending_one", pct = pct)
        } else {
            tf!("slides.sending_many", n = waiting, pct = pct)
        });
    }
    None
}

/// The time since the presentation began (or since it was last clicked,
/// which starts it over), a second at a time.
fn clock(ui: &mut Ui, v: &View) {
    let id = Id::unique(("slides-clock", v.id));
    let since: Instant = ui.data(|d| d.get_temp(id)).unwrap_or(v.since);
    let elapsed = since.elapsed();
    let secs = elapsed.as_secs();
    let text = if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    };
    let resp = ui
        .add(
            egui::Label::new(
                RichText::new(text)
                    .font(FontId::monospace(13.0))
                    .color(TEXT),
            )
            .sense(Sense::click())
            .selectable(false),
        )
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(t!("slides.clock_tip"));
    ui.add(Icon::Clock.image(crate::icons::SMALL, MUTED));
    if resp.clicked() {
        ui.data_mut(|d| d.insert_temp(id, Instant::now()));
    }
    let to_next = 1000 - u64::from(elapsed.subsec_millis());
    ui.ctx()
        .request_repaint_after(Duration::from_millis(to_next.max(1)));
}

/// The next slide, and my notes for this one.
fn next_and_notes(ui: &mut Ui, core: &Core, v: &View, rect: Rect) {
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    widgets::caption(&mut ui, t!("slides.next_slide"));
    ui.add_space(4.0);
    let aspect = natural(v).y / natural(v).x.max(1.0);
    let (room, _) = ui.allocate_exact_size(
        vec2(
            rect.width(),
            (rect.width() * aspect).min(rect.height() * 0.5),
        ),
        Sense::hover(),
    );
    if v.at + 1 < v.count {
        slide(&ui, core, v, v.at + 1, room, 1);
    } else {
        ui.painter()
            .rect_filled(room, CornerRadius::same(RADIUS_SM), BG2);
        ui.painter().text(
            room.center(),
            egui::Align2::CENTER_CENTER,
            t!("slides.end_of_presentation"),
            theme::small(),
            FAINT,
        );
    }
    ui.add_space(14.0);
    // The notes take what is left, when anything is.
    if ui.available_height() < 60.0 {
        return;
    }
    widgets::caption(&mut ui, t!("slides.notes"));
    ui.add_space(4.0);
    let notes = core.voice.slides().notes(v.at);
    ScrollArea::vertical()
        .id_salt(("slides-notes", v.id, v.at))
        .auto_shrink([false, false])
        .show(&mut ui, |ui| {
            if notes.is_empty() {
                ui.label(RichText::new(t!("slides.no_notes")).color(FAINT));
            } else {
                ui.label(
                    RichText::new(notes)
                        .font(FontId::proportional(15.0))
                        .color(TEXT),
                );
            }
        });
}

// ---------------------------------------------------------------- everyone else

/// Where a viewer is in someone's presentation: following them, or on a
/// slide of their own; and how far zoomed in, around which point of the
/// slide (0 to 1 across and down).
#[derive(Clone, Copy, Debug)]
struct Browse {
    at: Option<usize>,
    zoom: f32,
    center: Vec2,
    /// The slide the zoom was for: another one starts fitted.
    shown: Option<usize>,
}

impl Default for Browse {
    fn default() -> Self {
        Browse {
            at: None,
            zoom: 1.0,
            center: vec2(0.5, 0.5),
            shown: None,
        }
    }
}

fn viewer(
    ui: &mut Ui,
    core: &Core,
    v: &View,
    uid: i64,
    users: &[VoiceUser],
    cid: i64,
    acts: &mut Acts,
) -> HashMap<i64, Layer> {
    let id = Id::unique(("slides-browse", v.id));
    let mut b: Browse = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let last = v.count.saturating_sub(1);
    let mut at = b.at.unwrap_or(v.at).min(last);
    if let Some(nav) = keys(ui, core) {
        at = apply(nav, at, v.count);
        b.at = Some(at);
    }
    let full = sane(ui.available_rect_before_wrap());
    ui.allocate_rect(full, Sense::hover());
    if full.width() < ROOMY.x || full.height() < ROOMY.y {
        slide(ui, core, v, at, full, 0);
        ui.data_mut(|d| d.insert_temp(id, b));
        return HashMap::new();
    }
    let bar = Rect::from_min_size(full.min, vec2(full.width(), BAR_H));
    let people_h = if full.height() > 460.0 { 96.0 } else { 0.0 };
    let people_rect = Rect::from_min_max(pos2(full.left(), full.bottom() - people_h), full.max);
    let bottom = if people_h > 0.0 {
        people_rect.top() - GAP
    } else {
        full.bottom()
    };
    let area = sane(Rect::from_min_max(
        pos2(full.left(), bar.bottom() + GAP),
        pos2(full.right(), bottom),
    ));
    if viewer_bar(
        ui,
        core,
        v,
        uid,
        b.at.is_some_and(|mine| mine != v.at),
        bar,
        cid,
    ) {
        b.at = None;
        at = v.at.min(last);
    }

    if b.shown != Some(at) {
        b.zoom = 1.0;
        b.center = vec2(0.5, 0.5);
        b.shown = Some(at);
    }
    let base = fit(natural(v), area);
    crate::smoke::target(|| "slide".into(), area);
    let resp = ui.interact(
        area,
        Id::unique(("slides-zoom", v.id)),
        Sense::click_and_drag(),
    );
    if resp.hovered() {
        let (wheel, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = pinch * (wheel / 300.0).exp();
        if (factor - 1.0).abs() > 1e-4 {
            let to = (b.zoom * factor).clamp(1.0, MAX_ZOOM);
            if let Some(p) = resp.hover_pos() {
                b.center = zoom_at(b.center, p, area, base.size(), b.zoom, to);
            }
            b.zoom = to;
        }
    }
    if resp.dragged() && b.zoom > 1.0 {
        let shown = base.size() * b.zoom;
        b.center -= vec2(resp.drag_delta().x / shown.x, resp.drag_delta().y / shown.y);
    }
    if resp.double_clicked() {
        if b.zoom > 1.0 {
            b.zoom = 1.0;
        } else if let Some(p) = resp.interact_pointer_pos() {
            b.center = zoom_at(b.center, p, area, base.size(), 1.0, 2.0);
            b.zoom = 2.0;
        }
    }
    if b.zoom > 1.0 {
        let icon = if resp.dragged() {
            CursorIcon::Grabbing
        } else {
            CursorIcon::Grab
        };
        resp.on_hover_cursor(icon);
    }
    b.center = clamp_center(b.center, area.size(), base.size() * b.zoom);
    let place = |base: Rect, b: &Browse| {
        let size = base.size() * b.zoom;
        Rect::from_min_size(
            area.center() - vec2(b.center.x * size.x, b.center.y * size.y),
            size,
        )
    };
    let shown = place(base, &b);
    let picture = match texture(ui, core, v, at, shown.width(), 0) {
        Some(t) => {
            // A page of a PDF may have a shape of its own.
            let base = fit(t.size, area);
            let rect = place(base, &b);
            ui.painter_at(area).image(
                t.id,
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            Some(rect)
        }
        None => {
            waiting(ui, shown.intersect(area));
            None
        }
    };
    // The annotations of the slide on screen, zoomed in with it.
    if let Some(picture) = picture
        && let Some(surface) = core.voice.ink().slides_of(uid, v.id)
    {
        let page = crate::ink::Page {
            surface,
            n: at as u32,
        };
        super::ink::over(ui, core, page, picture, area, true, acts);
    }
    // The next slide decoded ahead at this size, for going on at once.
    if at < last {
        let _ = texture(ui, core, v, at + 1, base.width(), 1);
    }
    if ui.rect_contains_pointer(area) {
        controls(ui, v, area, at, &mut b);
    }
    if b.at.is_some_and(|mine| mine == v.at) {
        // Back on the presenter's slide: following them again.
        b.at = None;
    }
    ui.data_mut(|d| d.insert_temp(id, b));
    if people_h > 0.0 {
        people(ui, core, users, people_rect, acts)
    } else {
        HashMap::new()
    }
}

/// Where the middle of the view has to be, zooming from `from` to `to`
/// times the fitted size `base`, to keep the point of the slide under `p`
/// where it is.
fn zoom_at(center: Vec2, p: egui::Pos2, area: Rect, base: Vec2, from: f32, to: f32) -> Vec2 {
    let off = p - area.center();
    let under = center + vec2(off.x / (base.x * from), off.y / (base.y * from));
    under - vec2(off.x / (base.x * to), off.y / (base.y * to))
}

/// Keeps a zoomed slide over the whole view: a side that fits stays in the
/// middle, one that does not stops at its edges.
fn clamp_center(center: Vec2, view: Vec2, shown: Vec2) -> Vec2 {
    let axis = |c: f32, view: f32, shown: f32| {
        let half = view / shown.max(1.0) / 2.0;
        if half >= 0.5 {
            0.5
        } else {
            c.clamp(half, 1.0 - half)
        }
    };
    vec2(
        axis(center.x, view.x, shown.x),
        axis(center.y, view.y, shown.y),
    )
}

/// The presenter, the file, and the buttons to go back to the presenter's
/// slide and to the people's tiles. True when "Voltar ao apresentador"
/// was clicked.
fn viewer_bar(
    ui: &mut Ui,
    core: &Core,
    v: &View,
    uid: i64,
    away: bool,
    rect: Rect,
    cid: i64,
) -> bool {
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    ui.spacing_mut().item_spacing.x = 8.0;
    let user = core.user(uid);
    let (avatar, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
    widgets::paint_avatar(&ui, core, &user, avatar, false, BG2);
    ui.label(
        RichText::new(tf!("slides.presenting_by", name = user.name()))
            .font(theme::bold(14.0))
            .color(TEXT),
    );
    let mut back = false;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        // Stop watching this presentation (go back to tiles).
        if Button::danger(t!("stage.stop_watching"))
            .small()
            .show(ui)
            .clicked()
        {
            set_watching(ui.ctx(), None);
        }
        if Button::ghost(t!("slides.view_people"))
            .small()
            .show(ui)
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp(set_aside(cid), v.id));
        }
        // Fullscreen, the same way as the call view's (stage.rs).
        let fullscreen = super::stage::is_fullscreen(ui.ctx());
        let (fs_icon, fs_tip) = if fullscreen {
            (Icon::ZoomOut, t!("stage.exit_fullscreen"))
        } else {
            (Icon::Expand, t!("stage.fullscreen"))
        };
        if widgets::icon_button(ui, fs_icon, 28.0, Ib::Bare, fs_tip).clicked() {
            super::stage::set_fullscreen(ui.ctx(), !fullscreen);
        }
        if away {
            back = Button::primary(t!("slides.back_to_presenter"))
                .small()
                .show(ui)
                .clicked();
            ui.label(
                RichText::new(tf!("slides.presenter_on_slide", n = v.at + 1))
                    .font(theme::small())
                    .color(WARN),
            );
        }
        widgets::ellipsis(ui, &v.name, theme::small(), MUTED);
    });
    back
}

/// The buttons over the slide while the pointer is on it: the slides,
/// and the zoom.
fn controls(ui: &mut Ui, v: &View, area: Rect, at: usize, b: &mut Browse) {
    let side = 32.0;
    let pill = Rect::from_center_size(
        pos2(area.center().x, area.bottom() - 34.0),
        vec2(330.0_f32.min(area.width() - 16.0).max(0.0), side + 12.0),
    );
    ui.painter().rect_filled(
        pill,
        CornerRadius::same(((side + 12.0) / 2.0) as u8),
        Color32::from_black_alpha(0xc8),
    );
    let left = sane(Rect::from_min_max(
        pill.min + vec2(8.0, 0.0),
        pos2(pill.center().x + 40.0, pill.max.y),
    ));
    if let Some(to) = steps(ui, left, at, v.count, side, Ib::Bare) {
        b.at = Some(to);
    }
    let right = sane(Rect::from_min_max(
        pos2(left.right(), pill.top()),
        pill.max - vec2(8.0, 0.0),
    ));
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(right)
            .layout(Layout::right_to_left(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 4.0;
    let zoom_in =
        widgets::icon_button(&mut row, Icon::ZoomIn, side, Ib::Bare, t!("slides.zoom_in"));
    if zoom_in.clicked() {
        zoom_in.surrender_focus();
        b.zoom = (b.zoom * 1.25).min(MAX_ZOOM);
    }
    let percent = row
        .add(
            egui::Label::new(
                RichText::new(format!("{:.0}%", b.zoom * 100.0))
                    .font(theme::small())
                    .color(TEXT),
            )
            .sense(Sense::click())
            .selectable(false),
        )
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(t!("slides.fit_to_screen"));
    if percent.clicked() {
        b.zoom = 1.0;
    }
    let zoom_out = widgets::icon_button(
        &mut row,
        Icon::ZoomOut,
        side,
        Ib::Bare,
        t!("slides.zoom_out"),
    );
    if zoom_out.clicked() {
        zoom_out.surrender_focus();
        b.zoom = (b.zoom / 1.25).max(1.0);
    }
}

/// The people of the call in a row of small tiles, under the slide. Their
/// cameras come in the small copy.
fn people(
    ui: &mut Ui,
    core: &Core,
    users: &[VoiceUser],
    rect: Rect,
    acts: &mut Acts,
) -> HashMap<i64, Layer> {
    let h = rect.height();
    let w = h * 16.0 / 9.0;
    let total = users.len() as f32 * (w + GAP) - GAP;
    let me = core.me_id();
    let mut wanted = HashMap::new();
    let mut area = ui.new_child(UiBuilder::new().max_rect(rect));
    ScrollArea::horizontal()
        .id_salt("slides-people")
        .auto_shrink([false, false])
        .show(&mut area, |ui| {
            let origin = ui.cursor().min + vec2(((rect.width() - total) / 2.0).max(0.0), 0.0);
            for (i, u) in users.iter().enumerate() {
                let r = Rect::from_min_size(origin + vec2(i as f32 * (w + GAP), 0.0), vec2(w, h));
                // A screen I stopped watching is not asked for here either.
                let theirs = u.user_id != me
                    && u.video.is_some()
                    && (u.video != Some(Video::Screen) || core.voice.watching(u.user_id))
                    && ui.is_rect_visible(r)
                    && wanted.len() < plan::MAX_VIDEOS;
                if theirs {
                    wanted.insert(u.user_id, Layer::Low);
                }
                let click = super::stage::tile(ui, core, u, r, u.user_id == me || theirs);
                if let Some(at) = click.menu {
                    super::stage::open_person(acts, u.user_id, at);
                }
            }
            ui.allocate_space(vec2(total.max(rect.width()), h));
        });
    wanted
}

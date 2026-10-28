//! Annotating what is shared in a call (ink/): the strokes over the
//! picture, the button that starts annotating it, and while annotating, the
//! drawing and the toolbar across the top of the picture. The tools are
//! what Zoom, Teams and Meet have in common: disappearing ink, the pen, the
//! highlighter, an arrow, a rectangle and an ellipse, the eraser, colours
//! and widths, undo and redo, clearing, who may annotate (for the person
//! sharing), and saving a picture of it all.

use egui::{
    Align, Color32, CornerRadius, CursorIcon, Id, Key, KeyboardShortcut, Layout, Modifiers,
    PointerButton, Pos2, Rect, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2,
};

use super::Acts;
use super::widgets::{self, Ib};
use crate::core::{Core, Pop, PopKind};
use crate::icons::Icon;
use crate::ink::{self, COLORS, Ink, Page, Pen, SIZES, Surface, Tool};
use crate::t;
use crate::theme::{self, ACCENT, MUTED, TEXT};

/// The tools, in the toolbar's order.
const TOOLS: [(Tool, Icon); 8] = [
    (Tool::Vanish, Icon::Vanish),
    (Tool::Pen, Icon::Edit),
    (Tool::Highlighter, Icon::Marker),
    (Tool::Arrow, Icon::Arrow),
    (Tool::Rect, Icon::Square),
    (Tool::Ellipse, Icon::Circle),
    (Tool::Text, Icon::Text),
    (Tool::Eraser, Icon::Eraser),
];

/// The toolbar's buttons, and the icons in them.
const SIDE: f32 = 30.0;
const ICON: f32 = 17.0;

/// How near a stroke (in points) the eraser takes it.
const ERASER_REACH: f32 = 8.0;

/// What a pop of the colours shows them as.
const SWATCH: f32 = 30.0;

/// Over `picture`, where the picture of `page` shows, within `clip` (what
/// of it is on screen): its strokes. When `interactive`, the button that
/// starts annotating it, or, while I annotate it, the drawing and the
/// toolbar.
pub fn over(
    ui: &mut Ui,
    core: &Core,
    page: Page,
    picture: Rect,
    clip: Rect,
    interactive: bool,
    acts: &mut Acts,
) {
    let ink = core.voice.ink();
    let clip = clip.intersect(ui.clip_rect());
    if clip.width() <= 0.0 || clip.height() <= 0.0 {
        return;
    }
    // A picture of it being taken: the strokes without the names, and none
    // of the buttons.
    let hidden = ink.tools_hidden();
    ink::paint::paint(&ui.painter_at(clip), ink, page, picture, |uid| {
        if hidden {
            String::new()
        } else {
            core.user(uid).name().to_owned()
        }
    });
    if !interactive || !ink.may_annotate(page.surface) {
        return;
    }
    if ink.active() == Some(page.surface) {
        draw(ui, ink, page, picture, clip);
        if !hidden {
            toolbar(ui, ink, page.surface, clip, acts);
        }
        keys(ui, ink);
    } else if !hidden && ui.rect_contains_pointer(clip) {
        start(ui, ink, page.surface, clip);
    }
}

/// Where a point of the screen is on a picture shown at `picture` (0 to 1).
fn to_page(picture: Rect, p: Pos2) -> Pos2 {
    pos2(
        (p.x - picture.left()) / picture.width().max(1.0),
        (p.y - picture.top()) / picture.height().max(1.0),
    )
}

/// The pointer draws (or erases) over the picture.
fn draw(ui: &mut Ui, ink: &Ink, page: Page, picture: Rect, clip: Rect) {
    let area = picture.intersect(clip);
    if area.width() <= 0.0 || area.height() <= 0.0 {
        return;
    }
    let id = Id::unique(("ink-draw", page.surface.owner, page.surface.id));
    let tool = ink.pen().tool;
    if tool == Tool::Text {
        text_input(ui, ink, page, picture, clip);
        return;
    }
    let resp = ui
        .interact(area, id, Sense::click_and_drag())
        .on_hover_cursor(if tool == Tool::Eraser {
            CursorIcon::Cell
        } else {
            CursorIcon::Crosshair
        });
    crate::smoke::target(|| "anotacao".into(), area);
    let size = picture.size();
    let last_id = id.with("last");
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let origin = ui.input(|i| i.pointer.press_origin());
    let started = resp.drag_started_by(PointerButton::Primary);
    let dragging = resp.dragged_by(PointerButton::Primary);
    let clicked = resp.clicked_by(PointerButton::Primary);
    if tool == Tool::Eraser {
        if started || clicked {
            ink.begin_erase();
            if let Some(at) = origin.or(pointer) {
                ink.erase(page, to_page(picture, at), size, ERASER_REACH);
                ui.data_mut(|d| d.insert_temp(last_id, at));
            }
        }
        if dragging && let Some(at) = pointer {
            // All along the way from the last point: a quick stroke of the
            // eraser takes what it passed over.
            let from = ui.data(|d| d.get_temp::<Pos2>(last_id)).unwrap_or(at);
            let steps = ((at - from).length() / 4.0).ceil().max(1.0) as usize;
            for k in 1..=steps {
                let q = from + (at - from) * (k as f32 / steps as f32);
                ink.erase(page, to_page(picture, q), size, ERASER_REACH);
            }
            ui.data_mut(|d| d.insert_temp(last_id, at));
        }
        if resp.drag_stopped() || clicked {
            ink.end_erase();
        }
        return;
    }
    if started
        && let Some(at) = origin.or(pointer)
        && ink.begin(page, to_page(picture, at))
    {
        ui.data_mut(|d| d.insert_temp(last_id, at));
    }
    if dragging && let Some(at) = pointer {
        let last = ui.data(|d| d.get_temp::<Pos2>(last_id));
        if last.is_none_or(|l| (at - l).length() >= 1.0) {
            if let Some(wait) = ink.extend(to_page(picture, at)) {
                ui.ctx().request_repaint_after(wait);
            }
            ui.data_mut(|d| d.insert_temp(last_id, at));
        }
    }
    if let Some(wait) = ink.tick() {
        ui.ctx().request_repaint_after(wait);
    }
    if resp.drag_stopped() {
        ink.finish();
    }
    // A click with a pen: a dot.
    if clicked
        && !tool.shape()
        && let Some(at) = pointer
        && ink.begin(page, to_page(picture, at))
    {
        ink.finish();
    }
}

/// What the text tool shows while someone types a text annotation: where
/// the anchor is (0 to 1 across and down the picture) and what has been
/// typed so far. Kept in egui's own frame data (`ui.data`) so the input
/// stays open across frames while the user types.
#[derive(Clone, Default)]
struct TextDraft {
    at: Pos2,
    text: String,
}

/// The text tool: a click places the anchor, a text box opens there,
/// Enter commits the typed text as a stroke and Esc (or clicking out)
/// cancels. `page` and `picture` are the page being annotated and where
/// its picture is on screen.
fn text_input(ui: &mut Ui, ink: &Ink, page: Page, picture: Rect, clip: Rect) {
    let area = picture.intersect(clip);
    let id = Id::unique(("ink-text", page.surface.owner, page.surface.id, page.n));
    let resp = ui
        .interact(area, id, Sense::click())
        .on_hover_cursor(CursorIcon::Text);
    crate::smoke::target(|| "anotacao".into(), area);
    let mut draft: TextDraft = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let open = ui.data(|d| d.get_temp::<bool>(id.with("open")).unwrap_or(false));
    let clicked = resp.clicked_by(PointerButton::Primary);
    let pointer = ui.input(|i| i.pointer.interact_pos());
    if clicked && let Some(at) = pointer {
        // Clicking somewhere new while a box is open commits the previous
        // text first (Enter is not the only way to confirm).
        if open && !draft.text.trim().is_empty() {
            ink.insert_text(page, draft.at, draft.text.trim());
        }
        draft = TextDraft {
            at: to_page(picture, at),
            text: String::new(),
        };
        ui.data_mut(|d| {
            d.insert_temp(id, draft.clone());
            d.insert_temp(id.with("open"), true);
            // Force the new box to grab focus on its first frame.
            d.remove::<bool>(id.with("edit"));
        });
    }
    // Nothing to show until a click has placed a box.
    if !open && !clicked {
        return;
    }
    // The input, laid at the anchor over the picture.
    let pen = ink.pen();
    let anchor = pos2(
        picture.left() + draft.at.x * picture.width(),
        picture.top() + draft.at.y * picture.height(),
    );
    let font_px = (crate::ink::TEXT_SIZES[pen.size] * picture.height()).max(14.0);
    let width = (picture.width() * 0.5).clamp(140.0, 480.0);
    let input_rect = Rect::from_min_size(anchor, vec2(width, font_px * 1.4 + 10.0));
    let edit_id = id.with("edit");
    let first_frame = ui.data(|d| !d.get_temp::<bool>(edit_id).unwrap_or(false));
    // Escape before anything is typed: the big-area click handler above
    // would re-open an empty box, so read it here and bail.
    let escape = ui.input(|i| i.key_pressed(Key::Escape));
    let resp = ui
        .scope_builder(UiBuilder::new().max_rect(input_rect), |ui| {
            ui.painter().rect(
                input_rect,
                CornerRadius::same(6),
                Color32::from_black_alpha(0xd0),
                Stroke::new(1.0, color_from_slot(pen.color)),
                egui::StrokeKind::Inside,
            );
            let inner = input_rect.shrink(5.0);
            // Single line, not multiline: a multiline edit swallows the
            // Enter key (it inserts a newline), so the commit-on-Enter
            // below would never see it. Single line reports `lost_focus`
            // when Enter is pressed and leaves the text as typed.
            let text_edit = egui::TextEdit::singleline(&mut draft.text)
                .id(edit_id)
                .margin(egui::Margin::symmetric(4, 3))
                .font(egui::FontId::proportional(font_px))
                .text_color(color_from_slot(pen.color))
                .background_color(Color32::TRANSPARENT)
                .char_limit(crate::ink::MAX_TEXT_BYTES)
                .desired_width(inner.width())
                .hint_text(egui::RichText::new(t!("ink.text_hint")).color(MUTED));
            ui.put(inner, text_edit)
        })
        .inner;
    // Keep the box focused from the frame it opens; `ui.put` returns the
    // response, and the edit keeps focus across frames once it has it.
    if first_frame {
        resp.request_focus();
        ui.data_mut(|d| d.insert_temp(edit_id, true));
    }
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    // Clicking somewhere outside the box also ends the edit: commit what
    // is there (unless it was an Escape).
    let clicked_away = resp.lost_focus() && !enter && !escape;
    if escape {
        end_text_draft(ui, id, edit_id);
        return;
    }
    if enter || clicked_away {
        if !draft.text.trim().is_empty() {
            ink.insert_text(page, draft.at, draft.text.trim());
        }
        end_text_draft(ui, id, edit_id);
        return;
    }
    ui.data_mut(|d| d.insert_temp(id, draft));
}

/// Clears the open text draft, the "box is open" flag and the "has focus"
/// marker, so the next click starts a fresh box.
fn end_text_draft(ui: &Ui, id: Id, edit_id: Id) {
    ui.data_mut(|d| {
        d.remove::<TextDraft>(id);
        d.remove::<bool>(id.with("open"));
        d.remove::<bool>(edit_id);
    });
}

/// The RGBA colour of slot `i` in [`crate::ink::COLORS`], as a Color32.
fn color_from_slot(i: usize) -> Color32 {
    let [r, g, b] = crate::ink::COLORS[i.min(crate::ink::COLORS.len() - 1)];
    Color32::from_rgb(r, g, b)
}

/// Ctrl+Z and Ctrl+Y (or Ctrl+Shift+Z), as in Zoom.
fn keys(ui: &Ui, ink: &Ink) {
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let (redo, undo) = ui.ctx().input_mut(|i| {
        let redo = i.consume_shortcut(&KeyboardShortcut::new(
            Modifiers::COMMAND | Modifiers::SHIFT,
            Key::Z,
        )) || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Y));
        let undo = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Z));
        (redo, undo)
    });
    if redo {
        ink.redo();
    } else if undo {
        ink.undo();
    }
}

/// "Anotar", at the top right of the picture while the pointer is on it.
fn start(ui: &mut Ui, ink: &Ink, surface: Surface, clip: Rect) {
    let galley =
        ui.painter()
            .layout_no_wrap(t!("ink.annotate").to_owned(), theme::bold(13.0), TEXT);
    let size = vec2(galley.size().x + 44.0, 30.0);
    let rect = Rect::from_min_size(pos2(clip.right() - size.x - 10.0, clip.top() + 10.0), size);
    let resp = ui.interact(
        rect,
        Id::unique(("ink-start", surface.owner, surface.id)),
        Sense::click(),
    );
    crate::smoke::target(|| "anotar".into(), rect);
    let bg = Color32::from_black_alpha(if resp.hovered() { 0xf0 } else { 0xc0 });
    ui.painter().rect_filled(rect, CornerRadius::same(15), bg);
    Icon::Edit.image(16.0, TEXT).paint_at(
        ui,
        Rect::from_center_size(pos2(rect.left() + 19.0, rect.center().y), Vec2::splat(16.0)),
    );
    ui.painter().galley(
        pos2(rect.left() + 33.0, rect.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
    if resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(t!("ink.annotate_tip"))
        .clicked()
    {
        ink.open(surface);
    }
}

/// A thin line between the toolbar's groups.
fn separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, SIDE), Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range().shrink(6.0),
        Stroke::new(1.0, Color32::from_white_alpha(0x30)),
    );
}

/// A button of the toolbar that may be off for now.
fn button(ui: &mut Ui, icon: Icon, style: Ib, tip: &str, enabled: bool) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        if !enabled {
            ui.set_opacity(0.35);
        }
        widgets::icon_button_sized(ui, icon, SIDE, ICON, style, tip).clicked()
    })
    .inner
}

/// The tools, across the top of the picture.
fn toolbar(ui: &mut Ui, ink: &Ink, surface: Surface, clip: Rect, acts: &mut Acts) {
    let owner = ink.owns(surface);
    let pen = ink.pen();
    let buttons = TOOLS.len() + 1 + 2 + 1 + usize::from(owner) + 2;
    let width = buttons as f32 * (SIDE + 2.0) + 3.0 * 11.0 + 12.0;
    let height = SIDE + 10.0;
    let bar = Rect::from_center_size(
        pos2(clip.center().x, clip.top() + 10.0 + height / 2.0),
        vec2(width.min(clip.width() - 8.0).max(0.0), height),
    );
    // Clicks on the bar itself are not drawing.
    ui.interact(
        bar,
        Id::unique(("ink-bar", surface.owner, surface.id)),
        Sense::click(),
    );
    ui.painter().rect_filled(
        bar,
        CornerRadius::same((height / 2.0) as u8),
        Color32::from_black_alpha(0xd8),
    );
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(bar.shrink2(vec2(6.0, 5.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 2.0;
    for (tool, icon) in TOOLS {
        let style = if pen.tool == tool { Ib::On } else { Ib::Bare };
        if widgets::icon_button_sized(&mut row, icon, SIDE, ICON, style, tool.name()).clicked() {
            ink.set_pen(Pen { tool, ..pen });
        }
    }
    separator(&mut row);
    // The colour and the width, in a menu.
    let (swatch, resp) = row.allocate_exact_size(Vec2::splat(SIDE), Sense::click());
    if resp.hovered() {
        row.painter()
            .rect_filled(swatch, CornerRadius::same(theme::RADIUS_MD), theme::BG3);
    }
    let [r, g, b] = COLORS[pen.color];
    row.painter().circle(
        swatch.center(),
        7.0 + pen.size as f32 * 1.5,
        Color32::from_rgb(r, g, b),
        Stroke::new(1.5, Color32::from_white_alpha(0x90)),
    );
    if resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(t!("ink.color_width"))
        .clicked()
    {
        toggle_pop(acts, PopKind::InkColor, swatch);
    }
    separator(&mut row);
    if button(
        &mut row,
        Icon::Undo,
        Ib::Bare,
        t!("ink.undo"),
        ink.can_undo(),
    ) {
        ink.undo();
    }
    if button(
        &mut row,
        Icon::Redo,
        Ib::Bare,
        t!("ink.redo"),
        ink.can_redo(),
    ) {
        ink.redo();
    }
    separator(&mut row);
    let (mine, any) = ink.has_strokes(surface);
    if owner {
        let at = row.cursor().min;
        if button(&mut row, Icon::Trash, Ib::Bare, t!("ink.clear"), any) {
            toggle_pop(
                acts,
                PopKind::InkClear(surface),
                Rect::from_min_size(at, Vec2::splat(SIDE)),
            );
        }
        let at = row.cursor().min;
        let who = if ink.everyone(surface) {
            Icon::Users
        } else {
            Icon::UsersOff
        };
        if button(&mut row, who, Ib::Bare, t!("ink.who_can"), true) {
            toggle_pop(
                acts,
                PopKind::InkWho(surface),
                Rect::from_min_size(at, Vec2::splat(SIDE)),
            );
        }
    } else if button(
        &mut row,
        Icon::Trash,
        Ib::Bare,
        t!("ink.clear_mine_tip"),
        mine,
    ) {
        ink.clear_surface(surface, false);
    }
    if button(&mut row, Icon::Save, Ib::Bare, t!("ink.save_image"), true) {
        acts.later(move |c| c.save_annotation(clip));
    }
    if button(&mut row, Icon::X, Ib::Bare, t!("ink.stop"), true) {
        ink.close();
    }
}

/// Opens one of the toolbar's menus under `anchor`, or closes it when it
/// is the one open.
fn toggle_pop(acts: &mut Acts, kind: PopKind, anchor: Rect) {
    acts.later(move |c| {
        c.pop = if c.pop.is_some_and(|p| p.kind == kind) {
            None
        } else {
            Some(Pop { kind, anchor })
        };
    });
}

/// The menus' sizes, for where they open.
pub const COLOR_POP: Vec2 = vec2(4.0 * (SWATCH + 6.0) + 10.0, 168.0);

/// The colours and the widths.
pub fn color_pop(ui: &mut Ui, core: &Core, acts: &mut Acts) {
    let ink = core.voice.ink();
    let pen = ink.pen();
    // A colour or a width chosen for the eraser is chosen for the pen.
    let drawing = if pen.tool == Tool::Eraser {
        Tool::Pen
    } else {
        pen.tool
    };
    widgets::caption(ui, t!("ink.color"));
    ui.add_space(4.0);
    for row in COLORS.chunks(4).enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (k, [r, g, b]) in row.1.iter().enumerate() {
                let i = row.0 * 4 + k;
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(SWATCH), Sense::click());
                crate::smoke::target(|| format!("cor:{i}"), rect);
                let ring = if i == pen.color {
                    Stroke::new(2.0, ACCENT)
                } else if resp.hovered() {
                    Stroke::new(2.0, MUTED)
                } else {
                    Stroke::new(1.0, Color32::from_white_alpha(0x40))
                };
                ui.painter().circle(
                    rect.center(),
                    SWATCH / 2.0 - 3.0,
                    Color32::from_rgb(*r, *g, *b),
                    ring,
                );
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    ink.set_pen(Pen {
                        tool: drawing,
                        color: i,
                        ..pen
                    });
                    acts.later(|c| c.pop = None);
                }
            }
        });
    }
    ui.add_space(8.0);
    widgets::caption(ui, t!("ink.thickness"));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (i, label) in [t!("ink.thin"), t!("ink.medium"), t!("ink.thick")]
            .into_iter()
            .enumerate()
        {
            let (rect, resp) = ui.allocate_exact_size(vec2(SWATCH + 10.0, SWATCH), Sense::click());
            let bg = if i == pen.size {
                theme::BG4
            } else if resp.hovered() {
                theme::BG3
            } else {
                Color32::TRANSPARENT
            };
            ui.painter()
                .rect_filled(rect, CornerRadius::same(theme::RADIUS_SM), bg);
            let [r, g, b] = COLORS[pen.color];
            ui.painter().circle_filled(
                rect.center(),
                2.5 + i as f32 * 2.5,
                Color32::from_rgb(r, g, b),
            );
            if resp
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text(label)
                .clicked()
            {
                ink.set_pen(Pen {
                    tool: drawing,
                    size: i.min(SIZES.len() - 1),
                    ..pen
                });
                acts.later(|c| c.pop = None);
            }
        }
    });
}

/// Clearing, for the person sharing: theirs, or everyone's.
pub fn clear_pop(ui: &mut Ui, core: &Core, surface: Surface, acts: &mut Acts) {
    let ink = core.voice.ink();
    let (mine, _) = ink.has_strokes(surface);
    if mine && super::modals::menu_item(ui, t!("ink.clear_mine"), false) {
        ink.clear_surface(surface, false);
        acts.later(|c| c.pop = None);
    }
    if super::modals::menu_item(ui, t!("ink.clear_all"), true) {
        ink.clear_surface(surface, true);
        acts.later(|c| c.pop = None);
    }
}

/// Who may annotate what I share: everyone, or only me.
pub fn who_pop(ui: &mut Ui, core: &Core, surface: Surface, acts: &mut Acts) {
    let ink = core.voice.ink();
    let everyone = ink.everyone(surface);
    if super::modals::menu_check(ui, t!("ink.everyone"), everyone) {
        ink.set_everyone(surface, true);
        acts.later(|c| c.pop = None);
    }
    if super::modals::menu_check(ui, t!("ink.only_me"), !everyone) {
        ink.set_everyone(surface, false);
        acts.later(|c| c.pop = None);
    }
}

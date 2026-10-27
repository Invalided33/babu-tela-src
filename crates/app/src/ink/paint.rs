//! The strokes over a picture in the interface, and the name of whoever is
//! drawing next to their stroke.

use std::time::Instant;

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke as Line, pos2, vec2};

use super::{Ink, Look, Page, Tool, lines};

/// Paints the strokes on `page` over the picture shown at `rect` (the
/// painter clips them to what is on screen). `name` says who someone is.
/// When something changes by itself (ink fading, a name hiding), the next
/// frame is asked for then.
pub fn paint(painter: &Painter, ink: &Ink, page: Page, rect: Rect, name: impl Fn(i64) -> String) {
    let now = Instant::now();
    let (looks, next) = ink.looks(page, now);
    if let Some(at) = next {
        painter
            .ctx()
            .request_repaint_after(at.saturating_duration_since(now));
    }
    if looks.is_empty() || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let map = |p: Pos2| {
        pos2(
            rect.left() + p.x * rect.width(),
            rect.top() + p.y * rect.height(),
        )
    };
    for l in &looks {
        stroke(painter, l, map, rect.height());
    }
    // The names over every stroke.
    for l in looks.iter().filter(|l| l.tag) {
        tag(painter, l, map, &name(l.author));
    }
}

fn color(l: &Look) -> Color32 {
    let [r, g, b, a] = l.color;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn stroke(painter: &Painter, l: &Look, map: impl Fn(Pos2) -> Pos2, height: f32) {
    let c = color(l);
    if c.a() == 0 {
        return;
    }
    if l.tool == Tool::Text {
        text_stroke(painter, l, &map, height, c);
        return;
    }
    let width = (l.width * height).max(1.0);
    // Round ends for the pens, while they are opaque (where an end
    // overlaps the line, a see-through one would show darker).
    let caps = matches!(l.tool, Tool::Vanish | Tool::Pen) && l.color[3] >= 250;
    for line in lines(l.tool, &l.points, l.width, &map, height) {
        match line.as_slice() {
            [] => {}
            [only] => {
                painter.circle_filled(*only, width / 2.0, c);
            }
            [first, .., last] => {
                let (first, last) = (*first, *last);
                painter.add(Shape::line(line, Line::new(width, c)));
                if caps {
                    painter.circle_filled(first, width / 2.0, c);
                    painter.circle_filled(last, width / 2.0, c);
                }
            }
        }
    }
}

/// Draws the words of a `Tool::Text` stroke at the first point of `l`,
/// at the chosen font size (`l.width` is the font size as a fraction of
/// the picture's height).
fn text_stroke(painter: &Painter, l: &Look, map: &impl Fn(Pos2) -> Pos2, height: f32, c: Color32) {
    let (Some(text), Some(anchor)) = (l.text.as_deref(), l.points.first()) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let px = (l.width * height).max(10.0);
    let at = map(*anchor);
    painter.text(
        at,
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::proportional(px),
        c,
    );
}

/// The author's name, on their colour, next to where the stroke is now.
fn tag(painter: &Painter, l: &Look, map: impl Fn(Pos2) -> Pos2, name: &str) {
    let Some(last) = l.points.last() else {
        return;
    };
    let [r, g, b, a] = l.color;
    if a == 0 || name.is_empty() {
        return;
    }
    let light = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b) > 160.0;
    let text = if light {
        Color32::from_rgb(0x1c, 0x1c, 0x1e)
    } else {
        Color32::WHITE
    };
    let galley = painter.layout_no_wrap(name.to_owned(), crate::theme::small(), text);
    let at = map(*last) + vec2(12.0, 10.0);
    let rect = Rect::from_min_size(at, galley.size() + vec2(12.0, 6.0));
    painter.rect_filled(
        rect,
        CornerRadius::same(6),
        Color32::from_rgba_unmultiplied(r, g, b, a.min(235)),
    );
    painter.galley(rect.min + vec2(6.0, 3.0), galley, text);
}

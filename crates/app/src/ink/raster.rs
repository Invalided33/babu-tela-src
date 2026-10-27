//! The strokes drawn into a bitmap on the processor (tiny-skia), for the
//! window over my own shared screen (overlay.rs), which Windows composes
//! from a bitmap rather than from the interface's frames.

use egui::{Pos2, pos2};
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, PixmapMut, Stroke, Transform};

use super::{Look, lines};

/// Draws `looks` over `pixmap`, which shows the whole picture. `bgra`: the
/// bitmap is in Windows' order (blue first), as a layered window takes it;
/// tiny-skia writes red first, so red and blue trade places on the way in.
pub fn draw(pixmap: &mut PixmapMut, looks: &[Look], bgra: bool) {
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    let map = |p: Pos2| pos2(p.x * w, p.y * h);
    for l in looks {
        let [r, g, b, a] = l.color;
        if a == 0 {
            continue;
        }
        let (r, b) = if bgra { (b, r) } else { (r, b) };
        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, a);
        paint.anti_alias = true;
        let width = (l.width * h).max(1.0);
        let stroke = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        for line in lines(l.tool, &l.points, l.width, map, h) {
            match line.as_slice() {
                [] => {}
                [only] => {
                    if let Some(dot) = PathBuilder::from_circle(only.x, only.y, width / 2.0) {
                        pixmap.fill_path(
                            &dot,
                            &paint,
                            FillRule::Winding,
                            Transform::identity(),
                            None,
                        );
                    }
                }
                [first, rest @ ..] => {
                    let mut pb = PathBuilder::new();
                    pb.move_to(first.x, first.y);
                    for p in rest {
                        pb.line_to(p.x, p.y);
                    }
                    if let Some(path) = pb.finish() {
                        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                    }
                }
            }
        }
    }
}

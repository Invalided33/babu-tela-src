//! The member list at the right of a server: online first, then offline.

use egui::{Align2, CornerRadius, CursorIcon, Id, Rect, ScrollArea, Sense, Ui, Vec2, pos2, vec2};

use super::Acts;
use super::widgets;
use crate::core::{Core, Modal};
use crate::model::*;
use crate::tf;
use crate::theme::{self, *};

pub fn draw(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Some(s) = core.current_server() else {
        return;
    };
    let (on, off): (Vec<&User>, Vec<&User>) = s
        .members
        .iter()
        .partition(|m| m.status.public() != Status::Offline);
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add_space(6.0);
            widgets::caption(ui, &tf!("member.online", n = on.len()));
            ui.add_space(4.0);
            for m in on {
                row(ui, core, s, m, false, acts);
            }
            if !off.is_empty() {
                ui.add_space(12.0);
                widgets::caption(ui, &tf!("member.offline", n = off.len()));
                ui.add_space(4.0);
                for m in off {
                    row(ui, core, s, m, true, acts);
                }
            }
        });
}

fn row(ui: &mut Ui, core: &Core, s: &Server, m: &User, offline: bool, acts: &mut Acts) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::hover());
    let resp = ui
        .interact(rect, Id::unique(("member", m.id)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(RADIUS_SM), BG2);
    }
    let dim = if offline { 0.45 } else { 1.0 };
    let av = Rect::from_center_size(
        pos2(rect.left() + 8.0 + 16.0, rect.center().y),
        Vec2::splat(32.0),
    );
    // Offline rows are faded, avatar included.
    if offline {
        ui.scope(|ui| {
            ui.set_opacity(dim);
            widgets::paint_avatar(ui, core, m, av, true, BG1);
        });
    } else {
        widgets::paint_avatar(ui, core, m, av, true, BG1);
    }
    let color = s
        .role_color(m)
        .and_then(theme::parse_hex)
        .unwrap_or(TEXT)
        .gamma_multiply(dim);
    let mut right = rect.right() - 8.0;
    if m.id == s.owner_id {
        ui.painter().text(
            pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            "★",
            egui::FontId::proportional(12.0),
            WARN.gamma_multiply(dim),
        );
        right -= 16.0;
    }
    let mut job =
        egui::text::LayoutJob::simple_singleline(m.name().to_owned(), theme::bold(14.0), color);
    job.wrap = egui::text::TextWrapping::truncate_at_width((right - av.right() - 10.0).max(10.0));
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        pos2(av.right() + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    if resp.clicked() {
        let uid = m.id;
        acts.later(move |c| c.open_modal(Modal::Profile { uid }));
    }
}

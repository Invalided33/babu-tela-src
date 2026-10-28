//! The card of a file sent directly ("Enviar direto", direct/): what it is,
//! where it comes from, and how its download goes, with its buttons. On my
//! own message, whether this PC still shares it.

use std::time::Duration;

use egui::{CornerRadius, Frame, Margin, RichText, Sense, Stroke, Ui, vec2};

use super::Acts;
use super::widgets::{self, Button};
use crate::core::{Action, Core};
use crate::direct::{self, Card, Phase, Wait};
use crate::icons::Icon;
use crate::model::{Attachment, Message, Status};
use crate::theme::{self, *};
use crate::{t, tf};

/// The widest a card gets.
const WIDTH: f32 = 460.0;

/// How often a card that moves (its bar, its speed) is drawn again.
const MOVING: Duration = Duration::from_millis(250);

pub fn card(ui: &mut Ui, core: &Core, m: &Message, a: &Attachment, acts: &mut Acts) {
    ui.add_space(4.0);
    let author = m.author_id();
    let mine = author == core.me_id();
    let who = core.user(author).name().to_owned();
    let state = core.direct.card(a, author);
    let id = a.direct.as_ref().map(|d| d.id.clone()).unwrap_or_default();
    let total = u64::try_from(a.size).unwrap_or(0);
    let width = ui.available_width().min(WIDTH);
    Frame::new()
        .fill(BG1)
        .stroke(Stroke::new(1.0, BG4))
        .corner_radius(CornerRadius::same(RADIUS_MD))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(width - 28.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.add(Icon::Direct.tinted(ACCENT));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    widgets::ellipsis(ui, &a.name, theme::body(), TEXT);
                    let from = if mine {
                        t!("direct.from_me").to_owned()
                    } else {
                        tf!("direct.from_pc", who = who)
                    };
                    line(
                        ui,
                        &tf!(
                            "direct.subtitle",
                            size = widgets::fmt_size(a.size),
                            from = from
                        ),
                        MUTED,
                    );
                });
            });
            body(ui, core, m, a, &state, &id, &who, total, acts);
        });
    if matches!(
        state,
        Card::Going {
            phase: Phase::Connecting | Phase::Queued | Phase::Receiving,
            ..
        }
    ) {
        ui.ctx().request_repaint_after(MOVING);
    }
}

fn line(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.label(RichText::new(text).size(12.0).color(color));
}

/// A file id as the confirmation dialogs carry it.
fn as_number(id: &str) -> Option<u128> {
    u128::from_str_radix(id, 16).ok()
}

#[allow(clippy::too_many_arguments)]
fn body(
    ui: &mut Ui,
    core: &Core,
    m: &Message,
    a: &Attachment,
    state: &Card,
    id: &str,
    who: &str,
    total: u64,
    acts: &mut Acts,
) {
    match state {
        Card::Broken => line(ui, t!("direct.broken"), FAINT),
        Card::Elsewhere => line(ui, t!("direct.elsewhere"), FAINT),
        Card::Stopped => line(ui, t!("direct.stopped"), FAINT),
        Card::Sharing { sending, waiting } => {
            let mut text = match sending {
                0 => t!("direct.sharing_idle").to_owned(),
                1 => t!("direct.sharing_one").to_owned(),
                n => tf!("direct.sharing_many", n = n),
            };
            if *waiting > 0 {
                text.push_str(&tf!("direct.queue", n = waiting));
            }
            line(ui, &text, MUTED);
            if let Some(n) = as_number(id)
                && Button::ghost(t!("direct.stop_sharing"))
                    .small()
                    .show(ui)
                    .clicked()
            {
                acts.later(move |c| {
                    c.confirm(
                        t!("direct.stop_title"),
                        t!("direct.stop_body"),
                        t!("direct.stop_confirm"),
                        Action::StopSharing(n),
                    );
                });
            }
        }
        Card::Offer => {
            let online = core.user(m.author_id()).status != Status::Offline;
            let text = if online {
                tf!("direct.offer_online", who = who)
            } else {
                tf!("direct.offer_offline", who = who)
            };
            line(ui, &text, MUTED);
            if Button::primary(t!("direct.download"))
                .small()
                .show(ui)
                .clicked()
            {
                let (a, msg, cid, from) = (a.clone(), m.id, m.channel_id, m.author_id());
                acts.later(move |c| c.save_direct(a, msg, cid, from));
            }
        }
        Card::Going {
            phase,
            done,
            speed,
            relayed,
        } => {
            bar(ui, *done, total, *phase == Phase::Receiving);
            line(
                ui,
                &going(*phase, *done, total, *speed, *relayed, who),
                MUTED,
            );
            ui.horizontal(|ui| {
                if matches!(phase, Phase::Waiting(_) | Phase::Busy | Phase::Retrying)
                    && Button::primary(t!("direct.try_now"))
                        .small()
                        .show(ui)
                        .clicked()
                {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.resume(&id));
                }
                if Button::ghost(t!("direct.pause")).small().show(ui).clicked() {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.pause(&id));
                }
                cancel(ui, id, acts);
            });
        }
        Card::Paused { done } => {
            bar(ui, *done, total, false);
            line(
                ui,
                &tf!(
                    "direct.paused",
                    done = widgets::fmt_size(*done as i64),
                    total = widgets::fmt_size(total as i64),
                    pct = percent(*done, total)
                ),
                MUTED,
            );
            ui.horizontal(|ui| {
                if Button::primary(t!("direct.resume"))
                    .small()
                    .show(ui)
                    .clicked()
                {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.resume(&id));
                }
                cancel(ui, id, acts);
            });
        }
        Card::Failed { why } => {
            let (text, retry) = direct::failure(why, who);
            line(ui, &text, DANGER);
            ui.horizontal(|ui| {
                if retry
                    && Button::primary(t!("direct.try_again"))
                        .small()
                        .show(ui)
                        .clicked()
                {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.resume(&id));
                }
                if Button::ghost(t!("settings.remove"))
                    .small()
                    .show(ui)
                    .clicked()
                {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.cancel(&id));
                }
            });
        }
        Card::Done { path } => {
            let place = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.add(Icon::Folder.image(crate::icons::SMALL, ONLINE));
                line(ui, &tf!("direct.downloaded", place = place), ONLINE);
            });
            ui.horizontal(|ui| {
                if Button::primary(t!("chat.open")).small().show(ui).clicked() {
                    let (id, path) = (id.to_owned(), path.clone());
                    acts.later(move |c| c.open_direct(&id, path, false));
                }
                if Button::ghost(t!("direct.show_in_folder"))
                    .small()
                    .show(ui)
                    .clicked()
                {
                    let (id, path) = (id.to_owned(), path.clone());
                    acts.later(move |c| c.open_direct(&id, path, true));
                }
            });
        }
    }
}

/// "Cancelar", which asks first: what was downloaded goes too.
fn cancel(ui: &mut Ui, id: &str, acts: &mut Acts) {
    if let Some(n) = as_number(id)
        && Button::ghost(t!("common.cancel"))
            .small()
            .show(ui)
            .clicked()
    {
        acts.later(move |c| {
            c.confirm(
                t!("direct.cancel_title"),
                t!("direct.cancel_body"),
                t!("direct.cancel_title"),
                Action::CancelDownload(n),
            );
        });
    }
}

/// How far, as a bar the width of the card.
fn bar(ui: &mut Ui, done: u64, total: u64, moving: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), BG3);
    let mut filled = rect;
    filled.set_width(rect.width() * fraction(done, total));
    let color = if moving { ACCENT } else { MUTED };
    painter.rect_filled(filled, CornerRadius::same(2), color);
}

fn fraction(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 1.0;
    }
    (done as f64 / total as f64).clamp(0.0, 1.0) as f32
}

fn percent(done: u64, total: u64) -> u32 {
    (fraction(done, total) * 100.0).floor() as u32
}

/// What a download that should be going says.
fn going(phase: Phase, done: u64, total: u64, speed: f64, relayed: bool, who: &str) -> String {
    match phase {
        Phase::Connecting | Phase::Idle => tf!("direct.connecting", who = who),
        Phase::Queued => tf!("direct.queued", who = who),
        Phase::Receiving => {
            let mut text = tf!(
                "direct.progress",
                done = widgets::fmt_size(done as i64),
                total = widgets::fmt_size(total as i64),
                pct = percent(done, total)
            );
            if speed >= 1.0 {
                text.push_str(&format!(" · {}/s", widgets::fmt_size(speed as i64)));
                let left = total.saturating_sub(done) as f64 / speed;
                text.push_str(&format!(" · {}", eta(left)));
            }
            if relayed {
                text.push_str(t!("direct.via_relay"));
            }
            text
        }
        Phase::Retrying => t!("direct.retrying").to_owned(),
        Phase::Waiting(Wait::Offline) => tf!("direct.wait_offline", who = who),
        Phase::Waiting(Wait::NoAnswer) => tf!("direct.wait_no_answer", who = who),
        Phase::Waiting(Wait::Cut) => tf!("direct.wait_cut", who = who),
        Phase::Busy => tf!("direct.busy", who = who),
    }
}

/// "faltam 5 min", from seconds.
fn eta(secs: f64) -> String {
    let s = secs.round() as u64;
    let (h, m) = (s / 3600, (s % 3600) / 60);
    let (d, one) = match (h, m) {
        (0, 0) => (format!("{s} s"), s == 1),
        (0, m) => (format!("{m} min"), m == 1),
        (h, 0) => (format!("{h} h"), h == 1),
        (h, m) => (format!("{h} h {m} min"), h == 1),
    };
    if one {
        tf!("direct.eta_one", d = d)
    } else {
        tf!("direct.eta_many", d = d)
    }
}

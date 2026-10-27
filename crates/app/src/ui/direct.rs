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
                        "do seu PC".to_owned()
                    } else {
                        format!("do PC de {who}")
                    };
                    line(
                        ui,
                        &format!("{} · direto {from}", widgets::fmt_size(a.size)),
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
        Card::Broken => line(ui, "Este anexo veio incompleto.", FAINT),
        Card::Elsewhere => line(
            ui,
            "Enviado de outro PC seu: só ele compartilha este arquivo.",
            FAINT,
        ),
        Card::Stopped => line(ui, "Você parou de compartilhar este arquivo.", FAINT),
        Card::Sharing { sending, waiting } => {
            let mut text = match sending {
                0 => "Compartilhando deste PC enquanto o app estiver aberto.".to_owned(),
                1 => "Enviando para 1 pessoa agora.".to_owned(),
                n => format!("Enviando para {n} pessoas agora."),
            };
            if *waiting > 0 {
                text.push_str(&format!(" {waiting} na fila."));
            }
            line(ui, &text, MUTED);
            if let Some(n) = as_number(id)
                && Button::ghost("Parar de compartilhar")
                    .small()
                    .show(ui)
                    .clicked()
            {
                acts.later(move |c| {
                    c.confirm(
                        "Parar de compartilhar",
                        "Quem ainda não baixou não vai conseguir baixar, e downloads em andamento param.",
                        "Parar",
                        Action::StopSharing(n),
                    );
                });
            }
        }
        Card::Offer => {
            let online = core.user(m.author_id()).status != Status::Offline;
            let text = if online {
                format!("Fica disponível enquanto {who} estiver com o app aberto.")
            } else {
                format!("{who} parece offline: o download começa quando {who} voltar.")
            };
            line(ui, &text, MUTED);
            if Button::primary("Baixar").small().show(ui).clicked() {
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
                    && Button::primary("Tentar agora").small().show(ui).clicked()
                {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.resume(&id));
                }
                if Button::ghost("Pausar").small().show(ui).clicked() {
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
                &format!(
                    "Pausado · {} de {} ({}%)",
                    widgets::fmt_size(*done as i64),
                    widgets::fmt_size(total as i64),
                    percent(*done, total)
                ),
                MUTED,
            );
            ui.horizontal(|ui| {
                if Button::primary("Continuar").small().show(ui).clicked() {
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
                if retry && Button::primary("Tentar de novo").small().show(ui).clicked() {
                    let id = id.to_owned();
                    acts.later(move |c| c.direct.resume(&id));
                }
                if Button::ghost("Remover").small().show(ui).clicked() {
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
                line(ui, &format!("Baixado em {place}."), ONLINE);
            });
            ui.horizontal(|ui| {
                if Button::primary("Abrir").small().show(ui).clicked() {
                    let (id, path) = (id.to_owned(), path.clone());
                    acts.later(move |c| c.open_direct(&id, path, false));
                }
                if Button::ghost("Mostrar na pasta").small().show(ui).clicked() {
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
        && Button::ghost("Cancelar").small().show(ui).clicked()
    {
        acts.later(move |c| {
            c.confirm(
                "Cancelar download",
                "O que já foi baixado deste arquivo vai ser apagado.",
                "Cancelar download",
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
        Phase::Connecting | Phase::Idle => format!("Conectando ao PC de {who}…"),
        Phase::Queued => format!("Na fila: {who} está enviando para outras pessoas."),
        Phase::Receiving => {
            let mut text = format!(
                "{} de {} · {}%",
                widgets::fmt_size(done as i64),
                widgets::fmt_size(total as i64),
                percent(done, total)
            );
            if speed >= 1.0 {
                text.push_str(&format!(" · {}/s", widgets::fmt_size(speed as i64)));
                let left = total.saturating_sub(done) as f64 / speed;
                text.push_str(&format!(" · {}", eta(left)));
            }
            if relayed {
                text.push_str(" · pelo relay");
            }
            text
        }
        Phase::Retrying => "A conexão caiu. Tentando de novo…".to_owned(),
        Phase::Waiting(Wait::Offline) => {
            format!("{who} está offline. Continua quando {who} voltar.")
        }
        Phase::Waiting(Wait::NoAnswer) => format!(
            "{who} não respondeu: o app precisa estar aberto no PC que enviou. Continua quando {who} voltar."
        ),
        Phase::Waiting(Wait::Cut) => {
            format!("A conexão com {who} caiu várias vezes. Continua quando {who} voltar.")
        }
        Phase::Busy => {
            format!("{who} está enviando para muita gente. Tentando de novo daqui a pouco.")
        }
    }
}

/// "faltam 5 min", from seconds.
fn eta(secs: f64) -> String {
    let s = secs.round() as u64;
    let (h, m) = (s / 3600, (s % 3600) / 60);
    match (h, m) {
        (0, 0) => format!("falta{} {s} s", if s == 1 { "" } else { "m" }),
        (0, 1) => "falta 1 min".to_owned(),
        (0, m) => format!("faltam {m} min"),
        (h, 0) => format!("falta{} {h} h", if h == 1 { "" } else { "m" }),
        (h, m) => format!("falta{} {h} h {m} min", if h == 1 { "" } else { "m" }),
    }
}

//! The card in the middle of the window: signing in (with Google, which
//! also makes the account the first time), and the waiting screens while
//! the server does not answer.

use std::time::{Duration, Instant};

use egui::{
    Align2, Area, CentralPanel, Color32, CornerRadius, Frame, Id, Key, Margin, Order, Stroke, Ui,
    pos2, vec2,
};

use super::Acts;
use super::widgets::{self, Button};
use crate::core::{Core, FormId, Gate, Phase};
use crate::theme::{self, *};

/// The indigo glow behind the card (`radial-gradient(circle at 25% 15%,
/// #211f4d, var(--bg0) 60%)`), drawn as rings of fading colour.
fn backdrop(ui: &mut Ui) {
    CentralPanel::default()
        .frame(Frame::new().fill(BG0))
        .show(ui, |ui| {
            let r = ui.max_rect();
            let center = pos2(r.left() + r.width() * 0.25, r.top() + r.height() * 0.15);
            let reach = r.width().hypot(r.height()) * 0.6;
            let glow = Color32::from_rgb(0x21, 0x1f, 0x4d);
            let steps = 64;
            for i in 0..steps {
                let t = i as f32 / steps as f32;
                let radius = reach * (1.0 - t);
                let color = theme::mix(BG0, glow, t);
                ui.painter().circle_filled(center, radius, color);
            }
        });
}

/// Draws the card centred over the window and returns what `add` returned.
fn card<R>(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let ctx = ui.ctx().clone();
    Area::new(Id::unique(id))
        .order(Order::Middle)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, theme::TITLEBAR_H / 2.0))
        .show(&ctx, |ui| {
            Frame::new()
                .fill(BG2)
                .stroke(Stroke::new(1.0, Color32::from_white_alpha(0x0d)))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::same(28))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 20],
                    blur: 60,
                    spread: 0,
                    color: Color32::from_black_alpha(0x88),
                })
                .show(ui, |ui| {
                    ui.set_width(344.0);
                    add(ui)
                })
                .inner
        })
        .inner
}

fn logo(ui: &mut Ui, centred: bool) {
    let Some(icon) = super::app_icon(ui) else {
        return;
    };
    let image = egui::Image::from_texture(egui::load::SizedTexture::from_handle(&icon))
        .fit_to_exact_size(vec2(56.0, 56.0));
    if centred {
        ui.vertical_centered(|ui| ui.add(image));
    } else {
        ui.add(image);
    }
    ui.add_space(14.0);
}

/// Signed out: one way in, Google's. The first time, the account is made
/// with the Google account's name.
pub fn sign_in(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    backdrop(ui);
    let waiting = core.google.is_some();
    let error = core.errors.get(&FormId::Login).cloned();
    // Enter starts it too, as it sent the old card's form.
    let enter = ui.input(|i| i.key_pressed(Key::Enter));
    card(ui, "auth-card", |ui| {
        logo(ui, false);
        ui.label(
            egui::RichText::new("Bem-vindo ao Babu Tela")
                .font(theme::h1())
                .color(TEXT),
        );
        ui.add_space(4.0);
        widgets::sub(
            ui,
            "Entre com sua conta do Google para continuar a conversa.",
        );
        ui.add_space(22.0);
        google_flow(ui, waiting, error.as_ref(), enter && !waiting, acts);
        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(
                "Na primeira vez, sua conta do Babu Tela é criada com o nome da sua conta do \
                 Google. Ninguém vê o seu e-mail.",
            )
            .color(FAINT)
            .size(12.5),
        );
    });
}

/// The Google button, or, while the browser is open, what to do there and
/// how to give up; the error of the last try under it.
fn google_flow(ui: &mut Ui, waiting: bool, error: Option<&String>, enter: bool, acts: &mut Acts) {
    if waiting {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.add(egui::Spinner::new().size(18.0).color(ACCENT));
            ui.label(
                egui::RichText::new("Continue no navegador: escolha sua conta do Google lá.")
                    .color(MUTED),
            );
        });
        ui.add_space(8.0);
        if widgets::link(ui, "Cancelar").clicked() {
            acts.later(Core::cancel_google);
        }
        return;
    }
    if widgets::google_button(ui, "Continuar com o Google", true).clicked() || enter {
        acts.later(Core::google_sign_in);
    }
    widgets::error_line(ui, error);
}

/// A newer version coming down as the app starts (Discord's splash screen,
/// "Downloading update"): it installs itself, and the app opens again on it.
pub fn updating(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Gate::Updating { version, progress } = &core.gate else {
        return;
    };
    let (version, sent, total, fraction) = (
        version.clone(),
        progress.sent(),
        progress.total(),
        progress.fraction(),
    );
    backdrop(ui);
    card(ui, "update-card", |ui| {
        ui.vertical_centered(|ui| {
            logo(ui, true);
            ui.label(
                egui::RichText::new("Atualizando")
                    .font(theme::h1())
                    .color(TEXT),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!(
                    "Baixando a versão {version}. O Babu Tela abre de novo sozinho quando terminar."
                ))
                .color(MUTED),
            );
            ui.add_space(20.0);
        });
        let shown = if total > 0 { fraction } else { 0.0 };
        ui.add(
            egui::ProgressBar::new(shown)
                .desired_height(8.0)
                .fill(ACCENT)
                .corner_radius(4),
        );
        ui.add_space(6.0);
        let amount = if total > 0 {
            format!(
                "{} de {}",
                widgets::fmt_size(sent as i64),
                widgets::fmt_size(total as i64)
            )
        } else {
            widgets::fmt_size(sent as i64)
        };
        ui.label(egui::RichText::new(amount).size(12.5).color(FAINT));
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            if widgets::link(ui, "Abrir agora e atualizar depois").clicked() {
                acts.later(Core::skip_update);
            }
        });
    });
    // The bar moves as the bytes arrive, with nothing else to wake it.
    ui.ctx().request_repaint_after(Duration::from_millis(100));
}

/// "Conectando…", or "Servidor fora do ar" with a countdown.
pub fn waiting(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    backdrop(ui);
    let retry = match core.phase {
        Phase::Waiting { retry } | Phase::Connecting { retry } => retry,
        _ => None,
    };
    let left = retry.map(|at| {
        at.saturating_duration_since(Instant::now())
            .as_secs_f32()
            .round() as u64
    });
    card(ui, "waiting-card", |ui| {
        ui.vertical_centered(|ui| {
            logo(ui, true);
            ui.label(
                egui::RichText::new(if retry.is_some() {
                    "Servidor fora do ar"
                } else {
                    "Conectando…"
                })
                .font(theme::h1())
                .color(TEXT),
            );
            ui.add_space(6.0);
            let text = match left {
                None => "Conectando ao servidor do Babu Tela…".to_owned(),
                Some(s) => format!(
                    "Não consegui falar com o servidor do Babu Tela. Confira sua internet; se ela \
                     estiver funcionando, o servidor pode estar passando por manutenção. Tentando de \
                     novo em {s} s."
                ),
            };
            ui.label(egui::RichText::new(text).color(MUTED));
            // Through Tor (tor.rs): how far it is into the network (each
            // change redraws, main.rs).
            if let Some(status) = crate::tor::status() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(super::modals::tor_text(&status))
                        .size(12.5)
                        .color(FAINT),
                );
            }
            ui.add_space(20.0);
        });
        if retry.is_some() && Button::primary("Tentar agora").wide().show(ui).clicked() {
            acts.later(Core::retry_now);
        }
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            if crate::tor::active() && widgets::link(ui, "Conectar sem o Tor").clicked() {
                acts.later(|c| c.set_tor(false));
            }
            if widgets::link(ui, "Sair da conta").clicked() {
                acts.later(|c| c.logout(false));
            }
        });
    });
}

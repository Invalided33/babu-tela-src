//! The dialogs of the end-to-end encryption (e2e/): a new PC getting the
//! account's keys from an old one, and the security number with someone.

use egui::{Margin, RichText, TextEdit, Ui};

use super::Acts;
use super::modals::foot;
use super::widgets::{self, Button};
use crate::core::{Core, Modal};
use crate::e2e::{PinView, keys, provision};
use crate::theme::{self, *};

/// On an old PC: a new one asks for the keys, and its code is typed here.
pub fn approve_device(
    ui: &mut Ui,
    code: &mut String,
    error: Option<&str>,
    focus: bool,
    acts: &mut Acts,
) {
    widgets::h3(ui, "Aprovar um PC novo");
    widgets::sub(
        ui,
        "Um PC entrou na sua conta e pediu as chaves para ler e enviar mensagens cifradas. \
         Digite o código que aparece nele. Se não foi você, recuse: nada é enviado sem o código certo.",
    );
    ui.add_space(12.0);
    let resp = ui.add(
        TextEdit::singleline(code)
            .hint_text("XXXX-XXXX-XXXX-XXXX")
            .char_limit(24)
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(12, 9))
            .background_color(BG0)
            .font(theme::body()),
    );
    if focus {
        resp.request_focus();
    }
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    widgets::error_line(ui, error.map(str::to_owned).as_ref());
    foot(ui, |ui| {
        if Button::primary("Aprovar").show(ui).clicked() || enter {
            acts.later(Core::approve_device);
        }
        if Button::ghost("Recusar").show(ui).clicked() {
            acts.later(Core::close_modal);
        }
    });
}

/// On a new PC: the code to type on an old one, or a new identity.
pub fn new_device(ui: &mut Ui, core: &Core, acts: &mut Acts) {
    widgets::h3(ui, "Este PC precisa das chaves da sua conta");
    widgets::sub(
        ui,
        "Suas mensagens são cifradas de ponta a ponta: só os seus PCs têm as chaves, nem o \
         servidor. Abra o Babu Tela num PC onde você já entrou e digite lá este código:",
    );
    ui.add_space(14.0);
    let code = core
        .provision
        .as_ref()
        .map_or_else(String::new, |r| provision::shown(&r.code));
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(code)
                .font(theme::bold(26.0))
                .color(TEXT)
                .extra_letter_spacing(2.0),
        );
    });
    ui.add_space(14.0);
    foot(ui, |ui| {
        if Button::primary("Pedir de novo").show(ui).clicked() {
            acts.later(Core::ask_for_keys_again);
        }
        if Button::ghost("Fechar").show(ui).clicked() {
            acts.later(Core::close_modal);
        }
    });
    ui.add_space(18.0);
    widgets::divider(ui);
    ui.add_space(10.0);
    widgets::sub(
        ui,
        "Não tem mais nenhum desses PCs? Crie uma identidade nova: o que foi enviado antes não \
         poderá ser lido aqui, e quem conversa com você verá que a sua chave mudou.",
    );
    ui.add_space(8.0);
    if Button::danger("Criar uma identidade nova")
        .show(ui)
        .clicked()
    {
        acts.later(|c| {
            c.close_modal();
            c.reset_identity();
        });
    }
}

/// The security number with `uid`, to compare with them outside the app.
pub fn number(ui: &mut Ui, core: &Core, uid: i64, acts: &mut Acts) {
    let name = core.user(uid).name().to_owned();
    widgets::h3(ui, &format!("Código de segurança com {name}"));
    let Some((now, changed)) = core.security_number(uid) else {
        widgets::sub(
            ui,
            "Este PC ainda não tem a chave dessa pessoa: ela aparece quando vocês conversarem.",
        );
        foot(ui, |ui| {
            if Button::ghost("Fechar").show(ui).clicked() {
                acts.later(Core::close_modal);
            }
        });
        return;
    };
    widgets::sub(
        ui,
        &format!(
            "Compare este número com o que aparece no app de {name}, pessoalmente ou numa \
             ligação. Se for o mesmo, ninguém, nem o servidor, está no meio da conversa de vocês."
        ),
    );
    ui.add_space(12.0);
    show_number(ui, &now);
    let view = core.e2e.pin_view(uid);
    ui.add_space(10.0);
    if let Some(new) = changed {
        ui.label(
            RichText::new(format!(
                "A chave de {name} mudou. Pode ser que {name} tenha trocado de PC sem levar as \
                 chaves, ou que alguém esteja tentando se passar por {name}. Até você aceitar, \
                 nada seu vai para a chave nova. O número novo seria:"
            ))
            .color(WARN),
        );
        ui.add_space(6.0);
        show_number(ui, &new);
        foot(ui, |ui| {
            if Button::danger("Aceitar a nova chave").show(ui).clicked() {
                acts.later(move |c| c.accept_key_change(uid));
            }
            if Button::ghost("Fechar").show(ui).clicked() {
                acts.later(Core::close_modal);
            }
        });
        return;
    }
    let verified = matches!(view, PinView::Pinned { verified: true });
    ui.label(
        RichText::new(if verified {
            "Você marcou este número como conferido."
        } else {
            "Ainda não conferido."
        })
        .color(MUTED),
    );
    if core.e2e.in_record(uid) {
        ui.label(
            RichText::new(format!(
                "É a chave mais nova de {name} no registro público de chaves, que o app de {name} \
                 vigia e que os apps de quem conversa comparam entre si."
            ))
            .color(MUTED),
        );
    }
    foot(ui, |ui| {
        let label = if verified {
            "Desmarcar"
        } else {
            "Marcar como conferido"
        };
        if Button::primary(label).show(ui).clicked() {
            acts.later(move |c| c.mark_verified(uid, !verified));
        }
        if Button::ghost("Fechar").show(ui).clicked() {
            acts.later(Core::close_modal);
        }
    });
}

fn show_number(ui: &mut Ui, number: &str) {
    let groups = keys::grouped(number);
    let words: Vec<&str> = groups.split(' ').collect();
    for line in words.chunks(4) {
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(line.join("   "))
                    .font(egui::FontId::monospace(18.0))
                    .color(TEXT),
            );
        });
    }
}

/// Opens the security number with `uid`.
pub fn open(core: &mut Core, uid: i64) {
    core.open_modal(Modal::Security { uid });
}

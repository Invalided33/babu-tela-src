//! The dialogs of the end-to-end encryption (e2e/): a new PC getting the
//! account's keys from an old one, and the security number with someone.

use egui::{Margin, RichText, TextEdit, Ui};

use super::Acts;
use super::modals::foot;
use super::widgets::{self, Button};
use crate::core::{Core, Modal};
use crate::e2e::{PinView, keys, provision};
use crate::theme::{self, *};
use crate::{t, tf};

/// On an old PC: a new one asks for the keys, and its code is typed here.
pub fn approve_device(
    ui: &mut Ui,
    code: &mut String,
    error: Option<&str>,
    focus: bool,
    acts: &mut Acts,
) {
    widgets::h3(ui, t!("security.approve_title"));
    widgets::sub(ui, t!("security.approve_body"));
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
        if Button::primary(t!("security.approve")).show(ui).clicked() || enter {
            acts.later(Core::approve_device);
        }
        if Button::ghost(t!("security.decline")).show(ui).clicked() {
            acts.later(Core::close_modal);
        }
    });
}

/// On a new PC: the code to type on an old one, or a new identity.
pub fn new_device(ui: &mut Ui, core: &Core, acts: &mut Acts) {
    widgets::h3(ui, t!("security.new_device_title"));
    widgets::sub(ui, t!("security.new_device_body"));
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
        if Button::primary(t!("security.ask_again")).show(ui).clicked() {
            acts.later(Core::ask_for_keys_again);
        }
        if Button::ghost(t!("common.close")).show(ui).clicked() {
            acts.later(Core::close_modal);
        }
    });
    ui.add_space(18.0);
    widgets::divider(ui);
    ui.add_space(10.0);
    widgets::sub(ui, t!("security.no_devices"));
    ui.add_space(8.0);
    if Button::danger(t!("security.new_identity"))
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
    widgets::h3(ui, &tf!("security.number_title", name = name));
    let Some((now, changed)) = core.security_number(uid) else {
        widgets::sub(ui, t!("security.no_key_yet"));
        foot(ui, |ui| {
            if Button::ghost(t!("common.close")).show(ui).clicked() {
                acts.later(Core::close_modal);
            }
        });
        return;
    };
    widgets::sub(ui, &tf!("security.compare_body", name = name));
    ui.add_space(12.0);
    show_number(ui, &now);
    let view = core.e2e.pin_view(uid);
    ui.add_space(10.0);
    if let Some(new) = changed {
        ui.label(RichText::new(tf!("security.key_changed", name = name)).color(WARN));
        ui.add_space(6.0);
        show_number(ui, &new);
        foot(ui, |ui| {
            if Button::danger(t!("security.accept_key")).show(ui).clicked() {
                acts.later(move |c| c.accept_key_change(uid));
            }
            if Button::ghost(t!("common.close")).show(ui).clicked() {
                acts.later(Core::close_modal);
            }
        });
        return;
    }
    let verified = matches!(view, PinView::Pinned { verified: true });
    ui.label(
        RichText::new(if verified {
            t!("security.verified_yes")
        } else {
            t!("security.verified_no")
        })
        .color(MUTED),
    );
    if core.e2e.in_record(uid) {
        ui.label(RichText::new(tf!("security.in_record", name = name)).color(MUTED));
    }
    foot(ui, |ui| {
        let label = if verified {
            t!("security.unmark")
        } else {
            t!("security.mark_verified")
        };
        if Button::primary(label).show(ui).clicked() {
            acts.later(move |c| c.mark_verified(uid, !verified));
        }
        if Button::ghost(t!("common.close")).show(ui).clicked() {
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

//! The friends page (home with no conversation open): the five tabs.

use egui::{
    Align, CornerRadius, CursorIcon, Frame, Key, Layout, Margin, Rect, ScrollArea, Sense, Stroke,
    Ui, vec2,
};

use super::Acts;
use super::widgets::{self, Button};
use crate::core::{Core, ErrTo, FormId, FriendTab, Then};
use crate::icons::Icon;
use crate::model::*;
use crate::net::Req;
use crate::theme::{self, *};

fn tab(ui: &mut Ui, label: &str, on: bool, add: bool) -> bool {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::bold(14.0), TEXT);
    let size = galley.size() + vec2(20.0, 8.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let radius = CornerRadius::same(RADIUS_SM);
    let fg = if add {
        widgets::paint_gradient(ui, rect, radius, 1.0);
        ACCENT_INK
    } else if on {
        ui.painter().rect_filled(rect, radius, BG4);
        TEXT
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, radius, BG3);
        TEXT
    } else {
        MUTED
    };
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    resp.clicked()
}

pub fn draw(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let pending = core
        .friends
        .iter()
        .filter(|f| f.state != FriendState::Accepted)
        .count();
    let current = core.friend_tab;
    super::header(ui, "friends-head", |ui| {
        ui.add(Icon::Users.tinted(MUTED));
        ui.label(
            egui::RichText::new("Amigos")
                .font(theme::bold(15.0))
                .color(TEXT),
        );
        ui.add_space(4.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        let pending_label = if pending > 0 {
            format!("Pendentes ({pending})")
        } else {
            "Pendentes".to_owned()
        };
        let tabs = [
            (FriendTab::Online, "Online".to_owned()),
            (FriendTab::All, "Todos".to_owned()),
            (FriendTab::Pending, pending_label),
            (FriendTab::Blocked, "Bloqueados".to_owned()),
        ];
        for (t, label) in tabs {
            if tab(ui, &label, current == t, false) {
                acts.later(move |c| c.friend_tab = t);
            }
        }
        if tab(ui, "Adicionar amigo", current == FriendTab::Add, true) {
            acts.later(|c| c.friend_tab = FriendTab::Add);
        }
    });
    egui::CentralPanel::default()
        .frame(Frame::new().inner_margin(Margin::symmetric(20, 16)))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match current {
                    FriendTab::Add => add(ui, core, acts),
                    FriendTab::Blocked => blocked(ui, core, acts),
                    _ => list(ui, core, current, acts),
                });
        });
}

/// One row of the page (`.frow`): returns the rect to put buttons in.
fn frow(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let top = ui.cursor().top();
    ui.painter()
        .hline(ui.max_rect().x_range(), top, Stroke::new(1.0, BG3));
    let bg = ui.painter().add(egui::Shape::Noop);
    let r = Frame::new()
        .inner_margin(Margin::symmetric(8, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                add(ui);
            });
        })
        .response;
    if ui.rect_contains_pointer(r.rect) {
        ui.painter().set(
            bg,
            egui::Shape::rect_filled(r.rect, CornerRadius::same(RADIUS_MD), BG3),
        );
    }
}

fn who(ui: &mut Ui, name: &str, sub: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(
            egui::RichText::new(name)
                .font(theme::bold(14.0))
                .color(TEXT),
        );
        ui.label(egui::RichText::new(sub).size(13.0).color(MUTED));
    });
}

fn list(ui: &mut Ui, core: &Core, tab: FriendTab, acts: &mut Acts) {
    let all: Vec<&Friend> = core
        .friends
        .iter()
        .filter(|f| match tab {
            FriendTab::Pending => f.state != FriendState::Accepted,
            FriendTab::Online => {
                f.state == FriendState::Accepted && f.user.status.public() != Status::Offline
            }
            _ => f.state == FriendState::Accepted,
        })
        .collect();
    let label = match tab {
        FriendTab::Online => "Online",
        FriendTab::Pending => "Pedidos pendentes",
        _ => "Todos os amigos",
    };
    widgets::caption(ui, &format!("{label} — {}", all.len()));
    ui.add_space(6.0);
    if all.is_empty() {
        ui.label(egui::RichText::new("Ninguém por aqui ainda.").color(MUTED));
        return;
    }
    for f in all {
        let uid = f.user.id;
        let sub = match f.state {
            FriendState::Incoming => "Quer ser seu amigo",
            FriendState::Outgoing => "Pedido enviado",
            FriendState::Accepted => f.user.status.public().label(),
        };
        frow(ui, |ui| {
            widgets::avatar(ui, core, &f.user, 36.0, true, BG2);
            who(
                ui,
                f.user.name(),
                &format!("{} · {sub}", widgets::handle(&f.user)),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let unfriend = |acts: &mut Acts| {
                    acts.later(move |c| {
                        c.call(
                            Req::delete(format!("/friends/{uid}")),
                            Then::Nothing,
                            ErrTo::Toast,
                        )
                    })
                };
                match f.state {
                    FriendState::Accepted => {
                        if Button::ghost("Remover").small().show(ui).clicked() {
                            unfriend(acts);
                        }
                        if Button::ghost("Mensagem").small().show(ui).clicked() {
                            acts.later(move |c| c.open_dm(uid));
                        }
                    }
                    FriendState::Incoming => {
                        if Button::ghost("Recusar").small().show(ui).clicked() {
                            unfriend(acts);
                        }
                        if Button::primary("Aceitar").small().show(ui).clicked() {
                            acts.later(move |c| {
                                c.call(
                                    Req::put(format!("/friends/{uid}")),
                                    Then::Nothing,
                                    ErrTo::Toast,
                                )
                            });
                        }
                    }
                    FriendState::Outgoing => {
                        if Button::ghost("Cancelar").small().show(ui).clicked() {
                            unfriend(acts);
                        }
                    }
                }
            });
        });
    }
}

fn blocked(ui: &mut Ui, core: &Core, acts: &mut Acts) {
    let mut ids: Vec<i64> = core.blocks.iter().copied().collect();
    ids.sort_unstable();
    widgets::caption(ui, &format!("Bloqueados — {}", ids.len()));
    ui.add_space(6.0);
    if ids.is_empty() {
        ui.label(egui::RichText::new("Você não bloqueou ninguém.").color(MUTED));
        return;
    }
    for id in ids {
        let u = core.user(id);
        frow(ui, |ui| {
            widgets::avatar(ui, core, &u, 36.0, false, BG2);
            who(ui, u.name(), &widgets::handle(&u));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if Button::ghost("Desbloquear").small().show(ui).clicked() {
                    acts.later(move |c| {
                        c.call(
                            Req::delete(format!("/blocks/{id}")),
                            Then::Nothing,
                            ErrTo::Toast,
                        )
                    });
                }
            });
        });
    }
}

fn add(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    widgets::caption(ui, "Adicionar amigo");
    ui.add_space(4.0);
    ui.label(egui::RichText::new("Digite o nome de usuário exato da pessoa.").color(MUTED));
    ui.add_space(8.0);
    let busy = core.busy.contains(&FormId::AddFriend);
    let error = core.errors.get(&FormId::AddFriend).cloned();
    let mut send = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let width = (ui.available_width() - 140.0).max(120.0);
        let resp = ui
            .allocate_ui(vec2(width, 38.0), |ui| {
                widgets::input(ui, &mut core.add_friend, 40, false, "nome.de.usuario")
            })
            .inner;
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            send = true;
        }
        if Button::primary("Enviar pedido")
            .enabled(!busy)
            .show(ui)
            .clicked()
        {
            send = true;
        }
    });
    widgets::error_line(ui, error.as_ref());
    if send && !busy {
        acts.later(Core::add_friend);
    }
    let _ = Rect::NOTHING;
}

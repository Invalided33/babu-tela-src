//! The music modal: what is playing now, what comes next, and a field for
//! a new YouTube or SoundCloud link. Everyone in the call sees the same
//! queue; whoever added each song is the DJ for it.

use egui::{CornerRadius, Frame, Key, Margin, ScrollArea, Sense, Stroke, TextEdit, Ui};

use super::Acts;
use super::widgets::{self, Button};
use crate::core::Core;
use crate::icons::Icon;
use crate::music::{Item, State, View};
use crate::theme::{self, *};

/// The modal body.
pub fn modal(ui: &mut Ui, core: &mut Core, link: &mut String, focus: bool, acts: &mut Acts) {
    widgets::h3(ui, "Música na chamada");
    widgets::sub(
        ui,
        "Qualquer um da chamada pode entrar uma música. O computador de quem colocou o link \
         puxa a música e manda para todos.",
    );

    // "Entre numa chamada" check (even outside a call the modal is useful
    // for showing the help text, but we gray out the input).
    let in_call = core.voice.channel_id().is_some();

    let view = core.music.view();
    let extracting = core.music.extracting();
    now_playing(ui, &view, acts, core);
    coming_up(ui, &view, acts, core);
    ui.add_space(10.0);
    link_row(ui, core, link, focus, in_call, extracting, acts);
}

fn now_playing(ui: &mut Ui, view: &View, acts: &mut Acts, core: &Core) {
    let Some(item) = view.items.front() else {
        ui.add_space(10.0);
        widgets::sub(
            ui,
            "Nada tocando. Cole um link do YouTube ou do SoundCloud para começar.",
        );
        return;
    };
    ui.add_space(8.0);
    Frame::new()
        .fill(BG1)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .stroke(Stroke::new(1.0, BG0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new("Tocando agora")
                            .font(theme::small())
                            .color(ACCENT),
                    );
                    ui.label(
                        egui::RichText::new(&item.title)
                            .font(theme::bold(15.0))
                            .color(TEXT),
                    );
                    let subtitle = subtitle_for(item);
                    if !subtitle.is_empty() {
                        ui.label(
                            egui::RichText::new(subtitle)
                                .font(theme::tiny())
                                .color(MUTED),
                        );
                    }
                    let elapsed = format_time(view.elapsed as u32);
                    let total = item
                        .duration
                        .map(format_time)
                        .unwrap_or_else(|| "ao vivo".into());
                    ui.label(
                        egui::RichText::new(format!("{elapsed} / {total}"))
                            .font(theme::tiny())
                            .color(MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let id = item.id.clone();
                    if widgets::icon_button(ui, Icon::Right, 34.0, widgets::Ib::Bare, "Pular")
                        .clicked()
                    {
                        acts.later(move |c| c.music.skip(&id));
                    }
                    let (icon, label): (Icon, &str) = match view.state {
                        State::Playing => (Icon::Pause, "Pausar"),
                        State::Paused => (Icon::Play, "Tocar"),
                    };
                    if widgets::icon_button(ui, icon, 34.0, widgets::Ib::Bare, label).clicked() {
                        let paused = view.state == State::Playing;
                        acts.later(move |c| {
                            if paused {
                                c.music.pause();
                            } else {
                                c.music.resume();
                            }
                        });
                    }
                });
            });
            // Progress bar.
            if let Some(dur) = item.duration {
                let frac = (view.elapsed / dur as f32).clamp(0.0, 1.0);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), Sense::hover());
                ui.painter().rect_filled(rect, CornerRadius::same(2), BG0);
                let fill = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * frac, rect.height()),
                );
                ui.painter()
                    .rect_filled(fill, CornerRadius::same(2), ACCENT);
            }
        });
    let _ = core;
}

fn coming_up(ui: &mut Ui, view: &View, acts: &mut Acts, _core: &Core) {
    if view.items.len() <= 1 {
        return;
    }
    ui.add_space(10.0);
    ui.label(
        egui::RichText::new(format!("Fila ({})", view.items.len() - 1))
            .font(theme::bold(13.0))
            .color(TEXT),
    );
    ui.add_space(4.0);
    ScrollArea::vertical()
        .max_height(220.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for item in view.items.iter().skip(1) {
                queue_row(ui, item, acts);
            }
        });
}

fn queue_row(ui: &mut Ui, item: &Item, acts: &mut Acts) {
    Frame::new()
        .fill(BG1)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.set_width(ui.available_width() - 44.0);
                    widgets::ellipsis(ui, &item.title, theme::body(), TEXT);
                    let sub = subtitle_for(item);
                    if !sub.is_empty() {
                        widgets::ellipsis(ui, &sub, theme::tiny(), MUTED);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let id = item.id.clone();
                    if widgets::icon_button(
                        ui,
                        Icon::Trash,
                        28.0,
                        widgets::Ib::Bare,
                        "Remover da fila",
                    )
                    .clicked()
                    {
                        acts.later(move |c| c.music.skip(&id));
                    }
                });
            });
        });
    ui.add_space(4.0);
}

fn link_row(
    ui: &mut Ui,
    core: &Core,
    link: &mut String,
    focus: bool,
    in_call: bool,
    extracting: bool,
    acts: &mut Acts,
) {
    ui.horizontal(|ui| {
        let hint = if in_call {
            "Cole aqui o link do YouTube ou SoundCloud"
        } else {
            "Entre numa chamada para tocar música"
        };
        let edit = TextEdit::singleline(link)
            .hint_text(hint)
            .desired_width(ui.available_width() - 130.0)
            .margin(Margin::symmetric(10, 8))
            .background_color(BG1)
            .font(theme::body());
        let resp = ui.add_enabled(in_call, edit);
        if focus {
            resp.request_focus();
        }
        let submit = in_call
            && !extracting
            && !link.trim().is_empty()
            && (ui.input(|i| i.key_pressed(Key::Enter))
                || resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
        if Button::primary(if extracting {
            "Buscando…"
        } else {
            "Adicionar"
        })
        .enabled(in_call && !extracting && !link.trim().is_empty())
        .show(ui)
        .clicked()
            || submit
        {
            let name = core
                .me
                .as_ref()
                .map(|m| m.name().to_owned())
                .unwrap_or_default();
            let url = std::mem::take(link).trim().to_owned();
            acts.later(move |c| c.music.add_url(url, name));
        }
    });
    ui.add_space(2.0);
    widgets::sub(
        ui,
        "Só links de youtube.com, youtu.be ou soundcloud.com. O vídeo não vai; só o áudio.",
    );
    ui.horizontal(|ui| {
        if Button::ghost("Fechar").show(ui).clicked() {
            acts.later(Core::close_modal);
        }
    });
}

fn subtitle_for(item: &Item) -> String {
    let service = &item.service;
    match (item.uploader.as_deref(), !item.by_name.is_empty()) {
        (Some(up), true) => format!("{service} · {up} · adicionada por {}", item.by_name),
        (Some(up), false) => format!("{service} · {up}"),
        (None, true) => format!("{service} · adicionada por {}", item.by_name),
        (None, false) => service.clone(),
    }
}

fn format_time(secs: u32) -> String {
    let m = secs / 60;
    let s = secs % 60;
    format!("{m}:{s:02}")
}

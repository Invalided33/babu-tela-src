//! A conversation: the header, the messages, who is typing and the composer.

use chrono::{Datelike, Local, TimeZone};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Key, KeyboardShortcut, Layout,
    Margin, Modifiers, Panel, Rect, Response, ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2, pos2,
    vec2,
};

use super::Acts;
use super::widgets::{self, Ib};
use crate::core::{Action, Core, Editing, Modal, Pick, PopKind};
use crate::giphy::{self, Frames, Lookup};
use crate::icons::{self, Icon};
use crate::images::{Fit, Images};
use crate::markdown::{self, Click};
use crate::model::*;
use crate::theme::{self, *};
use crate::{t, tf};

fn local(ms: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

/// The month's name in the current language (`month.1`…`month.12`).
fn month_name(month: u32) -> &'static str {
    crate::lang::lookup(&format!("month.{month}")).unwrap_or("")
}

/// "Hoje às 14:05"/"Today at 14:05", or the date and time in the order the
/// language writes them ("16/10/2026 14:05").
pub fn fmt_time(ms: i64) -> String {
    let Some(d) = local(ms) else {
        return String::new();
    };
    if d.date_naive() == Local::now().date_naive() {
        tf!("chat.today_at", time = d.format("%H:%M"))
    } else {
        let fmt = match crate::lang::current() {
            crate::lang::Lang::En => "%m/%d/%Y %H:%M",
            crate::lang::Lang::Pt => "%d/%m/%Y %H:%M",
        };
        d.format(fmt).to_string()
    }
}

/// "17 de outubro de 2026"/"October 17, 2026", for the line between days.
fn fmt_day(ms: i64) -> String {
    let Some(d) = local(ms) else {
        return String::new();
    };
    tf!(
        "chat.day",
        day = d.day(),
        month = month_name(d.month()),
        year = d.year()
    )
}

fn hm(ms: i64) -> String {
    local(ms)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// What the clock in a conversation's header says of its messages' time.
pub fn ttl_text(ttl: i64) -> &'static str {
    match ttl {
        0 => t!("chat.ttl_off"),
        86_400 => t!("chat.ttl_1d"),
        _ => t!("chat.ttl_7d"),
    }
}

/// What became of a message's encryption, when it is anything but opened
/// and signed (e2e/): still opening, without a key here, refused (changed,
/// moved or not signed by its author), from before the encryption, when
/// nobody vouches for who wrote it, or past its time (only a report still
/// shows one).
fn seal_line(ui: &mut Ui, m: &Message) {
    if m.pending {
        return;
    }
    let (text, color) = match &m.seal {
        Seal::Open => return,
        Seal::Locked => (t!("chat.seal_opening").to_owned(), MUTED),
        Seal::Missing => (t!("chat.seal_missing").to_owned(), MUTED),
        Seal::Refused(why) => (tf!("chat.seal_refused", why = why), DANGER),
        Seal::Plain => (t!("chat.seal_plain").to_owned(), FAINT),
        Seal::Gone => (t!("chat.seal_gone").to_owned(), MUTED),
    };
    ui.label(egui::RichText::new(text).size(11.0).color(color));
}

/// Opens an attachment by its name: a message's file (encrypted on the
/// server) comes down decrypted and opens in its program; anything else
/// opens in the browser.
pub fn open_attachment(acts: &mut Acts, api: &crate::net::Api, a: &Attachment) {
    if crate::e2e::key_for(&a.url).is_some() {
        let a = a.clone();
        acts.later(move |c| c.open_attachment(a));
    } else {
        open_external(api.absolute(&a.url));
    }
}

/// Opens a web address or a file from the server in the browser.
pub fn open_external(url: String) {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        std::thread::spawn(move || {
            let _ = open::that(url);
        });
    }
}

pub fn draw(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let (cid, dm, name, topic) = match (core.current_dm(), core.current_channel()) {
        (Some(d), _) => (d.id, Some(d.user.clone()), d.user.name().to_owned(), None),
        (None, Some((_, c))) => (
            c.id,
            None,
            c.label().to_owned(),
            c.topic.clone().filter(|t| !t.is_empty()),
        ),
        _ => return,
    };
    let show_members = core.show_members;
    // How long the messages last here: either person of a direct
    // conversation sets it, whoever manages channels in a server's.
    let ttl = core.ttl_of(cid);
    let may_ttl = core
        .current_channel()
        .is_none_or(|(s, _)| s.can(P_MANAGE_CHANNELS));
    let show_ttl = ttl > 0 || may_ttl;
    super::header(ui, "chat-head", |ui| {
        match &dm {
            Some(u) => {
                widgets::avatar(ui, core, u, 26.0, true, BG2);
                ui.label(
                    egui::RichText::new(&name)
                        .font(theme::bold(15.0))
                        .color(TEXT),
                );
            }
            None => {
                ui.add(Icon::Hash.tinted(MUTED));
                ui.label(
                    egui::RichText::new(&name)
                        .font(theme::bold(15.0))
                        .color(TEXT),
                );
            }
        }
        let right = if dm.is_none() { 44.0 } else { 0.0 } + if show_ttl { 40.0 } else { 0.0 };
        match &topic {
            Some(t) => {
                let x = ui.cursor().left();
                let r = ui.max_rect();
                ui.painter().vline(
                    x,
                    (r.center().y - 9.0)..=(r.center().y + 9.0),
                    Stroke::new(1.0, BG4),
                );
                ui.add_space(10.0);
                ui.allocate_ui(vec2((ui.available_width() - right).max(10.0), 20.0), |ui| {
                    widgets::ellipsis(ui, t, theme::small(), MUTED);
                });
            }
            None => {
                ui.add_space((ui.available_width() - right).max(0.0));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if dm.is_none() {
                let style = if show_members { Ib::Normal } else { Ib::Bare };
                if widgets::icon_button(ui, Icon::Users, 34.0, style, t!("chat.members_list"))
                    .clicked()
                {
                    acts.later(|c| c.show_members = !c.show_members);
                }
            }
            if show_ttl {
                let style = if ttl > 0 { Ib::On } else { Ib::Bare };
                let mut tip = ttl_text(ttl).to_owned();
                if may_ttl {
                    tip.push_str(t!("chat.ttl_change"));
                }
                if widgets::icon_button(ui, Icon::Clock, 34.0, style, &tip).clicked() && may_ttl {
                    acts.later(move |c| c.open_modal(Modal::Ttl { cid }));
                }
            }
        });
    });

    // What may be done here.
    let blocked = dm.as_ref().is_some_and(|u| core.blocks.contains(&u.id));
    let (may_send, may_attach) = match core.current_channel() {
        Some((_, c)) => (c.can(P_SEND), c.can(P_ATTACH)),
        None => (!blocked, true),
    };
    let place = match &dm {
        Some(u) => tf!("chat.dm_placeholder", name = u.name()),
        None => tf!("chat.channel_placeholder", channel = name),
    };
    Panel::bottom("composer")
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().inner_margin(Margin {
            left: 16,
            right: 16,
            top: 0,
            bottom: 16,
        }))
        .show(ui, |ui| {
            if may_send {
                composer(ui, core, cid, &place, may_attach, acts);
            } else {
                locked(ui, blocked);
            }
        });
    // What the end-to-end encryption needs the person to know (security.rs).
    let needs_keys = core.e2e.state() == crate::e2e::State::NeedsDevice;
    let changed = dm
        .as_ref()
        .filter(|u| core.e2e.pin_view(u.id) == crate::e2e::PinView::Changed)
        .map(|u| (u.id, u.name().to_owned()));
    if needs_keys || changed.is_some() {
        Panel::bottom("e2e-aviso")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 8,
            }))
            .show(ui, |ui| e2e_banner(ui, needs_keys, changed, acts));
    }
    // The public record of keys does not add up (e2e/record.rs).
    if let Some(alert) = core.e2e.alert() {
        Panel::bottom("e2e-alerta")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 8,
            }))
            .show(ui, |ui| record_alert(ui, core, &alert, acts));
    }
    Panel::bottom("typing")
        .exact_size(20.0)
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().inner_margin(Margin::symmetric(16, 2)))
        .show(ui, |ui| typing(ui, core, cid));
    egui::CentralPanel::default()
        .frame(Frame::new())
        .show(ui, |ui| {
            let mut editing = core.editing.take();
            messages(ui, core, cid, &name, dm.is_some(), &mut editing, acts);
            core.editing = editing;
        });
}

/// Something in the public record of keys that does not add up: the server
/// showing two people different records, hiding entries, publishing in my
/// name or handing out a key that is not in it.
fn record_alert(ui: &mut Ui, core: &Core, alert: &crate::e2e::Alert, acts: &mut Acts) {
    use crate::e2e::Alert;
    let name = |uid: i64| core.user(uid).name().to_owned();
    let (text, who) = match alert {
        Alert::Fork(uid) if *uid == core.me_id() => (t!("chat.rec_fork_me").to_owned(), None),
        Alert::Fork(uid) => (tf!("chat.rec_fork", name = name(*uid)), Some(*uid)),
        Alert::Hidden(uid) => (tf!("chat.rec_hidden", name = name(*uid)), Some(*uid)),
        Alert::Foreign => (t!("chat.rec_foreign").to_owned(), None),
        Alert::Rewritten => (t!("chat.rec_rewritten").to_owned(), None),
        Alert::Owner(sid) => (
            tf!(
                "chat.rec_owner",
                server = core
                    .server(*sid)
                    .map_or(t!("chat.a_server"), |s| s.name.as_str())
            ),
            None,
        ),
        Alert::Unlisted(uid) => (tf!("chat.rec_unlisted", name = name(*uid)), Some(*uid)),
    };
    Frame::new()
        .fill(BG3)
        .stroke(Stroke::new(1.0, DANGER))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(format!("⚠ {text}")).color(DANGER));
                if let Some(uid) = who
                    && widgets::link(ui, t!("chat.security_code")).clicked()
                {
                    acts.later(move |c| super::security::open(c, uid));
                }
                if widgets::link(ui, t!("chat.got_it")).clicked() {
                    acts.later(|c| c.e2e.dismiss_alert());
                }
            });
        });
}

/// This PC has no keys yet, or someone's identity changed: a strip above
/// the composer, with the way to the dialog that sorts it out.
fn e2e_banner(ui: &mut Ui, needs_keys: bool, changed: Option<(i64, String)>, acts: &mut Acts) {
    Frame::new()
        .fill(BG3)
        .stroke(Stroke::new(1.0, WARN))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if needs_keys {
                    ui.label(egui::RichText::new(t!("chat.e2e_needs_keys")).color(WARN));
                    if widgets::link(ui, t!("chat.e2e_see_code")).clicked() {
                        acts.later(|c| c.open_modal(Modal::NewDevice));
                    }
                } else if let Some((uid, name)) = changed {
                    ui.label(egui::RichText::new(tf!("chat.e2e_changed", name = name)).color(WARN));
                    if widgets::link(ui, t!("chat.e2e_check")).clicked() {
                        acts.later(move |c| super::security::open(c, uid));
                    }
                }
            });
        });
}

fn locked(ui: &mut Ui, blocked: bool) {
    Frame::new()
        .fill(BG3)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(if blocked {
                        t!("chat.blocked_you")
                    } else {
                        t!("chat.no_send_perm")
                    })
                    .color(MUTED),
                );
            });
        });
}

fn typing(ui: &mut Ui, core: &Core, cid: i64) {
    let now = std::time::Instant::now();
    let who: Vec<String> = core
        .typing
        .get(&cid)
        .map(|m| {
            m.iter()
                .filter(|(_, until)| **until > now)
                .map(|(uid, _)| core.user(*uid).name().to_owned())
                .collect()
        })
        .unwrap_or_default();
    let text = match who.len() {
        0 => return,
        1 => tf!("chat.typing_one", name = who[0]),
        2 | 3 => tf!("chat.typing_some", names = who.join(", ")),
        _ => t!("chat.typing_many").to_owned(),
    };
    ui.label(egui::RichText::new(text).size(12.0).color(MUTED));
}

// ---------------------------------------------------------------- the composer

pub fn composer_id(cid: i64) -> Id {
    Id::unique(("composer", cid))
}

fn composer(
    ui: &mut Ui,
    core: &mut Core,
    cid: i64,
    place: &str,
    may_attach: bool,
    acts: &mut Acts,
) {
    let focus = std::mem::take(&mut core.focus_composer);
    let reply = core.reply.as_ref().map(|r| {
        r.author
            .as_ref()
            .map_or(t!("chat.unknown").to_owned(), |a| a.name().to_owned())
    });
    let files: Vec<(String, usize)> = core
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.clone(), i))
        .collect();
    let sends: Vec<Upload> = core
        .sending
        .iter()
        .filter(|s| s.cid == cid)
        .map(|s| Upload {
            id: s.id,
            files: s.files,
            fraction: s.progress.fraction(),
            sent: s.progress.sent(),
            total: s.progress.total(),
            direct: s.direct,
        })
        .collect();
    let direct = core.direct_send;
    if !sends.is_empty() {
        // The bar moves on its own while the bytes go.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
    let mut send = false;
    let mut paste = false;
    let id = composer_id(cid);
    // The "@" list, as it was on the last frame: while it shows, the
    // arrows, Enter, Tab and Esc are its own, taken before the box sees
    // them (Enter would send the message).
    let mut picker: Picker = ui.data(|d| d.get_temp(picker_id(cid))).unwrap_or_default();
    let keys = if picker.open && ui.memory(|m| m.has_focus(id)) {
        take_picker_keys(ui)
    } else {
        PickerKeys::default()
    };
    // A click on the list must not take the focus from the box first: the
    // list would close before the click lands.
    let list_layer = egui::LayerId::new(egui::Order::Foreground, mention_list_id(cid));
    let on_list = picker.open
        && ui
            .ctx()
            .pointer_interact_pos()
            .and_then(|p| ui.ctx().layer_id_at(p))
            == Some(list_layer);
    let surrender = ui.ctx().options(|o| o.input_options.surrender_focus_on);
    if on_list {
        ui.ctx().options_mut(|o| {
            o.input_options.surrender_focus_on = egui::SurrenderFocusOn::Never;
        });
    }
    // What the list shows this frame: where the "@" is, up to the cursor,
    // and who it may mean.
    let mut mention: Option<(usize, usize, Vec<User>)> = None;
    let composer_box = Frame::new()
        .fill(BG3)
        .corner_radius(CornerRadius::same(RADIUS))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(name) = &reply {
                Frame::new()
                    .inner_margin(Margin::symmetric(12, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(Icon::Reply.image(icons::SMALL, MUTED));
                            ui.label(
                                egui::RichText::new(t!("chat.replying_to"))
                                    .size(12.0)
                                    .color(MUTED),
                            );
                            ui.label(
                                egui::RichText::new(name)
                                    .font(theme::bold(12.0))
                                    .color(TEXT),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::icon_button_sized(
                                    ui,
                                    Icon::X,
                                    20.0,
                                    14.0,
                                    Ib::Bare,
                                    t!("chat.cancel_reply"),
                                )
                                .clicked()
                                {
                                    acts.later(|c| c.reply = None);
                                }
                            });
                        });
                    });
                let y = ui.cursor().top();
                ui.painter()
                    .hline(ui.max_rect().x_range(), y, Stroke::new(1.0, BG2));
            }
            for up in &sends {
                upload_row(ui, up, acts);
            }
            if !files.is_empty() {
                Frame::new()
                    .inner_margin(Margin {
                        left: 12,
                        right: 12,
                        top: 8,
                        bottom: 0,
                    })
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for (name, i) in &files {
                                chip(ui, name, *i, acts);
                            }
                        });
                    });
                direct_toggle(ui, direct, acts);
            }
            let stickers = core.giphy.enabled();
            let draft = core.drafts.entry(cid).or_default();
            let row = input_line(ui, draft, id, cid, place, may_attach, stickers, picker.open);
            let resp = &row.text;
            if focus {
                resp.request_focus();
            }
            // An "@" just before the cursor: who it may mean, as typed so
            // far (and not closed with Esc).
            let text = core.drafts.get(&cid).cloned().unwrap_or_default();
            let cursor = TextEdit::load_state(ui.ctx(), id)
                .and_then(|s| s.cursor.char_range())
                .map_or(text.chars().count(), |r| r.primary.index.0);
            let word = (resp.has_focus() || on_list)
                .then(|| mention_at(&text, cursor))
                .flatten();
            if keys.dismiss {
                picker.dismissed.clone_from(&word);
            }
            if picker.for_word != word {
                picker.selected = 0;
                picker.for_word.clone_from(&word);
            }
            if let Some((at, query)) = word.filter(|w| picker.dismissed.as_ref() != Some(w)) {
                let people = mention_people(core);
                let found: Vec<User> = mention_matches(&people, &query, core.me_id())
                    .into_iter()
                    .cloned()
                    .collect();
                if !found.is_empty() {
                    let n = found.len();
                    picker.selected = (picker.selected + keys.down + n - keys.up % n) % n;
                    mention = Some((at, cursor, found));
                }
            }
            if resp.changed() {
                acts.later(move |c| c.typed(cid));
            }
            if resp.has_focus() {
                let (enter, ctrl_v) = ui.input(|i| {
                    (
                        i.key_pressed(Key::Enter) && !i.modifiers.shift,
                        i.events.iter().any(|e| {
                            matches!(
                                e,
                                egui::Event::Key { key: Key::V, pressed: true, modifiers, .. }
                                    if modifiers.command
                            ) || matches!(e, egui::Event::Paste(_))
                        }),
                    )
                });
                send = enter;
                paste = ctrl_v && may_attach;
            }
            if row.attach.as_ref().is_some_and(Response::clicked) {
                acts.later(|c| c.pick_files(Pick::Attach));
            }
            if let Some(button) = row.sticker.as_ref().filter(|b| b.clicked()) {
                let anchor = button.rect;
                acts.later(move |c| c.open_stickers(anchor));
            }
            if row.emoji.clicked() {
                let anchor = row.emoji.rect;
                acts.later(move |c| c.open_emoji(PopKind::Emoji, anchor));
            }
        });
    ui.ctx()
        .options_mut(|o| o.input_options.surrender_focus_on = surrender);
    // The list over the composer, and who was picked in it: by Enter or
    // Tab (then it is not drawn this frame), or by a click.
    let mut open = false;
    if let Some((at, cursor, found)) = mention {
        let mut chosen = keys.accept.then_some(picker.selected);
        if chosen.is_none() {
            let rect = composer_box.response.rect;
            let (clicked, hovered) =
                mention_list(ui.ctx(), core, cid, rect, &found, picker.selected);
            if let Some(i) = hovered {
                picker.selected = i;
            }
            chosen = clicked;
            open = clicked.is_none();
        }
        if let Some(u) = chosen.and_then(|i| found.get(i)) {
            insert_mention(ui.ctx(), core, cid, at, cursor, &u.username);
            picker.dismissed = None;
            picker.selected = 0;
        }
    }
    picker.open = open;
    ui.data_mut(|d| d.insert_temp(picker_id(cid), picker));
    if send {
        acts.later(Core::send_message);
    }
    if paste {
        acts.later(|c| {
            c.paste_files();
        });
    }
}

// ---------------------------------------------------------------- @mentions

/// At most this many people in the "@" list.
const MENTION_ROWS: usize = 8;
const MENTION_ROW_H: f32 = 40.0;

/// The "@" list between frames: who is picked in it (the arrows move it),
/// for which "@…" that was, the "@…" it was closed for with Esc (it stays
/// closed until that changes), and whether it showed on the last frame.
#[derive(Clone, Default)]
struct Picker {
    selected: usize,
    for_word: Option<(usize, String)>,
    dismissed: Option<(usize, String)>,
    open: bool,
}

fn picker_id(cid: i64) -> Id {
    Id::unique(("mention-picker", cid))
}

/// The "@" list of conversation `cid` as the last frame left it, for the
/// tests' report: whether it shows, who is picked, and whether the
/// composer has the focus.
pub fn mention_report(ctx: &egui::Context, cid: i64) -> serde_json::Value {
    let picker: Picker = ctx.data(|d| d.get_temp(picker_id(cid))).unwrap_or_default();
    serde_json::json!({
        "open": picker.open,
        "selected": picker.selected,
        "word": picker.for_word.map(|(_, w)| w),
        "focused": ctx.memory(|m| m.has_focus(composer_id(cid))),
    })
}

fn mention_list_id(cid: i64) -> Id {
    Id::unique(("mention-list", cid))
}

/// The keys the "@" list takes while it shows.
#[derive(Default)]
struct PickerKeys {
    down: usize,
    up: usize,
    /// Enter or Tab.
    accept: bool,
    dismiss: bool,
}

/// Takes the list's keys out of this frame's input (only those pressed
/// without Shift or the like: Shift+Enter still breaks the line).
fn take_picker_keys(ui: &Ui) -> PickerKeys {
    fn take(i: &mut egui::InputState, key: Key) -> usize {
        let mut n = 0;
        i.events.retain(|e| {
            let hit = matches!(
                e,
                egui::Event::Key { key: k, pressed: true, modifiers, .. }
                    if *k == key && modifiers.is_none()
            );
            n += usize::from(hit);
            !hit
        });
        n
    }
    ui.input_mut(|i| PickerKeys {
        down: take(i, Key::ArrowDown),
        up: take(i, Key::ArrowUp),
        accept: take(i, Key::Enter) + take(i, Key::Tab) > 0,
        dismiss: take(i, Key::Escape) > 0,
    })
}

/// The "@" typed just before `cursor` (at the start or after a space), and
/// what follows it up to the cursor, if that is what a name can have
/// (letters, digits, _ and .): where the "@" is, in characters, and that
/// text.
fn mention_at(text: &str, cursor: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut i = cursor;
    while i > 0 {
        let c = chars[i - 1];
        if c == '@' {
            let at = i - 1;
            if at > 0 && !chars[at - 1].is_whitespace() {
                return None;
            }
            return Some((at, chars[i..cursor].iter().collect()));
        }
        if !(c.is_alphanumeric() || c == '_' || c == '.') || cursor - i >= 32 {
            return None;
        }
        i -= 1;
    }
    None
}

/// Who "@" followed by `query` may mean among `people` (never me): those
/// whose user name or a word of whose name starts with it, then those in
/// whose names it is (case and accents aside); online first, then by name.
fn mention_matches<'a>(people: &'a [User], query: &str, me: i64) -> Vec<&'a User> {
    let q = crate::emoji::fold(query);
    let mut found: Vec<(u8, bool, String, &User)> = people
        .iter()
        // Shorter user names are not read as mentions (markdown.rs).
        .filter(|u| u.id != me && u.username.len() >= 3)
        .filter_map(|u| {
            let name = crate::emoji::fold(u.name());
            let starts = q.is_empty()
                || u.username.starts_with(&q)
                || name
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|w| w.starts_with(&q));
            let rank = if starts {
                0
            } else if u.username.contains(&q) || name.contains(&q) {
                1
            } else {
                return None;
            };
            let away = u.status.public() == Status::Offline;
            Some((rank, away, name, u))
        })
        .collect();
    found.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    found
        .into_iter()
        .take(MENTION_ROWS)
        .map(|(_, _, _, u)| u)
        .collect()
}

/// Who can be mentioned in the conversation on screen: the server's
/// members, or the other person of a direct one.
fn mention_people(core: &Core) -> Vec<User> {
    if let Some((s, _)) = core.current_channel() {
        return s.members.clone();
    }
    core.current_dm()
        .map(|d| vec![d.user.clone()])
        .unwrap_or_default()
}

/// Puts "@user " where the "@…" being typed is (from character `at` to
/// `cursor`), the cursor after it. The text keeps the user name, which is
/// what every copy of the app (the older ones too) looks for.
fn insert_mention(
    ctx: &egui::Context,
    core: &mut Core,
    cid: i64,
    at: usize,
    cursor: usize,
    username: &str,
) {
    let id = composer_id(cid);
    let draft = core.drafts.entry(cid).or_default();
    let byte = |n: usize| draft.char_indices().nth(n).map_or(draft.len(), |(b, _)| b);
    let (start, end) = (byte(at), byte(cursor.max(at)));
    let put = format!("@{username} ");
    draft.replace_range(start..end, &put);
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    let caret = egui::text::CCursor::new(egui::text::CharIndex(at + put.chars().count()));
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(caret)));
    state.store(ctx, id);
    core.focus_composer = true;
}

/// The people an "@" may mean, over the composer (`above`) and as wide as
/// it, `selected` lit. Gives whoever was clicked, and whoever the pointer
/// moved onto (which picks them, as the arrows do).
fn mention_list(
    ctx: &egui::Context,
    core: &Core,
    cid: i64,
    above: Rect,
    found: &[User],
    selected: usize,
) -> (Option<usize>, Option<usize>) {
    const HEAD: f32 = 28.0;
    let height = HEAD + found.len() as f32 * MENTION_ROW_H + 8.0;
    let (mut clicked, mut hovered) = (None, None);
    egui::Area::new(mention_list_id(cid))
        .order(egui::Order::Foreground)
        .fixed_pos(pos2(above.left(), above.top() - height - 8.0))
        .show(ctx, |ui| {
            Frame::new()
                .fill(BG1)
                .stroke(Stroke::new(1.0, BG4))
                .corner_radius(CornerRadius::same(RADIUS))
                .inner_margin(Margin::same(4))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 6],
                    blur: 20,
                    spread: 0,
                    color: Color32::from_black_alpha(0x90),
                })
                .show(ui, |ui| {
                    ui.set_width(above.width() - 8.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let (head, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), HEAD), Sense::hover());
                    ui.painter().text(
                        pos2(head.left() + 8.0, head.center().y),
                        Align2::LEFT_CENTER,
                        "MEMBROS",
                        theme::bold(11.0),
                        FAINT,
                    );
                    let moving = ui.input(|i| i.pointer.is_moving());
                    for (i, u) in found.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(
                            vec2(ui.available_width(), MENTION_ROW_H),
                            Sense::click(),
                        );
                        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
                        crate::smoke::target(|| format!("mencao:{}", u.username), rect);
                        let lit = i == selected;
                        if lit {
                            ui.painter()
                                .rect_filled(rect, CornerRadius::same(RADIUS_SM), BG3);
                        }
                        let av = Rect::from_min_size(
                            pos2(rect.left() + 8.0, rect.center().y - 13.0),
                            Vec2::splat(26.0),
                        );
                        widgets::paint_avatar(ui, core, u, av, true, if lit { BG3 } else { BG1 });
                        let name = ui.painter().layout_no_wrap(
                            u.name().to_owned(),
                            theme::bold(14.0),
                            TEXT,
                        );
                        let at = pos2(av.right() + 10.0, rect.center().y - name.size().y / 2.0);
                        let after = at.x + name.size().x + 8.0;
                        ui.painter().galley(at, name, TEXT);
                        ui.painter().text(
                            pos2(after, rect.center().y),
                            Align2::LEFT_CENTER,
                            format!("@{}", u.username),
                            theme::small(),
                            MUTED,
                        );
                        if resp.hovered() && moving {
                            hovered = Some(i);
                        }
                        if resp.clicked() {
                            clicked = Some(i);
                        }
                    }
                });
        });
    (clicked, hovered)
}

/// The composer's line: what was laid out, for its callers (and the test
/// that checks the line up).
struct Line {
    attach: Option<Response>,
    text: Response,
    sticker: Option<Response>,
    emoji: Response,
}

/// The attach button, the message being written, the sticker button (when
/// GIPHY is on) and the emoji button, as the stylesheet had them
/// (`align-items: flex-end`): the buttons at the bottom of the line, by
/// the last line of text. The text goes first, at its place, so its height
/// is known before the buttons go next to it; a plain row would put the
/// buttons by the scroll area's own minimum height (64 px), not the text's.
/// `picking`: the "@" list shows, and Tab and Esc are its own (the box
/// keeps the focus).
#[allow(clippy::too_many_arguments)]
fn input_line(
    ui: &mut Ui,
    draft: &mut String,
    id: Id,
    cid: i64,
    place: &str,
    may_attach: bool,
    stickers: bool,
    picking: bool,
) -> Line {
    const BUTTON: f32 = 34.0;
    const GAP: f32 = 4.0;
    const MAX_HEIGHT: f32 = 200.0;
    Frame::new()
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let left = if may_attach { BUTTON + GAP } else { 0.0 };
            // The sticker and emoji buttons sit side by side.
            let right = if stickers { 2.0 * BUTTON } else { BUTTON };
            let width = (full.width() - left - GAP - right).max(40.0);
            let edit = TextEdit::multiline(draft)
                .id(id)
                .hint_text(egui::RichText::new(place).color(FAINT))
                .desired_rows(1)
                .desired_width(width)
                .char_limit(crate::net::MAX_MESSAGE)
                .font(theme::body())
                .frame(Frame::new().inner_margin(Margin::symmetric(4, 8)))
                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter));
            let filter = edit.get_event_filter();
            let edit = edit.event_filter(egui::EventFilter {
                tab: filter.tab || picking,
                escape: filter.escape || picking,
                ..filter
            });
            let slot = Rect::from_min_size(
                pos2(full.left() + left, full.top()),
                vec2(width, MAX_HEIGHT),
            );
            let mut column = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(slot)
                    .layout(Layout::top_down(Align::Min)),
            );
            let text = ScrollArea::vertical()
                .id_salt(("composer-scroll", cid))
                .max_height(MAX_HEIGHT)
                .max_width(width)
                .show(&mut column, |ui| ui.add(edit))
                .inner;
            let height = column.min_rect().height().max(BUTTON);
            let bottom = full.top() + height;
            let button = |ui: &mut Ui, x: f32, icon: Icon, tip: &str| {
                let rect = Rect::from_min_size(pos2(x, bottom - BUTTON), Vec2::splat(BUTTON));
                let mut at = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                widgets::icon_button(&mut at, icon, BUTTON, Ib::Bare, tip)
            };
            let attach =
                may_attach.then(|| button(ui, full.left(), Icon::Clip, t!("chat.attach_file")));
            let mut x = full.left() + left + width + GAP;
            let sticker = stickers.then(|| {
                let b = button(ui, x, Icon::Sticker, t!("chat.giphy_stickers"));
                x += BUTTON;
                b
            });
            let emoji = button(ui, x, Icon::Smile, t!("chat.emoji"));
            ui.advance_cursor_after_rect(Rect::from_min_max(full.min, pos2(full.right(), bottom)));
            Line {
                attach,
                text,
                sticker,
                emoji,
            }
        })
        .inner
}

/// A long message shows its start until it is opened: laying out a hundred
/// thousand characters on every frame would make the whole chat crawl.
const PREVIEW_CHARS: usize = 3000;
const PREVIEW_LINES: usize = 40;

/// The part of a message shown while it is closed, and whether there is
/// more. Cut at a line break or a space near the end when there is one.
fn preview(content: &str) -> (&str, bool) {
    let mut lines = 0;
    for (n, (i, c)) in content.char_indices().enumerate() {
        if c == '\n' {
            lines += 1;
            if lines >= PREVIEW_LINES {
                return (&content[..i], true);
            }
        }
        if n >= PREVIEW_CHARS {
            let head = &content[..i];
            let near = |j: &usize| *j > i * 3 / 4;
            let cut = head
                .rfind('\n')
                .filter(near)
                .or_else(|| head.rfind(' ').filter(near))
                .unwrap_or(i);
            return (&content[..cut], true);
        }
    }
    (content, false)
}

/// 100000 as "100.000".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(d);
    }
    out
}

/// A message whose files are still going up, as the composer shows it.
struct Upload {
    id: u64,
    files: usize,
    fraction: f32,
    sent: u64,
    total: u64,
    /// Sent directly: being prepared (hashed), not uploaded.
    direct: bool,
}

/// "Enviar direto", under the files waiting to go: a switch that lights up
/// when on. They then go straight from this PC to whoever downloads them,
/// never to the server (direct/).
fn direct_toggle(ui: &mut Ui, on: bool, acts: &mut Acts) {
    let label = t!("chat.send_direct");
    Frame::new()
        .inner_margin(Margin {
            left: 12,
            right: 12,
            top: 6,
            bottom: 0,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let ink = if on { ACCENT } else { MUTED };
                let galley = ui
                    .painter()
                    .layout_no_wrap(label.to_owned(), theme::bold(12.0), ink);
                let size = vec2(8.0 + icons::SMALL + 6.0 + galley.size().x + 10.0, 24.0);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                crate::smoke::target(|| "enviar-direto".to_owned(), rect);
                if ui.is_rect_visible(rect) {
                    let radius = CornerRadius::same(RADIUS_SM);
                    let (fill, edge) = if on {
                        (ACCENT_SOFT, ACCENT)
                    } else {
                        (BG1, BG4)
                    };
                    let p = ui.painter();
                    p.rect_filled(rect, radius, fill);
                    p.rect_stroke(
                        rect,
                        radius,
                        Stroke::new(1.0, edge),
                        egui::StrokeKind::Inside,
                    );
                    if resp.hovered() {
                        p.rect_filled(rect, radius, Color32::from_white_alpha(10));
                    }
                    if resp.has_focus() {
                        p.rect_stroke(
                            rect.expand(2.0),
                            radius,
                            Stroke::new(1.5, ACCENT),
                            egui::StrokeKind::Outside,
                        );
                    }
                    let icon = Rect::from_min_size(
                        pos2(rect.left() + 8.0, rect.center().y - icons::SMALL / 2.0),
                        Vec2::splat(icons::SMALL),
                    );
                    Icon::Direct.image(icons::SMALL, ink).paint_at(ui, icon);
                    let text = pos2(icon.right() + 6.0, rect.center().y - galley.size().y / 2.0);
                    ui.painter().galley(text, galley, ink);
                }
                resp.widget_info(|| {
                    egui::WidgetInfo::selected(egui::Role::CheckBox, true, on, label)
                });
                let hint = if on {
                    t!("chat.send_direct_on")
                } else {
                    t!("chat.send_direct_off")
                };
                widgets::ellipsis(ui, hint, theme::tiny(), MUTED);
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    acts.later(|c| c.direct_send = !c.direct_send);
                }
            });
        });
}

/// How far a send's files are, and the X that stops it (the message comes
/// back to the composer).
fn upload_row(ui: &mut Ui, up: &Upload, acts: &mut Acts) {
    Frame::new()
        .inner_margin(Margin {
            left: 12,
            right: 8,
            top: 8,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let icon = if up.direct { Icon::Direct } else { Icon::Clip };
                ui.add(icon.image(icons::SMALL, MUTED));
                let what = match (up.direct, up.files) {
                    (false, 1) => t!("chat.sending_one").to_owned(),
                    (false, n) => tf!("chat.sending_many", n = n),
                    (true, 1) => t!("chat.preparing_one").to_owned(),
                    (true, n) => tf!("chat.preparing_many", n = n),
                };
                ui.label(egui::RichText::new(what).size(12.0).color(TEXT));
                ui.label(
                    egui::RichText::new(tf!(
                        "chat.upload_progress",
                        sent = widgets::fmt_size(up.sent as i64),
                        total = widgets::fmt_size(up.total as i64),
                        pct = (up.fraction * 100.0).floor() as u32
                    ))
                    .size(12.0)
                    .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let cancel = widgets::icon_button_sized(
                        ui,
                        Icon::X,
                        18.0,
                        12.0,
                        Ib::Bare,
                        t!("chat.cancel_send"),
                    );
                    if cancel.clicked() {
                        let id = up.id;
                        acts.later(move |c| c.cancel_send(id));
                    }
                });
            });
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(2), BG1);
            let mut done = rect;
            done.set_width(rect.width() * up.fraction);
            painter.rect_filled(done, CornerRadius::same(2), ACCENT);
        });
}

fn chip(ui: &mut Ui, name: &str, i: usize, acts: &mut Acts) {
    Frame::new()
        .fill(BG1)
        .corner_radius(CornerRadius::same(RADIUS_SM))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.add(Icon::File.image(icons::SMALL, MUTED));
                ui.allocate_ui(vec2(160.0, 16.0), |ui| {
                    widgets::ellipsis(ui, name, theme::tiny(), TEXT);
                });
                let label = tf!("chat.remove_file", name = name);
                if widgets::icon_button_sized(ui, Icon::X, 18.0, 12.0, Ib::Bare, &label).clicked() {
                    acts.later(move |c| {
                        if i < c.files.len() {
                            c.files.remove(i);
                        }
                    });
                }
            });
        });
}

/// Puts an emoji where the cursor is in the composer (the emoji picker).
pub fn insert_emoji(ctx: &egui::Context, core: &mut Core, emoji: &str) {
    let Some(cid) = core.view.channel() else {
        return;
    };
    let id = composer_id(cid);
    let draft = core.drafts.entry(cid).or_default();
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    let at = state
        .cursor
        .char_range()
        .map_or(draft.chars().count(), |r| {
            r.primary.index.0.min(draft.chars().count())
        });
    let byte = draft.char_indices().nth(at).map_or(draft.len(), |(b, _)| b);
    draft.insert_str(byte, emoji);
    let cursor = egui::text::CCursor::new(egui::text::CharIndex(at + emoji.chars().count()));
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
    state.store(ctx, id);
    core.focus_composer = true;
}

// ---------------------------------------------------------------- the messages

#[allow(clippy::too_many_arguments)]
fn messages(
    ui: &mut Ui,
    core: &Core,
    cid: i64,
    name: &str,
    dm: bool,
    editing: &mut Option<Editing>,
    acts: &mut Acts,
) {
    let empty = crate::core::MsgList::default();
    let st = core.msgs.get(&cid).unwrap_or(&empty);
    let first_id = st.list.first().map(|m| m.id);
    let key = Id::unique(("msgs", cid));
    // When older messages are put on top, the view stays on what it showed.
    let remembered: Option<(Option<i64>, f32, f32)> = ui.data(|d| d.get_temp(key));
    let mut area = ScrollArea::vertical()
        .id_salt(key)
        .auto_shrink([false, false])
        .stick_to_bottom(true);
    let restore: Option<f32> = ui.data(|d| d.get_temp(key.with("restore")));
    if let Some(offset) = restore {
        area = area.vertical_scroll_offset(offset);
        ui.data_mut(|d| d.remove::<f32>(key.with("restore")));
    }
    let stick = core.stick_bottom;
    let (s, perms) = match core.current_channel() {
        Some((s, c)) => (Some(s), c.perms),
        None => (None, 0),
    };
    let output = area.show(ui, |ui| {
        ui.add_space(12.0);
        if st.more {
            ui.vertical_centered(|ui| {
                if widgets::link(ui, t!("chat.load_older")).clicked() {
                    acts.later(Core::load_older);
                }
            });
            ui.add_space(8.0);
        } else if !st.loading {
            Frame::new()
                .inner_margin(Margin {
                    left: 16,
                    right: 16,
                    top: 24,
                    bottom: 12,
                })
                .show(ui, |ui| {
                    let title = if dm {
                        name.to_owned()
                    } else {
                        format!("#{name}")
                    };
                    ui.label(egui::RichText::new(title).font(theme::h1()).color(TEXT));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(if dm {
                            t!("chat.dm_start")
                        } else {
                            t!("chat.channel_start")
                        })
                        .color(MUTED),
                    );
                });
        }
        let mut prev: Option<&Message> = None;
        let mut last_day = String::new();
        for m in &st.list {
            let day = fmt_day(m.created);
            if day != last_day {
                day_line(ui, &day);
                last_day = day;
                prev = None;
            }
            message(ui, core, m, prev, s, perms, dm, editing, acts);
            prev = Some(m);
        }
        ui.add_space(8.0);
        if stick {
            ui.scroll_to_cursor(Some(Align::BOTTOM));
        }
    });
    if stick {
        acts.later(|c| c.stick_bottom = false);
    }
    let height = output.content_size.y;
    let offset = output.state.clamped_offset().y;
    if let Some((old_first, old_height, old_offset)) = remembered
        && old_first.is_some()
        && old_first != first_id
        && first_id.is_some()
        && height > old_height
        && !stick
    {
        // Older messages arrived above: keep what was on screen in place.
        let fixed = old_offset + (height - old_height);
        ui.data_mut(|d| d.insert_temp(key.with("restore"), fixed));
        ui.ctx().request_repaint();
    }
    ui.data_mut(|d| d.insert_temp(key, (first_id, height, offset)));
}

fn day_line(ui: &mut Ui, day: &str) {
    ui.add_space(14.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
    let galley = ui
        .painter()
        .layout_no_wrap(day.to_owned(), theme::bold(12.0), FAINT);
    let mid = rect.center();
    let half = galley.size().x / 2.0 + 10.0;
    let line = Stroke::new(1.0, BG4);
    ui.painter()
        .hline((rect.left() + 16.0)..=(mid.x - half), mid.y, line);
    ui.painter()
        .hline((mid.x + half)..=(rect.right() - 16.0), mid.y, line);
    ui.painter()
        .galley(mid - galley.size() / 2.0, galley, FAINT);
    ui.add_space(6.0);
}

#[allow(clippy::too_many_arguments)]
fn message(
    ui: &mut Ui,
    core: &Core,
    m: &Message,
    prev: Option<&Message>,
    s: Option<&Server>,
    perms: i64,
    dm: bool,
    editing: &mut Option<Editing>,
    acts: &mut Acts,
) {
    let me = core.me_id();
    let author_id = m.author_id();
    if core.blocks.contains(&author_id) && !core.revealed.contains(&m.id) {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(56.0);
            let r = widgets::link(ui, t!("chat.blocked_message"));
            if r.clicked() {
                let id = m.id;
                acts.later(move |c| {
                    c.revealed.insert(id);
                });
            }
        });
        return;
    }
    let grouped = prev.is_some_and(|p| m.groups_with(p));
    let mine = author_id == me && me != 0;
    let can_delete = mine || (!dm && perms & P_MANAGE_MESSAGES != 0);
    let author = m
        .author
        .as_ref()
        .map(|a| core.users.get(&a.id).cloned().unwrap_or_else(|| a.clone()))
        .unwrap_or_else(|| User::unknown(0));
    let color = s
        .and_then(|s| s.member(author_id).and_then(|mm| s.role_color(mm)))
        .and_then(theme::parse_hex)
        .unwrap_or(TEXT);
    let highlight = core.mentions_me(&m.content);
    if !grouped {
        ui.add_space(12.0);
    }
    let top = ui.cursor().top();
    let full_x = ui.max_rect().x_range();
    // Background first, sized once the content is known.
    let bg_index = ui.painter().add(egui::Shape::Noop);
    let inner = ui.horizontal_top(|ui| {
        if m.pending {
            // Mine, not on the server yet.
            ui.multiply_opacity(0.55);
        }
        ui.spacing_mut().item_spacing.x = 0.0;
        // The gutter: the avatar, or the time on hover for a grouped line.
        let (gutter, _) = ui.allocate_exact_size(
            vec2(56.0, if grouped { 20.0 } else { 40.0 }),
            Sense::hover(),
        );
        if !grouped {
            let av = Rect::from_min_size(
                pos2(gutter.center().x - 19.0, gutter.top() + 2.0),
                Vec2::splat(38.0),
            );
            widgets::paint_avatar(ui, core, &author, av, false, BG2);
            if ui
                .interact(av, Id::unique(("av", m.id)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                acts.later(move |c| c.open_modal(Modal::Profile { uid: author_id }));
            }
        }
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 16.0).max(40.0));
            ui.spacing_mut().item_spacing.y = 2.0;
            if let Some(r) = &m.reply {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add(Icon::Reply.image(icons::SMALL, MUTED));
                    let who = r.author.as_ref().map_or(t!("chat.unknown"), |a| a.name());
                    ui.label(egui::RichText::new(who).font(theme::bold(12.0)).color(TEXT));
                    let quote = match (r.content.is_empty(), &r.sticker) {
                        (false, _) => r.content.as_str(),
                        (true, Some(_)) => t!("chat.a_sticker"),
                        (true, None) => t!("chat.an_attachment"),
                    };
                    widgets::ellipsis(ui, quote, theme::tiny(), MUTED);
                });
            }
            if !grouped {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let name = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(author.name())
                                    .font(theme::bold(14.0))
                                    .color(color),
                            )
                            .sense(Sense::click())
                            .selectable(false),
                        )
                        .on_hover_cursor(CursorIcon::PointingHand);
                    if name.hovered() {
                        ui.painter().hline(
                            name.rect.x_range(),
                            name.rect.bottom() - 1.0,
                            Stroke::new(1.0, color),
                        );
                    }
                    if name.clicked() {
                        acts.later(move |c| c.open_modal(Modal::Profile { uid: author_id }));
                    }
                    ui.label(
                        egui::RichText::new(fmt_time(m.created))
                            .size(11.0)
                            .color(FAINT),
                    );
                });
            }
            seal_line(ui, m);
            match editing {
                Some(e) if e.id == m.id => edit_box(ui, e, acts),
                _ => {
                    if !m.content.is_empty() || m.edited.is_some() {
                        let mid = m.id;
                        let open = core.expanded.contains(&mid);
                        let (start, long) = preview(&m.content);
                        let shown = if open { m.content.as_str() } else { start };
                        let pieces = markdown::parse(shown, &|n| core.known_name(n));
                        let revealed = |i: usize| core.spoilers.contains(&(mid, i));
                        let named = |user: &str| core.user_named(user).map(|u| u.name().to_owned());
                        match markdown::show(ui, &pieces, &revealed, &named, m.edited.is_some()) {
                            Some(Click::Link(url)) => open_external(url),
                            Some(Click::Spoiler(i)) => acts.later(move |c| {
                                c.spoilers.insert((mid, i));
                            }),
                            // A mention opens that person's profile.
                            Some(Click::Mention(user)) => {
                                if let Some(uid) = core.user_named(&user).map(|u| u.id) {
                                    acts.later(move |c| c.open_modal(Modal::Profile { uid }));
                                }
                            }
                            None => {}
                        }
                        if long {
                            let label = if open {
                                t!("chat.show_less").to_owned()
                            } else {
                                tf!(
                                    "chat.show_full",
                                    count = thousands(m.content.chars().count())
                                )
                            };
                            if widgets::link(ui, &label).clicked() {
                                acts.later(move |c| {
                                    if !c.expanded.remove(&mid) {
                                        c.expanded.insert(mid);
                                    }
                                });
                            }
                        }
                    }
                }
            }
            if let Some(gid) = &m.sticker {
                sticker(ui, core, m.id, gid, acts);
            }
            for a in &m.attachments {
                if a.kind == Kind::Direct {
                    super::direct::card(ui, core, m, a, acts);
                } else if super::player::playable(a) {
                    ui.add_space(4.0);
                    if a.kind == Kind::Video {
                        let max = vec2(ui.available_width().min(super::player::INLINE_W), 320.0);
                        super::player::video(ui, core, a, acts, max, false);
                    } else {
                        super::player::audio(ui, core, a, acts);
                    }
                } else {
                    attachment(ui, &core.images, &core.api, a, acts);
                }
            }
            if !m.reactions.is_empty() {
                reactions(ui, core, m, acts);
            }
        });
    });
    let rect = Rect::from_x_y_ranges(full_x, top..=inner.response.rect.bottom() + 2.0);
    // The toolbar sticks out above the message; hovering it keeps it open.
    let bar = tools_rect(rect, tool_count(m, mine, can_delete, dm));
    let hovered = ui.rect_contains_pointer(rect) || ui.rect_contains_pointer(bar);
    let bg = if highlight {
        Some(ACCENT_SOFT)
    } else if hovered {
        Some(Color32::from_black_alpha(0x1f))
    } else {
        None
    };
    if let Some(bg) = bg {
        ui.painter().set(
            bg_index,
            egui::Shape::rect_filled(rect, CornerRadius::ZERO, bg),
        );
    }
    if highlight {
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
            CornerRadius::ZERO,
            ACCENT,
        );
    }
    if grouped && hovered {
        ui.painter().text(
            pos2(rect.left() + 28.0, rect.top() + 10.0),
            Align2::CENTER_CENTER,
            hm(m.created),
            egui::FontId::proportional(10.0),
            FAINT,
        );
    }
    let editing_this = editing.as_ref().is_some_and(|e| e.id == m.id);
    // Nothing to reply to, react to or edit until the server has it.
    if hovered && !editing_this && !m.pending {
        tools(ui, rect, m, mine, can_delete, dm, acts);
    }
}

/// The edit box that replaces a message being edited.
fn edit_box(ui: &mut Ui, e: &mut Editing, acts: &mut Acts) {
    let resp = ui.add(
        TextEdit::multiline(&mut e.text)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .char_limit(crate::net::MAX_MESSAGE)
            .background_color(BG0)
            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
    );
    if std::mem::take(&mut e.focus) {
        resp.request_focus();
    }
    if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift) {
        acts.later(Core::finish_edit);
    }
    ui.label(
        egui::RichText::new(t!("chat.edit_hint"))
            .size(11.0)
            .color(FAINT),
    );
}

/// The little toolbar at the top right of a hovered message (`.tools`).
fn tools(
    ui: &mut Ui,
    rect: Rect,
    m: &Message,
    mine: bool,
    can_delete: bool,
    dm: bool,
    acts: &mut Acts,
) {
    let mut items: Vec<(Icon, &str, u8)> = vec![
        (Icon::Smile, t!("chat.react"), 0),
        (Icon::Reply, t!("chat.reply"), 1),
    ];
    if editable(m, mine) {
        items.push((Icon::Edit, t!("chat.edit"), 2));
    }
    if !mine && !dm {
        items.push((Icon::Flag, t!("chat.report"), 3));
    }
    if can_delete {
        items.push((Icon::Trash, t!("chat.delete"), 4));
    }
    let bar = tools_rect(rect, items.len());
    // Painted after the message and before the next one, so it covers the
    // bottom of the message above, as the old toolbar did.
    let painter = ui.painter().clone();
    painter.rect_filled(bar, CornerRadius::same(RADIUS_MD), BG1);
    painter.rect_stroke(
        bar,
        CornerRadius::same(RADIUS_MD),
        Stroke::new(1.0, BG4),
        egui::StrokeKind::Inside,
    );
    let mut x = bar.left() + 2.0;
    for (icon, tip, what) in items {
        let r = Rect::from_min_size(pos2(x, bar.top() + 2.0), vec2(30.0, 28.0));
        x += 30.0;
        let resp = ui
            .interact(r, Id::unique(("tool", m.id, what)), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(tip);
        let fg = match (resp.hovered(), what) {
            (true, 4) => DANGER,
            (true, _) => TEXT,
            _ => MUTED,
        };
        if resp.hovered() {
            painter.rect_filled(r, CornerRadius::same(RADIUS_SM), BG3);
        }
        let ir = Rect::from_center_size(r.center(), Vec2::splat(icons::SIZE));
        icon.image(icons::SIZE, fg).paint_at(ui, ir);
        if resp.clicked() {
            let mid = m.id;
            match what {
                0 => {
                    let anchor = r;
                    acts.later(move |c| c.open_emoji(PopKind::React(mid), anchor));
                }
                1 => {
                    let msg = m.clone();
                    acts.later(move |c| {
                        c.reply = Some(msg);
                        c.focus_composer = true;
                    });
                }
                2 => acts.later(move |c| c.start_edit(mid)),
                3 => acts.later(move |c| {
                    c.open_modal(Modal::Report {
                        mid,
                        reason: 0,
                        detail: String::new(),
                    })
                }),
                _ => acts.later(move |c| {
                    c.confirm(
                        t!("chat.delete_title"),
                        t!("chat.delete_body"),
                        t!("chat.delete"),
                        Action::DeleteMessage(mid),
                    )
                }),
            }
        }
    }
}

/// Where the toolbar of a message with `n` buttons goes.
fn tools_rect(message: Rect, n: usize) -> Rect {
    let w = n as f32 * 30.0 + 4.0;
    Rect::from_min_size(
        pos2(message.right() - 16.0 - w, message.top() - 14.0),
        vec2(w, 32.0),
    )
}

fn tool_count(m: &Message, mine: bool, can_delete: bool, dm: bool) -> usize {
    2 + usize::from(editable(m, mine)) + usize::from(!mine && !dm) + usize::from(can_delete)
}

/// Mine, and not a sticker (the server does not edit those).
fn editable(m: &Message, mine: bool) -> bool {
    mine && m.sticker.is_none()
}

/// How long a sticker in the conversation moves once it shows up; then it
/// rests on its first frame until the pointer is over it. Moving, each of
/// its frames draws the whole window again: 2 % of a core, measured, for
/// as long as it is on screen.
const STICKER_PLAYS: f64 = 8.0;

/// A sticker sent as a message: GIPHY's animation, its longer side up to
/// 160 points, moving for a while (and under the pointer) while the window
/// is in front. Hovering it shows a star that keeps it among the
/// favourites (or lets it go).
fn sticker(ui: &mut Ui, core: &Core, mid: i64, gid: &str, acts: &mut Acts) {
    const SIDE: f32 = 160.0;
    ui.add_space(2.0);
    let st = match core.giphy.sticker(gid) {
        Lookup::Ready(st) => st,
        found @ (Lookup::Loading | Lookup::Failed) => {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(SIDE), Sense::hover());
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS_MD), BG1);
            if matches!(found, Lookup::Failed) {
                // GIPHY did not answer; it is asked again in a minute.
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    t!("chat.giphy_silent"),
                    theme::tiny(),
                    FAINT,
                );
            }
            return;
        }
        Lookup::Missing | Lookup::Off => {
            gone_sticker(ui);
            return;
        }
    };
    let size = giphy::fit(st.large.size, SIDE);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    // The whole conversation is laid out on every frame: a sticker out of
    // sight is neither downloaded nor kept (so it lets its frames go).
    if !ui.is_rect_visible(rect) {
        return;
    }
    let hovered = ui.rect_contains_pointer(rect);
    match core.giphy.anim(&st.large, size.max_elem()) {
        Frames::Ready(anim) => {
            // Since when it shows, ready to move.
            let now = ui.input(|i| i.time);
            let key = Id::unique(("sticker-shown", mid));
            let since = ui.data_mut(|d| *d.get_temp_mut_or_insert_with(key, || now));
            let moving = core.stickers_move() && (hovered || now - since < STICKER_PLAYS);
            giphy::paint(ui, &anim, rect, moving);
            core.giphy.seen_in_chat(gid);
        }
        Frames::Loading => {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS_MD), BG1);
        }
        Frames::Failed => {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS_MD), BG1);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                t!("chat.sticker_unavailable"),
                theme::tiny(),
                FAINT,
            );
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, st.label()));
    // The title and who made it (GIPHY asks that creators be named).
    resp.on_hover_text(st.about());
    if hovered {
        let kept = core.stickers.iter().any(|s| s == gid);
        let star = Rect::from_min_size(
            pos2(rect.right() - 28.0, rect.top() + 4.0),
            Vec2::splat(24.0),
        );
        if star_button(ui, star, Id::unique(("sticker-star", mid)), kept).clicked() {
            let gid = gid.to_owned();
            acts.later(move |c| c.toggle_favorite(&gid));
        }
    }
}

/// A sticker GIPHY no longer has (or a server without GIPHY).
fn gone_sticker(ui: &mut Ui) {
    Frame::new()
        .fill(BG1)
        .stroke(Stroke::new(1.0, BG4))
        .corner_radius(CornerRadius::same(RADIUS_MD))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add(Icon::Sticker.tinted(FAINT));
                ui.label(egui::RichText::new(t!("chat.sticker_unavailable")).color(MUTED));
            });
        });
}

/// The round star over a sticker: filled when it is a favourite.
pub fn star_button(ui: &mut Ui, rect: Rect, id: Id, kept: bool) -> Response {
    let tip = if kept {
        t!("chat.unfavorite")
    } else {
        t!("chat.favorite_sticker")
    };
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip);
    let bg = if resp.hovered() { BG3 } else { BG0 };
    ui.painter()
        .circle_filled(rect.center(), rect.width() / 2.0, bg.gamma_multiply(0.9));
    let (icon, color) = if kept {
        (Icon::StarOn, WARN)
    } else {
        (Icon::Star, TEXT)
    };
    let ir = Rect::from_center_size(rect.center(), Vec2::splat(icons::SMALL));
    icon.image(icons::SMALL, color).paint_at(ui, ir);
    resp
}

fn reactions(ui: &mut Ui, core: &Core, m: &Message, acts: &mut Acts) {
    let me = core.me_id();
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        for r in &m.reactions {
            let mine = r.users.contains(&me);
            let text = format!("{} {}", r.emoji, r.users.len());
            let galley = ui.painter().layout_no_wrap(text, theme::small(), TEXT);
            let size = galley.size() + vec2(16.0, 4.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            let names: Vec<String> = r
                .users
                .iter()
                .map(|id| core.user(*id).name().to_owned())
                .collect();
            let resp = resp
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text(names.join(", "));
            let radius = CornerRadius::same(10);
            ui.painter()
                .rect_filled(rect, radius, if mine { ACCENT_SOFT } else { BG3 });
            if mine {
                ui.painter().rect_stroke(
                    rect,
                    radius,
                    Stroke::new(1.0, ACCENT),
                    egui::StrokeKind::Inside,
                );
            }
            ui.painter()
                .galley(rect.center() - galley.size() / 2.0, galley, TEXT);
            if resp.clicked() {
                let (mid, emoji) = (m.id, r.emoji.clone());
                acts.later(move |c| c.react(mid, &emoji));
            }
        }
    });
}

/// A picture inline, anything else as a card that opens it. A message's
/// file is encrypted on the server (e2e/file.rs): it comes down here,
/// decrypted, and opens in its program, instead of in the browser, which
/// would only get the encrypted bytes.
fn attachment(
    ui: &mut Ui,
    images: &std::sync::Arc<Images>,
    api: &crate::net::Api,
    a: &Attachment,
    acts: &mut Acts,
) {
    ui.add_space(4.0);
    let url = api.absolute(&a.url);
    let sealed = crate::e2e::key_for(&a.url).is_some();
    let open = |acts: &mut Acts, url: String| {
        if sealed {
            let a = a.clone();
            acts.later(move |c| c.open_attachment(a));
        } else {
            open_external(url);
        }
    };
    if a.kind == Kind::Image {
        let max = vec2(ui.available_width().min(420.0), 320.0);
        let resp = match images.get(&a.url, Fit::Within(840)) {
            Some(t) => {
                let natural = Images::size_of(&t);
                let scale = (max.x / natural.x).min(max.y / natural.y).min(1.0);
                let size = natural * scale;
                ui.add(
                    egui::Image::from_texture(egui::load::SizedTexture::from_handle(&t))
                        .fit_to_exact_size(size)
                        .corner_radius(CornerRadius::same(RADIUS_MD))
                        .sense(Sense::click()),
                )
            }
            None => {
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(260.0_f32.min(max.x), 160.0), Sense::click());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(RADIUS_MD), BG0);
                resp
            }
        };
        if resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(&a.name)
            .clicked()
        {
            open(acts, url);
        }
        return;
    }
    let icon = match a.kind {
        Kind::Video => Icon::Video,
        Kind::Audio => Icon::Play,
        _ => Icon::File,
    };
    let resp = Frame::new()
        .fill(BG1)
        .stroke(Stroke::new(1.0, BG4))
        .corner_radius(CornerRadius::same(RADIUS_MD))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.add(icon.tinted(ACCENT));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(egui::RichText::new(&a.name).color(ACCENT));
                    ui.label(
                        egui::RichText::new(widgets::fmt_size(a.size))
                            .size(12.0)
                            .color(MUTED),
                    );
                });
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(if sealed {
            t!("chat.open")
        } else {
            t!("chat.open_in_browser")
        });
    if resp.clicked() {
        open(acts, url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lays the composer's line out for real, without a window, 600 px
    /// wide, with `draft` written in it. The fonts take a pass to load.
    fn laid_out(draft: &str) -> (Rect, Line) {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0))),
            ..Default::default()
        };
        let mut out = None;
        for _ in 0..3 {
            let mut done = ctx.run_ui(input(), |ui| {
                let mut text = draft.to_owned();
                let frame = Frame::new().show(ui, |ui| {
                    ui.set_width(600.0);
                    input_line(
                        ui,
                        &mut text,
                        composer_id(1),
                        1,
                        "Conversar",
                        true,
                        true,
                        false,
                    )
                });
                out = Some((frame.response.rect, frame.inner));
            });
            // No renderer takes the font atlas here.
            done.textures_delta.clear();
        }
        out.expect("laid out")
    }

    #[test]
    fn the_composer_line_up() {
        // One line: the buttons sit on the same line as the text, and the
        // line is no taller than the text box (the stylesheet: 36 px).
        let (area, line) = laid_out("");
        let attach = line.attach.expect("attach").rect;
        let sticker = line.sticker.expect("figurinhas").rect;
        let (text, emoji) = (line.text.rect, line.emoji.rect);
        println!(
            "área {area:?}\nanexo {attach:?}\ntexto {text:?}\nfigurinhas {sticker:?}\nemoji {emoji:?}"
        );
        assert!((text.height() - 36.0).abs() <= 1.0, "texto {text:?}");
        assert!(area.height() <= 36.0 + 12.0 + 1.0, "área {area:?}");
        for (name, button) in [("anexo", attach), ("figurinhas", sticker), ("emoji", emoji)] {
            assert!(
                (button.center().y - text.center().y).abs() <= 1.0,
                "{name} {button:?} fora da linha do texto {text:?}"
            );
        }
        // In order, none over another, all inside the line.
        assert!(attach.right() <= text.left() && text.right() <= sticker.left());
        assert!(sticker.right() <= emoji.left(), "{sticker:?} {emoji:?}");
        assert!(emoji.right() <= area.right() + 0.5, "{emoji:?} {area:?}");
        // Several lines: the box grows, and the buttons stay by the last
        // one (the stylesheet's `align-items: flex-end`).
        let (_, line) = laid_out("um\ndois\ntrês\nquatro");
        let attach = line.attach.expect("attach").rect;
        let sticker = line.sticker.expect("figurinhas").rect;
        let (text, emoji) = (line.text.rect, line.emoji.rect);
        assert!(text.height() > 70.0, "texto {text:?}");
        for (name, button) in [("anexo", attach), ("figurinhas", sticker), ("emoji", emoji)] {
            assert!(
                (button.bottom() - text.bottom()).abs() <= 2.0,
                "{name} {button:?} longe da última linha {text:?}"
            );
        }
    }

    #[test]
    fn long_messages_open_on_request() {
        assert_eq!(preview("curta"), ("curta", false));
        let words = "palavra ".repeat(1000);
        let (start, long) = preview(&words);
        assert!(long);
        // Cut between words, not inside one.
        assert!(start.ends_with("palavra"), "{}", &start[start.len() - 20..]);
        assert!(start.chars().count() <= PREVIEW_CHARS);
        let lines = "linha\n".repeat(100);
        let (start, long) = preview(&lines);
        assert!(long);
        assert_eq!(start.lines().count(), PREVIEW_LINES);
        // A message of emoji is cut on a character, never inside one.
        let faces = "😀".repeat(5000);
        let (start, _) = preview(&faces);
        assert_eq!(start.chars().count(), PREVIEW_CHARS);
        assert_eq!(thousands(100_000), "100.000");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1.234.567");
    }

    #[test]
    fn days_read_in_each_language() {
        // 2026-10-17 12:00 UTC, which is the 17th in Brazil too.
        let ms = 1_792_238_400_000;
        crate::lang::set(crate::lang::Lang::Pt);
        assert!(
            fmt_day(ms).ends_with("de outubro de 2026"),
            "{}",
            fmt_day(ms)
        );
        crate::lang::set(crate::lang::Lang::En);
        let en = fmt_day(ms);
        assert!(en.starts_with("October ") && en.ends_with(", 2026"), "{en}");
        crate::lang::set(crate::lang::Lang::Pt);
        assert!(fmt_time(ms).contains(':'));
        assert_eq!(fmt_day(i64::MAX), "");
    }
}

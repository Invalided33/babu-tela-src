//! A voice channel: a tile for everyone in the call, with their video when
//! they share, and the call's buttons underneath.

use egui::{
    Align, Align2, Area, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin, Order,
    Panel, Rect, Response, ScrollArea, Sense, Stroke, Ui, UiBuilder, Vec2, ViewportCommand, pos2,
    vec2,
};

use std::collections::{HashMap, HashSet};

use super::Acts;
use super::sidebar::{ALL_BUTTONS, STAGE_BUTTONS};
use super::widgets::{self, Button, Ib};
use crate::core::{Core, Pop, PopKind};
use crate::icons::{self, Icon};
use crate::model::*;
use crate::t;
use crate::theme::{self, *};
use crate::voice::plan::{self, Candidate, Layer};

const GAP: f32 = 10.0;

/// In fullscreen, the call's buttons stay this long after the pointer last
/// moved (seconds), then hide, the pointer with them.
const BAR_FOR: f32 = 3.0;

/// The tiles' shape (a video's), and the narrowest they get: with more
/// people than fit at that size, the grid scrolls.
const ASPECT: f32 = 16.0 / 9.0;
const MIN_W: f32 = 260.0;

/// A grid with more columns wins when its tiles are at least this much of
/// the biggest possible: two people side by side rather than one over the
/// other (90 % of the size, in a maximized window), three as two and one
/// rather than in a column.
const WIDER_GRID: f32 = 0.85;

/// Where the tiles go, from the top left of the call view.
struct Grid {
    rects: Vec<Rect>,
    /// How tall the whole is (more than the view when it scrolls).
    height: f32,
}

/// `n` tiles in an `area`: all 16:9 and the same size, as big as they fit,
/// in rows from the top, the grid in the middle of the area and its last
/// row in the middle too. Below MIN_W wide they stop shrinking and the
/// grid gets taller than the area (it scrolls).
fn grid(n: usize, area: Vec2) -> Grid {
    if n == 0 {
        return Grid {
            rects: Vec::new(),
            height: 0.0,
        };
    }
    let area = vec2(area.x.max(0.0), area.y.max(0.0));
    let fit = |cols: usize| -> f32 {
        let rows = n.div_ceil(cols);
        let w = (area.x - GAP * (cols - 1) as f32) / cols as f32;
        let h = (area.y - GAP * (rows - 1) as f32) / rows as f32;
        w.min(h * ASPECT).max(0.0)
    };
    let best = (1..=n).map(fit).fold(0.0, f32::max);
    let (cols, w) = if best >= MIN_W {
        (1..=n)
            .rev()
            .map(|c| (c, fit(c)))
            .find(|(_, w)| *w >= best * WIDER_GRID && *w >= MIN_W)
            .unwrap_or((1, best))
    } else {
        let cols = (((area.x + GAP) / (MIN_W + GAP)).floor() as usize).clamp(1, n);
        let w = ((area.x - GAP * (cols - 1) as f32) / cols as f32).max(MIN_W.min(area.x));
        (cols, w)
    };
    let h = w / ASPECT;
    let rows = n.div_ceil(cols);
    let total = rows as f32 * h + (rows - 1) as f32 * GAP;
    let top = ((area.y - total) / 2.0).max(0.0);
    let rects = (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = cols.min(n - row * cols);
            let row_w = in_row as f32 * w + (in_row - 1) as f32 * GAP;
            let left = ((area.x - row_w) / 2.0).max(0.0);
            Rect::from_min_size(
                pos2(left + col as f32 * (w + GAP), top + row as f32 * (h + GAP)),
                vec2(w, h),
            )
        })
        .collect();
    Grid {
        rects,
        height: top + total,
    }
}

/// A tile at least this wide (in pixels of the screen) gets the big copy of
/// the video; smaller ones get the small copy, which is about half this.
const BIG_TILE: f32 = 800.0;

fn zoom_id(cid: i64) -> Id {
    Id::unique(("stage-zoom", cid))
}

/// Enlarges `uid`'s video in the call view of `cid` (or none), as a click
/// on it does. For the tests.
pub fn set_zoom(ctx: &egui::Context, cid: i64, uid: Option<i64>) {
    ctx.data_mut(|d| match uid {
        Some(uid) => {
            d.insert_temp(zoom_id(cid), uid);
        }
        None => {
            d.remove::<i64>(zoom_id(cid));
        }
    });
}

/// Whose video the call view of `cid` shows enlarged, for the tests.
pub fn zoomed(ctx: &egui::Context, cid: i64) -> Option<i64> {
    ctx.data(|d| d.get_temp(zoom_id(cid)))
}

pub fn is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

/// Whether the window was maximized as fullscreen began: it is again once
/// fullscreen ends.
fn was_maximized_id() -> Id {
    Id::unique("fullscreen-was-maximized")
}

/// Into fullscreen or out of it: the call view's buttons, a presentation's,
/// Escape. A maximized window kept the taskbar over it in fullscreen: winit
/// holds a maximized window without a frame to the screen's work area (the
/// screen without the taskbar), and Windows only puts the taskbar under a
/// fullscreen window that is not maximized (Chromium restores its window
/// first for the same reason). So the window is restored before, and
/// maximized again after.
pub fn set_fullscreen(ctx: &egui::Context, on: bool) {
    let (fullscreen, maximized) = ctx.input(|i| {
        let v = i.viewport();
        (v.fullscreen.unwrap_or(false), v.maximized.unwrap_or(false))
    });
    if on == fullscreen {
        return;
    }
    if on {
        ctx.data_mut(|d| d.insert_temp(was_maximized_id(), maximized));
        if maximized {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
        }
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
    } else {
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        let again = ctx.data_mut(|d| d.remove_temp::<bool>(was_maximized_id()));
        if again == Some(true) {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
        }
    }
}

pub fn draw(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Some((s, c)) = core.current_channel() else {
        return;
    };
    let (cid, name) = (c.id, c.label().to_owned());
    let may_join = c.can(P_CONNECT);
    let users: Vec<VoiceUser> = s.in_voice(cid).to_vec();
    let joined = core.voice.channel_id() == Some(cid);
    let show_members = core.show_members;
    // Fullscreen is the call alone: no header, no bar (its buttons float
    // over it), the enlarged video edge to edge.
    let fullscreen = is_fullscreen(ui.ctx());
    // A presentation fills the view, unless it was set aside for the
    // people's tiles (until it is brought back, or another one begins).
    let presentation = if joined {
        super::slides::shown(core, &users)
    } else {
        None
    };
    let aside: Option<u32> = ui.data(|d| d.get_temp(super::slides::set_aside(cid)));
    let set_aside = presentation.as_ref().is_some_and(|p| aside == Some(p.id));
    let slides = presentation.filter(|p| aside != Some(p.id));
    if !fullscreen {
        super::header(ui, "stage-head", |ui| {
            ui.add(Icon::Speaker.tinted(MUTED));
            ui.label(
                egui::RichText::new(&name)
                    .font(theme::bold(15.0))
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let style = if show_members { Ib::Normal } else { Ib::Bare };
                if widgets::icon_button(ui, Icon::Users, 34.0, style, t!("chat.members_list"))
                    .clicked()
                {
                    acts.later(|c| c.show_members = !c.show_members);
                }
                if widgets::icon_button(ui, Icon::Expand, 34.0, Ib::Bare, t!("stage.fullscreen"))
                    .clicked()
                {
                    set_fullscreen(ui.ctx(), true);
                }
                if set_aside
                    && Button::primary(t!("stage.view_presentation"))
                        .small()
                        .show(ui)
                        .clicked()
                {
                    ui.data_mut(|d| d.remove::<u32>(super::slides::set_aside(cid)));
                }
            });
        });
        Panel::bottom("stagebar")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(BG0).inner_margin(Margin::same(12)))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    if joined {
                        // The microphone and the sound are by the gear in
                        // the side panel; here, what goes with the call.
                        let n = STAGE_BUTTONS.len() as f32;
                        ui.allocate_ui_with_layout(
                            vec2(n * 48.0 + (n - 1.0) * GAP, 48.0),
                            Layout::left_to_right(Align::Center),
                            |ui| super::sidebar::voice_buttons(ui, core, acts, 48.0, STAGE_BUTTONS),
                        );
                    } else if may_join {
                        if Button::primary(t!("stage.join_call")).show(ui).clicked() {
                            acts.later(move |c| c.join_voice(cid));
                        }
                    } else {
                        ui.label(egui::RichText::new(t!("stage.no_join_perm")).color(MUTED));
                    }
                });
            });
    }
    let frame = if fullscreen {
        Frame::new().fill(Color32::BLACK)
    } else {
        Frame::new().fill(BG0).inner_margin(Margin::same(16))
    };
    let wanted = egui::CentralPanel::default()
        .frame(frame)
        .show(ui, |ui| {
            if let Some(p) = &slides {
                return super::slides::draw(ui, core, p, &users, cid, acts);
            }
            // Someone is presenting but the viewer hasn't opted in: show
            // a banner with an "Assistir" button, then the normal tiles.
            super::slides::presenting_banner(ui, core, &users);
            if users.is_empty() {
                super::empty(ui, t!("stage.empty_title"), t!("stage.empty_text"));
                return HashMap::new();
            }
            tiles(ui, core, &users, joined, cid, acts)
        })
        .inner;
    if fullscreen {
        // Someone else's screen, enlarged and watched: the bar can stop it.
        let zoomed: Option<i64> = ui.data(|d| d.get_temp(zoom_id(cid)));
        let screen = zoomed.filter(|uid| {
            slides.is_none()
                && *uid != core.me_id()
                && core.voice.watching(*uid)
                && users
                    .iter()
                    .any(|u| u.user_id == *uid && u.video == Some(Video::Screen))
        });
        fullscreen_bar(ui, core, acts, cid, joined, may_join, screen);
    }
    if joined {
        // While videos are shown, the view says so at least twice a second,
        // or their senders would take it for gone (the frames that the
        // pictures themselves bring may stop: a still screen sends none).
        if !wanted.is_empty() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(400));
        }
        core.voice.want_videos(wanted);
        // A screen enlarged is the one heard (a presentation over the
        // view leaves none enlarged, and one not watched is not heard).
        let zoomed: Option<i64> = ui.data(|d| d.get_temp(zoom_id(cid)));
        let focus = zoomed.filter(|uid| {
            slides.is_none()
                && core.voice.watching(*uid)
                && users
                    .iter()
                    .any(|u| u.user_id == *uid && u.video == Some(Video::Screen))
        });
        core.voice.set_screen_focus(focus);
    }
}

/// The call's buttons over the call in fullscreen: all of them (the side
/// panel is not on screen), "Parar de assistir" while someone's screen is
/// enlarged, and the way out of fullscreen. There for a few seconds after
/// the pointer moves and while it is on them; then they hide, the pointer
/// too, and the picture is all that shows.
#[allow(clippy::too_many_arguments)]
fn fullscreen_bar(
    ui: &Ui,
    core: &Core,
    acts: &mut Acts,
    cid: i64,
    joined: bool,
    may_join: bool,
    screen: Option<i64>,
) {
    let ctx = ui.ctx().clone();
    let id = Id::unique("fullscreen-bar");
    let still = ctx.input(|i| i.pointer.time_since_last_movement());
    // Where it was on the last frame: the pointer on it keeps it.
    let last: Option<Rect> = ctx.data(|d| d.get_temp(id));
    let pointer = ctx.input(|i| i.pointer.hover_pos());
    let on_bar = matches!((last, pointer), (Some(r), Some(p)) if r.expand(12.0).contains(p));
    // A menu or a dialog open over the call (the share picker, someone's
    // volume) keeps it, and the pointer.
    if still >= BAR_FOR && !on_bar && core.pop.is_none() && core.modal.is_none() {
        ctx.set_cursor_icon(CursorIcon::None);
        return;
    }
    if !on_bar {
        // To hide it on time; any movement before that brings a frame.
        ctx.request_repaint_after(std::time::Duration::from_secs_f32(
            (BAR_FOR - still).max(0.05),
        ));
    }
    let shown = Area::new(id)
        .order(Order::Foreground)
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -28.0))
        .show(&ctx, |ui| {
            Frame::new()
                .fill(Color32::from_black_alpha(0xc8))
                .stroke(Stroke::new(1.0, BG3))
                .corner_radius(CornerRadius::same(36))
                .inner_margin(Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if joined {
                            super::sidebar::voice_buttons(ui, core, acts, 48.0, ALL_BUTTONS);
                        } else if may_join
                            && Button::primary(t!("stage.join_call")).show(ui).clicked()
                        {
                            acts.later(move |c| c.join_voice(cid));
                        }
                        ui.add_space(6.0);
                        if let Some(uid) = screen
                            && Button::danger(t!("stage.stop_watching")).show(ui).clicked()
                        {
                            set_zoom(ui.ctx(), cid, None);
                            set_fullscreen(ui.ctx(), false);
                            acts.later(move |c| c.voice.set_watching(uid, false));
                        }
                        let out = widgets::icon_button_sized(
                            ui,
                            Icon::ZoomOut,
                            48.0,
                            icons::LARGE,
                            Ib::Normal,
                            t!("stage.exit_fullscreen"),
                        );
                        crate::smoke::target(|| "sair-da-tela-cheia".into(), out.rect);
                        if out.clicked() {
                            set_fullscreen(ui.ctx(), false);
                        }
                    });
                });
        });
    let rect = shown.response.rect;
    ctx.data_mut(|d| d.insert_temp(id, rect));
}

/// Lays out everyone's tile, and says which copy of whose video to
/// receive: only of tiles on screen, at most `plan::MAX_VIDEOS` of them
/// (an enlarged one, then screens, then whoever spoke last), the big copy
/// for a big tile.
fn tiles(
    ui: &mut Ui,
    core: &Core,
    users: &[VoiceUser],
    joined: bool,
    cid: i64,
    acts: &mut Acts,
) -> HashMap<i64, Layer> {
    // A clicked video fills the whole stage until clicked again (a screen
    // no longer watched does not).
    let zoom_id = zoom_id(cid);
    let zoomed: Option<i64> = ui.data(|d| d.get_temp(zoom_id));
    let zoomed = zoomed.filter(|uid| {
        users.iter().any(|u| {
            u.user_id == *uid
                && u.video.is_some()
                && (u.video != Some(Video::Screen) || core.voice.watching(*uid))
        })
    });
    let shown: Vec<&VoiceUser> = match zoomed {
        Some(uid) => users.iter().filter(|u| u.user_id == uid).collect(),
        None => users.iter().collect(),
    };
    let avail = ui.available_size();
    // An enlarged video takes the whole view; the rest share it out.
    let layout = match zoomed {
        Some(_) => Grid {
            rects: vec![Rect::from_min_size(pos2(0.0, 0.0), avail)],
            height: avail.y,
        },
        None => grid(shown.len(), avail),
    };
    let me = core.me_id();
    let ppp = ui.ctx().pixels_per_point();
    let mut wanted = HashMap::new();
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let origin = ui.cursor().min.to_vec2();
            let rects: Vec<Rect> = layout.rects.iter().map(|r| r.translate(origin)).collect();
            // A screen I stopped watching is not asked for: its picture
            // stops coming.
            let candidates: Vec<Candidate> = shown
                .iter()
                .zip(&rects)
                .filter(|(v, r)| {
                    joined
                        && v.video.is_some()
                        && v.user_id != me
                        && (v.video != Some(Video::Screen) || core.voice.watching(v.user_id))
                        && ui.is_rect_visible(**r)
                })
                .map(|(v, _)| Candidate {
                    uid: v.user_id,
                    zoomed: zoomed == Some(v.user_id),
                    screen: v.video == Some(Video::Screen),
                    spoke: core.voice.last_spoke(v.user_id),
                })
                .collect();
            let receiving: HashSet<i64> = candidates
                .iter()
                .filter(|c| core.voice.showing(c.uid) != Layer::Off)
                .map(|c| c.uid)
                .collect();
            let picked = plan::pick_videos(&candidates, &receiving, plan::MAX_VIDEOS);
            for (v, rect) in shown.iter().zip(&rects) {
                if picked.contains(&v.user_id) {
                    let big = zoomed == Some(v.user_id) || rect.width() * ppp >= BIG_TILE;
                    wanted.insert(v.user_id, if big { Layer::High } else { Layer::Low });
                }
            }
            for (v, rect) in shown.iter().zip(&rects) {
                let video = v.user_id == me || picked.contains(&v.user_id);
                let click = tile(ui, core, v, *rect, joined && video);
                // A screen's annotations, over it wherever it shows; drawn
                // on where it is enlarged (ui/ink.rs).
                if let Some(picture) = click.picture
                    && v.video == Some(Video::Screen)
                    && let Some(surface) = core.voice.ink().screen_of(v.user_id)
                {
                    let page = crate::ink::Page { surface, n: 0 };
                    let enlarged = zoomed == Some(v.user_id);
                    super::ink::over(ui, core, page, picture, *rect, enlarged, acts);
                }
                let uid = v.user_id;
                let here = zoomed == Some(uid);
                if click.zoom {
                    set_zoom(ui.ctx(), cid, (!here).then_some(uid));
                }
                // The tile's own fullscreen: this video, enlarged, alone on
                // the screen (the floating bar has the way back).
                if click.fullscreen {
                    set_zoom(ui.ctx(), cid, Some(uid));
                    set_fullscreen(ui.ctx(), true);
                }
                if click.stop {
                    if here {
                        set_zoom(ui.ctx(), cid, None);
                        set_fullscreen(ui.ctx(), false);
                    }
                    acts.later(move |c| c.voice.set_watching(uid, false));
                }
                if click.watch {
                    acts.later(move |c| c.voice.set_watching(uid, true));
                }
                if let Some(at) = click.menu {
                    open_person(acts, uid, at);
                }
            }
            ui.allocate_space(vec2(avail.x, layout.height));
        });
    wanted
}

/// Opens the menu of someone in a call (how loud they play here) where the
/// right click was.
pub fn open_person(acts: &mut Acts, uid: i64, at: egui::Pos2) {
    acts.later(move |c| {
        c.pop = Some(Pop {
            kind: PopKind::Person(uid),
            anchor: Rect::from_min_size(at, Vec2::ZERO),
        });
    });
}

/// What a tile was clicked for.
#[derive(Default)]
pub(super) struct TileClick {
    /// Their video, to enlarge it or put it back.
    pub(super) zoom: bool,
    /// A right click on someone else, here: their menu.
    pub(super) menu: Option<egui::Pos2>,
    /// Where their picture shows, when it does (for the annotations).
    pub(super) picture: Option<Rect>,
    /// The fullscreen button over their video.
    pub(super) fullscreen: bool,
    /// "Parar de assistir" over someone's screen.
    pub(super) stop: bool,
    /// "Assistir transmissão" on a screen not watched.
    pub(super) watch: bool,
}

/// A button over a video (`label` beside the icon, if any), dark so it
/// reads on any picture, its right edge at `right`.
fn over_button(
    ui: &mut Ui,
    id: Id,
    right: egui::Pos2,
    icon: Icon,
    label: Option<&str>,
    tip: &str,
) -> Response {
    const H: f32 = 32.0;
    let galley = label.map(|l| {
        ui.painter()
            .layout_no_wrap(l.to_owned(), theme::bold(13.0), TEXT)
    });
    let w = galley.as_ref().map_or(H, |g| H + g.size().x + 10.0);
    let rect = Rect::from_min_max(pos2(right.x - w, right.y - H), right);
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip);
    let fill = if resp.hovered() {
        Color32::from_black_alpha(0xf0)
    } else {
        Color32::from_black_alpha(0xb4)
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(RADIUS_MD), fill);
    let ink = if resp.hovered() { TEXT } else { MUTED };
    let icon_at = Rect::from_center_size(
        pos2(rect.left() + H / 2.0, rect.center().y),
        Vec2::splat(icons::SIZE),
    );
    icon.image(icons::SIZE, ink).paint_at(ui, icon_at);
    if let Some(g) = galley {
        let at = pos2(rect.left() + H - 2.0, rect.center().y - g.size().y / 2.0);
        ui.painter().galley(at, g, ink);
    }
    resp
}

/// One person. `video`: their picture is shown, when there is one (it is
/// being received, or it is mine).
pub(super) fn tile(ui: &mut Ui, core: &Core, v: &VoiceUser, rect: Rect, video: bool) -> TileClick {
    let u = core.user(v.user_id);
    let resp = ui.interact(rect, Id::unique(("tile", v.user_id)), Sense::click());
    crate::smoke::target(|| format!("quadro:{}", u.username), rect);
    let mut click = TileClick::default();
    if v.user_id != core.me_id() && resp.secondary_clicked() {
        click.menu = Some(resp.interact_pointer_pos().unwrap_or(rect.center()));
    }
    let gains = core.voice.gains(v.user_id);
    // Someone else's screen share, and whether I watch it.
    let their_screen = v.user_id != core.me_id() && v.video == Some(Video::Screen);
    let unwatched = their_screen && !core.voice.watching(v.user_id);
    // Enlarged in fullscreen: edge to edge, with no rounded corners.
    let screen_rect = ui.ctx().content_rect();
    let edge_to_edge = is_fullscreen(ui.ctx()) && rect.width() >= screen_rect.width() - 1.0;
    let radius = if edge_to_edge {
        CornerRadius::ZERO
    } else {
        CornerRadius::same(12)
    };
    // `radial-gradient(circle at 50% 40%, var(--bg3), var(--bg2))`, as
    // rings that stop short of the corners so the rounding stays clean,
    // and cut at the tile's edge (a wide tile is shorter than they are
    // big), as the browser cut the gradient.
    ui.painter().rect_filled(rect, radius, BG2);
    let center = pos2(rect.center().x, rect.top() + rect.height() * 0.4);
    let reach = (rect.width().min(rect.height()) * 0.5 - 2.0).max(1.0);
    let rings = ui.painter_at(rect);
    let steps = 32;
    for i in 0..steps {
        let t = i as f32 / steps as f32;
        rings.circle_filled(center, reach * (1.0 - t), theme::mix(BG2, BG3, t));
    }
    let frame = if video && v.video.is_some() {
        core.voice.frame(v.user_id)
    } else {
        None
    };
    match frame {
        Some(t) => {
            ui.painter().rect_filled(rect, radius, Color32::BLACK);
            let natural = t.size;
            let scale = (rect.width() / natural.x).min(rect.height() / natural.y);
            let size = natural * scale;
            let at = Rect::from_center_size(rect.center(), size);
            egui::Image::from_texture(t)
                .corner_radius(radius)
                .paint_at(ui, at);
            click.picture = Some(at);
            core.voice.frame_shown(v.user_id);
            // Edge to edge in fullscreen, a click changes nothing (the
            // floating bar has the way out), so none is lost by accident.
            if !edge_to_edge {
                let resp = resp
                    .on_hover_cursor(CursorIcon::ZoomIn)
                    .on_hover_text(t!("stage.click_to_enlarge"));
                click.zoom = resp.clicked();
            }
            // Over the picture while the pointer is on it, as Discord has
            // them on a stream: fullscreen, and for someone else's screen,
            // to stop watching it. (Not on the small tiles under a
            // presentation, which have no room, nor edge to edge in
            // fullscreen, where the floating bar has both.)
            if ui.rect_contains_pointer(rect) && rect.height() >= 140.0 && !edge_to_edge {
                let name = u.username.clone();
                let mut right = rect.right_bottom() - vec2(10.0, 10.0);
                let id = Id::unique(("tile-fullscreen", v.user_id));
                let full = over_button(ui, id, right, Icon::Expand, None, t!("stage.fullscreen"));
                crate::smoke::target(|| format!("tela-cheia:{name}"), full.rect);
                click.fullscreen = full.clicked();
                if their_screen {
                    right.x = full.rect.left() - 6.0;
                    let id = Id::unique(("tile-stop", v.user_id));
                    let stop = over_button(
                        ui,
                        id,
                        right,
                        Icon::X,
                        Some(t!("stage.stop_watching")),
                        t!("stage.stop_watching_tip"),
                    );
                    crate::smoke::target(|| format!("parar-de-assistir:{name}"), stop.rect);
                    click.stop = stop.clicked();
                }
            }
        }
        None => {
            // Smaller in the small tiles under a presentation.
            let side = 84.0_f32.min(rect.height() * 0.6);
            // A screen not watched: who shares it, and the way back to it
            // (when the tile has room for the button).
            let offer = unwatched && rect.height() >= 150.0;
            let center = if offer {
                rect.center() - vec2(0.0, 22.0)
            } else {
                rect.center()
            };
            let av = Rect::from_center_size(center, Vec2::splat(side));
            widgets::paint_avatar(ui, core, &u, av, false, BG3);
            if offer {
                let row = Rect::from_center_size(
                    pos2(rect.center().x, av.bottom() + 26.0),
                    vec2(rect.width(), 32.0),
                );
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .max_rect(row)
                        .layout(Layout::top_down(Align::Center)),
                );
                click.watch = Button::primary(t!("stage.watch_stream"))
                    .small()
                    .show(&mut child)
                    .clicked();
            }
        }
    }
    if core.voice.speaking(v.user_id) {
        ui.painter().rect_stroke(
            rect,
            radius,
            Stroke::new(3.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    // The name tag at the bottom left: what they turned off, what I
    // silenced of them here, and whether they share a screen or present.
    let mut label = u.name().to_owned();
    if v.video == Some(Video::Screen) {
        label.push_str(if gains.screen == 0.0 {
            t!("stage.tag_screen_muted")
        } else {
            t!("stage.tag_screen")
        });
    }
    let presents = if v.user_id == core.me_id() {
        core.voice.presenting()
    } else {
        core.voice.slides().presenting(v.user_id)
    };
    if presents {
        label.push_str(t!("stage.tag_presenting"));
    }
    let galley = ui.painter().layout_no_wrap(label, theme::small(), TEXT);
    let mut off = Vec::new();
    if v.deaf {
        off.push(Icon::HeadOff);
    } else if v.mute {
        off.push(Icon::MicOff);
    }
    if gains.voice == 0.0 {
        off.push(Icon::VolumeOff);
    }
    let icon_w = icons::SMALL + 6.0;
    let tag = Rect::from_min_size(
        pos2(rect.left() + 10.0, rect.bottom() - 10.0 - 24.0),
        vec2(galley.size().x + 18.0 + icon_w * off.len() as f32, 24.0),
    );
    ui.painter().rect_filled(
        tag,
        CornerRadius::same(RADIUS_SM),
        Color32::from_black_alpha(0xaa),
    );
    let mut x = tag.left() + 9.0;
    for icon in off {
        icon.image(icons::SMALL, DANGER).paint_at(
            ui,
            Rect::from_min_size(
                pos2(x, tag.center().y - icons::SMALL / 2.0),
                Vec2::splat(icons::SMALL),
            ),
        );
        x += icon_w;
    }
    ui.painter().galley(
        pos2(x, tag.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
    let _ = Align2::LEFT_CENTER;
    click
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The call view of a maximized window on a 1920 × 1080 screen, and of
    /// the window at its own size (1240 × 780), with the members' list.
    const BIG: Vec2 = vec2(1344.0, 844.0);
    const SMALL: Vec2 = vec2(664.0, 592.0);

    fn rows(g: &Grid) -> Vec<Vec<Rect>> {
        let mut rows: Vec<Vec<Rect>> = Vec::new();
        for r in &g.rects {
            match rows.last_mut() {
                Some(row) if (row[0].top() - r.top()).abs() < 0.5 => row.push(*r),
                _ => rows.push(vec![*r]),
            }
        }
        rows
    }

    /// Same size, 16:9, inside the area, apart from each other, and each
    /// row centred.
    fn well_formed(g: &Grid, area: Vec2) {
        let first = g.rects[0];
        for r in &g.rects {
            assert!((r.width() - first.width()).abs() < 0.01, "{r:?}");
            assert!((r.width() / r.height() - ASPECT).abs() < 0.01, "{r:?}");
            assert!(r.left() >= -0.01 && r.right() <= area.x + 0.01, "{r:?}");
        }
        for (i, a) in g.rects.iter().enumerate() {
            for b in &g.rects[i + 1..] {
                assert!(!a.shrink(GAP / 2.0 - 0.01).intersects(*b), "{a:?} {b:?}");
            }
        }
        for row in rows(g) {
            let left = row[0].left();
            let right = area.x - row[row.len() - 1].right();
            assert!(
                (left - right).abs() < 0.5,
                "fileira fora do centro: {row:?}"
            );
        }
    }

    #[test]
    fn three_people_are_two_and_one_in_the_middle() {
        for area in [BIG, SMALL] {
            let g = grid(3, area);
            well_formed(&g, area);
            let rows = rows(&g);
            assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
            // The grid in the middle of the view, top and bottom too.
            let bottom = area.y - g.rects[2].bottom();
            assert!((g.rects[0].top() - bottom).abs() < 0.5, "{area:?}");
            assert!(g.height <= area.y + 0.01, "{area:?}: não deveria rolar");
        }
        // Maximized, they are big: not three thin strips.
        assert!(grid(3, BIG).rects[0].width() > 600.0);
    }

    #[test]
    fn two_people_side_by_side_when_wide_one_over_the_other_when_not() {
        let wide = rows(&grid(2, BIG));
        assert_eq!(wide.len(), 1);
        let narrow = grid(2, SMALL);
        well_formed(&narrow, SMALL);
        assert_eq!(rows(&narrow).len(), 2);
        assert!(narrow.rects[0].width() > 450.0);
    }

    #[test]
    fn bigger_calls_fill_rows_and_centre_the_rest() {
        let counts = |n: usize, area: Vec2| -> Vec<usize> {
            let g = grid(n, area);
            well_formed(&g, area);
            rows(&g).iter().map(Vec::len).collect()
        };
        assert_eq!(counts(4, BIG), [2, 2]);
        assert_eq!(counts(5, BIG), [3, 2]);
        assert_eq!(counts(6, BIG), [3, 3]);
        assert_eq!(counts(7, BIG), [3, 3, 1]);
        assert_eq!(counts(9, BIG), [3, 3, 3]);
        assert_eq!(counts(4, SMALL), [2, 2]);
    }

    #[test]
    fn alone_is_one_big_tile_in_the_middle() {
        let g = grid(1, BIG);
        well_formed(&g, BIG);
        let r = g.rects[0];
        assert!((r.center().x - BIG.x / 2.0).abs() < 0.5);
        assert!((r.center().y - BIG.y / 2.0).abs() < 0.5);
        assert!(r.width() > 1300.0 || r.height() > 740.0, "{r:?}");
    }

    #[test]
    fn too_many_to_fit_keep_their_size_and_scroll() {
        let g = grid(14, SMALL);
        well_formed(&g, SMALL);
        assert!(g.rects.iter().all(|r| r.width() >= MIN_W - 0.01));
        assert!(g.height > SMALL.y, "deveria rolar: {}", g.height);
        assert_eq!(g.rects[0].top(), 0.0);
        // Nothing to show, or nowhere to show it, is not a problem.
        assert!(grid(0, SMALL).rects.is_empty());
        assert!(grid(3, Vec2::ZERO).rects.iter().all(|r| r.width() >= 0.0));
    }
}

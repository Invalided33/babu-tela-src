//! A voice channel: a tile for everyone in the call, with their video when
//! they share, and the call's buttons underneath.

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin, Panel, Rect,
    ScrollArea, Sense, Stroke, Ui, Vec2, pos2, vec2,
};

use std::collections::{HashMap, HashSet};

use super::Acts;
use super::widgets::{self, Button, Ib};
use crate::core::{Core, Pop, PopKind};
use crate::icons::{self, Icon};
use crate::model::*;
use crate::theme::{self, *};
use crate::voice::plan::{self, Candidate, Layer};

const GAP: f32 = 10.0;

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

pub fn draw(ui: &mut Ui, core: &mut Core, acts: &mut Acts) {
    let Some((s, c)) = core.current_channel() else {
        return;
    };
    let (cid, name) = (c.id, c.label().to_owned());
    let may_join = c.can(P_CONNECT);
    let users: Vec<VoiceUser> = s.in_voice(cid).to_vec();
    let joined = core.voice.channel_id() == Some(cid);
    let show_members = core.show_members;
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
    super::header(ui, "stage-head", |ui| {
        ui.add(Icon::Speaker.tinted(MUTED));
        ui.label(
            egui::RichText::new(&name)
                .font(theme::bold(15.0))
                .color(TEXT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let style = if show_members { Ib::Normal } else { Ib::Bare };
            if widgets::icon_button(ui, Icon::Users, 34.0, style, "Lista de membros").clicked() {
                acts.later(|c| c.show_members = !c.show_members);
            }
            // Fullscreen toggle in the stage header — just the icon.
            let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
            let fs_icon = if fullscreen {
                Icon::ZoomOut
            } else {
                Icon::Expand
            };
            if widgets::icon_button(ui, fs_icon, 34.0, Ib::Bare, "").clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
            }
            if set_aside
                && Button::primary("Ver apresentação")
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
                    let width = 5.0 * 48.0 + 4.0 * GAP;
                    ui.allocate_ui_with_layout(
                        vec2(width, 48.0),
                        Layout::left_to_right(Align::Center),
                        |ui| super::sidebar::voice_buttons(ui, core, acts, 48.0, false),
                    );
                } else if may_join {
                    if Button::primary("Entrar na chamada").show(ui).clicked() {
                        acts.later(move |c| c.join_voice(cid));
                    }
                } else {
                    ui.label(
                        egui::RichText::new("Você não tem permissão para entrar neste canal.")
                            .color(MUTED),
                    );
                }
            });
        });
    let wanted = egui::CentralPanel::default()
        .frame(Frame::new().fill(BG0).inner_margin(Margin::same(16)))
        .show(ui, |ui| {
            if let Some(p) = &slides {
                return super::slides::draw(ui, core, p, &users, cid, acts);
            }
            // Someone is presenting but the viewer hasn't opted in: show
            // a banner with an "Assistir" button, then the normal tiles.
            super::slides::presenting_banner(ui, core, &users);
            if users.is_empty() {
                super::empty(ui, "Ninguém na sala", "Entre para começar a conversa.");
                return HashMap::new();
            }
            tiles(ui, core, &users, joined, cid, acts)
        })
        .inner;
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
        // view leaves none enlarged).
        let zoomed: Option<i64> = ui.data(|d| d.get_temp(zoom_id(cid)));
        let focus = zoomed.filter(|uid| {
            slides.is_none()
                && users
                    .iter()
                    .any(|u| u.user_id == *uid && u.video == Some(Video::Screen))
        });
        core.voice.set_screen_focus(focus);
    }
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
    // A clicked video fills the whole stage until clicked again.
    let zoom_id = zoom_id(cid);
    let zoomed: Option<i64> = ui.data(|d| d.get_temp(zoom_id));
    let zoomed = zoomed.filter(|uid| users.iter().any(|u| u.user_id == *uid && u.video.is_some()));
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
            let candidates: Vec<Candidate> = shown
                .iter()
                .zip(&rects)
                .filter(|(v, r)| {
                    joined && v.video.is_some() && v.user_id != me && ui.is_rect_visible(**r)
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
                if click.zoom {
                    let next = if zoomed == Some(v.user_id) {
                        None
                    } else {
                        Some(v.user_id)
                    };
                    set_zoom(ui.ctx(), cid, next);
                }
                if let Some(at) = click.menu {
                    open_person(acts, v.user_id, at);
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
}

/// One person. `video`: their picture is shown, when there is one (it is
/// being received, or it is mine).
pub(super) fn tile(ui: &mut Ui, core: &Core, v: &VoiceUser, rect: Rect, video: bool) -> TileClick {
    let u = core.user(v.user_id);
    let resp = ui.interact(rect, Id::unique(("tile", v.user_id)), Sense::click());
    let mut click = TileClick::default();
    if v.user_id != core.me_id() && resp.secondary_clicked() {
        click.menu = Some(resp.interact_pointer_pos().unwrap_or(rect.center()));
    }
    let gains = core.voice.gains(v.user_id);
    let radius = CornerRadius::same(12);
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
            let resp = resp
                .on_hover_cursor(CursorIcon::ZoomIn)
                .on_hover_text("Clique para ampliar");
            click.zoom = resp.clicked();
        }
        None => {
            // Smaller in the small tiles under a presentation.
            let side = 84.0_f32.min(rect.height() * 0.6);
            let av = Rect::from_center_size(rect.center(), Vec2::splat(side));
            widgets::paint_avatar(ui, core, &u, av, false, BG3);
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
            " · tela sem som"
        } else {
            " · tela"
        });
    }
    let presents = if v.user_id == core.me_id() {
        core.voice.presenting()
    } else {
        core.voice.slides().presenting(v.user_id)
    };
    if presents {
        label.push_str(" · apresentando");
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

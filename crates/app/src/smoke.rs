//! `"Babu Tela.exe" --smoke-report=FILE` runs the app for real, waits for it
//! to connect, plays a few scripted steps, saves a picture of the window
//! next to FILE (FILE.png), writes what it saw to FILE and quits.
//!
//! PROSA_SMOKE_TOKEN signs in with a given session (without saving it);
//! PROSA_SMOKE_ACTIONS is a list of steps separated by `|`, for example
//! `go:first-server|open:settings`. See `Smoke::act` for the steps.
//! PROSA_SMOKE_ACTIVATE clicks a notification (see the `activate` step) as
//! the app starts, before it is connected.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::core::{
    Core, ErrTo, FormId, FriendTab, Gate, Modal, Phase, Pop, PopKind, ShareTab, StickerTab, Then,
    View,
};
use crate::net::Req;

/// Where things were drawn on the last frame, by name, for the steps that
/// point, click and type as a person would (`hover:`, `click:`). Only
/// filled in this test mode.
static TARGETS: LazyLock<Mutex<HashMap<String, egui::Rect>>> = LazyLock::new(Default::default);

/// Notes where `name` was drawn this frame, for `click:<name>` (in this
/// test mode only; `name` is not even made otherwise). The names: every
/// button by its text (`botao:Criar`), the server menu's items
/// (`menu:Criar canal`), and in the side list the server's name at the top
/// (`cabecalho`), a channel's row (`canal:geral`), its gear
/// (`engrenagem:geral`, there while the pointer is on the row) and a
/// category's "+" (`mais:Canais de voz`).
pub fn target(name: impl FnOnce() -> String, rect: egui::Rect) {
    if active() {
        TARGETS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name(), rect);
    }
}

/// Where target `name` was drawn on the last frame.
fn target_rect(name: &str) -> Option<egui::Rect> {
    TARGETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
        .copied()
}

/// The page that `who` (a user name) shares and I see now: their screen,
/// or the slide of their presentation on screen (the one they are on).
fn ink_page(core: &Core, who: &str) -> Option<crate::ink::Page> {
    let uid = core
        .users
        .values()
        .find(|u| u.username == who)
        .map(|u| u.id)
        .or_else(|| core.me.as_ref().filter(|m| m.username == who).map(|m| m.id))?;
    let ink = core.voice.ink();
    if let Some(surface) = ink.screen_of(uid) {
        return Some(crate::ink::Page { surface, n: 0 });
    }
    let slides = core.voice.slides();
    let view = if uid == core.me_id() {
        slides.mine()
    } else {
        slides
            .theirs()
            .into_iter()
            .find(|v| v.owner == crate::slides::Owner::Them(uid))
    }?;
    let surface = ink.slides_of(uid, view.id)?;
    Some(crate::ink::Page {
        surface,
        n: view.at as u32,
    })
}

/// A point for `hover:`/`click:`: `<x>:<y>` in points, or the middle of a
/// target drawn on the last frame.
fn point(arg: &str) -> Option<egui::Pos2> {
    if let Some((x, y)) = arg.split_once(':')
        && let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>())
    {
        return Some(egui::pos2(x, y));
    }
    TARGETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(arg)
        .map(|r| r.center())
}

enum Stage {
    WaitReady,
    Settle(Instant),
    Act(usize, Instant),
    /// Since when the picture is awaited, and when it was last asked for.
    Shoot(Instant, Instant),
}

pub struct Smoke {
    report: PathBuf,
    actions: Vec<String>,
    started: Instant,
    stage: Stage,
    done: Vec<String>,
    shot: Arc<Mutex<Option<Arc<egui::ColorImage>>>>,
    /// PROSA_SMOKE_ACTIVATE: a click on a notification made as the app
    /// starts, before it is connected, the way Windows hands one over
    /// when it starts the app for it.
    early_click: Option<String>,
    /// How the last click on a notification went ("ok" or why not).
    clicked: Arc<Mutex<Option<String>>>,
    /// What Windows kept of a test notification (its launch), or why not.
    toast_history: Arc<Mutex<Option<serde_json::Value>>>,
    /// The call as it was at the `note` steps, by label.
    notes: serde_json::Map<String, serde_json::Value>,
    /// Since when a step that waits (`holds`) has been holding the run.
    holding: Option<Instant>,
    /// When the `mark` steps happened (milliseconds since 1970), so the
    /// measurements of `cargo xtask medir` can cut the run into phases.
    marks: serde_json::Map<String, serde_json::Value>,
    /// The messages a `burst` step still has to send.
    burst: Option<Burst>,
    /// Input of the `hover`, `click`, `keys` and `enter` steps still to
    /// give the window, one batch a frame (a press and its release in the
    /// same frame would not be a click).
    input: VecDeque<Vec<egui::Event>>,
    /// The targets the steps asked for that were not on the screen.
    misses: Vec<String>,
}

/// Messages sent one after another, each saying when it left.
struct Burst {
    left: usize,
    every: Duration,
    next: Instant,
    sent: usize,
}

/// Time between steps, for the server to answer and the screen to settle.
const STEP: Duration = Duration::from_millis(1500);

/// Whether the app runs in this test mode (some test-only doors open only
/// then, like playing a file from the disk).
pub fn active() -> bool {
    static ACTIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ACTIVE.get_or_init(|| {
        std::env::args()
            .skip(1)
            .any(|a| a.starts_with("--smoke-report="))
    })
}

impl Smoke {
    pub fn from_args() -> Option<Smoke> {
        let report = std::env::args()
            .skip(1)
            .find_map(|a| a.strip_prefix("--smoke-report=").map(PathBuf::from))?;
        let actions = std::env::var("PROSA_SMOKE_ACTIONS")
            .unwrap_or_default()
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        Some(Smoke {
            report,
            actions,
            started: Instant::now(),
            stage: Stage::WaitReady,
            done: Vec::new(),
            shot: Arc::new(Mutex::new(None)),
            early_click: std::env::var("PROSA_SMOKE_ACTIVATE")
                .ok()
                .filter(|s| !s.is_empty()),
            clicked: Arc::new(Mutex::new(None)),
            toast_history: Arc::new(Mutex::new(None)),
            notes: serde_json::Map::new(),
            holding: None,
            marks: serde_json::Map::new(),
            burst: None,
            input: VecDeque::new(),
            misses: Vec::new(),
        })
    }

    pub fn frame(&mut self, ctx: &egui::Context, core: &mut Core) {
        self.step(ctx, core);
        // Drawn again from here on: what is not drawn this frame is gone.
        TARGETS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        ctx.request_repaint_after(self.next_wake());
    }

    /// Gives the window the next batch of the steps' input (from
    /// `raw_input_hook`, before the frame).
    pub fn feed(&mut self, raw: &mut egui::RawInput) {
        if let Some(batch) = self.input.pop_front() {
            raw.events.extend(batch);
        }
    }

    /// When the run needs a frame again. Measuring, only when a step is
    /// due, so an idle app is measured idle; otherwise ten times a second.
    fn next_wake(&self) -> Duration {
        let often = Duration::from_millis(100);
        if !self.input.is_empty() {
            return Duration::from_millis(1);
        }
        if !crate::probe::on() {
            return often;
        }
        let now = Instant::now();
        let until = match &self.stage {
            Stage::Act(i, _) => self
                .actions
                .get(*i)
                .and_then(|s| s.strip_prefix("until:"))
                .and_then(|ms| ms.parse::<f64>().ok()),
            _ => None,
        };
        match (&self.stage, &self.burst, until) {
            (_, Some(b), _) => b.next.saturating_duration_since(now),
            (Stage::Act(_, at), None, Some(ms)) => {
                let wall =
                    Duration::from_secs_f64((ms - crate::probe::wall_ms()).max(0.0) / 1000.0);
                wall.max((*at + STEP).saturating_duration_since(now))
            }
            (Stage::Act(..), None, None) if self.holding.is_some() => often,
            (Stage::Act(_, at), None, None) => (*at + STEP).saturating_duration_since(now),
            _ => often,
        }
        .max(Duration::from_millis(1))
    }

    fn step(&mut self, ctx: &egui::Context, core: &mut Core) {
        // PROSA_SMOKE_SECONDS gives a long run (a big upload) more time.
        let limit = std::env::var("PROSA_SMOKE_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(90);
        if self.started.elapsed() > Duration::from_secs(limit) {
            self.finish(core, None, "tempo esgotado");
        }
        match self.stage {
            Stage::WaitReady => {
                if let Some(target) = self.early_click.take() {
                    self.click(launch_for(core, &target));
                }
                // The startup update's screen is a screen to see as well.
                let settled = core.phase == Phase::Ready
                    || (core.phase == Phase::Auth && !core.api.signed_in())
                    || matches!(core.gate, Gate::Updating { .. });
                // Settles anyway after a while, so a run that never connects
                // still reports what it saw.
                if settled || self.started.elapsed() > Duration::from_secs(45) {
                    self.stage = Stage::Settle(Instant::now());
                }
            }
            Stage::Settle(at) => {
                if at.elapsed() > Duration::from_millis(2500) {
                    self.stage = Stage::Act(0, Instant::now() - STEP);
                }
            }
            Stage::Act(i, at) => {
                if at.elapsed() < STEP {
                    return;
                }
                if let Some(step) = self.actions.get(i).cloned() {
                    if self.holds(core, &step) {
                        return;
                    }
                    let wait = self.act(core, &step);
                    self.done.push(step);
                    self.stage = Stage::Act(i + 1, Instant::now() + wait);
                } else {
                    self.ask_for_shot(ctx);
                    self.stage = Stage::Shoot(Instant::now(), Instant::now());
                }
            }
            Stage::Shoot(at, asked) => {
                // A frame the graphics card dropped takes the request
                // with it: asked again until a picture comes.
                if asked.elapsed() > Duration::from_millis(1500) {
                    crate::logln!("smoke: a imagem da janela não veio; pedindo de novo");
                    self.ask_for_shot(ctx);
                    self.stage = Stage::Shoot(at, Instant::now());
                }
                let shot = self
                    .shot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(image) = shot {
                    let path = self.report.with_extension("png");
                    let [w, h] = image.size;
                    let saved =
                        image::RgbaImage::from_raw(w as u32, h as u32, image.as_raw().to_vec())
                            .is_some_and(|img| img.save(&path).is_ok());
                    self.finish(core, saved.then_some(path), "");
                } else if at.elapsed() > Duration::from_secs(6) {
                    self.finish(core, None, "sem captura de tela");
                } else {
                    ctx.request_repaint();
                }
            }
        }
    }

    /// Asks for a picture of the window, which the next frame drawn takes.
    fn ask_for_shot(&self, ctx: &egui::Context) {
        let slot = self.shot.clone();
        let wake = ctx.clone();
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
            egui::ScreenshotCallback::new(move |image| {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(image);
                wake.request_repaint();
            }),
        ));
    }

    /// Steps that wait for something hold the run, checked on every frame,
    /// until it happens or their time is up. `wait-sent:<seconds>`: every
    /// message with files has gone out (or failed). `burst:<n>:<ms>`: n
    /// messages, one every ms, each saying when it left (`medir:<ms>:<i>`),
    /// for `cargo xtask medir`.
    fn holds(&mut self, core: &mut Core, step: &str) -> bool {
        // until:<ms since 1970>: copies of the app measured together take
        // their steps at the same moments.
        if let Some(at) = step.strip_prefix("until:") {
            return crate::probe::wall_ms() < at.parse().unwrap_or(0.0);
        }
        if let Some(rest) = step.strip_prefix("burst:") {
            let (n, every) = rest.split_once(':').unwrap_or((rest, "1100"));
            let burst = self.burst.get_or_insert_with(|| Burst {
                left: n.parse().unwrap_or(10),
                every: Duration::from_millis(every.parse().unwrap_or(1100)),
                next: Instant::now(),
                sent: 0,
            });
            if burst.left == 0 {
                self.burst = None;
                return false;
            }
            if Instant::now() >= burst.next {
                if let Some(cid) = core.view.channel() {
                    *core.draft(cid) =
                        format!("medir:{:.1}:{}", crate::probe::wall_ms(), burst.sent);
                    core.send_message();
                }
                burst.sent += 1;
                burst.left -= 1;
                burst.next += burst.every;
            }
            return true;
        }
        // wait-emoji:<seconds>: the emoji list is in (or failed), or the
        // time is up.
        if let Some(secs) = step.strip_prefix("wait-emoji:") {
            let since = *self.holding.get_or_insert_with(Instant::now);
            let limit = Duration::from_secs(secs.parse().unwrap_or(60));
            if matches!(core.emoji.now(), crate::emoji::Now::Loading) && since.elapsed() < limit {
                core.emoji.want();
                return true;
            }
            self.holding = None;
            return false;
        }
        // wait-anims:<n>:<seconds>: n sticker animations are ready to show
        // (for a picture with them moving), or the time is up.
        if let Some(rest) = step.strip_prefix("wait-anims:") {
            let (n, secs) = rest.split_once(':').unwrap_or((rest, "20"));
            let since = *self.holding.get_or_insert_with(Instant::now);
            let ready = core.giphy.report()["anims"].as_u64().unwrap_or(0);
            let limit = Duration::from_secs(secs.parse().unwrap_or(20));
            if ready < n.parse().unwrap_or(1) && since.elapsed() < limit {
                return true;
            }
            self.holding = None;
            return false;
        }
        // wait-direto-msg:<seconds>: a message of someone else's with a file
        // sent directly (direct/) is in the open conversation.
        if let Some(secs) = step.strip_prefix("wait-direto-msg:") {
            let none = newest_direct(core).is_none();
            return self.hold_while(secs, 60, none);
        }
        // wait-direto-pct:<pct>:<seconds>: the newest download of a file
        // sent directly is that far (or no longer going).
        if let Some(rest) = step.strip_prefix("wait-direto-pct:") {
            let (pct, secs) = rest.split_once(':').unwrap_or((rest, "120"));
            let pct: f64 = pct.parse().unwrap_or(50.0);
            let report = core.direct.report();
            let far = report["downloads"]
                .as_array()
                .and_then(|l| l.last())
                .is_some_and(|d| {
                    let size = d["size"].as_f64().unwrap_or(0.0);
                    let done = d["done"].as_f64().unwrap_or(0.0);
                    d["state"] != "active" || done * 100.0 >= pct * size
                });
            return self.hold_while(secs, 120, !far);
        }
        // wait-direto:<seconds>: no download of a file sent directly is
        // going any more (whole, failed or paused).
        if let Some(secs) = step.strip_prefix("wait-direto:") {
            let going = core.direct.report()["downloads"]
                .as_array()
                .is_some_and(|l| l.iter().any(|d| d["state"] == "active"));
            return self.hold_while(secs, 600, going);
        }
        // wait-servido:<n>:<seconds>: n sends of my files sent directly
        // went out whole.
        if let Some(rest) = step.strip_prefix("wait-servido:") {
            let (n, secs) = rest.split_once(':').unwrap_or((rest, "600"));
            let whole = core.direct.report()["completed"].as_u64().unwrap_or(0);
            return self.hold_while(secs, 600, whole < n.parse().unwrap_or(1));
        }
        let Some(secs) = step.strip_prefix("wait-sent:") else {
            return false;
        };
        let limit = Duration::from_secs(secs.parse().unwrap_or(60));
        let since = *self.holding.get_or_insert_with(Instant::now);
        if !core.sending.is_empty() && since.elapsed() < limit {
            return true;
        }
        self.holding = None;
        false
    }

    /// Holds the run while `waiting`, for at most `secs` seconds (`default`
    /// when it does not say).
    fn hold_while(&mut self, secs: &str, default: u64, waiting: bool) -> bool {
        let since = *self.holding.get_or_insert_with(Instant::now);
        let limit = Duration::from_secs(secs.parse().unwrap_or(default));
        if waiting && since.elapsed() < limit {
            return true;
        }
        self.holding = None;
        false
    }

    /// Runs one step; returns how much longer than usual to wait after it.
    fn act(&mut self, core: &mut Core, step: &str) -> Duration {
        let (verb, arg) = step.split_once(':').unwrap_or((step, ""));
        match (verb, arg) {
            ("go", "home") => core.go_to(View::HOME),
            ("go", "first-server") => {
                if let Some(id) = core.servers.first().map(|s| s.id) {
                    core.go_to(View::Server {
                        server: id,
                        channel: None,
                    });
                }
            }
            ("go", "first-dm") => {
                if let Some(id) = core.dms.first().map(|d| d.id) {
                    core.go_to(View::Home { channel: Some(id) });
                }
            }
            // The first text channel of the open server (a call goes on in
            // the background).
            ("go", "text") => {
                let target = core.current_server().and_then(|s| {
                    s.channels
                        .iter()
                        .find(|c| !c.is_voice())
                        .map(|c| (s.id, c.id))
                });
                if let Some((server, channel)) = target {
                    core.go_to(View::Server {
                        server,
                        channel: Some(channel),
                    });
                }
            }
            ("go", "voice") => {
                let target = core.current_server().and_then(|s| {
                    s.channels
                        .iter()
                        .find(|c| c.is_voice())
                        .map(|c| (s.id, c.id))
                });
                if let Some((server, channel)) = target {
                    core.go_to(View::Server {
                        server,
                        channel: Some(channel),
                    });
                }
            }
            ("join", _) => {
                let target = core.current_server().and_then(|s| {
                    s.channels
                        .iter()
                        .find(|c| c.is_voice())
                        .map(|c| (s.id, c.id))
                });
                if let Some((sid, cid)) = target {
                    core.open_channel(sid, cid);
                }
                return Duration::from_secs(3);
            }
            ("tab", tab) => {
                core.go_to(View::HOME);
                core.friend_tab = match tab {
                    "all" => FriendTab::All,
                    "pending" => FriendTab::Pending,
                    "blocked" => FriendTab::Blocked,
                    "add" => FriendTab::Add,
                    _ => FriendTab::Online,
                };
            }
            ("open", "settings") => core.open_settings(),
            // "Continuar com o Google" on the sign-in card. With
            // BABUTELA_GOOGLE_AUTH_URL, a fake Google answers instead of the
            // browser (google.rs).
            ("google", _) => {
                core.google_sign_in();
                return Duration::from_secs(4);
            }
            ("logout", _) => {
                core.logout(false);
                return Duration::from_secs(1);
            }
            // scroll-modal:<px> scrolls the open dialog (to the end with
            // a large number), for the picture.
            ("scroll-modal", px) => core.modal_scroll = Some(px.parse().unwrap_or(100_000.0)),
            ("open", "server-settings") => {
                if let Some(sid) = core.view.server() {
                    core.open_server_settings(sid);
                }
            }
            ("open", "add-server") => core.open_modal(Modal::AddServer {
                name: String::new(),
                code: String::new(),
            }),
            ("open", "licencas") => core.open_modal(Modal::Licenses),
            ("open", "profile") => {
                let uid = core.me_id();
                core.open_modal(Modal::Profile { uid });
            }
            ("open", "edit-channel") => {
                if let Some(cid) = core.view.channel() {
                    core.open_edit_channel(cid);
                }
            }
            ("pop", "server-menu") => {
                core.pop = Some(Pop {
                    kind: PopKind::ServerMenu,
                    anchor: egui::Rect::from_min_size(
                        egui::pos2(80.0, 40.0),
                        egui::vec2(240.0, 50.0),
                    ),
                });
            }
            // The composer's emoji picker, opened by its button at the
            // bottom right of the conversation.
            ("pop", "emoji") => {
                let window = core.ctx.content_rect();
                core.open_emoji(
                    PopKind::Emoji,
                    egui::Rect::from_min_size(
                        egui::pos2(window.right() - 24.0 - 34.0, window.bottom() - 22.0 - 34.0),
                        egui::vec2(34.0, 34.0),
                    ),
                );
            }
            // The reaction picker of the newest message in the open
            // channel, opened from the middle of the window.
            ("pop", "react") => {
                let last = core
                    .view
                    .channel()
                    .and_then(|c| core.msgs.get(&c))
                    .and_then(|m| m.list.last())
                    .map(|m| m.id);
                if let Some(mid) = last {
                    let center = core.ctx.content_rect().center();
                    core.open_emoji(
                        PopKind::React(mid),
                        egui::Rect::from_min_size(
                            center - egui::vec2(0.0, 200.0),
                            egui::vec2(28.0, 28.0),
                        ),
                    );
                } else {
                    // Without a message the keys typed next would go to
                    // the composer instead.
                    self.misses.push(step.to_owned());
                }
            }
            // The sticker picker, opened by the composer's button: next to
            // the emoji one, at the bottom right of the conversation.
            ("pop", "sticker") => {
                let window = core.ctx.content_rect();
                core.open_stickers(egui::Rect::from_min_size(
                    egui::pos2(window.right() - 24.0 - 68.0, window.bottom() - 22.0 - 34.0),
                    egui::vec2(34.0, 34.0),
                ));
            }
            // sticker-search:<words> searches in the open picker, at once.
            ("sticker-search", words) => {
                core.sticker_query = words.to_owned();
                core.sticker_typed = None;
                return Duration::from_secs(1);
            }
            // sticker-tab:favoritas or sticker-tab:em-alta.
            ("sticker-tab", tab) => {
                core.sticker_query.clear();
                core.sticker_tab = if tab == "favoritas" {
                    StickerTab::Favorites
                } else {
                    StickerTab::Trending
                };
            }
            // send-sticker:<GIPHY id>, or :first for the picker's first.
            ("send-sticker", id) => {
                if let Some(id) = sticker_for(core, id) {
                    core.send_sticker(&id);
                }
                return Duration::from_secs(2);
            }
            // favorite:<GIPHY id> (or :first) keeps it, or lets it go.
            ("favorite", id) => {
                if let Some(id) = sticker_for(core, id) {
                    core.toggle_favorite(&id);
                }
                return Duration::from_secs(2);
            }
            // sticker-message:<GIPHY id> (or :first) shows a message that
            // is this sticker in the open channel, as if someone had sent
            // it: tests the drawing without the server.
            ("sticker-message", id) => {
                if let (Some(cid), Some(id)) = (core.view.channel(), sticker_for(core, id)) {
                    let msg = crate::model::Message {
                        id: i64::MAX - 100 - self.done.len() as i64,
                        channel_id: cid,
                        author: core.me.clone(),
                        created: chrono::Utc::now().timestamp_millis(),
                        sticker: Some(id),
                        ..Default::default()
                    };
                    core.msgs.entry(cid).or_default().list.push(msg);
                    core.stick_bottom = true;
                }
            }
            ("create-server", name) => {
                core.call(
                    Req::post("/servers").with(json!({ "name": name })),
                    Then::Arrive,
                    ErrTo::Form(FormId::CreateServer),
                );
                return Duration::from_secs(3);
            }
            // Reacts to the newest message in the open channel.
            ("react", emoji) => {
                let last = core
                    .view
                    .channel()
                    .and_then(|c| core.msgs.get(&c))
                    .and_then(|m| m.list.last())
                    .map(|m| m.id);
                if let Some(mid) = last {
                    core.react(mid, emoji);
                }
                return Duration::from_secs(2);
            }
            ("type", text) => {
                if let Some(cid) = core.view.channel() {
                    *core.draft(cid) = text.to_owned();
                }
            }
            ("send", text) => {
                if let Some(cid) = core.view.channel() {
                    *core.draft(cid) = text.to_owned();
                    core.send_message();
                }
                return Duration::from_secs(2);
            }
            // attach:<path>;<path>... puts files of this PC on the next
            // message, like the clip button.
            ("attach", paths) => {
                core.add_files(
                    paths
                        .split(';')
                        .filter(|p| !p.is_empty())
                        .map(PathBuf::from)
                        .collect(),
                );
            }
            // The X on the newest send's bar.
            ("cancel-send", _) => {
                if let Some(id) = core.sending.last().map(|s| s.id) {
                    core.cancel_send(id);
                }
                return Duration::from_secs(2);
            }
            // wait-sent:<seconds> holds the run until every send is over,
            // and burst:<n>:<ms> until its messages went out (see `holds`);
            // nothing left to do once they get here.
            (
                "wait-sent" | "burst" | "until" | "wait-anims" | "wait-emoji" | "wait-direto-msg"
                | "wait-direto-pct" | "wait-direto" | "wait-servido",
                _,
            ) => {}
            // direto:on|off: the composer's "Enviar direto", for the files
            // attached next.
            ("direto", on) => core.direct_send = on == "on",
            // baixar:<folder>: downloads the newest file sent directly in
            // the open conversation (someone else's) into that folder, as
            // "Baixar" and its dialog would.
            ("baixar", folder) => match newest_direct(core) {
                Some((m, a)) => {
                    let path = PathBuf::from(folder).join(&a.name);
                    let (mid, cid, from) = (m.id, m.channel_id, m.author_id());
                    if let Err(e) = core.direct.download(&a, mid, cid, from, path) {
                        core.toast(e);
                    }
                }
                None => self.misses.push(step.to_owned()),
            },
            // pausar-direto / continuar-direto: "Pausar" and "Continuar" on
            // the newest download.
            ("pausar-direto" | "continuar-direto", _) => {
                let newest = core.direct.report()["downloads"]
                    .as_array()
                    .and_then(|l| l.last())
                    .and_then(|d| d["id"].as_str().map(str::to_owned));
                match newest {
                    Some(id) if verb == "pausar-direto" => core.direct.pause(&id),
                    Some(id) => core.direct.resume(&id),
                    None => self.misses.push(step.to_owned()),
                }
            }
            // emoji-search:<words> searches in the open emoji picker.
            ("emoji-search", words) => {
                core.emoji_query = words.to_owned();
                core.emoji_pairs = None;
            }
            // emoji-pares:<emoji> shows the tones of an emoji of two
            // people, one for each, as a right click on it does.
            ("emoji-pares", emoji) => core.emoji_pairs = Some(emoji.to_owned()),
            // emoji-tone:<0 to 5> gives the picker that skin tone.
            ("emoji-tone", t) => core.set_emoji_tone(t.parse().unwrap_or(0)),
            // emoji-tab:<n> goes to the picker's n-th tab (from 0).
            ("emoji-tab", n) => {
                core.emoji_query.clear();
                core.emoji_jump = n.parse().ok();
            }
            // mark:<label> notes when this moment came, for `cargo xtask
            // medir` to cut the run into phases.
            ("mark", label) => {
                self.marks
                    .insert(label.to_owned(), json!(crate::probe::wall_ms()));
                crate::probe::phase(label);
            }
            // place:<x>:<y>:<width>:<height> moves and sizes the window, so
            // copies of the app measured side by side do not cover each
            // other.
            ("place", rest) => {
                let n: Vec<f32> = rest.split(':').filter_map(|v| v.parse().ok()).collect();
                if let [x, y, w, h] = n[..] {
                    core.ctx
                        .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
                    core.ctx
                        .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
                }
            }
            // maximizar: the window maximized, as it opens outside the
            // tests; maximizar:off, back to its size.
            ("maximizar", off) => {
                core.ctx
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(off != "off"));
                return Duration::from_secs(1);
            }
            // tela-cheia:on|off: the call view's fullscreen button.
            ("tela-cheia", on) => {
                crate::ui::stage::set_fullscreen(&core.ctx, on == "on");
                return Duration::from_secs(2);
            }
            // parar-de-assistir:<user name> and assistir:<user name>: the
            // buttons on their screen share's tile.
            ("parar-de-assistir" | "assistir", who) => {
                match core
                    .users
                    .values()
                    .find(|u| u.username == who)
                    .map(|u| u.id)
                {
                    Some(uid) => core.voice.set_watching(uid, verb == "assistir"),
                    None => self.misses.push(step.to_owned()),
                }
                return Duration::from_secs(2);
            }
            // share:camera, share:screen (the main one), or
            // share:window:<part of its title>.
            ("share", kind) => {
                if kind == "self" {
                    // This very window (the measurements' stamp is drawn
                    // across its top).
                    if let Some(hwnd) = core.shell.hwnd() {
                        core.voice
                            .share(Some(crate::voice::sources::Source::Window(hwnd)));
                    }
                } else if let Some(title) = kind.strip_prefix("window:") {
                    let found = windows_capture::window::Window::enumerate()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|w| w.title().is_ok_and(|t| t.contains(title)));
                    match found {
                        Some(w) => core.voice.share(Some(crate::voice::sources::Source::Window(
                            w.as_raw_hwnd() as isize,
                        ))),
                        None => crate::logln!("smoke: nenhuma janela com \"{title}\""),
                    }
                } else {
                    // Measuring, the main screen goes the way asked, to
                    // compare: share:screen through Graphics Capture and
                    // openh264 on the processor, share:screen-dxgi through
                    // Desktop Duplication instead (voice/dxgi.rs), and
                    // share:screen-zero all on the graphics card
                    // (voice/hw.rs). Otherwise all three are the usual
                    // share, on the card when it can.
                    crate::voice::dxgi::WANTED
                        .store(kind == "screen-dxgi", std::sync::atomic::Ordering::Relaxed);
                    crate::voice::hw::WANTED
                        .store(kind == "screen-zero", std::sync::atomic::Ordering::Relaxed);
                    let video = match kind {
                        "camera" => crate::model::Video::Camera,
                        _ => crate::model::Video::Screen,
                    };
                    core.voice.set_video(Some(video));
                }
                return Duration::from_secs(2);
            }
            // topo: this window above all others, so a whole screen shared
            // shows it whatever else is open (measuring a screen share needs
            // the stamp on the screen).
            ("topo", _) => {
                core.ctx
                    .send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                        egui::WindowLevel::AlwaysOnTop,
                    ));
            }
            // carimbo:on draws the measurements' stamp while a whole screen
            // is shared too (across this window, which then has to span the
            // screen's width).
            ("carimbo", on) => {
                crate::probe::STAMP_SCREEN.store(on == "on", std::sync::atomic::Ordering::Relaxed);
            }
            // zoom:<user name> enlarges their video in the open call view
            // (as a click on it does); "zoom:" alone puts it back.
            ("zoom", who) => {
                if let Some(cid) = core.view.channel() {
                    let uid = core
                        .users
                        .values()
                        .find(|u| !who.is_empty() && u.username == who)
                        .map(|u| u.id);
                    crate::ui::stage::set_zoom(&core.ctx, cid, uid);
                }
                return Duration::from_secs(1);
            }
            ("pop", "share") => {
                core.open_share();
                return Duration::from_secs(3);
            }
            // share-audio:on|off: whether the PC's sound goes with a
            // screen share (the share picker's box).
            ("share-audio", on) => core.set_share_audio(on == "on"),
            // qualidade:<720p30|1080p30|1080p60|source>: the share picker's
            // quality (a share going on starts again in it).
            ("qualidade", key) => {
                core.set_quality(key);
                return Duration::from_secs(2);
            }
            // device:input|output:<part of its name>, or "padrao" for
            // Windows' default, as the settings pick it (open:settings
            // first, which lists them).
            ("device", rest) => {
                let (kind, part) = rest.split_once(':').unwrap_or((rest, "padrao"));
                let list = core.audio_devices.as_ref().map(|d| {
                    if kind == "input" {
                        d.inputs.clone()
                    } else {
                        d.outputs.clone()
                    }
                });
                let device = if part == "padrao" {
                    None
                } else {
                    list.unwrap_or_default()
                        .into_iter()
                        .find(|d| d.name.contains(part))
                };
                if part != "padrao" && device.is_none() {
                    crate::logln!("smoke: nenhum dispositivo com \"{part}\"");
                } else if kind == "input" {
                    core.set_input_device(device);
                } else {
                    core.set_output_device(device);
                }
                return Duration::from_secs(2);
            }
            // ruido:ia|padrao|desligada: the noise suppression, as the
            // settings pick it.
            ("ruido", key) => match crate::voice::Noise::by_key(key) {
                Some(noise) => core.set_noise(noise),
                None => crate::logln!("smoke: supressão de ruído desconhecida: {key}"),
            },
            // silence:<user name>:voice|screen silences them here, as the
            // right-click menu does.
            ("silence", rest) => {
                let (who, what) = rest.split_once(':').unwrap_or((rest, "voice"));
                if let Some(uid) = core
                    .users
                    .values()
                    .find(|u| u.username == who)
                    .map(|u| u.id)
                {
                    let mut audio = core.person_audio(uid);
                    if what == "screen" {
                        audio.screen_muted = true;
                    } else {
                        audio.voice_muted = true;
                    }
                    core.set_person_audio(uid, audio, true);
                }
            }
            // person:<user name> opens their menu in the middle of the
            // window, for the picture.
            ("person", who) => {
                if let Some(uid) = core
                    .users
                    .values()
                    .find(|u| u.username == who)
                    .map(|u| u.id)
                {
                    let center = core.ctx.content_rect().center();
                    core.pop = Some(Pop {
                        kind: PopKind::Person(uid),
                        anchor: egui::Rect::from_min_size(center, egui::Vec2::ZERO),
                    });
                }
            }
            // Closes a window by its title, as if its X were clicked: tests
            // what happens to a share when the window goes away.
            ("close-window", title) => {
                let found = windows_capture::window::Window::enumerate()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|w| w.title().is_ok_and(|t| t.contains(title)));
                if let Some(w) = found {
                    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
                    // SAFETY: posting a message to a window handle; a stale
                    // one just fails.
                    unsafe {
                        PostMessageW(w.as_raw_hwnd() as _, WM_CLOSE, 0, 0);
                    }
                }
                return Duration::from_secs(1);
            }
            // A message with these files of this PC attached (separated by
            // ";"), shown in the open channel as if someone had sent it:
            // tests the players without uploading anything.
            ("media", paths) => {
                if let Some(cid) = core.view.channel() {
                    let attachments = paths
                        .split(';')
                        .filter(|p| !p.is_empty())
                        .map(|p| {
                            let name = std::path::Path::new(p)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                            let kind = match ext.as_str() {
                                "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "wmv" => {
                                    crate::model::Kind::Video
                                }
                                "mp3" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "flac"
                                | "weba" => crate::model::Kind::Audio,
                                _ => crate::model::Kind::File,
                            };
                            crate::model::Attachment {
                                url: format!("file:///{}", p.replace('\\', "/")),
                                size: std::fs::metadata(p).map_or(0, |m| m.len() as i64),
                                name,
                                kind,
                                direct: None,
                            }
                        })
                        .collect();
                    let msg = crate::model::Message {
                        id: i64::MAX - 1,
                        channel_id: cid,
                        author: core.me.clone(),
                        created: chrono::Utc::now().timestamp_millis(),
                        attachments,
                        ..Default::default()
                    };
                    core.msgs.entry(cid).or_default().list.push(msg);
                }
            }
            // video-teste:<path> writes a moving 720p video there (once),
            // for `media:` to play.
            ("video-teste", path) => {
                let path = std::path::PathBuf::from(path);
                if !path.exists() {
                    // On a thread of its own: this one has COM set up for
                    // the window already, in another mode.
                    let made =
                        std::thread::spawn(move || crate::media::mf::write_test_video(&path, 12))
                            .join();
                    if let Ok(Err(e)) = made {
                        crate::logln!("vídeo de teste: {e}");
                    }
                }
            }
            // Plays the n-th attachment of the newest message.
            ("play", n) => {
                if let Some(a) = newest_attachment(core, n) {
                    core.media.play(&a.url, &a.name, a.kind);
                }
                return Duration::from_secs(2);
            }
            // seek:<n>:<seconds> moves the n-th attachment's player.
            ("seek", rest) => {
                let (n, t) = rest.split_once(':').unwrap_or((rest, "0"));
                let url = newest_attachment(core, n).map(|a| a.url);
                if let Some(p) = url.and_then(|u| core.media.get(&u)) {
                    p.seek(t.parse().unwrap_or(0.0));
                }
            }
            // The enlarged view of the n-th attachment of the newest message.
            ("enlarge", n) => {
                if let Some(attachment) = newest_attachment(core, n) {
                    core.open_modal(Modal::Player { attachment });
                }
            }
            // A tab of the picker: share-tab:screens|windows|slides.
            ("share-tab", which) => {
                if let Some(Modal::Share { tab, .. }) = &mut core.modal {
                    *tab = match which {
                        "windows" => ShareTab::Windows,
                        "slides" => ShareTab::Slides,
                        _ => ShareTab::Screens,
                    };
                }
            }
            // apresentar:<path> presents that .pptx or PDF in the call, as
            // the picker does; apresentar:fim stops.
            ("apresentar", "fim") => core.voice.stop_presenting(),
            ("apresentar", path) => {
                core.present(std::path::PathBuf::from(path));
                return Duration::from_secs(2);
            }
            // slide:<n> takes my presentation to slide n (from 1).
            ("slide", n) => {
                if let Ok(n) = n.parse::<usize>() {
                    core.voice.go_to_slide(n.saturating_sub(1));
                }
            }
            // anotar:<user name> opens the annotations' toolbar over what
            // they share (their screen, or the slide of theirs on screen),
            // as its "Anotar" button does; anotar:fim closes it.
            ("anotar", "fim") => core.voice.ink().close(),
            ("anotar", who) => match ink_page(core, who) {
                Some(page) => core.voice.ink().open(page.surface),
                None => {
                    crate::logln!("smoke: {who} não compartilha nada que se anote");
                    self.misses.push(step.to_owned());
                }
            },
            // ferramenta:<vanish|pen|marker|arrow|rect|ellipse|eraser> and
            // cor:<0 to 7>: the pen, as the toolbar picks it.
            ("ferramenta", name) => {
                let ink = core.voice.ink();
                let tool = crate::ink::Tool::from_wire(name)
                    .or((name == "eraser").then_some(crate::ink::Tool::Eraser));
                match tool {
                    Some(tool) => ink.set_pen(crate::ink::Pen { tool, ..ink.pen() }),
                    None => crate::logln!("smoke: ferramenta desconhecida: {name}"),
                }
            }
            ("cor", n) => {
                let ink = core.voice.ink();
                ink.set_pen(crate::ink::Pen {
                    color: n.parse().unwrap_or(0),
                    ..ink.pen()
                });
            }
            // traco:<x>,<y>;<x>,<y>;... draws a stroke of the pen there (0
            // to 1 across and down) over what the open toolbar annotates.
            ("traco", path) => {
                let ink = core.voice.ink();
                let points: Vec<egui::Pos2> = path
                    .split(';')
                    .filter_map(|p| p.split_once(','))
                    .filter_map(|(x, y)| Some(egui::pos2(x.parse().ok()?, y.parse().ok()?)))
                    .collect();
                let page = ink.active().and_then(|s| {
                    let who = core.users.get(&s.owner).map(|u| u.username.clone())?;
                    ink_page(core, &who).filter(|p| p.surface == s)
                });
                match (page, points.split_first()) {
                    (Some(page), Some((first, rest))) if ink.begin(page, *first) => {
                        for p in rest {
                            ink.extend(*p);
                        }
                        ink.finish();
                    }
                    _ => {
                        crate::logln!("smoke: o traço não foi desenhado: {path}");
                        self.misses.push(step.to_owned());
                    }
                }
            }
            // limpar:meus|tudo, desfazer and refazer, as on the toolbar.
            ("limpar", which) => {
                let ink = core.voice.ink();
                if let Some(surface) = ink.active() {
                    ink.clear_surface(surface, which == "tudo");
                }
            }
            ("desfazer", _) => core.voice.ink().undo(),
            ("refazer", _) => core.voice.ink().redo(),
            // so-eu:on|off: who may annotate what I share (my screen, or
            // my presentation).
            ("so-eu", on) => {
                let ink = core.voice.ink();
                let mine = [crate::ink::Kind::Screen, crate::ink::Kind::Slides]
                    .into_iter()
                    .filter_map(|k| ink.mine(k));
                for surface in mine {
                    ink.set_everyone(surface, on != "on");
                }
            }
            // arrastar:<where> drags the pointer across it, from a third of
            // the way in to two thirds, as a hand drawing would.
            ("arrastar", at) => match target_rect(at) {
                Some(r) => {
                    let from = r.lerp_inside(egui::vec2(0.3, 0.35));
                    let to = r.lerp_inside(egui::vec2(0.7, 0.6));
                    self.input.push_back(vec![egui::Event::PointerMoved(from)]);
                    self.input.push_back(vec![egui::Event::PointerButton {
                        pos: from,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    }]);
                    for k in 1..=12 {
                        let p = from + (to - from) * (k as f32 / 12.0);
                        self.input.push_back(vec![egui::Event::PointerMoved(p)]);
                    }
                    self.input.push_back(vec![egui::Event::PointerButton {
                        pos: to,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }]);
                }
                None => {
                    crate::logln!("smoke: nada chamado \"{at}\" na tela");
                    self.misses.push(at.to_owned());
                }
            },
            // A click on one of the app's notifications, handed over the way
            // Windows does it (through the activator's COM class):
            // activate:home, activate:first-server, or activate:<a view as
            // the notifications carry it>.
            ("activate", target) => {
                self.click(launch_for(core, target));
                return Duration::from_secs(2);
            }
            // A notification for the open conversation, straight to the
            // notification center: what Windows keeps of it is reported.
            ("toast", _) => {
                let launch = core.view.to_launch();
                let slot = self.toast_history.clone();
                std::thread::spawn(move || {
                    let found = crate::notify::round_trip("Teste do Babu Tela", &launch);
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(match found {
                        Ok(launches) => json!(launches),
                        Err(e) => json!({ "error": e }),
                    });
                });
                return Duration::from_secs(4);
            }
            // note:<label> keeps how the call is now, for a report of a
            // call whose people leave before the end.
            ("note", label) => {
                let people = core.voice_location().map(|(s, c)| s.in_voice(c.id).len());
                self.notes.insert(
                    label.to_owned(),
                    json!({
                        "voiceStats": core.voice.stats(),
                        "voiceCall": core.voice.report(),
                        "voicePeople": people,
                        "noise": core.voice.noise_report(),
                        "giphy": core.giphy.report(),
                        "slides": core.voice.slides().report(),
                        "ink": core.voice.ink_report(),
                        "direct": core.direct.report(),
                        "unwatched": core.voice.unwatched(),
                        "unreadHot": core.total_hot(),
                        "fullscreen": core.ctx.input(|i| i.viewport().fullscreen),
                        "geometry": core
                            .shell
                            .hwnd()
                            .map_or(serde_json::Value::Null, crate::winsys::geometry),
                        "draft": core.view.channel().and_then(|c| core.drafts.get(&c)),
                        "mentionList": core.view.channel().map(|cid| {
                            crate::ui::chat::mention_report(&core.ctx, cid)
                        }),
                    }),
                );
            }
            // Closes the floating menu or picker that is open.
            ("pop", "close") => core.pop = None,
            // animar: the stickers move even with the window in the back
            // (Windows does not let a test bring it to the front), so
            // their cost is measured moving.
            ("animar", _) => core.keep_moving = true,
            // hover:<where> puts the pointer there, and click:<where>
            // clicks there too, as a person would (rclick: with the right
            // button); <where> is <x>:<y> in points or a target drawn on
            // the last frame (see `target`).
            ("hover" | "click" | "rclick", at) => match point(at) {
                Some(pos) => {
                    self.input.push_back(vec![egui::Event::PointerMoved(pos)]);
                    let button = match verb {
                        "click" => Some(egui::PointerButton::Primary),
                        "rclick" => Some(egui::PointerButton::Secondary),
                        _ => None,
                    };
                    for pressed in button.map_or(&[][..], |_| &[true, false]) {
                        self.input.push_back(vec![egui::Event::PointerButton {
                            pos,
                            button: button.unwrap_or(egui::PointerButton::Primary),
                            pressed: *pressed,
                            modifiers: egui::Modifiers::NONE,
                        }]);
                    }
                }
                None => {
                    crate::logln!("smoke: nada chamado \"{at}\" na tela");
                    self.misses.push(at.to_owned());
                }
            },
            // keys:<text> types into the field that has the focus, and
            // enter presses Enter there.
            ("keys", text) => self
                .input
                .push_back(vec![egui::Event::Text(text.to_owned())]),
            ("enter", _) => {
                for pressed in [true, false] {
                    self.input.push_back(vec![egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }]);
                }
                return Duration::from_secs(1);
            }
            // tecla:<egui's name for it> presses a key (tecla:ArrowRight,
            // tecla:PageDown), as a presenter's remote does.
            ("tecla", name) => match egui::Key::from_name(name) {
                Some(key) => {
                    for pressed in [true, false] {
                        self.input.push_back(vec![egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        }]);
                    }
                }
                None => crate::logln!("smoke: tecla desconhecida: {name}"),
            },
            // roda:<points> turns the mouse wheel where the pointer is
            // (positive: away from the person, which zooms a slide in).
            ("roda", points) => {
                if let Ok(points) = points.parse::<f32>() {
                    self.input.push_back(vec![egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, points),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Wheel,
                        modifiers: egui::Modifiers::NONE,
                    }]);
                }
            }
            ("open", "requests") => {
                if let Some(sid) = core.view.server() {
                    core.open_modal(Modal::Requests { sid });
                }
            }
            ("open", "invite") => {
                core.invite();
                return Duration::from_secs(2);
            }
            // private:on|off makes the open server private, or open again.
            ("private", on) => {
                if let Some(sid) = core.view.server() {
                    core.set_private(sid, on == "on");
                }
                return Duration::from_secs(2);
            }
            // server-icon:<path> gives the open server this picture of the
            // PC, as "Trocar imagem" does.
            ("server-icon", path) => {
                if let Some(sid) = core.view.server() {
                    core.upload_server_icon(sid, PathBuf::from(path));
                }
                return Duration::from_secs(4);
            }
            // join-code:<invite> enters with it (or asks to, for a private
            // server), as the "+" of the rail does.
            ("join-code", code) => {
                let code = crate::net::escape(code.trim());
                core.call(
                    Req::post(format!("/invites/{code}/join")),
                    Then::Arrive,
                    ErrTo::Form(FormId::JoinServer),
                );
                // Short: the toast that says how it went lasts 4 s.
                return Duration::from_secs(1);
            }
            // accept-request:<user name> or deny-request:<user name>:
            // someone who asked to join the open server.
            ("accept-request" | "deny-request", who) => {
                let found = core.current_server().and_then(|s| {
                    s.requests
                        .iter()
                        .find(|r| r.user.username == who)
                        .map(|r| (s.id, r.user.id))
                });
                match found {
                    Some((sid, uid)) if verb == "accept-request" => core.accept_request(sid, uid),
                    Some((sid, uid)) => core.deny_request(sid, uid),
                    None => {
                        crate::logln!("smoke: nenhum pedido de \"{who}\"");
                        self.misses.push(step.to_owned());
                    }
                }
                return Duration::from_secs(2);
            }
            // cancel-request:<server name>: gives up my request to join it.
            ("cancel-request", name) => {
                let sid = core
                    .join_requests
                    .iter()
                    .find(|r| r.server.name == name)
                    .map(|r| r.server.id);
                match sid {
                    Some(sid) => core.cancel_request(sid),
                    None => self.misses.push(step.to_owned()),
                }
                return Duration::from_secs(2);
            }
            // go:server:<name> opens that server.
            ("go", rest) if rest.starts_with("server:") => {
                let name = &rest["server:".len()..];
                if let Some(id) = core.servers.iter().find(|s| s.name == name).map(|s| s.id) {
                    core.go_to(View::Server {
                        server: id,
                        channel: None,
                    });
                } else {
                    self.misses.push(step.to_owned());
                }
            }
            ("wait", ms) => return Duration::from_millis(ms.parse().unwrap_or(1000)),
            _ => crate::logln!("smoke: passo desconhecido: {step}"),
        }
        Duration::ZERO
    }

    /// Clicks a notification that carries `launch`, from a thread of its
    /// own as Windows would, and keeps how it went for the report.
    fn click(&self, launch: String) {
        let slot = self.clicked.clone();
        std::thread::spawn(move || {
            let result = crate::notify::click_like_windows(&launch);
            *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(result.map_or_else(|e| e, |()| "ok".to_owned()));
        });
    }

    fn finish(&self, core: &Core, shot: Option<PathBuf>, problem: &str) -> ! {
        let messages = core
            .view
            .channel()
            .and_then(|c| core.msgs.get(&c))
            .map_or(0, |m| m.list.len());
        // The newest message of the open conversation, as it arrived.
        let newest = core
            .view
            .channel()
            .and_then(|c| core.msgs.get(&c))
            .and_then(|m| m.list.last())
            .map(|m| {
                json!({
                    "contentLength": m.content.chars().count(),
                    "attachments": m.attachments.iter().map(|a| json!({
                        "url": a.url, "name": a.name, "size": a.size, "kind": a.kind,
                        "direct": a.direct,
                    })).collect::<Vec<_>>(),
                    "sticker": m.sticker,
                    "pending": m.pending,
                })
            });
        let report = json!({
            "phase": format!("{:?}", core.phase),
            "user": core.me.as_ref().map(|u| u.username.clone()),
            "userId": core.me.as_ref().map(|u| u.id),
            "servers": core.servers.len(),
            "dms": core.dms.len(),
            "friends": core.friends.len(),
            "view": core.view,
            "messages": messages,
            "newest": newest,
            "sending": core.sending.iter().map(|s| json!({
                "files": s.files, "sent": s.progress.sent(), "total": s.progress.total(),
            })).collect::<Vec<_>>(),
            "pendingFiles": core.files.iter().map(|f| json!({
                "name": f.name, "uploaded": f.uploaded.is_some(),
            })).collect::<Vec<_>>(),
            "voice": core.voice.channel_id(),
            "voiceStats": core.voice.stats(),
            "voiceCall": core.voice.report(),
            "notes": self.notes,
            "unreadHot": core.total_hot(),
            "focused": core.focused,
            "window": core.shell.hwnd().map(|h| json!({
                "foreground": crate::winsys::is_foreground(h),
                "visible": crate::winsys::is_visible(h),
            })),
            "sharing": core.voice.video().map(|v| v.wire()),
            "sharingSource": core.voice.source().map(|s| format!("{s:?}")),
            "sharingAudio": core.voice.sharing_audio(),
            "voiceDevices": core.voice.opened_devices(),
            "media": core.media.report(),
            "stickers": core.stickers,
            "giphy": core.giphy.report(),
            "pop": core.pop.map(|p| format!("{:?}", p.kind)),
            "clicked": self.clicked.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            "toastHistory": self.toast_history.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            "videoFrom": core
                .voice_location()
                .map(|(s, c)| s.in_voice(c.id).iter().filter(|v| core.voice.frame(v.user_id).is_some()).map(|v| v.user_id).collect::<Vec<_>>()),
            "voicePeople": core.voice_location().map(|(s, c)| s.in_voice(c.id).len()),
            "toasts": core.toasts.iter().map(|t| t.text.clone()).collect::<Vec<_>>(),
            "errors": core.errors.values().cloned().collect::<Vec<_>>(),
            "modal": core.modal.is_some(),
            "steps": self.done,
            "marks": self.marks,
            "medidas": crate::probe::summary(),
            "memoria": crate::probe::on().then(crate::winsys::memory_parts),
            "screenshot": shot.map(|p| p.display().to_string()),
            "problem": problem,
            "elapsedMs": self.started.elapsed().as_millis() as u64,
        });
        // Apart from the rest: one json! that big hits the macro's limit.
        let mut report = report;
        report["account"] = json!(core.account);
        report["googleWaiting"] = json!(core.google.is_some());
        report["authCard"] = json!((core.phase == Phase::Auth).then_some("google"));
        report["maximized"] = json!(core.shell.hwnd().map(crate::winsys::is_maximized));
        report["fullscreen"] = json!(core.ctx.input(|i| i.viewport().fullscreen));
        report["unwatched"] = json!(core.voice.unwatched());
        report["draft"] = json!(core.view.channel().and_then(|c| core.drafts.get(&c)));
        report["mentionList"] = core.view.channel().map_or(serde_json::Value::Null, |cid| {
            crate::ui::chat::mention_report(&core.ctx, cid)
        });
        report["zoomed"] = json!(
            core.view
                .channel()
                .and_then(|cid| crate::ui::stage::zoomed(&core.ctx, cid))
        );
        report["geometry"] = core
            .shell
            .hwnd()
            .map_or(serde_json::Value::Null, crate::winsys::geometry);
        report["noise"] = core.voice.noise_report();
        report["slides"] = core.voice.slides().report();
        report["direct"] = core.direct.report();
        report["directSend"] = json!(core.direct_send);
        // The emoji list, and what the picker shows for the search typed.
        let mut emoji = core.emoji.report();
        emoji["recent"] = json!(core.prefs.emoji_recent);
        emoji["tone"] = json!(core.prefs.emoji_tone);
        emoji["pairsOpen"] = json!(core.emoji_pairs);
        if let Some(c) = core.emoji.catalog() {
            let query = core.emoji_query.trim();
            if !query.is_empty() {
                let found: Vec<&str> = c
                    .search(query)
                    .into_iter()
                    .take(12)
                    .map(|i| c.entries[i].name.as_str())
                    .collect();
                emoji["searched"] = json!({ "query": query, "found": found });
            }
        }
        report["emoji"] = emoji;
        report["modalKind"] = json!(core.modal.as_ref().map(modal_name));
        // Which version this is, and where its update stands (update.rs).
        report["version"] = json!(babutela_buildinfo::VERSION);
        report["updateGate"] = json!(match &core.gate {
            Gate::Checking(_) => "checking",
            Gate::Updating { .. } => "updating",
            Gate::Open => "open",
        });
        report["updateReady"] = json!(core.update_ready);
        report["misses"] = json!(self.misses);
        // My requests to join private servers, and the open server as it
        // is now (who asked to join it, its picture, its channels).
        report["joinRequests"] = json!(
            core.join_requests
                .iter()
                .map(|r| r.server.name.clone())
                .collect::<Vec<_>>()
        );
        report["server"] = json!(core.current_server().map(|s| json!({
            "name": s.name,
            "private": s.private,
            "icon": s.icon,
            "requests": s.requests.iter().map(|r| r.user.username.clone()).collect::<Vec<_>>(),
            "channels": s.channels.iter().map(|c| json!({
                "name": c.label(), "kind": c.kind, "category": c.category,
            })).collect::<Vec<_>>(),
        })));
        report["serverNames"] = json!(
            core.servers
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        );
        let _ = std::fs::write(
            &self.report,
            serde_json::to_vec_pretty(&report).unwrap_or_default(),
        );
        crate::tray::remove();
        std::process::exit(0);
    }
}

/// A sticker id for a step: `first` is the first one the picker shows (or
/// would show), `#n` the n-th (from 0), anything else is taken as it is.
fn sticker_for(core: &Core, id: &str) -> Option<String> {
    let at: usize = match id {
        "first" => 0,
        n if n.starts_with('#') => n[1..].parse().ok()?,
        other => return Some(other.to_owned()),
    };
    let query = core.sticker_query.trim();
    let shown = if !query.is_empty() {
        core.giphy.feed(query).ids
    } else {
        match core.sticker_tab {
            StickerTab::Favorites => core.stickers.clone(),
            StickerTab::Trending => core.giphy.feed("").ids,
        }
    };
    shown.into_iter().nth(at)
}

/// The newest file sent directly (direct/) in the open conversation by
/// someone else, and its message.
fn newest_direct(core: &Core) -> Option<(crate::model::Message, crate::model::Attachment)> {
    let me = core.me_id();
    let list = &core.msgs.get(&core.view.channel()?)?.list;
    list.iter()
        .rev()
        .filter(|m| m.author_id() != me)
        .find_map(|m| {
            m.attachments
                .iter()
                .find(|a| a.kind == crate::model::Kind::Direct)
                .map(|a| (m.clone(), a.clone()))
        })
}

/// The n-th attachment of the newest message in the open channel.
fn newest_attachment(core: &Core, n: &str) -> Option<crate::model::Attachment> {
    core.view
        .channel()
        .and_then(|c| core.msgs.get(&c))
        .and_then(|m| m.list.last())
        .and_then(|m| m.attachments.get(n.parse::<usize>().unwrap_or(0)))
        .cloned()
}

/// Which dialog is open, for the report (`new-channel-quick` is the one of
/// a category's "+").
fn modal_name(m: &Modal) -> &'static str {
    match m {
        Modal::AddServer { .. } => "add-server",
        Modal::NewChannel { quick: true, .. } => "new-channel-quick",
        Modal::NewChannel { .. } => "new-channel",
        Modal::EditChannel { .. } => "edit-channel",
        Modal::Invite { .. } => "invite",
        Modal::Ttl { .. } => "ttl",
        Modal::ServerSettings { .. } => "server-settings",
        Modal::Profile { .. } => "profile",
        Modal::Settings { .. } => "settings",
        Modal::Report { .. } => "report",
        Modal::Reports { .. } => "reports",
        Modal::Requests { .. } => "requests",
        Modal::Confirm { .. } => "confirm",
        Modal::Player { .. } => "player",
        Modal::Share { .. } => "share",
        Modal::Licenses => "licenses",
        Modal::ApproveDevice { .. } => "approve-device",
        Modal::NewDevice => "new-device",
        Modal::Security { .. } => "security",
        Modal::Music { .. } => "music",
    }
}

/// What a test click on a notification carries: `home`, `first-server`
/// (its first text channel), or anything else as it is.
fn launch_for(core: &Core, target: &str) -> String {
    match target {
        "home" => View::HOME.to_launch(),
        "first-server" => core
            .servers
            .first()
            .map_or(View::None, |s| View::Server {
                server: s.id,
                channel: s.channels.iter().find(|c| !c.is_voice()).map(|c| c.id),
            })
            .to_launch(),
        other => other.to_owned(),
    }
}

//! The client: everything the app knows and does, apart from drawing it.
//!
//! This is the old app.js minus its HTML: the state (`S`), the reactions to
//! the server's events (`EV`), the actions behind every button (`A`, `F`, `C`)
//! and the rules for unread counts, notifications and navigation. The
//! interface in ui/ reads this and calls its methods.
//!
//! Everything here runs on the thread that owns the window. Calls to the
//! server run on the tokio runtime and come back as a `Msg`, which `pump`
//! applies; eframe calls it on every frame, and also while the window is
//! hidden in the tray, so messages and notifications keep arriving.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gateway::{self, GwEvent, op};
use crate::giphy::{self, Giphy};
use crate::images::Images;
use crate::model::*;
use crate::net::{self, Api, ApiResult, Req};
use crate::prefs::{PersonAudio, Prefs};
use crate::shell::Shell;
use crate::sounds::{Cue, Sounds};
use crate::voice::Voice;

mod sealing;

/// How long a toast stays up.
const TOAST_FOR: Duration = Duration::from_secs(4);

/// Someone counts as typing for this long after their last TYPING.
const TYPING_FOR: Duration = Duration::from_secs(6);

/// At most one TYPING goes out this often while writing.
const TYPING_EVERY: Duration = Duration::from_secs(3);

/// Quitting waits at most this long for the call to hang up (it takes a
/// tenth of a second, its connections at most 2 s and its relay 1 s more).
const QUIT_WAIT: Duration = Duration::from_secs(4);

/// How long after the start the emoji list is fetched, so the picker opens
/// with it ready (emoji.rs). Late enough to leave the start alone.
const EMOJI_AFTER: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------- what is on screen

/// What the main area shows: a direct conversation (or the friends page
/// when there is none), or a channel of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum View {
    /// Before the first READY.
    None,
    Home {
        #[serde(rename = "channelId", default)]
        channel: Option<i64>,
    },
    Server {
        #[serde(rename = "serverId")]
        server: i64,
        #[serde(rename = "channelId", default)]
        channel: Option<i64>,
    },
}

impl View {
    pub const HOME: View = View::Home { channel: None };

    pub fn channel(self) -> Option<i64> {
        match self {
            View::None => None,
            View::Home { channel } | View::Server { channel, .. } => channel,
        }
    }

    pub fn server(self) -> Option<i64> {
        match self {
            View::Server { server, .. } => Some(server),
            _ => None,
        }
    }

    pub fn is_home(self) -> bool {
        matches!(self, View::Home { .. })
    }

    /// What a notification carries to say where clicking it goes.
    pub fn to_launch(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }

    /// The other way round: None for anything else (a notification of
    /// another version, or none at all).
    pub fn from_launch(launch: &str) -> Option<View> {
        serde_json::from_str(launch)
            .ok()
            .filter(|v| *v != View::None)
    }
}

/// Which screen the window shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    /// Waiting for the server to answer at all; `retry` is when the next
    /// attempt happens (`None` while the first is under way).
    Waiting { retry: Option<Instant> },
    /// Signed out: the card with "Continuar com o Google".
    Auth,
    /// Signed in, waiting for the first READY.
    Connecting { retry: Option<Instant> },
    /// The app itself.
    Ready,
}

#[derive(Default)]
pub struct MsgList {
    pub list: Vec<Message>,
    pub more: bool,
    pub loading: bool,
}

/// Unread messages in one conversation: all of them, and the ones that ask
/// for attention (direct messages and mentions), which drive the badges.
#[derive(Clone, Copy, Default)]
pub struct Unread {
    pub n: u32,
    pub hot: u32,
}

pub struct Toast {
    pub text: String,
    pub until: Instant,
}

/// A file waiting to go out with the next message.
#[derive(Clone)]
pub struct PendingFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// Already on the server, from a send that failed after it went up: a
    /// second try does not send it again. For a file sent directly, what
    /// its preparing made (direct/).
    pub uploaded: Option<Attachment>,
    /// It goes directly ("Enviar direto"), not to the server.
    pub direct: bool,
    /// The key it went up encrypted with, along with `uploaded`.
    pub key: Option<crate::e2e::FileKey>,
}

/// A message on its way, its files still going up (or, sent directly,
/// being prepared).
pub struct Sending {
    pub id: u64,
    pub cid: i64,
    pub files: usize,
    pub progress: Arc<crate::net::Progress>,
    pub direct: bool,
}

/// What the server needs to make a message, and to tell a second try at it
/// from a new one (`nonce`).
struct Outgoing {
    /// The number of this send (`Sending::id`, `temp_id`).
    send: u64,
    cid: i64,
    content: String,
    /// A GIPHY sticker's id, which goes alone (no text, no files).
    sticker: Option<String>,
    /// The message answered, as quoted inside the envelope.
    quote: Option<crate::e2e::msg::Quote>,
    nonce: String,
    /// What actually goes: everything above, sealed (sealing.rs). A second
    /// try sends the same envelope, so the server knows it by its nonce.
    env: Option<Value>,
    /// What the server keeps of its files (which uploads, which direct
    /// files).
    files: Vec<Value>,
}

/// A message that went out through the gateway, waiting since `at` for the
/// server's copy.
struct Unconfirmed {
    out: Outgoing,
    at: Instant,
}

/// How long a message sent through the gateway may go without the server's
/// copy before it goes again over HTTP (normally it takes 130 ms).
const GW_PATIENCE: Duration = Duration::from_secs(3);

/// Longer messages go over HTTP: the gateway closes on frames over 256 KB.
const GW_MESSAGE_MAX: usize = 64 * 1024;

pub struct Editing {
    pub id: i64,
    pub text: String,
    pub focus: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendTab {
    Online,
    All,
    Pending,
    Blocked,
    Add,
}

/// Forms whose errors show next to their button instead of as a toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FormId {
    /// The sign-in card ("Continuar com o Google").
    Login,
    CreateServer,
    JoinServer,
    CreateChannel,
    EditChannel,
    RenameServer,
    CreateRole,
    SaveRole,
    Profile,
    Report,
    AddFriend,
}

// ---------------------------------------------------------------- dialogs

pub struct RoleEditor {
    /// The role whose fields are loaded below.
    pub id: i64,
    pub name: String,
    pub color: [u8; 3],
    pub perms: i64,
}

pub enum Modal {
    AddServer {
        name: String,
        code: String,
    },
    NewChannel {
        sid: i64,
        kind: ChannelKind,
        name: String,
        category: String,
        /// From the "+" of a category: only the name is asked, the channel
        /// goes in that category, with its type (asked only when the
        /// category mixes both). The server menu's form asks everything.
        quick: bool,
    },
    EditChannel {
        cid: i64,
        name: String,
        topic: String,
        category: String,
    },
    Invite {
        code: String,
    },
    /// How long the messages of conversation `cid` last.
    Ttl {
        cid: i64,
    },
    ServerSettings {
        sid: i64,
        name: String,
        new_role: String,
        role: Option<RoleEditor>,
    },
    Profile {
        uid: i64,
    },
    Settings {
        display_name: String,
    },
    Report {
        mid: i64,
        reason: usize,
        detail: String,
    },
    Reports {
        sid: i64,
    },
    /// Who asked to join this private server.
    Requests {
        sid: i64,
    },
    Confirm {
        title: String,
        text: String,
        label: String,
        action: Action,
    },
    /// A video attachment, enlarged.
    Player {
        attachment: Attachment,
    },
    /// The works of others the app carries or uses, and their licences.
    Licenses,
    /// Another PC of my account asks for the keys (e2e/provision.rs): the
    /// code it shows is typed here, and checked, before anything goes.
    ApproveDevice {
        /// Its connection, which the answer goes to.
        conn: u64,
        pk: String,
        code: String,
        error: Option<String>,
    },
    /// This PC has no keys yet: the code to type on another PC of the
    /// account, or a new identity.
    NewDevice,
    /// The security number with someone, to compare with them.
    Security {
        uid: i64,
    },
    /// What to show in the call: a screen or a window, with pictures, or a
    /// presentation.
    Share {
        tab: ShareTab,
        items: Vec<ShareItem>,
        /// The list arriving from the thread that takes the pictures; gone
        /// once it has finished.
        incoming: Option<std::sync::mpsc::Receiver<crate::voice::sources::Choice>>,
    },
    /// Music in the call: the queue, and the field for a new YouTube or
    /// SoundCloud link.
    Music {
        link: String,
    },
}

/// The share picker's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareTab {
    Screens,
    Windows,
    /// A .pptx or a PDF, presented (slides/).
    Slides,
}

/// A screen or a window in the share picker.
pub struct ShareItem {
    pub source: crate::voice::sources::Source,
    pub title: String,
    pub detail: String,
    pub thumb: Option<egui::TextureHandle>,
}

/// What a confirmation dialog does once confirmed.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    DeleteMessage(i64),
    DeleteReported {
        sid: i64,
        mid: i64,
    },
    BanAuthor {
        sid: i64,
        uid: i64,
    },
    Block(i64),
    DeleteChannel(i64),
    DeleteServer(i64),
    LeaveServer(i64),
    Kick {
        sid: i64,
        uid: i64,
    },
    Ban {
        sid: i64,
        uid: i64,
    },
    DeleteAccount,
    /// A file of mine sent directly stops being shared (its id, the 32 hex
    /// characters read as a number).
    StopSharing(u128),
    /// A download of a file sent directly goes, with what it had written.
    CancelDownload(u128),
    /// The app opens again (a setting read at the start: Tor).
    Restart,
}

/// The small floating menus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PopKind {
    ServerMenu,
    /// Reactions for a message.
    React(i64),
    /// Emoji for the composer.
    Emoji,
    /// GIPHY stickers for the composer.
    Sticker,
    /// Someone in a call (a right click on them): how loud they play here.
    Person(i64),
    /// Annotating (ui/ink.rs): the colour and width of the pen; clearing
    /// and who may annotate, on what I share.
    InkColor,
    InkClear(crate::ink::Surface),
    InkWho(crate::ink::Surface),
}

/// What the sticker picker shows while nothing is searched for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StickerTab {
    Trending,
    Favorites,
}

#[derive(Clone, Copy, Debug)]
pub struct Pop {
    pub kind: PopKind,
    pub anchor: egui::Rect,
}

// ---------------------------------------------------------------- messages from elsewhere

/// What comes back to the interface thread.
pub enum Msg {
    Gw(u64, GwEvent),
    Done {
        then: Then,
        err: ErrTo,
        res: ApiResult,
    },
    SendFailed {
        /// The `Sending` it was.
        id: u64,
        cid: i64,
        content: String,
        files: Vec<PendingFile>,
        error: String,
    },
    Picked(Pick, Vec<PathBuf>),
    Alive,
    Offline(Duration),
    Tray(TrayAction),
    Navigate(View),
    Update(UpdateNews),
    Toast(String),
    /// Quitting: the call has hung up (or took too long), so the window
    /// can close.
    ReadyToClose,
    /// How a sign-in with Google ended: the server's answer (google.rs).
    Google(ApiResult),
    /// The microphones and outputs there are (for the settings).
    AudioDevices(crate::devices::Devices),
    /// News of the files sent directly (direct/).
    Direct(crate::direct::News),
    /// Where to save a file sent directly, as the person chose it.
    DirectSave {
        attachment: Attachment,
        msg: i64,
        cid: i64,
        from: i64,
        path: PathBuf,
    },
    /// Where this PC stands with the account's keys (e2e/).
    E2e(crate::e2e::State),
    /// Keys that messages waited for arrived: they open now.
    Keys,
    /// A message sealed, ready to go (sealing.rs).
    Sealed {
        send: u64,
        env: Value,
        /// Readers without keys yet (an old app).
        missing: Vec<i64>,
    },
    SealFailed {
        send: u64,
        error: String,
    },
    /// A new day (UTC) began: the keys of the day move on (sealing.rs
    /// `new_day`). The number is the round it was set for: an older round,
    /// superseded since, does nothing.
    NewDay(u64),
    /// The time of a message of a conversation whose messages disappear
    /// came (sealing.rs `schedule_expiry`).
    Expire {
        cid: i64,
        id: i64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Attach,
    Avatar,
    /// The picture of this server.
    ServerIcon(i64),
    /// A .pptx or a PDF to present in the call.
    Slides,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Mute,
    Deafen,
    Quit,
}

pub enum UpdateNews {
    /// A newer version is on its way down (the startup screen shows it).
    Found {
        version: String,
        progress: Arc<crate::net::Progress>,
    },
    /// An installer for this version is downloaded and checked; `tries`
    /// starts have run it, and `asked` is the settings' button asking.
    Ready {
        version: String,
        tries: u32,
        asked: bool,
    },
    /// The answer to a check that has nothing to install: for the person
    /// (the settings' button), or empty.
    Checked(String),
    /// The downloaded installer is no longer on disk.
    Gone,
}

/// What the start waits for before the sign-in or the chat, the way
/// Discord's splash screen waits for its updater (update.rs).
pub enum Gate {
    /// Asking the server about a newer version, until then at most.
    Checking(Instant),
    /// Downloading it, to install it and open again on it.
    Updating {
        version: String,
        progress: Arc<crate::net::Progress>,
    },
    /// Going on: nothing newer, no answer in time, a copy that does not
    /// update itself, or the person chose not to wait.
    Open,
}

/// How long the start waits for the server to say whether there is a newer
/// version. The window is open meanwhile, on the screen that says it is
/// connecting.
const GATE_WAIT: Duration = Duration::from_secs(5);

/// A startup download that stops moving for this long is left to finish in
/// the background: the app opens (or, in the tray, connects) meanwhile.
const UPDATE_STALL: Duration = Duration::from_secs(30);

/// Who asked to look for an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// The start, which waits for the answer (the gate).
    Startup,
    /// A start that does not wait: a copy that never installs on its own
    /// still says there is a newer version, once the first screen is up.
    Later,
    /// "Procurar atualizações", which says what it found.
    Button,
}

/// What to do with the answer of a call.
pub enum Then {
    Nothing,
    /// A message went out: its `Sending` is over.
    Sent(u64),
    Toast(String),
    CloseModal,
    CloseModalToast(String),
    Arrive,
    RoleCreated,
    OpenDm,
    Invite,
    /// An invite made with its signed key: the link to share (code and
    /// secret).
    InviteLink(String),
    Messages {
        cid: i64,
        older: bool,
    },
    AccountDeleted,
    FriendRequested {
        close_modal: bool,
    },
    ReopenReports(i64),
    RoleDeleted,
    Avatar,
    /// A picture went up for this server: it becomes its picture.
    ServerIcon(i64),
    /// My favourite stickers, as the server keeps them now; the list
    /// before the change comes back should it fail.
    Stickers(Vec<String>),
}

/// Where the error of a call goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrTo {
    Toast,
    Form(FormId),
}

// ---------------------------------------------------------------- the client

pub struct Core {
    pub ctx: egui::Context,
    pub rt: tokio::runtime::Handle,
    pub api: Arc<Api>,
    pub gw: gateway::Sender,
    gw_task: Option<tokio::task::JoinHandle<()>>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    pub shell: Arc<Shell>,
    pub images: Arc<Images>,
    /// GIPHY's stickers: the searches and the animations.
    pub giphy: Arc<Giphy>,
    pub sounds: Arc<Sounds>,
    pub voice: Voice,
    /// Music in the call: YouTube and SoundCloud links tocados em grupo.
    pub music: Arc<crate::music::Music>,
    /// Audio and video attachments being played.
    pub media: crate::media::Media,
    pub prefs: Prefs,
    /// The end-to-end encryption: the account's keys and everything sealed
    /// with them (e2e/).
    pub e2e: Arc<crate::e2e::E2e>,
    /// The signalling of calls and direct transfers, sealed (e2e/signals.rs).
    signals: Arc<crate::e2e::signals::Signals>,
    /// Messages that arrived locked: announced once they open.
    fresh_locked: HashSet<i64>,
    /// Messages being sealed, by their send number.
    sealing: HashMap<u64, Outgoing>,
    /// People told about once that their app cannot read what I send.
    warned_old: HashSet<i64>,
    /// This PC asking another of the account for the keys (its code).
    pub provision: Option<crate::e2e::provision::Request>,
    /// The secret of the invite link being used to join (e2e/members.rs),
    /// which never goes to the server: my entry is signed with it.
    pub joining: Option<[u8; 32]>,
    /// Private servers I asked to join with a signed invite: the owner it
    /// names is pinned once I am in (`invite_owner`).
    asked_with: HashMap<i64, ([u8; 32], crate::e2e::members::InviteCert)>,
    /// The round of the keys of the day (sealing.rs `new_day`): only the
    /// latest one's wait for the next day counts. And the day it ran for.
    day_round: u64,
    day_done: i64,
    /// Messages whose time to go is already waited for (`schedule_expiry`).
    expiring: HashSet<i64>,

    pub phase: Phase,
    /// The sign-in with Google under way (the browser is open).
    pub google: Option<tokio::task::JoinHandle<()>>,
    /// The Google account I sign in with, from READY.
    pub account: crate::google::Account,
    /// The microphones and outputs there are, as last listed (when the
    /// settings opened); None until then.
    pub audio_devices: Option<crate::devices::Devices>,
    wait_tries: u32,
    pub me: Option<User>,
    pub servers: Vec<Server>,
    pub dms: Vec<Dm>,
    pub friends: Vec<Friend>,
    pub users: HashMap<i64, User>,
    pub blocks: HashSet<i64>,
    pub revealed: HashSet<i64>,
    /// Spoilers clicked open: (message id, which spoiler in it).
    pub spoilers: HashSet<(i64, usize)>,
    pub msgs: HashMap<i64, MsgList>,
    pub unread: HashMap<i64, Unread>,
    pub typing: HashMap<i64, HashMap<i64, Instant>>,
    pub view: View,
    pub server_version: String,

    pub drafts: HashMap<i64, String>,
    pub reply: Option<Message>,
    pub editing: Option<Editing>,
    pub files: Vec<PendingFile>,
    /// The composer's "Enviar direto": the files waiting go straight to
    /// whoever downloads them, not to the server.
    pub direct_send: bool,
    /// The files sent directly: mine shared from here, and my downloads.
    pub direct: Arc<crate::direct::Direct>,
    /// Messages whose files are still going up.
    pub sending: Vec<Sending>,
    /// Messages sent through the gateway, not back from the server yet.
    outbox: Vec<Unconfirmed>,
    next_send: u64,
    /// Long messages opened in full ("Mostrar mensagem inteira").
    pub expanded: HashSet<i64>,
    /// My favourite stickers (GIPHY ids), newest first.
    pub stickers: Vec<String>,
    /// The private servers I asked to join and wait on.
    pub join_requests: Vec<MyRequest>,
    /// The sticker picker: what is searched for, since when (a search
    /// waits for the typing to stop), the tab, and its box to focus.
    pub sticker_query: String,
    pub sticker_typed: Option<Instant>,
    pub sticker_tab: StickerTab,
    pub sticker_focus: bool,
    /// Every emoji of the official list this PC draws (emoji.rs).
    pub emoji: Arc<crate::emoji::Emoji>,
    /// The emoji picker: what is searched for, its box to focus, a group
    /// to scroll to and whether the skin tones are on show.
    pub emoji_query: String,
    pub emoji_focus: bool,
    pub emoji_jump: Option<usize>,
    pub emoji_tones_open: bool,
    /// The emoji of two people whose tones, one for each, are on show in
    /// the picker (a right click on it).
    pub emoji_pairs: Option<String>,
    /// For the tests' measurements: stickers move with the window in the
    /// back too (see `stickers_move`).
    pub keep_moving: bool,
    pub stick_bottom: bool,
    pub focus_composer: bool,
    pub friend_tab: FriendTab,
    pub add_friend: String,
    pub show_members: bool,
    pub modal: Option<Modal>,
    pub modal_focus: bool,
    /// For the tests' pictures: scrolls the open dialog down this far on
    /// the next frame.
    pub modal_scroll: Option<f32>,
    pub pop: Option<Pop>,
    pub toasts: Vec<Toast>,
    pub reconnecting: bool,
    pub update_ready: Option<String>,
    /// The update check the start waits for.
    pub gate: Gate,
    /// The server answered while the gate was closed: carry on once it
    /// opens.
    answered: bool,
    /// The bytes of the startup download, when they last moved.
    update_moved: Option<(u64, Instant)>,
    /// Started in the tray (main.rs): an update the start installs leaves
    /// it there.
    pub started_hidden: bool,
    pending_go: Option<i64>,
    /// Where a notification clicked before the app was connected asked to
    /// go (Windows starts the app for such a click): opened at READY.
    wanted: Option<View>,
    pub busy: HashSet<FormId>,
    pub errors: HashMap<FormId, String>,
    last_typing: Option<Instant>,
    pub focused: bool,
    badge: u32,
    /// The window should come to the front on the next frame.
    pub raise: bool,
    pub quitting: bool,
    /// Quitting, and nothing is left to wait for: the window closes.
    ready_to_close: bool,
    /// For `cargo xtask medir`: when the messages put up this frame were
    /// sent, and under which series, so the end of the frame can tell how
    /// long they took to show.
    arrived_stamps: Vec<(&'static str, f64)>,
}

/// The id of whoever wrote a message, 0 for an account that is gone.
fn author(m: &Message) -> i64 {
    m.author_id()
}

/// The favourite stickers in a STICKERS event, or in the answer to a change.
fn sticker_list(v: &Value) -> Option<Vec<String>> {
    serde_json::from_value(v.get("stickers")?.clone()).ok()
}

/// How big a server's picture goes up: twice the rail's 46 px and some.
const SERVER_ICON_SIDE: u32 = 256;

/// The picture at `path` cut to a square from the middle and made `side`
/// pixels wide, as a PNG in the temporary folder (a GIF keeps its first
/// frame). Its path, for the upload.
fn square_png(path: &std::path::Path, side: u32) -> Result<std::path::PathBuf, String> {
    let img = image::open(path).map_err(|_| crate::t!("core.image_open_failed").to_owned())?;
    let short = img.width().min(img.height());
    if short == 0 {
        return Err(crate::t!("core.image_empty").into());
    }
    let square = img.crop_imm(
        (img.width() - short) / 2,
        (img.height() - short) / 2,
        short,
        short,
    );
    let out = if short > side {
        square.resize_exact(side, side, image::imageops::FilterType::Lanczos3)
    } else {
        square
    };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    let file = std::env::temp_dir().join(format!("servidor-{stamp}.png"));
    out.to_rgba8()
        .save(&file)
        .map_err(|e| crate::tf!("core.image_prepare_failed", e = e))?;
    Ok(file)
}

/// Asks where to save `picture` (an annotated screen or slide) and saves it
/// there as PNG: the place, or None when the person gave up.
fn save_png(picture: &egui::ColorImage) -> Result<Option<PathBuf>, String> {
    let [w, h] = picture.size;
    let image = image::RgbaImage::from_raw(w as u32, h as u32, picture.as_raw().to_vec())
        .ok_or(crate::t!("core.image_came_empty"))?;
    let name = crate::tf!(
        "core.annotation_filename",
        n = chrono::Local::now().format("%Y-%m-%d %H.%M.%S")
    );
    let Some(path) = rfd::FileDialog::new()
        .set_title(crate::t!("core.save_annotation_title"))
        .set_file_name(&name)
        .add_filter(crate::t!("core.png_image"), &["png"])
        .save_file()
    else {
        return Ok(None);
    };
    let path = if path.extension().is_none() {
        path.with_extension("png")
    } else {
        path
    };
    image
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(Some(path))
}

/// Puts a message the server sent into its conversation. The copies of
/// mine still on their way (shown the moment Enter was pressed) stay at the
/// end: the server numbers messages in the order it sends them out, so
/// anything it sends before my message comes before it. The copy of this
/// very message gives its place up to it.
fn place(list: &mut Vec<Message>, m: Message) {
    let tail = list.len() - list.iter().rev().take_while(|x| x.pending).count();
    if let Some(nonce) = m.nonce.as_deref()
        && let Some(i) = list[tail..]
            .iter()
            .position(|x| x.nonce.as_deref() == Some(nonce))
    {
        list.remove(tail + i);
    }
    // Both the answer to the send and the gateway bring my own messages,
    // and a reload may already have brought it.
    let known = list[..tail]
        .iter()
        .rev()
        .take_while(|x| x.id >= m.id)
        .any(|x| x.id == m.id);
    if known {
        crate::probe::count("msg.repetida", 1);
    } else {
        list.insert(tail, m);
    }
}

/// The id a message of mine has while it is on its way: below zero, so it
/// is never one the server gave.
fn temp_id(send: u64) -> i64 {
    -1 - i64::try_from(send).unwrap_or(i64::MAX - 1)
}

/// The mark that ties a message I send to the copy the server sends back.
/// It only has to differ between my messages on their way, across the
/// devices signed in to the same account.
fn new_nonce(send: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u64(send);
    format!("{:016x}", h.finish())
}

impl Core {
    pub fn new(
        ctx: egui::Context,
        rt: tokio::runtime::Handle,
        shell: Arc<Shell>,
        tx: mpsc::Sender<Msg>,
        rx: mpsc::Receiver<Msg>,
    ) -> Core {
        let api = Api::new(&shell.server_url());
        let e2e = crate::e2e::E2e::new(api.clone(), shell.dir.clone());
        let signals = crate::e2e::signals::Signals::new(e2e.clone(), rt.clone());
        let gw = gateway::Sender::default();
        let images = Images::new(ctx.clone(), rt.clone(), api.clone(), shell.path("cache"));
        let giphy = Giphy::new(ctx.clone(), rt.clone());
        let emoji = crate::emoji::Emoji::new(
            ctx.clone(),
            rt.clone(),
            api.http().clone(),
            shell.path("cache").join("emoji"),
            shell.server_url(),
        );
        // Ready by the time anyone opens a picker; a test run (and its
        // measurements) only fetches it when a picker opens.
        if !crate::smoke::active() {
            let emoji = emoji.clone();
            rt.spawn(async move {
                tokio::time::sleep(EMOJI_AFTER).await;
                emoji.want();
            });
        }
        let prefs = Prefs::load(&shell.path("prefs.json"));
        // Before any text is drawn: the saved language, or the Windows
        // account's the first time.
        crate::lang::set(prefs.lang.unwrap_or_else(crate::lang::os_default));
        // Before any connection is made.
        crate::voice::rtc::set_hide_ip(prefs.hide_ip);
        // Before anything opens a device.
        crate::devices::set_input(prefs.input_device.as_ref().map(|d| d.id.clone()));
        crate::devices::set_output(prefs.output_device.as_ref().map(|d| d.id.clone()));
        let sounds = Sounds::new(prefs.sounds_off.iter().cloned().collect());
        let music = crate::music::Music::new(
            rt.clone(),
            {
                let tx = tx.clone();
                let ctx = ctx.clone();
                Arc::new(move |text: String| {
                    let _ = tx.send(Msg::Toast(text));
                    ctx.request_repaint();
                })
            },
            ctx.clone(),
        );
        let mut voice = Voice::new(
            rt.clone(),
            api.clone(),
            gw.clone(),
            signals.clone(),
            sounds.clone(),
            music.clone(),
            ctx.clone(),
            {
                let tx = tx.clone();
                let ctx = ctx.clone();
                Arc::new(move |text: String| {
                    let _ = tx.send(Msg::Toast(text));
                    ctx.request_repaint();
                })
            },
        );
        voice.set_quality(crate::voice::Quality::by_key(
            prefs.quality.as_deref().unwrap_or_default(),
        ));
        voice.set_share_audio(prefs.share_audio.unwrap_or(true));
        voice.set_noise(prefs.noise.unwrap_or_default());
        for (uid, p) in &prefs.people {
            if let Ok(uid) = uid.parse() {
                voice.set_gains(uid, p.gains());
            }
        }
        crate::media::sweep();
        let media = crate::media::Media::new(rt.clone(), api.clone(), sounds.clone(), ctx.clone());
        let direct = crate::direct::Direct::new(
            rt.clone(),
            api.clone(),
            {
                let gw = gw.clone();
                let signals = signals.clone();
                // Sealed for the other one (e2e/signals.rs), all but the
                // transfer's number and a refusal, which the server reads.
                Box::new(move |to, msg, file: &str, data: Value| {
                    let sid = data
                        .get("sid")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if data.get("error").is_some() {
                        gw.send(&op::direct(to, msg, file, data));
                        return;
                    }
                    let (gw, file_id) = (gw.clone(), file.to_owned());
                    let context = crate::e2e::signals::direct_context(msg, file, &sid);
                    signals.seal(
                        to,
                        context,
                        data,
                        Box::new(move |mut sealed| {
                            sealed["sid"] = json!(sid);
                            gw.send(&op::direct(to, msg, &file_id, sealed));
                        }),
                    );
                })
            },
            crate::direct::dir(&shell),
            {
                let tx = tx.clone();
                let ctx = ctx.clone();
                Box::new(move |news| {
                    let _ = tx.send(Msg::Direct(news));
                    ctx.request_repaint();
                })
            },
        );
        let mut core = Core {
            ctx,
            rt,
            api,
            gw,
            gw_task: None,
            tx,
            rx,
            shell,
            images,
            giphy,
            sounds,
            voice,
            music,
            media,
            prefs,
            e2e,
            signals,
            fresh_locked: HashSet::new(),
            sealing: HashMap::new(),
            warned_old: HashSet::new(),
            provision: None,
            joining: None,
            asked_with: HashMap::new(),
            day_round: 0,
            day_done: i64::MIN,
            expiring: HashSet::new(),
            phase: Phase::Waiting { retry: None },
            google: None,
            account: crate::google::Account::default(),
            audio_devices: None,
            wait_tries: 0,
            me: None,
            servers: Vec::new(),
            dms: Vec::new(),
            friends: Vec::new(),
            users: HashMap::new(),
            blocks: HashSet::new(),
            revealed: HashSet::new(),
            spoilers: HashSet::new(),
            msgs: HashMap::new(),
            unread: HashMap::new(),
            typing: HashMap::new(),
            view: View::None,
            server_version: String::new(),
            drafts: HashMap::new(),
            reply: None,
            editing: None,
            files: Vec::new(),
            direct_send: false,
            direct,
            sending: Vec::new(),
            outbox: Vec::new(),
            next_send: 0,
            expanded: HashSet::new(),
            stickers: Vec::new(),
            join_requests: Vec::new(),
            sticker_query: String::new(),
            sticker_typed: None,
            sticker_tab: StickerTab::Trending,
            sticker_focus: false,
            emoji,
            emoji_query: String::new(),
            emoji_focus: false,
            emoji_jump: None,
            emoji_tones_open: false,
            emoji_pairs: None,
            keep_moving: false,
            stick_bottom: true,
            focus_composer: false,
            friend_tab: FriendTab::Online,
            add_friend: String::new(),
            show_members: true,
            modal: None,
            modal_focus: false,
            modal_scroll: None,
            pop: None,
            toasts: Vec::new(),
            reconnecting: false,
            update_ready: None,
            gate: Gate::Open,
            answered: false,
            update_moved: None,
            started_hidden: false,
            pending_go: None,
            wanted: None,
            busy: HashSet::new(),
            errors: HashMap::new(),
            last_typing: None,
            focused: true,
            badge: 0,
            raise: false,
            quitting: false,
            ready_to_close: false,
            arrived_stamps: Vec::new(),
        };
        core.wait_for_server();
        core.start_updates();
        core
    }

    // ------------------------------------------------------------ plumbing

    /// Runs `fut` on the runtime and applies what it returns.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let msg = fut.await;
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn start_form(&mut self, err: ErrTo) {
        if let ErrTo::Form(f) = err {
            self.busy.insert(f);
            self.errors.remove(&f);
        }
    }

    /// Calls the server; `then` says what to do with the answer.
    pub fn call(&mut self, req: Req, then: Then, err: ErrTo) {
        self.start_form(err);
        let api = self.api.clone();
        self.spawn(async move {
            let res = api.call(&req).await;
            Msg::Done { then, err, res }
        });
    }

    pub fn send_op(&self, v: &Value) {
        self.gw.send(v);
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        self.toasts.push(Toast {
            text,
            until: Instant::now() + TOAST_FOR,
        });
        self.ctx.request_repaint_after(TOAST_FOR);
    }

    // ------------------------------------------------------------ the loop

    /// Applies everything that arrived since the last frame and expires
    /// what has run out. Called by eframe before every frame, and while the
    /// window is hidden whenever something asks for a repaint.
    pub fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        self.watch_gate();
        self.resend_unconfirmed(false);
        self.media.tick();
        self.giphy.tick();
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        let mut next: Option<Instant> = self.toasts.iter().map(|t| t.until).min();
        for list in self.typing.values_mut() {
            list.retain(|_, until| *until > now);
            if let Some(&u) = list.values().min() {
                next = Some(next.map_or(u, |n| n.min(u)));
            }
        }
        if let Some(at) = next {
            self.ctx
                .request_repaint_after(at.saturating_duration_since(now));
        }
        // Coming back to the window reads what is on screen. Windows is
        // asked directly: a window that started in the tray has never been
        // drawn, so egui knows nothing about its focus yet.
        let focused = match self.shell.hwnd() {
            Some(hwnd) => crate::winsys::is_foreground(hwnd),
            None => self.ctx.input(|i| i.viewport().focused.unwrap_or(false)),
        };
        if focused
            && !self.focused
            && let Some(cid) = self.view.channel()
        {
            self.mark_read(cid);
        }
        self.focused = focused;
        self.voice.tick();
    }

    /// The end of every frame: what arrived before it is on screen now.
    pub fn frame_done(&mut self) {
        if self.arrived_stamps.is_empty() {
            return;
        }
        let now = crate::probe::wall_ms();
        for (series, sent) in self.arrived_stamps.drain(..) {
            crate::probe::record(series, now - sent);
        }
    }

    /// A message that did not go out: its copy goes, and the text goes back
    /// to the box (unless something new was typed there).
    fn unsent(
        &mut self,
        send: u64,
        cid: i64,
        content: String,
        files: Vec<PendingFile>,
        error: String,
    ) {
        crate::probe::count("msg.recusada", 1);
        self.sending.retain(|s| s.id != send);
        if let Some(st) = self.msgs.get_mut(&cid) {
            let temp = temp_id(send);
            st.list.retain(|m| !(m.pending && m.id == temp));
        }
        self.toast(error);
        let draft = self.drafts.entry(cid).or_default();
        if self.view.channel() == Some(cid) && draft.is_empty() {
            *draft = content;
            if !files.is_empty() {
                self.direct_send = files.iter().any(|f| f.direct);
            }
            self.files = files;
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Gw(generation, ev) => {
                if generation == self.gw.generation() {
                    self.gateway(ev);
                }
            }
            Msg::Done { then, err, res } => {
                if let Then::Sent(id) = then {
                    self.sending.retain(|s| s.id != id);
                }
                self.done(then, err, res);
            }
            Msg::SendFailed {
                id,
                cid,
                content,
                files,
                error,
            } => self.unsent(id, cid, content, files, error),
            Msg::Picked(Pick::Attach, paths) => self.add_files(paths),
            Msg::Picked(Pick::Avatar, paths) => {
                if let Some(path) = paths.into_iter().next() {
                    let api = self.api.clone();
                    self.spawn(async move {
                        let progress = crate::net::Progress::new(0);
                        let res = match api.upload(&path, &progress).await {
                            Ok(up) => Ok(json!({ "url": up.url, "kind": up.kind })),
                            Err(e) => Err(e),
                        };
                        Msg::Done {
                            then: Then::Avatar,
                            err: ErrTo::Toast,
                            res,
                        }
                    });
                }
            }
            Msg::Picked(Pick::ServerIcon(sid), paths) => {
                if let Some(path) = paths.into_iter().next() {
                    self.upload_server_icon(sid, path);
                }
            }
            Msg::Picked(Pick::Slides, paths) => {
                if let Some(path) = paths.into_iter().next() {
                    self.present(path);
                }
            }
            Msg::Alive => self.server_answered(),
            Msg::Offline(delay) => {
                self.phase = Phase::Waiting {
                    retry: Some(Instant::now() + delay),
                };
                self.ctx.request_repaint_after(Duration::from_secs(1));
            }
            Msg::Tray(action) => self.tray(action),
            Msg::Navigate(view) => {
                self.raise = true;
                if self.phase != Phase::Ready {
                    // Not connected yet (Windows may have started the app
                    // for this very click): go there once it is.
                    self.wanted = Some(view);
                } else if self.valid_view(view) {
                    self.go_to(view);
                }
            }
            Msg::Update(news) => self.update_news(news),
            Msg::Toast(text) => self.toast(text),
            Msg::AudioDevices(devices) => self.audio_devices = Some(devices),
            Msg::ReadyToClose => self.ready_to_close = true,
            Msg::Google(res) => self.google_done(res),
            Msg::Direct(news) => self.direct_news(news),
            Msg::DirectSave {
                attachment,
                msg,
                cid,
                from,
                path,
            } => {
                if let Err(e) = self.direct.download(&attachment, msg, cid, from, path) {
                    self.toast(e);
                }
            }
            Msg::E2e(state) => self.e2e_state(state),
            Msg::Keys => self.reopen_all(false),
            Msg::Sealed { send, env, missing } => self.sealed(send, env, missing),
            Msg::SealFailed { send, error } => self.seal_failed(send, error),
            Msg::NewDay(round) => {
                if round == self.day_round {
                    self.new_day();
                }
            }
            Msg::Expire { cid, id } => self.expire(cid, id),
        }
    }

    /// What the files sent directly have to say.
    fn direct_news(&mut self, news: crate::direct::News) {
        use crate::direct::News;
        match news {
            News::Changed => {}
            News::Downloaded { name, cid } => {
                if !self.focused {
                    let view = self.channel_view(cid);
                    self.notify(
                        crate::t!("core.download_done_title").into(),
                        name.clone(),
                        view,
                    );
                }
                self.toast(crate::tf!("core.download_done", name = name));
            }
            News::Stopped { name, from, why } => {
                let who = self.user(from).name().to_owned();
                let (text, _) = crate::direct::failure(&why, &who);
                self.toast(format!("{name}: {text}"));
            }
        }
    }

    /// Where conversation `cid` is: a direct one, or a server's channel.
    fn channel_view(&self, cid: i64) -> View {
        self.servers
            .iter()
            .find(|s| s.channel(cid).is_some())
            .map_or(View::Home { channel: Some(cid) }, |s| View::Server {
                server: s.id,
                channel: Some(cid),
            })
    }

    /// "Baixar" on a file sent directly: where it goes is asked on a
    /// thread of its own (as the file picker is), then it starts.
    pub fn save_direct(&self, attachment: Attachment, msg: i64, cid: i64, from: i64) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Salvar arquivo")
                .set_file_name(&attachment.name)
                .save_file()
            else {
                return;
            };
            let _ = tx.send(Msg::DirectSave {
                attachment,
                msg,
                cid,
                from,
                path,
            });
            ctx.request_repaint();
        });
    }

    /// "Abrir" on a downloaded file (or "Mostrar na pasta"): opened by
    /// Windows; one that is no longer there is offered again.
    pub fn open_direct(&mut self, id: &str, path: PathBuf, folder: bool) {
        if !path.exists() {
            self.direct.forget(id);
            self.toast(crate::t!("core.file_gone"));
            return;
        }
        std::thread::spawn(move || {
            let opened = if folder {
                // Explorer reads `/select,"<path>"` as it is, quotes and
                // all, so it goes without the usual quoting.
                use std::os::windows::process::CommandExt;
                let mut select = std::ffi::OsString::from("/select,\"");
                select.push(path.as_os_str());
                select.push("\"");
                std::process::Command::new("explorer.exe")
                    .raw_arg(select)
                    .spawn()
                    .map(drop)
            } else {
                open::that(&path)
            };
            if let Err(e) = opened {
                crate::logln!("direto: não abri {}: {e}", path.display());
            }
        });
    }

    fn tray(&mut self, action: TrayAction) {
        match action {
            TrayAction::Open => self.raise = true,
            TrayAction::Mute => self.toggle_mute(),
            TrayAction::Deafen => self.voice.toggle_deaf(),
            TrayAction::Quit => self.quit(),
        }
    }

    /// Closes the app for real (the tray's "Sair", closing the window with
    /// the tray turned off, the installer). The window goes at once, and
    /// the app once the call it was in has hung up properly: its
    /// connections close and give back their relay instead of being cut
    /// off. `can_close` says when.
    pub fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.voice.leave(true, Cue::None);
        let call = self.voice.hanging_up();
        // Downloads and sends stop where they are (to go on next time),
        // and their lists are on the disk.
        self.direct.quit();
        self.gw.end();
        crate::tray::remove();
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.spawn(async move {
            if let Some(call) = call
                && tokio::time::timeout(QUIT_WAIT, call).await.is_err()
            {
                crate::logln!("sair: a chamada não terminou em {QUIT_WAIT:?}");
            }
            Msg::ReadyToClose
        });
    }

    /// Quitting, with nothing left to wait for: the window should close
    /// (and, hidden or not, keeps being told to until it does).
    pub fn can_close(&self) -> bool {
        self.ready_to_close
    }

    // ------------------------------------------------------------ startup

    /// The app shows the sign-in only once the server answers.
    fn wait_for_server(&mut self) {
        let api = self.api.clone();
        let tries = self.wait_tries;
        self.spawn(async move {
            if api.alive().await {
                Msg::Alive
            } else {
                Msg::Offline(gateway::backoff(tries))
            }
        });
    }

    /// "Tentar agora" on the waiting screens.
    pub fn retry_now(&mut self) {
        match self.phase {
            Phase::Waiting { .. } => {
                self.wait_tries = 0;
                self.phase = Phase::Waiting { retry: None };
                self.wait_for_server();
            }
            Phase::Connecting { .. } => {
                self.phase = Phase::Connecting { retry: None };
                self.gw.retry_now();
            }
            _ => {}
        }
    }

    /// Called every frame on the waiting screen: tries again once the wait
    /// is over.
    pub fn waiting_tick(&mut self) {
        if let Phase::Waiting { retry: Some(at) } = self.phase {
            if Instant::now() >= at {
                self.wait_tries = self.wait_tries.saturating_add(1);
                self.phase = Phase::Waiting { retry: None };
                self.wait_for_server();
            } else {
                self.ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

    fn server_answered(&mut self) {
        self.wait_tries = 0;
        // A newer version may be about to replace this one: nothing signs
        // in (nobody would see the chat come and go) until that is known.
        if !matches!(self.gate, Gate::Open) {
            self.answered = true;
            return;
        }
        let token = std::env::var("PROSA_SMOKE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| self.shell.load_session());
        match token {
            Some(token) => {
                self.api.set_token(Some(token));
                self.view = View::None;
                self.phase = Phase::Connecting { retry: None };
                self.connect();
            }
            None => self.show_auth(),
        }
    }

    pub fn show_auth(&mut self) {
        self.phase = Phase::Auth;
        self.errors.remove(&FormId::Login);
    }

    /// "Continuar com o Google": opens Google's page in the browser and
    /// waits for it to come back (google.rs).
    pub fn google_sign_in(&mut self) {
        if self.google.is_some() || self.phase != Phase::Auth {
            return;
        }
        self.errors.remove(&FormId::Login);
        let api = self.api.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.google = Some(self.rt.spawn(async move {
            let res = crate::google::sign_in(api).await;
            let _ = tx.send(Msg::Google(res));
            ctx.request_repaint();
        }));
    }

    /// Gives up on the sign-in under way (its browser page stops working).
    pub fn cancel_google(&mut self) {
        if let Some(task) = self.google.take() {
            task.abort();
        }
    }

    /// How a sign-in with Google ended.
    fn google_done(&mut self, res: ApiResult) {
        if self.google.take().is_none() {
            // Cancelled meanwhile.
            return;
        }
        let value = match res {
            Ok(v) => v,
            Err(e) => {
                self.errors.insert(FormId::Login, e.message);
                return;
            }
        };
        // The person was in the browser: the app comes back in front.
        self.raise = true;
        if let Some(token) = value.get("token").and_then(Value::as_str)
            && self.phase == Phase::Auth
        {
            self.start_session(token.to_owned());
        }
    }

    /// Opens the live connection for the current token.
    fn connect(&mut self) {
        if let Some(task) = self.gw_task.take() {
            task.abort();
        }
        let generation = self.gw.begin();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let out = move |ev: GwEvent| {
            let ok = tx.send(Msg::Gw(generation, ev)).is_ok();
            ctx.request_repaint();
            ok
        };
        self.gw_task = Some(self.rt.spawn(gateway::run(
            self.api.clone(),
            self.gw.clone(),
            generation,
            out,
        )));
    }

    fn start_session(&mut self, token: String) {
        if !self.shell.save_session(Some(&token)) {
            crate::logln!("não consegui guardar a sessão");
        }
        self.api.set_token(Some(token));
        self.errors.remove(&FormId::Login);
        self.view = View::None;
        self.phase = Phase::Connecting { retry: None };
        self.connect();
    }

    /// Signs out. `silent` when the server already ended the session.
    pub fn logout(&mut self, silent: bool) {
        self.media.pause_all();
        if let Some(token) = self.api.token()
            && !silent
        {
            let api = self.api.clone();
            self.rt.spawn(async move { api.logout(&token).await });
        }
        self.api.set_token(None);
        self.shell.save_session(None);
        self.voice.leave(true, Cue::Leave);
        self.direct.logout();
        self.e2e.stop();
        self.signals.clear();
        self.provision = None;
        self.fresh_locked.clear();
        self.sealing.clear();
        self.warned_old.clear();
        self.asked_with.clear();
        self.expiring.clear();
        self.direct_send = false;
        self.gw.end();
        if let Some(task) = self.gw_task.take() {
            task.abort();
        }
        self.modal = None;
        self.pop = None;
        self.me = None;
        self.servers.clear();
        self.dms.clear();
        self.friends.clear();
        self.users.clear();
        self.blocks.clear();
        self.msgs.clear();
        self.outbox.clear();
        self.unread.clear();
        self.typing.clear();
        self.drafts.clear();
        self.files.clear();
        for s in self.sending.drain(..) {
            s.progress.cancel.store(true, Ordering::Relaxed);
        }
        self.expanded.clear();
        self.stickers.clear();
        self.sticker_query.clear();
        self.sticker_typed = None;
        self.giphy.set_key("", 0, "");
        self.reply = None;
        self.editing = None;
        self.reconnecting = false;
        self.view = View::None;
        self.set_badge(0);
        self.cancel_google();
        self.account = crate::google::Account::default();
        self.show_auth();
    }

    // ------------------------------------------------------------ answers

    fn done(&mut self, then: Then, err: ErrTo, res: ApiResult) {
        if let ErrTo::Form(f) = err {
            self.busy.remove(&f);
        }
        let value = match res {
            Ok(v) => v,
            Err(e) => {
                if let Then::Messages { cid, .. } = then
                    && let Some(st) = self.msgs.get_mut(&cid)
                {
                    st.loading = false;
                }
                // The favourite did not change after all.
                if let Then::Stickers(before) = then {
                    self.stickers = before;
                }
                if e.expired {
                    self.logout(true);
                }
                match err {
                    ErrTo::Form(f) if !e.expired => {
                        self.errors.insert(f, e.message);
                    }
                    _ => self.toast(e.message),
                }
                return;
            }
        };
        let int = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64);
        match then {
            Then::Nothing => {}
            Then::Sent(_) => {
                // Only while its copy still waits (the gateway was slower,
                // or is down): once the gateway brought it, it may already
                // have been deleted again.
                let mut sent = serde_json::from_value::<Message>(value);
                if let Ok(m) = &mut sent {
                    self.open_msg(m);
                }
                if let Ok(m) = sent
                    && m.nonce.is_some()
                    && let Some(st) = self.msgs.get_mut(&m.channel_id)
                    && !st.loading
                    && st
                        .list
                        .iter()
                        .rev()
                        .take_while(|x| x.pending)
                        .any(|x| x.nonce == m.nonce)
                {
                    crate::probe::count("msg.pela_resposta", 1);
                    place(&mut st.list, m);
                }
            }
            Then::Toast(text) => self.toast(text),
            Then::CloseModal => self.modal = None,
            Then::CloseModalToast(text) => {
                self.modal = None;
                self.toast(text);
            }
            Then::Arrive => {
                self.modal = None;
                // In through a signed invite: my entry, signed with the key
                // its link's secret makes (e2e/members.rs). Only asked (a
                // private server): the entry is whoever accepts it.
                let secret = self.joining.take();
                let pending = value.get("pending").and_then(Value::as_bool) == Some(true);
                if let (Some(secret), Some(sid), Some(inv)) = (
                    secret,
                    int(&value, "id"),
                    value.get("invite").cloned().and_then(|v| {
                        serde_json::from_value::<crate::e2e::members::InviteCert>(v).ok()
                    }),
                ) {
                    if pending {
                        // The owner the invite names is pinned once I am in:
                        // only then can I fetch its creator's identity.
                        self.asked_with.insert(sid, (secret, inv));
                    } else {
                        self.invite_owner(sid, secret, inv, true);
                    }
                }
                // A private server: asked, not in yet.
                if pending {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("o servidor");
                    self.toast(crate::tf!("core.request_sent", name = name));
                } else if let Some(id) = int(&value, "id") {
                    self.arrive(id);
                }
            }
            Then::RoleCreated => {
                if let Some(id) = int(&value, "id")
                    && let Some(Modal::ServerSettings { new_role, role, .. }) = &mut self.modal
                {
                    new_role.clear();
                    // The role arrives with the SERVER_UPDATE that follows;
                    // select it now and fill it in then.
                    *role = Some(RoleEditor {
                        id,
                        name: String::new(),
                        color: [0x96, 0x9b, 0xa8],
                        perms: -1,
                    });
                }
            }
            Then::OpenDm => {
                if let Ok(dm) = serde_json::from_value::<Dm>(value) {
                    let id = dm.id;
                    if !self.dms.iter().any(|d| d.id == id) {
                        self.dms.insert(0, dm);
                        self.index_users();
                    }
                    self.modal = None;
                    self.go_to(View::Home { channel: Some(id) });
                }
            }
            Then::Invite => {
                if let Some(code) = value.get("code").and_then(Value::as_str) {
                    self.open_modal(Modal::Invite {
                        code: code.to_owned(),
                    });
                }
            }
            Then::InviteLink(link) => self.open_modal(Modal::Invite { code: link }),
            Then::Messages { cid, older } => {
                let mut page: Messages = serde_json::from_value(value).unwrap_or_default();
                self.open_page(&mut page.messages);
                page.messages.retain(|m| m.seal != Seal::Gone);
                let st = self.msgs.entry(cid).or_default();
                st.loading = false;
                st.more = page.more;
                if older {
                    let mut list = page.messages;
                    list.append(&mut st.list);
                    st.list = list;
                } else {
                    // Mine still on their way stay, at the end.
                    let waiting: Vec<Message> = std::mem::take(&mut st.list)
                        .into_iter()
                        .filter(|m| m.pending)
                        .collect();
                    st.list = page.messages;
                    st.list.extend(waiting);
                    if self.view.channel() == Some(cid) {
                        self.stick_bottom = true;
                    }
                }
            }
            Then::AccountDeleted => {
                self.logout(true);
                self.toast(crate::t!("core.account_deleted"));
            }
            Then::FriendRequested { close_modal } => {
                if close_modal {
                    self.modal = None;
                } else {
                    self.add_friend.clear();
                }
                self.toast("Pedido de amizade enviado.");
            }
            Then::ReopenReports(sid) => self.open_modal(Modal::Reports { sid }),
            Then::RoleDeleted => {
                if let Some(Modal::ServerSettings { role, .. }) = &mut self.modal {
                    *role = None;
                }
            }
            Then::Avatar => {
                if value.get("kind").and_then(Value::as_str) != Some("image") {
                    self.toast("Escolha uma imagem PNG, JPG, GIF ou WebP.");
                } else if let Some(url) = value.get("url").and_then(Value::as_str) {
                    self.call(
                        Req::patch("/me").with(json!({ "avatar": url })),
                        Then::Nothing,
                        ErrTo::Toast,
                    );
                }
            }
            Then::Stickers(_) => {
                if let Some(list) = sticker_list(&value) {
                    self.stickers = list;
                }
            }
            Then::ServerIcon(sid) => {
                if let Some(url) = value.get("url").and_then(Value::as_str) {
                    self.call(
                        Req::patch(format!("/servers/{sid}")).with(json!({ "icon": url })),
                        Then::Toast("Imagem do servidor salva.".into()),
                        ErrTo::Toast,
                    );
                }
            }
        }
    }

    /// A picture for a server: made into a 256 px square PNG here (an
    /// icon is drawn at 46 px, and every member downloads it), then
    /// uploaded and set.
    pub fn upload_server_icon(&mut self, sid: i64, path: std::path::PathBuf) {
        let api = self.api.clone();
        self.spawn(async move {
            let made =
                tokio::task::spawn_blocking(move || square_png(&path, SERVER_ICON_SIDE)).await;
            let res = match made {
                Ok(Ok(png)) => {
                    let progress = crate::net::Progress::new(0);
                    let up = api.upload(&png, &progress).await;
                    let _ = std::fs::remove_file(&png);
                    up.map(|up| json!({ "url": up.url }))
                }
                Ok(Err(e)) => Err(crate::net::ApiError::msg(e)),
                Err(e) => Err(crate::net::ApiError::msg(e.to_string())),
            };
            Msg::Done {
                then: Then::ServerIcon(sid),
                err: ErrTo::Toast,
                res,
            }
        });
    }

    /// Lets someone who asked into the server, with my signature on it
    /// (e2e/members.rs).
    pub fn accept_request(&mut self, sid: i64, uid: i64) {
        let cert = self.e2e.sign_member("aceito", sid, uid);
        self.call(
            Req::put(format!("/servers/{sid}/requests/{uid}")).with(json!({ "cert": cert })),
            Then::Nothing,
            ErrTo::Toast,
        );
    }

    /// I vouch that `uid` is a member of `sid` (someone who came in with an
    /// invite from before signed ones): from then on they get the keys.
    pub fn vouch(&mut self, sid: i64, uid: i64) {
        let Some(cert) = self.e2e.sign_member("garantia", sid, uid) else {
            self.toast(crate::e2e::no_keys_text());
            return;
        };
        self.call(
            Req::post(format!("/servers/{sid}/e2e/certs")).with(json!({ "certs": [cert] })),
            Then::Toast("Membro confirmado.".into()),
            ErrTo::Toast,
        );
    }

    /// Turns down someone who asked.
    pub fn deny_request(&mut self, sid: i64, uid: i64) {
        self.call(
            Req::delete(format!("/servers/{sid}/requests/{uid}")),
            Then::Nothing,
            ErrTo::Toast,
        );
    }

    /// Gives up my request to join `sid`.
    pub fn cancel_request(&mut self, sid: i64) {
        let me = self.me_id();
        self.call(
            Req::delete(format!("/servers/{sid}/requests/{me}")),
            Then::Toast("Pedido cancelado.".into()),
            ErrTo::Toast,
        );
    }

    /// Makes `sid` private (joining takes a request) or open again.
    pub fn set_private(&mut self, sid: i64, on: bool) {
        self.call(
            Req::patch(format!("/servers/{sid}")).with(json!({ "private": on })),
            Then::Toast(if on {
                "Servidor privado: quem tiver um convite vai pedir para entrar.".into()
            } else {
                "Servidor aberto: quem tiver um convite entra direto.".into()
            }),
            ErrTo::Toast,
        );
    }

    // ------------------------------------------------------------ the gateway

    fn gateway(&mut self, ev: GwEvent) {
        match ev {
            GwEvent::Closed(delay) => {
                // The gateway going away does not take the call with it: the
                // audio and video travel peer to peer, and the gateway only
                // carries the signalling (which comes back when it does, and
                // in the meantime the ongoing connections keep flowing).
                // On READY the call says it is still here (Voice::rejoin):
                // the server holds my place for a few seconds, so a quick
                // drop goes by as if nothing happened; after that it took
                // me out of the call, and the connections are made again
                // as I come back (voice/rtc.rs).
                self.resend_unconfirmed(true);
                if self.phase == Phase::Ready {
                    self.reconnecting = true;
                } else if self.api.signed_in() {
                    self.phase = Phase::Connecting {
                        retry: Some(Instant::now() + delay),
                    };
                    self.ctx.request_repaint_after(Duration::from_secs(1));
                }
            }
            GwEvent::Event(t, d) => self.event(&t, d),
        }
    }

    fn event(&mut self, t: &str, d: Value) {
        fn parse<T: serde::de::DeserializeOwned>(d: Value) -> Result<T, serde_json::Error> {
            serde_json::from_value(d)
        }
        match t {
            "READY" => {
                if let Ok(ready) = parse::<Ready>(d) {
                    self.ready(ready);
                }
            }
            "AUTH_FAIL" => self.logout(true),
            "ERROR" => {
                let text = d
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Algo deu errado");
                let text = text.to_owned();
                self.toast(text);
            }
            // The server turned down a message sent through the gateway.
            "SEND_FAILED" => {
                let nonce = d.get("nonce").and_then(Value::as_str).unwrap_or_default();
                if let Some(i) = self.outbox.iter().position(|u| u.out.nonce == nonce) {
                    let out = self.outbox.remove(i).out;
                    let error = d
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or(crate::t!("core.send_failed"))
                        .to_owned();
                    self.unsent(out.send, out.cid, out.content, Vec::new(), error);
                }
            }
            "STATUS" => {
                if let Some(status) = d.get("status").and_then(Value::as_str) {
                    let status = Status::from(status.to_owned());
                    if let Some(me) = &mut self.me {
                        me.status = status;
                    }
                    self.index_users();
                }
            }
            "SERVER_UPDATE" => {
                if let Ok(s) = parse::<Server>(d) {
                    self.server_update(s);
                }
            }
            "SERVER_REMOVE" => {
                if let Some(id) = d.get("id").and_then(Value::as_i64) {
                    self.server_remove(id);
                }
            }
            // My requests to join private servers, as they are now.
            "JOIN_REQUESTS" => {
                let list: Vec<MyRequest> = d
                    .get("requests")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                // One that went because I am in now: it was accepted.
                let accepted: Vec<String> = self
                    .join_requests
                    .iter()
                    .filter(|r| !list.iter().any(|n| n.server.id == r.server.id))
                    .filter(|r| self.server(r.server.id).is_some())
                    .map(|r| crate::tf!("core.request_accepted", server = r.server.name))
                    .collect();
                self.join_requests = list;
                for text in accepted {
                    self.toast(text);
                }
            }
            "JOIN_DENIED" => {
                let name = d.get("name").and_then(Value::as_str).unwrap_or("servidor");
                let text = format!("Seu pedido para entrar em {name} foi recusado.");
                self.toast(text);
            }
            "MESSAGE_CREATE" => {
                if let Ok(mut m) = parse::<Message>(d) {
                    self.open_msg(&mut m);
                    // Its time is up already (a conversation whose messages
                    // disappear): never shown.
                    if m.seal != Seal::Gone {
                        self.message_create(m);
                    }
                }
            }
            "MESSAGE_UPDATE" => {
                if let Ok(mut m) = parse::<Message>(d) {
                    self.open_msg(&mut m);
                    if m.seal == Seal::Gone {
                        self.message_gone(m.channel_id, m.id);
                    } else if let Some(st) = self.msgs.get_mut(&m.channel_id)
                        && let Some(slot) = st.list.iter_mut().find(|x| x.id == m.id)
                    {
                        *slot = m;
                    }
                }
            }
            // Someone published other keys: the next bundle of theirs this
            // PC takes is checked against the identity pinned for them.
            "KEYS_CHANGED" => {
                if let Some(uid) = d.get("userId").and_then(Value::as_i64) {
                    self.e2e.forget(uid);
                    // Mine too: the record's new entry under my account is
                    // checked against the keys this PC has.
                    self.fetch_keys(crate::e2e::Need {
                        users: if uid == self.me_id() {
                            HashSet::new()
                        } else {
                            [uid].into_iter().collect()
                        },
                        record: true,
                        ..crate::e2e::Need::default()
                    });
                }
            }
            // Another PC of my account: asking for the keys, or sending them.
            "PROVISION" => self.provision_event(d),
            "MESSAGE_DELETE" => {
                let id = d.get("id").and_then(Value::as_i64).unwrap_or(0);
                let cid = d.get("channel_id").and_then(Value::as_i64).unwrap_or(0);
                self.message_gone(cid, id);
            }
            "TYPING" => {
                let cid = d.get("channelId").and_then(Value::as_i64).unwrap_or(0);
                let uid = d.get("userId").and_then(Value::as_i64).unwrap_or(0);
                self.typing
                    .entry(cid)
                    .or_default()
                    .insert(uid, Instant::now() + TYPING_FOR);
            }
            "PRESENCE" => {
                let uid = d.get("userId").and_then(Value::as_i64).unwrap_or(0);
                let status = Status::from(
                    d.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
                self.patch_user(uid, |u| u.status = status);
                // Downloads waiting on them try again (one invisible says
                // "offline" too as they connect, and may well answer).
                self.direct.presence(uid);
            }
            // A step of setting up the transfer of a file sent directly.
            "DIRECT" => {
                let from = d.get("from").and_then(Value::as_i64).unwrap_or(0);
                let msg = d.get("msg").and_then(Value::as_i64).unwrap_or(0);
                let file = d.get("file").and_then(Value::as_str).unwrap_or_default();
                let data = d.get("data").cloned().unwrap_or(Value::Null);
                let sid = data
                    .get("sid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                // A refusal (from the server, or the other one) says only
                // why; anything else must be sealed for me by them.
                if data.get("error").is_some() {
                    self.direct.signal(from, msg, file, data);
                } else {
                    let direct = self.direct.clone();
                    let file_id = file.to_owned();
                    let context = crate::e2e::signals::direct_context(msg, file, &sid);
                    self.signals.open(
                        from,
                        context,
                        data,
                        Box::new(move |mut opened| {
                            opened["sid"] = json!(sid);
                            direct.signal(from, msg, &file_id, opened);
                        }),
                    );
                }
            }
            "USER_UPDATE" => {
                if let Ok(u) = parse::<User>(d) {
                    if let Some(me) = &mut self.me
                        && me.id == u.id
                    {
                        me.apply_profile(&u);
                    }
                    self.patch_user(u.id, |x| x.apply_profile(&u));
                    for st in self.msgs.values_mut() {
                        for m in &mut st.list {
                            if let Some(a) = &mut m.author
                                && a.id == u.id
                            {
                                a.apply_profile(&u);
                            }
                        }
                    }
                }
            }
            "FRIEND_UPDATE" => {
                if let Ok(f) = parse::<Friend>(d) {
                    match self.friends.iter_mut().find(|x| x.user.id == f.user.id) {
                        Some(slot) => *slot = f,
                        None => self.friends.push(f),
                    }
                    self.index_users();
                }
            }
            "FRIEND_REMOVE" => {
                let uid = d.get("userId").and_then(Value::as_i64).unwrap_or(0);
                self.friends.retain(|f| f.user.id != uid);
            }
            // My favourite stickers changed (here or on another device).
            "STICKERS" => {
                if let Some(list) = sticker_list(&d) {
                    self.stickers = list;
                }
            }
            "BLOCKS" => {
                let ids: Vec<i64> = d
                    .get("ids")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                self.blocks = ids.into_iter().collect();
                self.revealed.clear();
            }
            "DM_CREATE" => {
                if let Ok(dm) = parse::<Dm>(d) {
                    if !self.dms.iter().any(|x| x.id == dm.id) {
                        self.dms.push(dm);
                    }
                    self.index_users();
                }
            }
            // A direct conversation changed (how long its messages last).
            "DM_UPDATE" => {
                if let Ok(dm) = parse::<Dm>(d)
                    && let Some(slot) = self.dms.iter_mut().find(|x| x.id == dm.id)
                {
                    *slot = dm;
                    self.index_users();
                }
            }
            "DM_REMOVE" => {
                if let Some(id) = d.get("id").and_then(Value::as_i64) {
                    self.dm_remove(id);
                }
            }
            "USER_DELETED" => {
                let uid = d.get("userId").and_then(Value::as_i64).unwrap_or(0);
                self.friends.retain(|f| f.user.id != uid);
                let gone: Vec<i64> = self
                    .dms
                    .iter()
                    .filter(|dm| dm.user.id == uid)
                    .map(|dm| dm.id)
                    .collect();
                for id in gone {
                    self.dm_remove(id);
                }
                for st in self.msgs.values_mut() {
                    st.list.retain(|m| author(m) != uid);
                }
            }
            "VOICE_STATE" => {
                let sid = d.get("serverId").and_then(Value::as_i64).unwrap_or(0);
                let cid = d.get("channelId").and_then(Value::as_i64).unwrap_or(0);
                let users: Vec<VoiceUser> = d
                    .get("users")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let Some(s) = self.servers.iter_mut().find(|s| s.id == sid) else {
                    return;
                };
                s.set_voice(cid, users.clone());
                if self.voice.channel_id() == Some(cid)
                    && let Some(me) = self.me.as_ref().map(|m| m.id)
                {
                    self.voice.sync(&users, me);
                    // The window over my shared screen names who draws
                    // (ink/overlay.rs), from its own thread.
                    let names: Vec<(i64, String)> = users
                        .iter()
                        .map(|u| (u.user_id, self.user(u.user_id).name().to_owned()))
                        .collect();
                    self.voice.ink().set_names(names);
                }
            }
            "SIGNAL" => {
                let from = d.get("from").and_then(Value::as_i64).unwrap_or(0);
                let data = d.get("data").cloned().unwrap_or(Value::Null);
                if let Some(me) = self.me.as_ref().map(|m| m.id) {
                    self.voice.on_signal(from, data, me);
                }
            }
            "VOICE_KICK" => {
                self.voice.leave(true, Cue::Disconnect);
                self.toast(crate::t!("core.joined_elsewhere"));
            }
            _ => {}
        }
    }

    fn ready(&mut self, d: Ready) {
        self.reconnecting = false;
        self.server_version = d.version;
        self.giphy.set_key(&d.giphy, d.user.id, &d.user.username);
        self.stickers = d.stickers;
        self.account = crate::google::Account::from_value(&d.account);
        self.me = Some(d.user);
        self.servers = d.servers;
        self.dms = d.dms;
        self.friends = d.friends;
        self.join_requests = d.requests;
        self.blocks = d.blocks.into_iter().collect();
        self.users.clear();
        self.index_users();
        self.msgs.clear();
        self.typing.clear();
        let mut v = self.view;
        if let Some(wanted) = self.wanted.take()
            && self.valid_view(wanted)
        {
            v = wanted;
        }
        if !self.valid_view(v) {
            v = self.prefs.view.unwrap_or(View::None);
        }
        if !self.valid_view(v) {
            v = match self.servers.first() {
                Some(s) => View::Server {
                    server: s.id,
                    channel: None,
                },
                None => View::HOME,
            };
        }
        self.phase = Phase::Ready;
        self.go_to(v);
        // In a call before the gateway dropped: tell the server we are
        // still in it (voice_join on the new connection), and make again
        // the connections that were waiting on something lost meanwhile.
        self.voice.rejoin();
        // The downloads of files sent directly go on (and those waiting on
        // someone who came back meanwhile).
        self.direct.ready(self.me_id());
        // The account's keys: from disk, new, or asked of another PC (whose
        // request goes again on every READY, for a PC that connected since).
        match self.e2e.state() {
            // Back after a while (the PC asleep, say): the day may have
            // turned meanwhile, with its keys.
            crate::e2e::State::Ready => self.new_day_if_due(),
            crate::e2e::State::NeedsDevice => self.ask_for_keys(),
            _ => self.start_e2e(),
        }
    }

    fn server_update(&mut self, mut s: Server) {
        // Reported messages are encrypted like any other: the moderators
        // read them with their own keys.
        let mut reported: Vec<Message> = s.reports.iter().map(|r| r.message.clone()).collect();
        self.open_page(&mut reported);
        for (r, m) in s.reports.iter_mut().zip(reported) {
            r.message = m;
        }
        let id = s.id;
        // Someone new asking to come in (I may let them): said once, as
        // a mention would be.
        let before: HashSet<i64> = self
            .server(id)
            .map(|old| old.requests.iter().map(|r| r.user.id).collect())
            .unwrap_or_default();
        let known = self.server(id).is_some();
        let asking: Vec<String> = s
            .requests
            .iter()
            .filter(|r| known && !before.contains(&r.user.id))
            .map(|r| r.user.name().to_owned())
            .collect();
        if !asking.is_empty() {
            let text = format!("{} pediu para entrar em {}.", asking.join(", "), s.name);
            if !self.focused {
                let view = View::Server {
                    server: id,
                    channel: None,
                };
                self.notify("Pedido de entrada".into(), text.clone(), view);
            }
            self.toast(text);
        }
        match self.servers.iter_mut().find(|x| x.id == id) {
            Some(slot) => *slot = s,
            None => self.servers.push(s),
        }
        self.index_users();
        self.members_check(id);
        // In now, after asking with a signed invite: its owner.
        if let Some((secret, inv)) = self.asked_with.remove(&id) {
            self.invite_owner(id, secret, inv, false);
        }
        if let (Some(vc), Some(me)) = (self.voice.channel_id(), self.me.as_ref().map(|m| m.id)) {
            let s = self.servers.iter().find(|x| x.id == id);
            let here = s.is_some_and(|s| s.channel(vc).is_some());
            let mine = s.is_some_and(|s| {
                s.voice
                    .values()
                    .any(|users| users.iter().any(|u| u.user_id == me))
            });
            let lost = self.servers.iter().all(|x| x.channel(vc).is_none());
            if lost || (here && !mine) {
                self.voice.leave(true, Cue::Disconnect);
            }
        }
        if self.pending_go == Some(id) {
            self.pending_go = None;
            self.go_to(View::Server {
                server: id,
                channel: None,
            });
            return;
        }
        if let View::Server { server, channel } = self.view
            && server == id
        {
            let gone =
                channel.is_none_or(|c| self.server(id).is_none_or(|s| s.channel(c).is_none()));
            if gone {
                self.go_to(View::Server {
                    server: id,
                    channel: None,
                });
            }
        }
        self.check_modal();
    }

    fn server_remove(&mut self, id: i64) {
        let Some(s) = self.server(id) else { return };
        if let Some(vc) = self.voice.channel_id()
            && s.channel(vc).is_some()
        {
            self.voice.leave(true, Cue::Disconnect);
        }
        self.servers.retain(|s| s.id != id);
        if self.view.server() == Some(id) {
            self.modal = None;
            self.go_to(View::HOME);
        }
        self.check_modal();
    }

    fn dm_remove(&mut self, id: i64) {
        self.dms.retain(|d| d.id != id);
        self.msgs.remove(&id);
        self.unread.remove(&id);
        if self.view == (View::Home { channel: Some(id) }) {
            self.go_to(View::HOME);
        }
        self.set_badge(self.total_hot());
    }

    /// In server `sid` through the signed invite `inv` (whose link's secret
    /// is `secret`): once its creator's identity is here, the owner it names
    /// is pinned (e2e/members.rs) and, with `sign`, my entry goes up, signed
    /// with the key the secret makes.
    fn invite_owner(
        &mut self,
        sid: i64,
        secret: [u8; 32],
        inv: crate::e2e::members::InviteCert,
        sign: bool,
    ) {
        let (e2e, api) = (self.e2e.clone(), self.api.clone());
        self.spawn(async move {
            let need = crate::e2e::Need {
                users: [inv.by].into_iter().collect(),
                ..crate::e2e::Need::default()
            };
            if let Err(e) = e2e.fetch(need).await {
                crate::logln!("e2e: a identidade de quem convidou: {e}");
            }
            let res = if !sign {
                e2e.pin_owner(&secret, &inv);
                Ok(Value::Null)
            } else if let Some(cert) = e2e.entry(&secret, &inv) {
                api.call(
                    &Req::post(format!("/servers/{sid}/e2e/certs"))
                        .with(json!({ "certs": [cert] })),
                )
                .await
            } else {
                crate::logln!("e2e: o link não é deste convite; a entrada fica sem certificado");
                Ok(Value::Null)
            };
            Msg::Done {
                then: Then::Nothing,
                err: ErrTo::Toast,
                res,
            }
        });
    }

    /// A message deleted, or whose time came: it goes from the screen, with
    /// what was under way with it.
    pub(super) fn message_gone(&mut self, cid: i64, id: i64) {
        if let Some(st) = self.msgs.get_mut(&cid) {
            st.list.retain(|m| m.id != id);
        }
        self.fresh_locked.remove(&id);
        self.direct.message_deleted(id);
        if self.editing.as_ref().is_some_and(|e| e.id == id) {
            self.editing = None;
        }
        if self.reply.as_ref().is_some_and(|r| r.id == id) {
            self.reply = None;
        }
    }

    fn message_create(&mut self, m: Message) {
        let uid = author(&m);
        if let Some(a) = &m.author {
            self.users.entry(a.id).or_insert_with(|| a.clone());
        }
        if let Some(nonce) = &m.nonce
            && uid == self.me_id()
        {
            if crate::probe::lose("eco") {
                return;
            }
            // It arrived: nothing to send again.
            self.outbox.retain(|u| &u.out.nonce != nonce);
        }
        let cid = m.channel_id;
        if let Some(st) = self.msgs.get_mut(&cid)
            && !st.loading
        {
            place(&mut st.list, m.clone());
        }
        if let Some(sent) = crate::probe::message_stamp(&m.content) {
            let now = crate::probe::wall_ms();
            if uid == self.me_id() {
                crate::probe::record("msg.eco", now - sent);
            } else {
                crate::probe::record("msg.entrega", now - sent);
                self.arrived_stamps.push(("msg.tela", sent));
            }
        }
        if let Some(list) = self.typing.get_mut(&cid) {
            list.remove(&uid);
        }
        let dm = self.dms.iter().position(|d| d.id == cid);
        if let Some(i) = dm {
            // The conversation with the newest message goes to the top.
            let d = self.dms.remove(i);
            self.dms.insert(0, d);
        }
        let me = self.me.as_ref().map_or(0, |u| u.id);
        let current = self.view.channel() == Some(cid) && self.channel_exists(cid);
        if current && self.focused {
            if uid == me || self.stick_bottom {
                self.stick_bottom = true;
            }
            return;
        }
        // What it says (a mention, the notification's text) is only known
        // once it opens.
        if m.seal == Seal::Locked {
            self.fresh_locked.insert(m.id);
            return;
        }
        self.alert(&m);
    }

    /// The unread count, sound and notification of a message that arrived
    /// while I was elsewhere.
    fn alert(&mut self, m: &Message) {
        let uid = author(m);
        let cid = m.channel_id;
        let me = self.me.as_ref().map_or(0, |u| u.id);
        if uid == me || self.blocks.contains(&uid) {
            return;
        }
        if self.view.channel() == Some(cid) && self.channel_exists(cid) && self.focused {
            return;
        }
        let dm = self.dms.iter().position(|d| d.id == cid);
        let hot = dm.is_some() || self.mentions_me(&m.content);
        let u = self.unread.entry(cid).or_default();
        u.n += 1;
        if hot {
            u.hot += 1;
        }
        self.set_badge(self.total_hot());
        let dnd = self.me.as_ref().is_some_and(|u| u.status == Status::Dnd);
        if hot && !dnd {
            self.sounds.play(Cue::Message);
            let mut view = View::Home { channel: Some(cid) };
            let mut place = String::new();
            if dm.is_none() {
                for s in &self.servers {
                    if let Some(c) = s.channel(cid) {
                        view = View::Server {
                            server: s.id,
                            channel: Some(cid),
                        };
                        place = format!(" (#{})", c.label());
                    }
                }
            }
            let who = m.author.as_ref().map_or("Desconhecido", |a| a.name());
            let body = if !m.content.is_empty() {
                m.content.clone()
            } else if m.sticker.is_some() {
                "Enviou uma figurinha".to_owned()
            } else {
                "Enviou um anexo".to_owned()
            };
            self.notify(format!("{who}{place}"), body, view);
        }
    }

    fn notify(&self, title: String, body: String, view: View) {
        if !self.shell.settings().notifications {
            return;
        }
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let on_click = move |_launch: String| {
                let _ = tx.send(Msg::Navigate(view));
                ctx.request_repaint();
            };
            if let Err(e) = crate::notify::show(&title, &body, &view.to_launch(), on_click) {
                crate::logln!("notificação: {e}");
            }
        });
    }

    // ------------------------------------------------------------ people

    fn index_users(&mut self) {
        for s in &self.servers {
            for m in &s.members {
                self.users.insert(m.id, m.clone());
            }
        }
        for d in &self.dms {
            self.users.insert(d.user.id, d.user.clone());
        }
        for f in &self.friends {
            self.users.insert(f.user.id, f.user.clone());
        }
        if let Some(me) = &self.me {
            let mut u = me.clone();
            u.status = me.status.public();
            self.users.insert(u.id, u);
        }
    }

    /// Changes every copy the app holds of one person.
    fn patch_user(&mut self, id: i64, patch: impl Fn(&mut User)) {
        for s in &mut self.servers {
            for m in &mut s.members {
                if m.id == id {
                    patch(m);
                }
            }
        }
        for d in &mut self.dms {
            if d.user.id == id {
                patch(&mut d.user);
            }
        }
        for f in &mut self.friends {
            if f.user.id == id {
                patch(&mut f.user);
            }
        }
        self.index_users();
        let name = self.user(id).name().to_owned();
        self.voice.ink().rename(id, &name);
    }

    /// Whoever this is, as well as the app knows them.
    pub fn user(&self, id: i64) -> User {
        self.users
            .get(&id)
            .cloned()
            .unwrap_or_else(|| User::unknown(id))
    }

    pub fn me_id(&self) -> i64 {
        self.me.as_ref().map_or(0, |u| u.id)
    }

    pub fn known_name(&self, name: &str) -> bool {
        self.users.values().any(|u| u.username == name)
    }

    /// Whoever has this user name, of the people the app knows.
    pub fn user_named(&self, username: &str) -> Option<&User> {
        self.users.values().find(|u| u.username == username)
    }

    /// Whether `content` mentions me: "@name" at the start or after a space,
    /// not followed by another character that names can have.
    pub fn mentions_me(&self, content: &str) -> bool {
        let Some(me) = &self.me else { return false };
        mentions(content, &me.username)
    }

    // ------------------------------------------------------------ lookups

    pub fn server(&self, id: i64) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }

    pub fn current_server(&self) -> Option<&Server> {
        self.view.server().and_then(|id| self.server(id))
    }

    /// The channel on screen, when it is one of a server's.
    pub fn current_channel(&self) -> Option<(&Server, &Channel)> {
        let View::Server { server, channel } = self.view else {
            return None;
        };
        let s = self.server(server)?;
        Some((s, s.channel(channel?)?))
    }

    /// The direct conversation on screen.
    pub fn current_dm(&self) -> Option<&Dm> {
        let View::Home { channel: Some(id) } = self.view else {
            return None;
        };
        self.dms.iter().find(|d| d.id == id)
    }

    fn channel_exists(&self, cid: i64) -> bool {
        self.dms.iter().any(|d| d.id == cid)
            || self.servers.iter().any(|s| s.channel(cid).is_some())
    }

    /// Whether `uid` shares a screen in some voice channel.
    pub fn shares_screen(&self, uid: i64) -> bool {
        self.servers.iter().any(|s| {
            s.voice
                .values()
                .flatten()
                .any(|v| v.user_id == uid && v.video == Some(Video::Screen))
        })
    }

    /// Where a voice channel lives.
    pub fn voice_location(&self) -> Option<(&Server, &Channel)> {
        let vc = self.voice.channel_id()?;
        self.servers
            .iter()
            .find_map(|s| s.channel(vc).map(|c| (s, c)))
    }

    pub fn find_msg(&self, id: i64) -> Option<&Message> {
        let cid = self.view.channel()?;
        self.msgs.get(&cid)?.list.iter().find(|m| m.id == id)
    }

    pub fn pending_friends(&self) -> usize {
        self.friends
            .iter()
            .filter(|f| f.state == FriendState::Incoming)
            .count()
    }

    // ------------------------------------------------------------ unread

    pub fn total_hot(&self) -> u32 {
        self.unread.values().map(|u| u.hot).sum()
    }

    fn set_badge(&mut self, n: u32) {
        if n != self.badge {
            self.badge = n;
            crate::tray::set_badge(n);
        }
    }

    pub fn mark_read(&mut self, cid: i64) {
        if self.unread.remove(&cid).is_some() {
            self.set_badge(self.total_hot());
        }
    }

    /// Unread across a server: the hot count, and whether there is any.
    pub fn server_unread(&self, s: &Server) -> (u32, bool) {
        let mut n = 0;
        let mut any = false;
        for c in &s.channels {
            if let Some(u) = self.unread.get(&c.id) {
                any = true;
                n += u.hot;
            }
        }
        (n, any)
    }

    // ------------------------------------------------------------ navigation

    pub fn valid_view(&self, v: View) -> bool {
        match v {
            View::None => false,
            View::Server { server, channel } => self
                .server(server)
                .is_some_and(|s| channel.is_some_and(|c| s.channel(c).is_some())),
            View::Home { channel } => channel.is_none_or(|c| self.dms.iter().any(|d| d.id == c)),
        }
    }

    fn default_channel(&self, s: &Server) -> Option<i64> {
        let last = self.prefs.last.get(&s.id.to_string()).copied();
        s.channels
            .iter()
            .find(|c| Some(c.id) == last)
            .or_else(|| s.channels.iter().find(|c| c.kind == ChannelKind::Text))
            .or_else(|| s.channels.first())
            .map(|c| c.id)
    }

    pub fn go_to(&mut self, mut v: View) {
        if let View::Server {
            server,
            channel: None,
        } = v
        {
            let Some(s) = self.server(server) else { return };
            v = View::Server {
                server,
                channel: self.default_channel(s),
            };
        }
        self.view = v;
        self.reply = None;
        self.editing = None;
        self.files.clear();
        self.pop = None;
        self.prefs.view = Some(v);
        if let View::Server {
            server,
            channel: Some(c),
        } = v
        {
            self.prefs.last.insert(server.to_string(), c);
        }
        self.save_prefs();
        let text_channel = match v {
            View::Home { channel: Some(c) } => Some(c),
            View::Server { .. } => self
                .current_channel()
                .filter(|(_, c)| !c.is_voice())
                .map(|(_, c)| c.id),
            _ => None,
        };
        if let Some(cid) = text_channel {
            self.unread.remove(&cid);
            self.set_badge(self.total_hot());
            if !self.msgs.contains_key(&cid) {
                self.load_messages(cid, None);
            }
        }
        self.stick_bottom = true;
        self.focus_composer = true;
    }

    pub fn load_messages(&mut self, cid: i64, before: Option<i64>) {
        let st = self.msgs.entry(cid).or_default();
        if st.loading {
            return;
        }
        st.loading = true;
        let path = match before {
            Some(b) => format!("/channels/{cid}/messages?before={b}"),
            None => format!("/channels/{cid}/messages"),
        };
        self.call(
            Req::get(path),
            Then::Messages {
                cid,
                older: before.is_some(),
            },
            ErrTo::Toast,
        );
    }

    pub fn load_older(&mut self) {
        let Some(cid) = self.view.channel() else {
            return;
        };
        let first = self
            .msgs
            .get(&cid)
            .and_then(|st| st.list.iter().find(|m| !m.pending))
            .map(|m| m.id);
        if let Some(first) = first {
            self.stick_bottom = false;
            self.load_messages(cid, Some(first));
        }
    }

    /// Goes to a server just created or joined; its SERVER_UPDATE may not
    /// have arrived yet.
    fn arrive(&mut self, sid: i64) {
        if self.server(sid).is_some() {
            self.go_to(View::Server {
                server: sid,
                channel: None,
            });
        } else {
            self.pending_go = Some(sid);
        }
    }

    /// Picking a channel in the list; a voice channel also joins the call.
    pub fn open_channel(&mut self, sid: i64, cid: i64) {
        self.go_to(View::Server {
            server: sid,
            channel: Some(cid),
        });
        let join = self
            .server(sid)
            .and_then(|s| s.channel(cid))
            .is_some_and(|c| c.is_voice() && c.can(P_CONNECT));
        if join {
            self.join_voice(cid);
        }
    }

    pub fn save_prefs(&mut self) {
        self.prefs.sounds_off = self.sounds.off_list();
        self.prefs.save(&self.shell.path("prefs.json"));
    }

    /// "Esconder meu IP": the connections made from now on go only through
    /// the relay (voice/rtc.rs).
    pub fn set_hide_ip(&mut self, on: bool) {
        self.prefs.hide_ip = on;
        crate::voice::rtc::set_hide_ip(on);
        self.save_prefs();
    }

    /// "Mostrar quando estou digitando".
    pub fn set_send_typing(&mut self, on: bool) {
        self.prefs.no_typing = !on;
        self.save_prefs();
    }

    /// The language chosen in Configurações. egui is immediate mode, so
    /// the whole interface redraws in the new language on the next frame;
    /// nothing has to restart.
    pub fn set_language(&mut self, lang: crate::lang::Lang) {
        crate::lang::set(lang);
        self.prefs.lang = Some(lang);
        self.save_prefs();
        self.ctx.request_repaint();
    }

    /// "Conectar pelo Tor" (tor.rs): chosen at the start, so the app opens
    /// again for it, once the person agrees.
    pub fn set_tor(&mut self, on: bool) {
        self.prefs.tor = on;
        self.save_prefs();
        if on == crate::tor::active() {
            return;
        }
        let text = if on {
            crate::t!("tor.restart_on")
        } else {
            crate::t!("tor.restart_off")
        };
        self.confirm(
            if on {
                crate::t!("settings.tor")
            } else {
                crate::t!("auth.without_tor")
            },
            text,
            crate::t!("common.reopen_now"),
            Action::Restart,
        );
    }

    /// Opens the app again (a setting read at the start): the new copy
    /// waits for this one to close (main.rs `RESTART_ARG`).
    pub fn restart(&mut self) {
        let started = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .arg(format!("{}{}", crate::RESTART_ARG, std::process::id()))
                .spawn()
        });
        match started {
            Ok(_) => self.quit(),
            Err(e) => self.toast(crate::tf!("core.restart_failed", e = e)),
        }
    }

    // ------------------------------------------------------------ dialogs

    pub fn open_modal(&mut self, m: Modal) {
        self.pop = None;
        self.modal = Some(m);
        self.modal_focus = true;
    }

    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    /// The picker of screens and windows. Their pictures are taken on a
    /// thread of their own and arrive one by one.
    pub fn open_music(&mut self) {
        self.open_modal(Modal::Music {
            link: String::new(),
        });
        self.modal_focus = true;
    }

    pub fn open_share(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let own = self.shell.hwnd();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("share-picker".into())
            .spawn(move || {
                crate::voice::sources::list(own, |choice| {
                    let _ = tx.send(choice);
                    ctx.request_repaint();
                });
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.voice.set_video(Some(Video::Screen));
            return;
        }
        self.open_modal(Modal::Share {
            tab: ShareTab::Screens,
            items: Vec::new(),
            incoming: Some(rx),
        });
    }

    /// Presents the file at `path` in the call, and opens the call's view,
    /// where the presenter's view is.
    pub fn present(&mut self, path: PathBuf) {
        self.close_modal();
        self.voice.present(path);
        if let Some((sid, cid)) = self.voice_location().map(|(s, c)| (s.id, c.id)) {
            self.go_to(View::Server {
                server: sid,
                channel: Some(cid),
            });
        }
    }

    /// Shares what was picked, and closes the picker.
    pub fn share(&mut self, source: crate::voice::sources::Source) {
        self.close_modal();
        self.voice.share(Some(source));
    }

    /// "Salvar imagem" on the annotations' toolbar: a picture of what is
    /// annotated, as it shows at `rect` with its strokes (Zoom's "Save",
    /// Teams' snapshot). The next frame is drawn without the toolbars and
    /// taken, cut to `rect`, and saved where the person picks, as PNG.
    pub fn save_annotation(&mut self, rect: egui::Rect) {
        let ink = self.voice.ink().clone();
        ink.set_hide_tools(true);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let ppp = self.ctx.pixels_per_point();
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::ScreenshotCallback::new(move |image| {
                    ink.set_hide_tools(false);
                    ctx.request_repaint();
                    let cut = image.region(&rect, Some(ppp));
                    std::thread::spawn(move || {
                        let text = match save_png(&cut) {
                            Ok(Some(path)) => format!(
                                "Imagem salva: {}",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ),
                            Ok(None) => return,
                            Err(e) => crate::tf!("core.save_image_failed", e = e),
                        };
                        let _ = tx.send(Msg::Toast(text));
                    });
                }),
            ));
        self.ctx.request_repaint();
    }

    /// Closes a dialog about something that no longer exists, as the old
    /// page did when a dialog had nothing left to show.
    fn check_modal(&mut self) {
        let gone = match &self.modal {
            Some(Modal::EditChannel { cid, .. }) => self
                .current_server()
                .is_none_or(|s| s.channel(*cid).is_none()),
            Some(Modal::ServerSettings { sid, .. })
            | Some(Modal::Reports { sid })
            | Some(Modal::Requests { sid })
            | Some(Modal::NewChannel { sid, .. }) => self.server(*sid).is_none(),
            _ => false,
        };
        if gone {
            self.modal = None;
        }
    }

    pub fn confirm(&mut self, title: &str, text: &str, label: &str, action: Action) {
        self.open_modal(Modal::Confirm {
            title: title.into(),
            text: text.into(),
            label: label.into(),
            action,
        });
    }

    pub fn confirmed(&mut self, action: Action) {
        self.modal = None;
        match action {
            Action::DeleteMessage(mid) => self.call(
                Req::delete(format!("/messages/{mid}")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::DeleteReported { sid, mid } => self.call(
                Req::delete(format!("/messages/{mid}")),
                Then::ReopenReports(sid),
                ErrTo::Toast,
            ),
            Action::BanAuthor { sid, uid } => self.call(
                Req::put(format!("/servers/{sid}/bans/{uid}")),
                Then::ReopenReports(sid),
                ErrTo::Toast,
            ),
            Action::Block(uid) => self.call(
                Req::put(format!("/blocks/{uid}")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::DeleteChannel(cid) => self.call(
                Req::delete(format!("/channels/{cid}")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::DeleteServer(sid) => self.call(
                Req::delete(format!("/servers/{sid}")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::LeaveServer(sid) => self.call(
                Req::post(format!("/servers/{sid}/leave")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::Kick { sid, uid } => self.call(
                Req::delete(format!("/servers/{sid}/members/{uid}")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::Ban { sid, uid } => self.call(
                Req::put(format!("/servers/{sid}/bans/{uid}")),
                Then::Nothing,
                ErrTo::Toast,
            ),
            Action::DeleteAccount => {
                self.call(Req::delete("/me"), Then::AccountDeleted, ErrTo::Toast);
            }
            Action::StopSharing(id) => self.direct.stop_sharing(&format!("{id:032x}")),
            Action::CancelDownload(id) => self.direct.cancel(&format!("{id:032x}")),
            Action::Restart => self.restart(),
        }
    }

    pub fn open_settings(&mut self) {
        let display_name = self
            .me
            .as_ref()
            .map(|u| u.display_name.clone())
            .unwrap_or_default();
        self.open_modal(Modal::Settings { display_name });
        self.list_audio_devices();
    }

    /// Asks Windows which microphones and outputs there are (for the
    /// settings), away from the window's thread.
    fn list_audio_devices(&mut self) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let _ = std::thread::Builder::new()
            .name("dispositivos".into())
            .spawn(move || {
                let _ = tx.send(Msg::AudioDevices(crate::devices::list()));
                ctx.request_repaint();
            });
    }

    /// The microphone chosen in the settings (None: Windows' default).
    pub fn input_device(&self) -> Option<&crate::devices::Device> {
        self.prefs.input_device.as_ref()
    }

    pub fn output_device(&self) -> Option<&crate::devices::Device> {
        self.prefs.output_device.as_ref()
    }

    /// Uses `device` (None: Windows' default) as the microphone from now
    /// on, in a call under way too.
    pub fn set_input_device(&mut self, device: Option<crate::devices::Device>) {
        crate::devices::set_input(device.as_ref().map(|d| d.id.clone()));
        self.prefs.input_device = device;
        self.save_prefs();
        self.voice.devices_changed();
    }

    /// Uses `device` (None: Windows' default) for everything the app plays.
    pub fn set_output_device(&mut self, device: Option<crate::devices::Device>) {
        crate::devices::set_output(device.as_ref().map(|d| d.id.clone()));
        self.prefs.output_device = device;
        self.save_prefs();
        self.voice.devices_changed();
        self.sounds.output_changed();
    }

    /// The noise suppression chosen in the settings.
    pub fn noise(&self) -> crate::voice::Noise {
        self.voice.noise()
    }

    /// Takes `noise` from now on, in a call under way too.
    pub fn set_noise(&mut self, noise: crate::voice::Noise) {
        self.prefs.noise = Some(noise);
        self.save_prefs();
        self.voice.set_noise(noise);
    }

    pub fn open_server_settings(&mut self, sid: i64) {
        let Some(s) = self.server(sid) else { return };
        let name = s.name.clone();
        self.open_modal(Modal::ServerSettings {
            sid,
            name,
            new_role: String::new(),
            role: None,
        });
    }

    pub fn open_edit_channel(&mut self, cid: i64) {
        let Some(c) = self.current_server().and_then(|s| s.channel(cid)) else {
            return;
        };
        let m = Modal::EditChannel {
            cid,
            name: c.name.clone().unwrap_or_default(),
            topic: c.topic.clone().unwrap_or_default(),
            category: c.category.clone().unwrap_or_default(),
        };
        self.open_modal(m);
    }

    pub fn invite(&mut self) {
        self.pop = None;
        let Some(sid) = self.view.server() else {
            return;
        };
        // Signed with a key whose secret only the link carries
        // (e2e/members.rs); without keys yet, a plain one.
        match self.server(sid).and_then(|s| self.e2e.new_invite(s)) {
            Some(inv) => {
                let link = crate::e2e::members::link(&inv.code, &inv.secret);
                self.call(
                    Req::post(format!("/servers/{sid}/invites"))
                        .with(json!({ "code": inv.code, "cert": inv.cert })),
                    Then::InviteLink(link),
                    ErrTo::Toast,
                );
            }
            None => self.call(
                Req::post(format!("/servers/{sid}/invites")),
                Then::Invite,
                ErrTo::Toast,
            ),
        }
    }

    pub fn open_dm(&mut self, uid: i64) {
        self.call(
            Req::post("/dms").with(json!({ "userId": uid })),
            Then::OpenDm,
            ErrTo::Toast,
        );
    }

    // ------------------------------------------------------------ forms

    pub fn add_friend(&mut self) {
        let name = self.add_friend.trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.call(
            Req::post("/friends").with(json!({ "username": name })),
            Then::FriendRequested { close_modal: false },
            ErrTo::Form(FormId::AddFriend),
        );
    }

    /// Saves the role being edited. Permissions I cannot grant keep the bits
    /// they had, as the disabled checkboxes did.
    pub fn save_role(&mut self) {
        let Some(Modal::ServerSettings {
            sid,
            role: Some(editor),
            ..
        }) = &self.modal
        else {
            return;
        };
        let Some(s) = self.server(*sid) else { return };
        let Some(role) = s.role(editor.id) else {
            return;
        };
        let mut perms = role.perms & !s.perms;
        for (bit, _, _) in perm_labels() {
            if editor.perms & bit != 0 && s.perms & bit != 0 {
                perms |= bit;
            }
        }
        let [r, g, b] = editor.color;
        let mut body = json!({ "perms": perms });
        if !role.is_everyone() {
            body["name"] = json!(editor.name.trim());
            body["color"] = json!(format!("#{r:02x}{g:02x}{b:02x}"));
        }
        self.call(
            Req::patch(format!("/roles/{}", editor.id)).with(body),
            Then::Toast("Cargo salvo.".into()),
            ErrTo::Form(FormId::SaveRole),
        );
    }

    pub fn set_status(&mut self, status: Status) {
        self.send_op(&op::status(status.wire()));
    }

    // ------------------------------------------------------------ the composer

    pub fn draft(&mut self, cid: i64) -> &mut String {
        self.drafts.entry(cid).or_default()
    }

    /// Something was typed in the composer: tells the others, at most every
    /// few seconds.
    pub fn typed(&mut self, cid: i64) {
        if self.prefs.no_typing {
            return;
        }
        let has_text = self.drafts.get(&cid).is_some_and(|d| !d.is_empty());
        let due = self
            .last_typing
            .is_none_or(|at| at.elapsed() > TYPING_EVERY);
        if has_text && due {
            self.last_typing = Some(Instant::now());
            self.send_op(&op::typing(cid));
        }
    }

    /// Stickers move only while the window is in front: each frame of
    /// theirs draws the whole window again.
    pub fn stickers_move(&self) -> bool {
        self.focused || self.keep_moving
    }

    pub fn may_attach_here(&self) -> bool {
        if self.current_dm().is_some() {
            return true;
        }
        self.current_channel()
            .is_some_and(|(_, c)| !c.is_voice() && c.can(P_ATTACH))
    }

    pub fn add_files(&mut self, paths: Vec<PathBuf>) {
        if !self.may_attach_here() {
            return;
        }
        for path in paths {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            if self.files.len() >= net::MAX_FILES {
                self.toast(crate::tf!("core.max_files", n = net::MAX_FILES));
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "arquivo".into());
            self.files.push(PendingFile {
                path,
                name,
                size: meta.len(),
                uploaded: None,
                direct: false,
                key: None,
            });
        }
        self.focus_composer = true;
    }

    /// The X on a send's bar: its upload stops, and the message comes back
    /// to the composer.
    pub fn cancel_send(&mut self, id: u64) {
        if let Some(s) = self.sending.iter().find(|s| s.id == id) {
            s.progress.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Opens the file picker on its own thread (the picker needs one that
    /// is not running anything else) and reports back what was chosen.
    pub fn pick_files(&self, pick: Pick) {
        if pick == Pick::Slides {
            // Found while the picker is open (slides/fonts.rs).
            crate::slides::fonts::Fonts::warm();
        }
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new();
            let picked = match pick {
                Pick::Attach => dialog.set_title("Anexar arquivos").pick_files(),
                Pick::Avatar => dialog
                    .set_title("Escolher foto")
                    .add_filter("Imagens", &["png", "jpg", "jpeg", "gif", "webp"])
                    .pick_file()
                    .map(|p| vec![p]),
                Pick::ServerIcon(_) => dialog
                    .set_title("Escolher a imagem do servidor")
                    .add_filter("Imagens", &["png", "jpg", "jpeg", "gif", "webp"])
                    .pick_file()
                    .map(|p| vec![p]),
                Pick::Slides => dialog
                    .set_title("Apresentar um arquivo")
                    .add_filter("PowerPoint ou PDF", &["pptx", "ppsx", "pdf"])
                    .pick_file()
                    .map(|p| vec![p]),
            };
            if let Some(paths) = picked {
                let _ = tx.send(Msg::Picked(pick, paths));
                ctx.request_repaint();
            }
        });
    }

    /// Ctrl+V in the composer: copied files, or a copied picture (a
    /// screenshot, say), which becomes a PNG.
    pub fn paste_files(&mut self) -> bool {
        let Ok(mut clip) = arboard::Clipboard::new() else {
            return false;
        };
        if let Ok(list) = clip.get().file_list()
            && !list.is_empty()
        {
            self.add_files(list);
            return true;
        }
        let Ok(img) = clip.get_image() else {
            return false;
        };
        let Some(buffer) =
            image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())
        else {
            return false;
        };
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = std::env::temp_dir().join(format!("imagem-{stamp}.png"));
        if buffer.save(&path).is_ok() {
            self.add_files(vec![path]);
            return true;
        }
        false
    }

    pub fn send_message(&mut self) {
        let Some(cid) = self.view.channel() else {
            return;
        };
        let content = self.drafts.get(&cid).map_or("", |d| d.trim()).to_owned();
        if content.is_empty() && self.files.is_empty() {
            return;
        }
        if content.encode_utf16().count() > net::MAX_MESSAGE {
            self.toast(crate::t!("core.message_too_long"));
            return;
        }
        let mut files = std::mem::take(&mut self.files);
        // "Enviar direto" goes for all of them; one prepared the other way
        // in a send that failed is made again.
        let direct = std::mem::take(&mut self.direct_send);
        for f in &mut files {
            if f.direct != direct {
                f.direct = direct;
                f.uploaded = None;
            }
        }
        let replying = self.reply.take();
        self.drafts.insert(cid, String::new());
        self.stick_bottom = true;
        let out = self.outgoing(cid, content, None, replying.as_ref());
        if !files.is_empty() {
            // Files keep their progress bar instead of a copy.
            self.post_message(out, files);
            return;
        }
        self.show_pending(&out, replying);
        self.dispatch(out);
    }

    /// A GIPHY sticker from the picker, as a message of its own (answering
    /// the message being replied to, if any).
    pub fn send_sticker(&mut self, id: &str) {
        self.pop = None;
        let Some(cid) = self.view.channel() else {
            return;
        };
        if !giphy::valid_id(id) {
            return;
        }
        self.giphy.chosen(id);
        let replying = self.reply.take();
        self.stick_bottom = true;
        self.focus_composer = true;
        let out = self.outgoing(cid, String::new(), Some(id.to_owned()), replying.as_ref());
        self.show_pending(&out, replying);
        self.dispatch(out);
    }

    /// Numbers a new message and marks it, so a second try can be told
    /// from a new one.
    fn outgoing(
        &mut self,
        cid: i64,
        content: String,
        sticker: Option<String>,
        replying: Option<&Message>,
    ) -> Outgoing {
        let send = self.next_send;
        self.next_send += 1;
        Outgoing {
            send,
            cid,
            quote: replying.map(Core::quote_of),
            nonce: new_nonce(send),
            content,
            sticker,
            env: None,
            files: Vec::new(),
        }
    }

    /// Text (or a sticker) shows up at once, faded until the server has it
    /// (a round trip to the object, more than 100 ms from Brazil).
    fn show_pending(&mut self, out: &Outgoing, replying: Option<Message>) {
        let me = self.me.clone();
        let Some(st) = self.msgs.get_mut(&out.cid) else {
            return;
        };
        if st.loading {
            return;
        }
        st.list.push(Message {
            id: temp_id(out.send),
            channel_id: out.cid,
            author: me,
            content: out.content.clone(),
            sticker: out.sticker.clone(),
            created: chrono::Utc::now().timestamp_millis(),
            reply: replying.map(|r| ReplyRef {
                author_id: r.author_id(),
                author: r.author,
                content: r.content,
                sticker: r.sticker,
            }),
            nonce: Some(out.nonce.clone()),
            pending: true,
            ..Message::default()
        });
        if let Some(sent) = crate::probe::message_stamp(&out.content) {
            self.arrived_stamps.push(("msg.local", sent));
        }
    }

    /// Through the gateway when it is up: it reaches the object without
    /// the hop through the Worker that a POST makes (125 ms instead of 144
    /// to the others, measured from Brazil). The server's copy is the
    /// answer; without it, HTTP takes over (`resend_unconfirmed`).
    fn dispatch(&mut self, out: Outgoing) {
        // Sealed first (sealing.rs), then here again.
        let Some(env) = &out.env else {
            self.seal_out(out);
            return;
        };
        let op = op::message(out.cid, env, &out.files, &out.nonce);
        if !self.reconnecting
            && op.to_string().len() <= GW_MESSAGE_MAX
            && (crate::probe::lose("envio") || self.gw.send(&op))
        {
            self.outbox.push(Unconfirmed {
                out,
                at: Instant::now(),
            });
            self.ctx.request_repaint_after(GW_PATIENCE);
            return;
        }
        self.post_message(out, Vec::new());
    }

    /// Opens the sticker picker by `anchor` (the composer's button), or
    /// closes it when it is open. It starts on the favourites, when there
    /// are any.
    pub fn open_stickers(&mut self, anchor: egui::Rect) {
        if matches!(
            self.pop,
            Some(Pop {
                kind: PopKind::Sticker,
                ..
            })
        ) {
            self.pop = None;
            return;
        }
        self.sticker_query.clear();
        self.sticker_typed = None;
        self.sticker_focus = true;
        self.sticker_tab = if self.stickers.is_empty() {
            StickerTab::Trending
        } else {
            StickerTab::Favorites
        };
        self.giphy.opened();
        self.pop = Some(Pop {
            kind: PopKind::Sticker,
            anchor,
        });
    }

    /// Opens the emoji picker, for the composer (`PopKind::Emoji`) or for
    /// a reaction (`PopKind::React`), or closes it when it is the one open.
    /// The list is fetched if it is not here yet.
    pub fn open_emoji(&mut self, kind: PopKind, anchor: egui::Rect) {
        if self.pop.is_some_and(|p| p.kind == kind) {
            self.pop = None;
            return;
        }
        self.emoji_query.clear();
        self.emoji_jump = None;
        self.emoji_tones_open = false;
        self.emoji_pairs = None;
        self.emoji_focus = true;
        self.emoji.want();
        self.pop = Some(Pop { kind, anchor });
    }

    /// An emoji picked goes first among the recent ones.
    pub fn used_emoji(&mut self, emoji: &str) {
        self.emoji_pairs = None;
        let recent = &mut self.prefs.emoji_recent;
        recent.retain(|e| e != emoji);
        recent.insert(0, emoji.to_owned());
        recent.truncate(crate::emoji::RECENT);
        self.save_prefs();
    }

    /// The skin tone the picker gives: 1 (light) to 5 (dark), 0 for none.
    pub fn set_emoji_tone(&mut self, tone: u8) {
        self.prefs.emoji_tone = tone.min(5);
        self.emoji_tones_open = false;
        self.save_prefs();
    }

    /// Keeps a sticker among my favourites, or lets it go: here at once,
    /// then on the server, which tells my other devices.
    pub fn toggle_favorite(&mut self, id: &str) {
        if !giphy::valid_id(id) {
            return;
        }
        let before = self.stickers.clone();
        let path = format!("/me/stickers/{id}");
        let req = if let Some(i) = self.stickers.iter().position(|s| s == id) {
            self.stickers.remove(i);
            Req::delete(path)
        } else {
            if self.stickers.len() >= giphy::MAX_IDS {
                self.toast(crate::tf!("core.max_favorites", n = giphy::MAX_IDS));
                return;
            }
            self.stickers.insert(0, id.to_owned());
            Req::put(path)
        };
        self.call(req, Then::Stickers(before), ErrTo::Toast);
    }

    /// Messages the gateway took that the server has not sent back go
    /// again over HTTP, with the same mark, so the server can tell if the
    /// first try did arrive: after `GW_PATIENCE`, or all of them at once
    /// when the connection fell (`all`).
    fn resend_unconfirmed(&mut self, all: bool) {
        if self.outbox.is_empty() {
            return;
        }
        let now = Instant::now();
        let (late, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.outbox)
            .into_iter()
            .partition(|u| all || now.duration_since(u.at) >= GW_PATIENCE);
        self.outbox = waiting;
        for u in late {
            self.post_message(u.out, Vec::new());
        }
        if let Some(next) = self.outbox.iter().map(|u| u.at + GW_PATIENCE).min() {
            self.ctx
                .request_repaint_after(next.saturating_duration_since(now));
        }
    }

    /// A message over HTTP: its files go up first (those sent directly are
    /// prepared instead: direct/), then the message.
    fn post_message(&mut self, out: Outgoing, mut files: Vec<PendingFile>) {
        let api = self.api.clone();
        let direct = self.direct.clone();
        let e2e = self.e2e.clone();
        let only = self.known_readers(out.cid);
        let ttl = self.ttl_of(out.cid);
        let progress = net::Progress::new(
            files
                .iter()
                .filter(|f| f.uploaded.is_none())
                .map(|f| f.size)
                .sum(),
        );
        if !files.is_empty() {
            self.sending.push(Sending {
                id: out.send,
                cid: out.cid,
                files: files.len(),
                progress: progress.clone(),
                direct: files.iter().any(|f| f.direct),
            });
        }
        let Outgoing {
            send: id,
            cid,
            content,
            sticker,
            quote,
            nonce,
            env,
            files: refs,
        } = out;
        self.spawn(async move {
            let uploading = Instant::now();
            let fresh = files.iter().any(|f| f.uploaded.is_none());
            let mut attachments = Vec::new();
            for f in &mut files {
                if f.uploaded.is_none() {
                    // To the server, encrypted here first (e2e/file.rs); a
                    // file sent directly only gets prepared.
                    let done = if f.direct {
                        direct
                            .prepare(&f.path, &f.name, &progress)
                            .await
                            .map(|a| (a, None))
                            .map_err(net::ApiError::msg)
                    } else {
                        api.upload_sealed(&f.path, &progress)
                            .await
                            .map(|(a, k)| (a, Some(k)))
                    };
                    match done {
                        Ok((a, key)) => {
                            f.uploaded = Some(a);
                            f.key = key;
                        }
                        Err(e) => {
                            return Msg::SendFailed {
                                id,
                                cid,
                                content,
                                files,
                                error: e.message,
                            };
                        }
                    }
                }
                if let Some(mut a) = f.uploaded.clone() {
                    a.name.clone_from(&f.name);
                    a.size = i64::try_from(f.size).unwrap_or(i64::MAX);
                    attachments.push((a, f.key.clone()));
                }
            }
            if fresh {
                crate::probe::since("arquivo.envio", uploading);
            }
            let shared: Vec<String> = attachments
                .iter()
                .filter_map(|(a, _): &(Attachment, _)| a.direct.as_ref().map(|d| d.id.clone()))
                .collect();
            // The envelope: the one made already (a second try), or one
            // with the files in it.
            let (env, refs) = match env {
                Some(env) if attachments.is_empty() => (env, refs),
                _ => {
                    let (list, server) = Core::file_refs(&attachments);
                    let body = crate::e2e::msg::Body {
                        ts: chrono::Utc::now().timestamp_millis(),
                        t: content.clone(),
                        st: sticker.clone(),
                        r: quote.clone(),
                        a: list,
                        ed: None,
                        h: None,
                        x: None,
                    };
                    match e2e.seal_body(cid, &body, only.as_ref(), ttl).await {
                        Ok((env, _)) => (env, server),
                        Err(error) => {
                            return Msg::SendFailed {
                                id,
                                cid,
                                content,
                                files,
                                error,
                            };
                        }
                    }
                }
            };
            let body = json!({ "e2e": env, "files": refs, "nonce": nonce });
            let posted = Instant::now();
            let res = api
                .call(&Req::post(format!("/channels/{cid}/messages")).with(body))
                .await;
            crate::probe::since("msg.http", posted);
            match res {
                // The message as the server keeps it: it takes the place
                // of the copy shown, should the gateway not have brought it.
                // Its files sent directly are shared from now on.
                Ok(m) => {
                    if let Some(mid) = m.get("id").and_then(Value::as_i64)
                        && !shared.is_empty()
                    {
                        direct.posted(&shared, mid);
                    }
                    Msg::Done {
                        then: Then::Sent(id),
                        err: ErrTo::Toast,
                        res: Ok(m),
                    }
                }
                Err(e) if e.expired => Msg::Done {
                    then: Then::Sent(id),
                    err: ErrTo::Toast,
                    res: Err(e),
                },
                Err(e) => Msg::SendFailed {
                    id,
                    cid,
                    content,
                    files,
                    error: e.message,
                },
            }
        });
    }

    pub fn start_edit(&mut self, id: i64) {
        let text = self
            .find_msg(id)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.editing = Some(Editing {
            id,
            text,
            focus: true,
        });
    }

    /// Enter in the edit box: saves if something changed.
    pub fn finish_edit(&mut self) {
        let Some(e) = self.editing.take() else { return };
        let content = e.text.trim().to_owned();
        let same = self.find_msg(e.id).is_some_and(|m| m.content == content);
        if content.is_empty() || same {
            return;
        }
        self.seal_edit(e.id, content);
    }

    pub fn react(&mut self, mid: i64, emoji: &str) {
        self.pop = None;
        let me = self.me_id();
        let mine = self.find_msg(mid).is_some_and(|m| m.reacted(emoji, me));
        // An encrypted message's reaction is encrypted too (sealing.rs).
        if let Some(m) = self.find_msg(mid).filter(|m| m.e2e.is_some()).cloned() {
            self.react_sealed(&m, emoji, mine);
            return;
        }
        let path = format!("/messages/{mid}/reactions/{}", net::escape(emoji));
        let req = if mine {
            Req::delete(path)
        } else {
            Req::put(path)
        };
        self.call(req, Then::Nothing, ErrTo::Toast);
    }

    // ------------------------------------------------------------ voice

    pub fn join_voice(&mut self, cid: i64) {
        if let Some(me) = self.me.as_ref().map(|u| u.id) {
            self.voice.join(cid, me);
        }
    }

    pub fn toggle_mute(&mut self) {
        if let Err(text) = self.voice.toggle_mute() {
            self.toast(text);
        }
    }

    pub fn set_quality(&mut self, key: &str) {
        self.prefs.quality = Some(key.to_owned());
        self.save_prefs();
        self.voice.set_quality(crate::voice::Quality::by_key(key));
    }

    /// How loud `uid` plays here.
    pub fn person_audio(&self, uid: i64) -> PersonAudio {
        self.prefs
            .people
            .get(&uid.to_string())
            .copied()
            .unwrap_or_default()
    }

    /// Sets how loud `uid` plays here, at once. `keep` writes it down too
    /// (not on every step of a slider being dragged).
    pub fn set_person_audio(&mut self, uid: i64, audio: PersonAudio, keep: bool) {
        if audio == PersonAudio::default() {
            self.prefs.people.remove(&uid.to_string());
        } else {
            self.prefs.people.insert(uid.to_string(), audio);
        }
        self.voice.set_gains(uid, audio.gains());
        if keep {
            self.save_prefs();
        }
    }

    /// Whether the PC's sound goes with a screen share, from now on.
    pub fn set_share_audio(&mut self, on: bool) {
        self.prefs.share_audio = Some(on);
        self.save_prefs();
        self.voice.set_share_audio(on);
    }

    pub fn quality_key(&self) -> &str {
        self.prefs.quality.as_deref().unwrap_or("1080p30")
    }

    // ------------------------------------------------------------ updates

    /// The start's part of the updates (update.rs): an update already
    /// downloaded but held back gets the banner; the installed copy asks
    /// the server before going on (the gate), any other copy a moment
    /// later; and from then on, every few hours.
    fn start_updates(&mut self) {
        if let Some(p) = self.shell.ready_update() {
            self.update_ready = Some(p.version);
        }
        if crate::update::automatic() {
            self.gate = Gate::Checking(Instant::now() + GATE_WAIT);
            self.ctx.request_repaint_after(GATE_WAIT);
            self.check_update(Check::Startup);
        } else {
            self.check_update(Check::Later);
        }
        let every = std::env::var("BABUTELA_TESTE_ATUALIZAR_A_CADA")
            .ok()
            .and_then(|s| s.parse().ok())
            .map_or(crate::update::CHECK_EVERY, Duration::from_secs);
        let (shell, http, base) = (
            self.shell.clone(),
            self.api.http().clone(),
            self.api.base().to_owned(),
        );
        let (tx, ctx, e2e) = (self.tx.clone(), self.ctx.clone(), self.e2e.clone());
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let progress = crate::net::Progress::new(0);
                match crate::update::fetch(&shell, &http, &base, &progress, |_| {}, &e2e).await {
                    Ok(crate::update::Outcome::Ready { version, tries }) => {
                        let news = UpdateNews::Ready {
                            version,
                            tries,
                            asked: false,
                        };
                        let _ = tx.send(Msg::Update(news));
                        ctx.request_repaint();
                    }
                    Ok(_) => {}
                    Err(e) => crate::logln!("atualização: {e}"),
                }
            }
        });
    }

    /// Looks for a newer version, and downloads it.
    pub fn check_update(&mut self, how: Check) {
        let shell = self.shell.clone();
        let http = self.api.http().clone();
        let base = self.api.base().to_owned();
        let progress = crate::net::Progress::new(0);
        let found = {
            let (tx, ctx, progress) = (self.tx.clone(), self.ctx.clone(), progress.clone());
            move |version: &str| {
                let news = UpdateNews::Found {
                    version: version.to_owned(),
                    progress,
                };
                let _ = tx.send(Msg::Update(news));
                ctx.request_repaint();
            }
        };
        let say = move |text: &str| {
            let text = if how == Check::Button { text } else { "" };
            Msg::Update(UpdateNews::Checked(text.to_owned()))
        };
        let e2e = self.e2e.clone();
        self.spawn(async move {
            if how == Check::Later {
                // The first screen first.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            match crate::update::fetch(&shell, &http, &base, &progress, found, &e2e).await {
                Ok(crate::update::Outcome::Ready { version, tries }) => {
                    Msg::Update(UpdateNews::Ready {
                        version,
                        tries,
                        asked: how == Check::Button,
                    })
                }
                Ok(crate::update::Outcome::Busy) => say(crate::t!("core.update_downloading")),
                Ok(crate::update::Outcome::None) => say(crate::t!("core.update_latest")),
                Err(e) => {
                    crate::logln!("atualização: {e}");
                    say(crate::t!("core.update_check_failed"))
                }
            }
        });
    }

    fn update_news(&mut self, news: UpdateNews) {
        match news {
            UpdateNews::Found { version, progress } => {
                // Only the start shows it; later, it comes down quietly.
                if matches!(self.gate, Gate::Checking(_)) {
                    self.gate = Gate::Updating { version, progress };
                }
            }
            UpdateNews::Ready {
                version,
                tries,
                asked,
            } => {
                self.update_ready = Some(version);
                if asked {
                    self.toast(
                        "Atualização pronta: ela entra quando o Babu Tela abrir de novo, ou já, \
                         em “Reiniciar agora”.",
                    );
                }
                if !matches!(self.gate, Gate::Open) {
                    if tries < crate::update::MAX_TRIES && self.install_at_start() {
                        return;
                    }
                    self.open_gate();
                }
            }
            UpdateNews::Checked(text) => {
                self.toast(text);
                if !matches!(self.gate, Gate::Open) {
                    self.open_gate();
                }
            }
            UpdateNews::Gone => {
                self.update_ready = None;
                self.toast("O instalador baixado sumiu. Vou procurar de novo.");
                self.check_update(Check::Button);
            }
        }
    }

    /// Opens the gate when the server takes too long to answer, or the
    /// download it started stops moving. Every frame (pump).
    fn watch_gate(&mut self) {
        let now = Instant::now();
        match &self.gate {
            Gate::Checking(until) if now >= *until => {
                crate::logln!("atualização: o servidor não respondeu a tempo; abrindo sem esperar");
                self.open_gate();
            }
            Gate::Updating { progress, .. } => {
                let sent = progress.sent();
                match self.update_moved {
                    Some((before, since)) if before == sent => {
                        if now.duration_since(since) >= UPDATE_STALL {
                            crate::logln!(
                                "atualização: o download parou; abrindo e deixando para depois"
                            );
                            self.open_gate();
                            return;
                        }
                    }
                    _ => self.update_moved = Some((sent, now)),
                }
                // Hidden in the tray nothing else wakes this up.
                self.ctx.request_repaint_after(Duration::from_secs(1));
            }
            _ => {}
        }
    }

    /// The start goes on (to the sign-in or the chat).
    fn open_gate(&mut self) {
        self.gate = Gate::Open;
        if std::mem::take(&mut self.answered) {
            self.server_answered();
        }
    }

    /// "Abrir agora" on the update screen: the download goes on, and the
    /// update waits for the next start.
    pub fn skip_update(&mut self) {
        if matches!(self.gate, Gate::Updating { .. }) {
            crate::logln!("atualização: aberto sem esperar o download");
            self.open_gate();
        }
    }

    /// The update the start found, installed now: the installer shows its
    /// progress if the window is on the screen, and opens the app again
    /// the way it was. Whether the app is quitting for it.
    fn install_at_start(&mut self) -> bool {
        let dir = crate::update::dir(&self.shell);
        let Some(p) = crate::update::pending(&dir) else {
            return false;
        };
        if let Some(why) = crate::update::hold_back(&p, self.api.base()) {
            crate::logln!("atualização: a {} fica para depois: {why}", p.version);
            return false;
        }
        // The window may not be up yet: it counts as it was asked for.
        let visible = self.shell.hwnd().is_some_and(crate::winsys::is_visible);
        let window = visible || !self.started_hidden;
        if !crate::update::apply(&dir, p, window) {
            return false;
        }
        self.quit();
        true
    }

    /// "Reiniciar agora" on the banner.
    pub fn install_update(&mut self) {
        let dir = crate::update::dir(&self.shell);
        let started =
            crate::update::pending(&dir).is_some_and(|p| crate::update::apply(&dir, p, true));
        if started {
            self.quit();
        } else {
            self.handle(Msg::Update(UpdateNews::Gone));
        }
    }
}

/// "@name" at the start or after whitespace, not followed by another
/// character a name can have — the rule the old page used for mentions.
pub fn mentions(content: &str, username: &str) -> bool {
    if username.is_empty() {
        return false;
    }
    let needle = format!("@{username}");
    let mut from = 0;
    while let Some(i) = content[from..].find(&needle) {
        let start = from + i;
        let end = start + needle.len();
        let before_ok = content[..start]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after_ok = content[end..].chars().next().is_none_or(|c| {
            !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
        });
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_picture_goes_up_square_and_small() {
        let dir = std::env::temp_dir().join(format!("babutela-icone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A wide photo: the middle square, made 256 px.
        let wide = dir.join("largo.jpg");
        image::RgbImage::from_pixel(900, 600, image::Rgb([200, 30, 30]))
            .save(&wide)
            .unwrap();
        let out = square_png(&wide, SERVER_ICON_SIDE).unwrap();
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (256, 256));
        assert_eq!(out.extension().and_then(|e| e.to_str()), Some("png"));
        let _ = std::fs::remove_file(&out);
        // A small one is not blown up.
        let small = dir.join("pequeno.png");
        image::RgbaImage::from_pixel(40, 60, image::Rgba([0, 0, 0, 255]))
            .save(&small)
            .unwrap();
        let out = square_png(&small, SERVER_ICON_SIDE).unwrap();
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (40, 40));
        let _ = std::fs::remove_file(&out);
        // Not a picture at all.
        let text = dir.join("texto.png");
        std::fs::write(&text, b"isto nao e uma imagem").unwrap();
        assert!(square_png(&text, SERVER_ICON_SIDE).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mentions_follow_the_old_rule() {
        assert!(mentions("@ana oi", "ana"));
        assert!(mentions("oi @ana", "ana"));
        assert!(mentions("oi\n@ana!", "ana"));
        assert!(mentions("oi @ana, tudo bem?", "ana"));
        // Part of a longer name, or glued to a word, does not count.
        assert!(!mentions("oi @ana.maria", "ana"));
        assert!(!mentions("oi @ana_b", "ana"));
        assert!(!mentions("oi@ana", "ana"));
        assert!(!mentions("oi ana", "ana"));
        assert!(!mentions("", "ana"));
        // A dot in my own name is a dot, not "any character".
        assert!(mentions("@ana.maria olha", "ana.maria"));
        assert!(!mentions("@anaxmaria olha", "ana.maria"));
        // A first match that fails does not hide a later one.
        assert!(mentions("@ana.x e @ana", "ana"));
    }

    #[test]
    fn views_survive_a_round_trip() {
        for v in [
            View::HOME,
            View::Home { channel: Some(4) },
            View::Server {
                server: 1,
                channel: Some(2),
            },
            View::Server {
                server: 1,
                channel: None,
            },
        ] {
            let text = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<View>(&text).unwrap(), v, "{text}");
        }
        // The shape the old page kept in localStorage reads the same.
        let old: View =
            serde_json::from_str(r#"{"type":"server","serverId":3,"channelId":9}"#).unwrap();
        assert_eq!(
            old,
            View::Server {
                server: 3,
                channel: Some(9)
            }
        );
        let home: View = serde_json::from_str(r#"{"type":"home","channelId":null}"#).unwrap();
        assert_eq!(home, View::HOME);
    }

    #[test]
    fn notifications_say_where_they_go() {
        let dm = View::Home { channel: Some(12) };
        assert_eq!(View::from_launch(&dm.to_launch()), Some(dm));
        let channel = View::Server {
            server: 3,
            channel: Some(9),
        };
        assert_eq!(View::from_launch(&channel.to_launch()), Some(channel));
        // A toast with nothing, or with something else, goes nowhere.
        assert_eq!(View::from_launch(""), None);
        assert_eq!(View::from_launch("action=reply"), None);
        assert_eq!(View::from_launch(&View::None.to_launch()), None);
    }

    #[test]
    fn messages_on_their_way_stay_last_until_they_land() {
        let sent = |id: i64, nonce: &str| Message {
            id,
            nonce: Some(nonce.to_owned()),
            ..Message::default()
        };
        let waiting = |send: u64, nonce: &str| Message {
            pending: true,
            ..sent(temp_id(send), nonce)
        };
        let ids = |list: &[Message]| list.iter().map(|m| m.id).collect::<Vec<_>>();
        let mut list = vec![sent(1, "x"), waiting(0, "a"), waiting(1, "b")];
        // Someone else's, sent out before mine: it goes above them.
        place(&mut list, sent(2, "z"));
        assert_eq!(ids(&list), [1, 2, -1, -2]);
        // The server got the second first: the order is the server's.
        place(&mut list, sent(3, "b"));
        assert_eq!(ids(&list), [1, 2, 3, -1]);
        place(&mut list, sent(4, "a"));
        assert_eq!(ids(&list), [1, 2, 3, 4]);
        // The same one again (the answer to the send, after the gateway).
        place(&mut list, sent(4, "a"));
        assert_eq!(ids(&list), [1, 2, 3, 4]);
        assert!(list.iter().all(|m| !m.pending));
    }
}

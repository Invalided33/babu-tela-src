//! "Enviar direto": a file that never goes to the server. The message only
//! says what it is (its name, its size, how it is cut in pieces and the hash
//! those pieces add up to: hash.rs), and whoever may read the message
//! downloads it straight from the sender's app, over WebRTC, while both are
//! online, the way Keet does it. None of it takes the server's storage; the
//! server only carries the few messages that set each connection up
//! (worker/src/direct.rs).
//!
//! - The sender's app keeps a list of what it shares: where the file is on
//!   this PC, and its pieces' hashes beside the list. It serves whoever asks
//!   (serve.rs), a few at a time, the others waiting their turn, and checks
//!   every piece as it reads it, so a file changed since it was sent stops
//!   instead of going out mixed.
//! - The receiving app keeps a list of its downloads. Each goes into
//!   `<name>.parcial`, next to where it was asked to go, every piece checked
//!   against its hash, and how far it got noted as it goes (fetch.rs): one
//!   that stopped, or an app that closed, carries on from there. It takes its
//!   name once it is whole.
//! - A download whose sender went offline waits for them to come back
//!   (PRESENCE, READY) and carries on by itself; one cut short tries again a
//!   few times first.
//!
//! Both lists belong to this PC (%LOCALAPPDATA%\Prosa\direto), each entry
//! marked with the account it is for.

mod fetch;
pub mod hash;
mod link;
mod serve;
pub mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc, watch};

use crate::model::{Attachment, DirectInfo, IceConfig, Kind};
use crate::net::{Api, Progress, Req};
use crate::shell::Shell;
use hash::Hash;
use wire::Stop;

/// What a send the person stopped says (the uploads say the same).
pub const CANCELLED: &str = "Envio cancelado.";

/// Files going out at once, to everyone and to any one person; whoever
/// comes next waits on the connection they opened (wire::queued).
const SENDS: usize = 6;
const SENDS_PER_PERSON: usize = 2;

/// Connections at once, sending or waiting their turn: past this, "busy".
const CONNECTIONS: usize = 32;

/// A download cut short tries again after this, up to RETRIES times in a
/// row without a piece more; then it waits for its sender to come back.
const RETRY_AFTER: Duration = Duration::from_secs(3);
const RETRIES: u32 = 5;

/// The sender has too many connections: again after this.
const BUSY_AFTER: Duration = Duration::from_secs(30);

/// A file prepared but never on a message (the send failed and was given
/// up) is forgotten after a day.
const UNPOSTED_FOR: i64 = 24 * 3600 * 1000;

/// A file no longer shared is remembered for a month, to say so to whoever
/// still asks for it.
const STOPPED_FOR: i64 = 30 * 24 * 3600 * 1000;

/// The relay's credentials last 12 hours (worker/src/ice.rs): asked again
/// after 6.
const ICE_FOR: Duration = Duration::from_secs(6 * 3600);

/// The lists' file, in `dir`.
const LIST: &str = "direto.json";

/// Where the lists live: %LOCALAPPDATA%\Prosa\direto, since they name places
/// on this PC (nothing for a roaming profile); a test profile
/// (PROSA_USERDATA) has its own. As update::dir does.
pub fn dir(shell: &Shell) -> PathBuf {
    if shell.own_profile()
        && let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(local).join("Prosa").join("direto");
    }
    shell.path("direto")
}

/// How a step of the signalling goes out: to whom, about which file of
/// which message, and what. In the app, the gateway's `direct` op.
pub type Signal = Box<dyn Fn(i64, i64, &str, Value) + Send + Sync>;

/// What the interface hears.
pub enum News {
    /// Something a card shows changed.
    Changed,
    /// A download is whole, at its place.
    Downloaded { name: String, cid: i64 },
    /// One stopped for good, and why (`failure`'s text).
    Stopped {
        name: String,
        from: i64,
        why: String,
    },
}

// ---------------------------------------------------------------- the lists

/// A file of mine, shared from this PC.
#[derive(Clone, Serialize, Deserialize)]
struct Shared {
    id: String,
    /// The account that shared it.
    owner: i64,
    path: PathBuf,
    name: String,
    size: u64,
    piece: u64,
    /// The pieces' root hash (hash::root), in hex: what the message says.
    root: String,
    /// When it was prepared (ms since 1970).
    created: i64,
    /// The messages it went on.
    #[serde(default)]
    msgs: Vec<i64>,
    /// No longer shared, since then: kept a while to say so.
    #[serde(default)]
    stopped: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    /// Should be going: it goes whenever it can.
    Active,
    Paused,
    Done,
    /// Stopped for good (`why`).
    Failed,
}

/// A download of mine.
#[derive(Clone, Serialize, Deserialize)]
struct Download {
    /// The file's id, as its message says.
    id: String,
    /// The account downloading it.
    owner: i64,
    msg: i64,
    /// The conversation the message is in.
    cid: i64,
    /// Who sent it (the message's author).
    from: i64,
    name: String,
    size: u64,
    piece: u64,
    root: String,
    /// Where it goes once whole.
    path: PathBuf,
    /// Pieces checked and surely on the disk.
    verified: u64,
    state: State,
    /// Why it failed: a `Stop` as the wire says it, or "disk:" and what
    /// the disk said.
    #[serde(default)]
    why: Option<String>,
    created: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct List {
    #[serde(default)]
    shared: Vec<Shared>,
    #[serde(default)]
    downloads: Vec<Download>,
}

impl List {
    /// Forgets what is past keeping (see UNPOSTED_FOR and STOPPED_FOR), with
    /// the hashes kept beside it. Whether anything went.
    fn prune(&mut self, dir: &Path, now: i64) -> bool {
        let before = self.shared.len();
        self.shared.retain(|s| {
            let keep = match s.stopped {
                Some(at) => now - at < STOPPED_FOR,
                None => !s.msgs.is_empty() || now - s.created < UNPOSTED_FOR,
            };
            if !keep {
                let _ = std::fs::remove_file(shared_hashes(dir, &s.id));
            }
            keep
        });
        self.shared.len() != before
    }
}

fn shared_hashes(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("envio-{id}.hashes"))
}

fn download_hashes(dir: &Path, owner: i64, id: &str) -> PathBuf {
    dir.join(format!("baixa-{owner}-{id}.hashes"))
}

/// Where a download waits until it is whole: `<its place>.parcial`.
fn partial_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".parcial");
    PathBuf::from(s)
}

// ---------------------------------------------------------------- while the app runs

/// Where a download stands, for its card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Setting the connection up.
    Connecting,
    /// Connected, waiting for its turn: the sender is sending to others.
    Queued,
    Receiving,
    /// Cut short: trying again in a moment.
    Retrying,
    /// Not going until the sender is back (see `Wait`).
    Waiting(Wait),
    /// The sender has too many connections: trying again in a while.
    Busy,
    /// Not going (paused, done, failed).
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// They have no connection open.
    Offline,
    /// Nobody answered (no app of theirs has the file open to share, or
    /// an app too old for it).
    NoAnswer,
    /// The connection fell RETRIES times in a row.
    Cut,
}

/// Stops an attempt (download or send), saying why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    Go,
    Pause,
    /// The download goes, with what it had written.
    Cancel,
    /// Signing out or quitting: it stays as it is, to go on later.
    Leave,
}

/// One attempt at a download, while it runs.
struct Run {
    /// Its number in the signalling.
    sid: String,
    halt: watch::Sender<Halt>,
    /// What the sender signals, for it.
    signals: mpsc::UnboundedSender<Value>,
}

/// A download of mine while the app runs.
struct Live {
    /// Bytes on the disk so far, for the bar.
    done: Arc<AtomicU64>,
    phase: Phase,
    run: Option<Run>,
    /// The next attempt, waiting (RETRY_AFTER, BUSY_AFTER).
    retry: Option<tokio::task::AbortHandle>,
    /// Attempts in a row without a piece more.
    tries: u32,
    speed: Speed,
    relayed: bool,
}

impl Live {
    fn new(done: u64) -> Live {
        Live {
            done: Arc::new(AtomicU64::new(done)),
            phase: Phase::Idle,
            run: None,
            retry: None,
            tries: 0,
            speed: Speed::default(),
            relayed: false,
        }
    }

    fn halt(&mut self, why: Halt) {
        if let Some(r) = self.retry.take() {
            r.abort();
        }
        if let Some(run) = &self.run {
            run.halt.send_replace(why);
        }
    }
}

/// A download's speed, smoothed over the samples the card takes.
#[derive(Default)]
struct Speed {
    at: Option<(Instant, u64)>,
    rate: f64,
}

impl Speed {
    fn sample(&mut self, bytes: u64) -> f64 {
        let now = Instant::now();
        match self.at {
            Some((at, was)) if now - at >= Duration::from_millis(500) => {
                let rate = bytes.saturating_sub(was) as f64 / (now - at).as_secs_f64();
                self.rate = if self.rate == 0.0 {
                    rate
                } else {
                    self.rate * 0.7 + rate * 0.3
                };
                self.at = Some((now, bytes));
            }
            Some(_) => {}
            None => self.at = Some((now, bytes)),
        }
        self.rate
    }
}

/// A file of mine going out to someone (or waiting its turn).
struct Serving {
    from: i64,
    sid: String,
    file: String,
    halt: watch::Sender<Halt>,
    signals: mpsc::UnboundedSender<Value>,
    sent: Arc<AtomicU64>,
    /// It has its turn and is sending.
    going: Arc<AtomicBool>,
}

struct Inner {
    /// The account signed in, 0 when none.
    me: i64,
    list: List,
    /// My downloads (the account's) while the app runs, by file id.
    live: HashMap<String, Live>,
    serving: Vec<Serving>,
    /// The turns of each person sending to (SENDS_PER_PERSON).
    turns: HashMap<i64, Arc<Semaphore>>,
    /// Each partial file's: held by an attempt from its start to its end,
    /// so two never write the same file and nothing deletes it under one.
    disks: HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>,
}

/// What a download's card shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Card {
    /// The attachment says it is direct, but not what to ask for.
    Broken,
    /// Mine, shared from this PC: to this many people now, and this many
    /// waiting their turn.
    Sharing {
        sending: usize,
        waiting: usize,
    },
    /// Mine, no longer shared.
    Stopped,
    /// Mine, sent from another PC of mine: only that one has it.
    Elsewhere,
    /// Someone else's, not downloaded here.
    Offer,
    Going {
        phase: Phase,
        done: u64,
        /// Bytes a second.
        speed: f64,
        /// Through the relay (TURN) rather than straight between the PCs.
        relayed: bool,
    },
    Paused {
        done: u64,
    },
    /// Stopped for good: `failure` says why.
    Failed {
        why: String,
    },
    Done {
        path: PathBuf,
    },
}

/// What went wrong, for the card and the toast, and whether trying again
/// may help.
pub fn failure(why: &str, who: &str) -> (String, bool) {
    if let Some(disk) = why.strip_prefix("disk:") {
        return (crate::tf!("direct.fail_disk", disk = disk), true);
    }
    match Stop::from_wire(why) {
        Stop::Changed => (crate::tf!("direct.fail_changed", who = who), false),
        Stop::Gone => (crate::tf!("direct.fail_gone", who = who), false),
        Stop::Denied => (crate::t!("direct.fail_denied").to_owned(), false),
        Stop::Unreadable => (crate::tf!("direct.fail_unreadable", who = who), true),
        Stop::Corrupt => (crate::t!("direct.fail_corrupt").to_owned(), true),
        Stop::Offline | Stop::Busy => (crate::tf!("direct.fail_busy", who = who), true),
    }
}

/// The lists, and everything going on with them.
pub struct Direct {
    rt: tokio::runtime::Handle,
    api: Arc<Api>,
    signal: Signal,
    dir: PathBuf,
    news: Box<dyn Fn(News) + Send + Sync>,
    inner: Mutex<Inner>,
    ice: Mutex<Option<(Instant, IceConfig)>>,
    /// The turns to send (SENDS).
    sends: Arc<Semaphore>,
    /// Sends that went out whole since the app started (for the tests).
    completed: AtomicU64,
    saver: std::sync::mpsc::Sender<Save>,
}

impl Direct {
    pub fn new(
        rt: tokio::runtime::Handle,
        api: Arc<Api>,
        signal: Signal,
        dir: PathBuf,
        news: Box<dyn Fn(News) + Send + Sync>,
    ) -> Arc<Direct> {
        let mut list: List = std::fs::read(dir.join(LIST))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let pruned = list.prune(&dir, now_ms());
        let direct = Arc::new(Direct {
            rt,
            api,
            signal,
            saver: saver(dir.join(LIST)),
            dir,
            news,
            inner: Mutex::new(Inner {
                me: 0,
                list,
                live: HashMap::new(),
                serving: Vec::new(),
                turns: HashMap::new(),
                disks: HashMap::new(),
            }),
            ice: Mutex::new(None),
            sends: Arc::new(Semaphore::new(SENDS)),
            completed: AtomicU64::new(0),
        });
        if pruned {
            direct.save(&direct.lock());
        }
        direct
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lists go to the disk, from a thread of their own.
    fn save(&self, st: &Inner) {
        if let Ok(bytes) = serde_json::to_vec_pretty(&st.list) {
            let _ = self.saver.send(Save { bytes, done: None });
        }
    }

    fn changed(&self) {
        (self.news)(News::Changed);
    }

    /// One step of the signalling, out.
    fn tell(&self, to: i64, msg: i64, file: &str, data: Value) {
        (self.signal)(to, msg, file, data);
    }

    /// The ICE servers (and the relay's credentials), asked of the server
    /// once in a while.
    async fn ice(&self) -> Option<IceConfig> {
        if let Some((at, ice)) = &*self.ice.lock().unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < ICE_FOR
        {
            return Some(ice.clone());
        }
        let got: Option<IceConfig> = self.api.call_as(&Req::get("/ice")).await.ok();
        if let Some(ice) = &got {
            *self.ice.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((Instant::now(), ice.clone()));
        }
        got
    }

    // ------------------------------------------------------------ sending

    /// Gets a file ready to go directly: its pieces' hashes, read from the
    /// disk (`progress` counts the bytes and can cancel), kept beside the
    /// list. The attachment its message carries.
    pub async fn prepare(
        &self,
        path: &Path,
        name: &str,
        progress: &Arc<Progress>,
    ) -> Result<Attachment, String> {
        let me = self.lock().me;
        if me == 0 {
            return Err("Entre na sua conta para enviar.".into());
        }
        let unreadable = |e: std::io::Error| format!("Não consegui ler {name}: {e}");
        let meta = std::fs::metadata(path).map_err(unreadable)?;
        let (size, modified) = (meta.len(), meta.modified().ok());
        let piece = hash::piece_for(size)
            .ok_or_else(|| format!("{name} é grande demais para enviar direto."))?;
        let (p, prog) = (path.to_path_buf(), progress.clone());
        let hashes = tokio::task::spawn_blocking(move || {
            hash::hash_file(&p, size, piece, &prog.sent, &prog.cancel)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            if e.to_string() == CANCELLED {
                CANCELLED.to_owned()
            } else {
                unreadable(e)
            }
        })?;
        // Changed while it was read: not the file that was measured.
        let after = std::fs::metadata(path).map_err(unreadable)?;
        if after.len() != size || after.modified().ok() != modified {
            return Err(format!(
                "{name} mudou enquanto era preparado. Tente de novo."
            ));
        }
        let root = hash::root(size, piece, &hashes);
        let id = new_id(&root, path);
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        write_atomic(&shared_hashes(&self.dir, &id), &hash::to_bytes(&hashes))
            .map_err(|e| format!("Não consegui guardar os dados de {name}: {e}"))?;
        let mut st = self.lock();
        if st.me != me {
            return Err("A conta mudou.".into());
        }
        st.list.shared.push(Shared {
            id: id.clone(),
            owner: me,
            path: path.to_path_buf(),
            name: name.to_owned(),
            size,
            piece,
            root: hash::hex(&root),
            created: now_ms(),
            msgs: Vec::new(),
            stopped: None,
        });
        self.save(&st);
        Ok(Attachment {
            url: String::new(),
            name: name.to_owned(),
            size: i64::try_from(size).unwrap_or(i64::MAX),
            kind: Kind::Direct,
            direct: Some(DirectInfo {
                id,
                sha256: hash::hex(&root),
                piece,
            }),
        })
    }

    /// Message `msg` went out with these files of mine.
    pub fn posted(&self, ids: &[String], msg: i64) {
        let mut st = self.lock();
        let me = st.me;
        let mut changed = false;
        for s in &mut st.list.shared {
            if s.owner == me && ids.contains(&s.id) && !s.msgs.contains(&msg) {
                s.msgs.push(msg);
                changed = true;
            }
        }
        if changed {
            self.save(&st);
        }
    }

    /// "Parar de compartilhar": whoever is getting it now stops, and anyone
    /// who asks again is told it is gone.
    pub fn stop_sharing(&self, id: &str) {
        let mut st = self.lock();
        let me = st.me;
        let Some(s) = st
            .list
            .shared
            .iter_mut()
            .find(|s| s.id == id && s.owner == me && s.stopped.is_none())
        else {
            return;
        };
        s.stopped = Some(now_ms());
        let _ = std::fs::remove_file(shared_hashes(&self.dir, id));
        for sv in st.serving.iter().filter(|sv| sv.file == id) {
            sv.halt.send_replace(Halt::Cancel);
        }
        self.save(&st);
        drop(st);
        self.changed();
    }

    // ------------------------------------------------------------ receiving

    /// Starts downloading file `a` of message `msg` (in conversation `cid`,
    /// sent by `from`) to `path`.
    pub fn download(
        self: &Arc<Self>,
        a: &Attachment,
        msg: i64,
        cid: i64,
        from: i64,
        path: PathBuf,
    ) -> Result<(), String> {
        let invalid = || "Este anexo veio incompleto.".to_owned();
        let info = a.direct.as_ref().ok_or_else(invalid)?;
        let size = u64::try_from(a.size).map_err(|_| invalid())?;
        let valid = info.id.len() == 32
            && parse_hash(&info.sha256).is_some()
            && info.piece.is_power_of_two()
            && hash::pieces(size, info.piece) <= hash::MAX_PIECES;
        if !valid {
            return Err(invalid());
        }
        let mut st = self.lock();
        let me = st.me;
        if me == 0 || from == me {
            return Err("Este arquivo é seu.".into());
        }
        let going = |d: &Download| matches!(d.state, State::Active | State::Paused);
        if st
            .list
            .downloads
            .iter()
            .any(|d| d.owner == me && d.id == info.id && going(d))
        {
            return Err("Este arquivo já está sendo baixado.".into());
        }
        if st
            .list
            .downloads
            .iter()
            .any(|d| d.owner == me && d.path == path && going(d))
        {
            return Err("Outro download já vai para esse lugar.".into());
        }
        // One of this file that is over (done, or failed) gives its place;
        // what a failed one had written goes on, if it was going to the same
        // place (it is the same file: same id, same hashes).
        let mut verified = 0;
        if let Some(i) = st
            .list
            .downloads
            .iter()
            .position(|d| d.owner == me && d.id == info.id)
        {
            let old = st.list.downloads.remove(i);
            if old.state == State::Failed {
                if old.path == path {
                    verified = old.verified;
                } else {
                    let _ = std::fs::remove_file(partial_path(&old.path));
                }
            }
        }
        st.list.downloads.push(Download {
            id: info.id.clone(),
            owner: me,
            msg,
            cid,
            from,
            name: a.name.clone(),
            size,
            piece: info.piece,
            root: info.sha256.clone(),
            path,
            verified,
            state: State::Active,
            why: None,
            created: now_ms(),
        });
        st.live.insert(
            info.id.clone(),
            Live::new((verified * info.piece).min(size)),
        );
        self.save(&st);
        self.start(&mut st, &info.id);
        drop(st);
        self.changed();
        Ok(())
    }

    /// Starts an attempt at download `id`, unless one is running or it
    /// should not go.
    fn start(self: &Arc<Self>, st: &mut Inner, id: &str) {
        let me = st.me;
        let Some(d) = st
            .list
            .downloads
            .iter()
            .find(|d| d.owner == me && d.id == id && d.state == State::Active)
        else {
            return;
        };
        let Some(root) = parse_hash(&d.root) else {
            return;
        };
        let partial = partial_path(&d.path);
        let disk = st.disks.entry(partial.clone()).or_default().clone();
        let live = st
            .live
            .entry(id.to_owned())
            .or_insert_with(|| Live::new((d.verified * d.piece).min(d.size)));
        if live.run.is_some() {
            return;
        }
        if let Some(r) = live.retry.take() {
            r.abort();
        }
        let sid = new_sid();
        let (halt, halt_rx) = watch::channel(Halt::Go);
        let (signals, signals_rx) = mpsc::unbounded_channel();
        live.run = Some(Run {
            sid: sid.clone(),
            halt,
            signals,
        });
        live.phase = Phase::Connecting;
        live.relayed = false;
        let job = fetch::Job {
            owner: me,
            from: d.from,
            msg: d.msg,
            file: d.id.clone(),
            sid: sid.clone(),
            size: d.size,
            piece: d.piece,
            root,
            path: d.path.clone(),
            partial,
            hashes: download_hashes(&self.dir, me, id),
            verified: d.verified,
            done: live.done.clone(),
            disk,
            halt: halt_rx,
            signals: signals_rx,
        };
        let this = self.clone();
        let id = id.to_owned();
        self.rt.spawn(async move {
            let end = fetch::run(&this, job).await;
            this.ended(&id, &sid, end);
        });
    }

    /// An attempt ended: what next.
    fn ended(self: &Arc<Self>, id: &str, sid: &str, end: fetch::End) {
        let mut guard = self.lock();
        let st = &mut *guard;
        let me = st.me;
        let Some(live) = st.live.get_mut(id) else {
            return;
        };
        let Some(run) = live.run.take_if(|r| r.sid == sid) else {
            return;
        };
        let halt = *run.halt.borrow();
        live.speed = Speed::default();
        let Some(d) = st
            .list
            .downloads
            .iter_mut()
            .find(|d| d.owner == me && d.id == id)
        else {
            return;
        };
        let mut news = None;
        let mut again = None;
        match (halt, end) {
            (Halt::Cancel, _) => {}
            (Halt::Pause | Halt::Leave, _) => live.phase = Phase::Idle,
            (Halt::Go, fetch::End::Done) => {
                d.state = State::Done;
                d.verified = hash::pieces(d.size, d.piece);
                live.phase = Phase::Idle;
                news = Some(News::Downloaded {
                    name: d.name.clone(),
                    cid: d.cid,
                });
            }
            (Halt::Go, fetch::End::Refused(Stop::Offline)) => {
                live.phase = Phase::Waiting(Wait::Offline);
            }
            (Halt::Go, fetch::End::NoAnswer) => live.phase = Phase::Waiting(Wait::NoAnswer),
            (Halt::Go, fetch::End::Refused(Stop::Busy)) => {
                live.phase = Phase::Busy;
                again = Some(BUSY_AFTER);
            }
            // Cut short: again in a moment, unless it keeps happening with
            // nothing more done.
            (Halt::Go, fetch::End::Cut { progressed, why }) => {
                crate::logln!("direto: {} parou: {why}", d.name);
                live.tries = if progressed { 1 } else { live.tries + 1 };
                if live.tries >= RETRIES {
                    live.tries = 0;
                    live.phase = Phase::Waiting(Wait::Cut);
                } else {
                    live.phase = Phase::Retrying;
                    again = Some(RETRY_AFTER);
                }
            }
            (Halt::Go, fetch::End::Refused(why)) => {
                d.state = State::Failed;
                d.why = Some(why.wire().to_owned());
                live.phase = Phase::Idle;
            }
            (Halt::Go, fetch::End::Disk(e)) => {
                d.state = State::Failed;
                d.why = Some(format!("disk:{e}"));
                live.phase = Phase::Idle;
            }
            (Halt::Go, fetch::End::Halted) => live.phase = Phase::Idle,
        }
        if d.state == State::Failed {
            news = Some(News::Stopped {
                name: d.name.clone(),
                from: d.from,
                why: d.why.clone().unwrap_or_default(),
            });
        }
        if d.state == State::Done {
            let _ = std::fs::remove_file(download_hashes(&self.dir, me, id));
        }
        if let Some(after) = again {
            let this = self.clone();
            let key = id.to_owned();
            let task = self.rt.spawn(async move {
                tokio::time::sleep(after).await;
                let mut st = this.lock();
                if let Some(live) = st.live.get_mut(&key) {
                    live.retry = None;
                }
                this.start(&mut st, &key);
            });
            live.retry = Some(task.abort_handle());
        }
        self.save(st);
        drop(guard);
        if let Some(n) = news {
            (self.news)(n);
        }
        self.changed();
    }

    /// A download's attempt says where it is.
    fn set_phase(&self, id: &str, sid: &str, phase: Phase, relayed: Option<bool>) {
        let mut st = self.lock();
        if let Some(live) = st.live.get_mut(id)
            && live.run.as_ref().is_some_and(|r| r.sid == sid)
        {
            live.phase = phase;
            if let Some(r) = relayed {
                live.relayed = r;
            }
            drop(st);
            self.changed();
        }
    }

    /// Download `id` of `owner` has `verified` pieces safely on the disk.
    fn checkpoint(&self, owner: i64, id: &str, verified: u64) {
        let mut st = self.lock();
        let Some(d) = st
            .list
            .downloads
            .iter_mut()
            .find(|d| d.owner == owner && d.id == id)
        else {
            return;
        };
        if d.verified != verified {
            d.verified = verified;
            self.save(&st);
        }
    }

    /// "Pausar".
    pub fn pause(&self, id: &str) {
        let mut st = self.lock();
        let me = st.me;
        let Some(d) = st
            .list
            .downloads
            .iter_mut()
            .find(|d| d.owner == me && d.id == id && d.state == State::Active)
        else {
            return;
        };
        d.state = State::Paused;
        if let Some(live) = st.live.get_mut(id) {
            live.halt(Halt::Pause);
            live.phase = Phase::Idle;
        }
        self.save(&st);
        drop(st);
        self.changed();
    }

    /// "Continuar", "Tentar de novo", "Tentar agora".
    pub fn resume(self: &Arc<Self>, id: &str) {
        let mut st = self.lock();
        let me = st.me;
        let Some(d) = st
            .list
            .downloads
            .iter_mut()
            .find(|d| d.owner == me && d.id == id && d.state != State::Done)
        else {
            return;
        };
        d.state = State::Active;
        d.why = None;
        if let Some(live) = st.live.get_mut(id) {
            live.tries = 0;
        }
        self.save(&st);
        self.start(&mut st, id);
        drop(st);
        self.changed();
    }

    /// "Cancelar": the download goes, and what it had written with it. A
    /// download that is whole only leaves the list (its file stays).
    pub fn cancel(&self, id: &str) {
        let mut st = self.lock();
        let me = st.me;
        let Some(i) = st
            .list
            .downloads
            .iter()
            .position(|d| d.owner == me && d.id == id)
        else {
            return;
        };
        let d = st.list.downloads.remove(i);
        let running = match st.live.remove(id) {
            Some(mut live) => {
                live.halt(Halt::Cancel);
                live.run.is_some()
            }
            None => false,
        };
        // A running attempt deletes them itself as it ends (fetch.rs);
        // otherwise nothing has them open.
        if !running && d.state != State::Done {
            let _ = std::fs::remove_file(partial_path(&d.path));
            let _ = std::fs::remove_file(download_hashes(&self.dir, me, id));
        }
        self.save(&st);
        drop(st);
        self.changed();
    }

    // ------------------------------------------------------------ events

    /// A DIRECT event: a step of someone's signalling.
    pub fn signal(self: &Arc<Self>, from: i64, msg: i64, file: &str, data: Value) {
        let Some(sid) = data["sid"].as_str().map(str::to_owned) else {
            return;
        };
        let mut st = self.lock();
        let me = st.me;
        if me == 0 {
            return;
        }
        // An answer for a download of mine.
        let mine = st
            .list
            .downloads
            .iter()
            .any(|d| d.owner == me && d.id == file && d.from == from);
        if mine
            && let Some(run) = st.live.get(file).and_then(|l| l.run.as_ref())
            && run.sid == sid
        {
            let _ = run.signals.send(data);
            return;
        }
        // More for a send of mine.
        if let Some(sv) = st.serving.iter().find(|s| s.from == from && s.sid == sid) {
            let _ = sv.signals.send(data);
            return;
        }
        // Someone asks for a file of mine.
        if data.get("description").is_some() && data.get("error").is_none() {
            self.serve(&mut st, from, msg, file, sid, data);
        }
    }

    /// Someone asks for file `file` of mine, with their offer.
    fn serve(
        self: &Arc<Self>,
        st: &mut Inner,
        from: i64,
        msg: i64,
        file: &str,
        sid: String,
        offer: Value,
    ) {
        let me = st.me;
        // Not on this PC (another of mine may have it): someone else answers.
        let Some(s) = st
            .list
            .shared
            .iter_mut()
            .find(|s| s.id == file && s.owner == me)
        else {
            return;
        };
        let refuse = |why: Stop| self.tell(from, msg, file, wire::refusal(&sid, why));
        if s.stopped.is_some() {
            refuse(Stop::Gone);
            return;
        }
        if st.serving.len() >= CONNECTIONS {
            refuse(Stop::Busy);
            return;
        }
        // The server checked the message has it: the message is surely
        // this file's, even if the answer to the send never came.
        if !s.msgs.contains(&msg) {
            s.msgs.push(msg);
        }
        let entry = s.clone();
        let (halt, halt_rx) = watch::channel(Halt::Go);
        let (signals, signals_rx) = mpsc::unbounded_channel();
        let sent = Arc::new(AtomicU64::new(0));
        let going = Arc::new(AtomicBool::new(false));
        st.serving.push(Serving {
            from,
            sid: sid.clone(),
            file: file.to_owned(),
            halt,
            signals,
            sent: sent.clone(),
            going: going.clone(),
        });
        let turn = st
            .turns
            .entry(from)
            .or_insert_with(|| Arc::new(Semaphore::new(SENDS_PER_PERSON)))
            .clone();
        self.save(st);
        let job = serve::Job {
            from,
            msg,
            sid: sid.clone(),
            id: entry.id.clone(),
            path: entry.path,
            size: entry.size,
            piece: entry.piece,
            root: parse_hash(&entry.root).unwrap_or_default(),
            hashes: shared_hashes(&self.dir, &entry.id),
            offer,
            halt: halt_rx,
            signals: signals_rx,
            sent,
            going,
            sends: self.sends.clone(),
            turn,
        };
        let this = self.clone();
        self.rt.spawn(async move {
            let whole = serve::run(&this, job).await;
            this.served(from, &sid, whole);
        });
        self.changed();
    }

    /// A send of mine is over (`whole`: all of it got there).
    fn served(&self, from: i64, sid: &str, whole: bool) {
        if whole {
            self.completed.fetch_add(1, Ordering::Relaxed);
        }
        let mut st = self.lock();
        st.serving.retain(|s| !(s.from == from && s.sid == sid));
        if !st.serving.iter().any(|s| s.from == from) {
            st.turns.remove(&from);
        }
        drop(st);
        self.changed();
    }

    /// A PRESENCE event: whoever it is about may be back.
    pub fn presence(self: &Arc<Self>, uid: i64) {
        let mut st = self.lock();
        let me = st.me;
        let waiting: Vec<String> = st
            .list
            .downloads
            .iter()
            .filter(|d| d.owner == me && d.from == uid && d.state == State::Active)
            .filter(|d| {
                st.live
                    .get(&d.id)
                    .is_none_or(|l| l.run.is_none() && matches!(l.phase, Phase::Waiting(_)))
            })
            .map(|d| d.id.clone())
            .collect();
        for id in waiting {
            self.start(&mut st, &id);
        }
    }

    /// READY: who is signed in, and a connection that may have missed
    /// people coming back. Everything that should be going goes.
    pub fn ready(self: &Arc<Self>, me: i64) {
        let mut st = self.lock();
        if st.me != me {
            leave(&mut st);
            st.me = me;
        }
        let idle: Vec<String> = st
            .list
            .downloads
            .iter()
            .filter(|d| d.owner == me && d.state == State::Active)
            .filter(|d| {
                st.live
                    .get(&d.id)
                    .is_none_or(|l| l.run.is_none() && l.retry.is_none())
            })
            .map(|d| d.id.clone())
            .collect();
        for id in idle {
            self.start(&mut st, &id);
        }
        drop(st);
        self.changed();
    }

    /// A MESSAGE_DELETE: its files are no longer shared (once no other
    /// message has them), and its downloads that are not whole cannot be.
    pub fn message_deleted(&self, msg: i64) {
        let mut st = self.lock();
        let me = st.me;
        let mut stop = Vec::new();
        for s in &mut st.list.shared {
            if s.owner == me && s.msgs.contains(&msg) {
                s.msgs.retain(|&m| m != msg);
                if s.msgs.is_empty() && s.stopped.is_none() {
                    stop.push(s.id.clone());
                }
            }
        }
        let gone: Vec<String> = st
            .list
            .downloads
            .iter()
            .filter(|d| d.owner == me && d.msg == msg)
            .map(|d| d.id.clone())
            .collect();
        self.save(&st);
        drop(st);
        for id in stop {
            self.stop_sharing(&id);
        }
        for id in gone {
            self.cancel(&id);
        }
    }

    /// Signing out: everything stops as it is, to go on when this account
    /// is back.
    pub fn logout(&self) {
        let mut st = self.lock();
        leave(&mut st);
        st.me = 0;
        drop(st);
        self.changed();
    }

    /// Quitting: as signing out, and the lists are on the disk before the
    /// app goes.
    pub fn quit(&self) {
        let st = {
            let mut st = self.lock();
            leave(&mut st);
            serde_json::to_vec_pretty(&st.list)
        };
        if let Ok(bytes) = st {
            let (done, wait) = std::sync::mpsc::channel();
            if self
                .saver
                .send(Save {
                    bytes,
                    done: Some(done),
                })
                .is_ok()
            {
                let _ = wait.recv_timeout(Duration::from_secs(2));
            }
        }
    }

    // ------------------------------------------------------------ what shows

    /// What the card of attachment `a`, on a message by `author`, shows.
    pub fn card(&self, a: &Attachment, author: i64) -> Card {
        let Some(info) = &a.direct else {
            return Card::Broken;
        };
        let mut st = self.lock();
        let me = st.me;
        if author == me {
            let Some(s) = st
                .list
                .shared
                .iter()
                .find(|s| s.id == info.id && s.owner == me)
            else {
                return Card::Elsewhere;
            };
            if s.stopped.is_some() {
                return Card::Stopped;
            }
            let (mut sending, mut waiting) = (0, 0);
            for sv in st.serving.iter().filter(|sv| sv.file == info.id) {
                if sv.going.load(Ordering::Relaxed) {
                    sending += 1;
                } else {
                    waiting += 1;
                }
            }
            return Card::Sharing { sending, waiting };
        }
        let Some(d) = st
            .list
            .downloads
            .iter()
            .find(|d| d.owner == me && d.id == info.id)
        else {
            return Card::Offer;
        };
        let (state, why, path) = (d.state, d.why.clone(), d.path.clone());
        let saved = (d.verified * d.piece).min(d.size);
        let live = st.live.get_mut(&info.id);
        let done = live
            .as_ref()
            .map_or(saved, |l| l.done.load(Ordering::Relaxed));
        match state {
            State::Done => Card::Done { path },
            State::Failed => Card::Failed {
                why: why.unwrap_or_default(),
            },
            State::Paused => Card::Paused { done },
            State::Active => match live {
                Some(l) => Card::Going {
                    phase: l.phase,
                    done,
                    speed: if l.phase == Phase::Receiving {
                        l.speed.sample(done)
                    } else {
                        0.0
                    },
                    relayed: l.relayed,
                },
                None => Card::Going {
                    phase: Phase::Idle,
                    done,
                    speed: 0.0,
                    relayed: false,
                },
            },
        }
    }

    /// Forgets a download that is whole (its file went from the disk), so
    /// its card offers it again.
    pub fn forget(&self, id: &str) {
        let mut st = self.lock();
        let me = st.me;
        st.list
            .downloads
            .retain(|d| !(d.owner == me && d.id == id && d.state == State::Done));
        self.save(&st);
        drop(st);
        self.changed();
    }

    /// For the test runs (smoke.rs): what is shared and downloaded, and how
    /// far.
    pub fn report(&self) -> Value {
        let mut st = self.lock();
        let me = st.me;
        let shared: Vec<Value> = st
            .list
            .shared
            .iter()
            .filter(|s| s.owner == me)
            .map(|s| {
                let serving = st.serving.iter().filter(|sv| sv.file == s.id);
                json!({
                    "id": s.id, "name": s.name, "size": s.size, "piece": s.piece,
                    "msgs": s.msgs, "stopped": s.stopped.is_some(),
                    "sending": serving.clone().filter(|sv| sv.going.load(Ordering::Relaxed)).count(),
                    "sent": serving.map(|sv| sv.sent.load(Ordering::Relaxed)).sum::<u64>(),
                })
            })
            .collect();
        let rows: Vec<(String, Download)> = st
            .list
            .downloads
            .iter()
            .filter(|d| d.owner == me)
            .map(|d| (d.id.clone(), d.clone()))
            .collect();
        let downloads: Vec<Value> = rows
            .into_iter()
            .map(|(id, d)| {
                let live = st.live.get_mut(&id);
                json!({
                    "id": id, "name": d.name, "size": d.size, "piece": d.piece,
                    "state": d.state, "why": d.why, "verified": d.verified,
                    "path": d.path.display().to_string(),
                    "done": live.as_ref().map(|l| l.done.load(Ordering::Relaxed)),
                    "phase": live.as_ref().map(|l| format!("{:?}", l.phase)),
                    "relayed": live.as_ref().map(|l| l.relayed),
                    "tries": live.as_ref().map(|l| l.tries),
                })
            })
            .collect();
        json!({
            "me": me, "shared": shared, "downloads": downloads,
            "serving": st.serving.len(),
            "completed": self.completed.load(Ordering::Relaxed),
        })
    }
}

/// Everything going stops as it is (signing out, another account, quitting).
fn leave(st: &mut Inner) {
    for live in st.live.values_mut() {
        live.halt(Halt::Leave);
    }
    st.live.clear();
    for sv in &st.serving {
        sv.halt.send_replace(Halt::Leave);
    }
}

// ---------------------------------------------------------------- bits

struct Save {
    bytes: Vec<u8>,
    /// Told once these bytes (or newer) are on the disk.
    done: Option<std::sync::mpsc::Sender<()>>,
}

/// The thread that writes the lists: only the newest of what waits.
fn saver(path: PathBuf) -> std::sync::mpsc::Sender<Save> {
    let (tx, rx) = std::sync::mpsc::channel::<Save>();
    let spawned = std::thread::Builder::new()
        .name("direto-lista".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut save = first;
                let mut waiting = Vec::new();
                loop {
                    if let Some(done) = save.done.take() {
                        waiting.push(done);
                    }
                    match rx.try_recv() {
                        Ok(newer) => save = newer,
                        Err(_) => break,
                    }
                }
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = write_atomic(&path, &save.bytes) {
                    crate::logln!("direto: não consegui gravar a lista: {e}");
                }
                for done in waiting {
                    let _ = done.send(());
                }
            }
        });
    if let Err(e) = spawned {
        crate::logln!("direto: sem a linha que grava a lista: {e}");
    }
    tx
}

/// Writes `bytes` to `path` whole or not at all: to a file beside it, then
/// over it.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A number that only has to differ from every other.
fn unique() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish()
}

/// A file's id: 32 hex characters, from its hash, its place and the moment,
/// so the same file sent twice is two files.
fn new_id(root: &Hash, path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(root);
    h.update(path.as_os_str().as_encoded_bytes());
    h.update(unique().to_le_bytes());
    h.update(unique().to_le_bytes());
    let full: Hash = h.finalize().into();
    ::hex::encode(&full[..16])
}

/// An attempt's number in the signalling.
fn new_sid() -> String {
    format!("{:016x}", unique())
}

fn parse_hash(s: &str) -> Option<Hash> {
    let bytes = ::hex::decode(s).ok()?;
    bytes.try_into().ok()
}

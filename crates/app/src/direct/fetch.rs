//! One attempt at a download: from the offer to the last piece, or to
//! whatever stopped it.
//!
//! What is already in the partial file counts as far as the list says it
//! was checked (`verified`, noted as it goes); anything past that is cut,
//! and the sender is asked for the rest. The pieces' hashes come first
//! when they are not here yet, checked against the message's root before
//! anything else; then every piece is checked as it is written, on a
//! thread of its own, which every few seconds makes sure the disk has it
//! and notes how far it got. When the disk cannot keep up, that thread's
//! queue fills, the channel stops being read, and the sender slows down
//! (the WebRTC stack holds back what the application has not read, and
//! SCTP's window does the rest).

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::DataChannelEvent;
use webrtc::peer_connection::RTCPeerConnectionState;

use super::hash::{self, Hash};
use super::link::{Link, LinkEv};
use super::wire::{self, Said, Stop};
use super::{Direct, Halt, Phase};
use crate::voice::relay::Relay;

/// How long the connection may take to come up: the answer, ICE (the relay
/// over TCP takes a few seconds more), DTLS and SCTP.
pub(super) const SETUP: Duration = Duration::from_secs(30);

/// How often what was written is made sure on the disk, and noted.
const CHECKPOINT: Duration = Duration::from_secs(5);

/// Chunks waiting for the disk (64 KiB each).
const QUEUE: usize = 64;

pub(super) struct Job {
    pub owner: i64,
    /// The sender, and the message and file asked for.
    pub from: i64,
    pub msg: i64,
    pub file: String,
    /// This attempt's number in the signalling.
    pub sid: String,
    pub size: u64,
    pub piece: u64,
    pub root: Hash,
    /// Where it goes once whole, where it waits until then, and where its
    /// pieces' hashes are kept.
    pub path: PathBuf,
    pub partial: PathBuf,
    pub hashes: PathBuf,
    /// Pieces the list says are checked.
    pub verified: u64,
    /// Bytes on the disk, for the bar.
    pub done: Arc<AtomicU64>,
    pub disk: Arc<tokio::sync::Mutex<()>>,
    pub halt: watch::Receiver<Halt>,
    pub signals: mpsc::UnboundedReceiver<Value>,
}

/// How an attempt ended.
pub(super) enum End {
    /// Whole, and at its place.
    Done,
    /// The sender (or the server) said why not; `Corrupt` when what came
    /// did not hold together.
    Refused(Stop),
    /// Nobody answered the offer in time.
    NoAnswer,
    /// The connection fell or never came up; `progressed` when it brought
    /// at least one piece more.
    Cut { progressed: bool, why: String },
    /// Stopped here (paused, cancelled, signed out).
    Halted,
    /// This PC could not write it.
    Disk(String),
}

fn cut(why: &str) -> End {
    End::Cut {
        progressed: false,
        why: why.to_owned(),
    }
}

/// What came does not hold together: it stops for good, and the log says
/// how.
fn broken(why: &str) -> Out {
    crate::logln!("direto: o que veio não confere: {why}");
    Out::End(End::Refused(Stop::Corrupt))
}

pub(super) async fn run(d: &Arc<Direct>, mut job: Job) -> End {
    // The partial file is this attempt's until it ends.
    let _disk = job.disk.clone().lock_owned().await;
    let end = attempt(d, &mut job).await;
    if *job.halt.borrow() == Halt::Cancel {
        let _ = std::fs::remove_file(&job.partial);
        let _ = std::fs::remove_file(&job.hashes);
    }
    end
}

async fn attempt(d: &Arc<Direct>, job: &mut Job) -> End {
    if *job.halt.borrow() != Halt::Go {
        return End::Halted;
    }
    let count = hash::pieces(job.size, job.piece);
    // An empty file needs nobody: its root says it all.
    if count == 0 {
        if hash::root(0, job.piece, &[]) != job.root {
            return End::Refused(Stop::Corrupt);
        }
        if let Err(e) = std::fs::File::create(&job.partial) {
            return End::Disk(e.to_string());
        }
        return finish(job);
    }
    let (file, start) = match open_partial(&job.partial, job.verified, job.piece, job.size) {
        Ok(opened) => opened,
        Err(e) => return End::Disk(e.to_string()),
    };
    job.done
        .store((start * job.piece).min(job.size), Ordering::Relaxed);
    // Whole already: the app closed between the last piece and the name.
    if start == count {
        drop(file);
        return finish(job);
    }
    let known = load_hashes(&job.hashes, count, job.size, job.piece, &job.root);
    let ice = d.ice().await;
    let relay = Relay::start(ice.as_ref()).await;
    let end = match Link::new(ice.as_ref(), &relay.servers()).await {
        Ok(mut link) => {
            let end = talk(d, job, &mut link, file, start, known, count).await;
            link.close().await;
            end
        }
        Err(e) => cut(&e),
    };
    relay.close().await;
    end
}

/// How the conversation over the connection ended.
enum Out {
    /// The sender says it is all out.
    Done,
    End(End),
}

async fn talk(
    d: &Arc<Direct>,
    job: &mut Job,
    link: &mut Link,
    file: std::fs::File,
    start: u64,
    known: Option<Vec<Hash>>,
    count: u64,
) -> End {
    let (to, msg, fid, sid) = (job.from, job.msg, job.file.clone(), job.sid.clone());
    let tell = |data: Value| d.tell(to, msg, &fid, wire::signal(&sid, data));
    match link.offer().await {
        Ok(offer) => tell(offer),
        Err(e) => return cut(&e),
    }
    let channel = link.channel.clone();
    let deadline = tokio::time::Instant::now() + SETUP;
    let mut open = false;
    let mut answered = false;
    let mut file = Some(file);
    let mut list: Option<Arc<Vec<Hash>>> = known.map(Arc::new);
    // The list of hashes as it comes, when it was not here.
    let mut coming: Vec<u8> = Vec::new();
    let mut writer: Option<Writer> = None;
    // Where the next byte of the file goes.
    let mut next = (start * job.piece).min(job.size);
    let mut receiving = false;
    let out = loop {
        tokio::select! {
            ev = link.events.recv() => match ev {
                Some(LinkEv::Candidate(c)) => tell(link.candidate(c).await),
                Some(LinkEv::State(
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed,
                )) => break Out::End(cut("a conexão caiu")),
                Some(LinkEv::State(_)) => {}
                None => break Out::End(cut("a conexão acabou")),
            },
            Some(data) = job.signals.recv() => {
                if let Some(why) = data.get("error").and_then(Value::as_str) {
                    break Out::End(End::Refused(Stop::from_wire(why)));
                }
                answered |= data.get("description").is_some();
                if let Err(e) = link.remote(&data).await {
                    break Out::End(cut(&e));
                }
            }
            ev = channel.poll() => match ev {
                Some(DataChannelEvent::OnOpen) if !open => {
                    open = true;
                    let get = wire::get(start, list.is_none());
                    if channel.send_text(&get.to_string()).await.is_err() {
                        break Out::End(cut("o canal fechou"));
                    }
                    // The hashes are here already: the file can come at once.
                    if let Some(l) = &list
                        && let Some(f) = file.take()
                    {
                        writer = Some(Writer::start(d, job, f, start, l.clone()));
                    }
                }
                Some(DataChannelEvent::OnMessage(m)) if m.is_string => {
                    let said = serde_json::from_slice::<Value>(&m.data)
                        .ok()
                        .as_ref()
                        .and_then(wire::read);
                    match said {
                        Some(Said::Done) => break Out::Done,
                        Some(Said::Stop(why)) => break Out::End(End::Refused(why)),
                        Some(Said::Queued) => d.set_phase(&job.file, &job.sid, Phase::Queued, None),
                        _ => {}
                    }
                }
                Some(DataChannelEvent::OnMessage(m)) => {
                    let Some((kind, offset, len)) =
                        wire::read_chunk(&m.data).map(|(k, o, b)| (k, o, b.len()))
                    else {
                        break broken(&format!("uma mensagem de {} bytes", m.data.len()));
                    };
                    if !receiving {
                        receiving = true;
                        let relayed = link.relayed().await;
                        d.set_phase(&job.file, &job.sid, Phase::Receiving, Some(relayed));
                    }
                    let bytes = m.data.freeze().slice(wire::HEADER..);
                    if kind == wire::HASHES {
                        // Only when asked for, in order, and no more than
                        // the file has pieces.
                        let want = count as usize * 32;
                        if list.is_some()
                            || offset != coming.len() as u64
                            || coming.len() + len > want
                        {
                            break broken(&format!(
                                "hashes em {offset} ({len} bytes), com {} de {want}",
                                coming.len()
                            ));
                        }
                        coming.extend_from_slice(&bytes);
                        if coming.len() == want {
                            let got = hash::from_bytes(&coming).unwrap_or_default();
                            if hash::root(job.size, job.piece, &got) != job.root {
                                break broken("os hashes não dão a raiz da mensagem");
                            }
                            // Kept for the next attempt (which may as well
                            // ask again, should this not stick).
                            if let Err(e) = super::write_atomic(&job.hashes, &coming) {
                                crate::logln!("direto: não guardei os hashes: {e}");
                            }
                            let got = Arc::new(got);
                            list = Some(got.clone());
                            if let Some(f) = file.take() {
                                writer = Some(Writer::start(d, job, f, start, got));
                            }
                        }
                        continue;
                    }
                    // The file, in order, from where it was asked.
                    let Some(w) = &writer else {
                        break broken("o arquivo antes dos hashes");
                    };
                    if offset != next || next + len as u64 > job.size {
                        break broken(&format!("{len} bytes em {offset}, esperados em {next}"));
                    }
                    next += len as u64;
                    if !w.write(bytes).await {
                        // It stopped: why is in what it returns.
                        break Out::End(cut("o disco parou"));
                    }
                }
                Some(DataChannelEvent::OnClose) | None => break Out::End(cut("o canal fechou")),
                Some(_) => {}
            },
            () = tokio::time::sleep_until(deadline), if !open => {
                break Out::End(if answered {
                    cut("não deu para conectar")
                } else {
                    End::NoAnswer
                });
            }
            _ = job.halt.changed() => break Out::End(End::Halted),
        }
    };
    // The writer ends with what it has: its pieces count, whatever ended it.
    let (verified, failed) = match writer.take() {
        Some(w) => w.close(start).await,
        None => (start, None),
    };
    drop(file);
    let progressed = verified > start;
    match failed {
        Some(Failed::Disk(e)) => return End::Disk(e),
        Some(Failed::Corrupt) => {
            return End::Cut {
                progressed,
                why: "um pedaço veio diferente do seu hash".into(),
            };
        }
        None => {}
    }
    match out {
        Out::Done if verified == count => finish(job),
        // "All out", with pieces missing.
        Out::Done => {
            crate::logln!("direto: o envio acabou com {verified} de {count} pedaços");
            End::Refused(Stop::Corrupt)
        }
        Out::End(End::Cut { why, .. }) => End::Cut { progressed, why },
        Out::End(end) => end,
    }
}

/// Whole: the file takes its name.
fn finish(job: &Job) -> End {
    if let Err(e) = std::fs::rename(&job.partial, &job.path) {
        return End::Disk(e.to_string());
    }
    let _ = std::fs::remove_file(&job.hashes);
    End::Done
}

/// The partial file, cut to the pieces known to be good, ready to go on
/// from there, and from which piece.
fn open_partial(
    path: &Path,
    verified: u64,
    piece: u64,
    size: u64,
) -> std::io::Result<(std::fs::File, u64)> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let len = file.metadata()?.len();
    let count = hash::pieces(size, piece);
    // What the disk has may be less than the list says: the file went, or
    // the PC went down before the disk had it.
    let on_disk = if len >= size { count } else { len / piece };
    let start = verified.min(count).min(on_disk);
    let at = (start * piece).min(size);
    file.set_len(at)?;
    file.seek(SeekFrom::Start(at))?;
    Ok((file, start))
}

/// The pieces' hashes kept from an earlier attempt, if they are whole and
/// add up to the root.
fn load_hashes(path: &Path, count: u64, size: u64, piece: u64, root: &Hash) -> Option<Vec<Hash>> {
    let list = hash::from_bytes(&std::fs::read(path).ok()?)?;
    (list.len() as u64 == count && hash::root(size, piece, &list) == *root).then_some(list)
}

/// Why the writer stopped before its queue did.
enum Failed {
    Disk(String),
    /// A piece did not match its hash.
    Corrupt,
}

/// The thread that writes and checks the pieces.
struct Writer {
    tx: mpsc::Sender<Bytes>,
    task: tokio::task::JoinHandle<(u64, Option<Failed>)>,
}

impl Writer {
    fn start(
        d: &Arc<Direct>,
        job: &Job,
        file: std::fs::File,
        start: u64,
        list: Arc<Vec<Hash>>,
    ) -> Writer {
        let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
        let (d, owner, id) = (d.clone(), job.owner, job.file.clone());
        let (size, piece, done) = (job.size, job.piece, job.done.clone());
        let task = tokio::task::spawn_blocking(move || {
            let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
            // The piece being written, and how much of it is in.
            let (mut i, mut filled) = (start, 0u64);
            let mut h = Sha256::new();
            let mut synced = Instant::now();
            let mut failed = None;
            'queue: while let Some(bytes) = rx.blocking_recv() {
                let mut rest = &bytes[..];
                while !rest.is_empty() {
                    let len = hash::piece_len(size, piece, i);
                    if len == 0 {
                        // Past the end of the file.
                        failed = Some(Failed::Corrupt);
                        break 'queue;
                    }
                    let n = ((len - filled) as usize).min(rest.len());
                    if let Err(e) = out.write_all(&rest[..n]) {
                        failed = Some(Failed::Disk(e.to_string()));
                        break 'queue;
                    }
                    h.update(&rest[..n]);
                    filled += n as u64;
                    rest = &rest[n..];
                    done.fetch_add(n as u64, Ordering::Relaxed);
                    if filled == len {
                        let got: Hash = h.finalize_reset().into();
                        if list.get(i as usize) != Some(&got) {
                            failed = Some(Failed::Corrupt);
                            break 'queue;
                        }
                        i += 1;
                        filled = 0;
                    }
                }
                if synced.elapsed() >= CHECKPOINT {
                    if let Err(e) = sync(&mut out) {
                        failed = Some(Failed::Disk(e.to_string()));
                        break;
                    }
                    d.checkpoint(owner, &id, i);
                    synced = Instant::now();
                }
            }
            // The pieces checked are made sure on the disk, and noted.
            match sync(&mut out) {
                Ok(()) => d.checkpoint(owner, &id, i),
                Err(e) => {
                    failed.get_or_insert(Failed::Disk(e.to_string()));
                }
            }
            (i, failed)
        });
        Writer { tx, task }
    }

    /// Hands bytes to the disk; false once the writer stopped.
    async fn write(&self, bytes: Bytes) -> bool {
        self.tx.send(bytes).await.is_ok()
    }

    /// Waits for everything handed over to be written: the pieces checked,
    /// and what stopped it, if anything.
    async fn close(self, start: u64) -> (u64, Option<Failed>) {
        drop(self.tx);
        self.task
            .await
            .unwrap_or_else(|e| (start, Some(Failed::Disk(e.to_string()))))
    }
}

fn sync(out: &mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()> {
    out.flush()?;
    out.get_ref().sync_data()
}

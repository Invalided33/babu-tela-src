//! Sending a file of mine to one person who asked: the answer to their
//! offer, their turn, the pieces' hashes if they want them, then the file
//! from the piece they asked for.
//!
//! A thread reads the file a block at a time, a few blocks ahead, and checks
//! each piece against its hash before its last block goes: a file changed
//! since it was sent stops (wire::Stop::Changed) without a whole piece of it
//! going out. The channel's send waits once 8 MiB are on their way and not
//! yet received (link.rs), which paces everything to the connection.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::RTCPeerConnectionState;

use super::fetch::SETUP;
use super::hash::{self, Hash};
use super::link::{Link, LinkEv};
use super::wire::{self, Said, Stop};
use super::{Direct, Halt};
use crate::voice::relay::Relay;

/// Once everything is out, how long the other side has to say it has it
/// (by closing): what is still on its way must not be cut off.
const LINGER: Duration = Duration::from_secs(30);

/// After saying why it stops, how long the other side has to hear it.
const LINGER_STOP: Duration = Duration::from_secs(5);

/// Blocks read ahead of the connection (hash::BLOCK each).
const AHEAD: usize = 2;

pub(super) struct Job {
    /// Who asked, about which message, and their attempt's number.
    pub from: i64,
    pub msg: i64,
    pub sid: String,
    /// The file: its id, where it is, and how it was cut.
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub piece: u64,
    pub root: Hash,
    /// Where its pieces' hashes are kept.
    pub hashes: PathBuf,
    pub offer: Value,
    pub halt: watch::Receiver<Halt>,
    pub signals: mpsc::UnboundedReceiver<Value>,
    /// Bytes of the file sent, for the card and the tests.
    pub sent: Arc<AtomicU64>,
    /// Set once it has its turn.
    pub going: Arc<AtomicBool>,
    /// Everyone's turns, and this person's.
    pub sends: Arc<Semaphore>,
    pub turn: Arc<Semaphore>,
}

fn refuse(d: &Direct, job: &Job, why: Stop) {
    d.tell(job.from, job.msg, &job.id, wire::refusal(&job.sid, why));
}

/// Whether it all went out.
pub(super) async fn run(d: &Arc<Direct>, mut job: Job) -> bool {
    // The file as it was sent, and its pieces' hashes, before any of the
    // work of a connection.
    let why = match std::fs::metadata(&job.path) {
        Ok(m) if m.len() == job.size => None,
        Ok(_) => Some(Stop::Changed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Stop::Gone),
        Err(_) => Some(Stop::Unreadable),
    };
    if let Some(why) = why {
        refuse(d, &job, why);
        return false;
    }
    let count = hash::pieces(job.size, job.piece);
    let list = std::fs::read(&job.hashes)
        .ok()
        .and_then(|b| hash::from_bytes(&b))
        .filter(|l| l.len() as u64 == count && hash::root(job.size, job.piece, l) == job.root);
    let Some(list) = list else {
        refuse(d, &job, Stop::Unreadable);
        return false;
    };
    let ice = d.ice().await;
    let relay = Relay::start(ice.as_ref()).await;
    let sent = match Link::new(ice.as_ref(), &relay.servers()).await {
        Ok(mut link) => {
            let sent = send(d, &mut job, &mut link, Arc::new(list)).await;
            if let Err(e) = &sent {
                crate::logln!("direto: o envio para {} parou: {e}", job.from);
            }
            link.close().await;
            sent.is_ok()
        }
        Err(e) => {
            crate::logln!("direto: sem conexão para {}: {e}", job.from);
            refuse(d, &job, Stop::Busy);
            false
        }
    };
    relay.close().await;
    sent
}

/// What the other side says on the channel, when it is JSON.
fn said(data: &[u8]) -> Option<Said> {
    serde_json::from_slice::<Value>(data)
        .ok()
        .as_ref()
        .and_then(wire::read)
}

async fn say(channel: &Arc<dyn DataChannel>, v: &Value) -> Result<(), String> {
    channel
        .send_text(&v.to_string())
        .await
        .map_err(|e| e.to_string())
}

/// Sends, unless the send is stopped meanwhile.
async fn put(
    channel: &Arc<dyn DataChannel>,
    halt: &mut watch::Receiver<Halt>,
    msg: BytesMut,
) -> Result<(), Ended> {
    tokio::select! {
        r = channel.send(msg) => r.map_err(|e| Ended::Fell(e.to_string())),
        _ = halt.changed() => Err(Ended::Halted),
    }
}

/// Why a send ended before it was all out.
enum Ended {
    Fell(String),
    Halted,
    /// Said why to the other side (wire::stop).
    Said(Stop),
}

async fn send(
    d: &Arc<Direct>,
    job: &mut Job,
    link: &mut Link,
    list: Arc<Vec<Hash>>,
) -> Result<(), String> {
    let channel = link.channel.clone();
    let ended = transfer(d, job, link, &channel, list).await;
    let why = match ended {
        Ok(()) => {
            linger(job, link, &channel, LINGER).await;
            return Ok(());
        }
        Err(Ended::Fell(e)) => return Err(e),
        Err(Ended::Halted) => match *job.halt.borrow() {
            // No longer shared.
            Halt::Cancel => Stop::Gone,
            // Signing out or quitting: back when I am.
            _ => Stop::Offline,
        },
        Err(Ended::Said(why)) => why,
    };
    if say(&channel, &wire::stop(why)).await.is_ok() {
        linger(job, link, &channel, LINGER_STOP).await;
    }
    Err(format!("parou: {}", why.wire()))
}

/// Waits (at most `most`) for the other side to close, so what is still on
/// its way gets there.
async fn linger(job: &mut Job, link: &mut Link, channel: &Arc<dyn DataChannel>, most: Duration) {
    let until = tokio::time::sleep(most);
    tokio::pin!(until);
    loop {
        tokio::select! {
            ev = channel.poll() => {
                if matches!(ev, Some(DataChannelEvent::OnClose) | None) {
                    return;
                }
            }
            ev = link.events.recv() => {
                if matches!(
                    ev,
                    Some(LinkEv::State(
                        RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                    )) | None
                ) {
                    return;
                }
            }
            () = &mut until => return,
            _ = job.halt.changed() => return,
        }
    }
}

/// Everyone's turn and this person's, as soon as there are both.
async fn both(
    turn: Arc<Semaphore>,
    sends: Arc<Semaphore>,
) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
    let mine = turn.acquire_owned().await.ok()?;
    let all = sends.acquire_owned().await.ok()?;
    Some((mine, all))
}

async fn transfer(
    d: &Arc<Direct>,
    job: &mut Job,
    link: &mut Link,
    channel: &Arc<dyn DataChannel>,
    list: Arc<Vec<Hash>>,
) -> Result<(), Ended> {
    let (to, msg, id, sid) = (job.from, job.msg, job.id.clone(), job.sid.clone());
    let tell = |data: Value| d.tell(to, msg, &id, wire::signal(&sid, data));
    let fell = |why: &str| Ended::Fell(why.to_owned());
    let answer = link.answer(&job.offer).await.map_err(Ended::Fell)?;
    tell(answer);
    // Until they ask for it.
    let deadline = tokio::time::Instant::now() + SETUP;
    let (from, hashes) = loop {
        tokio::select! {
            ev = link.events.recv() => match ev {
                Some(LinkEv::Candidate(c)) => tell(link.candidate(c).await),
                Some(LinkEv::State(
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed,
                )) | None => return Err(fell("a conexão caiu")),
                Some(LinkEv::State(_)) => {}
            },
            Some(data) = job.signals.recv() => {
                link.remote(&data).await.map_err(Ended::Fell)?;
            }
            ev = channel.poll() => match ev {
                Some(DataChannelEvent::OnMessage(m)) if m.is_string => {
                    if let Some(Said::Get { from, hashes }) = said(&m.data) {
                        break (from, hashes);
                    }
                }
                Some(DataChannelEvent::OnClose) | None => return Err(fell("o canal fechou")),
                Some(_) => {}
            },
            () = tokio::time::sleep_until(deadline) => return Err(fell("não pediram o arquivo")),
            _ = job.halt.changed() => return Err(Ended::Halted),
        }
    };
    let count = hash::pieces(job.size, job.piece);
    if from > count {
        return Err(Ended::Said(Stop::Corrupt));
    }
    // Their turn: at once when there is one, otherwise they hear they wait.
    let now = (
        job.turn.clone().try_acquire_owned(),
        job.sends.clone().try_acquire_owned(),
    );
    let _turn = match now {
        (Ok(mine), Ok(all)) => (mine, all),
        _ => {
            drop(now);
            say(channel, &wire::queued()).await.map_err(Ended::Fell)?;
            let wait = both(job.turn.clone(), job.sends.clone());
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    got = &mut wait => match got {
                        Some(turn) => break turn,
                        None => return Err(fell("sem vez")),
                    },
                    ev = channel.poll() => {
                        if matches!(ev, Some(DataChannelEvent::OnClose) | None) {
                            return Err(fell("desistiram"));
                        }
                    }
                    ev = link.events.recv() => match ev {
                        Some(LinkEv::Candidate(c)) => tell(link.candidate(c).await),
                        Some(LinkEv::State(
                            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed,
                        )) | None => return Err(fell("a conexão caiu")),
                        Some(LinkEv::State(_)) => {}
                    },
                    Some(data) = job.signals.recv() => {
                        let _ = link.remote(&data).await;
                    }
                    _ = job.halt.changed() => return Err(Ended::Halted),
                }
            }
        }
    };
    job.going.store(true, Ordering::Relaxed);
    d.changed();
    if hashes {
        let bytes = hash::to_bytes(&list);
        for (i, c) in bytes.chunks(wire::CHUNK).enumerate() {
            let at = (i * wire::CHUNK) as u64;
            put(channel, &mut job.halt, wire::chunk(wire::HASHES, at, c)).await?;
        }
    }
    // The file, from the piece they asked for.
    let (tx, mut rx) = mpsc::channel::<Block>(AHEAD);
    let (path, size, piece) = (job.path.clone(), job.size, job.piece);
    let reader = tokio::task::spawn_blocking(move || read(&path, size, piece, from, &list, &tx));
    let result = loop {
        tokio::select! {
            block = rx.recv() => match block {
                Some(Block::Data(offset, bytes)) => {
                    if let Err(e) = send_block(channel, &mut job.halt, &job.sent, offset, &bytes).await {
                        break Err(e);
                    }
                }
                Some(Block::Changed) => break Err(Ended::Said(Stop::Changed)),
                Some(Block::Unreadable(e)) => {
                    crate::logln!("direto: não consegui ler {}: {e}", job.path.display());
                    break Err(Ended::Said(Stop::Unreadable));
                }
                None => break Ok(()),
            },
            ev = channel.poll() => {
                if matches!(ev, Some(DataChannelEvent::OnClose) | None) {
                    break Err(fell("fecharam"));
                }
            }
            ev = link.events.recv() => {
                if matches!(
                    ev,
                    Some(LinkEv::State(
                        RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                    )) | None
                ) {
                    break Err(fell("a conexão caiu"));
                }
            }
            _ = job.halt.changed() => break Err(Ended::Halted),
        }
    };
    // The reader stops at its next block once nobody takes them.
    drop(rx);
    let _ = reader.await;
    result?;
    say(channel, &wire::done()).await.map_err(Ended::Fell)
}

/// One block of the file, in chunks.
async fn send_block(
    channel: &Arc<dyn DataChannel>,
    halt: &mut watch::Receiver<Halt>,
    sent: &AtomicU64,
    offset: u64,
    bytes: &[u8],
) -> Result<(), Ended> {
    for (k, c) in bytes.chunks(wire::CHUNK).enumerate() {
        let at = offset + (k * wire::CHUNK) as u64;
        put(channel, halt, wire::chunk(wire::DATA, at, c)).await?;
        sent.fetch_add(c.len() as u64, Ordering::Relaxed);
    }
    Ok(())
}

/// What the reading thread hands over.
enum Block {
    /// Bytes of the file, from this offset.
    Data(u64, Bytes),
    /// A piece did not match its hash: the file changed since it was sent.
    Changed,
    Unreadable(String),
}

/// Reads the file from piece `from` to the end, a block at a time, each
/// piece checked before its last block goes.
fn read(
    path: &std::path::Path,
    size: u64,
    piece: u64,
    from: u64,
    list: &[Hash],
    tx: &mpsc::Sender<Block>,
) {
    let fail = |e: std::io::Error| {
        let _ = tx.blocking_send(Block::Unreadable(e.to_string()));
    };
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    if let Err(e) = file.seek(SeekFrom::Start(from * piece)) {
        return fail(e);
    }
    let mut h = Sha256::new();
    for i in from..hash::pieces(size, piece) {
        let mut left = hash::piece_len(size, piece, i);
        let mut offset = i * piece;
        while left > 0 {
            let n = left.min(hash::BLOCK as u64) as usize;
            let mut buf = vec![0u8; n];
            if let Err(e) = file.read_exact(&mut buf) {
                return fail(e);
            }
            h.update(&buf);
            left -= n as u64;
            if left == 0 {
                let got: Hash = h.finalize_reset().into();
                if list.get(i as usize) != Some(&got) {
                    let _ = tx.blocking_send(Block::Changed);
                    return;
                }
            }
            if tx
                .blocking_send(Block::Data(offset, Bytes::from(buf)))
                .is_err()
            {
                return;
            }
            offset += n as u64;
        }
    }
}

//! Sending a file of mine to one person who asked: the answer to their
//! offer, their turn, the pieces' hashes if they want them, then the file
//! from the piece they asked for.
//!
//! A thread reads the file a block at a time, a few blocks ahead, and checks
//! each piece against its hash before its last block goes: a file changed
//! since it was sent stops (wire::Stop::Changed) without a whole piece of it
//! going out. The channel's send waits once 8 MiB are on their way and not
//! yet received (link.rs), which paces everything to the connection.
//! Through the relay too: Cloudflare's TURN lets a few seconds at 34 MB/s
//! through, then drops what passes its limit, and BBR settles under it, at
//! 17 to 19 MB/s (build/medidas/rele-sem-teto.md). The 5.5 MB/s cap it had
//! before BBRv3 made it three times slower.

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

use super::fetch::{SETUP, STALL};
use super::hash::{self, Hash};
use super::link::{Link, LinkEv};
use super::meter::{Gauge, Meter, Readings};
use super::wire::{self, Said, Stop};
use super::{Direct, Halt};
use crate::voice::relay::Relay;

/// Once everything is out, how long the other side has to say it has it
/// (by closing): what is still on its way must not be cut off. The clock
/// starts once the connection holds nothing of mine unacknowledged; until
/// then the wait lasts as long as that keeps going down (`linger`). Handed
/// over is not delivered: up to SEND_BUFFER (8 MiB, direct/link.rs) can
/// still wait in the connection, which a 1 Mbit/s upload takes 67 s to
/// send. Measured from the last block handed over, 30 s closed a 12 MiB
/// send at 1 Mbit/s with 4.7 MiB still queued, and the attempt after it,
/// 8 MiB to go, could not get one whole 4 MiB piece there in 30 s either.
const LINGER: Duration = Duration::from_secs(30);

/// How often the final wait looks at what is still unacknowledged.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

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
    /// How the connection goes, for the report (meter.rs).
    pub readings: Readings,
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

/// Sends, unless the send is stopped meanwhile. A send held STALL (the
/// connection's buffer full and nothing of it acknowledged) is a connection
/// gone, whatever ICE says: the turn goes to whoever is next.
async fn put(
    channel: &Arc<dyn DataChannel>,
    halt: &mut watch::Receiver<Halt>,
    msg: BytesMut,
) -> Result<(), Ended> {
    tokio::select! {
        r = channel.send(msg) => r.map_err(|e| Ended::Fell(e.to_string())),
        _ = halt.changed() => Err(Ended::Halted),
        () = tokio::time::sleep(STALL) => Err(Ended::Fell("nada andou em 60 s".into())),
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
    let meter = Meter::start(
        "envio",
        job.readings.clone(),
        link.peer(),
        job.sent.clone(),
        Some(d.measurements()),
    );
    let ended = transfer(d, job, link, &channel, list, &meter.gauge).await;
    let name = job
        .path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let what = format!("{name} para {}", job.from);
    let ended = match ended {
        Ok(()) => {
            // Measured until it is all there, not only handed over.
            let heard = linger(job, link, &channel, LINGER, true).await;
            meter.finish(what).await;
            // They stopped before it all got there (paused, say), or the
            // rest stopped getting there: not a whole send. A close counts
            // as one, as an older app closes once it has it all, saying
            // nothing.
            match heard {
                Heard::Bye => return Err("pararam de baixar antes do fim".into()),
                Heard::Stalled => return Err("o fim não chegou em 60 s".into()),
                Heard::Got | Heard::Closed => return Ok(()),
            }
        }
        Err(e) => {
            meter.finish(what).await;
            e
        }
    };
    let why = match ended {
        Ended::Fell(e) => return Err(e),
        Ended::Halted => match *job.halt.borrow() {
            // No longer shared.
            Halt::Cancel => Stop::Gone,
            // Signing out or quitting: back when I am.
            _ => Stop::Offline,
        },
        Ended::Said(why) => why,
    };
    if say(&channel, &wire::stop(why)).await.is_ok() {
        linger(job, link, &channel, LINGER_STOP, false).await;
    }
    Err(format!("parou: {}", why.wire()))
}

/// How a wait for the other side ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Heard {
    /// They have it all (wire::got).
    Got,
    /// They stopped first (wire::bye).
    Bye,
    /// The channel or the connection closed, or the wait ran out.
    Closed,
    /// What was still on its way stopped getting there (nothing of it
    /// acknowledged for STALL): the rest never arrived.
    Stalled,
}

/// Waits for the other side to say it has it all, or to close, so what is
/// still on its way gets there: at most `most`, or, `delivering` (the whole
/// file was handed over), `most` from when nothing of mine is left
/// unacknowledged, and until then for as long as that keeps going down
/// (STALL without it is a connection gone).
async fn linger(
    job: &mut Job,
    link: &mut Link,
    channel: &Arc<dyn DataChannel>,
    most: Duration,
    delivering: bool,
) -> Heard {
    let left = || async { channel.outstanding_bytes().await.unwrap_or(0) };
    let mut before = if delivering { left().await } else { 0 };
    let wait = |bytes: usize| if bytes == 0 { most } else { STALL };
    let until = tokio::time::sleep(wait(before));
    tokio::pin!(until);
    let mut look = tokio::time::interval(PROGRESS_EVERY);
    look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = look.tick(), if delivering && before > 0 => {
                let now = left().await;
                if now < before {
                    until.as_mut().reset(tokio::time::Instant::now() + wait(now));
                }
                before = now;
            }
            ev = channel.poll() => match ev {
                Some(DataChannelEvent::OnClose) | None => return Heard::Closed,
                Some(DataChannelEvent::OnMessage(m)) if m.is_string => match said(&m.data) {
                    Some(Said::Got) => return Heard::Got,
                    Some(Said::Bye) => return Heard::Bye,
                    _ => {}
                },
                Some(_) => {}
            },
            ev = link.events.recv() => {
                if matches!(
                    ev,
                    Some(LinkEv::State(
                        RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                    )) | None
                ) {
                    return Heard::Closed;
                }
            }
            () = &mut until => {
                return if before > 0 { Heard::Stalled } else { Heard::Closed };
            }
            _ = job.halt.changed() => return Heard::Closed,
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
    gauge: &Gauge,
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
                    ev = channel.poll() => match ev {
                        Some(DataChannelEvent::OnClose) | None => return Err(fell("desistiram")),
                        Some(DataChannelEvent::OnMessage(m))
                            if m.is_string && said(&m.data) == Some(Said::Bye) =>
                        {
                            return Err(fell("desistiram"));
                        }
                        Some(_) => {}
                    },
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
    // The transfer's own time starts with its turn.
    gauge.flowing();
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
    // Since when nothing was handed to the connection: waiting on the disk
    // (the reader). Handing it over waits on the connection when its
    // buffer is full.
    let mut idle = std::time::Instant::now();
    let result = loop {
        tokio::select! {
            block = rx.recv() => match block {
                Some(Block::Data(offset, bytes)) => {
                    gauge.waited_on_disk(idle.elapsed());
                    let handing = std::time::Instant::now();
                    let sent =
                        send_block(channel, &mut job.halt, &job.sent, offset, &bytes).await;
                    gauge.waited_on_net(handing.elapsed());
                    idle = std::time::Instant::now();
                    if let Err(e) = sent {
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
            ev = channel.poll() => match ev {
                Some(DataChannelEvent::OnClose) | None => break Err(fell("fecharam")),
                // They stopped (wire::bye): nothing more goes.
                Some(DataChannelEvent::OnMessage(m))
                    if m.is_string && said(&m.data) == Some(Said::Bye) =>
                {
                    break Err(fell("pararam de baixar"));
                }
                Some(_) => {}
            },
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

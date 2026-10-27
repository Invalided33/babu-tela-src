//! The public record of keys (key transparency, as Signal's "automatic key
//! verification" and the IETF's keytrans): every key an account publishes,
//! and every version of the app published, goes into one list on the
//! server that only grows, and every app keeps a copy of all of it.
//!
//! - Nobody gets a key that is not in the list: a key the server hands out
//!   for someone must be their newest entry (mod.rs, `listed`).
//! - Everyone watches their own entries: one for my account that is not a
//!   key of mine means someone published in my name (`mine` in Saved).
//! - The apps compare the list as each one has it, in every message and
//!   every signal they exchange (the size and the hash chained over all of
//!   it, `Head`): a server that showed one person a list with a false key
//!   and everyone else another is caught as soon as those lists meet.
//!
//! The list is small enough for each app to hold whole (an entry is 57
//! bytes), so there is no need for the trees and proofs a list of millions
//! needs: an app takes only what comes after what it has, and checks that
//! the numbers follow on.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::prim;

const DS_HEAD: &[u8] = b"babutela/e2e/v1/registro";
const DS_KEY: &[u8] = b"babutela/e2e/v1/registro/chave";
const DS_RELEASE: &[u8] = b"babutela/e2e/v1/registro/versao";

/// The start of the file, and its version.
const MAGIC: &[u8; 8] = b"BTREG01\n";
const ENTRY: usize = 8 + 1 + 8 + 32 + 8;

/// One entry, as the server numbers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub seq: u64,
    /// b'k': an account's keys; b'r': a version of the app.
    pub kind: u8,
    pub uid: i64,
    pub leaf: [u8; 32],
    /// When the server took it (ms).
    pub at: i64,
}

impl Entry {
    fn bytes(&self) -> [u8; ENTRY] {
        let mut out = [0u8; ENTRY];
        out[..8].copy_from_slice(&self.seq.to_be_bytes());
        out[8] = self.kind;
        out[9..17].copy_from_slice(&self.uid.to_be_bytes());
        out[17..49].copy_from_slice(&self.leaf);
        out[49..].copy_from_slice(&self.at.to_be_bytes());
        out
    }

    fn from_bytes(b: &[u8]) -> Option<Entry> {
        Some(Entry {
            seq: u64::from_be_bytes(b.get(..8)?.try_into().ok()?),
            kind: *b.get(8)?,
            uid: i64::from_be_bytes(b.get(9..17)?.try_into().ok()?),
            leaf: b.get(17..49)?.try_into().ok()?,
            at: i64::from_be_bytes(b.get(49..57)?.try_into().ok()?),
        })
    }
}

/// What two apps compare: how many entries each has, and the hash chained
/// over them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Head {
    pub n: u64,
    pub r: String,
}

/// An account's keys as an entry: its id, identity, X-Wing key and when it
/// was made (the server computes the same, crates/worker/src/e2e.rs).
pub fn key_leaf(uid: i64, identity: &[u8; 32], kem: &[u8], at: i64) -> [u8; 32] {
    prim::sha256(&[DS_KEY, &uid.to_be_bytes(), identity, kem, &at.to_be_bytes()])
}

/// A version of the app as an entry (what its signature covers, update.rs).
pub fn release_leaf(version: &str, file: &str, sha256: &str) -> [u8; 32] {
    let sha = sha256.to_ascii_lowercase();
    prim::sha256(&[
        DS_RELEASE,
        &(version.len() as u32).to_be_bytes(),
        version.as_bytes(),
        &(file.len() as u32).to_be_bytes(),
        file.as_bytes(),
        &(sha.len() as u32).to_be_bytes(),
        sha.as_bytes(),
    ])
}

/// Why entries from the server did not go on this PC's copy.
#[derive(Debug, PartialEq, Eq)]
pub enum Broken {
    /// The server changed an entry this copy already had (its number).
    Changed(u64),
    /// They do not follow on from this copy's last one.
    OutOfOrder,
    /// The copy could not be written.
    Disk(String),
}

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Broken::Changed(seq) => write!(
                f,
                "o servidor mudou a entrada {seq} do registro público de chaves"
            ),
            Broken::OutOfOrder => f.write_str("o registro público de chaves veio fora de ordem"),
            Broken::Disk(e) => write!(f, "não deu para guardar o registro público: {e}"),
        }
    }
}

/// This PC's copy of the list.
pub struct Log {
    path: PathBuf,
    entries: Vec<Entry>,
    /// heads[i]: the hash over the first i entries.
    heads: Vec<[u8; 32]>,
    /// Each account's newest key entry (an index into `entries`).
    newest: HashMap<i64, usize>,
}

impl Log {
    /// The copy in `path`, as far as it is whole (an entry cut short by a
    /// crash, or one out of order, ends it there).
    pub fn load(path: &Path) -> Log {
        let mut log = Log {
            path: path.to_path_buf(),
            entries: Vec::new(),
            heads: vec![prim::sha256(&[DS_HEAD])],
            newest: HashMap::new(),
        };
        if let Ok(bytes) = std::fs::read(path)
            && let Some(rest) = bytes.strip_prefix(MAGIC.as_slice())
        {
            for chunk in rest.as_chunks::<ENTRY>().0 {
                match Entry::from_bytes(chunk) {
                    Some(e) if e.seq == log.size() + 1 => log.push(e),
                    _ => break,
                }
            }
        }
        log
    }

    fn push(&mut self, e: Entry) {
        let last = *self.heads.last().unwrap_or(&[0; 32]);
        self.heads.push(prim::sha256(&[&last, &e.bytes()]));
        if e.kind == b'k' {
            self.newest.insert(e.uid, self.entries.len());
        }
        self.entries.push(e);
    }

    pub fn size(&self) -> u64 {
        self.entries.len() as u64
    }

    /// The hash over the first `n` entries, if this copy has that many.
    pub fn head_at(&self, n: u64) -> Option<[u8; 32]> {
        self.heads.get(usize::try_from(n).ok()?).copied()
    }

    pub fn head(&self) -> Head {
        Head {
            n: self.size(),
            r: prim::b64(&self.head_at(self.size()).unwrap_or_default()),
        }
    }

    /// Whether `h`, another app's copy, agrees with this one: Some(true) or
    /// Some(false) when this copy has that many entries, None when it must
    /// take the newer ones first.
    pub fn agrees(&self, h: &Head) -> Option<bool> {
        let mine = self.head_at(h.n)?;
        Some(prim::unb64_n::<32>(&h.r).is_some_and(|r| prim::same(&r, &mine)))
    }

    /// The newest key entry of `uid`.
    pub fn newest_key(&self, uid: i64) -> Option<&Entry> {
        self.newest.get(&uid).and_then(|i| self.entries.get(*i))
    }

    pub fn has_release(&self, leaf: &[u8; 32]) -> bool {
        self.entries
            .iter()
            .any(|e| e.kind == b'r' && &e.leaf == leaf)
    }

    /// Entries after the last one this copy has: they must follow on, one by
    /// one. Written to disk before they count. Returns the ones taken.
    pub fn extend(&mut self, list: Vec<Entry>) -> Result<Vec<Entry>, Broken> {
        let mut taken = Vec::new();
        for e in list {
            if e.seq <= self.size() {
                // One this copy has: it must be the same.
                let had = self
                    .entries
                    .get(usize::try_from(e.seq).unwrap_or(0).wrapping_sub(1));
                if had != Some(&e) {
                    return Err(Broken::Changed(e.seq));
                }
                continue;
            }
            if e.seq != self.size() + 1 || !(e.kind == b'k' || e.kind == b'r') {
                return Err(Broken::OutOfOrder);
            }
            taken.push(e.clone());
            self.push(e);
        }
        if !taken.is_empty() {
            self.write(&taken).map_err(Broken::Disk)?;
        }
        Ok(taken)
    }

    fn write(&self, new: &[Entry]) -> Result<(), String> {
        let fresh = !self.path.exists();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(MAGIC.len() + new.len() * ENTRY);
        if fresh {
            buf.extend_from_slice(MAGIC);
        }
        for e in new {
            buf.extend_from_slice(&e.bytes());
        }
        f.write_all(&buf).map_err(|e| e.to_string())?;
        f.sync_data().map_err(|e| e.to_string())
    }
}

/// Where an account's copy lives, next to its keys.
pub fn path(dir: &Path, uid: i64) -> PathBuf {
    dir.join(format!("registro-{uid}.bin"))
}

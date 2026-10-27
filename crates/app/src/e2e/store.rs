//! What this PC keeps of the keys, one file per account, encrypted with the
//! Windows user's key (DPAPI, as session.bin): `chaves-<conta>.bin` in the
//! profile folder. Lose it (and every other PC of the account) and what
//! was sent to the account cannot be read again: nobody else has the keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::keys::{Account, KemKey, Pin, b64_32};
use super::msg::{InSession, OutSession};
use super::prim::Identity;

#[derive(Default, Serialize, Deserialize)]
pub struct Saved {
    pub v: u32,
    pub uid: i64,
    #[serde(with = "b64_32")]
    pub identity: [u8; 32],
    pub kem: Vec<KemKey>,
    /// Others' identities, as first seen (or verified).
    #[serde(default)]
    pub pins: HashMap<String, Pin>,
    /// Identities of theirs accepted before the current one: what they
    /// signed then still checks.
    #[serde(default)]
    pub older: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub out: Vec<OutSession>,
    /// Sessions I can read, by their id (base64).
    #[serde(default)]
    pub inb: HashMap<String, InSession>,
    /// The record entries (record.rs) of the keys my account published, as
    /// far as this PC knows (base64): any other entry under my account is
    /// someone publishing in my name.
    #[serde(default)]
    pub mine: Vec<String>,
    /// The ratchet of the keys of the day (keys.rs, `eph_next`): the seed of
    /// the oldest day still kept, and that day. Every PC of the account
    /// has the same (provision.rs), and makes the same keys from it.
    #[serde(
        default,
        with = "super::keys::b64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub eph_seed: Option<[u8; 32]>,
    #[serde(default)]
    pub eph_day: i64,
    /// The servers this PC has seen with members' certificates
    /// (members.rs): from then on only those they vouch for get keys, even
    /// if the server stops showing certificates.
    #[serde(default)]
    pub strict: Vec<i64>,
    /// Each server's owner, by the server's id (members.rs): as the signed
    /// invite this PC came in with names them, or else as the server showed
    /// them the first time. A server's owner never changes, so one it names
    /// later roots nothing here.
    #[serde(default)]
    pub owners: HashMap<String, i64>,
    /// Only in what goes to a new PC: the old one's copy of the record, for
    /// the new one to compare with its own (never kept on disk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<super::record::Head>,
}

impl Saved {
    pub fn account(&self) -> Account {
        let mut a = Account {
            uid: self.uid,
            identity: Identity::from_seed(self.identity),
            kem: self.kem.clone(),
            eph: Vec::new(),
        };
        if let Some(seed) = self.eph_seed {
            let today = super::keys::day_of(chrono::Utc::now().timestamp_millis());
            a.set_eph(seed, self.eph_day, today + 1);
        }
        a
    }

    /// Moves the ratchet of the keys of the day on to the oldest day still
    /// kept (`EPH_KEEP_DAYS` back), dropping the seeds before it, and makes
    /// one for an account that had none. True when anything changed.
    pub fn roll_eph(&mut self, today: i64) -> bool {
        let oldest = today - super::keys::EPH_KEEP_DAYS;
        let Some(mut seed) = self.eph_seed else {
            self.eph_seed = Some(super::prim::random());
            self.eph_day = today;
            return true;
        };
        if self.eph_day >= oldest {
            return false;
        }
        while self.eph_day < oldest {
            seed = super::keys::eph_next(&seed);
            self.eph_day += 1;
        }
        self.eph_seed = Some(seed);
        true
    }
}

pub fn path(dir: &Path, uid: i64) -> PathBuf {
    dir.join(format!("chaves-{uid}.bin"))
}

pub fn load(dir: &Path, uid: i64) -> Option<Saved> {
    let enc = std::fs::read(path(dir, uid)).ok()?;
    let plain = crate::winsys::unprotect(&enc).ok()?;
    let saved: Saved = serde_json::from_slice(&plain).ok()?;
    (saved.v == 1 && saved.uid == uid).then_some(saved)
}

/// Written whole to a new file, then put in place: a crash in the middle
/// leaves the old one.
pub fn save(dir: &Path, saved: &Saved) -> std::io::Result<()> {
    let plain = serde_json::to_vec(saved).map_err(std::io::Error::other)?;
    let enc = crate::winsys::protect(&plain)?;
    let file = path(dir, saved.uid);
    let tmp = file.with_extension("novo");
    std::fs::write(&tmp, enc)?;
    std::fs::rename(&tmp, &file)
}

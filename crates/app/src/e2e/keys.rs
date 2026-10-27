//! An account's keys, and the identities of the others as this PC first saw
//! them.
//!
//! Each account has one identity (Ed25519) and one X-Wing key at a time,
//! the same on every PC it signs in on: a PC gets them from another of the
//! person's PCs (provision.rs), never from the server. What the server
//! keeps is the public half and a certificate: the identity's signature
//! over the account's id and its X-Wing key. A key the server makes up has
//! no such signature, and one it swaps whole (identity and all) is not the
//! identity the others pinned the first time they saw it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::prim::{self, Identity, KemPair};

/// Labels: a signature or a hash made for one of these is never taken for
/// another.
const DS_CERT: &[u8] = b"babutela/e2e/v1/cert";
const DS_NUMBER: &[u8] = b"babutela/e2e/v1/numero";

/// An X-Wing key of mine. The newest one is what others seal for; the older
/// ones stay, to open what was sealed for them.
#[derive(Clone, Serialize, Deserialize)]
pub struct KemKey {
    #[serde(with = "b64_32")]
    pub secret: [u8; 32],
    #[serde(with = "b64_vec")]
    pub public: Vec<u8>,
    /// When it was made (ms), in its certificate.
    pub at: i64,
    /// The identity's signature over it (`cert_message`).
    #[serde(with = "b64_vec")]
    pub sig: Vec<u8>,
}

/// The id of an X-Wing key: the first bytes of its hash.
pub fn kid(public: &[u8]) -> [u8; 8] {
    let h = prim::sha256(&[public]);
    let mut out = [0u8; 8];
    out.copy_from_slice(&h[..8]);
    out
}

fn cert_message(uid: i64, kem: &[u8], at: i64) -> Vec<u8> {
    let mut m = Vec::with_capacity(DS_CERT.len() + 16 + kem.len());
    m.extend_from_slice(DS_CERT);
    m.extend_from_slice(&uid.to_be_bytes());
    m.extend_from_slice(&at.to_be_bytes());
    m.extend_from_slice(kem);
    m
}

/// My account's keys.
pub struct Account {
    pub uid: i64,
    pub identity: Identity,
    /// Oldest first; the last is the current one.
    pub kem: Vec<KemKey>,
    /// The keys of the day (`eph_day_keys`) still kept, for conversations
    /// whose messages disappear (`at` is the day).
    pub eph: Vec<KemKey>,
}

const DS_EPH: &[u8] = b"babutela/e2e/v1/chave-do-dia";
const DS_EPH_NEXT: &[u8] = b"babutela/e2e/v1/catraca-do-dia";

/// How many days a key of the day is kept after its day: the longest a
/// message lasts (7) and one more. Then it is gone from every PC, and what
/// was sealed for it cannot be opened again, by anyone (forward secrecy).
pub const EPH_KEEP_DAYS: i64 = 8;

/// The day (UTC) of a time in ms.
pub fn day_of(ms: i64) -> i64 {
    ms.div_euclid(86_400_000)
}

/// The ratchet of the keys of the day: each day's seed is the hash of the
/// day before's, so a PC that keeps only the oldest seed it still needs
/// makes the same keys as the account's other PCs, and nobody can go back
/// to an earlier day from it.
pub fn eph_next(seed: &[u8; 32]) -> [u8; 32] {
    prim::sha256(&[DS_EPH_NEXT, seed])
}

/// The X-Wing key of the day whose seed is `seed`.
pub fn eph_pair(seed: &[u8; 32]) -> Option<KemPair> {
    prim::kem_from_seed(&prim::key32(&[], seed, &[DS_EPH]))
}

fn eph_message(uid: i64, day: i64, kem: &[u8]) -> Vec<u8> {
    [DS_EPH, &uid.to_be_bytes(), &day.to_be_bytes(), kem].concat()
}

/// Someone's key of a day, as the readers of a conversation whose messages
/// disappear hand it out (`{d, kem, sig}`), checked against their identity.
pub fn parse_eph(uid: i64, identity: &[u8; 32], v: &Value) -> Option<(i64, Vec<u8>)> {
    let day = v.get("d")?.as_i64()?;
    let kem = prim::unb64(v.get("kem")?.as_str()?)?;
    let sig = prim::unb64(v.get("sig")?.as_str()?)?;
    (kem.len() == prim::KEM_PUBLIC && prim::verify(identity, &eph_message(uid, day, &kem), &sig))
        .then_some((day, kem))
}

impl Account {
    /// A new identity for `uid`, with its first X-Wing key.
    pub fn generate(uid: i64, now: i64) -> Account {
        let identity = Identity::generate();
        let mut a = Account {
            uid,
            identity,
            kem: Vec::new(),
            eph: Vec::new(),
        };
        a.add_kem(now);
        a
    }

    /// The keys of the days from `day` (whose seed is `seed`) to `last`.
    pub fn set_eph(&mut self, mut seed: [u8; 32], day: i64, last: i64) {
        self.eph.clear();
        for d in day..=last {
            if let Some(KemPair { secret, public }) = eph_pair(&seed) {
                let sig = self
                    .identity
                    .sign(&eph_message(self.uid, d, &public))
                    .to_vec();
                self.eph.push(KemKey {
                    secret,
                    public,
                    at: d,
                    sig,
                });
            }
            seed = eph_next(&seed);
        }
    }

    /// My keys of these days, as the server hands them out.
    pub fn eph_bundle(&self, days: &[i64]) -> Vec<Value> {
        self.eph
            .iter()
            .filter(|k| days.contains(&k.at))
            .map(|k| json!({ "d": k.at, "kem": prim::b64(&k.public), "sig": prim::b64(&k.sig) }))
            .collect()
    }

    /// My key of `day`, if kept.
    pub fn eph_of(&self, day: i64) -> Option<&KemKey> {
        self.eph.iter().find(|k| k.at == day)
    }

    /// Whether the key with this id is one of my keys of the day.
    pub fn is_eph(&self, id: &[u8; 8]) -> bool {
        self.eph.iter().any(|k| &kid(&k.public) == id)
    }

    /// A new X-Wing key, signed: from now on others seal for it.
    pub fn add_kem(&mut self, now: i64) {
        let KemPair { secret, public } = prim::kem_generate();
        let sig = self
            .identity
            .sign(&cert_message(self.uid, &public, now))
            .to_vec();
        self.kem.push(KemKey {
            secret,
            public,
            at: now,
            sig,
        });
    }

    pub fn current(&self) -> &KemKey {
        self.kem.last().expect("uma conta sempre tem uma chave")
    }

    /// The X-Wing key with this id (the account's, or a key of the day
    /// still kept).
    pub fn kem_key(&self, id: &[u8; 8]) -> Option<&KemKey> {
        self.kem
            .iter()
            .rev()
            .chain(self.eph.iter())
            .find(|k| &kid(&k.public) == id)
    }

    /// What the server keeps for the others: the public halves.
    pub fn bundle(&self) -> Value {
        let k = self.current();
        json!({
            "v": 1,
            "uid": self.uid,
            "id": prim::b64(&self.identity.public()),
            "kem": prim::b64(&k.public),
            "at": k.at,
            "sig": prim::b64(&k.sig),
        })
    }
}

/// Someone's public keys, checked: the X-Wing key is signed by the identity,
/// for this account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub uid: i64,
    pub identity: [u8; 32],
    pub kem: Vec<u8>,
    pub at: i64,
}

impl Bundle {
    /// The bundle in `v`, if it is whole, for `uid`, and its signature holds.
    pub fn parse(uid: i64, v: &Value) -> Option<Bundle> {
        if v.get("v")?.as_i64()? != 1 || v.get("uid")?.as_i64()? != uid {
            return None;
        }
        let identity: [u8; 32] = prim::unb64_n(v.get("id")?.as_str()?)?;
        let kem = prim::unb64(v.get("kem")?.as_str()?)?;
        let at = v.get("at")?.as_i64()?;
        let sig = prim::unb64(v.get("sig")?.as_str()?)?;
        if kem.len() != prim::KEM_PUBLIC
            || !prim::verify(&identity, &cert_message(uid, &kem, at), &sig)
        {
            return None;
        }
        Some(Bundle {
            uid,
            identity,
            kem,
            at,
        })
    }

    pub fn kid(&self) -> [u8; 8] {
        kid(&self.kem)
    }
}

/// How this PC knows someone's identity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pin {
    #[serde(with = "b64_32")]
    pub identity: [u8; 32],
    /// Compared with them outside the app (the security number).
    #[serde(default)]
    pub verified: bool,
    /// A different identity the server now hands out for them, not
    /// accepted yet: nothing is sealed for it until it is.
    #[serde(default, with = "b64_opt", skip_serializing_if = "Option::is_none")]
    pub changed: Option<[u8; 32]>,
}

/// What a bundle the server handed out means for someone already pinned.
#[derive(Debug, PartialEq, Eq)]
pub enum Trust {
    /// First time: pinned now.
    New,
    /// The identity pinned.
    Same,
    /// Another identity: someone reset theirs, or someone is in the middle.
    Changed,
    /// Not the account's newest key in the public record (record.rs):
    /// the server made it up, or keeps a newer one from this PC.
    Unlisted,
}

/// The security number of two people: 60 digits, the same on both sides
/// (each half is one identity, and the halves go in order). Compared out
/// loud or in person, it shows that the identities each app pinned are the
/// ones the other really has.
pub fn security_number(a: (i64, &[u8; 32]), b: (i64, &[u8; 32])) -> String {
    let mut halves = [half(a.0, a.1), half(b.0, b.1)];
    halves.sort();
    halves.concat()
}

/// 30 digits for one identity: SHA-256 repeated 5200 times (as Signal's
/// numbers), in 6 groups of 5.
fn half(uid: i64, identity: &[u8; 32]) -> String {
    let mut h = prim::sha256(&[DS_NUMBER, &uid.to_be_bytes(), identity]);
    for _ in 0..5200 {
        h = prim::sha256(&[&h, identity]);
    }
    h[..30]
        .chunks(5)
        .map(|c| {
            let n = c.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            format!("{:05}", n % 100_000)
        })
        .collect()
}

/// The number in groups of 5, as it is shown.
pub fn grouped(number: &str) -> String {
    number
        .as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) mod b64_32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::prim::b64(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        super::prim::unb64_n(&s).ok_or_else(|| serde::de::Error::custom("base64 de 32 bytes"))
    }
}

pub(super) mod b64_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_str(&super::prim::b64(v)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        match s {
            Some(s) => super::prim::unb64_n(&s)
                .map(Some)
                .ok_or_else(|| serde::de::Error::custom("base64 de 32 bytes")),
            None => Ok(None),
        }
    }
}

pub(super) mod b64_vec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::prim::b64(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::prim::unb64(&s).ok_or_else(|| serde::de::Error::custom("base64"))
    }
}

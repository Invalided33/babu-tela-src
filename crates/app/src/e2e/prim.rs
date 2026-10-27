//! The building blocks, and nothing else: every other part of e2e/ goes
//! through these, so the choice of algorithms lives in one place.
//!
//! - Signatures: Ed25519 (ring), from a 32-byte seed.
//! - Key encapsulation: X-Wing (draft-connolly-cfrg-xwing-kem, draft 06),
//!   the hybrid of X25519 and ML-KEM-768 (FIPS 203), from libcrux, whose
//!   ML-KEM is formally verified. A secret it carries stays secret as long
//!   as either of the two holds, so a recording made today cannot be opened
//!   by a quantum computer later.
//! - Key derivation: HKDF-SHA256 (RFC 5869). Encryption: AES-256-GCM. Both
//!   ring, the code under rustls in this app.
//!
//! Every derivation and signature starts with a label of its own (`DS_*` in
//! the other files), so nothing made for one purpose is ever accepted for
//! another.

use libcrux_kem::{Algorithm, Ct, PrivateKey, PublicKey, key_gen};
use rand::rngs::SysRng;
use rand_core::UnwrapErr;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use ring::{digest, hkdf, hmac};

const XWING: Algorithm = Algorithm::XWingKemDraft06;

/// X-Wing's sizes.
pub const KEM_PUBLIC: usize = 1216;
pub const KEM_CIPHERTEXT: usize = 1120;
/// AES-GCM's tag.
pub const TAG: usize = 16;

/// Bytes from the operating system's generator.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    // Without the system's randomness there is no safe key to make, and
    // Windows does not fail here.
    SystemRandom::new()
        .fill(&mut out)
        .expect("o Windows não deu números aleatórios");
    out
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = digest::Context::new(&digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
    out
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// HKDF-SHA256 into `out` (at most 8160 bytes).
pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[&[u8]], out: &mut [u8]) {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    prk.expand(info, Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .expect("tamanho de chave válido");
}

pub fn key32(salt: &[u8], ikm: &[u8], info: &[&[u8]]) -> [u8; 32] {
    let mut out = [0u8; 32];
    hkdf(salt, ikm, info, &mut out);
    out
}

pub fn hmac256(key: &[u8], data: &[&[u8]]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut ctx = hmac::Context::with_key(&key);
    for d in data {
        ctx.update(d);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.sign().as_ref());
    out
}

fn aead_key(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("chave de 32 bytes"))
}

/// Encrypts `data` in place and appends the tag. Each (key, nonce) pair must
/// be used once: the callers either use a key once, or count the nonce.
pub fn seal(key: &[u8; 32], nonce: [u8; 12], aad: &[u8], data: &mut Vec<u8>) {
    aead_key(key)
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), data)
        .expect("tamanho dentro do limite do AES-GCM");
}

/// The plain text, or None when anything was changed (or the key is not
/// the one it was sealed with).
pub fn open(key: &[u8; 32], nonce: [u8; 12], aad: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut buf = data.to_vec();
    let n = aead_key(key)
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut buf,
        )
        .ok()?
        .len();
    buf.truncate(n);
    Some(buf)
}

/// An account's identity: the key that signs everything it says.
pub struct Identity {
    seed: [u8; 32],
    pair: Ed25519KeyPair,
}

impl Identity {
    pub fn generate() -> Identity {
        Identity::from_seed(random())
    }

    pub fn from_seed(seed: [u8; 32]) -> Identity {
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("semente de 32 bytes");
        Identity { seed, pair }
    }

    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn public(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.pair.public_key().as_ref());
        out
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out.copy_from_slice(self.pair.sign(msg).as_ref());
        out
    }
}

pub fn verify(public: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    public.len() == 32
        && sig.len() == 64
        && UnparsedPublicKey::new(&ED25519, public)
            .verify(msg, sig)
            .is_ok()
}

fn rng() -> UnwrapErr<SysRng> {
    UnwrapErr(SysRng)
}

/// An X-Wing key pair: the private key is its 32-byte seed.
pub struct KemPair {
    pub secret: [u8; 32],
    pub public: Vec<u8>,
}

pub fn kem_generate() -> KemPair {
    let (sk, pk) = key_gen(XWING, &mut rng()).expect("o gerador do Windows não falha");
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&sk.encode());
    KemPair {
        secret,
        public: pk.encode(),
    }
}

/// The X-Wing key pair a 32-byte seed makes, the same every time (the
/// account's keys of the day, keys.rs, which each of its PCs makes alike).
pub fn kem_from_seed(seed: &[u8; 32]) -> Option<KemPair> {
    let (sk, pk) = libcrux_kem::key_gen_derand(XWING, seed).ok()?;
    let mut secret = [0u8; 32];
    secret.copy_from_slice(sk.encode().get(..32)?);
    Some(KemPair {
        secret,
        public: pk.encode(),
    })
}

/// A fresh secret for `public`, and the capsule that carries it there.
pub fn encapsulate(public: &[u8]) -> Option<([u8; 32], Vec<u8>)> {
    if public.len() != KEM_PUBLIC {
        return None;
    }
    let pk = PublicKey::decode(XWING, public).ok()?;
    let (ss, ct) = pk.encapsulate(&mut rng()).ok()?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&ss.encode());
    Some((out, ct.encode()))
}

/// The secret in `capsule`. A capsule that was changed gives another
/// secret (ML-KEM's implicit rejection), which then fails to open anything.
pub fn decapsulate(secret: &[u8; 32], capsule: &[u8]) -> Option<[u8; 32]> {
    if capsule.len() != KEM_CIPHERTEXT {
        return None;
    }
    let sk = PrivateKey::decode(XWING, secret).ok()?;
    let ct = Ct::decode(XWING, capsule).ok()?;
    let ss = ct.decapsulate(&sk).ok()?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&ss.encode());
    Some(out)
}

/// Compares in constant time (tags, keys).
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Base64 without padding, URL-safe: what goes in JSON and URLs.
pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .ok()
}

/// A fixed-size value from base64.
pub fn unb64_n<const N: usize>(s: &str) -> Option<[u8; N]> {
    unb64(s)?.try_into().ok()
}

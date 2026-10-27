//! Sealing for one account: bytes only the holder of its X-Wing key opens.
//!
//! The KEM/DEM construction (as HPKE's base mode, RFC 9180): a fresh X-Wing
//! secret for the recipient's key, HKDF over it into a one-time AES-256-GCM
//! key, and the context (who to whom, about what) in both the derivation and
//! the additional data, so a sealed thing moved elsewhere does not open.
//! The signed form adds the sender's identity signature inside: the server
//! can neither read it nor make one up.
//!
//! Layout: the recipient key's id (8 bytes), the capsule (1120), the
//! encrypted text with its tag.

use super::keys::{Account, Bundle};
use super::prim;

const DS_SEAL: &[u8] = b"babutela/e2e/v1/selo";
const DS_SIGNED: &[u8] = b"babutela/e2e/v1/assinado";

const HEAD: usize = 8 + prim::KEM_CIPHERTEXT;

/// Everything that must match on both ends, in one byte string.
fn bind(context: &[u8], from: i64, to: i64, key_id: &[u8; 8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(context.len() + 24);
    b.extend_from_slice(&from.to_be_bytes());
    b.extend_from_slice(&to.to_be_bytes());
    b.extend_from_slice(key_id);
    b.extend_from_slice(&(context.len() as u32).to_be_bytes());
    b.extend_from_slice(context);
    b
}

fn derive(shared: &[u8; 32], capsule: &[u8], public_hash: &[u8; 32], bound: &[u8]) -> [u8; 32] {
    let salt = prim::sha256(&[capsule]);
    prim::key32(&salt, shared, &[DS_SEAL, public_hash, bound])
}

/// `plain` sealed for `to`'s current X-Wing key. `context` says what it is;
/// opening needs the same context and the same sender id.
pub fn seal_to(from: i64, to: &Bundle, context: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
    let key_id = to.kid();
    let (shared, capsule) = prim::encapsulate(&to.kem)?;
    let bound = bind(context, from, to.uid, &key_id);
    let key = derive(&shared, &capsule, &prim::sha256(&[&to.kem]), &bound);
    let mut body = plain.to_vec();
    prim::seal(&key, [0; 12], &[DS_SEAL, &bound].concat(), &mut body);
    let mut out = Vec::with_capacity(HEAD + body.len());
    out.extend_from_slice(&key_id);
    out.extend_from_slice(&capsule);
    out.extend_from_slice(&body);
    Some(out)
}

/// The id of the key something was sealed for.
pub fn key_id(sealed: &[u8]) -> Option<[u8; 8]> {
    sealed.get(..8)?.try_into().ok()
}

/// What `from` sealed for me in `context`, or None.
pub fn open_from(me: &Account, from: i64, context: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < HEAD + prim::TAG {
        return None;
    }
    let key_id = key_id(sealed)?;
    let capsule = &sealed[8..HEAD];
    let key = me.kem_key(&key_id)?;
    let (secret, public) = (&key.secret, &key.public);
    let shared = prim::decapsulate(secret, capsule)?;
    let bound = bind(context, from, me.uid, &key_id);
    let key = derive(&shared, capsule, &prim::sha256(&[public]), &bound);
    prim::open(&key, [0; 12], &[DS_SEAL, &bound].concat(), &sealed[HEAD..])
}

fn signed_message(context: &[u8], from: i64, to: i64, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(DS_SIGNED.len() + context.len() + body.len() + 24);
    m.extend_from_slice(DS_SIGNED);
    m.extend_from_slice(&bind(context, from, to, &[0; 8]));
    m.extend_from_slice(body);
    m
}

/// `body` signed by my identity and sealed for `to`.
pub fn seal_signed(me: &Account, to: &Bundle, context: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    let sig = me
        .identity
        .sign(&signed_message(context, me.uid, to.uid, body));
    let mut plain = Vec::with_capacity(64 + body.len());
    plain.extend_from_slice(&sig);
    plain.extend_from_slice(body);
    seal_to(me.uid, to, context, &plain)
}

/// What `from` (whose identity this PC pinned as `identity`) signed and
/// sealed for me in `context`. None when it does not open or the signature
/// is not theirs.
pub fn open_signed(
    me: &Account,
    from: i64,
    identity: &[u8; 32],
    context: &[u8],
    sealed: &[u8],
) -> Option<Vec<u8>> {
    let plain = open_from(me, from, context, sealed)?;
    if plain.len() < 64 {
        return None;
    }
    let (sig, body) = plain.split_at(64);
    prim::verify(identity, &signed_message(context, from, me.uid, body), sig).then(|| body.to_vec())
}

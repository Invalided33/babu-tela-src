//! How a file sent directly is cut and checked, the way BitTorrent does it:
//! in pieces of the same size (a power of two, 4 MiB and up), each with its
//! SHA-256, and one hash over all of those (the root), which goes on the
//! message. Whoever downloads gets the pieces' hashes from the sender, checks
//! them against the root, and checks each piece as it arrives; the sender
//! checks each piece as it reads it, so a file changed on its disk since it
//! was sent stops instead of going out mixed. A download that stopped goes
//! on from its last checked piece.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

/// The smallest piece: below this, the list of hashes would be the cost.
pub const MIN_PIECE: u64 = 4 << 20;

/// The largest the server takes (worker/src/direct.rs).
pub const MAX_PIECE: u64 = 1 << 30;

/// At most this many pieces (the server's limit too): their hashes, 1 MiB
/// in all, go over the connection before the file.
pub const MAX_PIECES: u64 = 32_768;

/// How much is read from the disk at a time.
pub const BLOCK: usize = 1 << 20;

/// The piece size for a file of `size` bytes: the smallest that keeps it to
/// MAX_PIECES pieces. None past what that allows (32 TiB).
pub fn piece_for(size: u64) -> Option<u64> {
    let mut piece = MIN_PIECE;
    while size.div_ceil(piece) > MAX_PIECES {
        piece *= 2;
        if piece > MAX_PIECE {
            return None;
        }
    }
    Some(piece)
}

pub fn pieces(size: u64, piece: u64) -> u64 {
    size.div_ceil(piece.max(1))
}

/// How long piece `i` is: `piece` bytes, but the last one may be short.
pub fn piece_len(size: u64, piece: u64, i: u64) -> u64 {
    size.saturating_sub(i * piece).min(piece)
}

/// The hash the message carries: over the size, the piece size and every
/// piece's hash, in order.
pub fn root(size: u64, piece: u64, hashes: &[Hash]) -> Hash {
    let mut h = Sha256::new();
    h.update(b"babutela/direto/1");
    h.update(size.to_le_bytes());
    h.update(piece.to_le_bytes());
    for x in hashes {
        h.update(x);
    }
    h.finalize().into()
}

/// The pieces' hashes laid end to end, as they travel and as they are kept.
pub fn to_bytes(hashes: &[Hash]) -> Vec<u8> {
    hashes.concat()
}

pub fn from_bytes(bytes: &[u8]) -> Option<Vec<Hash>> {
    let (hashes, rest) = bytes.as_chunks::<32>();
    rest.is_empty().then(|| hashes.to_vec())
}

pub fn hex(h: &Hash) -> String {
    ::hex::encode(h)
}

/// The hashes of every piece of the file at `path` (`size` bytes long),
/// read from the start. `done` counts the bytes read; `stop` ends it early
/// (an error then).
pub fn hash_file(
    path: &Path,
    size: u64,
    piece: u64,
    done: &AtomicU64,
    stop: &AtomicBool,
) -> std::io::Result<Vec<Hash>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; BLOCK];
    let count = pieces(size, piece);
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut left = piece_len(size, piece, i);
        let mut h = Sha256::new();
        while left > 0 {
            if stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::other(super::CANCELLED));
            }
            let n = (left as usize).min(BLOCK);
            file.read_exact(&mut buf[..n])?;
            h.update(&buf[..n]);
            left -= n as u64;
            done.fetch_add(n as u64, Ordering::Relaxed);
        }
        out.push(h.finalize().into());
    }
    // A file that grew while it was read is not the one measured.
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(std::io::Error::other(
            "o arquivo mudou enquanto era preparado",
        ));
    }
    Ok(out)
}

//! Files that go to the server: encrypted on this PC before they go up, so
//! R2 holds bytes nobody can read without the message.
//!
//! Each file has a key of its own (it travels inside the encrypted
//! message). The file is padded (Padmé, as messages are) and cut in blocks
//! of 64 KiB; each block is AES-256-GCM with its number as the nonce and a
//! flag on the last one, and the padded length in the additional data
//! (the STREAM construction of Hoang, Reyhanitabar, Rogaway and Vizár):
//! blocks cannot be reordered, dropped, cut short or mixed with another
//! file's, and a player can still read from the middle, a block at a time.

use super::prim;

/// Plain bytes in one block.
pub const BLOCK: u64 = 64 * 1024;
/// One block on the server.
pub const SEALED_BLOCK: u64 = BLOCK + prim::TAG as u64;

const DS_FILE: &[u8] = b"babutela/e2e/v1/arquivo";

/// One file's key, and its sizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey {
    pub key: [u8; 32],
    /// The file's own size.
    pub len: u64,
    /// With the padding: what is encrypted.
    pub padded: u64,
}

impl FileKey {
    pub fn new(len: u64) -> FileKey {
        FileKey {
            key: prim::random(),
            len,
            padded: super::msg::padme(len),
        }
    }

    /// The key as it goes in the message (`FileRef::fk`), with the size
    /// the message says.
    pub fn from_parts(fk: &str, len: u64) -> Option<FileKey> {
        Some(FileKey {
            key: prim::unb64_n(fk)?,
            len,
            padded: super::msg::padme(len),
        })
    }

    pub fn encoded(&self) -> String {
        prim::b64(&self.key)
    }

    /// Blocks in the file (an empty file still has one, empty).
    pub fn blocks(&self) -> u64 {
        self.padded.div_ceil(BLOCK).max(1)
    }

    /// Its size on the server.
    pub fn sealed_len(&self) -> u64 {
        self.padded + self.blocks() * prim::TAG as u64
    }

    fn nonce(&self, i: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[..8].copy_from_slice(&i.to_be_bytes());
        n[11] = u8::from(i + 1 == self.blocks());
        n
    }

    fn aad(&self) -> Vec<u8> {
        [DS_FILE, &self.padded.to_be_bytes()].concat()
    }

    /// Plain bytes of block `i` (its length).
    pub fn block_len(&self, i: u64) -> u64 {
        let start = i * BLOCK;
        BLOCK.min(self.padded.saturating_sub(start))
    }

    /// Block `i`, encrypted. `plain` is its bytes of the file; what is
    /// missing past the file's end is the padding (zeros).
    pub fn seal_block(&self, i: u64, plain: &[u8]) -> Vec<u8> {
        let mut data = plain.to_vec();
        data.resize(self.block_len(i) as usize, 0);
        prim::seal(&self.key, self.nonce(i), &self.aad(), &mut data);
        data
    }

    /// Block `i` back, or None when it is not this file's block `i`.
    pub fn open_block(&self, i: u64, sealed: &[u8]) -> Option<Vec<u8>> {
        if i >= self.blocks() || sealed.len() as u64 != self.block_len(i) + prim::TAG as u64 {
            return None;
        }
        prim::open(&self.key, self.nonce(i), &self.aad(), sealed)
    }

    /// Where block `i` starts on the server.
    pub fn sealed_offset(i: u64) -> u64 {
        i * SEALED_BLOCK
    }

    /// The whole file back from its bytes on the server, the padding cut.
    pub fn open_all(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() as u64 != self.sealed_len() {
            return None;
        }
        let mut out = Vec::with_capacity(self.padded as usize);
        for (i, block) in sealed.chunks(SEALED_BLOCK as usize).enumerate() {
            out.extend_from_slice(&self.open_block(i as u64, block)?);
        }
        if out.len() as u64 != self.padded {
            return None;
        }
        out.truncate(self.len as usize);
        Some(out)
    }

    /// The blocks that hold the file's bytes `start..end`.
    pub fn blocks_for(&self, start: u64, end: u64) -> std::ops::Range<u64> {
        let first = start / BLOCK;
        let last = end.saturating_sub(1).min(self.padded.saturating_sub(1)) / BLOCK;
        first..(last + 1).min(self.blocks())
    }
}

/// The keys of the files on the messages opened so far, by the file's name
/// on the server (the part after "/uploads/", random): what the pictures,
/// players and downloads decrypt with. In memory only.
static KEYS: std::sync::Mutex<Option<std::collections::HashMap<String, FileKey>>> =
    std::sync::Mutex::new(None);

fn name_of(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

pub fn remember(url: &str, key: FileKey) {
    let mut keys = KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    keys.get_or_insert_with(Default::default)
        .insert(name_of(url).to_owned(), key);
}

/// The key of the file at `url`, if it is one of a message's (an avatar or
/// a server's picture is not encrypted).
pub fn key_for(url: &str) -> Option<FileKey> {
    if !url.contains("/uploads/") {
        return None;
    }
    let keys = KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    keys.as_ref()?.get(name_of(url)).cloned()
}

pub fn forget_all() {
    *KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The file `bytes` (as downloaded from `url`) decrypted, or as they are
/// when they are not encrypted.
pub fn open_downloaded(url: &str, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    match key_for(url) {
        Some(k) => k
            .open_all(&bytes)
            .ok_or_else(|| "o arquivo veio alterado ou incompleto".to_owned()),
        None => Ok(bytes),
    }
}

/// The encrypted bytes `start..start + len` of the file at `path`, for an
/// upload part: the blocks that cover them are encrypted from the file and
/// cut to the range. The same block always encrypts the same way (its
/// nonce is its number), so a block split across two parts joins up.
pub fn sealed_range(
    key: &FileKey,
    file: &mut std::fs::File,
    start: u64,
    len: u64,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let end = (start + len).min(key.sealed_len());
    let mut out = Vec::with_capacity(len as usize);
    let mut i = start / SEALED_BLOCK;
    while FileKey::sealed_offset(i) < end {
        let from = i * BLOCK;
        let want = key.block_len(i);
        // The file's own bytes in this block; the rest is padding.
        let real = want.min(key.len.saturating_sub(from));
        let mut plain = vec![0u8; real as usize];
        if real > 0 {
            file.seek(SeekFrom::Start(from))?;
            file.read_exact(&mut plain)?;
        }
        let sealed = key.seal_block(i, &plain);
        let at = FileKey::sealed_offset(i);
        let lo = start.saturating_sub(at) as usize;
        let hi = ((end - at) as usize).min(sealed.len());
        out.extend_from_slice(&sealed[lo..hi]);
        i += 1;
    }
    Ok(out)
}

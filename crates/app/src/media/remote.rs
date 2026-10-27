//! A hosted file read in pieces, for the audio player: the decoder asks for
//! bytes and seeks, and each piece it needs comes from the server as an
//! HTTP range. A long recording starts playing at once and never has to fit
//! in memory.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use bytes::Bytes;
use symphonia::core::io::MediaSource;

use crate::net::Api;

/// How much each request brings: a few seconds even of uncompressed sound.
const PIECE: u64 = 2 << 20;

/// Pieces kept, for the decoder's small jumps back (headers, seeking).
const KEEP: usize = 4;

pub struct Remote {
    api: Arc<Api>,
    rt: tokio::runtime::Handle,
    url: String,
    len: u64,
    pos: u64,
    /// The pieces read last, as (where they start, bytes), newest first.
    cache: VecDeque<(u64, Bytes)>,
}

impl Remote {
    /// Opens `url`, reading its first piece (and so its size). Runs on the
    /// player's own thread: it waits on the network.
    pub fn open(api: Arc<Api>, rt: tokio::runtime::Handle, url: String) -> Result<Remote, String> {
        let (first, len) = rt.block_on(api.fetch_range(&url, 0, PIECE - 1))?;
        Ok(Remote {
            api,
            rt,
            url,
            len,
            pos: 0,
            cache: VecDeque::from([(0, first)]),
        })
    }

    /// The piece holding byte `pos`, from the cache or the server. Pieces
    /// start at multiples of PIECE, so going back finds the same ones.
    fn piece_at(&mut self, pos: u64) -> io::Result<(u64, Bytes)> {
        if let Some(found) = self
            .cache
            .iter()
            .find(|(start, b)| *start <= pos && pos < start + b.len() as u64)
        {
            return Ok(found.clone());
        }
        let start = pos - pos % PIECE;
        let end = (start + PIECE).min(self.len) - 1;
        let (bytes, _) = self
            .rt
            .block_on(self.api.fetch_range(&self.url, start, end))
            .map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.cache.push_front((start, bytes.clone()));
        self.cache.truncate(KEEP);
        Ok((start, bytes))
    }
}

impl Read for Remote {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let (start, bytes) = self.piece_at(self.pos)?;
        let at = (self.pos - start) as usize;
        let n = buf.len().min(bytes.len() - at);
        buf[..n].copy_from_slice(&bytes[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Remote {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(d) => i128::from(self.len) + i128::from(d),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
        };
        let Ok(target) = u64::try_from(target) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "antes do começo do arquivo",
            ));
        };
        // Past the end is allowed, like a file: reads there give nothing.
        self.pos = target;
        Ok(target)
    }
}

impl MediaSource for Remote {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

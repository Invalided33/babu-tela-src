//! The app inside the installer: `cargo xtask release` packs the built
//! "Babu Tela.exe" with [`pack`], the installer embeds the result and
//! [`unpack`]s it on the user's machine.
//!
//! Layout: an 8-byte tag, the unpacked size (u64, little endian), the
//! SHA-256 of the unpacked bytes, then an .xz stream (LZMA2 behind the x86
//! branch filter, which suits machine code). The hash is checked after
//! unpacking, so a damaged download never becomes a broken app.

use std::io::{self, Read, Write};

use lzma_rust2::{CheckType, FilterType, XzOptions, XzReader, XzWriter};
use sha2::{Digest, Sha256};

const TAG: &[u8; 8] = b"BTSETUP1";
const HEADER: usize = 8 + 8 + 32;

/// What the header says about the packed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub size: u64,
    pub sha256: [u8; 32],
}

/// Packs `data` (the app's executable). Slow on purpose: the strongest
/// preset, done once per release.
pub fn pack(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut options = XzOptions::with_preset(9);
    // No need for a dictionary larger than the file itself, and every byte
    // of it is memory the installer has to set aside.
    let fit = (data.len().max(1 << 16) as u64).next_power_of_two();
    options.lzma_options.dict_size = options.lzma_options.dict_size.min(fit as u32);
    options.prepend_pre_filter(FilterType::BcjX86, 0);
    options.set_check_sum_type(CheckType::Crc64);

    let mut out = Vec::with_capacity(data.len() / 2);
    out.extend_from_slice(TAG);
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(&Sha256::digest(data));
    let mut xz = XzWriter::new(out, options)?;
    xz.write_all(data)?;
    xz.finish()
}

/// Reads the header, without unpacking anything.
pub fn info(packed: &[u8]) -> Option<Info> {
    if packed.len() < HEADER || &packed[..8] != TAG {
        return None;
    }
    let size = u64::from_le_bytes(packed[8..16].try_into().ok()?);
    let sha256 = packed[16..48].try_into().ok()?;
    Some(Info { size, sha256 })
}

/// Unpacks into `out`, calling `progress(done, total)` along the way, and
/// checks the result against the header.
pub fn unpack(
    packed: &[u8],
    out: &mut impl Write,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<()> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let info = info(packed).ok_or_else(|| bad("o instalador está danificado (cabeçalho)"))?;
    let mut xz = XzReader::new(&packed[HEADER..], false);
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = xz.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        done += n as u64;
        if done > info.size {
            return Err(bad("o instalador está danificado (tamanho)"));
        }
        progress(done, info.size);
    }
    out.flush()?;
    if done != info.size || hash.finalize().as_slice() != info.sha256 {
        return Err(bad("o instalador está danificado (conferência)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        // Something that looks a little like code: repeats, with variety.
        (0..300_000u32)
            .flat_map(|i| [(i % 251) as u8, 0xe8, (i >> 3) as u8, 0x48])
            .collect()
    }

    #[test]
    fn round_trips_and_shrinks() {
        let data = sample();
        let packed = pack(&data).unwrap();
        assert!(packed.len() < data.len() / 4, "{}", packed.len());
        let info = info(&packed).unwrap();
        assert_eq!(info.size, data.len() as u64);
        let mut out = Vec::new();
        let mut last = (0, 0);
        unpack(&packed, &mut out, |done, total| last = (done, total)).unwrap();
        assert_eq!(out, data);
        assert_eq!(last, (data.len() as u64, data.len() as u64));
    }

    #[test]
    fn damage_is_caught() {
        let data = sample();
        let mut packed = pack(&data).unwrap();
        assert!(info(b"nope").is_none());
        // A wrong hash in the header.
        packed[20] ^= 1;
        let err = unpack(&packed, &mut Vec::new(), |_, _| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        packed[20] ^= 1;
        // A flipped byte in the compressed data.
        let at = packed.len() / 2;
        packed[at] ^= 0x55;
        assert!(unpack(&packed, &mut Vec::new(), |_, _| {}).is_err());
    }
}

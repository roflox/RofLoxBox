//! DeviceSQL strings.
//!
//! The first byte selects the encoding:
//! - odd values: short ASCII, `kind = ((len + 1) << 1) | 1` with the text following
//! - `0x40`: long ASCII, then `u16` total length (header included) and a pad byte
//! - `0x90`: long UTF-16LE, same header as long ASCII

use crate::bytes::{slice, u8_at, u16_at};
use crate::error::{Error, Result};

const LONG_ASCII: u8 = 0x40;
const LONG_UTF16LE: u8 = 0x90;
const LONG_HEADER_LEN: usize = 4;
/// rekordbox marks an ISRC by storing ASCII inside a UTF-16 string, prefixed with this byte.
const ISRC_MARKER: u8 = 0x03;

pub(super) fn read(page: &[u8], offset: usize) -> Result<String> {
    let kind = u8_at(page, offset)?;
    match kind {
        LONG_ASCII => Ok(latin1(long_body(page, offset, kind)?)),
        LONG_UTF16LE => Ok(utf16le(long_body(page, offset, kind)?)),
        _ if kind & 1 == 1 => {
            let len = usize::from(kind >> 1)
                .checked_sub(1)
                .ok_or(Error::InvalidString { kind, offset })?;
            Ok(latin1(slice(page, offset + 1, len)?))
        }
        _ => Err(Error::InvalidString { kind, offset }),
    }
}

/// Reads the ISRC field of a track row, which uses its own variant of the UTF-16 string.
pub(super) fn read_isrc(page: &[u8], offset: usize) -> Result<String> {
    if u8_at(page, offset)? == LONG_UTF16LE
        && let Some((&ISRC_MARKER, ascii)) = long_body(page, offset, LONG_UTF16LE)?.split_first()
    {
        return Ok(latin1(ascii));
    }
    read(page, offset)
}

fn long_body(page: &[u8], offset: usize, kind: u8) -> Result<&[u8]> {
    let len = usize::from(u16_at(page, offset + 1)?)
        .checked_sub(LONG_HEADER_LEN)
        .ok_or(Error::InvalidString { kind, offset })?;
    slice(page, offset + LONG_HEADER_LEN, len)
}

fn latin1(bytes: &[u8]) -> String {
    trim_nul(bytes.iter().map(|&b| char::from(b)).collect())
}

fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    trim_nul(String::from_utf16_lossy(&units))
}

fn trim_nul(mut s: String) -> String {
    s.truncate(s.trim_end_matches('\0').len());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ascii() {
        // "abc": kind = ((3 + 1) << 1) | 1 = 9
        assert_eq!(read(b"\x09abc", 0).unwrap(), "abc");
        assert_eq!(read(b"\x03", 0).unwrap(), "");
    }

    #[test]
    fn long_ascii() {
        assert_eq!(read(b"\x40\x07\x00\x00abc", 0).unwrap(), "abc");
    }

    #[test]
    fn long_utf16le() {
        assert_eq!(read(b"\x90\x08\x00\x00\xe9\x00\x41\x00", 0).unwrap(), "éA");
    }

    #[test]
    fn isrc() {
        let raw = b"\x90\x12\x00\x00\x03GBAYE6700149\x00";
        assert_eq!(read_isrc(raw, 0).unwrap(), "GBAYE6700149");
        // A real UTF-16 string starting with U+0103 is not mistaken for an ISRC by `read`.
        assert_eq!(read(b"\x90\x06\x00\x00\x03\x01", 0).unwrap(), "ă");
    }

    #[test]
    fn invalid_kind() {
        assert!(matches!(read(b"\x02", 0), Err(Error::InvalidString { kind: 2, .. })));
        assert!(matches!(read(b"\x01", 0), Err(Error::InvalidString { kind: 1, .. })));
        assert!(read(b"\x40\x02\x00\x00", 0).is_err());
    }

    #[test]
    fn truncated() {
        assert!(matches!(read(b"\x09ab", 0), Err(Error::OutOfBounds { .. })));
    }
}
